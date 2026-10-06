//! The toy package's control scheme: device-agnostic actions in one gameplay context,
//! default keyboard bindings (rebindable through the router), and the action-to-intent
//! mapping the simulation turns into move inputs.

use mantis_client::core_api::MoveButtons;
use mantis_client::input::InputError;
use mantis_client::input::action::{ActionKind, ActionTable};
use mantis_client::input::binding::{Binding, ContextTable};
use mantis_client::input::device::{ButtonSource, KeyCode};
use mantis_client::input::intent::MoveIntentMap;
use mantis_client::input::router::InputRouter;

/// Module action ids by name, as defined in the input tables.
pub type ModuleActions = Vec<(&'static str, mantis_client::input::action::ActionId)>;

/// The built scheme.
#[derive(Debug)]
pub struct Controls {
    /// The router, with the gameplay context active.
    pub router: InputRouter,
    /// Move intents from actions.
    pub intents: MoveIntentMap,
}

impl Controls {
    /// Forward, backward, strafe left and right, jump, and walk, bound to W, S, A, D,
    /// Space, and Left Shift.
    ///
    /// # Errors
    /// [`InputError`] (a duplicate definition would be a bug in this table).
    pub fn build() -> Result<Controls, InputError> {
        Self::build_with(None).map(|(c, _)| c)
    }

    /// The same scheme plus the client modules' actions, defined in a `modules` context
    /// beneath gameplay (rebindable like any action). Returns the module action ids.
    ///
    /// # Errors
    /// [`InputError`], for example a module default key already bound by the host.
    pub fn build_with(
        modules: Option<&mantis_client::modules::ClientModules>,
    ) -> Result<(Controls, ModuleActions), InputError> {
        let mut actions = ActionTable::new();
        let mut contexts = ContextTable::new();
        let module_ctx = contexts.define("modules", false)?;
        let module_actions = match modules {
            Some(m) => m.define_actions(&mut actions, &mut contexts, module_ctx)?,
            None => Vec::new(),
        };
        let gameplay = contexts.define("gameplay", false)?;
        let mut intents = MoveIntentMap::new();
        let table = [
            ("move_forward", KeyCode::W, MoveButtons::FORWARD),
            ("move_backward", KeyCode::S, MoveButtons::BACKWARD),
            ("strafe_left", KeyCode::A, MoveButtons::STRAFE_LEFT),
            ("strafe_right", KeyCode::D, MoveButtons::STRAFE_RIGHT),
            ("jump", KeyCode::Space, MoveButtons::JUMP),
            ("walk", KeyCode::ShiftLeft, MoveButtons::WALK),
        ];
        for (name, key, button) in table {
            let action = actions.define(name, ActionKind::Button)?;
            contexts.bind(
                &actions,
                gameplay,
                action,
                Binding::Button(ButtonSource::Key(key)),
            )?;
            intents.map_button(&actions, action, button)?;
        }
        let mut router = InputRouter::new(actions, contexts);
        router.push_context(module_ctx)?;
        router.push_context(gameplay)?;
        Ok((Controls { router, intents }, module_actions))
    }
}
