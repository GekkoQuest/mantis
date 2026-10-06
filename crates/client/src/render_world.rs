//! The render world: everything the render thread is allowed to know about the game,
//! published by the simulation thread once per tick (decision 0009).
//!
//! - **Remote entities** arrive as an interpolation window of up to [`REMOTE_WINDOW`]
//!   timestamped samples. Each frame the render thread picks the pair bracketing
//!   `render_time - interpolation_delay` and blends it. The window holds three samples,
//!   not two, so the bracketing pair exists for every frame between two publishes even
//!   when server tick phase and client tick phase differ. Past the newest sample the
//!   entity is extrapolated along its velocity for a bounded time, then held.
//! - **The local avatar** arrives as the latest predicted state plus velocity and is
//!   extrapolated to render time, so prediction's latency win is not spent waiting on a
//!   tick boundary. Reconciliation corrections are smoothed here, visually, by a decaying
//!   offset; the simulation itself always holds the corrected state.
//!
//! The render thread has no other access to game state. All math here is IEEE
//! arithmetic only (no transcendentals), keeping this crate inside the determinism lint.
//!
//! [`render_world_channel`] moves worlds from the sim thread to the render thread through
//! three preallocated buffers: the sim writes one, the render thread reads one, and the
//! third sits in a shared slot. Publishing and acquiring swap boxes under a short lock and
//! never allocate after construction.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::core_api::{Angle16, EntityId, Tick, Vec3, angle_delta, angle_to_turns};
use crate::time::HostInstant;

/// Samples carried per remote entity.
pub const REMOTE_WINDOW: usize = 3;

/// Presentation timing parameters, shared by sim and render.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PresentationConfig {
    /// How far behind render time remote entities are shown.
    pub interpolation_delay: Duration,
    /// How long a remote entity may be extrapolated past its newest sample before it holds.
    pub max_remote_extrapolation: Duration,
    /// How far past its state time the local avatar may be extrapolated.
    pub max_local_extrapolation: Duration,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            interpolation_delay: Duration::from_millis(100),
            max_remote_extrapolation: Duration::from_millis(250),
            max_local_extrapolation: Duration::from_millis(50),
        }
    }
}

/// One timestamped remote entity sample, already mapped to host time.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct RemoteSample {
    /// Host instant the sample describes.
    pub time: HostInstant,
    /// Position.
    pub position: Vec3,
    /// Velocity, units per second.
    pub velocity: Vec3,
    /// Facing.
    pub yaw: Angle16,
}

/// A remote entity's interpolation window: one to three samples, strictly increasing in time.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RemoteEntity {
    id: EntityId,
    samples: [RemoteSample; REMOTE_WINDOW],
    len: u8,
}

impl RemoteEntity {
    /// Builds a window from `samples`, oldest first. Returns `None` if `samples` is empty,
    /// longer than [`REMOTE_WINDOW`], not strictly increasing in time, or not finite.
    pub fn new(id: EntityId, samples: &[RemoteSample]) -> Option<Self> {
        if samples.is_empty() || samples.len() > REMOTE_WINDOW {
            return None;
        }
        if samples
            .windows(2)
            .any(|w| matches!(w, [a, b] if a.time >= b.time))
        {
            return None;
        }
        if samples
            .iter()
            .any(|s| !s.position.is_finite() || !s.velocity.is_finite())
        {
            return None;
        }
        let mut out = [RemoteSample::default(); REMOTE_WINDOW];
        for (dst, src) in out.iter_mut().zip(samples) {
            *dst = *src;
        }
        #[expect(clippy::cast_possible_truncation)] // len <= REMOTE_WINDOW.
        Some(Self {
            id,
            samples: out,
            len: samples.len() as u8,
        })
    }

    /// The entity.
    pub fn id(&self) -> EntityId {
        self.id
    }

    /// The samples, oldest first.
    pub fn samples(&self) -> &[RemoteSample] {
        self.samples.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// A visual correction offset decaying to zero over a window.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Correction {
    /// Offset at `start`: where the avatar was drawn minus where it now is.
    pub offset: Vec3,
    /// When the offset was applied.
    pub start: HostInstant,
    /// How long it takes to decay to zero.
    pub window: Duration,
}

impl Correction {
    /// No correction.
    pub const NONE: Correction = Correction {
        offset: Vec3::ZERO,
        start: HostInstant::ZERO,
        window: Duration::ZERO,
    };

    /// The remaining offset at `now`.
    pub fn at(&self, now: HostInstant) -> Vec3 {
        self.offset * correction_weight(now.saturating_since(self.start), self.window)
    }
}

/// Weight of a correction `elapsed` into a decay `window`: 1 at the start, 0 at the end,
/// smooth (zero slope) at both ends. `1 - smoothstep(s)`, arithmetic only.
#[expect(clippy::cast_possible_truncation)] // s is in [0, 1].
pub fn correction_weight(elapsed: Duration, window: Duration) -> f32 {
    if window.is_zero() || elapsed >= window {
        return 0.0;
    }
    let s = (elapsed.as_secs_f64() / window.as_secs_f64()) as f32;
    1.0 - s * s * (3.0 - 2.0 * s)
}

/// The local avatar as published: latest predicted state plus velocity.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LocalAvatar {
    /// The avatar entity.
    pub id: EntityId,
    /// Host instant the predicted state is presented as of: the start of the tick whose
    /// input produced it, so that input takes effect the moment it is sampled.
    pub state_time: HostInstant,
    /// Predicted position.
    pub position: Vec3,
    /// Predicted velocity.
    pub velocity: Vec3,
    /// Predicted facing.
    pub yaw: Angle16,
    /// Visual smoothing of the last reconciliation correction.
    pub correction: Correction,
}

/// How a pose was produced; useful to presentation and to tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoseSource {
    /// Local avatar, extrapolated from the latest predicted state.
    LocalPredicted,
    /// Remote entity, blended between two samples.
    RemoteInterpolated,
    /// Remote entity, extrapolated past its newest sample (bounded).
    RemoteExtrapolated,
    /// Remote entity, held at a sample because render time is outside the window.
    RemoteHeld,
}

/// An entity's pose at one render instant.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Pose {
    /// The entity.
    pub id: EntityId,
    /// Position.
    pub position: Vec3,
    /// Facing in turns, in [0, 1).
    pub yaw_turns: f32,
    /// How the pose was produced.
    pub source: PoseSource,
}

/// Returned when a render world is full; the entity is dropped and counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RenderWorldFull;

impl core::fmt::Display for RenderWorldFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("render world remote capacity exceeded")
    }
}

impl std::error::Error for RenderWorldFull {}

/// One published render world.
#[derive(Clone, Debug)]
pub struct RenderWorld {
    tick: Tick,
    published_at: HostInstant,
    config: PresentationConfig,
    local: Option<LocalAvatar>,
    remotes: Vec<RemoteEntity>,
    remote_capacity: usize,
    overflowed: u32,
}

impl RenderWorld {
    /// An empty world that can hold `remote_capacity` remote entities without allocating.
    pub fn with_capacity(remote_capacity: usize, config: PresentationConfig) -> Self {
        Self {
            tick: Tick::ZERO,
            published_at: HostInstant::ZERO,
            config,
            local: None,
            remotes: Vec::with_capacity(remote_capacity),
            remote_capacity,
            overflowed: 0,
        }
    }

    /// Clears the world and stamps it for `tick`, published at `at`. Keeps capacity.
    pub fn begin(&mut self, tick: Tick, at: HostInstant) {
        self.tick = tick;
        self.published_at = at;
        self.local = None;
        self.remotes.clear();
        self.overflowed = 0;
    }

    /// Sets the local avatar.
    pub fn set_local(&mut self, local: Option<LocalAvatar>) {
        self.local = local;
    }

    /// Adds a remote entity. Fails closed when full: the entity is not shown this tick
    /// and the overflow is counted, never silently grown into an allocation.
    ///
    /// # Errors
    /// [`RenderWorldFull`] when the world already holds its capacity of remotes.
    pub fn push_remote(&mut self, remote: RemoteEntity) -> Result<(), RenderWorldFull> {
        if self.remotes.len() >= self.remote_capacity {
            self.overflowed = self.overflowed.saturating_add(1);
            return Err(RenderWorldFull);
        }
        self.remotes.push(remote);
        Ok(())
    }

    /// Tick this world was published for.
    pub fn tick(&self) -> Tick {
        self.tick
    }

    /// Host instant the publishing tick started.
    pub fn published_at(&self) -> HostInstant {
        self.published_at
    }

    /// Sets the interpolation delay remote entities are shown behind render time (the
    /// simulation publishes its jitter buffer's delay with every world).
    pub fn set_interpolation_delay(&mut self, delay: Duration) {
        self.config.interpolation_delay = delay;
    }

    /// Presentation timing.
    pub fn config(&self) -> &PresentationConfig {
        &self.config
    }

    /// The local avatar, if any.
    pub fn local(&self) -> Option<&LocalAvatar> {
        self.local.as_ref()
    }

    /// Remote entities.
    pub fn remotes(&self) -> &[RemoteEntity] {
        &self.remotes
    }

    /// Remote capacity.
    pub fn remote_capacity(&self) -> usize {
        self.remote_capacity
    }

    /// Remotes dropped this tick because the world was full.
    pub fn overflowed(&self) -> u32 {
        self.overflowed
    }

    /// The local avatar's pose at render instant `now`.
    #[expect(clippy::cast_possible_truncation)] // Bounded extrapolation seconds fit f32.
    pub fn sample_local(&self, now: HostInstant) -> Option<Pose> {
        let l = self.local.as_ref()?;
        let ahead = now
            .saturating_since(l.state_time)
            .min(self.config.max_local_extrapolation);
        let dt = ahead.as_secs_f64() as f32;
        let position = l.position + l.velocity * dt + l.correction.at(now);
        Some(Pose {
            id: l.id,
            position,
            yaw_turns: angle_to_turns(l.yaw),
            source: PoseSource::LocalPredicted,
        })
    }

    /// A remote entity's pose at render instant `now` (the delay is applied here).
    pub fn sample_remote(&self, remote: &RemoteEntity, now: HostInstant) -> Pose {
        sample_window(
            remote,
            now.saturating_sub(self.config.interpolation_delay),
            self.config.max_remote_extrapolation,
        )
    }

    /// Samples every entity at `now` into `out`, local avatar first. Does not allocate
    /// while `out` has capacity for every entity; entities past capacity are skipped and
    /// counted by [`FramePoses::skipped`].
    pub fn sample_into(&self, now: HostInstant, out: &mut FramePoses) {
        out.clear();
        if let Some(p) = self.sample_local(now) {
            out.push(p);
        }
        let t = now.saturating_sub(self.config.interpolation_delay);
        for r in &self.remotes {
            out.push(sample_window(r, t, self.config.max_remote_extrapolation));
        }
    }
}

/// Samples a remote window at interpolation instant `t`.
#[expect(clippy::cast_possible_truncation)] // Ratios and bounded seconds fit f32.
fn sample_window(remote: &RemoteEntity, t: HostInstant, max_extrapolation: Duration) -> Pose {
    let samples = remote.samples();
    let id = remote.id;
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        // Unreachable by construction (len >= 1); fail closed to the origin.
        return Pose {
            id,
            position: Vec3::ZERO,
            yaw_turns: 0.0,
            source: PoseSource::RemoteHeld,
        };
    };
    if t < first.time {
        return Pose {
            id,
            position: first.position,
            yaw_turns: angle_to_turns(first.yaw),
            source: PoseSource::RemoteHeld,
        };
    }
    if t > last.time {
        let ahead = t.saturating_since(last.time);
        if ahead > max_extrapolation {
            let dt = max_extrapolation.as_secs_f64() as f32;
            let position = last.position + last.velocity * dt;
            return Pose {
                id,
                position,
                yaw_turns: angle_to_turns(last.yaw),
                source: PoseSource::RemoteHeld,
            };
        }
        let dt = ahead.as_secs_f64() as f32;
        let position = last.position + last.velocity * dt;
        return Pose {
            id,
            position,
            yaw_turns: angle_to_turns(last.yaw),
            source: PoseSource::RemoteExtrapolated,
        };
    }
    for pair in samples.windows(2) {
        if let [a, b] = pair
            && t >= a.time
            && t < b.time
        {
            let span = b.time.saturating_since(a.time).as_secs_f64();
            let alpha = (t.saturating_since(a.time).as_secs_f64() / span) as f32;
            let position = a.position + (b.position - a.position) * alpha;
            let yaw = angle_to_turns(a.yaw) + f32::from(angle_delta(a.yaw, b.yaw)) / 65_536.0 * alpha;
            return Pose {
                id,
                position,
                yaw_turns: yaw.rem_euclid(1.0),
                source: PoseSource::RemoteInterpolated,
            };
        }
    }
    // t is exactly the newest sample.
    Pose {
        id,
        position: last.position,
        yaw_turns: angle_to_turns(last.yaw),
        source: PoseSource::RemoteInterpolated,
    }
}

/// Per-frame pose output with fixed capacity.
#[derive(Clone, Debug)]
pub struct FramePoses {
    poses: Vec<Pose>,
    capacity: usize,
    skipped: u32,
}

impl FramePoses {
    /// Output that holds `capacity` poses without allocating.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            poses: Vec::with_capacity(capacity),
            capacity,
            skipped: 0,
        }
    }

    /// Clears poses and the skip count.
    pub fn clear(&mut self) {
        self.poses.clear();
        self.skipped = 0;
    }

    fn push(&mut self, p: Pose) {
        if self.poses.len() < self.capacity {
            self.poses.push(p);
        } else {
            self.skipped = self.skipped.saturating_add(1);
        }
    }

    /// The poses.
    pub fn as_slice(&self) -> &[Pose] {
        &self.poses
    }

    /// Poses dropped because the output was full.
    pub fn skipped(&self) -> u32 {
        self.skipped
    }
}

// ---------------------------------------------------------------------------------------
// Publish channel
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
struct Slot {
    world: Box<RenderWorld>,
    fresh: bool,
    published: u64,
}

/// Sim-thread end of the render world channel.
#[derive(Debug)]
pub struct RenderWorldPublisher {
    back: Box<RenderWorld>,
    shared: Arc<Mutex<Slot>>,
}

/// Render-thread end of the render world channel.
#[derive(Debug)]
pub struct RenderWorldReader {
    front: Box<RenderWorld>,
    shared: Arc<Mutex<Slot>>,
    acquired: u64,
}

/// Creates a render world channel with three preallocated worlds.
pub fn render_world_channel(
    remote_capacity: usize,
    config: PresentationConfig,
) -> (RenderWorldPublisher, RenderWorldReader) {
    let mk = || Box::new(RenderWorld::with_capacity(remote_capacity, config));
    let shared = Arc::new(Mutex::new(Slot {
        world: mk(),
        fresh: false,
        published: 0,
    }));
    (
        RenderWorldPublisher {
            back: mk(),
            shared: Arc::clone(&shared),
        },
        RenderWorldReader {
            front: mk(),
            shared,
            acquired: 0,
        },
    )
}

impl RenderWorldPublisher {
    /// The world being written for the next publish.
    pub fn back_mut(&mut self) -> &mut RenderWorld {
        &mut self.back
    }

    /// Publishes the back world. The render thread sees it on its next acquire; a world
    /// published but never acquired is replaced, which is correct because only the latest
    /// state matters. Returns the publish count.
    pub fn publish(&mut self) -> u64 {
        let mut slot = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        core::mem::swap(&mut slot.world, &mut self.back);
        slot.fresh = true;
        slot.published = slot.published.wrapping_add(1);
        slot.published
    }

    /// Total publishes so far.
    pub fn published(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .published
    }
}

impl RenderWorldReader {
    /// Takes the newest published world if there is one, and returns the world to render.
    /// Between publishes it keeps returning the same world; frames never block on ticks.
    pub fn acquire(&mut self) -> &RenderWorld {
        {
            let mut slot = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.fresh {
                core::mem::swap(&mut slot.world, &mut self.front);
                slot.fresh = false;
                self.acquired = slot.published;
            }
        }
        &self.front
    }

    /// The world returned by the last acquire.
    pub fn current(&self) -> &RenderWorld {
        &self.front
    }

    /// Publish count of the world currently held (0 before the first).
    pub fn acquired_publish(&self) -> u64 {
        self.acquired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_api::angle_from_turns;
    use crate::testing::TestResult;

    const MS: u64 = 1_000_000;

    fn at_ms(ms: u64) -> HostInstant {
        HostInstant::from_nanos(ms * MS)
    }

    fn sample(ms: u64, x: f32, vx: f32, yaw: u16) -> RemoteSample {
        RemoteSample {
            time: at_ms(ms),
            position: Vec3::new(x, 0.0, 0.0),
            velocity: Vec3::new(vx, 0.0, 0.0),
            yaw: Angle16(yaw),
        }
    }

    fn cfg(delay_ms: u64) -> PresentationConfig {
        PresentationConfig {
            interpolation_delay: Duration::from_millis(delay_ms),
            max_remote_extrapolation: Duration::from_millis(100),
            max_local_extrapolation: Duration::from_millis(40),
        }
    }

    #[test]
    fn remote_window_validation() {
        let id = EntityId::new(1, 0);
        assert!(RemoteEntity::new(id, &[]).is_none());
        assert!(RemoteEntity::new(id, &[sample(10, 0.0, 0.0, 0), sample(10, 1.0, 0.0, 0)]).is_none());
        assert!(RemoteEntity::new(id, &[sample(20, 0.0, 0.0, 0), sample(10, 1.0, 0.0, 0)]).is_none());
        let four = [
            sample(1, 0.0, 0.0, 0),
            sample(2, 0.0, 0.0, 0),
            sample(3, 0.0, 0.0, 0),
            sample(4, 0.0, 0.0, 0),
        ];
        assert!(RemoteEntity::new(id, &four).is_none());
        let mut bad = sample(1, 0.0, 0.0, 0);
        bad.position.x = f32::NAN;
        assert!(RemoteEntity::new(id, &[bad]).is_none());
        assert_eq!(
            RemoteEntity::new(id, &four[..3]).map(|r| r.samples().len()),
            Some(3)
        );
    }

    #[test]
    fn remote_interpolates_between_bracketing_pair() -> TestResult {
        let w = RenderWorld::with_capacity(4, cfg(100));
        let r = RemoteEntity::new(
            EntityId::new(1, 0),
            &[
                sample(100, 0.0, 0.0, 0),
                sample(200, 10.0, 0.0, 0),
                sample(300, 30.0, 0.0, 0),
            ],
        )
        .ok_or("window")?;
        // Render 250 ms → interpolation instant 150 ms: halfway through the first pair.
        let p = w.sample_remote(&r, at_ms(250));
        assert_eq!(p.source, PoseSource::RemoteInterpolated);
        assert!((p.position.x - 5.0).abs() < 1e-5);
        // Render 350 ms → 250 ms: halfway through the second pair.
        let p = w.sample_remote(&r, at_ms(350));
        assert!((p.position.x - 20.0).abs() < 1e-5);
        // Exactly on a sample, including the first and the newest: that sample, interpolated.
        for (render_ms, x) in [(200, 0.0), (300, 10.0), (400, 30.0)] {
            let p = w.sample_remote(&r, at_ms(render_ms));
            assert_eq!((p.source, p.position.x), (PoseSource::RemoteInterpolated, x));
        }
        // Before the window: held at the first sample.
        let p = w.sample_remote(&r, at_ms(150));
        assert_eq!((p.source, p.position.x), (PoseSource::RemoteHeld, 0.0));
        Ok(())
    }

    #[test]
    fn remote_extrapolation_is_bounded() -> TestResult {
        let w = RenderWorld::with_capacity(4, cfg(0));
        let r = RemoteEntity::new(EntityId::new(1, 0), &[sample(0, 0.0, 10.0, 0)]).ok_or("window")?;
        let p = w.sample_remote(&r, at_ms(50));
        assert_eq!(p.source, PoseSource::RemoteExtrapolated);
        assert!((p.position.x - 0.5).abs() < 1e-5);
        // Past the 100 ms bound the entity holds at the bound.
        let p = w.sample_remote(&r, at_ms(10_000));
        assert_eq!(p.source, PoseSource::RemoteHeld);
        assert!((p.position.x - 1.0).abs() < 1e-5);
        Ok(())
    }

    #[test]
    fn remote_yaw_takes_shortest_arc_across_zero() -> TestResult {
        let world = RenderWorld::with_capacity(1, cfg(0));
        // From 0.9 turns to 0.1 turns is +0.2 through zero, not -0.8.
        let from = angle_from_turns(0.9).0;
        let to = angle_from_turns(0.1).0;
        let remote = RemoteEntity::new(
            EntityId::new(1, 0),
            &[sample(0, 0.0, 0.0, from), sample(100, 0.0, 0.0, to)],
        )
        .ok_or("window")?;
        let mid = world.sample_remote(&remote, at_ms(50));
        assert!(
            mid.yaw_turns.abs() < 1e-3 || (mid.yaw_turns - 1.0).abs() < 1e-3,
            "{}",
            mid.yaw_turns
        );
        let quarter = world.sample_remote(&remote, at_ms(25));
        assert!((quarter.yaw_turns - 0.95).abs() < 1e-3, "{}", quarter.yaw_turns);
        Ok(())
    }

    #[test]
    fn local_avatar_is_extrapolated_not_interpolated() -> TestResult {
        let mut w = RenderWorld::with_capacity(0, cfg(100));
        w.set_local(Some(LocalAvatar {
            id: EntityId::new(9, 1),
            state_time: at_ms(1000),
            position: Vec3::new(1.0, 0.0, 0.0),
            velocity: Vec3::new(10.0, 0.0, 0.0),
            yaw: Angle16(0),
            correction: Correction::NONE,
        }));
        // At its state time the avatar is exactly at the predicted state: no tick of lag.
        let p = w.sample_local(at_ms(1000)).ok_or("local")?;
        assert_eq!(p.position.x, 1.0);
        // 20 ms later it has moved on by velocity; the interpolation delay does not apply.
        let p = w.sample_local(at_ms(1020)).ok_or("local")?;
        assert!((p.position.x - 1.2).abs() < 1e-5);
        // Extrapolation is bounded at 40 ms.
        let p = w.sample_local(at_ms(5000)).ok_or("local")?;
        assert!((p.position.x - 1.4).abs() < 1e-5);
        // Before its state time (a frame that started before the publish) it is not pulled back.
        let p = w.sample_local(at_ms(990)).ok_or("local")?;
        assert_eq!(p.position.x, 1.0);
        Ok(())
    }

    #[test]
    fn correction_decays_smoothly_to_zero() {
        let c = Correction {
            offset: Vec3::new(1.0, 0.0, 0.0),
            start: at_ms(100),
            window: Duration::from_millis(100),
        };
        assert_eq!(c.at(at_ms(100)).x, 1.0);
        assert!((c.at(at_ms(150)).x - 0.5).abs() < 1e-6);
        assert_eq!(c.at(at_ms(200)).x, 0.0);
        assert_eq!(c.at(at_ms(900)).x, 0.0);
        // Monotonic decay.
        let mut prev = 1.0f32;
        for ms in 100..=200 {
            let v = c.at(at_ms(ms)).x;
            assert!(v <= prev);
            prev = v;
        }
        assert_eq!(correction_weight(Duration::ZERO, Duration::ZERO), 0.0);
    }

    #[test]
    fn world_capacity_fails_closed_and_counts() -> TestResult {
        let mut w = RenderWorld::with_capacity(1, cfg(0));
        let r = RemoteEntity::new(EntityId::new(1, 0), &[sample(0, 0.0, 0.0, 0)]).ok_or("window")?;
        w.push_remote(r)?;
        assert_eq!(w.push_remote(r), Err(RenderWorldFull));
        assert_eq!(w.overflowed(), 1);
        w.begin(Tick(2), at_ms(5));
        assert_eq!((w.remotes().len(), w.overflowed(), w.tick()), (0, 0, Tick(2)));
        Ok(())
    }

    #[test]
    fn sample_into_respects_capacity() -> TestResult {
        let mut w = RenderWorld::with_capacity(3, cfg(0));
        for i in 0..3 {
            w.push_remote(
                RemoteEntity::new(EntityId::new(i, 0), &[sample(0, 0.0, 0.0, 0)]).ok_or("window")?,
            )?;
        }
        let mut out = FramePoses::with_capacity(2);
        w.sample_into(at_ms(0), &mut out);
        assert_eq!((out.as_slice().len(), out.skipped()), (2, 1));
        Ok(())
    }

    #[test]
    fn channel_delivers_latest_and_reuses_buffers() {
        let (mut publisher, mut reader) = render_world_channel(8, cfg(0));
        assert_eq!(reader.acquire().tick(), Tick::ZERO);
        assert_eq!(reader.acquired_publish(), 0);

        publisher.back_mut().begin(Tick(1), at_ms(1));
        assert_eq!(publisher.publish(), 1);
        publisher.back_mut().begin(Tick(2), at_ms(2));
        assert_eq!(publisher.publish(), 2);
        // Two publishes between frames: the reader sees only the latest.
        assert_eq!(reader.acquire().tick(), Tick(2));
        assert_eq!(reader.acquired_publish(), 2);
        // No publish since: the same world again, frames never block on ticks.
        assert_eq!(reader.acquire().tick(), Tick(2));

        // Buffers rotate; capacity is preserved through every swap.
        for t in 3..20 {
            publisher.back_mut().begin(Tick(t), at_ms(t));
            let _ = publisher.publish();
            assert_eq!(reader.acquire().tick(), Tick(t));
            assert_eq!(reader.current().remote_capacity(), 8);
            assert!(reader.current().remotes.capacity() >= 8);
        }
        assert_eq!(publisher.published(), 19);
    }

    #[test]
    fn channel_works_across_threads() -> TestResult {
        let (mut publisher, mut reader) = render_world_channel(2, cfg(0));
        let h = std::thread::spawn(move || {
            for t in 1..=1000u64 {
                publisher.back_mut().begin(Tick(t), at_ms(t));
                let _ = publisher.publish();
            }
        });
        let mut last = 0u64;
        loop {
            let t = reader.acquire().tick().0;
            assert!(t >= last, "render world went backwards: {t} after {last}");
            last = t;
            if t == 1000 {
                break;
            }
            std::thread::yield_now();
        }
        h.join().map_err(|_| "publisher thread panicked")?;
        Ok(())
    }
}
