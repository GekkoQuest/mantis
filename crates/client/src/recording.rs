//! MCRC: client session recordings for the editor's replay viewer (dev builds).
//!
//! A recording captures, per simulation tick, everything the client simulation consumed
//! (the snapshot frames delivered that tick and the input taken from the accumulator),
//! what it produced (the move intent it sent), and a hash of its state afterwards
//! ([`crate::sim::ClientSim::state_hash`]). Replaying the inputs into a fresh simulation
//! built from the same motion model, ground, timestep, and intent map must reproduce every
//! intent and every hash; a difference is a divergence, as in the server replay
//! (`mantis_core::replay`).
//!
//! # Dev builds only, opt-in, size-capped
//!
//! The capture hook (`ClientSim::start_recording` and
//! `ClientSim::take_recording`) and the recorder field it fills exist only
//! when `debug_assertions` is on: a release client carries neither the field nor any
//! per-tick check. This module itself (the format, [`Recorder`], and the parser) is
//! always compiled, so a release editor can still open a recording made by a dev client.
//!
//! A [`Recorder`] reserves its whole byte cap up front and never grows: when the next tick
//! record would not fit, that record is dropped, recording stops, and the end record says
//! so ([`Recording::truncated`]). Recording a tick performs no heap operation.
//!
//! # Layout (version 1)
//!
//! Little-endian throughout. Floats are written as their raw IEEE bit patterns and every
//! pattern is accepted: a recording reproduces exactly what the simulation was given,
//! including values it rejects.
//!
//! Header, [`HEADER_LEN`] bytes:
//!
//! | offset | size | field |
//! |-------:|-----:|-------|
//! | 0  | 4  | magic `MCRC` |
//! | 4  | 2  | version `u16`, 1 |
//! | 6  | 2  | flags `u16`; none defined, must be 0 |
//! | 8  | 32 | build id ([`BuildId`], the same 32-byte identity the server log carries) |
//! | 40 | 32 | gameplay content hash ([`ContentHash`]) |
//! | 72 | 8  | start tick `u64`: the first record's tick, 0 when there is none |
//!
//! Then records, each starting with a tag byte. Tick records (tag 1), ticks strictly
//! increasing:
//!
//! | size | field |
//! |-----:|-------|
//! | 1 | tag `1` |
//! | 8 | tick `u64` |
//! | 8 | tick host time, [`HostInstant`] nanoseconds `u64` |
//! | 4 | frame count `u32`, then that many frames (below) |
//! | 48 | input: held, pressed, released action bits, `u128` each (bit `i` = action `i`) |
//! | 1 | axis count `u8` (at most `MAX_ACTIONS`), then that many axis values `f32` |
//! | 4 | look yaw `u16`, look pitch `u16` |
//! | 4 | frames merged `u32` |
//! | 1 | move sent: 0 none, 1 present (then the intent, 20 bytes) |
//! | 20 | intent: seq `u32`, tick `u64`, buttons `u16` (defined bits only), yaw `u16`, aim yaw `u16`, aim pitch `u16` |
//! | 8 | client state hash `u64` after the tick |
//!
//! Axis values past the count are `+0.0`; the recorder omits trailing all-zero-bit axes.
//!
//! Frame (every [`SnapshotFrame`] field):
//!
//! | size | field |
//! |-----:|-------|
//! | 8 | server tick `u64` |
//! | 8 | received at, host nanoseconds `u64` |
//! | 4 | session epoch `u32` (bumped at every hand-off and reconnect) |
//! | 4 | connection `u32` (bumped at every reconnect) |
//! | 1 (+4) | resume from: 0 none, 1 present then the first input seq sent on the connection `u32` |
//! | 1 (+4) | ack: 0 none, 1 present then input seq `u32` |
//! | 1 (+8+n) | local: 0 none, 1 present then entity and the state ([`RecordState`], 27 bytes for [`MotionState`]) |
//! | 12 | local modifiers: speed, jump, gravity scale `f32` |
//! | 4 + 42 each | remotes: entity, tick `u64`, position, velocity (`f32` x3 each), yaw `u16` |
//! | 4 + 8 each | removed entities |
//! | 4 + markers | timeline markers (below) |
//! | 4 + 12 each | entered: entity, appearance `u32` |
//!
//! An entity is index `u32` then generation `u32`. A marker is graph `u32`, node `u16`,
//! kind `u8` (0 cast start, 1 impact then the target entity, 2 periodic tick then `n`
//! `u16`, 3 expire, 4 package-defined then its `u16`), at tick `u64`, offset `u32`, source
//! entity, target (0 none, 1 present then entity), instance `u64`.
//!
//! The end record (tag 2), exactly once, last, [`END_LEN`] bytes: end flags `u8` (bit 0:
//! truncated; other bits must be 0) and the tick record count `u64`.
//!
//! The parser rejects the whole recording on: wrong magic, another version, any flag bit,
//! an unknown tag, a presence or boolean byte other than 0 or 1, undefined button bits,
//! an unknown marker kind, an axis count above `MAX_ACTIONS`, a count that cannot fit in
//! the bytes left, ticks not strictly increasing, a start tick other than the first
//! record's, an end count that disagrees, a missing end record, or bytes after it.
//!
//! **Versions.** Version 2 added the session epoch, the connection, and the first input of
//! the connection to each frame; version 1 is not read. That is acceptable for this
//! format only: a recording is a development artifact already bound to the build that
//! made it (its header holds the build id), so a version 1 file comes from a build that
//! no longer exists and could not be replayed anyway. It is no precedent against the
//! rule that shipped asset formats keep reading their older versions.

use std::marker::PhantomData;
use std::sync::OnceLock;

use mantis_core::content::ContentHash;
use mantis_core::graph::{
    GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, PackageMarker, TimelineMarker,
};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::BuildId;
use mantis_formats::bytes::{FormatError, Reader};

use crate::camera::LookSample;
use crate::core_api::{
    AimAngles, Angle16, EntityId, InputSeq, MotionModifiers, MotionState, MoveButtons, MoveInput, Tick, Vec3,
};
use crate::input::accumulator::TickInput;
use crate::input::action::{ActionBits, ActionId, ActionKind, ActionTable, MAX_ACTIONS};
use crate::snapshot::{RemoteState, SnapshotFrame};
use crate::time::HostInstant;

/// File magic.
pub const MAGIC: [u8; 4] = *b"MCRC";
/// The version this module writes and reads.
pub const VERSION: u16 = 2;
/// Header size in bytes.
pub const HEADER_LEN: usize = 80;
/// End record size in bytes, tag included.
pub const END_LEN: usize = 10;
/// The smallest byte cap a [`Recorder`] accepts (header plus end record); smaller caps are
/// raised to it.
pub const MIN_CAP: usize = HEADER_LEN + END_LEN;
/// End flag: recording stopped early (the cap was reached, or a tick arrived out of order).
pub const END_TRUNCATED: u8 = 1;

const TAG_TICK: u8 = 1;
const TAG_END: u8 = 2;
const START_TICK_AT: usize = 72;

const ENTITY_LEN: usize = 8;
const REMOTE_LEN: usize = ENTITY_LEN + 8 + 12 + 12 + 2;
const ENTERED_LEN: usize = ENTITY_LEN + 4;
/// Smallest encoded marker (cast start, no target).
const MARKER_MIN_LEN: usize = 4 + 2 + 1 + 8 + 4 + ENTITY_LEN + 1 + 8;
/// Smallest encoded frame (no ack, no local, no lists).
const FRAME_MIN_LEN: usize = 8 + 8 + 4 + 4 + 1 + 1 + 1 + 12 + 4 * 4;

// ---------------------------------------------------------------------------------------
// State encoding
// ---------------------------------------------------------------------------------------

/// A simulation state type a recording can carry: its byte encoding and its state hash.
///
/// The encoding must be bit-exact (floats by bit pattern) so a replay sees exactly the
/// recorded state, and `decode` must reject any malformed field.
pub trait RecordState: Copy + Send + 'static {
    /// Writes the state.
    fn encode(&self, w: &mut RecordWriter<'_>);

    /// Reads a state written by [`RecordState::encode`].
    ///
    /// # Errors
    /// The first malformed field.
    fn decode(r: &mut Reader<'_>) -> Result<Self, FormatError>;

    /// Feeds the state to a stable hasher (IEEE bit patterns, as `StateHash` does).
    fn hash_state(&self, h: &mut StableHasher);
}

/// Position, velocity (`f32` bit patterns), yaw `u16`, grounded `u8` (0 or 1): 27 bytes.
impl RecordState for MotionState {
    fn encode(&self, w: &mut RecordWriter<'_>) {
        w.vec3(self.position);
        w.vec3(self.velocity);
        w.u16(self.yaw.0);
        w.u8(u8::from(self.grounded));
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, FormatError> {
        Ok(MotionState {
            position: read_vec3(r)?,
            velocity: read_vec3(r)?,
            yaw: Angle16(r.u16()?),
            grounded: read_flag(r)?,
        })
    }

    fn hash_state(&self, h: &mut StableHasher) {
        self.state_hash(h);
    }
}

/// A bounded little-endian writer into a recording buffer.
///
/// Writes past the limit are refused and remembered ([`RecordWriter::overflowed`]); the
/// buffer never grows past the limit, so with capacity reserved up to it, writing never
/// allocates.
#[derive(Debug)]
pub struct RecordWriter<'a> {
    buf: &'a mut Vec<u8>,
    limit: usize,
    overflow: bool,
}

impl<'a> RecordWriter<'a> {
    /// A writer appending to `buf` while its length stays at or below `limit`.
    pub fn new(buf: &'a mut Vec<u8>, limit: usize) -> Self {
        Self {
            buf,
            limit,
            overflow: false,
        }
    }

    /// True once any write was refused.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        if self.overflow || self.buf.len().saturating_add(b.len()) > self.limit {
            self.overflow = true;
            return;
        }
        self.buf.extend_from_slice(b);
    }

    /// A `u8`.
    pub fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    /// A `u16`.
    pub fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    /// A `u32`.
    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    /// A `u64`.
    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    /// A `u128`.
    pub fn u128(&mut self, v: u128) {
        self.bytes(&v.to_le_bytes());
    }

    /// An `f32` by bit pattern.
    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    /// A vector, three `f32` bit patterns.
    pub fn vec3(&mut self, v: Vec3) {
        self.f32(v.x);
        self.f32(v.y);
        self.f32(v.z);
    }

    /// A list count; a count past `u32` is refused like an overflow.
    pub fn count(&mut self, n: usize) {
        match u32::try_from(n) {
            Ok(n) => self.u32(n),
            Err(_) => self.overflow = true,
        }
    }

    fn entity(&mut self, id: EntityId) {
        self.u32(id.index());
        self.u32(id.generation());
    }

    fn presence(&mut self, present: bool) {
        self.u8(u8::from(present));
    }
}

// ---------------------------------------------------------------------------------------
// Action bits
// ---------------------------------------------------------------------------------------

/// Every dense action id, in index order. `ActionBits` exposes no raw accessor, so bit `i`
/// of the recorded `u128` is the membership of the `i`-th id. Built once (the only
/// allocation, outside any tick: [`Recorder::new`] and the parser initialise it).
fn action_ids() -> &'static [ActionId] {
    static IDS: OnceLock<Vec<ActionId>> = OnceLock::new();
    IDS.get_or_init(|| {
        let mut table = ActionTable::new();
        (0..MAX_ACTIONS)
            .filter_map(|i| table.define(&format!("a{i}"), ActionKind::Button).ok())
            .collect()
    })
}

fn bits_to_u128(bits: ActionBits) -> u128 {
    action_ids().iter().enumerate().fold(0u128, |acc, (i, id)| {
        if bits.contains(*id) {
            acc | (1u128 << i)
        } else {
            acc
        }
    })
}

fn u128_to_bits(raw: u128) -> ActionBits {
    let mut bits = ActionBits::EMPTY;
    for (i, id) in action_ids().iter().enumerate() {
        if raw & (1u128 << i) != 0 {
            bits.insert(*id);
        }
    }
    bits
}

// ---------------------------------------------------------------------------------------
// Shared encoders
// ---------------------------------------------------------------------------------------

/// Borrowed view of one frame, from a live [`SnapshotFrame`] or a parsed [`RecordedFrame`].
struct FrameView<'f, S> {
    server_tick: Tick,
    received_at: HostInstant,
    epoch: u32,
    connection: u32,
    resume_from: Option<InputSeq>,
    ack: Option<InputSeq>,
    local: Option<&'f (EntityId, S)>,
    local_mods: MotionModifiers,
    remotes: &'f [RemoteState],
    removed: &'f [EntityId],
    markers: &'f [TimelineMarker],
    entered: &'f [(EntityId, u32)],
}

impl<'f, S> FrameView<'f, S> {
    fn of_snapshot(f: &'f SnapshotFrame<S>) -> Self {
        Self {
            server_tick: f.server_tick,
            received_at: f.received_at,
            epoch: f.epoch,
            connection: f.connection,
            resume_from: f.resume_from,
            ack: f.ack,
            local: f.local.as_ref(),
            local_mods: f.local_mods,
            remotes: &f.remotes,
            removed: &f.removed,
            markers: &f.markers,
            entered: &f.entered,
        }
    }

    fn of_recorded(f: &'f RecordedFrame<S>) -> Self {
        Self {
            server_tick: f.server_tick,
            received_at: f.received_at,
            epoch: f.epoch,
            connection: f.connection,
            resume_from: f.resume_from,
            ack: f.ack,
            local: f.local.as_ref(),
            local_mods: f.local_mods,
            remotes: &f.remotes,
            removed: &f.removed,
            markers: &f.markers,
            entered: &f.entered,
        }
    }
}

fn put_frame<S: RecordState>(w: &mut RecordWriter<'_>, f: &FrameView<'_, S>) {
    w.u64(f.server_tick.0);
    w.u64(f.received_at.as_nanos());
    w.u32(f.epoch);
    w.u32(f.connection);
    w.presence(f.resume_from.is_some());
    if let Some(seq) = f.resume_from {
        w.u32(seq.0);
    }
    w.presence(f.ack.is_some());
    if let Some(ack) = f.ack {
        w.u32(ack.0);
    }
    w.presence(f.local.is_some());
    if let Some((id, state)) = f.local {
        w.entity(*id);
        state.encode(w);
    }
    w.f32(f.local_mods.speed_scale);
    w.f32(f.local_mods.jump_scale);
    w.f32(f.local_mods.gravity_scale);
    w.count(f.remotes.len());
    for r in f.remotes {
        w.entity(r.id);
        w.u64(r.tick.0);
        w.vec3(r.position);
        w.vec3(r.velocity);
        w.u16(r.yaw.0);
    }
    w.count(f.removed.len());
    for id in f.removed {
        w.entity(*id);
    }
    w.count(f.markers.len());
    for m in f.markers {
        put_marker(w, m);
    }
    w.count(f.entered.len());
    for (id, appearance) in f.entered {
        w.entity(*id);
        w.u32(*appearance);
    }
}

fn put_marker(w: &mut RecordWriter<'_>, m: &TimelineMarker) {
    w.u32(m.id.graph.0);
    w.u16(m.id.node.0);
    match m.kind {
        MarkerKind::CastStart => w.u8(0),
        MarkerKind::Impact { target } => {
            w.u8(1);
            w.entity(target);
        }
        MarkerKind::TickN(n) => {
            w.u8(2);
            w.u16(n);
        }
        MarkerKind::Expire => w.u8(3),
        MarkerKind::Package(p) => {
            w.u8(4);
            w.u16(p.0);
        }
    }
    w.u64(m.at.0);
    w.u32(m.offset);
    w.entity(m.source);
    w.presence(m.target.is_some());
    if let Some(t) = m.target {
        w.entity(t);
    }
    w.u64(m.instance.0);
}

fn put_input(w: &mut RecordWriter<'_>, input: &TickInput) {
    w.u128(bits_to_u128(input.held));
    w.u128(bits_to_u128(input.pressed));
    w.u128(bits_to_u128(input.released));
    let axes = input
        .axes
        .iter()
        .rposition(|a| a.to_bits() != 0)
        .map_or(0, |i| i + 1);
    let n = u8::try_from(axes).unwrap_or(u8::MAX);
    w.u8(n);
    for a in input.axes.iter().take(usize::from(n)) {
        w.f32(*a);
    }
    w.u16(input.look.yaw.0);
    w.u16(input.look.pitch.0);
    w.u32(input.frames);
}

fn put_sent(w: &mut RecordWriter<'_>, sent: Option<&MoveInput>) {
    w.presence(sent.is_some());
    if let Some(m) = sent {
        w.u32(m.seq.0);
        w.u64(m.tick.0);
        w.u16(m.buttons.bits());
        w.u16(m.yaw.0);
        w.u16(m.aim.yaw.0);
        w.u16(m.aim.pitch.0);
    }
}

fn put_header(w: &mut RecordWriter<'_>, header: &RecordingHeader) {
    w.bytes(&MAGIC);
    w.u16(VERSION);
    w.u16(0);
    w.bytes(&header.build.0);
    w.bytes(header.content.as_bytes());
    w.u64(header.start_tick.0);
}

fn put_end(w: &mut RecordWriter<'_>, truncated: bool, records: u64) {
    w.u8(TAG_END);
    w.u8(if truncated { END_TRUNCATED } else { 0 });
    w.u64(records);
}

// ---------------------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------------------

/// Writes an MCRC recording into a buffer reserved once at its byte cap.
///
/// Per tick: [`Recorder::begin_tick`], one [`Recorder::frame`] per delivered snapshot
/// frame, then [`Recorder::end_tick`]. [`Recorder::finish`] appends the end record and
/// returns the bytes. Nothing here panics or allocates after construction.
#[derive(Debug)]
pub struct Recorder<S> {
    buf: Vec<u8>,
    limit: usize,
    records: u64,
    last_tick: Option<Tick>,
    open: Option<OpenRecord>,
    truncated: bool,
    state: PhantomData<fn(&S)>,
}

#[derive(Clone, Copy, Debug)]
struct OpenRecord {
    tick: Tick,
    start: usize,
    count_at: usize,
    frames: u32,
    overflow: bool,
}

impl<S: RecordState> Recorder<S> {
    /// A recorder for a session of `build` against `content`, never holding more than
    /// `cap` bytes (raised to [`MIN_CAP`]). Reserves `cap` bytes now.
    pub fn new(build: BuildId, content: ContentHash, cap: usize) -> Self {
        let cap = cap.max(MIN_CAP);
        let _ = action_ids();
        let mut buf = Vec::with_capacity(cap);
        let mut w = RecordWriter::new(&mut buf, cap);
        put_header(
            &mut w,
            &RecordingHeader {
                build,
                content,
                start_tick: Tick::ZERO,
            },
        );
        Self {
            buf,
            limit: cap - END_LEN,
            records: 0,
            last_tick: None,
            open: None,
            truncated: false,
            state: PhantomData,
        }
    }

    /// True once recording stopped early; later ticks are ignored.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Tick records committed.
    pub fn ticks_recorded(&self) -> u64 {
        self.records
    }

    /// Bytes held (without the end record [`Recorder::finish`] adds).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when only the header is held.
    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    fn write(&mut self, f: impl FnOnce(&mut RecordWriter<'_>)) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.overflow {
            return;
        }
        let mut w = RecordWriter::new(&mut self.buf, self.limit);
        f(&mut w);
        open.overflow = w.overflowed();
    }

    /// Opens the record for `tick`, run at host instant `at`. A tick not after the last
    /// recorded one stops the recording (marked truncated).
    pub fn begin_tick(&mut self, tick: Tick, at: HostInstant) {
        if let Some(stale) = self.open.take() {
            self.buf.truncate(stale.start);
        }
        if self.truncated {
            return;
        }
        if self.last_tick.is_some_and(|last| tick <= last) {
            self.truncated = true;
            return;
        }
        let start = self.buf.len();
        self.open = Some(OpenRecord {
            tick,
            start,
            count_at: start + 1 + 8 + 8,
            frames: 0,
            overflow: false,
        });
        self.write(|w| {
            w.u8(TAG_TICK);
            w.u64(tick.0);
            w.u64(at.as_nanos());
            w.u32(0);
        });
    }

    /// Records one snapshot frame delivered this tick.
    pub fn frame(&mut self, frame: &SnapshotFrame<S>) {
        self.write(|w| put_frame(w, &FrameView::of_snapshot(frame)));
        if let Some(open) = self.open.as_mut() {
            match open.frames.checked_add(1) {
                Some(n) => open.frames = n,
                None => open.overflow = true,
            }
        }
    }

    /// Closes the tick record with the input taken, the intent sent, and the state hash
    /// after the tick. When the record does not fit the cap it is dropped and recording
    /// stops.
    pub fn end_tick(&mut self, input: &TickInput, sent: Option<&MoveInput>, state_hash: u64) {
        self.write(|w| {
            put_input(w, input);
            put_sent(w, sent);
            w.u64(state_hash);
        });
        let Some(open) = self.open.take() else {
            return;
        };
        if open.overflow {
            self.buf.truncate(open.start);
            self.truncated = true;
            return;
        }
        patch(&mut self.buf, open.count_at, &open.frames.to_le_bytes());
        if self.records == 0 {
            patch(&mut self.buf, START_TICK_AT, &open.tick.0.to_le_bytes());
        }
        self.records = self.records.saturating_add(1);
        self.last_tick = Some(open.tick);
    }

    /// Appends the end record and returns the recording. An unclosed tick is dropped.
    pub fn finish(mut self) -> Vec<u8> {
        if let Some(stale) = self.open.take() {
            self.buf.truncate(stale.start);
        }
        // `limit` leaves exactly END_LEN bytes of the reserved capacity for this.
        let cap = self.limit + END_LEN;
        let mut w = RecordWriter::new(&mut self.buf, cap);
        put_end(&mut w, self.truncated, self.records);
        self.buf
    }
}

fn patch(buf: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(dst) = buf.get_mut(at..at + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

/// The recorder as the client simulation holds it: the state bound erased, so the tick
/// loop needs no `RecordState` bound.
#[cfg(debug_assertions)]
pub(crate) trait SimRecorder<S>: Send {
    fn begin_tick(&mut self, tick: Tick, at: HostInstant);
    fn frame(&mut self, frame: &SnapshotFrame<S>);
    fn hash_state(&self, state: &S, h: &mut StableHasher);
    fn end_tick(&mut self, input: &TickInput, sent: Option<&MoveInput>, state_hash: u64);
    fn finish(self: Box<Self>) -> Vec<u8>;
}

#[cfg(debug_assertions)]
impl<S: RecordState> SimRecorder<S> for Recorder<S> {
    fn begin_tick(&mut self, tick: Tick, at: HostInstant) {
        Recorder::begin_tick(self, tick, at);
    }
    fn frame(&mut self, frame: &SnapshotFrame<S>) {
        Recorder::frame(self, frame);
    }
    fn hash_state(&self, state: &S, h: &mut StableHasher) {
        state.hash_state(h);
    }
    fn end_tick(&mut self, input: &TickInput, sent: Option<&MoveInput>, state_hash: u64) {
        Recorder::end_tick(self, input, sent, state_hash);
    }
    fn finish(self: Box<Self>) -> Vec<u8> {
        Recorder::finish(*self)
    }
}

// ---------------------------------------------------------------------------------------
// Parsed form
// ---------------------------------------------------------------------------------------

/// The recording header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordingHeader {
    /// The build that recorded.
    pub build: BuildId,
    /// The gameplay content it ran against.
    pub content: ContentHash,
    /// The first record's tick (zero when there is none).
    pub start_tick: Tick,
}

/// One recorded snapshot frame: every [`SnapshotFrame`] field.
#[derive(Clone, PartialEq, Debug)]
pub struct RecordedFrame<S> {
    /// Server tick.
    pub server_tick: Tick,
    /// Host arrival instant.
    pub received_at: HostInstant,
    /// Session epoch.
    pub epoch: u32,
    /// Connection.
    pub connection: u32,
    /// The first input sent on the connection.
    pub resume_from: Option<InputSeq>,
    /// Last applied input.
    pub ack: Option<InputSeq>,
    /// The avatar and its state.
    pub local: Option<(EntityId, S)>,
    /// The avatar's modifiers.
    pub local_mods: MotionModifiers,
    /// Remote states.
    pub remotes: Vec<RemoteState>,
    /// Removed entities.
    pub removed: Vec<EntityId>,
    /// Timeline markers.
    pub markers: Vec<TimelineMarker>,
    /// Entered entities with appearance ids.
    pub entered: Vec<(EntityId, u32)>,
}

impl<S: Copy> RecordedFrame<S> {
    /// Copies this frame into a channel frame (as acquired, cleared). False when the
    /// channel frame's capacity refused part of it.
    pub fn fill(&self, frame: &mut SnapshotFrame<S>) -> bool {
        frame.server_tick = self.server_tick;
        frame.received_at = self.received_at;
        frame.epoch = self.epoch;
        frame.connection = self.connection;
        frame.resume_from = self.resume_from;
        frame.ack = self.ack;
        frame.local = self.local;
        frame.local_mods = self.local_mods;
        let mut ok = true;
        for r in &self.remotes {
            ok &= frame.push_remote(*r);
        }
        for id in &self.removed {
            ok &= frame.push_removed(*id);
        }
        for m in &self.markers {
            ok &= frame.push_marker(*m);
        }
        for (id, appearance) in &self.entered {
            ok &= frame.push_entered(*id, *appearance);
        }
        ok
    }

    /// The largest list in the frame (the channel capacity replaying it needs).
    pub fn widest_list(&self) -> usize {
        self.remotes
            .len()
            .max(self.removed.len())
            .max(self.markers.len())
            .max(self.entered.len())
    }
}

/// One recorded simulation tick.
#[derive(Clone, PartialEq, Debug)]
pub struct TickRecord<S> {
    /// The tick.
    pub tick: Tick,
    /// The host instant it ran at.
    pub time: HostInstant,
    /// Snapshot frames delivered before it, in delivery order.
    pub frames: Vec<RecordedFrame<S>>,
    /// The input taken from the accumulator.
    pub input: TickInput,
    /// The move intent sent, if any.
    pub sent: Option<MoveInput>,
    /// [`crate::sim::ClientSim::state_hash`] after the tick.
    pub state_hash: u64,
}

/// A parsed, fully validated recording.
#[derive(Clone, PartialEq, Debug)]
pub struct Recording<S> {
    /// The header.
    pub header: RecordingHeader,
    /// Every tick record, ticks strictly increasing.
    pub ticks: Vec<TickRecord<S>>,
    /// True when recording stopped early (cap reached).
    pub truncated: bool,
}

impl<S: RecordState> Recording<S> {
    /// Parses and validates a whole recording.
    ///
    /// # Errors
    /// The first malformed field (see the module docs for the rules).
    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = RecordingReader::new(bytes)?;
        let mut ticks = Vec::new();
        while let Some(t) = reader.next_record()? {
            ticks.push(t);
        }
        Ok(Self {
            header: reader.header,
            ticks,
            truncated: reader.truncated().unwrap_or(false),
        })
    }

    /// Encodes the recording (the inverse of [`Recording::parse`] for a valid one). Does
    /// not validate: a test can encode a deliberately malformed recording.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut w = RecordWriter::new(&mut buf, usize::MAX);
        put_header(&mut w, &self.header);
        for t in &self.ticks {
            w.u8(TAG_TICK);
            w.u64(t.tick.0);
            w.u64(t.time.as_nanos());
            w.count(t.frames.len());
            for f in &t.frames {
                put_frame(&mut w, &FrameView::of_recorded(f));
            }
            put_input(&mut w, &t.input);
            put_sent(&mut w, t.sent.as_ref());
            w.u64(t.state_hash);
        }
        put_end(&mut w, self.truncated, self.ticks.len() as u64);
        buf
    }
}

/// Streams the tick records of a recording, validating as it goes. The recording is
/// valid only once [`RecordingReader::next_record`] has returned `Ok(None)` (end record
/// read, nothing after it).
#[derive(Debug)]
pub struct RecordingReader<'a, S> {
    r: Reader<'a>,
    header: RecordingHeader,
    last_tick: Option<Tick>,
    records: u64,
    end: Option<bool>,
    failed: bool,
    state: PhantomData<fn() -> S>,
}

impl<'a, S: RecordState> RecordingReader<'a, S> {
    /// Reads and validates the header.
    ///
    /// # Errors
    /// [`FormatError::Magic`], [`FormatError::Version`], [`FormatError::Flags`], or
    /// [`FormatError::Length`].
    pub fn new(bytes: &'a [u8]) -> Result<Self, FormatError> {
        let _ = action_ids();
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let build = BuildId(r.array()?);
        let content = ContentHash::from_bytes(r.array()?);
        let start_tick = Tick(read_u64(&mut r)?);
        Ok(Self {
            r,
            header: RecordingHeader {
                build,
                content,
                start_tick,
            },
            last_tick: None,
            records: 0,
            end: None,
            failed: false,
            state: PhantomData,
        })
    }

    /// The header.
    pub fn header(&self) -> &RecordingHeader {
        &self.header
    }

    /// The end record's truncated flag, once it has been read.
    pub fn truncated(&self) -> Option<bool> {
        self.end
    }

    /// The next tick record, or `None` after a valid end record.
    ///
    /// # Errors
    /// The first malformed field; the reader then yields nothing more.
    pub fn next_record(&mut self) -> Result<Option<TickRecord<S>>, FormatError> {
        if self.end.is_some() {
            return Ok(None);
        }
        if self.failed {
            return Err(FormatError::Inconsistent);
        }
        let out = self.read_next();
        if out.is_err() {
            self.failed = true;
        }
        out
    }

    fn read_next(&mut self) -> Result<Option<TickRecord<S>>, FormatError> {
        let r = &mut self.r;
        match r.u8()? {
            TAG_TICK => {
                let record = read_tick::<S>(r)?;
                let in_order = match self.last_tick {
                    None => record.tick == self.header.start_tick,
                    Some(last) => record.tick > last,
                };
                if !in_order {
                    return Err(FormatError::Inconsistent);
                }
                self.last_tick = Some(record.tick);
                self.records += 1;
                Ok(Some(record))
            }
            TAG_END => {
                let flags = r.u8()?;
                if flags & !END_TRUNCATED != 0 {
                    return Err(FormatError::Flags(u32::from(flags)));
                }
                let count = read_u64(r)?;
                r.finish()?;
                if count != self.records || (self.records == 0 && self.header.start_tick != Tick::ZERO) {
                    return Err(FormatError::Inconsistent);
                }
                self.end = Some(flags & END_TRUNCATED != 0);
                Ok(None)
            }
            tag => Err(FormatError::Encoding(u32::from(tag))),
        }
    }
}

impl<S: RecordState> Iterator for RecordingReader<'_, S> {
    type Item = Result<TickRecord<S>, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        self.next_record().transpose()
    }
}

// ---------------------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------------------

fn read_u64(r: &mut Reader<'_>) -> Result<u64, FormatError> {
    Ok(u64::from_le_bytes(r.array()?))
}

fn read_u128(r: &mut Reader<'_>) -> Result<u128, FormatError> {
    Ok(u128::from_le_bytes(r.array()?))
}

fn read_vec3(r: &mut Reader<'_>) -> Result<Vec3, FormatError> {
    Ok(Vec3::new(r.f32_raw()?, r.f32_raw()?, r.f32_raw()?))
}

fn read_flag(r: &mut Reader<'_>) -> Result<bool, FormatError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FormatError::Validity),
    }
}

fn read_entity(r: &mut Reader<'_>) -> Result<EntityId, FormatError> {
    Ok(EntityId::new(r.u32()?, r.u32()?))
}

/// A list count whose items (at least `min_len` bytes each) can fit in what is left, so a
/// nonsense count never drives an allocation.
fn read_count(r: &mut Reader<'_>, min_len: usize) -> Result<usize, FormatError> {
    let n = usize::try_from(r.u32()?).map_err(|_| FormatError::Dimensions)?;
    if n.saturating_mul(min_len) > r.remaining() {
        return Err(FormatError::Dimensions);
    }
    Ok(n)
}

fn read_tick<S: RecordState>(r: &mut Reader<'_>) -> Result<TickRecord<S>, FormatError> {
    let tick = Tick(read_u64(r)?);
    let time = HostInstant::from_nanos(read_u64(r)?);
    let n = read_count(r, FRAME_MIN_LEN)?;
    let mut frames = Vec::with_capacity(n);
    for _ in 0..n {
        frames.push(read_frame(r)?);
    }
    let input = read_input(r)?;
    let sent = if read_flag(r)? { Some(read_move(r)?) } else { None };
    let state_hash = read_u64(r)?;
    Ok(TickRecord {
        tick,
        time,
        frames,
        input,
        sent,
        state_hash,
    })
}

fn read_frame<S: RecordState>(r: &mut Reader<'_>) -> Result<RecordedFrame<S>, FormatError> {
    let server_tick = Tick(read_u64(r)?);
    let received_at = HostInstant::from_nanos(read_u64(r)?);
    let epoch = r.u32()?;
    let connection = r.u32()?;
    let resume_from = if read_flag(r)? {
        Some(InputSeq(r.u32()?))
    } else {
        None
    };
    let ack = if read_flag(r)? {
        Some(InputSeq(r.u32()?))
    } else {
        None
    };
    let local = if read_flag(r)? {
        Some((read_entity(r)?, S::decode(r)?))
    } else {
        None
    };
    let local_mods = MotionModifiers {
        speed_scale: r.f32_raw()?,
        jump_scale: r.f32_raw()?,
        gravity_scale: r.f32_raw()?,
    };
    let n = read_count(r, REMOTE_LEN)?;
    let mut remotes = Vec::with_capacity(n);
    for _ in 0..n {
        remotes.push(RemoteState {
            id: read_entity(r)?,
            tick: Tick(read_u64(r)?),
            position: read_vec3(r)?,
            velocity: read_vec3(r)?,
            yaw: Angle16(r.u16()?),
        });
    }
    let n = read_count(r, ENTITY_LEN)?;
    let mut removed = Vec::with_capacity(n);
    for _ in 0..n {
        removed.push(read_entity(r)?);
    }
    let n = read_count(r, MARKER_MIN_LEN)?;
    let mut markers = Vec::with_capacity(n);
    for _ in 0..n {
        markers.push(read_marker(r)?);
    }
    let n = read_count(r, ENTERED_LEN)?;
    let mut entered = Vec::with_capacity(n);
    for _ in 0..n {
        entered.push((read_entity(r)?, r.u32()?));
    }
    Ok(RecordedFrame {
        server_tick,
        received_at,
        epoch,
        connection,
        resume_from,
        ack,
        local,
        local_mods,
        remotes,
        removed,
        markers,
        entered,
    })
}

fn read_marker(r: &mut Reader<'_>) -> Result<TimelineMarker, FormatError> {
    let id = MarkerId {
        graph: GraphId(r.u32()?),
        node: NodeKey(r.u16()?),
    };
    let kind = match r.u8()? {
        0 => MarkerKind::CastStart,
        1 => MarkerKind::Impact {
            target: read_entity(r)?,
        },
        2 => MarkerKind::TickN(r.u16()?),
        3 => MarkerKind::Expire,
        4 => MarkerKind::Package(PackageMarker(r.u16()?)),
        k => return Err(FormatError::Encoding(u32::from(k))),
    };
    let at = Tick(read_u64(r)?);
    let offset = r.u32()?;
    let source = read_entity(r)?;
    let target = if read_flag(r)? {
        Some(read_entity(r)?)
    } else {
        None
    };
    let instance = GraphInstanceId(read_u64(r)?);
    Ok(TimelineMarker {
        id,
        kind,
        at,
        offset,
        source,
        target,
        instance,
    })
}

fn read_input(r: &mut Reader<'_>) -> Result<TickInput, FormatError> {
    let held = u128_to_bits(read_u128(r)?);
    let pressed = u128_to_bits(read_u128(r)?);
    let released = u128_to_bits(read_u128(r)?);
    let n = usize::from(r.u8()?);
    if n > MAX_ACTIONS {
        return Err(FormatError::Dimensions);
    }
    let mut axes = [0.0f32; MAX_ACTIONS];
    for a in axes.iter_mut().take(n) {
        *a = r.f32_raw()?;
    }
    let look = LookSample {
        yaw: Angle16(r.u16()?),
        pitch: Angle16(r.u16()?),
    };
    let frames = r.u32()?;
    Ok(TickInput {
        held,
        pressed,
        released,
        axes,
        look,
        frames,
    })
}

fn read_move(r: &mut Reader<'_>) -> Result<MoveInput, FormatError> {
    let seq = InputSeq(r.u32()?);
    let tick = Tick(read_u64(r)?);
    let raw = r.u16()?;
    let buttons = MoveButtons::from_bits(raw).ok_or(FormatError::Flags(u32::from(raw)))?;
    Ok(MoveInput {
        seq,
        tick,
        buttons,
        yaw: Angle16(r.u16()?),
        aim: AimAngles {
            yaw: Angle16(r.u16()?),
            pitch: Angle16(r.u16()?),
        },
    })
}
