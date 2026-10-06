//! Decision 0019: world space is `mantis_core` kinematics' left-handed frame and the
//! renderer's view transform is the only conversion. Whatever the facing, a strafe-right
//! input moves the drawn avatar to the right of the screen, forward moves it into the
//! screen, and jump moves it up.

use mantis_client::core_api::{
    AimAngles, Angle16, Motion, MotionModifiers, MotionParams, MotionState, MoveButtons, MoveInput,
};
use mantis_client::world_view::{WorldViewConfig, third_person_camera};
use mantis_core::kinematics::{FlatGround, InputSeq};
use mantis_core::time::Tick;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Clip-space x, y, and view depth of a world point.
fn screen(camera: &mantis_render::math::Camera, p: mantis_core::math::Vec3) -> (f32, f32, f32) {
    let clip = camera.view_projection() * glam::Vec4::new(p.x, p.y, p.z, 1.0);
    let view = camera.view() * glam::Vec4::new(p.x, p.y, p.z, 1.0);
    (clip.x / clip.w, clip.y / clip.w, view.z)
}

#[test]
fn strafe_right_moves_the_drawn_avatar_to_screen_right_at_every_facing() -> TestResult {
    let motion = Motion::new(MotionParams::DEFAULT)?;
    let ground = FlatGround(0.0);
    let config = WorldViewConfig::new(640, 360, wgpu::TextureFormat::Rgba8Unorm);
    for step in 0..16u16 {
        let yaw = Angle16(step * 4096);
        let yaw_turns = f32::from(yaw.0) / 65_536.0;
        let start = MotionState::at_rest(mantis_core::math::Vec3::new(3.0, 0.0, -2.0), yaw);
        let camera = third_person_camera(start.position, yaw_turns, 0.0, &config);
        let run = |buttons: MoveButtons| {
            let mut s = start;
            for k in 0..10u32 {
                let input = MoveInput {
                    seq: InputSeq(k + 1),
                    tick: Tick(u64::from(k)),
                    buttons,
                    yaw,
                    aim: AimAngles::default(),
                };
                s = motion.step(&ground, &s, &input, &MotionModifiers::NONE, 1.0 / 30.0);
            }
            s.position
        };
        let (x0, y0, z0) = screen(&camera, start.position);
        let (xr, _, _) = screen(&camera, run(MoveButtons::STRAFE_RIGHT));
        let (xl, _, _) = screen(&camera, run(MoveButtons::STRAFE_LEFT));
        let (_, _, zf) = screen(&camera, run(MoveButtons::FORWARD));
        assert!(
            xr > x0 + 0.01,
            "yaw {yaw:?}: strafe right drew at {xr}, from {x0}"
        );
        assert!(xl < x0 - 0.01, "yaw {yaw:?}: strafe left drew at {xl}, from {x0}");
        assert!(zf > z0, "yaw {yaw:?}: forward goes into the screen");
        // Jumping rises on screen.
        let mut s = start;
        let jump = MoveInput {
            seq: InputSeq(1),
            tick: Tick(0),
            buttons: MoveButtons::JUMP,
            yaw,
            aim: AimAngles::default(),
        };
        for _ in 0..4 {
            s = motion.step(&ground, &s, &jump, &MotionModifiers::NONE, 1.0 / 30.0);
        }
        let (_, yj, _) = screen(&camera, s.position);
        assert!(yj > y0, "yaw {yaw:?}: jumping rises on screen");
    }
    Ok(())
}

#[test]
fn the_audio_listener_hears_screen_right_on_the_right() -> TestResult {
    use mantis_audio::spatial::Listener;
    let config = WorldViewConfig::new(640, 360, wgpu::TextureFormat::Rgba8Unorm);
    for step in 0..8u16 {
        let yaw_turns = f32::from(step) / 8.0;
        let camera = third_person_camera(mantis_core::math::Vec3::ZERO, yaw_turns, 0.0, &config);
        let mantis_audio::AudioEvent::SetListener {
            position,
            forward,
            up,
        } = mantis_client::media::listener_event(&camera)
        else {
            return Err("not a listener event".into());
        };
        let listener = Listener::new(position, forward, up).ok_or("listener")?;
        // A point to the camera's screen-right pans right.
        let view = camera.view();
        let right_world = view.inverse().transform_point3(glam::Vec3::new(3.0, 0.0, 5.0));
        let clip = camera.view_projection() * right_world.extend(1.0);
        assert!(clip.x / clip.w > 0.0, "on screen-right");
        let (_, pan) = listener.locate(right_world.to_array(), 0.1);
        assert!(pan > 0.3, "yaw {yaw_turns}: pan {pan}");
    }
    Ok(())
}
