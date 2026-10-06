//! Inverse kinematics: analytic two-bone IK (feet) and a clamped look-at (head).
//!
//! Both work on a local pose plus its current model-space transforms, write corrected
//! local rotations, and leave refreshing the model-space subtree to the caller
//! ([`crate::skeleton::Skeleton::model_space_from`]). Targets are in model space (the
//! character's frame).

use glam::{Quat, Vec3};

use crate::graph::{LookAt, TwoBoneChain};
use crate::skeleton::Skeleton;
use crate::transform::{Transform, slerp_shortest};

const EPSILON: f32 = 1e-6;

fn parent_rotation(skeleton: &Skeleton, model: &[Transform], bone: usize) -> Quat {
    skeleton
        .parent(bone)
        .and_then(|p| model.get(p))
        .map_or(Quat::IDENTITY, |m| m.rotation)
}

fn blend_local(local: &mut [Transform], bone: usize, rotation: Quat, weight: f32) {
    if let Some(l) = local.get_mut(bone) {
        l.rotation = if weight >= 1.0 {
            rotation.normalize()
        } else {
            slerp_shortest(l.rotation, rotation.normalize(), weight)
        };
    }
}

/// The unit direction of `v` with its component along `axis` (unit) removed.
fn perpendicular(v: Vec3, axis: Vec3) -> Vec3 {
    (v - axis * v.dot(axis)).normalize_or_zero()
}

/// Two-bone IK by the law of cosines. Moves `chain.root` and `chain.mid` so `chain.tip`
/// reaches `target` exactly when it is within reach, and points the fully extended
/// chain at it otherwise (fully folded when it is closer than the bones allow). The
/// middle joint bends toward the pole direction; the tip keeps its model-space rotation
/// (a foot stays level). `weight` in `[0, 1]` blends from the animated pose.
///
/// Returns false, changing nothing, when the chain is degenerate (a zero-length bone)
/// or the bones are missing. On true, the caller refreshes model space from `chain.root`.
pub fn solve_two_bone(
    skeleton: &Skeleton,
    local: &mut [Transform],
    model: &[Transform],
    chain: &TwoBoneChain,
    target: Vec3,
    weight: f32,
) -> bool {
    let weight = if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (Some(root), Some(mid), Some(tip)) =
        (model.get(chain.root), model.get(chain.mid), model.get(chain.tip))
    else {
        return false;
    };
    if weight <= 0.0 || !target.is_finite() {
        return false;
    }
    let (a, b, c) = (root.translation, mid.translation, tip.translation);
    let upper = (b - a).length();
    let lower = (c - b).length();
    if upper < EPSILON || lower < EPSILON {
        return false;
    }
    let to_target = target - a;
    let dir = to_target
        .try_normalize()
        .or_else(|| (c - a).try_normalize())
        .unwrap_or(Vec3::NEG_Y);
    let reach = to_target
        .length()
        .clamp((upper - lower).abs(), upper + lower)
        .max(EPSILON);
    // Bend direction: the pole, else the current bend, else any perpendicular.
    let mut bend = perpendicular(chain.pole, dir);
    if bend == Vec3::ZERO {
        bend = perpendicular(b - a, dir);
    }
    if bend == Vec3::ZERO {
        bend = dir.any_orthonormal_vector();
    }
    let cos_root = ((upper * upper + reach * reach - lower * lower) / (2.0 * upper * reach)).clamp(-1.0, 1.0);
    let sin_root = (1.0 - cos_root * cos_root).max(0.0).sqrt();
    let mid_goal = a + dir * (upper * cos_root) + bend * (upper * sin_root);
    let tip_goal = a + dir * reach;

    let root_turn = Quat::from_rotation_arc((b - a) / upper, (mid_goal - a) / upper);
    let tip_after_root = a + root_turn * (c - a);
    let mid_turn = match (
        (tip_after_root - mid_goal).try_normalize(),
        (tip_goal - mid_goal).try_normalize(),
    ) {
        (Some(from), Some(to)) => Quat::from_rotation_arc(from, to),
        _ => Quat::IDENTITY,
    };
    let root_model = root_turn * root.rotation;
    let mid_model = mid_turn * root_turn * mid.rotation;
    let root_parent = parent_rotation(skeleton, model, chain.root);
    let mid_parent = root_turn * parent_rotation(skeleton, model, chain.mid);
    let tip_parent = mid_turn * root_turn * parent_rotation(skeleton, model, chain.tip);
    blend_local(local, chain.root, root_parent.inverse() * root_model, weight);
    blend_local(local, chain.mid, mid_parent.inverse() * mid_model, weight);
    blend_local(local, chain.tip, tip_parent.inverse() * tip.rotation, weight);
    true
}

/// Turns `look.head` so its forward axis points at `target`, by at most
/// `look.max_angle` from the animated direction. `weight` in `[0, 1]` scales the turn.
/// Returns false, changing nothing, when there is nothing to do; on true the caller
/// refreshes model space from `look.head`.
pub fn solve_look_at(
    skeleton: &Skeleton,
    local: &mut [Transform],
    model: &[Transform],
    look: &LookAt,
    target: Vec3,
    weight: f32,
) -> bool {
    let weight = if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let Some(head) = model.get(look.head) else {
        return false;
    };
    let forward = (head.rotation * look.axis).normalize_or_zero();
    let (Some(desired), true) = ((target - head.translation).try_normalize(), weight > 0.0) else {
        return false;
    };
    let angle = forward.angle_between(desired);
    if !angle.is_finite() || angle < EPSILON {
        return false;
    }
    let axis = forward
        .cross(desired)
        .try_normalize()
        .unwrap_or_else(|| forward.any_orthonormal_vector());
    let turn = Quat::from_axis_angle(axis, angle.min(look.max_angle) * weight);
    let parent = parent_rotation(skeleton, model, look.head);
    if let Some(l) = local.get_mut(look.head) {
        l.rotation = (parent.inverse() * turn * head.rotation).normalize();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::tests::{TestResult, leg};

    fn chain(pole: Vec3) -> TwoBoneChain {
        TwoBoneChain {
            root: 1,
            mid: 2,
            tip: 3,
            pole,
        }
    }

    /// Solves on the leg's bind pose and returns the resulting model transforms.
    fn solve(
        target: Vec3,
        pole: Vec3,
        weight: f32,
    ) -> Result<(Vec<Transform>, Vec<Transform>), Box<dyn std::error::Error>> {
        let s = leg()?;
        let mut local = s.bind_pose().to_vec();
        let mut model = vec![Transform::IDENTITY; 4];
        s.model_space(&local, &mut model);
        let before = model.clone();
        assert!(solve_two_bone(
            &s,
            &mut local,
            &model,
            &chain(pole),
            target,
            weight
        ));
        s.model_space_from(1, &local, &mut model);
        Ok((before, model))
    }

    fn pos(model: &[Transform], bone: usize) -> Vec3 {
        model.get(bone).map_or(Vec3::NAN, |m| m.translation)
    }

    #[test]
    fn reaches_targets_within_reach_exactly() -> TestResult {
        for target in [
            Vec3::new(0.0, 0.5, 0.5),
            Vec3::new(0.3, 1.2, -0.4),
            Vec3::new(1.2, 2.5, 0.8),
            Vec3::new(0.0, 0.05, 0.0),
        ] {
            let (_, model) = solve(target, Vec3::Z, 1.0)?;
            let tip = pos(&model, 3);
            assert!(tip.distance(target) < 1e-4, "{tip:?} vs {target:?}");
            // Bone lengths are preserved.
            assert!((pos(&model, 2).distance(pos(&model, 1)) - 1.0).abs() < 1e-4);
            assert!((pos(&model, 3).distance(pos(&model, 2)) - 1.0).abs() < 1e-4);
        }
        Ok(())
    }

    #[test]
    fn out_of_reach_extends_fully_toward_the_target() -> TestResult {
        let target = Vec3::new(3.0, 2.0, 0.0);
        let (_, model) = solve(target, Vec3::Z, 1.0)?;
        let hip = pos(&model, 1);
        let tip = pos(&model, 3);
        assert!(tip.distance(hip + Vec3::X * 2.0) < 1e-4, "{tip:?}");
        assert!(pos(&model, 2).distance(hip + Vec3::X) < 1e-3, "straight");
        Ok(())
    }

    #[test]
    fn knee_bends_toward_the_pole() -> TestResult {
        let target = Vec3::new(0.0, 0.8, 0.0);
        for pole in [Vec3::Z, Vec3::NEG_Z, Vec3::X] {
            let (_, model) = solve(target, pole, 1.0)?;
            let knee = pos(&model, 2);
            let bend = knee - Vec3::new(0.0, f32::midpoint(2.0, 0.8), 0.0);
            assert!(bend.normalize().dot(pole) > 0.999, "{pole:?}: knee {knee:?}");
            assert!(pos(&model, 3).distance(target) < 1e-4);
        }
        Ok(())
    }

    #[test]
    fn tip_keeps_its_model_rotation_and_weight_blends() -> TestResult {
        let target = Vec3::new(0.4, 0.7, 0.3);
        let (before, model) = solve(target, Vec3::Z, 1.0)?;
        let r0 = before.get(3).map(|m| m.rotation).ok_or("tip")?;
        let r1 = model.get(3).map(|m| m.rotation).ok_or("tip")?;
        assert!(r0.angle_between(r1) < 1e-4);
        let (before, none) = solve(target, Vec3::Z, 1e-9)?;
        assert!(
            pos(&none, 3).distance(pos(&before, 3)) < 1e-4,
            "weight ~0 changes nothing visible"
        );
        let s = leg()?;
        let mut local = s.bind_pose().to_vec();
        let model = vec![Transform::IDENTITY; 4];
        assert!(
            !solve_two_bone(&s, &mut local, &model, &chain(Vec3::Z), target, 1.0),
            "degenerate"
        );
        assert!(!solve_two_bone(
            &s,
            &mut local,
            &model,
            &chain(Vec3::Z),
            target,
            0.0
        ));
        Ok(())
    }

    fn look(max_angle: f32) -> LookAt {
        LookAt {
            head: 1,
            axis: Vec3::Z,
            max_angle,
        }
    }

    fn look_solve(target: Vec3, max_angle: f32) -> Result<Vec3, Box<dyn std::error::Error>> {
        let s = leg()?;
        let mut local = s.bind_pose().to_vec();
        let mut model = vec![Transform::IDENTITY; 4];
        s.model_space(&local, &mut model);
        assert!(solve_look_at(
            &s,
            &mut local,
            &model,
            &look(max_angle),
            target,
            1.0
        ));
        s.model_space_from(1, &local, &mut model);
        Ok(model.get(1).map(|m| m.rotation * Vec3::Z).ok_or("head")?)
    }

    #[test]
    fn look_at_points_within_the_limit() -> TestResult {
        let head = Vec3::new(0.0, 2.0, 0.0);
        let target = head + Vec3::new(1.0, 0.5, 1.0);
        let fwd = look_solve(target, 1.5)?;
        assert!(fwd.distance((target - head).normalize()) < 1e-4, "{fwd:?}");
        // Directly behind and beyond the limit: turns exactly the maximum angle.
        let behind = look_solve(head + Vec3::new(0.1, 0.0, -1.0), 0.5)?;
        assert!((behind.angle_between(Vec3::Z) - 0.5).abs() < 1e-4, "{behind:?}");
        Ok(())
    }
}
