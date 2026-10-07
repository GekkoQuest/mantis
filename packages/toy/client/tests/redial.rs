//! Reconnecting through the gateway ([`toy_client::gateway::Redial`]): a dial that fails
//! is a failed attempt with the reason kept, the next dial carries the resume ticket in
//! `Hello`, and the shared status goes from reconnecting back to live.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mantis_adapter_contract::native::encode_outbound_frame;
use mantis_adapter_contract::{
    Channel, ConnectionId, DisconnectReason, Inbound, MovementMode, Outbound, ResumeTicket, Transport,
    TransportError, TransportEvent, TransportKind, Welcome,
};
use mantis_client::core_api::MotionState;
use mantis_client::net::{NativeSession, NetConfig, move_channel};
use mantis_client::reconnect::ReconnectStatus;
use mantis_client::snapshot::snapshot_channel;
use mantis_client::time::{HostClock, ManualClock};
use mantis_core::content::ContentHash;
use mantis_core::time::Tick;
use mantis_core::wire::{BoundedArray, MessageId};
use toy_client::gateway::Redial;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// One fake connection: frames the server queued, a drop flag, and the `Hello` tokens
/// the client sent.
#[derive(Default)]
struct Wire {
    inbound: VecDeque<Vec<u8>>,
    drop: bool,
    hellos: Vec<Vec<u8>>,
}

struct Fake(Arc<Mutex<Wire>>);

impl Transport for Fake {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        let mut w = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(bytes) = w.inbound.pop_front() {
            sink(TransportEvent::Frame {
                conn: ConnectionId(0),
                channel: Channel::Reliable,
                bytes: &bytes,
            });
        }
        if std::mem::take(&mut w.drop) {
            sink(TransportEvent::Disconnected {
                conn: ConnectionId(0),
                reason: DisconnectReason::TimedOut,
            });
        }
    }
    fn send(&mut self, _conn: ConnectionId, _channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        let hello = match (bytes.get(1), bytes.get(2), bytes.get(3..)) {
            (Some(&lo), Some(&hi), Some(payload)) => {
                mantis_adapter_contract::parse_inbound(MessageId(u16::from_le_bytes([lo, hi])), payload).ok()
            }
            _ => None,
        };
        if let Some(Inbound::Hello(h)) = hello {
            let mut w = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            w.hellos.push(h.token.iter().copied().collect());
        }
        Ok(())
    }
    fn disconnect(&mut self, _conn: ConnectionId) {}
    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }
    fn max_unreliable_payload(&self) -> usize {
        1100
    }
}

fn frame(msg: &Outbound) -> Vec<u8> {
    let mut out = Vec::new();
    encode_outbound_frame(msg, &mut out);
    out
}

fn welcome() -> Vec<u8> {
    frame(&Outbound::Welcome(Welcome {
        protocol: 1,
        capabilities: 0,
        session: 9,
        tick: Tick(10),
        tick_rate: 30,
        mode: MovementMode::Predictive,
        avatar: None,
        character: 77,
    }))
}

fn wire(w: &Arc<Mutex<Wire>>) -> std::sync::MutexGuard<'_, Wire> {
    w.lock().unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn a_failed_dial_is_retried_and_the_next_one_resumes_with_the_ticket() -> TestResult {
    let clock = Arc::new(ManualClock::new());
    let first = Arc::new(Mutex::new(Wire::default()));
    let (snap_tx, _inbox) = snapshot_channel::<MotionState>(8, 8);
    let (_outbox, moves) = move_channel(16);
    let mut net = NativeSession::new(
        Fake(Arc::clone(&first)),
        Arc::clone(&clock) as Arc<dyn HostClock>,
        NetConfig::new(ContentHash::ZERO),
        snap_tx,
        moves,
    );

    // The gateway is unreachable for the first dial, then reachable and accepting.
    let second = Arc::new(Mutex::new(Wire::default()));
    let mut dials = 0u32;
    let gateway = Arc::clone(&second);
    let mut redial = Redial::new(
        b"launcher",
        Box::new(move || {
            dials += 1;
            if dials == 1 {
                Err("gateway.test:7777: no route".to_owned())
            } else {
                wire(&gateway).inbound.push_back(welcome());
                Ok(Fake(Arc::clone(&gateway)))
            }
        }),
    );
    let status = Arc::clone(redial.status());

    net.start(b"launcher");
    {
        let token = BoundedArray::from_slice(&[5u8; 32]).ok_or("ticket")?;
        let mut w = wire(&first);
        w.inbound.push_back(welcome());
        w.inbound.push_back(frame(&Outbound::ResumeTicket(ResumeTicket {
            token,
            expires_ms: 30_000,
        })));
    }
    net.step();
    redial.step(clock.now(), &mut net);
    assert_eq!(status.get(), ReconnectStatus::Live);

    // The connection drops.
    wire(&first).drop = true;
    net.step();
    redial.step(clock.now(), &mut net);
    assert!(matches!(
        status.get(),
        ReconnectStatus::Reconnecting { attempt: 1, .. }
    ));
    assert!(status.get().notice().is_some_and(|n| n.contains("reconnecting")));

    // The first dial fails: kept as the reason, counted as attempt 1; the next waits 500 ms.
    clock.advance(Duration::from_millis(250));
    redial.step(clock.now(), &mut net);
    assert_eq!(redial.dials(), 1);
    assert_eq!(redial.last_error(), Some("gateway.test:7777: no route"));
    net.step();
    redial.step(clock.now(), &mut net);
    assert!(matches!(
        status.get(),
        ReconnectStatus::Reconnecting { attempt: 2, .. }
    ));
    clock.advance(Duration::from_millis(499));
    redial.step(clock.now(), &mut net);
    assert_eq!(redial.dials(), 1);

    // The second dial reaches the gateway with the ticket, and the session is live again.
    clock.advance(Duration::from_millis(1));
    redial.step(clock.now(), &mut net);
    assert_eq!(redial.dials(), 2);
    assert_eq!(wire(&second).hellos, [vec![5u8; 32]]);
    net.step();
    redial.step(clock.now(), &mut net);
    assert_eq!(status.get(), ReconnectStatus::Live);
    assert_eq!(redial.reconnects(), 1);
    assert_eq!(net.stats().reconnects, 1);
    Ok(())
}
