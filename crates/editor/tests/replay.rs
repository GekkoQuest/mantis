//! The replay viewer end to end: a client simulation runs a scripted session (a local
//! avatar, remotes entering and leaving, timeline markers, movement input, and a server
//! correction) while recording; the recording replays tick by tick in a fresh simulation
//! and every tick matches; seeking works both ways; and corrupting one recorded field
//! (a remote position bit, an input, an authoritative state, a hash) is reported as a
//! divergence at exactly that tick. Headless.
//!
//! Recording is a dev-build feature, so this suite runs with `debug_assertions` only.

#![cfg(debug_assertions)]
#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use mantis_client::camera::LookSample;
use mantis_client::core_api::{
    Angle16, CoreMotion, EntityId, FlatGround, InputSeq, Motion, MotionModifiers, MotionParams, MotionState,
    MoveButtons, MoveInput, Tick, TickRate, Vec3,
};
use mantis_client::input::accumulator::InputAccumulator;
use mantis_client::input::action::{ActionBits, ActionId, ActionKind, ActionTable};
use mantis_client::input::intent::{AxisButtons, MoveIntentMap};
use mantis_client::input::router::ActionFrame;
use mantis_client::recording::Recording;
use mantis_client::render_world::{PresentationConfig, RenderWorld};
use mantis_client::sim::{ClientSim, ClientSimConfig, ClientSimParts};
use mantis_client::snapshot::{RemoteState, snapshot_channel};
use mantis_client::threads::sim_thread::TickHandler;
use mantis_client::time::HostInstant;
use mantis_core::content::ContentHash;
use mantis_core::graph::{
    GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, PackageMarker, TimelineMarker,
};
use mantis_core::log::BuildId;
use mantis_editor::replay::{ReplayCapture, ReplayError, ReplayViewer, ReplayWiring};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;
type Model = CoreMotion<FlatGround>;
type Factory = Box<dyn FnMut(ReplayWiring<MotionState>) -> ClientSimParts<Model, ReplayCapture>>;

const AVATAR: EntityId = EntityId::new(1, 1);
const R1: EntityId = EntityId::new(2, 1);
const R2: EntityId = EntityId::new(3, 1);
const R3: EntityId = EntityId::new(4, 2);
const TICKS: u64 = 60;
const TICK_NANOS: u64 = 33_333_333;
const DT: f32 = 1.0 / 30.0;
/// The tick whose R2 sample carries a unique, searchable position.
const MARKED_TICK: u64 = 26;
const MARKED_POSITION: Vec3 = Vec3::new(77.125, 3.5, -9.25);

fn at(t: u64) -> HostInstant {
    HostInstant::from_nanos(t * TICK_NANOS)
}

/// Everything a simulation is built from, shared by the recording and every replay.
#[derive(Clone)]
struct Setup {
    motion: Motion,
    intents: MoveIntentMap,
    forward: ActionId,
    strafe: ActionId,
    config: ClientSimConfig,
}

impl Setup {
    fn new() -> Result<Self, Error> {
        let mut actions = ActionTable::new();
        let forward = actions.define("forward", ActionKind::Button)?;
        let strafe = actions.define("strafe", ActionKind::Axis)?;
        let mut intents = MoveIntentMap::new();
        intents.map_button(&actions, forward, MoveButtons::FORWARD)?;
        intents.map_axis(
            &actions,
            AxisButtons {
                action: strafe,
                threshold: 0.5,
                positive: MoveButtons::STRAFE_RIGHT,
                negative: MoveButtons::STRAFE_LEFT,
            },
        )?;
        Ok(Self {
            motion: Motion::new(MotionParams::DEFAULT)?,
            intents,
            forward,
            strafe,
            config: ClientSimConfig {
                server_rate: TickRate::new(30).ok_or("zero rate")?,
                correction_window: Duration::from_millis(150),
                snap_distance: 4.0,
                remote_capacity: 8,
                remote_timeout: Duration::from_secs(1),
                max_remote_extrapolation: Duration::from_millis(250),
                delay: mantis_client::jitter::DelayConfig::default(),
                input_buffer: 32,
                timeline_rise_shift: 4,
                timeline_adaptive: true,
            },
        })
    }

    fn parts(&self, w: ReplayWiring<MotionState>) -> ClientSimParts<Model, ReplayCapture> {
        ClientSimParts {
            config: self.config,
            motion: CoreMotion::new(self.motion),
            ground: Arc::new(FlatGround(0.0)),
            dt: DT,
            initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
            accumulator: w.accumulator,
            intents: self.intents.clone(),
            inbox: w.inbox,
            outbox: w.outbox,
            markers: None,
        }
    }

    fn factory(&self) -> Factory {
        let setup = self.clone();
        Box::new(move |w| setup.parts(w))
    }
}

fn marker(kind: MarkerKind, t: u64, target: Option<EntityId>) -> TimelineMarker {
    TimelineMarker {
        id: MarkerId {
            graph: GraphId::named("ability.replay"),
            node: NodeKey((t % 4) as u16),
        },
        kind,
        at: Tick(t + 1),
        offset: 1,
        source: R1,
        target,
        instance: GraphInstanceId(t),
    }
}

/// What the recorded session observed, for comparison with the replay.
struct Session {
    bytes: Vec<u8>,
    hashes: Vec<u64>,
    final_state: MotionState,
    final_remotes: usize,
    final_server_tick: Option<Tick>,
    corrected: u64,
    resets: u64,
    saw_correction: bool,
}

/// Runs the scripted session with recording on.
fn record_session(setup: &Setup) -> Result<Session, Error> {
    let (mut snapshots, inbox) = snapshot_channel::<MotionState>(4, 8);
    let accumulator = Arc::new(InputAccumulator::new());
    let mut sim = ClientSim::new(setup.parts(ReplayWiring {
        accumulator: Arc::clone(&accumulator),
        inbox,
        outbox: ReplayCapture::default(),
    }));
    sim.start_recording(BuildId([7; 32]), ContentHash::of(b"replay test content"), 1 << 20);
    let mut world = RenderWorld::with_capacity(setup.config.remote_capacity, PresentationConfig::default());
    let ground = FlatGround(0.0);
    let mut server = MotionState::at_rest(Vec3::ZERO, Angle16(0));
    let mut ack: Option<InputSeq> = None;
    let mut uplink: VecDeque<(u64, MoveInput)> = VecDeque::new();
    let mut hashes = Vec::new();
    let mut saw_correction = false;

    for t in 0..TICKS {
        let mods = if t >= 30 {
            MotionModifiers {
                speed_scale: 0.8,
                ..MotionModifiers::NONE
            }
        } else {
            MotionModifiers::NONE
        };
        // The server applies inputs two ticks after they were sent.
        while let Some(&(arrive, mv)) = uplink.front() {
            if arrive > t {
                break;
            }
            let _ = uplink.pop_front();
            server = setup.motion.step(&ground, &server, &mv, &mods, DT);
            ack = Some(mv.seq);
        }
        if t == 18 {
            server.position.x += 0.3; // knocked aside: the client must correct
        }
        // Snapshots from tick 3, one lost every ninth tick, a stale duplicate every tenth.
        if t >= 3 && t % 9 != 0 {
            let server_ticks: &[u64] = if t % 10 == 4 { &[t - 1, t] } else { &[t] };
            for &st in server_ticks {
                let mut f = snapshots.acquire().ok_or("no free frame")?;
                f.server_tick = Tick(st);
                f.received_at = at(t);
                f.ack = ack;
                f.local = Some((AVATAR, server));
                f.local_mods = mods;
                f.push_remote(RemoteState {
                    id: R1,
                    tick: Tick(st),
                    position: Vec3::new(st as f32 * 0.5, 0.0, 4.0),
                    velocity: Vec3::new(15.0, 0.0, 0.0),
                    yaw: Angle16(16_384),
                });
                if st < 40 {
                    let position = if st == MARKED_TICK {
                        MARKED_POSITION
                    } else {
                        Vec3::new(-2.0, 0.0, st as f32 * 0.25)
                    };
                    f.push_remote(RemoteState {
                        id: R2,
                        tick: Tick(st),
                        position,
                        velocity: Vec3::new(0.0, 0.0, 7.5),
                        yaw: Angle16(0),
                    });
                } else if st == 40 {
                    f.push_removed(R2);
                }
                if st >= 20 {
                    if st == 20 {
                        f.push_entered(R3, 77);
                    }
                    f.push_remote(RemoteState {
                        id: R3,
                        tick: Tick(st.saturating_sub(1)),
                        position: Vec3::new(5.0, 1.0, -(st as f32)),
                        velocity: Vec3::new(0.0, 0.0, -30.0),
                        yaw: Angle16(32_768),
                    });
                }
                match st % 6 {
                    0 => {
                        f.push_marker(marker(MarkerKind::CastStart, st, Some(R1)));
                        f.push_marker(marker(MarkerKind::Impact { target: AVATAR }, st, Some(AVATAR)));
                    }
                    2 => {
                        f.push_marker(marker(MarkerKind::TickN((st / 6) as u16), st, None));
                    }
                    4 => {
                        f.push_marker(marker(MarkerKind::Package(PackageMarker(5)), st, None));
                        f.push_marker(marker(MarkerKind::Expire, st, None));
                    }
                    _ => {}
                }
                if !snapshots.send(f) {
                    return Err("snapshot channel closed".into());
                }
            }
        }
        // Input: forward held, then a strafe, with the look turning.
        let mut frame = ActionFrame::default();
        if (5..25).contains(&t) || (35..50).contains(&t) {
            frame.held.insert(setup.forward);
            if t == 5 || t == 35 {
                frame.pressed.insert(setup.forward);
            }
        }
        if t == 25 || t == 50 {
            frame.released.insert(setup.forward);
        }
        let strafe = if (15..20).contains(&t) {
            1.0
        } else if (45..48).contains(&t) {
            -1.0
        } else {
            0.0
        };
        if let Some(a) = frame.axes.get_mut(setup.strafe.index()) {
            *a = strafe;
        }
        let look = LookSample {
            yaw: Angle16((t * 700) as u16),
            pitch: Angle16(64_000),
        };
        accumulator.merge_frame(&frame, look);
        if t % 3 == 0 {
            accumulator.merge_frame(&frame, look); // two render frames this tick
        }

        let before = sim.outbox().sent();
        world.begin(Tick(t), at(t));
        sim.tick(Tick(t), at(t), &mut world);
        if sim.outbox().sent() > before
            && let Some(mv) = sim.outbox().last()
        {
            uplink.push_back((t + 2, mv));
        }
        saw_correction |= sim.correction().offset != Vec3::ZERO;
        hashes.push(sim.state_hash());
    }
    let bytes = sim.take_recording().ok_or("not recording")?;
    Ok(Session {
        bytes,
        hashes,
        final_state: *sim.predictor().state(),
        final_remotes: sim.remote_count(),
        final_server_tick: sim.last_server_tick(),
        corrected: sim.predictor().stats().corrected,
        resets: sim.stats().resets,
        saw_correction,
    })
}

fn viewer(setup: &Setup, bytes: &[u8]) -> Result<ReplayViewer<Model, Factory>, Error> {
    Ok(ReplayViewer::open(bytes, setup.factory())?)
}

#[test]
fn a_recorded_session_replays_tick_for_tick() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    // The script exercised what it claims to.
    assert!(session.corrected >= 1, "no correction from the server");
    assert!(session.saw_correction);
    assert_eq!(session.resets, 1, "one spawn");
    assert_eq!(session.final_remotes, 2, "R2 left, R3 entered");
    let recording = Recording::<MotionState>::parse(&session.bytes)?;
    assert_eq!(recording.ticks.len() as u64, TICKS);
    assert!(!recording.truncated);
    let frames = || recording.ticks.iter().flat_map(|t| &t.frames);
    assert!(frames().map(|f| f.markers.len()).sum::<usize>() >= 20);
    assert!(frames().any(|f| f.removed.contains(&R2)));
    assert!(frames().any(|f| f.entered.contains(&(R3, 77))));
    assert!(recording.ticks.iter().any(|t| t.frames.len() == 2));
    assert!(recording.ticks.iter().filter(|t| t.sent.is_some()).count() > 50);

    let mut v = viewer(&setup, &session.bytes)?;
    assert_eq!(v.tick(), None);
    assert_eq!(v.inspect().replayed, 0);
    for (i, hash) in session.hashes.iter().enumerate() {
        let report = v.step()?.ok_or("ended early")?;
        assert_eq!(report.tick, Tick(i as u64));
        assert_eq!(report.state_hash, *hash, "tick {i}");
        assert_eq!(v.tick(), Some(Tick(i as u64)));
    }
    assert_eq!(v.step()?, None);
    assert!(v.at_end());

    let view = v.inspect();
    assert_eq!(view.tick, Some(Tick(TICKS - 1)));
    assert_eq!((view.replayed, view.total), (TICKS as usize, TICKS as usize));
    assert_eq!(view.local, Some(AVATAR));
    assert!(view.local_state.bits_eq(&session.final_state));
    assert_eq!(view.remote_count, session.final_remotes);
    assert_eq!(view.last_server_tick, session.final_server_tick);
    assert_eq!(view.state_hash, *session.hashes.last().ok_or("no hashes")?);
    let text = view.to_string();
    assert!(text.contains("local 1:1") && text.contains("remotes 2"), "{text}");
    Ok(())
}

#[test]
fn seeking_moves_both_ways_and_rebuilds_on_the_way_back() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let mut v = viewer(&setup, &session.bytes)?;
    let hash_at = |t: u64| session.hashes.get(t as usize).copied().ok_or("no hash");

    v.seek(Tick(30))?;
    assert_eq!(v.tick(), Some(Tick(30)));
    assert_eq!(v.inspect().state_hash, hash_at(30)?);
    v.seek(Tick(10))?;
    assert_eq!(v.tick(), Some(Tick(10)));
    assert_eq!(v.inspect().state_hash, hash_at(10)?);
    assert_eq!(v.inspect().remote_count, 2);
    v.seek(Tick(45))?;
    assert_eq!(v.inspect().state_hash, hash_at(45)?);
    v.seek(Tick(1_000))?;
    assert!(v.at_end());
    assert_eq!(v.tick(), Some(Tick(TICKS - 1)));
    v.restart();
    assert_eq!(v.tick(), None);
    assert_eq!(v.run_to_end()?, TICKS as usize);
    Ok(())
}

/// Replays until the first error, asserting every earlier tick verified.
fn first_error(setup: &Setup, bytes: &[u8]) -> Result<ReplayError, Error> {
    let mut v = viewer(setup, bytes)?;
    loop {
        match v.step() {
            Ok(Some(_)) => {}
            Ok(None) => return Err("replay matched a corrupted recording".into()),
            Err(e) => {
                let tick = match e {
                    ReplayError::Divergence { tick, .. }
                    | ReplayError::IntentDivergence { tick, .. }
                    | ReplayError::Inbox { tick } => tick,
                };
                assert_eq!(v.tick(), Some(tick), "the failing tick has been applied");
                return Ok(e);
            }
        }
    }
}

#[test]
fn a_flipped_remote_position_bit_diverges_at_its_tick() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let pattern: Vec<u8> = [MARKED_POSITION.x, MARKED_POSITION.y, MARKED_POSITION.z]
        .iter()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let hits: Vec<usize> = session
        .bytes
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, w)| *w == pattern.as_slice())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits.len(), 1, "the marked position is unique");
    let mut bytes = session.bytes.clone();
    let at = *hits.first().ok_or("no hit")?;
    *bytes.get_mut(at).ok_or("offset")? ^= 1; // lowest mantissa bit of x
    let parsed = Recording::<MotionState>::parse(&bytes)?;
    let x = parsed
        .ticks
        .get(MARKED_TICK as usize)
        .and_then(|t| t.frames.first())
        .and_then(|f| f.remotes.iter().find(|r| r.id == R2))
        .map(|r| r.position.x)
        .ok_or("marked sample")?;
    assert_eq!(x.to_bits(), MARKED_POSITION.x.to_bits() ^ 1);

    match first_error(&setup, &bytes)? {
        ReplayError::Divergence {
            tick,
            expected,
            actual,
        } => {
            assert_eq!(tick, Tick(MARKED_TICK));
            assert_eq!(Some(&expected), session.hashes.get(MARKED_TICK as usize));
            assert_ne!(expected, actual);
        }
        other => return Err(format!("expected a divergence, got {other}").into()),
    }
    Ok(())
}

#[test]
fn an_altered_input_is_an_intent_divergence_at_its_tick() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let mut recording = Recording::<MotionState>::parse(&session.bytes)?;
    let rec = recording.ticks.get_mut(10).ok_or("tick 10")?;
    assert!(rec.input.held.contains(setup.forward) && !rec.input.pressed.contains(setup.forward));
    rec.input.held = ActionBits::EMPTY;
    let bytes = recording.encode();

    match first_error(&setup, &bytes)? {
        ReplayError::IntentDivergence {
            tick,
            expected,
            actual,
        } => {
            assert_eq!(tick, Tick(10));
            let expected = expected.ok_or("recorded intent")?;
            let actual = actual.ok_or("replayed intent")?;
            assert!(expected.buttons.contains(MoveButtons::FORWARD));
            assert!(!actual.buttons.contains(MoveButtons::FORWARD));
            assert_eq!((expected.seq, expected.tick), (actual.seq, actual.tick));
        }
        other => return Err(format!("expected an intent divergence, got {other}").into()),
    }
    Ok(())
}

#[test]
fn an_altered_axis_or_look_is_caught_too() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let base = Recording::<MotionState>::parse(&session.bytes)?;

    let mut axis = base.clone();
    let rec = axis.ticks.get_mut(16).ok_or("tick 16")?;
    if let Some(a) = rec.input.axes.get_mut(setup.strafe.index()) {
        *a = 0.25; // below the threshold: no strafe
    }
    assert!(matches!(
        first_error(&setup, &axis.encode())?,
        ReplayError::IntentDivergence { tick: Tick(16), .. }
    ));

    let mut look = base;
    look.ticks.get_mut(40).ok_or("tick 40")?.input.look.yaw = Angle16(1);
    assert!(matches!(
        first_error(&setup, &look.encode())?,
        ReplayError::IntentDivergence { tick: Tick(40), .. }
    ));
    Ok(())
}

#[test]
fn an_altered_authoritative_state_or_hash_diverges_at_its_tick() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let base = Recording::<MotionState>::parse(&session.bytes)?;

    let mut state = base.clone();
    let frame = state
        .ticks
        .get_mut(33)
        .and_then(|t| t.frames.first_mut())
        .ok_or("frame at 33")?;
    let (_, s) = frame.local.as_mut().ok_or("local at 33")?;
    s.position.x += 0.5;
    assert!(matches!(
        first_error(&setup, &state.encode())?,
        ReplayError::Divergence { tick: Tick(33), .. }
    ));

    let mut hash = base;
    hash.ticks.get_mut(47).ok_or("tick 47")?.state_hash ^= 1;
    match first_error(&setup, &hash.encode())? {
        ReplayError::Divergence {
            tick,
            expected,
            actual,
        } => {
            assert_eq!(tick, Tick(47));
            assert_eq!(expected ^ 1, actual);
        }
        other => return Err(format!("expected a divergence, got {other}").into()),
    }
    Ok(())
}

#[test]
fn a_factory_that_ignores_the_wiring_is_reported() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let base = setup.clone();
    let mut v = ReplayViewer::open(&session.bytes, move |w: ReplayWiring<MotionState>| {
        let (_, own_inbox) = snapshot_channel::<MotionState>(1, 1);
        base.parts(ReplayWiring {
            inbox: own_inbox,
            ..w
        })
    })?;
    v.seek(Tick(2))?; // no snapshots yet
    assert_eq!(v.step(), Err(ReplayError::Inbox { tick: Tick(3) }));
    Ok(())
}

#[test]
fn a_malformed_recording_does_not_open() -> TestResult {
    let setup = Setup::new()?;
    let session = record_session(&setup)?;
    let cut = session.bytes.get(..session.bytes.len() - 1).ok_or("cut")?;
    assert!(ReplayViewer::open(cut, setup.factory()).is_err());
    let mut bad = session.bytes.clone();
    *bad.first_mut().ok_or("empty")? = b'X';
    assert!(ReplayViewer::open(&bad, setup.factory()).is_err());
    Ok(())
}
