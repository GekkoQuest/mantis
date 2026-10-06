//! Opens a window and runs a world session with no server: the sim thread ticks, the
//! render thread samples mouse look every frame and clears the window to a color derived
//! from the camera yaw and pitch. Move the mouse to see look applied per frame; close the
//! window to shut down (the render and sim threads are joined on exit).
//!
//! Run with `cargo run -p mantis-client --example window`. Tests never open a window.

use std::sync::Arc;

use mantis_client::core_api::{
    Angle16, CoreMotion, FlatGround, Motion, MotionParams, MotionState, MoveInput, TickRate, Vec3,
};
use mantis_client::host::{WorldSessionConfig, WorldSessionParts, build_world_session};
use mantis_client::input::action::ActionTable;
use mantis_client::input::binding::ContextTable;
use mantis_client::input::intent::MoveIntentMap;
use mantis_client::input::router::InputRouter;
use mantis_client::platform::{ClearSink, MonotonicClock, PlatformConfig, PlatformError, run};
use mantis_client::sim::IntentSink;
use mantis_client::threads::render_thread::FrameContext;
use mantis_client::time::HostClock;

struct DiscardIntents;

impl IntentSink for DiscardIntents {
    fn send_move(&mut self, _input: &MoveInput) {}
}

fn color(frame: &FrameContext<'_>) -> [f64; 3] {
    let yaw = f64::from(frame.camera.yaw_turns());
    let pitch = f64::from(frame.camera.pitch_turns()) + 0.5;
    [yaw * 0.6, pitch * 0.5, 0.25]
}

fn main() -> Result<(), PlatformError> {
    run(PlatformConfig::default(), |target, events| {
        let rate = TickRate::new(30).ok_or_else(|| PlatformError::Start("tick rate".to_owned()))?;
        let config = WorldSessionConfig::new(rate);
        let clock: Arc<dyn HostClock> = Arc::new(MonotonicClock::new());
        let motion = Motion::new(MotionParams::DEFAULT).map_err(|e| PlatformError::Start(e.to_owned()))?;
        let session = build_world_session(
            &config,
            WorldSessionParts {
                motion: CoreMotion::<FlatGround>::new(motion),
                ground: Arc::new(FlatGround(0.0)),
                initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
                router: InputRouter::new(ActionTable::new(), ContextTable::new()),
                intents: MoveIntentMap::new(),
                outbox: DiscardIntents,
            },
            clock,
            events,
            ClearSink::new(target, color),
        );
        // The platform keeps the running session until the window closes, then drops it,
        // which stops and joins the render and sim threads.
        session
            .spawn(&config, None)
            .map_err(|e| PlatformError::Start(e.to_string()))
    })
}
