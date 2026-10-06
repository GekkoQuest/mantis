//! The legacy adapter's client half as a bot wire, so headless bots can
//! play through either of the package's adapters.

use std::collections::BTreeSet;

use mantis_adapter_contract::core_types::{Angle16, EntityId, MotionState, Tick, WireString};
use mantis_adapter_contract::{Channel, ExtensionKind, Inbound, LocalAvatar, MovementMode};
use mantis_server::bots::{BotEvent, BotWire};
use toy_adapter_legacy::client::{ServerPacket, decode_server, encode_inbound};
use toy_adapter_legacy::refusal_from;

/// A legacy client's view of the protocol.
pub struct LegacyWire {
    build: u32,
    known: BTreeSet<EntityId>,
}

impl LegacyWire {
    /// A client announcing `build`.
    #[must_use]
    pub fn new(build: u32) -> Self {
        Self {
            build,
            known: BTreeSet::new(),
        }
    }
}

impl BotWire for LegacyWire {
    fn mode(&self) -> MovementMode {
        MovementMode::Validated
    }

    fn encode(&self, msg: &Inbound, out: &mut Vec<u8>) -> Option<Channel> {
        encode_inbound(msg, self.build, out).then_some(Channel::Reliable)
    }

    fn decode(&mut self, bytes: &[u8], events: &mut dyn FnMut(BotEvent)) {
        let mut tick = None;
        let mut local = None;
        let mut remotes = 0usize;
        let known = &mut self.known;
        let result = decode_server(bytes, |p| match p {
            ServerPacket::LoginOk { avatar, .. } => events(BotEvent::Welcome { avatar }),
            ServerPacket::LoginFail(reason) => events(BotEvent::Refused(reason)),
            ServerPacket::SetPos { position, .. } => events(BotEvent::Corrected { position }),
            ServerPacket::ExtensionRefused { kind, reason } => events(BotEvent::ExtensionRefused {
                kind: ExtensionKind(kind),
                // The legacy protocol has no request ids.
                request: 0,
                reason: refusal_from(reason),
            }),
            ServerPacket::FeatureData { kind, len, bytes } => events(BotEvent::ExtensionMessage {
                kind: ExtensionKind(kind),
                len,
                bytes,
            }),
            ServerPacket::FeatureState { len, key, enabled } => {
                let text = key
                    .get(..usize::from(len))
                    .and_then(|k| core::str::from_utf8(k).ok());
                if let Some(key) = text.and_then(WireString::new) {
                    events(BotEvent::FeatureState { key, enabled });
                }
            }
            ServerPacket::WorldTick(t) => tick = Some(Tick(u64::from(t))),
            ServerPacket::SelfState {
                id,
                position,
                heading,
            } => {
                local = Some(LocalAvatar {
                    id,
                    state: MotionState::at_rest(position, Angle16(heading)),
                });
            }
            ServerPacket::Enter { id, .. } => {
                known.insert(id);
            }
            ServerPacket::Leave { id } => {
                known.remove(&id);
            }
            ServerPacket::Move { .. } => remotes += 1,
            ServerPacket::Effect { .. } => {}
        });
        if let (Ok(()), Some(tick)) = (result, tick) {
            events(BotEvent::Snapshot {
                tick,
                ack: None,
                local,
                remotes,
            });
        }
    }

    fn sees(&self, id: EntityId) -> bool {
        self.known.contains(&id)
    }
}
