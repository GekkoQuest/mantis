//! The `town-300` layout: 300 avatars inside the toy world and in view, and the crowd
//! selector's reference budgets placing exactly 50 near, 100 mid, and 150 far, frame
//! after frame while they walk.

use mantis_render::crowd::{CrowdAgent, CrowdConfig, CrowdSelector, CrowdTier};
use mantis_render::math::{Camera, Frustum};
use toy_client::town::{AVATARS, BRAZIERS, town_300};

fn camera(aspect: f32) -> Camera {
    let l = town_300();
    Camera {
        position: l.eye,
        yaw: l.yaw,
        pitch: l.pitch,
        fov_y: l.fov_y,
        aspect,
        near: 0.1,
    }
}

#[test]
fn the_layout_fits_the_toy_world() {
    let l = town_300();
    assert_eq!(l.avatars.len(), AVATARS);
    assert_eq!(l.braziers.len(), BRAZIERS);
    // The toy world spans -32 to 32 m on both axes; a walking avatar stays within 0.6 m
    // of its spot.
    for p in l
        .avatars
        .iter()
        .map(|a| a.center)
        .chain(l.braziers.iter().copied())
    {
        assert!(
            p.x.abs() < 31.0 && p.z.abs() < 31.0,
            "{p} is outside the toy world"
        );
    }
    let kinds = (0..3).map(|k| l.avatars.iter().filter(|a| a.kind == k).count());
    assert!(kinds.into_iter().all(|n| n == AVATARS / 3), "three equal kinds");
}

#[test]
fn the_reference_budgets_place_exactly_50_100_150_while_the_crowd_walks() {
    let l = town_300();
    for aspect in [16.0 / 9.0, 4.0 / 3.0] {
        let cam = camera(aspect);
        let frustum = Frustum::from_view_projection(&cam.view_projection());
        let mut selector = CrowdSelector::new(CrowdConfig::default(), AVATARS);
        let mut tiers = vec![CrowdTier::Culled; AVATARS];
        for frame in 0..120u16 {
            let time = f32::from(frame) / 60.0;
            let agents: Vec<CrowdAgent> = l
                .avatars
                .iter()
                .map(|a| CrowdAgent {
                    position: a.model(time).transform_point3(glam::Vec3::new(0.0, 0.93, 0.0)),
                    radius: 1.1,
                    priority: 1.0,
                })
                .collect();
            let s = selector.select(cam.position, &frustum, &agents, &mut tiers);
            assert_eq!(
                (s.near, s.mid, s.far, s.culled),
                (50, 100, 150, 0),
                "frame {frame}, aspect {aspect}"
            );
        }
    }
}
