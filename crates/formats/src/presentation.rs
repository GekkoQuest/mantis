//! Presentation graph v1: client-only content that binds visual, audio, camera, and
//! animation reactions to the timeline markers gameplay graphs emit (decision 0006).
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); action offsets are world-space vectors from the anchor.
//!
//! A binding names a marker id, `(graph, node)` as in `mantis_core::graph::MarkerId`,
//! plus a filter on the marker's kind, and lists the actions to run, each with a delay
//! after the marker's time. The server never loads this format, and it has its own content
//! hash: retiming a particle burst never changes the gameplay bundle.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MPRS"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | binding count `u32`, at most [`MAX_BINDINGS`] |
//! | 12 | 4 | reserved, 0 |
//! | 16 | | binding records, each followed by its action records |
//!
//! Binding record (12 bytes), sorted by `(graph, node, filter code, filter argument)`,
//! no two equal:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | graph `u32` |
//! | 4 | 2 | node `u16` |
//! | 6 | 1 | filter `u8`: 0 any, 1 cast start, 2 impact, 3 tick, 4 expire, 5 package |
//! | 7 | 1 | action count `u8`, 1 to [`MAX_ACTIONS`] |
//! | 8 | 2 | filter argument `u16`: tick number (0 every tick) for 3, package kind for 5, else 0 |
//! | 10 | 2 | reserved, 0 |
//!
//! Action record (64 bytes):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 1 | op `u8`: 0 spawn effect, 1 play sound, 2 camera shake, 3 animation trigger |
//! | 1 | 1 | anchor `u8`: 0 the marker's source entity, 1 its target |
//! | 2 | 1 | flags `u8`: bit 0 follow the anchor (effects and sounds only); other bits 0 |
//! | 3 | 1 | reserved, 0 |
//! | 4 | 4 | delay `f32` seconds after the marker, 0 to [`MAX_DELAY`] |
//! | 8 | 12 | offset `[f32; 3]` from the anchor, meters |
//! | 20 | 4 | value `f32`: effect scale (> 0, at most [`MAX_EFFECT_SCALE`]), sound volume (0 to 4), shake amplitude (> 0, at most 10); 0 for animation triggers |
//! | 24 | 32 | payload, by op (unused bytes 0): effect content hash `[u8; 32]`; sound id `u32` then pitch `f32` (0.25 to 4); shake frequency `f32` (> 0, at most 100 Hz), duration `f32` (> 0, at most 10 s), radius `f32` (> 0); animation parameter name hash `u32` |
//! | 56 | 8 | reserved, 0 |
//!
//! Every `f32` is finite. The length must match exactly.

use mantis_core::content::ContentHash;

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MPRS";
/// Most bindings in one graph.
pub const MAX_BINDINGS: usize = 65_536;
/// Most actions in one binding.
pub const MAX_ACTIONS: usize = 16;
/// Longest action delay, seconds.
pub const MAX_DELAY: f32 = 60.0;
/// Largest effect scale.
pub const MAX_EFFECT_SCALE: f32 = 100.0;

/// Which marker kinds a binding reacts to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MarkerFilter {
    /// Every kind.
    Any,
    /// `CastStart`.
    CastStart,
    /// `Impact`.
    Impact,
    /// `TickN`: a specific tick number, or every tick when 0.
    Tick(u16),
    /// `Expire`.
    Expire,
    /// A package-defined kind.
    Package(u16),
}

impl MarkerFilter {
    fn code(self) -> (u8, u16) {
        match self {
            MarkerFilter::Any => (0, 0),
            MarkerFilter::CastStart => (1, 0),
            MarkerFilter::Impact => (2, 0),
            MarkerFilter::Tick(n) => (3, n),
            MarkerFilter::Expire => (4, 0),
            MarkerFilter::Package(p) => (5, p),
        }
    }

    fn from_code(code: u8, arg: u16) -> Result<Self, FormatError> {
        let filter = match code {
            0 => MarkerFilter::Any,
            1 => MarkerFilter::CastStart,
            2 => MarkerFilter::Impact,
            3 => MarkerFilter::Tick(arg),
            4 => MarkerFilter::Expire,
            5 => MarkerFilter::Package(arg),
            other => return Err(FormatError::Encoding(u32::from(other))),
        };
        if filter.code() == (code, arg) {
            Ok(filter)
        } else {
            Err(FormatError::Reserved)
        }
    }
}

/// Which entity an action is placed on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Anchor {
    /// The entity running the gameplay graph.
    Source,
    /// The graph instance's target.
    Target,
}

/// What an action does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ActionOp {
    /// Spawns a particle effect (a `particle_effect` asset).
    SpawnEffect {
        /// The effect asset.
        effect: ContentHash,
        /// Uniform scale of the effect instance (> 0, at most [`MAX_EFFECT_SCALE`]): spawn
        /// offsets, speeds, acceleration, and particle sizes scale with it.
        scale: f32,
        /// Follow the anchor while alive.
        follow: bool,
    },
    /// Plays a sound of the package's sound bank.
    PlaySound {
        /// Sound id in the bank.
        sound: u32,
        /// Linear volume.
        volume: f32,
        /// Pitch multiplier.
        pitch: f32,
        /// Follow the anchor while playing.
        follow: bool,
    },
    /// Shakes the camera when it is within `radius` of the anchor.
    CameraShake {
        /// Peak angular amplitude, degrees.
        amplitude: f32,
        /// Oscillation frequency, hertz.
        frequency: f32,
        /// Seconds until the shake has decayed to nothing.
        duration: f32,
        /// Reach from the anchor, meters.
        radius: f32,
    },
    /// Fires a trigger parameter on the anchor's animation graph.
    AnimTrigger {
        /// Parameter name hash (as in the animation graph format).
        parameter: u32,
    },
}

/// One scheduled reaction.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Action {
    /// Seconds after the marker.
    pub delay: f32,
    /// Where it happens.
    pub anchor: Anchor,
    /// Offset from the anchor, meters.
    pub offset: [f32; 3],
    /// What happens.
    pub op: ActionOp,
}

/// Actions bound to one marker id and kind filter.
#[derive(Clone, PartialEq, Debug)]
pub struct Binding {
    /// Gameplay graph id (`mantis_core::graph::GraphId`).
    pub graph: u32,
    /// Node key within it (`mantis_core::graph::NodeKey`).
    pub node: u16,
    /// Which marker kinds.
    pub filter: MarkerFilter,
    /// The actions, in authored order.
    pub actions: Vec<Action>,
}

impl Binding {
    fn key(&self) -> (u32, u16, (u8, u16)) {
        (self.graph, self.node, self.filter.code())
    }
}

/// A presentation graph: every binding of one content unit.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PresentationGraph {
    /// Bindings, sorted by marker id and filter.
    pub bindings: Vec<Binding>,
}

fn check_action(a: &Action) -> Result<(), FormatError> {
    let finite = a.delay.is_finite() && a.offset.iter().all(|v| v.is_finite());
    if !finite {
        return Err(FormatError::NonFinite);
    }
    let within = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
    let positive = |v: f32, hi: f32| v.is_finite() && v > 0.0 && v <= hi;
    let ok = within(a.delay, 0.0, MAX_DELAY)
        && match a.op {
            ActionOp::SpawnEffect { scale, .. } => positive(scale, MAX_EFFECT_SCALE),
            ActionOp::PlaySound { volume, pitch, .. } => within(volume, 0.0, 4.0) && within(pitch, 0.25, 4.0),
            ActionOp::CameraShake {
                amplitude,
                frequency,
                duration,
                radius,
            } => {
                positive(amplitude, 10.0)
                    && positive(frequency, 100.0)
                    && positive(duration, 10.0)
                    && positive(radius, f32::MAX)
            }
            ActionOp::AnimTrigger { .. } => true,
        };
    if ok { Ok(()) } else { Err(FormatError::Dimensions) }
}

impl PresentationGraph {
    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for counts or values out of range,
    /// [`FormatError::NonFinite`], or [`FormatError::Inconsistent`] when bindings are not
    /// strictly sorted.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.bindings.len() > MAX_BINDINGS {
            return Err(FormatError::Dimensions);
        }
        for b in &self.bindings {
            if b.actions.is_empty() || b.actions.len() > MAX_ACTIONS {
                return Err(FormatError::Dimensions);
            }
            for a in &b.actions {
                check_action(a)?;
            }
        }
        if self
            .bindings
            .windows(2)
            .any(|w| matches!(w, [x, y] if x.key() >= y.key()))
        {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    /// Parses and validates.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<PresentationGraph, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let count = r.u32()? as usize;
        if count > MAX_BINDINGS {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let mut bindings = Vec::with_capacity(count.min(r.remaining() / 76));
        for _ in 0..count {
            bindings.push(parse_binding(&mut r)?);
        }
        r.finish()?;
        let graph = PresentationGraph { bindings };
        graph.validate()?;
        Ok(graph)
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.count(self.bindings.len());
        w.u32(0);
        for b in &self.bindings {
            let (code, arg) = b.filter.code();
            w.u32(b.graph);
            w.u16(b.node);
            w.u8(code);
            w.u8(u8::try_from(b.actions.len()).unwrap_or(u8::MAX));
            w.u16(arg);
            w.u16(0);
            for a in &b.actions {
                write_action(&mut w, a);
            }
        }
        w.into_bytes()
    }

    /// The content hash of the encoded graph (its own domain, separate from gameplay).
    pub fn content_hash(&self) -> ContentHash {
        ContentHash::of(&self.encode())
    }
}

fn parse_binding(r: &mut Reader<'_>) -> Result<Binding, FormatError> {
    let graph = r.u32()?;
    let node = r.u16()?;
    let code = r.u8()?;
    let action_count = usize::from(r.u8()?);
    let arg = r.u16()?;
    if r.u16()? != 0 {
        return Err(FormatError::Reserved);
    }
    let filter = MarkerFilter::from_code(code, arg)?;
    if action_count == 0 || action_count > MAX_ACTIONS {
        return Err(FormatError::Dimensions);
    }
    let mut actions = Vec::with_capacity(action_count);
    for _ in 0..action_count {
        actions.push(parse_action(r)?);
    }
    Ok(Binding {
        graph,
        node,
        filter,
        actions,
    })
}

fn parse_action(r: &mut Reader<'_>) -> Result<Action, FormatError> {
    let op = r.u8()?;
    let anchor = match r.u8()? {
        0 => Anchor::Source,
        1 => Anchor::Target,
        other => return Err(FormatError::Encoding(u32::from(other))),
    };
    let flags = r.u8()?;
    if r.u8()? != 0 {
        return Err(FormatError::Reserved);
    }
    let delay = r.f32_raw()?;
    let offset = [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?];
    let value = r.f32_raw()?;
    let payload: [u8; 32] = r.array()?;
    if r.array::<8>()? != [0; 8] {
        return Err(FormatError::Reserved);
    }
    let follow = flags & 1 != 0;
    let word = |i: usize| -> [u8; 4] {
        let mut w = [0u8; 4];
        for (d, s) in w.iter_mut().zip(payload.iter().skip(i * 4)) {
            *d = *s;
        }
        w
    };
    let used_words = match op {
        0 => 8,
        1 => 2,
        2 => 3,
        3 => 1,
        other => return Err(FormatError::Encoding(u32::from(other))),
    };
    if payload.iter().skip(used_words * 4).any(|b| *b != 0) {
        return Err(FormatError::Reserved);
    }
    let follows = matches!(op, 0 | 1);
    if flags & !1 != 0 || (follow && !follows) {
        return Err(FormatError::Flags(u32::from(flags)));
    }
    let f = |i: usize| f32::from_le_bytes(word(i));
    let op = match op {
        0 => ActionOp::SpawnEffect {
            effect: ContentHash::from_bytes(payload),
            scale: value,
            follow,
        },
        1 => ActionOp::PlaySound {
            sound: u32::from_le_bytes(word(0)),
            volume: value,
            pitch: f(1),
            follow,
        },
        2 => ActionOp::CameraShake {
            amplitude: value,
            frequency: f(0),
            duration: f(1),
            radius: f(2),
        },
        _ => {
            if value != 0.0 {
                return Err(FormatError::Reserved);
            }
            ActionOp::AnimTrigger {
                parameter: u32::from_le_bytes(word(0)),
            }
        }
    };
    let action = Action {
        delay,
        anchor,
        offset,
        op,
    };
    check_action(&action)?;
    Ok(action)
}

fn write_action(w: &mut Writer, a: &Action) {
    let (op, value, follow, payload) = match a.op {
        ActionOp::SpawnEffect {
            effect,
            scale,
            follow,
        } => (0u8, scale, follow, *effect.as_bytes()),
        ActionOp::PlaySound {
            sound,
            volume,
            pitch,
            follow,
        } => {
            let mut p = [0u8; 32];
            for (d, s) in p
                .iter_mut()
                .zip(sound.to_le_bytes().into_iter().chain(pitch.to_le_bytes()))
            {
                *d = s;
            }
            (1, volume, follow, p)
        }
        ActionOp::CameraShake {
            amplitude,
            frequency,
            duration,
            radius,
        } => {
            let mut p = [0u8; 32];
            let words = frequency
                .to_le_bytes()
                .into_iter()
                .chain(duration.to_le_bytes())
                .chain(radius.to_le_bytes());
            for (d, s) in p.iter_mut().zip(words) {
                *d = s;
            }
            (2, amplitude, false, p)
        }
        ActionOp::AnimTrigger { parameter } => {
            let mut p = [0u8; 32];
            for (d, s) in p.iter_mut().zip(parameter.to_le_bytes()) {
                *d = s;
            }
            (3, 0.0, false, p)
        }
    };
    w.u8(op);
    w.u8(match a.anchor {
        Anchor::Source => 0,
        Anchor::Target => 1,
    });
    w.u8(u8::from(follow));
    w.u8(0);
    w.f32(a.delay);
    w.vec3(a.offset);
    w.f32(value);
    w.bytes(&payload);
    w.bytes(&[0; 8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample() -> PresentationGraph {
        let hash = ContentHash::from_bytes([7; 32]);
        PresentationGraph {
            bindings: vec![
                Binding {
                    graph: 10,
                    node: 1,
                    filter: MarkerFilter::CastStart,
                    actions: vec![
                        Action {
                            delay: 0.0,
                            anchor: Anchor::Source,
                            offset: [0.0, 1.0, 0.0],
                            op: ActionOp::SpawnEffect {
                                effect: hash,
                                scale: 1.0,
                                follow: true,
                            },
                        },
                        Action {
                            delay: 0.1,
                            anchor: Anchor::Source,
                            offset: [0.0; 3],
                            op: ActionOp::PlaySound {
                                sound: 42,
                                volume: 0.8,
                                pitch: 1.0,
                                follow: false,
                            },
                        },
                    ],
                },
                Binding {
                    graph: 10,
                    node: 2,
                    filter: MarkerFilter::Impact,
                    actions: vec![
                        Action {
                            delay: 0.0,
                            anchor: Anchor::Target,
                            offset: [0.0; 3],
                            op: ActionOp::CameraShake {
                                amplitude: 1.5,
                                frequency: 12.0,
                                duration: 0.4,
                                radius: 20.0,
                            },
                        },
                        Action {
                            delay: 0.0,
                            anchor: Anchor::Target,
                            offset: [0.0; 3],
                            op: ActionOp::AnimTrigger {
                                parameter: 0xdead_beef,
                            },
                        },
                    ],
                },
                Binding {
                    graph: 10,
                    node: 2,
                    filter: MarkerFilter::Tick(0),
                    actions: vec![Action {
                        delay: 0.25,
                        anchor: Anchor::Target,
                        offset: [0.0; 3],
                        op: ActionOp::PlaySound {
                            sound: 43,
                            volume: 1.0,
                            pitch: 1.2,
                            follow: true,
                        },
                    }],
                },
            ],
        }
    }

    #[test]
    fn round_trips_with_its_own_hash() -> TestResult {
        let g = sample();
        let bytes = g.encode();
        assert_eq!(bytes.len(), 16 + 3 * 12 + 5 * 64);
        assert_eq!(PresentationGraph::parse(&bytes)?, g);
        let mut retimed = g.clone();
        if let Some(a) = retimed.bindings.first_mut().and_then(|b| b.actions.get_mut(1)) {
            a.delay = 0.2;
        }
        assert_ne!(
            retimed.content_hash(),
            g.content_hash(),
            "retiming changes only this hash"
        );
        Ok(())
    }

    #[test]
    fn rules_reject() {
        let bad = |edit: fn(&mut PresentationGraph)| {
            let mut g = sample();
            edit(&mut g);
            PresentationGraph::parse(&g.encode()).err()
        };
        assert_eq!(
            bad(|g| g.bindings.swap(0, 1)),
            Some(FormatError::Inconsistent),
            "unsorted"
        );
        assert_eq!(
            bad(|g| if let Some(b) = g.bindings.first_mut() {
                b.actions.clear();
            }),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            bad(
                |g| if let Some(a) = g.bindings.first_mut().and_then(|b| b.actions.first_mut()) {
                    a.delay = 61.0;
                }
            ),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            bad(
                |g| if let Some(a) = g.bindings.first_mut().and_then(|b| b.actions.first_mut()) {
                    a.offset = [f32::NAN, 0.0, 0.0];
                }
            ),
            Some(FormatError::NonFinite)
        );
        assert_eq!(
            bad(
                |g| if let Some(a) = g.bindings.get_mut(1).and_then(|b| b.actions.first_mut()) {
                    a.op = ActionOp::CameraShake {
                        amplitude: 0.0,
                        frequency: 1.0,
                        duration: 1.0,
                        radius: 1.0,
                    };
                }
            ),
            Some(FormatError::Dimensions)
        );
        // Effect scale: positive and at most MAX_EFFECT_SCALE.
        let scaled = |scale: f32| {
            let mut g = sample();
            for a in g.bindings.iter_mut().flat_map(|b| b.actions.iter_mut()) {
                if let ActionOp::SpawnEffect { scale: s, .. } = &mut a.op {
                    *s = scale;
                }
            }
            PresentationGraph::parse(&g.encode()).err()
        };
        assert_eq!(scaled(2.5), None, "a scaled effect");
        assert_eq!(scaled(MAX_EFFECT_SCALE), None);
        assert_eq!(scaled(0.0), Some(FormatError::Dimensions));
        assert_eq!(scaled(MAX_EFFECT_SCALE * 1.5), Some(FormatError::Dimensions));
        // Header and record level corruption.
        let bytes = sample().encode();
        let corrupt = |at: usize, v: &[u8]| {
            let mut b = bytes.clone();
            if let Some(s) = b.get_mut(at..at + v.len()) {
                s.copy_from_slice(v);
            }
            PresentationGraph::parse(&b).err()
        };
        assert_eq!(corrupt(0, b"MPRX"), Some(FormatError::Magic));
        assert_eq!(corrupt(6, &[1, 0]), Some(FormatError::Flags(1)));
        assert_eq!(
            corrupt(16 + 6, &[9]),
            Some(FormatError::Encoding(9)),
            "filter code"
        );
        assert_eq!(
            corrupt(16 + 8, &[1, 0]),
            Some(FormatError::Reserved),
            "argument on cast start"
        );
        assert_eq!(corrupt(28, &[7]), Some(FormatError::Encoding(7)), "action op");
        assert_eq!(corrupt(28 + 2, &[2]), Some(FormatError::Flags(2)));
        assert_eq!(corrupt(28 + 56, &[1]), Some(FormatError::Reserved));
    }

    #[test]
    fn no_corruption_or_truncation_panics() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            assert!(PresentationGraph::parse(bytes.get(..cut).unwrap_or(&[])).is_err());
        }
        for i in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                if let Some(x) = b.get_mut(i) {
                    *x ^= mask;
                }
                if let Ok(g) = PresentationGraph::parse(&b) {
                    assert_eq!(PresentationGraph::parse(&g.encode()).ok(), Some(g));
                }
            }
        }
    }
}
