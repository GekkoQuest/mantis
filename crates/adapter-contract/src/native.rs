//! The engine's native protocol codec (plan 9): frame envelope, delta
//! snapshots, and [`NativeAdapter`], the Predictive adapter a package
//! configures for the native client (plan 14). Transport-independent; the
//! QUIC transport lives in `mantis-net`.
//!
//! # Frames (version 1)
//!
//! Every frame starts with a kind byte:
//! - `1`: message: `id: u16`, then the message payload (`contract.idl`);
//! - `2`: snapshot, encoded as below.
//!
//! # Snapshots
//!
//! Integers in the fixed fields are little-endian; `v` is an unsigned LEB128
//! varint and `z` a zigzag varint; an entity id is `index v, generation v`.
//!
//! ```text
//! server_tick u64 | baseline_lag v (0: none, else server_tick - baseline tick)
//! flags u8 (bit0 ack, bit1 local, bit2 mods)
//! [ack u32] [local: entity u64, pos 3xf32, vel 3xf32, yaw u16, grounded u8] [mods 3xf32]
//! entered v x (entity id, appearance u32)
//! remotes v x (entity id, tick_lag v, mask u8, [pos 3z], [vel 3z], [yaw u16])
//! removed v x (entity id)
//! markers v x marker
//! ```
//!
//! Remote positions are quantized to 1/64 unit and velocities to 1/256 unit
//! per second, clamped to +/-2^24 steps so dequantization is exact. Mask bit 3
//! means "delta against the baseline's sample for this id": the present
//! components are differences from it, and absent components are copied from
//! it. Without bit 3, mask bits 0 to 2 must all be set. The local avatar is
//! never quantized (the snapshot contract requires full precision).
//!
//! The decoder parses the whole frame once to validate it (bounds, duplicate
//! ids, finite floats, known tags, a present baseline, no trailing bytes) and
//! only then visits it.

use mantis_core::ecs::EntityId;
use mantis_core::graph::{
    GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, PackageMarker, TimelineMarker,
};
use mantis_core::kinematics::{Angle16, InputSeq, MotionModifiers, MotionState};
use mantis_core::math::Vec3;
use mantis_core::time::Tick;
use mantis_core::wire::{DecodeError, Decoder, Encoder, MessageId, Wire};

use crate::snapshot::{LocalAvatar, RemoteSample, SnapshotFrame, SnapshotHeader, SnapshotVisitor};
use crate::transport::TransportKind;
use crate::{
    AdapterError, AppearanceId, Inbound, MovementMode, Outbound, WireAdapter, decode_outbound, parse_inbound,
};

/// Frame kind: a contract message.
pub const FRAME_MESSAGE: u8 = 1;
/// Frame kind: a snapshot.
pub const FRAME_SNAPSHOT: u8 = 2;

/// Largest counts a snapshot may carry (decoder bounds).
pub const MAX_ENTERED: usize = 1024;
/// See [`MAX_ENTERED`].
pub const MAX_REMOTES: usize = 1024;
/// See [`MAX_ENTERED`].
pub const MAX_REMOVED: usize = 1024;
/// See [`MAX_ENTERED`].
pub const MAX_MARKERS: usize = 256;

const POS_SCALE: f32 = 64.0;
const VEL_SCALE: f32 = 256.0;
const Q_LIMIT: i32 = 1 << 24;
const Q_LIMIT_F: f32 = 16_777_216.0;

const MASK_POS: u8 = 1;
const MASK_VEL: u8 = 2;
const MASK_YAW: u8 = 4;
const MASK_DELTA: u8 = 8;
const MASK_FULL: u8 = MASK_POS | MASK_VEL | MASK_YAW;

// ---- varints -------------------------------------------------------------

fn put_v(e: &mut Encoder<'_>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            e.u8(byte);
            return;
        }
        e.u8(byte | 0x80);
    }
}

fn get_v(d: &mut Decoder<'_>) -> Result<u64, DecodeError> {
    let mut v: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = d.u8()?;
        let bits = u64::from(byte & 0x7F);
        if shift == 63 && bits > 1 {
            return Err(DecodeError::Invalid("varint overflow"));
        }
        v |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(DecodeError::Invalid("varint overflow"))
}

fn put_z(e: &mut Encoder<'_>, v: i64) {
    put_v(e, ((v << 1) ^ (v >> 63)).cast_unsigned());
}

fn get_z(d: &mut Decoder<'_>) -> Result<i64, DecodeError> {
    let u = get_v(d)?;
    Ok((u >> 1).cast_signed() ^ -((u & 1).cast_signed()))
}

/// An entity id as `index v, generation v` (usually two bytes).
fn put_id(e: &mut Encoder<'_>, id: EntityId) {
    put_v(e, u64::from(id.index()));
    put_v(e, u64::from(id.generation()));
}

fn get_id(d: &mut Decoder<'_>) -> Result<EntityId, DecodeError> {
    let index = u32::try_from(get_v(d)?).map_err(|_| DecodeError::Invalid("entity index"))?;
    let generation = u32::try_from(get_v(d)?).map_err(|_| DecodeError::Invalid("entity generation"))?;
    Ok(EntityId::new(index, generation))
}

fn get_count(d: &mut Decoder<'_>, max: usize) -> Result<usize, DecodeError> {
    let n = get_v(d)?;
    usize::try_from(n)
        .ok()
        .filter(|n| *n <= max)
        .ok_or(DecodeError::Invalid("count over bound"))
}

// ---- quantization ----------------------------------------------------------

#[allow(clippy::cast_possible_truncation)] // clamped into i32 range first
fn quant(x: f32, scale: f32) -> i32 {
    let q = (x * scale).round();
    if q.is_nan() {
        return 0;
    }
    q.clamp(-Q_LIMIT_F, Q_LIMIT_F) as i32
}

#[allow(clippy::cast_precision_loss)] // |q| <= 2^24: exact in f32
fn dequant(q: i32, scale: f32) -> f32 {
    q as f32 / scale
}

fn quant3(v: Vec3, scale: f32) -> [i32; 3] {
    [quant(v.x, scale), quant(v.y, scale), quant(v.z, scale)]
}

fn dequant3(q: [i32; 3], scale: f32) -> Vec3 {
    Vec3::new(dequant(q[0], scale), dequant(q[1], scale), dequant(q[2], scale))
}

fn check_q(q: i64) -> Result<i32, DecodeError> {
    i32::try_from(q)
        .ok()
        .filter(|q| (-Q_LIMIT..=Q_LIMIT).contains(q))
        .ok_or(DecodeError::Invalid("quantized value out of range"))
}

// ---- frames -----------------------------------------------------------------

/// Appends a message frame.
pub fn encode_message_frame(id: MessageId, payload_writer: impl FnOnce(&mut Vec<u8>), out: &mut Vec<u8>) {
    let mut e = Encoder::new(out);
    e.u8(FRAME_MESSAGE);
    e.u16(id.0);
    payload_writer(out);
}

/// Appends an inbound message frame (client side).
pub fn encode_inbound(msg: &Inbound, out: &mut Vec<u8>) {
    encode_message_frame(msg.id(), |o| msg.encode(o), out);
}

/// Appends an outbound message frame (server side).
pub fn encode_outbound_frame(msg: &Outbound, out: &mut Vec<u8>) {
    encode_message_frame(msg.id(), |o| msg.encode(o), out);
}

/// A server-to-client frame, decoded.
#[allow(clippy::large_enum_variant)] // returned by value from the decoder; never stored in bulk
pub enum ServerFrame {
    /// A session message.
    Message(Outbound),
    /// A snapshot was decoded and delivered to the visitor.
    Snapshot,
}

/// Where a decoder finds delta baselines: the frames the client received
/// recently, by server tick.
pub trait BaselineStore {
    /// The frame for `tick`, if retained.
    fn baseline(&self, tick: Tick) -> Option<&SnapshotFrame>;
}

/// No baselines: only non-delta snapshots decode.
pub struct NoBaseline;

impl BaselineStore for NoBaseline {
    fn baseline(&self, _tick: Tick) -> Option<&SnapshotFrame> {
        None
    }
}

impl BaselineStore for [SnapshotFrame] {
    fn baseline(&self, tick: Tick) -> Option<&SnapshotFrame> {
        self.iter().find(|f| f.header.server_tick == tick)
    }
}

impl BaselineStore for SnapshotFrame {
    fn baseline(&self, tick: Tick) -> Option<&SnapshotFrame> {
        (self.header.server_tick == tick).then_some(self)
    }
}

/// Decodes one server-to-client frame. Snapshots go to `visitor`; messages
/// are returned.
///
/// # Errors
/// [`DecodeError`] for any malformed frame, an unknown kind, or a missing
/// baseline.
pub fn decode_server_frame(
    bytes: &[u8],
    baselines: &(impl BaselineStore + ?Sized),
    visitor: &mut impl SnapshotVisitor,
) -> Result<ServerFrame, DecodeError> {
    let (kind, rest) = bytes.split_first().ok_or(DecodeError::UnexpectedEnd)?;
    match *kind {
        FRAME_MESSAGE => {
            let mut d = Decoder::new(rest);
            let id = MessageId(d.u16()?);
            let payload = d.take(d.remaining())?;
            decode_outbound(id, payload)
                .map(ServerFrame::Message)
                .map_err(|_| DecodeError::Invalid("outbound message"))
        }
        FRAME_SNAPSHOT => {
            decode_snapshot(rest, baselines, visitor)?;
            Ok(ServerFrame::Snapshot)
        }
        _ => Err(DecodeError::Invalid("frame kind")),
    }
}

/// The server tick and baseline tick of an encoded snapshot body (after the
/// frame kind byte), without decoding the rest.
///
/// # Errors
/// [`DecodeError`] for a malformed prefix.
pub fn peek_snapshot_ticks(body: &[u8]) -> Result<(Tick, Option<Tick>), DecodeError> {
    let mut d = Decoder::new(body);
    let tick = Tick(d.u64()?);
    let lag = get_v(&mut d)?;
    let baseline = if lag == 0 {
        None
    } else {
        Some(Tick(
            tick.0
                .checked_sub(lag)
                .ok_or(DecodeError::Invalid("baseline lag"))?,
        ))
    };
    Ok((tick, baseline))
}

// ---- snapshot encoding ------------------------------------------------------

fn encode_marker(e: &mut Encoder<'_>, m: &TimelineMarker, server_tick: Tick) {
    e.u32(m.id.graph.0);
    e.u16(m.id.node.0);
    match m.kind {
        MarkerKind::CastStart => e.u8(0),
        MarkerKind::Impact { target } => {
            e.u8(1);
            e.u64(target.to_bits());
        }
        MarkerKind::TickN(n) => {
            e.u8(2);
            e.u16(n);
        }
        MarkerKind::Expire => e.u8(3),
        MarkerKind::Package(p) => {
            e.u8(4);
            e.u16(p.0);
        }
    }
    put_z(
        e,
        i64::try_from(i128::from(m.at.0) - i128::from(server_tick.0)).unwrap_or(0),
    );
    put_v(e, u64::from(m.offset));
    e.u64(m.source.to_bits());
    m.target.encode(e);
    e.u64(m.instance.0);
}

/// Encodes a snapshot body (after the frame kind byte). `baseline`, when
/// given, must be a frame the client acknowledged; remotes present in it are
/// delta-encoded against it.
pub fn encode_snapshot(frame: &SnapshotFrame, baseline: Option<&SnapshotFrame>, out: &mut Vec<u8>) {
    let h = &frame.header;
    let mut e = Encoder::new(out);
    e.u64(h.server_tick.0);
    let lag = baseline
        .and_then(|b| h.server_tick.checked_sub(b.header.server_tick))
        .filter(|lag| *lag > 0);
    let baseline = baseline.filter(|_| lag.is_some());
    put_v(&mut e, lag.unwrap_or(0));
    let mods_default = h.local_mods == MotionModifiers::NONE;
    let flags =
        u8::from(h.ack.is_some()) | (u8::from(h.local.is_some()) << 1) | (u8::from(!mods_default) << 2);
    e.u8(flags);
    if let Some(ack) = h.ack {
        e.u32(ack.0);
    }
    if let Some(local) = h.local {
        e.u64(local.id.to_bits());
        local.state.position.encode(&mut e);
        local.state.velocity.encode(&mut e);
        e.u16(local.state.yaw.0);
        e.bool(local.state.grounded);
    }
    if !mods_default {
        e.f32(h.local_mods.speed_scale);
        e.f32(h.local_mods.jump_scale);
        e.f32(h.local_mods.gravity_scale);
    }
    put_v(&mut e, frame.entered.len() as u64);
    for (id, appearance) in frame.entered.iter() {
        put_id(&mut e, *id);
        e.u32(appearance.0);
    }
    put_v(&mut e, frame.remotes.len() as u64);
    for r in frame.remotes.iter() {
        put_id(&mut e, r.id);
        put_v(&mut e, h.server_tick.saturating_sub(r.tick));
        let pos = quant3(r.position, POS_SCALE);
        let vel = quant3(r.velocity, VEL_SCALE);
        let base = baseline.and_then(|b| b.find_remote(r.id));
        if let Some(b) = base {
            let bpos = quant3(b.position, POS_SCALE);
            let bvel = quant3(b.velocity, VEL_SCALE);
            let mask = MASK_DELTA
                | if pos == bpos { 0 } else { MASK_POS }
                | if vel == bvel { 0 } else { MASK_VEL }
                | if r.yaw == b.yaw { 0 } else { MASK_YAW };
            e.u8(mask);
            if mask & MASK_POS != 0 {
                for (q, bq) in pos.iter().zip(bpos) {
                    put_z(&mut e, i64::from(*q) - i64::from(bq));
                }
            }
            if mask & MASK_VEL != 0 {
                for (q, bq) in vel.iter().zip(bvel) {
                    put_z(&mut e, i64::from(*q) - i64::from(bq));
                }
            }
            if mask & MASK_YAW != 0 {
                e.u16(r.yaw.0);
            }
        } else {
            e.u8(MASK_FULL);
            for q in pos.iter().chain(&vel) {
                put_z(&mut e, i64::from(*q));
            }
            e.u16(r.yaw.0);
        }
    }
    put_v(&mut e, frame.removed.len() as u64);
    for id in frame.removed.iter() {
        put_id(&mut e, *id);
    }
    put_v(&mut e, frame.markers.len() as u64);
    for m in frame.markers.iter() {
        encode_marker(&mut e, m, h.server_tick);
    }
}

// ---- snapshot decoding ------------------------------------------------------

fn decode_marker(d: &mut Decoder<'_>, server_tick: Tick) -> Result<TimelineMarker, DecodeError> {
    let id = MarkerId {
        graph: GraphId(d.u32()?),
        node: NodeKey(d.u16()?),
    };
    let kind = match d.u8()? {
        0 => MarkerKind::CastStart,
        1 => MarkerKind::Impact {
            target: EntityId::from_bits(d.u64()?),
        },
        2 => MarkerKind::TickN(d.u16()?),
        3 => MarkerKind::Expire,
        4 => MarkerKind::Package(PackageMarker(d.u16()?)),
        _ => return Err(DecodeError::Invalid("marker kind")),
    };
    let at = i128::from(server_tick.0) + i128::from(get_z(d)?);
    let at = u64::try_from(at).map_err(|_| DecodeError::Invalid("marker tick"))?;
    let offset = u32::try_from(get_v(d)?).map_err(|_| DecodeError::Invalid("marker offset"))?;
    Ok(TimelineMarker {
        id,
        kind,
        at: Tick(at),
        offset,
        source: EntityId::from_bits(d.u64()?),
        target: Option::<EntityId>::decode(d)?,
        instance: GraphInstanceId(d.u64()?),
    })
}

fn decode_q3(d: &mut Decoder<'_>, base: Option<[i32; 3]>) -> Result<[i32; 3], DecodeError> {
    let mut out = [0i32; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        let delta = get_z(d)?;
        let b = base.and_then(|b| b.get(i).copied()).map_or(0, i64::from);
        *slot = check_q(b + delta)?;
    }
    Ok(out)
}

/// The header fields after the baseline lag.
fn parse_header(d: &mut Decoder<'_>, server_tick: Tick) -> Result<SnapshotHeader, DecodeError> {
    let flags = d.u8()?;
    if flags & !0b111 != 0 {
        return Err(DecodeError::Invalid("snapshot flags"));
    }
    let ack = if flags & 1 != 0 {
        Some(InputSeq(d.u32()?))
    } else {
        None
    };
    let local = if flags & 2 != 0 {
        Some(LocalAvatar {
            id: EntityId::from_bits(d.u64()?),
            state: MotionState {
                position: Vec3::decode(d)?,
                velocity: Vec3::decode(d)?,
                yaw: Angle16(d.u16()?),
                grounded: d.bool()?,
            },
        })
    } else {
        None
    };
    let local_mods = if flags & 4 != 0 {
        MotionModifiers {
            speed_scale: d.finite_f32()?,
            jump_scale: d.finite_f32()?,
            gravity_scale: d.finite_f32()?,
        }
    } else {
        MotionModifiers::NONE
    };
    Ok(SnapshotHeader {
        server_tick,
        ack,
        local,
        local_mods,
    })
}

/// One remote sample, resolving deltas against `baseline`.
fn parse_remote(
    d: &mut Decoder<'_>,
    server_tick: Tick,
    baseline: Option<&SnapshotFrame>,
) -> Result<RemoteSample, DecodeError> {
    let id = get_id(d)?;
    let tick_lag = get_v(d)?;
    let tick = Tick(
        server_tick
            .0
            .checked_sub(tick_lag)
            .ok_or(DecodeError::Invalid("sample tick"))?,
    );
    let mask = d.u8()?;
    if mask & !(MASK_FULL | MASK_DELTA) != 0 {
        return Err(DecodeError::Invalid("remote mask"));
    }
    if mask & MASK_DELTA == 0 {
        if mask != MASK_FULL {
            return Err(DecodeError::Invalid("partial sample without delta"));
        }
        let position = dequant3(decode_q3(d, None)?, POS_SCALE);
        let velocity = dequant3(decode_q3(d, None)?, VEL_SCALE);
        return Ok(RemoteSample {
            id,
            tick,
            position,
            velocity,
            yaw: Angle16(d.u16()?),
        });
    }
    let base = baseline
        .and_then(|b| b.find_remote(id))
        .ok_or(DecodeError::Invalid("delta without baseline sample"))?;
    let bpos = quant3(base.position, POS_SCALE);
    let bvel = quant3(base.velocity, VEL_SCALE);
    let pos = if mask & MASK_POS != 0 {
        decode_q3(d, Some(bpos))?
    } else {
        bpos
    };
    let vel = if mask & MASK_VEL != 0 {
        decode_q3(d, Some(bvel))?
    } else {
        bvel
    };
    let yaw = if mask & MASK_YAW != 0 {
        Angle16(d.u16()?)
    } else {
        base.yaw
    };
    Ok(RemoteSample {
        id,
        tick,
        position: dequant3(pos, POS_SCALE),
        velocity: dequant3(vel, VEL_SCALE),
        yaw,
    })
}

/// One full parse; visits only when `visitor` is `Some`.
fn parse_snapshot(
    body: &[u8],
    baselines: &(impl BaselineStore + ?Sized),
    mut visitor: Option<&mut dyn SnapshotVisitor>,
) -> Result<(), DecodeError> {
    let mut d = Decoder::new(body);
    let server_tick = Tick(d.u64()?);
    let lag = get_v(&mut d)?;
    let baseline = if lag == 0 {
        None
    } else {
        let t = Tick(
            server_tick
                .0
                .checked_sub(lag)
                .ok_or(DecodeError::Invalid("baseline lag"))?,
        );
        Some(
            baselines
                .baseline(t)
                .ok_or(DecodeError::Invalid("missing baseline"))?,
        )
    };
    let header = parse_header(&mut d, server_tick)?;
    if let Some(v) = visitor.as_deref_mut() {
        v.header(&header);
    }
    for _ in 0..get_count(&mut d, MAX_ENTERED)? {
        let id = get_id(&mut d)?;
        let appearance = AppearanceId(d.u32()?);
        if let Some(v) = visitor.as_deref_mut() {
            v.entered(id, appearance);
        }
    }
    let remotes = get_count(&mut d, MAX_REMOTES)?;
    // Validation pass only: ids collected on the stack, checked for duplicates.
    let mut ids = [0u64; MAX_REMOTES];
    for i in 0..remotes {
        let sample = parse_remote(&mut d, server_tick, baseline)?;
        if let Some(slot) = ids.get_mut(i) {
            *slot = sample.id.to_bits();
        }
        if let Some(v) = visitor.as_deref_mut() {
            v.remote(&sample);
        }
    }
    if visitor.is_none() {
        let read = ids.get_mut(..remotes).unwrap_or(&mut []);
        read.sort_unstable();
        if read.windows(2).any(|w| matches!(w, [a, b] if a == b)) {
            return Err(DecodeError::Invalid("duplicate remote id"));
        }
    }
    for _ in 0..get_count(&mut d, MAX_REMOVED)? {
        let id = get_id(&mut d)?;
        if let Some(v) = visitor.as_deref_mut() {
            v.removed(id);
        }
    }
    for _ in 0..get_count(&mut d, MAX_MARKERS)? {
        let m = decode_marker(&mut d, server_tick)?;
        if let Some(v) = visitor.as_deref_mut() {
            v.marker(&m);
        }
    }
    d.finish()
}

/// Decodes a snapshot body (after the frame kind byte) into `visitor`. The
/// whole body is validated first; on error the visitor has seen nothing.
///
/// # Errors
/// [`DecodeError`] for any malformed body or a missing baseline.
pub fn decode_snapshot(
    body: &[u8],
    baselines: &(impl BaselineStore + ?Sized),
    visitor: &mut impl SnapshotVisitor,
) -> Result<(), DecodeError> {
    parse_snapshot(body, baselines, None)?;
    parse_snapshot(body, baselines, Some(visitor))
}

// ---- the native adapter -------------------------------------------------------

/// The engine's native adapter: Predictive movement over QUIC, the native
/// frame envelope, and delta snapshots. A package's native adapter is a
/// thin configuration of this type (plan 14).
#[derive(Clone, Copy, Debug)]
pub struct NativeAdapter {
    name: &'static str,
}

impl NativeAdapter {
    /// A native adapter reporting `name` in logs.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl WireAdapter for NativeAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn movement_mode(&self) -> MovementMode {
        MovementMode::Predictive
    }

    fn transport(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn decode(&self, frame: &[u8], out: &mut dyn FnMut(Inbound)) -> Result<(), AdapterError> {
        let (kind, rest) = frame.split_first().ok_or(DecodeError::UnexpectedEnd)?;
        if *kind != FRAME_MESSAGE {
            return Err(AdapterError::Protocol("clients send only message frames"));
        }
        let mut d = Decoder::new(rest);
        let id = MessageId(d.u16()?);
        let payload = d.take(d.remaining())?;
        out(parse_inbound(id, payload)?);
        Ok(())
    }

    fn encode_outbound(&self, msg: &Outbound, out: &mut Vec<u8>) -> Result<(), AdapterError> {
        encode_outbound_frame(msg, out);
        Ok(())
    }

    fn encode_snapshot(
        &self,
        frame: &SnapshotFrame,
        baseline: Option<&SnapshotFrame>,
        out: &mut Vec<u8>,
    ) -> Result<(), AdapterError> {
        out.push(FRAME_SNAPSHOT);
        encode_snapshot(frame, baseline, out);
        Ok(())
    }
}
