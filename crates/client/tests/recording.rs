//! MCRC client recordings: the format round trip, every rejection rule, the byte cap, and
//! the cost of the simulation hook (nothing allocated per tick, recording on or off).

#![expect(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::sync::Arc;
use std::time::Duration;

use mantis_client::camera::LookSample;
use mantis_client::core_api::{
    AimAngles, Angle16, CoreMotion, EntityId, FlatGround, InputSeq, Motion, MotionModifiers, MotionParams,
    MotionState, MoveButtons, MoveInput, Tick, TickRate, Vec3,
};
use mantis_client::input::accumulator::{InputAccumulator, TickInput};
use mantis_client::input::action::{ActionBits, ActionId, ActionKind, ActionTable, MAX_ACTIONS};
use mantis_client::input::intent::MoveIntentMap;
use mantis_client::input::router::ActionFrame;
use mantis_client::recording::{
    END_LEN, HEADER_LEN, MAGIC, MIN_CAP, RecordedFrame, Recorder, Recording, RecordingHeader,
    RecordingReader, TickRecord, VERSION,
};
use mantis_client::render_world::{PresentationConfig, RenderWorld};
use mantis_client::sim::{ClientSim, ClientSimConfig, ClientSimParts, IntentSink};
use mantis_client::snapshot::{RemoteState, SnapshotSender, snapshot_channel};
use mantis_client::threads::sim_thread::TickHandler;
use mantis_client::time::HostInstant;
use mantis_core::content::ContentHash;
use mantis_core::graph::{
    GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, PackageMarker, TimelineMarker,
};
use mantis_core::log::BuildId;
use mantis_formats::FormatError;
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Motion3 = CoreMotion<FlatGround>;

const AVATAR: EntityId = EntityId::new(1, 1);
const NPC: EntityId = EntityId::new(7, 2);
const BUILD: BuildId = BuildId([0xB1; 32]);
const TICK_NANOS: u64 = 33_333_333;

fn content() -> ContentHash {
    ContentHash::of(b"recording test content")
}

fn at(t: u64) -> HostInstant {
    HostInstant::from_nanos(t * TICK_NANOS)
}

fn marker(kind: MarkerKind, tick: u64, target: Option<EntityId>) -> TimelineMarker {
    TimelineMarker {
        id: MarkerId {
            graph: GraphId::named("ability.test"),
            node: NodeKey(3),
        },
        kind,
        at: Tick(tick + 2),
        offset: 2,
        source: NPC,
        target,
        instance: GraphInstanceId(tick * 10 + 1),
    }
}

/// An action table holding every dense id, so tests can set any bit.
fn all_actions() -> Result<Vec<ActionId>, Box<dyn std::error::Error>> {
    let mut t = ActionTable::new();
    let mut ids = Vec::new();
    for i in 0..MAX_ACTIONS {
        ids.push(t.define(&format!("action{i}"), ActionKind::Button)?);
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------------------
// Format
// ---------------------------------------------------------------------------------------

#[test]
fn round_trip_preserves_every_field() -> TestResult {
    let ids = all_actions()?;
    let id = |i: usize| ids.get(i).copied().ok_or("no id");
    let (mut tx, _rx) = snapshot_channel::<MotionState>(3, 8);
    let mut rec = Recorder::<MotionState>::new(BUILD, content(), 1 << 16);

    let mut full = tx.acquire().ok_or("frame")?;
    full.server_tick = Tick(40);
    full.received_at = HostInstant::from_nanos(123_456_789);
    full.ack = Some(InputSeq(9));
    let state = MotionState {
        position: Vec3::new(1.5, -0.0, 3.25),
        velocity: Vec3::new(0.1, 2.0, -7.0),
        yaw: Angle16(40_000),
        grounded: false,
    };
    full.local = Some((AVATAR, state));
    full.local_mods = MotionModifiers {
        speed_scale: 0.5,
        jump_scale: 1.25,
        gravity_scale: 2.0,
    };
    let nan = f32::from_bits(0x7FC0_1234);
    assert!(full.push_remote(RemoteState {
        id: NPC,
        tick: Tick(39),
        position: Vec3::new(10.0, 0.0, -4.5),
        velocity: Vec3::new(nan, 1.0, 0.0),
        yaw: Angle16(17),
    }));
    assert!(full.push_remote(RemoteState {
        id: EntityId::new(8, 0),
        tick: Tick(40),
        position: Vec3::new(-1.0, 2.0, 3.0),
        velocity: Vec3::ZERO,
        yaw: Angle16(65_535),
    }));
    assert!(full.push_removed(EntityId::new(9, 4)));
    for (k, m) in [
        marker(MarkerKind::CastStart, 40, None),
        marker(
            MarkerKind::Impact {
                target: EntityId::new(8, 0),
            },
            40,
            Some(EntityId::new(8, 0)),
        ),
        marker(MarkerKind::TickN(3), 40, None),
        marker(MarkerKind::Expire, 40, None),
        marker(MarkerKind::Package(PackageMarker(77)), 40, Some(AVATAR)),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(full.push_marker(m), "marker {k}");
    }
    assert!(full.push_entered(EntityId::new(8, 0), 0xDEAD));
    let mut empty = tx.acquire().ok_or("frame")?;
    empty.server_tick = Tick(41);

    let mut held = ActionBits::EMPTY;
    held.insert(id(0)?);
    held.insert(id(127)?);
    let mut pressed = ActionBits::EMPTY;
    pressed.insert(id(64)?);
    let mut released = ActionBits::EMPTY;
    released.insert(id(5)?);
    let mut axes = [0.0f32; MAX_ACTIONS];
    if let Some(a) = axes.get_mut(3) {
        *a = -0.75;
    }
    if let Some(a) = axes.get_mut(120) {
        *a = -0.0;
    }
    let input = TickInput {
        held,
        pressed,
        released,
        axes,
        look: LookSample {
            yaw: Angle16(1234),
            pitch: Angle16(65_000),
        },
        frames: 3,
    };
    let sent = MoveInput {
        seq: InputSeq(10),
        tick: Tick(5),
        buttons: MoveButtons::ALL,
        yaw: Angle16(1234),
        aim: AimAngles {
            yaw: Angle16(1234),
            pitch: Angle16(65_000),
        },
    };
    rec.begin_tick(Tick(5), at(5));
    rec.frame(&full);
    rec.frame(&empty);
    rec.end_tick(&input, Some(&sent), 0x0123_4567_89AB_CDEF);
    rec.begin_tick(Tick(7), at(7));
    rec.end_tick(&TickInput::default(), None, 42);
    assert_eq!(rec.ticks_recorded(), 2);
    assert!(!rec.is_truncated());
    let bytes = rec.finish();

    assert_eq!(bytes.get(..4), Some(&MAGIC[..]));
    let parsed = Recording::<MotionState>::parse(&bytes)?;
    assert_eq!(
        parsed.header,
        RecordingHeader {
            build: BUILD,
            content: content(),
            start_tick: Tick(5),
        }
    );
    assert!(!parsed.truncated);
    assert_eq!(parsed.ticks.len(), 2);
    let t0 = parsed.ticks.first().ok_or("tick 0")?;
    assert_eq!(
        (t0.tick, t0.time, t0.state_hash),
        (Tick(5), at(5), 0x0123_4567_89AB_CDEF)
    );
    assert_eq!(t0.sent, Some(sent));
    assert_eq!(t0.input, input);
    assert_eq!(
        t0.input.axes.get(120).map(|a| a.to_bits()),
        Some((-0.0f32).to_bits())
    );
    assert_eq!(t0.frames.len(), 2);
    let f0 = t0.frames.first().ok_or("frame 0")?;
    assert_eq!(
        (f0.server_tick, f0.received_at, f0.ack),
        (Tick(40), full.received_at, Some(InputSeq(9)))
    );
    let (lid, ls) = f0.local.ok_or("local")?;
    assert_eq!(lid, AVATAR);
    assert!(ls.bits_eq(&state));
    assert_eq!(f0.local_mods, full.local_mods);
    assert_eq!(f0.remotes.len(), 2);
    assert_eq!(
        f0.remotes.first().map(|r| r.velocity.x.to_bits()),
        Some(0x7FC0_1234),
        "NaN payload kept bit for bit"
    );
    assert_eq!(f0.remotes.get(1), full.remotes.get(1));
    assert_eq!(f0.removed, full.removed);
    assert_eq!(f0.markers, full.markers);
    assert_eq!(f0.entered, full.entered);
    let f1 = t0.frames.get(1).ok_or("frame 1")?;
    assert_eq!(f1.server_tick, Tick(41));
    assert!(f1.local.is_none() && f1.remotes.is_empty() && f1.markers.is_empty());
    let t1 = parsed.ticks.get(1).ok_or("tick 1")?;
    assert_eq!((t1.tick, t1.sent, t1.state_hash), (Tick(7), None, 42));
    assert_eq!(t1.input, TickInput::default());

    // Re-encoding reproduces the bytes exactly, and the streaming reader agrees.
    assert_eq!(parsed.encode(), bytes);
    let reader = RecordingReader::<MotionState>::new(&bytes)?;
    assert_eq!(reader.header().start_tick, Tick(5));
    let streamed: Vec<TickRecord<MotionState>> = reader.collect::<Result<_, _>>()?;
    assert_eq!(streamed.len(), 2);
    assert_eq!(streamed.get(1), parsed.ticks.get(1));

    // A channel frame refilled from the recording matches the original.
    let mut again = tx.acquire().ok_or("frame")?;
    assert!(f0.fill(&mut again));
    assert_eq!(again.markers, full.markers);
    assert_eq!(again.entered, full.entered);
    assert_eq!(f0.widest_list(), 5);
    Ok(())
}

#[test]
fn an_empty_recording_round_trips() -> TestResult {
    let bytes = Recorder::<MotionState>::new(BUILD, content(), 0).finish();
    assert_eq!(bytes.len(), HEADER_LEN + END_LEN);
    let parsed = Recording::<MotionState>::parse(&bytes)?;
    assert!(parsed.ticks.is_empty() && !parsed.truncated);
    assert_eq!(parsed.header.start_tick, Tick::ZERO);
    assert_eq!(parsed.encode(), bytes);
    Ok(())
}

/// A frame with no ack and no local, holding one marker and nothing else.
fn bare_frame(server_tick: u64) -> RecordedFrame<MotionState> {
    RecordedFrame {
        server_tick: Tick(server_tick),
        received_at: at(server_tick),
        ack: None,
        local: None,
        local_mods: MotionModifiers::default(),
        remotes: Vec::new(),
        removed: Vec::new(),
        markers: vec![marker(MarkerKind::CastStart, server_tick, None)],
        entered: Vec::new(),
    }
}

fn record(
    tick: u64,
    frames: Vec<RecordedFrame<MotionState>>,
    sent: Option<MoveInput>,
) -> TickRecord<MotionState> {
    TickRecord {
        tick: Tick(tick),
        time: at(tick),
        frames,
        input: TickInput::default(),
        sent,
        state_hash: tick ^ 0x5555,
    }
}

fn recording(ticks: Vec<TickRecord<MotionState>>) -> Recording<MotionState> {
    Recording {
        header: RecordingHeader {
            build: BUILD,
            content: content(),
            start_tick: ticks.first().map_or(Tick::ZERO, |t| t.tick),
        },
        ticks,
        truncated: false,
    }
}

fn parse(bytes: &[u8]) -> Result<Recording<MotionState>, FormatError> {
    Recording::<MotionState>::parse(bytes)
}

fn with(bytes: &[u8], off: usize, patch: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut out = bytes.to_vec();
    out.get_mut(off..off + patch.len())
        .ok_or("patch out of range")?
        .copy_from_slice(patch);
    Ok(out)
}

// Offsets in a recording whose first record holds one bare frame.
const REC: usize = HEADER_LEN; // tag
const FRAME_COUNT: usize = REC + 1 + 8 + 8;
const FRAME: usize = FRAME_COUNT + 4;
const ACK_TAG: usize = FRAME + 16;
const LOCAL_TAG: usize = ACK_TAG + 1;
const REMOTE_COUNT: usize = LOCAL_TAG + 1 + 12;
const REMOVED_COUNT: usize = REMOTE_COUNT + 4;
const MARKER_COUNT: usize = REMOVED_COUNT + 4;
const MARKER_KIND: usize = MARKER_COUNT + 4 + 4 + 2;
const MARKER_LEN: usize = 4 + 2 + 1 + 8 + 4 + 8 + 1 + 8;
const ENTERED_COUNT: usize = MARKER_COUNT + 4 + MARKER_LEN;
const INPUT: usize = ENTERED_COUNT + 4;
const AXIS_COUNT: usize = INPUT + 48;

#[test]
fn every_rejection_rule_rejects_the_whole_recording() -> TestResult {
    let sent = MoveInput {
        seq: InputSeq(1),
        tick: Tick(3),
        buttons: MoveButtons::FORWARD,
        ..MoveInput::default()
    };
    let good = recording(vec![record(3, vec![bare_frame(3)], Some(sent))]).encode();
    let parsed = parse(&good)?;
    assert_eq!(parsed.ticks.len(), 1);
    // The offsets used below point where they claim to.
    assert_eq!(good.get(REC), Some(&1));
    assert_eq!(
        good.get(FRAME_COUNT..FRAME_COUNT + 4),
        Some(&1u32.to_le_bytes()[..])
    );
    assert_eq!(
        good.get(MARKER_COUNT..MARKER_COUNT + 4),
        Some(&1u32.to_le_bytes()[..])
    );
    assert_eq!(good.get(MARKER_KIND), Some(&0));
    assert_eq!(
        good.get(ENTERED_COUNT..ENTERED_COUNT + 4),
        Some(&0u32.to_le_bytes()[..])
    );
    assert_eq!(good.get(AXIS_COUNT), Some(&0));

    // Magic, version, flags.
    for i in 0..4 {
        let mut b = good.clone();
        if let Some(x) = b.get_mut(i) {
            *x ^= 0x20;
        }
        assert_eq!(parse(&b), Err(FormatError::Magic));
    }
    for v in [0u16, 2, VERSION + 1, u16::MAX] {
        assert_eq!(
            parse(&with(&good, 4, &v.to_le_bytes())?),
            Err(FormatError::Version(v))
        );
    }
    for bit in 0..16 {
        let flags = 1u16 << bit;
        assert_eq!(
            parse(&with(&good, 6, &flags.to_le_bytes())?),
            Err(FormatError::Flags(u32::from(flags)))
        );
    }

    // Truncation at every byte, and trailing bytes.
    for n in 0..good.len() {
        assert!(
            parse(good.get(..n).ok_or("prefix")?).is_err(),
            "prefix of {n} bytes accepted"
        );
    }
    let mut long = good.clone();
    long.push(0);
    assert!(matches!(parse(&long), Err(FormatError::Length { .. })));
    let mut doubled = good.clone();
    doubled.extend_from_slice(good.get(good.len() - END_LEN..).ok_or("end")?);
    assert!(parse(&doubled).is_err());

    // Nonsense counts never allocate their claim; they fail.
    for off in [
        FRAME_COUNT,
        REMOTE_COUNT,
        REMOVED_COUNT,
        MARKER_COUNT,
        ENTERED_COUNT,
    ] {
        for n in [u32::MAX, 0x0100_0000] {
            assert_eq!(
                parse(&with(&good, off, &n.to_le_bytes())?),
                Err(FormatError::Dimensions),
                "count at {off}"
            );
        }
    }
    assert_eq!(
        parse(&with(&good, AXIS_COUNT, &[129])?),
        Err(FormatError::Dimensions)
    );
    assert_eq!(
        parse(&with(&good, AXIS_COUNT, &[255])?),
        Err(FormatError::Dimensions)
    );

    // Tags, presence bytes, enums, and bit sets.
    assert_eq!(parse(&with(&good, REC, &[9])?), Err(FormatError::Encoding(9)));
    assert_eq!(parse(&with(&good, REC, &[0])?), Err(FormatError::Encoding(0)));
    assert_eq!(parse(&with(&good, ACK_TAG, &[2])?), Err(FormatError::Validity));
    assert_eq!(parse(&with(&good, LOCAL_TAG, &[7])?), Err(FormatError::Validity));
    assert_eq!(
        parse(&with(&good, MARKER_KIND, &[5])?),
        Err(FormatError::Encoding(5))
    );
    let sent_tag = good.len() - END_LEN - 8 - 20 - 1;
    assert_eq!(good.get(sent_tag), Some(&1));
    assert_eq!(parse(&with(&good, sent_tag, &[3])?), Err(FormatError::Validity));
    let buttons = sent_tag + 1 + 4 + 8;
    assert_eq!(
        good.get(buttons..buttons + 2),
        Some(&MoveButtons::FORWARD.bits().to_le_bytes()[..])
    );
    for raw in [0x0040u16, 0x8000, 0xFFFF] {
        assert_eq!(
            parse(&with(&good, buttons, &raw.to_le_bytes())?),
            Err(FormatError::Flags(u32::from(raw)))
        );
    }

    // The end record: flags, count, presence.
    let end = good.len() - END_LEN;
    assert_eq!(good.get(end), Some(&2));
    assert!(parse(&with(&good, end + 1, &[1])?)?.truncated);
    for f in [2u8, 4, 0x80] {
        assert_eq!(
            parse(&with(&good, end + 1, &[f])?),
            Err(FormatError::Flags(u32::from(f)))
        );
    }
    for c in [0u64, 2, u64::MAX] {
        assert_eq!(
            parse(&with(&good, end + 2, &c.to_le_bytes())?),
            Err(FormatError::Inconsistent)
        );
    }
    assert!(parse(good.get(..end).ok_or("no end")?).is_err());

    // A local state's boolean byte.
    let mut with_local = bare_frame(3);
    with_local.local = Some((AVATAR, MotionState::at_rest(Vec3::ZERO, Angle16(0))));
    let local = recording(vec![record(3, vec![with_local], None)]).encode();
    let grounded = LOCAL_TAG + 1 + 8 + 12 + 12 + 2;
    assert_eq!(local.get(grounded), Some(&1));
    assert!(parse(&local).is_ok());
    assert_eq!(parse(&with(&local, grounded, &[2])?), Err(FormatError::Validity));

    // Tick order and the start tick.
    let same = recording(vec![record(3, Vec::new(), None), record(3, Vec::new(), None)]).encode();
    assert_eq!(parse(&same), Err(FormatError::Inconsistent));
    let back = recording(vec![record(5, Vec::new(), None), record(4, Vec::new(), None)]).encode();
    assert_eq!(parse(&back), Err(FormatError::Inconsistent));
    let mut skewed = recording(vec![record(5, Vec::new(), None)]);
    skewed.header.start_tick = Tick(4);
    assert_eq!(parse(&skewed.encode()), Err(FormatError::Inconsistent));
    let mut empty = recording(Vec::new());
    empty.header.start_tick = Tick(1);
    assert_eq!(parse(&empty.encode()), Err(FormatError::Inconsistent));
    let gaps = recording(vec![record(5, Vec::new(), None), record(9, Vec::new(), None)]).encode();
    assert_eq!(
        parse(&gaps)?.ticks.len(),
        2,
        "skipped ticks are allowed, order is not"
    );

    // A reader stops at the first error.
    let bad = with(&good, MARKER_KIND, &[5])?;
    let mut reader = RecordingReader::<MotionState>::new(&bad)?;
    assert_eq!(reader.next(), Some(Err(FormatError::Encoding(5))));
    assert_eq!(reader.next(), None);
    Ok(())
}

#[test]
fn the_byte_cap_truncates_cleanly() -> TestResult {
    let (mut tx, _rx) = snapshot_channel::<MotionState>(1, 8);
    let mut frame = tx.acquire().ok_or("frame")?;
    for i in 0..8 {
        assert!(frame.push_remote(RemoteState {
            id: EntityId::new(i + 10, 0),
            tick: Tick(1),
            position: Vec3::new(1.0, 2.0, 3.0),
            velocity: Vec3::ZERO,
            yaw: Angle16(0),
        }));
    }
    for cap in [0, MIN_CAP, MIN_CAP + 100, MIN_CAP + 1000, 4096] {
        let mut rec = Recorder::<MotionState>::new(BUILD, content(), cap);
        let ((), stats) = count_allocs(|| {
            for t in 1..100u64 {
                frame.server_tick = Tick(t);
                rec.begin_tick(Tick(t), at(t));
                rec.frame(&frame);
                rec.end_tick(&TickInput::default(), None, t);
            }
        });
        assert!(stats.is_zero(), "cap {cap}: recording allocated: {stats}");
        assert!(rec.is_truncated(), "cap {cap}");
        let kept = rec.ticks_recorded();
        let bytes = rec.finish();
        assert!(
            bytes.len() <= cap.max(MIN_CAP),
            "cap {cap}: {} bytes",
            bytes.len()
        );
        let parsed = parse(&bytes)?;
        assert!(parsed.truncated);
        assert_eq!(parsed.ticks.len() as u64, kept);
        // The records kept are the first ones, intact.
        for (i, t) in parsed.ticks.iter().enumerate() {
            assert_eq!(t.tick, Tick(i as u64 + 1));
            assert_eq!(t.state_hash, i as u64 + 1);
            assert_eq!(t.frames.first().map(|f| f.remotes.len()), Some(8));
        }
    }
    // Out-of-order ticks stop a recording too.
    let mut rec = Recorder::<MotionState>::new(BUILD, content(), 4096);
    rec.begin_tick(Tick(5), at(5));
    rec.end_tick(&TickInput::default(), None, 1);
    rec.begin_tick(Tick(5), at(5));
    rec.end_tick(&TickInput::default(), None, 2);
    rec.begin_tick(Tick(6), at(6));
    rec.end_tick(&TickInput::default(), None, 3);
    let parsed = parse(&rec.finish())?;
    assert!(parsed.truncated);
    assert_eq!(parsed.ticks.len(), 1);
    // An unclosed tick is dropped by finish.
    let mut rec = Recorder::<MotionState>::new(BUILD, content(), 4096);
    rec.begin_tick(Tick(1), at(1));
    let parsed = parse(&rec.finish())?;
    assert!(parsed.ticks.is_empty() && !parsed.truncated);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The simulation hook
// ---------------------------------------------------------------------------------------

/// Counts sent intents without allocating.
#[derive(Default)]
struct LastSent {
    sent: u64,
    last: Option<MoveInput>,
}

impl IntentSink for LastSent {
    fn send_move(&mut self, input: &MoveInput) {
        self.sent += 1;
        self.last = Some(*input);
    }
}

fn rate() -> Result<TickRate, Box<dyn std::error::Error>> {
    TickRate::new(30).ok_or_else(|| "zero rate".into())
}

fn sim_config() -> Result<ClientSimConfig, Box<dyn std::error::Error>> {
    Ok(ClientSimConfig {
        server_rate: rate()?,
        correction_window: Duration::from_millis(100),
        snap_distance: 5.0,
        remote_capacity: 8,
        remote_timeout: Duration::from_secs(2),
        max_remote_extrapolation: Duration::from_millis(250),
        delay: mantis_client::jitter::DelayConfig::default(),
        input_buffer: 64,
        timeline_rise_shift: 4,
        timeline_adaptive: true,
    })
}

/// A client sim with a scripted server: the server applies each sent intent one tick
/// later with the same motion model, and nudges the avatar once to force a correction.
struct Session {
    sim: ClientSim<Motion3, LastSent>,
    snapshots: SnapshotSender<MotionState>,
    accumulator: Arc<InputAccumulator>,
    world: RenderWorld,
    forward: ActionId,
    motion: Motion,
    server: MotionState,
    ack: Option<InputSeq>,
    tick: u64,
    delivered: u64,
    /// Flips the lowest bit of the remote's x position in the next snapshot.
    nudge_remote: bool,
}

impl Session {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut actions = ActionTable::new();
        let forward = actions.define("forward", ActionKind::Button)?;
        let mut intents = MoveIntentMap::new();
        intents.map_button(&actions, forward, MoveButtons::FORWARD)?;
        let motion = Motion::new(MotionParams::DEFAULT)?;
        let (snapshots, inbox) = snapshot_channel(4, 8);
        let accumulator = Arc::new(InputAccumulator::new());
        let config = sim_config()?;
        let sim = ClientSim::new(ClientSimParts {
            config,
            motion: CoreMotion::new(motion),
            ground: Arc::new(FlatGround(0.0)),
            dt: 1.0 / 30.0,
            initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
            accumulator: Arc::clone(&accumulator),
            intents,
            inbox,
            outbox: LastSent::default(),
            markers: None,
        });
        Ok(Self {
            sim,
            snapshots,
            accumulator,
            world: RenderWorld::with_capacity(config.remote_capacity, PresentationConfig::default()),
            forward,
            motion,
            server: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
            ack: None,
            tick: 0,
            delivered: 0,
            nudge_remote: false,
        })
    }

    /// Delivers this tick's snapshot and input (outside any measured scope).
    fn feed(&mut self) {
        let t = self.tick;
        if t >= 2
            && let Some(mut f) = self.snapshots.acquire()
        {
            f.server_tick = Tick(t);
            f.received_at = at(t);
            f.ack = self.ack;
            f.local = Some((AVATAR, self.server));
            let mut x = t as f32 * 0.5;
            if std::mem::take(&mut self.nudge_remote) {
                x = f32::from_bits(x.to_bits() ^ 1);
            }
            let _ = f.push_remote(RemoteState {
                id: NPC,
                tick: Tick(t),
                position: Vec3::new(x, 0.0, 2.0),
                velocity: Vec3::new(15.0, 0.0, 0.0),
                yaw: Angle16(9),
            });
            if t.is_multiple_of(5) {
                let _ = f.push_marker(marker(MarkerKind::TickN((t / 5) as u16), t, None));
            }
            let _ = self.snapshots.send(f);
            self.delivered += 1;
        }
        let mut frame = ActionFrame::default();
        if (t / 7).is_multiple_of(2) {
            frame.held.insert(self.forward);
        }
        let look = LookSample {
            yaw: Angle16((t * 300) as u16),
            pitch: Angle16(0),
        };
        self.accumulator.merge_frame(&frame, look);
    }

    fn run(&mut self) {
        let t = self.tick;
        self.world.begin(Tick(t), at(t));
        let before = self.sim.outbox().sent;
        self.sim.tick(Tick(t), at(t), &mut self.world);
        if self.sim.outbox().sent > before
            && let Some(mv) = self.sim.outbox().last
        {
            self.server = self.motion.step(
                &FlatGround(0.0),
                &self.server,
                &mv,
                &MotionModifiers::default(),
                1.0 / 30.0,
            );
            if t == 12 {
                self.server.position.x += 0.25;
            }
            self.ack = Some(mv.seq);
        }
        self.tick += 1;
    }

    fn step(&mut self) {
        self.feed();
        self.run();
    }
}

#[test]
fn state_hash_is_deterministic_and_covers_remote_tracks() -> TestResult {
    let mut a = Session::new()?;
    let mut b = Session::new()?;
    assert_eq!(a.sim.state_hash(), b.sim.state_hash());
    for _ in 0..20 {
        a.step();
        b.step();
        assert_eq!(a.sim.state_hash(), b.sim.state_hash());
    }
    assert!(
        a.sim.predictor().stats().corrected >= 1,
        "{:?}",
        a.sim.predictor().stats()
    );
    assert_eq!(a.sim.last_server_tick(), Some(Tick(19)));
    // One remote sample differing by one bit changes the hash.
    b.nudge_remote = true;
    a.step();
    b.step();
    assert_eq!(a.sim.remote_count(), 1);
    assert_ne!(a.sim.state_hash(), b.sim.state_hash());
    Ok(())
}

#[cfg(debug_assertions)]
#[test]
fn the_sim_records_every_tick_it_runs() -> TestResult {
    let mut s = Session::new()?;
    assert!(!s.sim.is_recording());
    assert_eq!(s.sim.take_recording(), None);
    s.sim.start_recording(BUILD, content(), 1 << 20);
    assert!(s.sim.is_recording());
    let mut hashes = Vec::new();
    for _ in 0..40 {
        s.step();
        hashes.push(s.sim.state_hash());
    }
    let bytes = s.sim.take_recording().ok_or("no recording")?;
    assert!(!s.sim.is_recording());
    let rec = parse(&bytes)?;
    assert!(!rec.truncated);
    assert_eq!(rec.header.build, BUILD);
    assert_eq!(rec.header.content, content());
    assert_eq!(rec.ticks.len(), 40);
    for (i, (t, h)) in rec.ticks.iter().zip(&hashes).enumerate() {
        assert_eq!(t.tick, Tick(i as u64));
        assert_eq!(t.time, at(i as u64));
        assert_eq!(t.state_hash, *h, "tick {i}");
    }
    let frames: usize = rec.ticks.iter().map(|t| t.frames.len()).sum();
    assert_eq!(frames as u64, s.delivered);
    let sent = rec.ticks.iter().filter(|t| t.sent.is_some()).count();
    assert_eq!(sent as u64, s.sim.stats().moves_sent);
    let markers: usize = rec
        .ticks
        .iter()
        .flat_map(|t| &t.frames)
        .map(|f| f.markers.len())
        .sum();
    assert!(markers >= 7);
    assert!(rec.ticks.iter().any(|t| t.input.held.contains(s.forward)));
    // The recording reflects real prediction work.
    assert!(s.sim.predictor().stats().corrected >= 1);
    Ok(())
}

#[cfg(debug_assertions)]
#[test]
fn the_sim_stops_cleanly_at_its_cap() -> TestResult {
    let mut s = Session::new()?;
    s.sim.start_recording(BUILD, content(), 2048);
    for _ in 0..60 {
        s.step();
    }
    let bytes = s.sim.take_recording().ok_or("no recording")?;
    assert!(bytes.len() <= 2048);
    let rec = parse(&bytes)?;
    assert!(rec.truncated);
    assert!(!rec.ticks.is_empty() && rec.ticks.len() < 60);
    assert_eq!(rec.ticks.first().map(|t| t.tick), Some(Tick(0)));
    Ok(())
}

#[test]
fn a_tick_allocates_nothing_with_recording_off() -> TestResult {
    let mut s = Session::new()?;
    for _ in 0..30 {
        s.step();
    }
    for _ in 0..40 {
        s.feed();
        assert_no_alloc("client sim tick, recording off", || s.run());
    }
    assert!(s.sim.stats().snapshots > 50);
    Ok(())
}

#[cfg(debug_assertions)]
#[test]
fn a_tick_allocates_nothing_with_recording_on() -> TestResult {
    let mut s = Session::new()?;
    for _ in 0..30 {
        s.step();
    }
    s.sim.start_recording(BUILD, content(), 1 << 20);
    for _ in 0..40 {
        s.feed();
        assert_no_alloc("client sim tick, recording on", || s.run());
    }
    let rec = parse(&s.sim.take_recording().ok_or("no recording")?)?;
    assert_eq!(rec.ticks.len(), 40);
    assert_eq!(rec.header.start_tick, Tick(30));
    Ok(())
}
