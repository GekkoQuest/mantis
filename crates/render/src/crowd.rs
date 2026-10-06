//! Crowd tiers (plan 8.3): every frame each visible character gets a tier by distance and
//! budget: full skeletal **near**, GPU-skinned instanced **mid** (vertex animation
//! textures), **far** (a lower-detail mesh playing the same baked vertex animation at a
//! reduced frame rate), or **culled**.
//!
//! Selection orders visible agents by effective distance (distance divided by a priority
//! factor, so party members and targets win ties for detail), then fills the near budget,
//! the mid budget, and the far budget in that order; an agent that misses a tier's budget
//! or range falls to the next. Hysteresis keeps an agent near a boundary from flipping
//! tier every frame: an agent stays in its previous tier while within that tier's range
//! stretched by the hysteresis margin. Allocation-free after construction.

use glam::Vec3;

use crate::math::{Frustum, Sphere};

/// A crowd tier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, Default)]
pub enum CrowdTier {
    /// Full skeletal animation and mesh.
    Near,
    /// Instanced mesh playing a baked vertex animation.
    Mid,
    /// A lower-detail mesh playing the same baked vertex animation at a reduced frame
    /// rate (plan 8.4).
    Far,
    /// Not drawn (outside the view, beyond range, or over every budget).
    #[default]
    Culled,
}

/// Tier ranges and budgets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CrowdConfig {
    /// Farthest effective distance for the near tier.
    pub near_distance: f32,
    /// Farthest effective distance for the mid tier.
    pub mid_distance: f32,
    /// Farthest effective distance drawn at all.
    pub far_distance: f32,
    /// Most near agents.
    pub near_budget: u32,
    /// Most mid agents.
    pub mid_budget: u32,
    /// Most far agents.
    pub far_budget: u32,
    /// Relative range extension that keeps an agent in its previous tier (0.1 = 10%).
    pub hysteresis: f32,
}

impl Default for CrowdConfig {
    /// The `town-300` reference budgets: 50 near, 100 mid, 150 far.
    fn default() -> Self {
        Self {
            near_distance: 25.0,
            mid_distance: 80.0,
            far_distance: 250.0,
            near_budget: 50,
            mid_budget: 100,
            far_budget: 150,
            hysteresis: 0.1,
        }
    }
}

/// One character as the selector sees it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CrowdAgent {
    /// World position.
    pub position: Vec3,
    /// Bounding radius.
    pub radius: f32,
    /// Detail priority, at least 1 by convention: effective distance is distance divided
    /// by it.
    pub priority: f32,
}

/// Tier counts of one selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CrowdStats {
    /// Near agents.
    pub near: u32,
    /// Mid agents.
    pub mid: u32,
    /// Far agents.
    pub far: u32,
    /// Culled agents.
    pub culled: u32,
    /// Agents that missed a tier they qualified for by range because its budget was full.
    pub demoted: u32,
}

/// The per-frame tier selector.
#[derive(Clone, Debug)]
pub struct CrowdSelector {
    config: CrowdConfig,
    order: Vec<(f32, u32)>,
    previous: Vec<CrowdTier>,
}

impl CrowdSelector {
    /// A selector for up to `capacity` agents.
    pub fn new(config: CrowdConfig, capacity: usize) -> Self {
        Self {
            config,
            order: Vec::with_capacity(capacity),
            previous: vec![CrowdTier::Culled; capacity],
        }
    }

    /// The configuration.
    pub fn config(&self) -> &CrowdConfig {
        &self.config
    }

    /// Assigns a tier to every agent in `agents` (index-aligned with `out`). Agents past
    /// the selector's capacity, or past `out`'s length, are culled.
    pub fn select(
        &mut self,
        camera: Vec3,
        frustum: &Frustum,
        agents: &[CrowdAgent],
        out: &mut [CrowdTier],
    ) -> CrowdStats {
        let c = self.config;
        let mut stats = CrowdStats::default();
        out.fill(CrowdTier::Culled);
        self.order.clear();
        let capacity = self.order.capacity().min(self.previous.len());
        for (i, a) in agents.iter().enumerate().take(capacity) {
            let visible = a.position.is_finite()
                && frustum.intersects_sphere(&Sphere {
                    center: a.position,
                    radius: a.radius.max(0.0),
                });
            let priority = if a.priority.is_finite() && a.priority > 0.0 {
                a.priority
            } else {
                1.0
            };
            if visible && let Ok(index) = u32::try_from(i) {
                self.order
                    .push(((a.position - camera).length() / priority, index));
            }
        }
        // Nearest first; equal distances keep agent order (deterministic).
        self.order
            .sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let stretch = 1.0 + c.hysteresis.max(0.0);
        for &(d, index) in &self.order {
            let previous = self
                .previous
                .get(index as usize)
                .copied()
                .unwrap_or(CrowdTier::Culled);
            let range = |tier: CrowdTier, base: f32| if previous == tier { base * stretch } else { base };
            let mut tier = CrowdTier::Culled;
            let mut qualified_better = false;
            if d <= range(CrowdTier::Near, c.near_distance) {
                if stats.near < c.near_budget {
                    tier = CrowdTier::Near;
                } else {
                    qualified_better = true;
                }
            }
            if tier == CrowdTier::Culled && d <= range(CrowdTier::Mid, c.mid_distance) {
                if stats.mid < c.mid_budget {
                    tier = CrowdTier::Mid;
                } else {
                    qualified_better = true;
                }
            }
            if tier == CrowdTier::Culled && d <= range(CrowdTier::Far, c.far_distance) {
                if stats.far < c.far_budget {
                    tier = CrowdTier::Far;
                } else {
                    qualified_better = true;
                }
            }
            match tier {
                CrowdTier::Near => stats.near += 1,
                CrowdTier::Mid => stats.mid += 1,
                CrowdTier::Far => stats.far += 1,
                CrowdTier::Culled => stats.culled += 1,
            }
            if qualified_better {
                stats.demoted += 1;
            }
            if let Some(slot) = out.get_mut(index as usize) {
                *slot = tier;
            }
        }
        // Invisible agents.
        let visible = u32::try_from(self.order.len()).unwrap_or(u32::MAX);
        stats.culled += u32::try_from(agents.len())
            .unwrap_or(u32::MAX)
            .saturating_sub(visible);
        for (prev, now) in self.previous.iter_mut().zip(out.iter()) {
            *prev = *now;
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Camera;

    fn camera() -> (Vec3, Frustum) {
        let cam = Camera {
            position: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.6,
            aspect: 1.0,
            near: 0.1,
        };
        (
            cam.position,
            Frustum::from_view_projection(&cam.view_projection()),
        )
    }

    fn agent(z: f32) -> CrowdAgent {
        CrowdAgent {
            position: Vec3::new(0.0, 0.0, z),
            radius: 1.0,
            priority: 1.0,
        }
    }

    #[test]
    fn reference_town_budget_fills_each_tier_exactly() {
        let (eye, frustum) = camera();
        // 300 agents spread from 1 to 240 units ahead.
        let agents: Vec<CrowdAgent> = (0..300u16).map(|i| agent(1.0 + f32::from(i) * 0.8)).collect();
        let mut s = CrowdSelector::new(
            CrowdConfig {
                near_distance: 1000.0,
                mid_distance: 1000.0,
                far_distance: 1000.0,
                ..CrowdConfig::default()
            },
            300,
        );
        let mut out = vec![CrowdTier::Culled; 300];
        let stats = s.select(eye, &frustum, &agents, &mut out);
        assert_eq!(
            (stats.near, stats.mid, stats.far, stats.culled),
            (50, 100, 150, 0)
        );
        // The nearest 50 are near, the next 100 mid, the rest far.
        assert!(out.iter().take(50).all(|t| *t == CrowdTier::Near));
        assert!(out.iter().skip(50).take(100).all(|t| *t == CrowdTier::Mid));
        assert!(out.iter().skip(150).all(|t| *t == CrowdTier::Far));
    }

    #[test]
    fn ranges_choose_tiers_and_budgets_demote_the_farthest() {
        let (eye, frustum) = camera();
        let config = CrowdConfig {
            near_budget: 2,
            mid_budget: 1,
            far_budget: 1,
            hysteresis: 0.0,
            ..CrowdConfig::default()
        };
        let agents = [
            agent(10.0),
            agent(5.0),
            agent(20.0),
            agent(50.0),
            agent(200.0),
            agent(300.0),
        ];
        let mut s = CrowdSelector::new(config, 8);
        let mut out = [CrowdTier::Culled; 6];
        let stats = s.select(eye, &frustum, &agents, &mut out);
        // Three agents qualify for near; the farthest of them (20) is demoted to mid,
        // which pushes the mid-range agent (50) to far, which pushes 200 out of budget.
        assert_eq!(
            out,
            [
                CrowdTier::Near,
                CrowdTier::Near,
                CrowdTier::Mid,
                CrowdTier::Far,
                CrowdTier::Culled,
                CrowdTier::Culled
            ]
        );
        assert_eq!(stats.demoted, 3);
        assert_eq!(stats.culled, 2);
    }

    #[test]
    fn hysteresis_holds_a_tier_at_the_boundary() {
        let (eye, frustum) = camera();
        let mut s = CrowdSelector::new(CrowdConfig::default(), 4);
        let mut out = [CrowdTier::Culled; 1];
        let _ = s.select(eye, &frustum, &[agent(24.0)], &mut out);
        assert_eq!(out, [CrowdTier::Near]);
        // Just past the boundary but within 10%: stays near.
        let _ = s.select(eye, &frustum, &[agent(26.0)], &mut out);
        assert_eq!(out, [CrowdTier::Near]);
        let _ = s.select(eye, &frustum, &[agent(28.0)], &mut out);
        assert_eq!(out, [CrowdTier::Mid], "past the stretched range");
        // Coming back inside the stretched range does not promote: hysteresis only holds.
        let _ = s.select(eye, &frustum, &[agent(26.0)], &mut out);
        assert_eq!(out, [CrowdTier::Mid]);
        let _ = s.select(eye, &frustum, &[agent(24.0)], &mut out);
        assert_eq!(out, [CrowdTier::Near]);
    }

    #[test]
    fn priority_promotes_and_invisible_agents_are_culled() {
        let (eye, frustum) = camera();
        let config = CrowdConfig {
            near_budget: 1,
            hysteresis: 0.0,
            ..CrowdConfig::default()
        };
        let mut s = CrowdSelector::new(config, 4);
        let mut out = [CrowdTier::Culled; 3];
        let important = CrowdAgent {
            priority: 2.0,
            ..agent(20.0)
        };
        let behind = CrowdAgent {
            position: Vec3::new(0.0, 0.0, -10.0),
            radius: 1.0,
            priority: 1.0,
        };
        let stats = s.select(eye, &frustum, &[agent(15.0), important, behind], &mut out);
        assert_eq!(
            out,
            [CrowdTier::Mid, CrowdTier::Near, CrowdTier::Culled],
            "priority 2 at 20 beats 15"
        );
        assert_eq!(stats.culled, 1);
    }
}
