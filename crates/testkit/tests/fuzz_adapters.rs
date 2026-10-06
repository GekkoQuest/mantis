//! Structure-aware fuzzing of every adapter's frame decoders (plan 6.7, 15,
//! M9): the native adapter (client frames, and server frames with full and
//! delta snapshots) and the toy package's legacy TCP opcode protocol (client
//! packets through the adapter, and server packets through its client half).
//!
//! Seeds are valid frames built from structurally valid values. Each is
//! mutated (`support::mutate`) many times, and every input must be refused
//! with an error, or accepted and then hold the canonical round trip:
//! decode, encode, decode again, encode again gives the same bytes (bytes,
//! not values, so a NaN cannot hide behind `!=`). A panic anywhere fails the
//! test. Seeded and deterministic; `MANTIS_FUZZ_ITERS` sets the iterations
//! for long runs (`crates/testkit/ci/fuzz-long.md`).

#![expect(clippy::cast_possible_truncation)]

mod support;

use mantis_adapter_contract::core_types::{
    Angle16, ContentHash, EntityId, FuzzSample, GraphId, GraphInstanceId, InputSeq, MarkerId, MarkerKind,
    MotionModifiers, MotionState, NodeKey, PackageMarker, Tick, TimelineMarker, Vec3,
};
use mantis_adapter_contract::native::{
    BaselineStore, NoBaseline, ServerFrame, decode_server_frame, encode_inbound, encode_outbound_frame,
};
use mantis_adapter_contract::{
    AppearanceId, Cast, Choose, Extension, ExtensionMessage, ExtensionRefused, FeatureState, Goodbye, Hello,
    Inbound, Interact, LocalAvatar, Move, MoveClaim, Outbound, PermittedModules, Refuse, RemoteBases,
    RemoteSample, SetPosition, SnapshotAck, SnapshotFrame, SnapshotHeader, Welcome, WireAdapter,
};
use mantis_core::rng::{Rng, Salt, Seed};
use support::{iterations, mutate};
use toy_adapter_legacy::client::{
    ServerPacket, decode_server, encode_inbound as legacy_inbound, encode_server,
};
use toy_adapter_legacy::{LegacyAdapter, LegacyConfig};

const BUILD: u32 = 0x0001_0007;

fn rng(name: &str) -> Rng {
    Rng::for_cell(Seed(support::seed(0xADA7)), Tick(0), Salt::named(name))
}

fn coord(rng: &mut Rng) -> f32 {
    (rng.next_f32() - 0.5) * 2000.0
}

fn vec3(rng: &mut Rng) -> Vec3 {
    Vec3::new(coord(rng), coord(rng), coord(rng))
}

fn inbound(rng: &mut Rng) -> Inbound {
    match rng.below(9) {
        0 => Inbound::Hello(Hello::fuzz_sample(rng)),
        1 => Inbound::Move(Move::fuzz_sample(rng)),
        2 => Inbound::MoveClaim(MoveClaim::fuzz_sample(rng)),
        3 => Inbound::Cast(Cast::fuzz_sample(rng)),
        4 => Inbound::Interact(Interact::fuzz_sample(rng)),
        5 => Inbound::Choose(Choose::fuzz_sample(rng)),
        6 => Inbound::Extension(Extension::fuzz_sample(rng)),
        7 => Inbound::SnapshotAck(SnapshotAck::fuzz_sample(rng)),
        _ => Inbound::Goodbye(Goodbye::fuzz_sample(rng)),
    }
}

fn outbound(rng: &mut Rng) -> Outbound {
    match rng.below(7) {
        0 => Outbound::Welcome(Welcome::fuzz_sample(rng)),
        1 => Outbound::Refuse(Refuse::fuzz_sample(rng)),
        2 => Outbound::SetPosition(SetPosition::fuzz_sample(rng)),
        3 => Outbound::ExtensionRefused(ExtensionRefused::fuzz_sample(rng)),
        4 => Outbound::ExtensionMessage(ExtensionMessage::fuzz_sample(rng)),
        5 => Outbound::FeatureState(FeatureState::fuzz_sample(rng)),
        _ => Outbound::PermittedModules(PermittedModules::fuzz_sample(rng)),
    }
}

fn entity(rng: &mut Rng, index: u32) -> EntityId {
    EntityId::new(index, rng.below(4))
}

/// One of the first 24 entities after `base`.
fn near(rng: &mut Rng, base: u32) -> EntityId {
    let i = base + rng.below(24);
    entity(rng, i)
}

/// A structurally valid snapshot at `tick`: distinct ids per list, remote
/// samples no newer than the snapshot, finite values.
fn snapshot(rng: &mut Rng, tick: u64) -> SnapshotFrame {
    let mut f = SnapshotFrame::with_capacity(16, 32, 16, 8);
    f.header = SnapshotHeader {
        server_tick: Tick(tick),
        ack: rng.chance(1, 2).then(|| InputSeq(rng.next_u32())),
        local: rng.chance(2, 3).then(|| LocalAvatar {
            id: entity(rng, 0),
            state: MotionState {
                position: vec3(rng),
                velocity: vec3(rng),
                yaw: Angle16(rng.below(65_536) as u16),
                grounded: rng.chance(1, 2),
            },
        }),
        local_mods: if rng.chance(1, 3) {
            MotionModifiers {
                speed_scale: rng.next_f32() * 2.0,
                jump_scale: rng.next_f32() * 2.0,
                gravity_scale: rng.next_f32() * 2.0,
            }
        } else {
            MotionModifiers::NONE
        },
    };
    let base = rng.below(1000);
    for i in 0..rng.below(8) {
        let _ = f
            .entered
            .push((entity(rng, base + i), AppearanceId(rng.below(50))));
    }
    for i in 0..rng.below(24) {
        let lag = u64::from(rng.below(4)).min(tick);
        let _ = f.remotes.push(RemoteSample {
            id: entity(rng, base + i),
            tick: Tick(tick - lag),
            position: vec3(rng),
            velocity: vec3(rng),
            yaw: Angle16(rng.below(65_536) as u16),
        });
    }
    for i in 0..rng.below(8) {
        let _ = f.removed.push(entity(rng, base + 2000 + i));
    }
    for _ in 0..rng.below(6) {
        let kind = match rng.below(5) {
            0 => MarkerKind::CastStart,
            1 => MarkerKind::Impact {
                target: near(rng, base),
            },
            2 => MarkerKind::TickN(rng.below(100) as u16 + 1),
            3 => MarkerKind::Expire,
            _ => MarkerKind::Package(PackageMarker(rng.below(1000) as u16)),
        };
        let offset = rng.below(60);
        let _ = f.markers.push(TimelineMarker {
            id: MarkerId {
                graph: GraphId(rng.next_u32()),
                node: NodeKey(rng.below(64) as u16),
            },
            kind,
            at: Tick(tick + u64::from(rng.below(10))),
            offset,
            source: near(rng, base),
            target: rng.chance(1, 2).then(|| near(rng, base)),
            instance: GraphInstanceId(u64::from(rng.next_u32())),
        });
    }
    f
}

/// Moves a snapshot a few ticks on: some remotes move, keeping ids.
fn later(rng: &mut Rng, base: &SnapshotFrame) -> SnapshotFrame {
    let tick = base.header.server_tick.0 + 1 + u64::from(rng.below(5));
    let mut f = SnapshotFrame::with_capacity(16, 32, 16, 8);
    f.header = base.header;
    f.header.server_tick = Tick(tick);
    for r in base.remotes.iter() {
        let mut r = *r;
        if rng.chance(1, 2) {
            r.position = vec3(rng);
            r.tick = Tick(tick);
        }
        let _ = f.remotes.push(r);
    }
    f
}

/// From `base`: a frame-level baseline after it that lost some remotes (the
/// budget dropped them), and a frame after that carrying them all again, so
/// the dropped ones delta against `base` (`MASK_OWN_BASE`).
fn dropped_and_back(rng: &mut Rng, base: &SnapshotFrame) -> (SnapshotFrame, SnapshotFrame) {
    let full = later(rng, base);
    let mut mid = SnapshotFrame::with_capacity(16, 32, 16, 8);
    mid.header = full.header;
    for r in full.remotes.iter() {
        if rng.chance(1, 2) {
            let _ = mid.remotes.push(*r);
        }
    }
    let next = later(rng, &full);
    (mid, next)
}

/// Per-remote baselines: one acknowledged frame.
struct OneBase<'a>(&'a SnapshotFrame);

impl RemoteBases for OneBase<'_> {
    fn base_for(&self, id: EntityId) -> Option<(Tick, &RemoteSample)> {
        self.0.find_remote(id).map(|r| (self.0.header.server_tick, r))
    }
}

// ---- the native adapter ----------------------------------------------------

fn native_client(a: &dyn WireAdapter, bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let mut msgs = Vec::new();
    a.decode(bytes, &mut |m| msgs.push(m)).map_err(|_| ())?;
    let mut out = Vec::new();
    for m in &msgs {
        encode_inbound(m, &mut out);
    }
    Ok(out)
}

/// One server frame decoded and re-encoded canonically (a snapshot as a
/// full snapshot).
fn native_server(bytes: &[u8], baselines: &(impl BaselineStore + ?Sized)) -> Result<Vec<u8>, ()> {
    let a = toy_adapter_native::adapter();
    let mut scratch = SnapshotFrame::with_capacity(1024, 1024, 1024, 256);
    let mut out = Vec::new();
    match decode_server_frame(bytes, baselines, &mut scratch).map_err(|_| ())? {
        ServerFrame::Message(m) => encode_outbound_frame(&m, &mut out),
        ServerFrame::Snapshot => a.encode_snapshot(&scratch, None, &mut out).map_err(|_| ())?,
    }
    Ok(out)
}

/// A decoder and re-encoder: canonical bytes, or a refusal.
type Canon<'a> = dyn FnMut(&[u8]) -> Result<Vec<u8>, ()> + 'a;

fn fixed_point(what: &str, input: &[u8], canon: &mut Canon<'_>) -> Result<bool, String> {
    let Ok(first) = canon(input) else {
        return Ok(false);
    };
    let second =
        canon(&first).map_err(|()| format!("{what}: accepted {input:02x?}, refused its re-encoding"))?;
    if second != first {
        return Err(format!(
            "{what}: accepted {input:02x?}, but its re-encoding does not round-trip"
        ));
    }
    Ok(true)
}

#[test]
fn native_client_frames_refuse_or_round_trip() -> Result<(), String> {
    let a = toy_adapter_native::adapter();
    let mut rng = rng("testkit.fuzz.native.client");
    let mut accepted = 0u32;
    for _ in 0..iterations() * 4 {
        let msg = inbound(&mut rng);
        let mut seed = Vec::new();
        encode_inbound(&msg, &mut seed);
        let mut canon = |b: &[u8]| native_client(&a, b);
        if !fixed_point("native client seed", &seed, &mut canon)? {
            return Err(format!("native client: valid {msg:?} refused"));
        }
        for _ in 0..8 {
            let bad = mutate(&mut rng, &seed);
            accepted += u32::from(fixed_point("native client", &bad, &mut canon)?);
        }
    }
    eprintln!("fuzz: native client frames: {accepted} mutations accepted, all round-trip");
    Ok(())
}

#[test]
fn native_server_frames_and_delta_snapshots_refuse_or_round_trip() -> Result<(), String> {
    let a = toy_adapter_native::adapter();
    let mut rng = rng("testkit.fuzz.native.server");
    let mut accepted = 0u32;
    for i in 0..iterations() {
        // A message frame.
        let mut seed = Vec::new();
        a.encode_outbound(&outbound(&mut rng), &mut seed)
            .map_err(|e| e.to_string())?;
        let mut canon = |b: &[u8]| native_server(b, &NoBaseline);
        if !fixed_point("native message seed", &seed, &mut canon)? {
            return Err("native server: a valid message refused".to_owned());
        }
        for _ in 0..4 {
            let bad = mutate(&mut rng, &seed);
            accepted += u32::from(fixed_point("native message", &bad, &mut canon)?);
        }
        // A full snapshot, then a delta against it.
        let base = snapshot(&mut rng, 100 + u64::from(i));
        let mut full = Vec::new();
        a.encode_snapshot(&base, None, &mut full)
            .map_err(|e| e.to_string())?;
        if !fixed_point("native full snapshot seed", &full, &mut canon)? {
            return Err(format!("native server: a valid full snapshot refused: {base:?}"));
        }
        for _ in 0..8 {
            let bad = mutate(&mut rng, &full);
            accepted += u32::from(fixed_point("native full snapshot", &bad, &mut canon)?);
        }
        let next = later(&mut rng, &base);
        let mut delta = Vec::new();
        a.encode_snapshot(&next, Some(&base), &mut delta)
            .map_err(|e| e.to_string())?;
        let store: &[SnapshotFrame] = std::slice::from_ref(&base);
        let mut with_base = |b: &[u8]| native_server(b, store);
        if !fixed_point("native delta snapshot seed", &delta, &mut with_base)? {
            return Err("native server: a valid delta snapshot refused".to_owned());
        }
        if native_server(&delta, &NoBaseline).is_ok() {
            return Err("native server: a delta decoded without its baseline".to_owned());
        }
        for _ in 0..8 {
            let bad = mutate(&mut rng, &delta);
            accepted += u32::from(fixed_point("native delta snapshot", &bad, &mut with_base)?);
        }
        // Remotes missing from the frame-level baseline, delta against their
        // own acknowledged frame.
        let (mid, back) = dropped_and_back(&mut rng, &base);
        let mut own = Vec::new();
        a.encode_snapshot_based(&back, Some(&mid), &OneBase(&base), &mut own)
            .map_err(|e| e.to_string())?;
        let both = [base.clone(), mid.clone()];
        let mut with_both = |b: &[u8]| native_server(b, &both[..]);
        if !fixed_point("native own-base snapshot seed", &own, &mut with_both)? {
            return Err("native server: a valid own-base snapshot refused".to_owned());
        }
        let dropped = back.remotes.iter().any(|r| mid.find_remote(r.id).is_none());
        if dropped && native_server(&own, std::slice::from_ref(&mid)).is_ok() {
            return Err("native server: an own-base delta decoded without its base frame".to_owned());
        }
        for _ in 0..8 {
            let bad = mutate(&mut rng, &own);
            accepted += u32::from(fixed_point("native own-base snapshot", &bad, &mut with_both)?);
        }
    }
    eprintln!("fuzz: native server frames: {accepted} mutations accepted, all round-trip");
    Ok(())
}

// ---- the toy legacy TCP opcode protocol -------------------------------------

fn legacy() -> LegacyAdapter {
    LegacyAdapter::new(LegacyConfig {
        build: BUILD,
        content: ContentHash::from_bytes([3; 32]),
    })
}

/// Client packets through the adapter, re-encoded as the legacy client
/// would send them (messages the protocol has no form for are refused).
fn legacy_client(a: &LegacyAdapter, bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let mut msgs = Vec::new();
    a.decode(bytes, &mut |m| msgs.push(m)).map_err(|_| ())?;
    let mut out = Vec::new();
    for m in &msgs {
        if !legacy_inbound(m, BUILD, &mut out) {
            return Err(());
        }
    }
    Ok(out)
}

/// Server packets through the legacy client's decoder, re-encoded.
fn legacy_server(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let mut packets: Vec<ServerPacket> = Vec::new();
    decode_server(bytes, |p| packets.push(p)).map_err(|_| ())?;
    let mut out = Vec::new();
    for p in &packets {
        encode_server(p, &mut out).map_err(|_| ())?;
    }
    Ok(out)
}

#[test]
fn legacy_client_packets_refuse_or_round_trip() -> Result<(), String> {
    let a = legacy();
    let mut rng = rng("testkit.fuzz.legacy.client");
    let mut accepted = 0u32;
    let mut seeds = 0u32;
    for _ in 0..iterations() * 4 {
        // One to three packets per frame, as a TCP read may carry.
        let mut seed = Vec::new();
        for _ in 0..=rng.below(3) {
            let _ = legacy_inbound(&inbound(&mut rng), BUILD, &mut seed);
        }
        if seed.is_empty() {
            continue;
        }
        seeds += 1;
        let mut canon = |b: &[u8]| legacy_client(&a, b);
        // A seed may itself be refused (a sample the protocol cannot carry,
        // such as an over-long token); it must still not panic.
        let _ = fixed_point("legacy client seed", &seed, &mut canon)?;
        for _ in 0..8 {
            let bad = mutate(&mut rng, &seed);
            accepted += u32::from(fixed_point("legacy client", &bad, &mut canon)?);
        }
    }
    eprintln!("fuzz: legacy client packets: {seeds} seeds, {accepted} mutations accepted, all round-trip");
    assert!(accepted > 0, "the mutator reached accepting paths");
    Ok(())
}

#[test]
fn legacy_server_packets_refuse_or_round_trip() -> Result<(), String> {
    let a = legacy();
    let mut rng = rng("testkit.fuzz.legacy.server");
    let mut accepted = 0u32;
    for i in 0..iterations() {
        let mut seed = Vec::new();
        // What the server writes: a snapshot's packets and a message.
        a.encode_snapshot(&snapshot(&mut rng, 50 + u64::from(i)), None, &mut seed)
            .map_err(|e| e.to_string())?;
        // A message the protocol cannot carry (an entity with no object
        // id) is refused whole: nothing is written.
        let before = seed.len();
        if a.encode_outbound(&outbound(&mut rng), &mut seed).is_err() && seed.len() != before {
            return Err("legacy server: a refused message wrote bytes".to_owned());
        }
        if !fixed_point("legacy server seed", &seed, &mut legacy_server)? {
            let why = decode_server(&seed, |_| {}).err();
            return Err(format!(
                "legacy server: a frame the server wrote was refused: {why:?}"
            ));
        }
        for _ in 0..8 {
            let bad = mutate(&mut rng, &seed);
            accepted += u32::from(fixed_point("legacy server", &bad, &mut legacy_server)?);
        }
    }
    eprintln!("fuzz: legacy server packets: {accepted} mutations accepted, all round-trip");
    Ok(())
}

#[test]
fn garbage_never_panics_any_adapter() {
    let native = toy_adapter_native::adapter();
    let legacy = legacy();
    let mut rng = rng("testkit.fuzz.adapters.garbage");
    let base = snapshot(&mut rng, 10);
    let store: &[SnapshotFrame] = std::slice::from_ref(&base);
    for _ in 0..iterations() * 10 {
        let n = rng.below(96) as usize;
        let bytes: Vec<u8> = (0..n).map(|_| rng.next_u32() as u8).collect();
        let _ = native_client(&native, &bytes);
        let _ = native_server(&bytes, store);
        let _ = legacy_client(&legacy, &bytes);
        let _ = legacy_server(&bytes);
    }
}
