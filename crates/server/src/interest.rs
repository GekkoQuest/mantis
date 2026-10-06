//! Interest management (plan 7.4): a spatial hash of everything replicable,
//! rebuilt once per tick on the cell thread, then read by every client's job.
//!
//! The hash is a pre-sized vector sorted by grid-cell key: rebuilding it is a
//! clear, a fill, and an allocation-free sort; a radius query binary-searches
//! one contiguous key range per grid row.

use mantis_adapter_contract::AppearanceId;
use mantis_adapter_contract::core_types::{Angle16, Tick, TimelineMarker, Vec3};
use mantis_core::mem::BoundedVec;

use crate::components::ReplicationId;

/// One interest tier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    /// Close: highest priority weight.
    Near,
    /// Medium distance.
    Mid,
    /// Far: lowest weight.
    Far,
}

/// Tier radii and priority weights. Priority accumulates every tick by the
/// tier's weight and resets when the entity is sent, so with a per-snapshot
/// entity budget, near entities send every tick and mid and far ones at
/// proportionally lower rates, and no entity starves.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TierConfig {
    /// Near radius.
    pub near: f32,
    /// Mid radius.
    pub mid: f32,
    /// Far radius (interest boundary).
    pub far: f32,
    /// Extra distance before an entity leaves interest (prevents flicker).
    pub hysteresis: f32,
    /// Priority weights per tier.
    pub weights: [u32; 3],
    /// Remote samples per snapshot before the payload limit applies.
    pub budget: usize,
}

impl TierConfig {
    /// Generic defaults for the toy package.
    pub const DEFAULT: Self = Self {
        near: 20.0,
        mid: 50.0,
        far: 100.0,
        hysteresis: 5.0,
        weights: [9, 3, 1],
        // Meets the cell-500-100 snapshot-bytes budget row (about 16 bytes
        // per remote on the native wire, every tick).
        budget: 32,
    };

    /// The tier at distance `d`, or `None` beyond the far radius.
    #[must_use]
    pub fn tier(&self, d: f32) -> Option<Tier> {
        if d <= self.near {
            Some(Tier::Near)
        } else if d <= self.mid {
            Some(Tier::Mid)
        } else if d <= self.far {
            Some(Tier::Far)
        } else {
            None
        }
    }

    /// The priority weight of `tier`.
    #[must_use]
    pub fn weight(&self, tier: Tier) -> u32 {
        let i = match tier {
            Tier::Near => 0,
            Tier::Mid => 1,
            Tier::Far => 2,
        };
        self.weights.get(i).copied().unwrap_or(1)
    }
}

/// One replicable entity, as jobs see it this tick.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Replicated {
    /// Grid key (sort order).
    pub key: i64,
    /// Stable identity.
    pub repl: ReplicationId,
    /// Position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Facing.
    pub yaw: Angle16,
    /// Appearance.
    pub look: AppearanceId,
}

/// The per-tick replication view of a cell: read-only for jobs.
pub struct RepView {
    /// The tick described.
    pub tick: Tick,
    /// Grid cell size.
    pub cell: f32,
    /// Entities sorted by `key`.
    pub entries: BoundedVec<Replicated>,
    /// Markers emitted this tick.
    pub markers: BoundedVec<TimelineMarker>,
}

#[allow(clippy::cast_possible_truncation)] // clamped into i32 first
fn grid(v: f32, cell: f32) -> i32 {
    let g = (v / cell).floor();
    if g.is_nan() {
        0
    } else {
        g.clamp(-1.0e9, 1.0e9) as i32
    }
}

/// Row-major key, monotonic in both coordinates: `iz` is biased by 2^31 so
/// negative rows sort before positive ones within a column.
fn key(ix: i32, iz: i32) -> i64 {
    (i64::from(ix) << 32) | (i64::from(iz) + (1i64 << 31))
}

impl RepView {
    /// A view for up to `entities` entities and `markers` markers per tick,
    /// with grid cells of size `cell`. The only allocating call.
    #[must_use]
    pub fn with_capacity(entities: usize, markers: usize, cell: f32) -> Self {
        Self {
            tick: Tick::ZERO,
            cell: if cell > 0.0 { cell } else { 1.0 },
            entries: BoundedVec::with_capacity(entities),
            markers: BoundedVec::with_capacity(markers),
        }
    }

    /// Starts a new tick.
    pub fn begin(&mut self, tick: Tick) {
        self.tick = tick;
        self.entries.clear();
        self.markers.clear();
    }

    /// Adds one entity (refused past capacity; returns false).
    pub fn push(
        &mut self,
        repl: ReplicationId,
        position: Vec3,
        velocity: Vec3,
        yaw: Angle16,
        look: AppearanceId,
    ) -> bool {
        let key = key(grid(position.x, self.cell), grid(position.z, self.cell));
        self.entries
            .push(Replicated {
                key,
                repl,
                position,
                velocity,
                yaw,
                look,
            })
            .is_ok()
    }

    /// Sorts for queries; call after the last `push`.
    pub fn finish(&mut self) {
        self.entries.sort_unstable_by_key(|e| (e.key, e.repl));
    }

    /// Calls `f(index, entry, distance)` for every entry within `radius` of
    /// `center` (horizontal distance).
    pub fn for_each_within(&self, center: Vec3, radius: f32, mut f: impl FnMut(usize, &Replicated, f32)) {
        let (x0, x1) = (
            grid(center.x - radius, self.cell),
            grid(center.x + radius, self.cell),
        );
        let (z0, z1) = (
            grid(center.z - radius, self.cell),
            grid(center.z + radius, self.cell),
        );
        for ix in x0..=x1 {
            let lo = self.entries.partition_point(|e| e.key < key(ix, z0));
            let hi = self.entries.partition_point(|e| e.key <= key(ix, z1));
            for i in lo..hi {
                if let Some(e) = self.entries.get(i) {
                    let d = (e.position - center).horizontal().length();
                    if d <= radius {
                        f(i, e, d);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_adapter_contract::core_types::EntityId;

    #[test]
    fn radius_queries_match_brute_force() {
        let mut v = RepView::with_capacity(1000, 0, 10.0);
        v.begin(Tick(1));
        let mut n = 0u32;
        for ix in -15..15i16 {
            for iz in -15..15i16 {
                let p = Vec3::new(f32::from(ix) * 7.3, 0.0, f32::from(iz) * 5.1);
                assert!(v.push(
                    ReplicationId(EntityId::new(n, 0)),
                    p,
                    Vec3::ZERO,
                    Angle16(0),
                    AppearanceId(0)
                ));
                n += 1;
            }
        }
        v.finish();
        for (c, r) in [
            (Vec3::ZERO, 20.0),
            (Vec3::new(-50.0, 0.0, 30.0), 33.0),
            (Vec3::new(500.0, 0.0, 0.0), 10.0),
        ] {
            let mut got = Vec::new();
            v.for_each_within(c, r, |_, e, _| got.push(e.repl));
            got.sort_unstable();
            let mut want: Vec<_> = v
                .entries
                .iter()
                .filter(|e| (e.position - c).horizontal().length() <= r)
                .map(|e| e.repl)
                .collect();
            want.sort_unstable();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn tiers() {
        let t = TierConfig::DEFAULT;
        assert_eq!(t.tier(5.0), Some(Tier::Near));
        assert_eq!(t.tier(30.0), Some(Tier::Mid));
        assert_eq!(t.tier(90.0), Some(Tier::Far));
        assert_eq!(t.tier(101.0), None);
        assert_eq!(t.weight(Tier::Near), 9);
    }
}
