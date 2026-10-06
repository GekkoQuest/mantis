//! World streaming (plan 8.3): content-addressed sectors loaded by priority around the
//! camera and its predicted movement, at a level of detail chosen by distance.
//!
//! [`SectorStreamer::update`] runs once per frame on the render thread with no allocation.
//! It decides, for every sector in the world index:
//! - **want**: within the load radius of the camera *or* of the predicted position
//!   (camera plus velocity times the look-ahead), so sectors ahead load before arrival;
//! - **priority**: nearer is higher, sectors ahead of the movement and view get a bonus,
//!   plus the sector's own streaming bias (from its `STRM` chunk);
//! - **level of detail**: by distance against the LOD thresholds scaled by the sector's
//!   `lod_distance_scale`;
//! - **keep or evict**: a loaded sector stays until it is beyond the unload radius
//!   (larger than the load radius, so a camera at the edge does not thrash), or until the
//!   resident budget forces out the lowest-priority one.
//!
//! Loading itself happens elsewhere: requests go to a [`SectorLoader`] (the client wires
//! it to its streaming pool, whose budgeted hand-off keeps render-thread uploads under
//! the per-frame budget row), and completions come back through
//! [`SectorStreamer::complete`].

use glam::Vec3;
use mantis_core::content::ContentHash;

/// Most levels of detail a sector can be streamed at.
pub const MAX_LODS: usize = 4;
const MAX_LODS_U8: u8 = 4;

/// One sector in the world index.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SectorEntry {
    /// Grid coordinates.
    pub coord: (i32, i32),
    /// Content hash of the sector container (client copy) at each LOD, finest first.
    pub lods: [ContentHash; MAX_LODS],
    /// LODs available (1 to 4).
    pub lod_count: u8,
    /// Center of the sector's bounds.
    pub center: Vec3,
    /// Streaming priority bias (`STRM`).
    pub priority_bias: f32,
    /// LOD distance multiplier (`STRM`).
    pub lod_distance_scale: f32,
}

/// Streaming tuning.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StreamingConfig {
    /// Sectors within this distance (of the camera or the predicted position) load.
    pub load_radius: f32,
    /// Loaded sectors beyond this distance unload (greater than `load_radius`).
    pub unload_radius: f32,
    /// Seconds of movement to predict ahead.
    pub lookahead: f32,
    /// Distance where each LOD stops being used (finest first), before scaling.
    pub lod_distances: [f32; MAX_LODS],
    /// Most sectors resident or loading at once.
    pub resident_budget: u32,
    /// Most requests in flight at once.
    pub max_in_flight: u32,
    /// Priority bonus at full alignment with the movement or view direction.
    pub direction_bonus: f32,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            load_radius: 300.0,
            unload_radius: 360.0,
            lookahead: 3.0,
            lod_distances: [80.0, 160.0, 320.0, f32::MAX],
            resident_budget: 64,
            max_in_flight: 8,
            direction_bonus: 0.25,
        }
    }
}

/// A request handle issued by the loader.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestId(pub u64);

/// Where sector loads go.
pub trait SectorLoader {
    /// Starts loading `hash`; higher `priority` loads first.
    fn request(&mut self, hash: ContentHash, priority: f32) -> RequestId;
    /// Changes a pending request's priority.
    fn reprioritize(&mut self, id: RequestId, priority: f32);
    /// Abandons a pending request.
    fn cancel(&mut self, id: RequestId);
    /// Releases a loaded sector's resources.
    fn unload(&mut self, hash: ContentHash);
}

/// A sector's streaming state.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum SectorState {
    /// Not loaded.
    #[default]
    Unloaded,
    /// Requested at a LOD.
    Loading {
        /// The request.
        id: RequestId,
        /// LOD requested.
        lod: u8,
    },
    /// Resident at a LOD (and possibly loading a different one).
    Loaded {
        /// LOD resident.
        lod: u8,
        /// A pending change of LOD.
        pending: Option<(RequestId, u8)>,
    },
}

/// One frame's streaming activity.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StreamStats {
    /// Requests issued this frame.
    pub requested: u32,
    /// Requests cancelled this frame.
    pub cancelled: u32,
    /// Sectors unloaded this frame.
    pub unloaded: u32,
    /// Sectors resident.
    pub resident: u32,
    /// Requests in flight.
    pub in_flight: u32,
}

/// The sector streamer.
#[derive(Debug)]
pub struct SectorStreamer {
    config: StreamingConfig,
    sectors: Vec<SectorEntry>,
    states: Vec<SectorState>,
    priorities: Vec<f32>,
    order: Vec<u32>,
}

impl SectorStreamer {
    /// A streamer over a world index.
    pub fn new(config: StreamingConfig, sectors: Vec<SectorEntry>) -> Self {
        let n = sectors.len();
        Self {
            config,
            sectors,
            states: vec![SectorState::Unloaded; n],
            priorities: vec![0.0; n],
            order: Vec::with_capacity(n),
        }
    }

    /// The state of sector `i`.
    pub fn state(&self, i: usize) -> Option<SectorState> {
        self.states.get(i).copied()
    }

    /// Index of the sector at grid `coord`.
    pub fn find(&self, coord: (i32, i32)) -> Option<usize> {
        self.sectors.iter().position(|s| s.coord == coord)
    }

    fn lod_for(&self, s: &SectorEntry, distance: f32) -> u8 {
        let scale = if s.lod_distance_scale > 0.0 {
            s.lod_distance_scale
        } else {
            1.0
        };
        let max = s.lod_count.clamp(1, MAX_LODS_U8) - 1;
        let lod = self
            .config
            .lod_distances
            .iter()
            .position(|d| distance <= *d * scale)
            .unwrap_or(MAX_LODS - 1);
        u8::try_from(lod).unwrap_or(max).min(max)
    }

    /// A loader reports that a request finished. Unknown or stale ids are ignored
    /// (`false`). On failure the sector returns to unloaded and is retried by priority.
    pub fn complete(&mut self, id: RequestId, ok: bool) -> bool {
        for state in &mut self.states {
            match *state {
                SectorState::Loading { id: pending, lod } if pending == id => {
                    *state = if ok {
                        SectorState::Loaded { lod, pending: None }
                    } else {
                        SectorState::Unloaded
                    };
                    return true;
                }
                SectorState::Loaded {
                    lod,
                    pending: Some((pending, new_lod)),
                } if pending == id => {
                    *state = SectorState::Loaded {
                        lod: if ok { new_lod } else { lod },
                        pending: None,
                    };
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Plans this frame's loads and unloads. Allocation-free.
    #[allow(clippy::too_many_lines)] // One pass over the index: score, then act in priority order.
    pub fn update(
        &mut self,
        camera: Vec3,
        velocity: Vec3,
        view: Vec3,
        loader: &mut impl SectorLoader,
    ) -> StreamStats {
        let cfg = self.config;
        let mut report = StreamStats::default();
        let predicted = camera + velocity * cfg.lookahead;
        let move_dir = velocity.try_normalize().unwrap_or(Vec3::ZERO);
        let view_dir = view.try_normalize().unwrap_or(Vec3::ZERO);
        // Score every sector.
        self.order.clear();
        for (i, sector) in self.sectors.iter().enumerate() {
            let flat = |p: Vec3| Vec3::new(p.x, 0.0, p.z);
            let d_now = (flat(sector.center) - flat(camera)).length();
            let d_ahead = (flat(sector.center) - flat(predicted)).length();
            let dist = d_now.min(d_ahead);
            let to = (flat(sector.center) - flat(camera))
                .try_normalize()
                .unwrap_or(Vec3::ZERO);
            let align = to.dot(move_dir).max(to.dot(view_dir)).max(0.0);
            let priority = 1.0 / (1.0 + dist) * (1.0 + cfg.direction_bonus * align)
                + sector.priority_bias.max(-1.0) * 0.01;
            if let Some(p) = self.priorities.get_mut(i) {
                *p = priority;
            }
            if let Ok(index) = u32::try_from(i) {
                self.order.push(index);
            }
            // Unload anything beyond the unload radius; cancel loads no longer wanted.
            let Some(state) = self.states.get_mut(i) else {
                continue;
            };
            let wanted = dist <= cfg.load_radius;
            let keep = dist <= cfg.unload_radius;
            match *state {
                SectorState::Loading { id, .. } if !wanted => {
                    loader.cancel(id);
                    report.cancelled += 1;
                    *state = SectorState::Unloaded;
                }
                SectorState::Loaded { lod, pending } if !keep => {
                    if let Some((id, _)) = pending {
                        loader.cancel(id);
                        report.cancelled += 1;
                    }
                    if let Some(h) = sector.lods.get(usize::from(lod)) {
                        loader.unload(*h);
                    }
                    report.unloaded += 1;
                    *state = SectorState::Unloaded;
                }
                _ => {}
            }
        }
        let priorities = &self.priorities;
        self.order.sort_unstable_by(|a, b| {
            let pa = priorities.get(*a as usize).copied().unwrap_or(0.0);
            let pb = priorities.get(*b as usize).copied().unwrap_or(0.0);
            pb.total_cmp(&pa).then(a.cmp(b))
        });
        // Count residents and requests in flight.
        let mut resident = 0u32;
        let mut in_flight = 0u32;
        for st in &self.states {
            match st {
                SectorState::Unloaded => {}
                SectorState::Loading { .. } => {
                    resident += 1;
                    in_flight += 1;
                }
                SectorState::Loaded { pending, .. } => {
                    resident += 1;
                    in_flight += u32::from(pending.is_some());
                }
            }
        }
        // Over budget: evict the lowest-priority residents first.
        for &i in self.order.iter().rev() {
            if resident <= cfg.resident_budget {
                break;
            }
            let (Some(state), Some(sector)) = (self.states.get_mut(i as usize), self.sectors.get(i as usize))
            else {
                continue;
            };
            match *state {
                SectorState::Loading { id, .. } => {
                    loader.cancel(id);
                    report.cancelled += 1;
                    in_flight = in_flight.saturating_sub(1);
                }
                SectorState::Loaded { lod, pending } => {
                    if let Some((id, _)) = pending {
                        loader.cancel(id);
                        report.cancelled += 1;
                        in_flight = in_flight.saturating_sub(1);
                    }
                    if let Some(h) = sector.lods.get(usize::from(lod)) {
                        loader.unload(*h);
                    }
                    report.unloaded += 1;
                }
                SectorState::Unloaded => continue,
            }
            *state = SectorState::Unloaded;
            resident -= 1;
        }
        // In priority order: request wanted sectors (evicting a lower-priority resident
        // when the budget is full), adjust LODs, refresh pending priorities.
        for k in 0..self.order.len() {
            let Some(i) = self.order.get(k).map(|i| *i as usize) else {
                continue;
            };
            let (Some(sector), Some(state)) = (self.sectors.get(i), self.states.get(i).copied()) else {
                continue;
            };
            let flat = |p: Vec3| Vec3::new(p.x, 0.0, p.z);
            let dist = (flat(sector.center) - flat(camera))
                .length()
                .min((flat(sector.center) - flat(predicted)).length());
            let priority = self.priorities.get(i).copied().unwrap_or(0.0);
            let lod = self.lod_for(sector, dist);
            let hash = sector
                .lods
                .get(usize::from(lod))
                .copied()
                .unwrap_or(ContentHash::ZERO);
            let next = match state {
                SectorState::Unloaded if dist <= cfg.load_radius => {
                    if in_flight >= cfg.max_in_flight {
                        continue;
                    }
                    if resident >= cfg.resident_budget {
                        // Lower priorities come later in the order; take the lowest resident.
                        let victim = (k + 1..self.order.len())
                            .rev()
                            .filter_map(|v| self.order.get(v).map(|x| *x as usize))
                            .find(|v| !matches!(self.states.get(*v), Some(SectorState::Unloaded) | None));
                        let Some(v) = victim else { continue };
                        if let (Some(vs), Some(ve)) = (self.states.get(v).copied(), self.sectors.get(v)) {
                            match vs {
                                SectorState::Loading { id, .. } => {
                                    loader.cancel(id);
                                    report.cancelled += 1;
                                    in_flight = in_flight.saturating_sub(1);
                                }
                                SectorState::Loaded { lod: vl, pending } => {
                                    if let Some((id, _)) = pending {
                                        loader.cancel(id);
                                        report.cancelled += 1;
                                        in_flight = in_flight.saturating_sub(1);
                                    }
                                    if let Some(h) = ve.lods.get(usize::from(vl)) {
                                        loader.unload(*h);
                                    }
                                    report.unloaded += 1;
                                }
                                SectorState::Unloaded => {}
                            }
                        }
                        if let Some(slot) = self.states.get_mut(v) {
                            *slot = SectorState::Unloaded;
                        }
                        resident = resident.saturating_sub(1);
                        if in_flight >= cfg.max_in_flight {
                            continue;
                        }
                    }
                    in_flight += 1;
                    resident += 1;
                    report.requested += 1;
                    SectorState::Loading {
                        id: loader.request(hash, priority),
                        lod,
                    }
                }
                SectorState::Loading { id, .. }
                | SectorState::Loaded {
                    pending: Some((id, _)),
                    ..
                } => {
                    loader.reprioritize(id, priority);
                    continue;
                }
                SectorState::Loaded {
                    lod: have,
                    pending: None,
                } if have != lod => {
                    if in_flight >= cfg.max_in_flight {
                        continue;
                    }
                    in_flight += 1;
                    report.requested += 1;
                    SectorState::Loaded {
                        lod: have,
                        pending: Some((loader.request(hash, priority), lod)),
                    }
                }
                _ => continue,
            };
            if let Some(slot) = self.states.get_mut(i) {
                *slot = next;
            }
        }
        report.resident = resident;
        report.in_flight = in_flight;
        report
    }
}

#[cfg(test)]
mod tests;
