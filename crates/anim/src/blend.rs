//! Pose blending: weighted N-way blends, two-way crossfades, masked override layers, and
//! additive layers. Nothing here allocates.

use glam::{Quat, Vec3};

use crate::transform::{Transform, align, normalize_or_identity};

fn usable(w: f32) -> f32 {
    if w.is_finite() && w > 0.0 { w } else { 0.0 }
}

/// Weighted blend of any number of poses into `out`. Weights are normalized to sum to 1
/// (negative and non-finite weights count as 0). Rotations are averaged after aligning
/// each to the first contributing pose's hemisphere (neighborhood alignment), then
/// normalized. Inputs should be as long as `out`; a shorter input does not contribute to
/// the bones it lacks. Returns false, leaving `out` untouched, when no weight is positive.
pub fn blend_poses(inputs: &[(&[Transform], f32)], out: &mut [Transform]) -> bool {
    let total: f32 = inputs.iter().map(|(_, w)| usable(*w)).sum();
    if total <= 0.0 || !total.is_finite() {
        return false;
    }
    for (bone, o) in out.iter_mut().enumerate() {
        let mut translation = Vec3::ZERO;
        let mut scale = Vec3::ZERO;
        let mut rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
        let mut reference: Option<Quat> = None;
        for (pose, w) in inputs {
            let w = usable(*w) / total;
            let Some(x) = pose.get(bone).filter(|_| w > 0.0) else {
                continue;
            };
            let r = *reference.get_or_insert(x.rotation);
            translation += x.translation * w;
            scale += x.scale * w;
            rotation += align(r, x.rotation) * w;
        }
        *o = Transform::new(translation, normalize_or_identity(rotation), scale);
    }
    true
}

/// Two-way blend in place: `out = lerp(out, other, t)` per bone (shortest-path nlerp).
pub fn lerp_into(out: &mut [Transform], other: &[Transform], t: f32) {
    for (o, x) in out.iter_mut().zip(other) {
        *o = o.lerp(x, t);
    }
}

/// Calls `f` with each bone index of a mask (`mask` empty means all `bone_count` bones).
fn for_each_masked(bone_count: usize, mask: &[u16], mut f: impl FnMut(usize)) {
    if mask.is_empty() {
        (0..bone_count).for_each(f);
    } else {
        mask.iter()
            .map(|b| usize::from(*b))
            .filter(|b| *b < bone_count)
            .for_each(&mut f);
    }
}

/// Override layer: masked bones of `base` move toward `layer` by `weight` (clamped to
/// `[0, 1]`).
pub fn apply_override(base: &mut [Transform], layer: &[Transform], weight: f32, mask: &[u16]) {
    let w = usable(weight).min(1.0);
    if w <= 0.0 {
        return;
    }
    let n = base.len();
    for_each_masked(n, mask, |b| {
        if let (Some(o), Some(x)) = (base.get_mut(b), layer.get(b)) {
            *o = if w >= 1.0 { *x } else { o.lerp(x, w) };
        }
    });
}

/// The additive delta of `pose` relative to `reference`: `reference⁻¹ * pose`, so that
/// `reference * delta == pose`.
pub fn additive_delta(reference: &Transform, pose: &Transform) -> Transform {
    reference.inverse() * *pose
}

/// Additive layer: for masked bones, `base = base * lerp(identity, delta, weight)` where
/// `delta = reference_inverse * layer` (see [`additive_delta`]). `weight` is clamped to
/// `[0, 1]`.
pub fn apply_additive(
    base: &mut [Transform],
    layer: &[Transform],
    reference_inverse: &[Transform],
    weight: f32,
    mask: &[u16],
) {
    let w = usable(weight).min(1.0);
    if w <= 0.0 {
        return;
    }
    let n = base.len();
    for_each_masked(n, mask, |b| {
        if let (Some(o), Some(x), Some(r)) = (base.get_mut(b), layer.get(b), reference_inverse.get(b)) {
            let delta = *r * *x;
            *o = *o * Transform::IDENTITY.lerp(&delta, w);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::tests::TestResult;

    fn t(x: f32, yaw: f32) -> Transform {
        Transform::new(
            Vec3::new(x, 0.0, 0.0),
            Quat::from_rotation_y(yaw),
            Vec3::splat(1.0 + x),
        )
    }

    #[test]
    fn n_way_blend_normalizes_weights() -> TestResult {
        let a = [t(0.0, 0.0)];
        let b = [t(1.0, 0.0)];
        let c = [t(2.0, 0.0)];
        let mut out = [Transform::IDENTITY];
        assert!(blend_poses(&[(&a, 2.0), (&b, 1.0), (&c, 1.0)], &mut out));
        let o = out.first().ok_or("bone")?;
        assert!((o.translation.x - 0.75).abs() < 1e-6);
        assert!((o.scale.x - 1.75).abs() < 1e-6);
        // Scaling every weight changes nothing.
        let mut again = [Transform::IDENTITY];
        assert!(blend_poses(&[(&a, 20.0), (&b, 10.0), (&c, 10.0)], &mut again));
        assert!(again.first().ok_or("bone")?.approx_eq(o, 1e-6));
        Ok(())
    }

    #[test]
    fn n_way_blend_aligns_hemispheres() -> TestResult {
        let a = [Transform::new(Vec3::ZERO, Quat::from_rotation_y(0.4), Vec3::ONE)];
        let b = [Transform::new(Vec3::ZERO, -Quat::from_rotation_y(0.4), Vec3::ONE)];
        let mut out = [Transform::IDENTITY];
        assert!(blend_poses(&[(&a, 1.0), (&b, 1.0)], &mut out));
        let r = out.first().ok_or("bone")?.rotation;
        assert!(r.angle_between(Quat::from_rotation_y(0.4)) < 1e-5, "{r:?}");
        Ok(())
    }

    #[test]
    fn zero_or_bad_weights_leave_out_untouched() {
        let a = [t(1.0, 0.3)];
        let mut out = [Transform::IDENTITY];
        assert!(!blend_poses(&[(&a, 0.0), (&a, -1.0), (&a, f32::NAN)], &mut out));
        assert!(!blend_poses(&[], &mut out));
        assert_eq!(out, [Transform::IDENTITY]);
    }

    #[test]
    fn masked_override() {
        let mut base = [t(0.0, 0.0), t(0.0, 0.0), t(0.0, 0.0)];
        let layer = [t(1.0, 0.0), t(1.0, 0.0), t(1.0, 0.0)];
        apply_override(&mut base, &layer, 0.5, &[1, 9]);
        let xs: Vec<f32> = base.iter().map(|b| b.translation.x).collect();
        assert_eq!(xs, [0.0, 0.5, 0.0]);
        apply_override(&mut base, &layer, 1.0, &[]);
        assert_eq!(base, layer);
        apply_override(&mut base, &[t(5.0, 0.0); 3], 0.0, &[]);
        assert_eq!(base, layer);
    }

    #[test]
    fn additive_reproduces_the_layer_on_its_reference() {
        let reference = [t(1.0, 0.2), t(-0.5, 1.0)];
        let layer = [t(1.5, 0.7), t(-0.5, 1.3)];
        let inverse: Vec<Transform> = reference.iter().map(Transform::inverse).collect();
        // base == reference: the result is the layer pose itself.
        let mut base = reference;
        apply_additive(&mut base, &layer, &inverse, 1.0, &[]);
        for (b, l) in base.iter().zip(&layer) {
            assert!(b.approx_eq(l, 1e-4), "{b:?} vs {l:?}");
        }
        // A delta adds on top of a different base: the yaw offsets add up.
        let mut other = [t(0.0, 0.0), t(0.0, -0.4)];
        apply_additive(&mut other, &layer, &inverse, 1.0, &[1]);
        let yaw = crate::root_motion::yaw_of(other.get(1).map_or(Quat::IDENTITY, |b| b.rotation));
        assert!((yaw - (-0.4 + 0.3)).abs() < 1e-5, "{yaw}");
        assert_eq!(other.first().map(|b| b.translation.x), Some(0.0), "masked out");
        // Half weight applies half the delta.
        let mut half = [t(0.0, 0.0), t(0.0, 0.0)];
        apply_additive(&mut half, &layer, &inverse, 0.5, &[]);
        let yaw = crate::root_motion::yaw_of(half.first().map_or(Quat::IDENTITY, |b| b.rotation));
        assert!((yaw - 0.25).abs() < 1e-4, "{yaw}");
        let d = additive_delta(&t(1.0, 0.2), &t(1.5, 0.7));
        assert!((t(1.0, 0.2) * d).approx_eq(&t(1.5, 0.7), 1e-4));
    }

    #[test]
    fn lerp_into_blends_in_place() {
        let mut a = [t(0.0, 0.0)];
        lerp_into(&mut a, &[t(2.0, 0.0)], 0.25);
        assert!((a.first().map_or(0.0, |x| x.translation.x) - 0.5).abs() < 1e-6);
    }
}
