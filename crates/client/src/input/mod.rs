//! Input (plan 8.7): device-agnostic actions, rebindable, with per-context action sets.
//!
//! Flow:
//! 1. The platform layer translates OS events into [`device::RawInput`].
//! 2. On the render thread, [`router::InputRouter`] routes them through the active
//!    context stack into a per-frame [`router::ActionFrame`]; mouse motion goes to the
//!    camera ([`crate::camera`]) the same frame.
//! 3. The render thread merges each frame into the [`accumulator::InputAccumulator`].
//! 4. On the simulation thread, each tick takes a [`accumulator::TickInput`] and
//!    [`intent::MoveIntentMap`] turns it into a move intent.

pub mod accumulator;
pub mod action;
pub mod binding;
pub mod device;
pub mod intent;
pub mod router;

/// Input configuration errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputError {
    /// An action with this name already exists.
    DuplicateAction,
    /// A context with this name already exists.
    DuplicateContext,
    /// The action table is full.
    TooManyActions,
    /// The context table or stack is full.
    TooManyContexts,
    /// The action id is not in the table.
    UnknownAction,
    /// The context id is not in the table.
    UnknownContext,
    /// Names must be non-empty.
    InvalidName,
    /// The binding kind does not match the action kind.
    KindMismatch,
    /// A binding parameter is out of range or not finite.
    InvalidBinding,
    /// The device input is already bound to another action in this context.
    Conflict {
        /// The action it is bound to.
        existing: action::ActionId,
    },
    /// No such binding.
    NotBound,
}

impl core::fmt::Display for InputError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InputError::DuplicateAction => f.write_str("duplicate action name"),
            InputError::DuplicateContext => f.write_str("duplicate context name"),
            InputError::TooManyActions => f.write_str("action table full"),
            InputError::TooManyContexts => f.write_str("context table or stack full"),
            InputError::UnknownAction => f.write_str("unknown action"),
            InputError::UnknownContext => f.write_str("unknown context"),
            InputError::InvalidName => f.write_str("empty name"),
            InputError::KindMismatch => f.write_str("binding kind does not match action kind"),
            InputError::InvalidBinding => f.write_str("binding parameter out of range"),
            InputError::Conflict { existing } => {
                write!(f, "input already bound to action {}", existing.index())
            }
            InputError::NotBound => f.write_str("no such binding"),
        }
    }
}

impl std::error::Error for InputError {}

#[cfg(test)]
mod tests {
    use super::accumulator::InputAccumulator;
    use super::action::{ActionId, ActionKind, ActionTable, MAX_ACTIONS};
    use super::binding::{Binding, ContextId, ContextTable, SourceKey};
    use super::device::{AxisSource, ButtonSource, GamepadAxis, KeyCode, RawInput};
    use super::intent::{AxisButtons, MoveIntentMap};
    use super::router::InputRouter;
    use super::*;
    use crate::camera::LookSample;
    use crate::core_api::{AimAngles, Angle16, InputSeq, MoveButtons, Tick};
    use crate::testing::TestResult;

    struct Fixture {
        router: InputRouter,
        forward: ActionId,
        jump: ActionId,
        move_y: ActionId,
        gameplay: ContextId,
        menu: ContextId,
        text: ContextId,
    }

    fn key(k: KeyCode, pressed: bool) -> RawInput {
        RawInput::Button {
            source: ButtonSource::Key(k),
            pressed,
        }
    }

    fn fixture() -> Result<Fixture, InputError> {
        let mut a = ActionTable::new();
        let forward = a.define("move_forward", ActionKind::Button)?;
        let jump = a.define("jump", ActionKind::Button)?;
        let move_y = a.define("move_y", ActionKind::Axis)?;
        let confirm = a.define("menu_confirm", ActionKind::Button)?;
        let mut c = ContextTable::new();
        let gameplay = c.define("gameplay", false)?;
        let menu = c.define("menu", false)?;
        let text = c.define("text_entry", true)?;
        c.bind(
            &a,
            gameplay,
            forward,
            Binding::Button(ButtonSource::Key(KeyCode::W)),
        )?;
        c.bind(
            &a,
            gameplay,
            forward,
            Binding::Button(ButtonSource::Key(KeyCode::ArrowUp)),
        )?;
        c.bind(
            &a,
            gameplay,
            jump,
            Binding::Button(ButtonSource::Key(KeyCode::Space)),
        )?;
        c.bind(
            &a,
            gameplay,
            move_y,
            Binding::ButtonAxis {
                source: ButtonSource::Key(KeyCode::I),
                value: 1.0,
            },
        )?;
        c.bind(
            &a,
            gameplay,
            move_y,
            Binding::ButtonAxis {
                source: ButtonSource::Key(KeyCode::K),
                value: -1.0,
            },
        )?;
        c.bind(
            &a,
            gameplay,
            move_y,
            Binding::Axis {
                source: AxisSource::Gamepad(GamepadAxis::LeftStickY),
                scale: 1.0,
                deadzone: 0.2,
            },
        )?;
        // The menu binds Space to confirm, shadowing jump, but not W.
        c.bind(
            &a,
            menu,
            confirm,
            Binding::Button(ButtonSource::Key(KeyCode::Space)),
        )?;
        let mut router = InputRouter::new(a, c);
        router.push_context(gameplay)?;
        Ok(Fixture {
            router,
            forward,
            jump,
            move_y,
            gameplay,
            menu,
            text,
        })
    }

    #[test]
    fn press_hold_release_edges() -> TestResult {
        let mut f = fixture()?;
        f.router.handle(key(KeyCode::W, true));
        let fr = f.router.end_frame();
        assert!(fr.pressed.contains(f.forward) && fr.held.contains(f.forward));
        f.router.handle(key(KeyCode::W, true)); // OS key repeat: no new edge
        let fr = f.router.end_frame();
        assert!(!fr.pressed.contains(f.forward) && fr.held.contains(f.forward));
        f.router.handle(key(KeyCode::W, false));
        let fr = f.router.end_frame();
        assert!(fr.released.contains(f.forward) && !fr.held.contains(f.forward));
        Ok(())
    }

    #[test]
    fn two_sources_on_one_action_count_holds() -> TestResult {
        let mut f = fixture()?;
        f.router.handle(key(KeyCode::W, true));
        f.router.handle(key(KeyCode::ArrowUp, true));
        f.router.handle(key(KeyCode::W, false));
        let fr = f.router.end_frame();
        assert!(fr.held.contains(f.forward) && !fr.released.contains(f.forward));
        f.router.handle(key(KeyCode::ArrowUp, false));
        assert!(f.router.end_frame().released.contains(f.forward));
        Ok(())
    }

    #[test]
    fn contexts_shadow_and_exclusive_blocks() -> TestResult {
        let mut f = fixture()?;
        f.router.push_context(f.menu)?;
        f.router.handle(key(KeyCode::Space, true));
        f.router.handle(key(KeyCode::W, true)); // not bound in menu: falls through to gameplay
        let fr = f.router.end_frame();
        assert!(!fr.held.contains(f.jump), "menu shadows Space");
        assert!(fr.held.contains(f.forward), "W falls through to gameplay");
        f.router.push_context(f.text)?;
        f.router.handle(key(KeyCode::W, true));
        let fr = f.router.end_frame();
        assert!(fr.held.is_empty(), "exclusive text entry blocks everything");
        assert_eq!(f.router.pop_context(), Some(f.text));
        assert_eq!(f.router.stack(), &[f.gameplay, f.menu]);
        Ok(())
    }

    #[test]
    fn context_change_and_focus_loss_release_everything() -> TestResult {
        let mut f = fixture()?;
        f.router.handle(key(KeyCode::W, true));
        let _ = f.router.end_frame();
        f.router.push_context(f.menu)?;
        let fr = f.router.end_frame();
        assert!(fr.released.contains(f.forward) && fr.held.is_empty());
        f.router.handle(key(KeyCode::W, true));
        f.router.handle(RawInput::FocusLost);
        let fr = f.router.end_frame();
        assert!(fr.held.is_empty());
        // The release of a key pressed before the change is harmless.
        f.router.handle(key(KeyCode::W, false));
        assert!(f.router.end_frame().released.is_empty());
        Ok(())
    }

    #[test]
    fn axes_combine_digital_and_analog_with_deadzone() -> TestResult {
        let mut f = fixture()?;
        f.router.handle(RawInput::Axis {
            source: AxisSource::Gamepad(GamepadAxis::LeftStickY),
            value: 0.1,
        });
        assert_eq!(f.router.end_frame().axis(f.move_y), 0.0, "inside dead zone");
        f.router.handle(RawInput::Axis {
            source: AxisSource::Gamepad(GamepadAxis::LeftStickY),
            value: 0.6,
        });
        assert!(
            (f.router.end_frame().axis(f.move_y) - 0.5).abs() < 1e-6,
            "rescaled past dead zone"
        );
        f.router.handle(key(KeyCode::I, true));
        assert_eq!(f.router.end_frame().axis(f.move_y), 1.0, "clamped sum");
        f.router.handle(key(KeyCode::K, true));
        assert!((f.router.end_frame().axis(f.move_y) - 0.5).abs() < 1e-6);
        f.router.handle(RawInput::Axis {
            source: AxisSource::Gamepad(GamepadAxis::LeftStickY),
            value: f32::NAN,
        });
        assert_eq!(
            f.router.end_frame().axis(f.move_y),
            0.0,
            "NaN reads as zero, digital cancels"
        );
        Ok(())
    }

    #[test]
    fn mouse_motion_accumulates_per_frame() -> TestResult {
        let mut f = fixture()?;
        f.router.handle(RawInput::MouseMotion { dx: 3.0, dy: -1.0 });
        f.router.handle(RawInput::MouseMotion { dx: 2.0, dy: -1.0 });
        let fr = f.router.end_frame();
        assert_eq!((fr.look_dx, fr.look_dy), (5.0, -2.0));
        let fr = f.router.end_frame();
        assert_eq!((fr.look_dx, fr.look_dy), (0.0, 0.0));
        Ok(())
    }

    #[test]
    fn binding_validation_and_rebinding() -> TestResult {
        let mut f = fixture()?;
        let (fwd, jump, gp) = (f.forward, f.jump, f.gameplay);
        let actions = f.router.actions().clone();
        let ctx = f.router.contexts_mut();
        let w = ButtonSource::Key(KeyCode::W);
        assert_eq!(
            ctx.bind(&actions, gp, jump, Binding::Button(w)),
            Err(InputError::Conflict { existing: fwd })
        );
        assert_eq!(
            ctx.bind(
                &actions,
                gp,
                jump,
                Binding::ButtonAxis {
                    source: ButtonSource::Key(KeyCode::Q),
                    value: 1.0
                }
            ),
            Err(InputError::KindMismatch)
        );
        // Rebind forward from W to E; W is free afterwards.
        ctx.rebind(
            &actions,
            gp,
            fwd,
            SourceKey::Button(w),
            Binding::Button(ButtonSource::Key(KeyCode::E)),
        )?;
        assert_eq!(
            ctx.get(gp).and_then(|c| c.binding_for(SourceKey::Button(w))),
            None
        );
        // A conflicting rebind fails atomically and keeps the old binding.
        let e = SourceKey::Button(ButtonSource::Key(KeyCode::E));
        let r = ctx.rebind(
            &actions,
            gp,
            fwd,
            e,
            Binding::Button(ButtonSource::Key(KeyCode::Space)),
        );
        assert_eq!(r, Err(InputError::Conflict { existing: jump }));
        assert_eq!(
            ctx.get(gp).and_then(|c| c.binding_for(e)).map(|(a, _)| a),
            Some(fwd)
        );
        assert_eq!(ctx.unbind(gp, e), Ok(fwd));
        assert_eq!(ctx.unbind(gp, e), Err(InputError::NotBound));
        f.router.handle(key(KeyCode::E, true));
        assert!(f.router.end_frame().held.is_empty());
        Ok(())
    }

    #[test]
    fn action_table_limits() -> TestResult {
        let mut a = ActionTable::new();
        for i in 0..MAX_ACTIONS {
            a.define(&format!("a{i}"), ActionKind::Button)?;
        }
        assert_eq!(
            a.define("one_more", ActionKind::Button),
            Err(InputError::TooManyActions)
        );
        assert_eq!(
            a.define("a0", ActionKind::Button),
            Err(InputError::DuplicateAction)
        );
        assert_eq!(a.define("", ActionKind::Button), Err(InputError::InvalidName));
        Ok(())
    }

    #[test]
    fn taps_between_ticks_are_not_lost() -> TestResult {
        let mut f = fixture()?;
        let acc = InputAccumulator::new();
        let look = LookSample {
            yaw: Angle16(123),
            pitch: Angle16(7),
        };
        // Frame 1: press and release jump inside one frame. Frame 2: nothing.
        f.router.handle(key(KeyCode::Space, true));
        f.router.handle(key(KeyCode::Space, false));
        acc.merge_frame(&f.router.end_frame(), look);
        acc.merge_frame(&f.router.end_frame(), look);
        let t = acc.take();
        assert_eq!(t.frames, 2);
        assert!(t.active(f.jump), "tap seen by the tick");
        assert!(!t.held.contains(f.jump));
        let t = acc.take();
        assert!(!t.active(f.jump), "edge consumed once");
        assert_eq!(t.look, look, "look persists");
        Ok(())
    }

    #[test]
    fn intent_map_builds_move_with_quantized_look() -> TestResult {
        let mut f = fixture()?;
        let mut map = MoveIntentMap::new();
        let actions = f.router.actions().clone();
        map.map_button(&actions, f.forward, MoveButtons::FORWARD)?;
        map.map_button(&actions, f.jump, MoveButtons::JUMP)?;
        map.map_axis(
            &actions,
            AxisButtons {
                action: f.move_y,
                threshold: 0.5,
                positive: MoveButtons::FORWARD,
                negative: MoveButtons::BACKWARD,
            },
        )?;
        assert_eq!(
            map.map_button(&actions, f.move_y, MoveButtons::STRAFE_LEFT),
            Err(InputError::KindMismatch)
        );
        let acc = InputAccumulator::new();
        f.router.handle(key(KeyCode::K, true));
        f.router.handle(key(KeyCode::Space, true));
        acc.merge_frame(
            &f.router.end_frame(),
            LookSample {
                yaw: Angle16(500),
                pitch: Angle16(9),
            },
        );
        let m = map.build(&acc.take(), InputSeq(4), Tick(10));
        assert_eq!(m.buttons, MoveButtons::BACKWARD.with(MoveButtons::JUMP));
        assert_eq!(
            (m.seq, m.tick, m.yaw, m.aim),
            (
                InputSeq(4),
                Tick(10),
                Angle16(500),
                AimAngles {
                    yaw: Angle16(500),
                    pitch: Angle16(9)
                }
            )
        );
        Ok(())
    }
}
