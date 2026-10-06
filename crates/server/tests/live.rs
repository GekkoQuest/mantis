//! Live (Ops) changes and system outcomes inside a cell: a signed change
//! reaches the cell as a logged `SetLive` intent and applies at the next
//! tick boundary; module flags and tunables are read when modules act; a
//! system's durable change is recorded as an outcome in the same tick; and
//! replay re-derives all of it from the cell's log.

#![expect(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_adapter_contract::core_types::WireString;
use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_server::harness::Harness;
use mantis_server::intent::CellIntent;
use mantis_server::modules::{
    ExtensionKind, LIVE_FLAG, LIVE_TUNABLE, ModuleOutcome, Payload, Registrar, RegistryError, ServerModule,
    SystemOutcomes, emit_outcome, flag, outcome_room, tunable,
};

const MANIFEST: &str =
    "[module]\nkey = \"test.mint\"\nversion = \"0.1.0\"\n\n[flags]\nenabled = true\nminting = true\n";
const MINTED: ExtensionKind = ExtensionKind(950);

/// Mints `test.mint.rate` coins per tick while `test.mint.minting` is on,
/// recording each mint as a system outcome.
#[derive(Debug, Default)]
struct Mint {
    coins: u64,
}

impl StateHash for Mint {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.coins);
    }
}

impl Resource for Mint {
    const NAME: &'static str = "test.mint.state";
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the rate is a small positive whole number set by the test"
)]
fn mint(world: &mut World, _: &TickContext) -> Result<(), SystemError> {
    if !flag(world, "test.mint.minting") || !outcome_room(world) {
        return Ok(());
    }
    let rate = tunable(world, "test.mint.rate").unwrap_or(0.0);
    let n = if rate > 0.0 { rate as u64 } else { 0 };
    world
        .resource_mut::<Mint>()
        .ok_or(SystemError::Invariant("mint"))?
        .coins += n;
    emit_outcome(
        world,
        ModuleOutcome {
            kind: MINTED,
            session: None,
            result: Ok(()),
            payload: Payload::from_slice(&n.to_le_bytes()).unwrap_or(Payload::EMPTY),
        },
    )
}

struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "test.mint"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        assert_eq!(r.tunable("rate", 2.0), "test.mint.rate");
        r.resource(Mint::default())?;
        let access = r
            .access()
            .write_resource::<Mint>()
            .write_resource::<SystemOutcomes>()
            .build()?;
        r.system(
            SystemDesc {
                name: "test.mint.run",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            mint,
        )?;
        Ok(())
    }
}

fn coins(h: &Harness) -> u64 {
    h.world().resource::<Mint>().unwrap().coins
}

fn bed() -> Harness {
    Harness::new(&[Arc::new(Module)], &[MANIFEST], &[], &[]).unwrap()
}

#[test]
fn live_changes_apply_at_the_next_tick_and_replay() {
    let mut h = bed();
    h.tick().unwrap();
    assert_eq!(coins(&h), 2);
    assert!(flag(h.world(), "test.mint.minting"));

    // A tunable change queued now applies when the next tick starts.
    assert!(h.set_live("test.mint.rate", LIVE_TUNABLE, 5.0));
    assert_eq!(tunable(h.world(), "test.mint.rate"), Some(2.0));
    h.tick().unwrap();
    assert_eq!(tunable(h.world(), "test.mint.rate"), Some(5.0));
    assert_eq!(coins(&h), 7);

    // A module flag, read when the module acts.
    h.set_live("test.mint.minting", LIVE_FLAG, 0.0);
    h.tick().unwrap();
    assert_eq!(coins(&h), 7);

    // A flag naming the module switches it as a whole.
    h.set_live("test.mint.minting", LIVE_FLAG, 1.0);
    h.set_live("test.mint", LIVE_FLAG, 0.0);
    h.tick().unwrap();
    assert_eq!(coins(&h), 7, "the module's systems are off");
    h.set_live("test.mint", LIVE_FLAG, 1.0);
    h.tick().unwrap();
    assert_eq!(coins(&h), 12);

    // Every mint was recorded as a system outcome in its tick.
    let minted: Vec<u64> = h
        .outcomes
        .iter()
        .filter(|o| o.kind == MINTED && o.session.is_none())
        .map(|o| u64::from_le_bytes(o.payload.as_slice().try_into().unwrap()))
        .collect();
    assert_eq!(minted, vec![2, 5, 5]);

    // Replay applies the same changes at the same ticks and re-checks the
    // system outcomes against the log.
    assert_eq!(h.replay().unwrap(), 5);
}

#[test]
fn unknown_unsafe_and_client_sent_changes_are_refused() {
    let mut h = bed();
    h.set_live("test.mint.nothing", LIVE_FLAG, 1.0);
    h.set_live("test.mint.speed", LIVE_TUNABLE, 1.0);
    h.set_live("test.mint.rate", LIVE_TUNABLE, f32::NAN);
    h.set_live("test.mint.rate", 9, 1.0);
    h.tick().unwrap();
    assert_eq!(tunable(h.world(), "test.mint.rate"), Some(2.0));
    assert_eq!(tunable(h.world(), "test.mint.speed"), None);

    // A session may never send a live change, even a well-formed one.
    h.join(1, 41, 0.0, 0.0);
    h.tick().unwrap();
    let forged = CellIntent::SetLive {
        name: WireString::new("test.mint.rate").unwrap(),
        kind: LIVE_TUNABLE,
        value: 100.0,
    };
    assert!(h.push_intent(1, forged));
    h.tick().unwrap();
    assert_eq!(tunable(h.world(), "test.mint.rate"), Some(2.0));
    assert!(h.replay().unwrap() > 0);
}
