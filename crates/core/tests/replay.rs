//! Record and replay (plan 6.8, decision 0007), end to end.
//!
//! A small but complete simulation (movement from intents through
//! `Motion::step`, seeded random rolls, a gameplay graph with actions, and
//! idempotent economy commands) runs through the scheduler while a recorder
//! writes the unified log. A fresh simulation is then replayed from the log,
//! and every tick's world state hash must match. Divergence, corruption,
//! truncation, and foreign logs are each shown to be caught.

#![expect(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::BTreeSet;

use mantis_core::content::ContentHash;
use mantis_core::ecs::{Access, Component, Components, EntityId, Resource, World, Write};
use mantis_core::graph::{
    ActionCall, ActionError, ActionHandler, ActionParams, GameplayGraph, GraphCatalog, GraphId, GraphRuntime,
    MarkerSpec, Node, NodeKey, NodeKind, Target,
};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::kinematics::{
    Angle16, FlatGround, InputSeq, Motion, MotionModifiers, MotionParams, MotionState, MoveButtons, MoveInput,
};
use mantis_core::log::{BuildId, CellId, LogError, LogHeader, LogReader, LogSchema, LogWriter, SessionId};
use mantis_core::math::Vec3;
use mantis_core::mem::BoundedVec;
use mantis_core::replay::{ReplayError, ReplayReport, Replayable, replay};
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::schedule::{Phase, Schedule, SystemDesc, SystemError, TickContext};
use mantis_core::time::{Tick, TickRate};
use mantis_core::wire::{DecodeError, Decoder, Encoder, Wire};

// ---- components and resources ------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Body(MotionState);
mantis_core::impl_state_hash!(Body { 0 });
impl Component for Body {
    const NAME: &'static str = "test.body";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wallet(i64);
mantis_core::impl_state_hash!(Wallet { 0 });
impl Component for Wallet {
    const NAME: &'static str = "test.wallet";
}

/// Inputs delivered and not yet applied: simulation state.
struct PendingMoves(BoundedVec<(EntityId, MoveInput)>);
impl StateHash for PendingMoves {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0.len() as u64);
        for (e, m) in self.0.iter() {
            e.state_hash(h);
            m.state_hash(h);
        }
    }
}
impl Resource for PendingMoves {
    const NAME: &'static str = "test.pending_moves";
}

/// Idempotency keys already applied, and injected entropy: simulation state.
#[derive(Default)]
struct Economy {
    applied: BTreeSet<u64>,
    entropy: u64,
}
impl StateHash for Economy {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.applied.len() as u64);
        for k in &self.applied {
            h.write_u64(*k);
        }
        h.write_u64(self.entropy);
    }
}
impl Resource for Economy {
    const NAME: &'static str = "test.economy";
}

// ---- log schema ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MoveIntent {
    entity: EntityId,
    input: MoveInput,
}
impl Wire for MoveIntent {
    fn encode(&self, e: &mut Encoder<'_>) {
        self.entity.encode(e);
        self.input.encode(e);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            entity: EntityId::decode(d)?,
            input: MoveInput::decode(d)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grant {
    key: u64,
    entity: EntityId,
    amount: i64,
}
impl Wire for Grant {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u64(self.key);
        self.entity.encode(e);
        e.i64(self.amount);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            key: d.u64()?,
            entity: EntityId::decode(d)?,
            amount: d.i64()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrantOutcome {
    Applied { key: u64, balance: i64 },
    Duplicate { key: u64 },
    Rejected { key: u64 },
}
impl Wire for GrantOutcome {
    fn encode(&self, e: &mut Encoder<'_>) {
        match *self {
            Self::Applied { key, balance } => {
                e.u8(1);
                e.u64(key);
                e.i64(balance);
            }
            Self::Duplicate { key } => {
                e.u8(2);
                e.u64(key);
            }
            Self::Rejected { key } => {
                e.u8(3);
                e.u64(key);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match d.u8()? {
            1 => Ok(Self::Applied {
                key: d.u64()?,
                balance: d.i64()?,
            }),
            2 => Ok(Self::Duplicate { key: d.u64()? }),
            3 => Ok(Self::Rejected { key: d.u64()? }),
            _ => Err(DecodeError::Invalid("grant outcome tag")),
        }
    }
}

struct Schema;
impl LogSchema for Schema {
    type Intent = MoveIntent;
    type Command = Grant;
    type Outcome = GrantOutcome;
}

// ---- the simulation -----------------------------------------------------------

const BONUS: Salt = Salt::named("test.combat.bonus");
const AVATARS: u32 = 24;

#[derive(Debug, PartialEq, Eq)]
enum SimError {
    System(String),
    OutcomeMismatch,
    UnknownEntity,
}

struct WalletHandler<'a> {
    comps: &'a mut Components,
}
impl ActionHandler for WalletHandler<'_> {
    fn apply(&mut self, call: &ActionCall) -> Result<(), ActionError> {
        let mut w = self
            .comps
            .get_mut::<Wallet>(call.target)
            .ok_or(ActionError("no wallet"))?;
        w.0 += i64::from(call.params.0[0]);
        Ok(())
    }
}

struct Sim {
    world: World,
    schedule: Schedule,
    rate: TickRate,
    seed: Seed,
    derived: Vec<GrantOutcome>,
}

fn stipend_catalog() -> (GraphCatalog, GraphId) {
    let mut catalog = GraphCatalog::new();
    let pay = catalog.register_action("test.pay").unwrap();
    let graph = GraphId::named("test.stipend");
    catalog
        .insert(GameplayGraph {
            id: graph,
            entry: NodeKey(1),
            nodes: vec![
                Node {
                    key: NodeKey(1),
                    kind: NodeKind::Repeat {
                        counter: 0,
                        times: 3,
                        body: NodeKey(2),
                        done: Some(NodeKey(4)),
                    },
                },
                Node {
                    key: NodeKey(2),
                    kind: NodeKind::Action {
                        action: pay,
                        target: Target::Source,
                        params: ActionParams([2, 0, 0, 0]),
                        next: Some(NodeKey(3)),
                    },
                },
                Node {
                    key: NodeKey(3),
                    kind: NodeKind::Delay {
                        ticks: 4,
                        next: Some(NodeKey(1)),
                    },
                },
                Node {
                    key: NodeKey(4),
                    kind: NodeKind::Marker {
                        marker: MarkerSpec::Expire,
                        offset: 0,
                        next: None,
                    },
                },
            ],
        })
        .unwrap();
    (catalog, graph)
}

fn build_sim(seed: Seed, params: MotionParams) -> Sim {
    let mut world = World::new();
    world.register::<Body>().unwrap();
    world.register::<Wallet>().unwrap();
    for i in 0..AVATARS {
        let x = (i % 6) as f32 * 3.0;
        let z = (i / 6) as f32 * 3.0;
        world
            .spawn((
                Body(MotionState::at_rest(Vec3::new(x, 0.0, z), Angle16(0))),
                Wallet(100),
            ))
            .unwrap();
    }
    world
        .insert_resource(PendingMoves(BoundedVec::with_capacity(256)))
        .unwrap();
    world.insert_resource(Economy::default()).unwrap();
    world
        .insert_resource(GraphRuntime::with_capacity(64, 64))
        .unwrap();

    let (catalog, graph) = stipend_catalog();
    let motion = Motion::new(params).unwrap();
    let mut schedule = Schedule::new();

    let mut bodies = world.query::<(Write<Body>,)>().unwrap();
    let access = Access::builder(&mut world)
        .query(&bodies)
        .write_resource::<PendingMoves>()
        .build()
        .unwrap();
    schedule
        .add(
            SystemDesc {
                name: "movement.integrate",
                phase: Phase::Movement,
                priority: 0,
                access,
            },
            move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
                let World {
                    components,
                    resources,
                } = w;
                let pending = resources
                    .get_mut::<PendingMoves>()
                    .ok_or(SystemError::Invariant("pending"))?;
                let dt = c.rate.dt_seconds();
                bodies.for_each(components, |e, (mut body,)| {
                    let input = pending
                        .0
                        .iter()
                        .find(|(pe, _)| *pe == e)
                        .map_or_else(MoveInput::default, |(_, m)| *m);
                    let next = motion.step(&FlatGround(0.0), &body.0, &input, &MotionModifiers::NONE, dt);
                    body.0 = next;
                })?;
                pending.0.clear();
                Ok(())
            },
        )
        .unwrap();

    let mut wallets = world.query::<(Write<Wallet>,)>().unwrap();
    let access = Access::builder(&mut world).query(&wallets).build().unwrap();
    schedule
        .add(
            SystemDesc {
                name: "combat.bonus_rolls",
                phase: Phase::Combat,
                priority: 0,
                access,
            },
            move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
                wallets.for_each(&mut w.components, |e, (mut wallet,)| {
                    if Rng::for_entity(c.seed, c.tick, e, BONUS).chance(1, 20) {
                        wallet.0 += 1;
                    }
                })?;
                Ok(())
            },
        )
        .unwrap();

    let access = Access::builder(&mut world)
        .write::<Wallet>()
        .write_resource::<GraphRuntime>()
        .build()
        .unwrap();
    schedule
        .add(
            SystemDesc {
                name: "effects.graphs",
                phase: Phase::Effects,
                priority: 0,
                access,
            },
            move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
                let World {
                    components,
                    resources,
                } = w;
                let runtime = resources
                    .get_mut::<GraphRuntime>()
                    .ok_or(SystemError::Invariant("runtime"))?;
                if c.tick.get().is_multiple_of(7) {
                    let who = EntityId::new((c.tick.get() % u64::from(AVATARS)) as u32, 0);
                    let _ = runtime.start(&catalog, graph, who, None, c.tick);
                }
                let mut handler = WalletHandler { comps: components };
                let report = runtime.evaluate(&catalog, c.tick, c.seed, &mut handler);
                if report.failed > 0 {
                    return Err(SystemError::Invariant("graph failed"));
                }
                Ok(())
            },
        )
        .unwrap();

    Sim {
        world,
        schedule,
        rate: TickRate::HZ_30,
        seed,
        derived: Vec::new(),
    }
}

impl Sim {
    /// Executes an economy command on delivery (Inbound), deriving its
    /// outcome synchronously, as the host does before acknowledging it.
    fn execute(&mut self, cmd: &Grant) -> GrantOutcome {
        let duplicate = self
            .world
            .resource::<Economy>()
            .is_some_and(|e| e.applied.contains(&cmd.key));
        if duplicate {
            return GrantOutcome::Duplicate { key: cmd.key };
        }
        let Some(mut wallet) = self.world.get_mut::<Wallet>(cmd.entity) else {
            return GrantOutcome::Rejected { key: cmd.key };
        };
        if wallet.0 + cmd.amount < 0 {
            return GrantOutcome::Rejected { key: cmd.key };
        }
        wallet.0 += cmd.amount;
        let balance = wallet.0;
        if let Some(e) = self.world.resource_mut::<Economy>() {
            e.applied.insert(cmd.key);
        }
        GrantOutcome::Applied {
            key: cmd.key,
            balance,
        }
    }
}

impl Replayable for Sim {
    type Schema = Schema;
    type Error = SimError;

    fn apply_intent(
        &mut self,
        _tick: Tick,
        _session: SessionId,
        intent: &MoveIntent,
    ) -> Result<(), SimError> {
        if !self.world.is_alive(intent.entity) {
            return Err(SimError::UnknownEntity);
        }
        let pending = self
            .world
            .resource_mut::<PendingMoves>()
            .ok_or(SimError::UnknownEntity)?;
        pending
            .0
            .push((intent.entity, intent.input))
            .map_err(|_| SimError::System("inbox full".into()))
    }

    fn apply_seed(&mut self, _tick: Tick, seed: u64) -> Result<(), SimError> {
        if let Some(e) = self.world.resource_mut::<Economy>() {
            e.entropy ^= seed;
        }
        Ok(())
    }

    fn apply_command(&mut self, _tick: Tick, command: &Grant) -> Result<(), SimError> {
        let outcome = self.execute(command);
        self.derived.push(outcome);
        Ok(())
    }

    fn check_outcome(&mut self, _tick: Tick, outcome: &GrantOutcome) -> Result<(), SimError> {
        if self.derived.is_empty() || self.derived.remove(0) != *outcome {
            return Err(SimError::OutcomeMismatch);
        }
        Ok(())
    }

    fn step(&mut self, tick: Tick) -> Result<(), SimError> {
        let ctx = TickContext {
            tick,
            rate: self.rate,
            seed: self.seed,
        };
        self.schedule
            .run_tick(&mut self.world, &ctx)
            .map_err(|e| SimError::System(e.to_string()))
    }

    fn state_hash(&self) -> u64 {
        self.world.state_hash()
    }
}

// ---- recording ----------------------------------------------------------------

const BUILD: BuildId = BuildId([0x5A; 32]);
const SEED: Seed = Seed(0xC0FF_EE00);
const START: Tick = Tick(1000);
const TICKS: u64 = 240;

fn content() -> ContentHash {
    ContentHash::of(b"test.replay.content.v1")
}

fn header() -> LogHeader {
    LogHeader {
        build: BUILD,
        content: content(),
        cell: CellId(1),
        seed: SEED,
        start_tick: START,
    }
}

/// Runs the simulation for `TICKS` ticks with driver-generated traffic and
/// records it. The driver's randomness is outside the simulation; only what
/// it delivers is logged.
fn record() -> (Vec<u8>, Vec<u64>) {
    let mut sim = build_sim(SEED, MotionParams::DEFAULT);
    let mut log = LogWriter::<Schema, Vec<u8>>::create(Vec::new(), &header(), 1 << 16).unwrap();
    let mut traffic = Rng::for_cell(Seed(0xD21E), Tick(0), Salt::named("test.driver"));
    let mut hashes = Vec::new();
    let mut seq = 0u32;
    for t in START.get()..START.get() + TICKS {
        let tick = Tick(t);
        for i in 0..AVATARS {
            if traffic.chance(2, 3) {
                seq += 1;
                let bits = (traffic.below(64)) as u16 & !MoveButtons::JUMP.bits();
                let intent = MoveIntent {
                    entity: EntityId::new(i, 0),
                    input: MoveInput {
                        seq: InputSeq(seq),
                        tick,
                        buttons: MoveButtons::from_bits(bits).unwrap(),
                        yaw: Angle16(traffic.below(65_536) as u16),
                        ..MoveInput::default()
                    },
                };
                log.append_intent(tick, SessionId(u64::from(i)), &intent).unwrap();
                sim.apply_intent(tick, SessionId(u64::from(i)), &intent).unwrap();
            }
        }
        if t % 50 == 0 {
            let s = traffic.next_u64();
            log.append_seed(tick, s).unwrap();
            sim.apply_seed(tick, s).unwrap();
        }
        if traffic.chance(1, 4) {
            // Keys repeat on purpose: duplicates must be idempotent.
            let cmd = Grant {
                key: u64::from(traffic.below(40)),
                entity: EntityId::new(traffic.below(AVATARS), 0),
                amount: i64::from(traffic.below(21)) - 10,
            };
            log.append_command(tick, &cmd).unwrap();
            let outcome = sim.execute(&cmd);
            log.append_outcome(tick, &outcome).unwrap();
            assert!(log.commit().unwrap().synced, "outcome durable before the ack");
        }
        sim.step(tick).unwrap();
        let h = sim.state_hash();
        hashes.push(h);
        log.end_tick(tick, h).unwrap();
        log.flush_segment().unwrap();
    }
    assert!(sim.world.resource::<GraphRuntime>().is_some());
    (log.into_sink(), hashes)
}

#[test]
fn recording_is_bitwise_reproducible() {
    let (a, ha) = record();
    let (b, hb) = record();
    assert_eq!(ha, hb, "same inputs, same per-tick hashes");
    assert_eq!(a, b, "same inputs, byte-identical log");
    // The world actually changed over time: hashes are not constant.
    let distinct: BTreeSet<u64> = ha.iter().copied().collect();
    assert!(distinct.len() > (TICKS as usize) / 2);
}

#[test]
fn replay_matches_every_tick() {
    let (bytes, hashes) = record();
    let mut reader = LogReader::<Schema>::open(&bytes, BUILD, content()).unwrap();
    let mut sim = build_sim(reader.header().seed, MotionParams::DEFAULT);
    let report = replay(&mut sim, &mut reader).unwrap();
    assert_eq!(report.ticks, TICKS);
    assert_eq!(report.last_tick, Some(Tick(START.get() + TICKS - 1)));
    assert!(!report.truncated_tail);
    assert_eq!(report.trailing_records, 0);
    assert_eq!(sim.state_hash(), *hashes.last().unwrap());
}

#[test]
fn a_changed_rule_diverges_at_the_first_affected_tick() {
    let (bytes, _) = record();
    let mut reader = LogReader::<Schema>::open(&bytes, BUILD, content()).unwrap();
    let mut tuned = MotionParams::DEFAULT;
    tuned.run_speed = f32::from_bits(MotionParams::DEFAULT.run_speed.to_bits() + 1); // one ulp
    let mut sim = build_sim(SEED, tuned);
    match replay(&mut sim, &mut reader) {
        Err(ReplayError::Divergence {
            tick,
            expected,
            actual,
        }) => {
            assert_eq!(tick, START, "the first tick with movement");
            assert_ne!(expected, actual);
        }
        other => panic!("expected divergence, got {other:?}"),
    }
}

#[test]
fn a_different_seed_diverges() {
    let (bytes, _) = record();
    let mut reader = LogReader::<Schema>::open(&bytes, BUILD, content()).unwrap();
    let mut sim = build_sim(Seed(SEED.0 + 1), MotionParams::DEFAULT);
    assert!(matches!(
        replay(&mut sim, &mut reader),
        Err(ReplayError::Divergence { .. })
    ));
}

#[test]
fn foreign_logs_are_refused() {
    let (bytes, _) = record();
    assert!(matches!(
        LogReader::<Schema>::open(&bytes, BuildId([0; 32]), content()),
        Err(LogError::BuildMismatch { .. })
    ));
    assert!(matches!(
        LogReader::<Schema>::open(&bytes, BUILD, ContentHash::of(b"other content")),
        Err(LogError::ContentMismatch { .. })
    ));
}

#[test]
fn corruption_is_an_error_and_truncation_a_clean_stop() {
    let (bytes, _) = record();
    let mut bad = bytes.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x10;
    let mut reader = LogReader::<Schema>::open(&bad, BUILD, content()).unwrap();
    let mut sim = build_sim(SEED, MotionParams::DEFAULT);
    assert!(matches!(
        replay(&mut sim, &mut reader),
        Err(ReplayError::Log(
            LogError::Corrupt { .. } | LogError::TruncatedTail { .. }
        )) | Ok(ReplayReport {
            truncated_tail: true,
            ..
        })
    ));

    // A crash mid-write: the last record is cut short.
    let cut = &bytes[..bytes.len() - 5];
    let mut reader = LogReader::<Schema>::open(cut, BUILD, content()).unwrap();
    let mut sim = build_sim(SEED, MotionParams::DEFAULT);
    let report = replay(&mut sim, &mut reader).unwrap();
    assert!(report.truncated_tail);
    assert_eq!(report.ticks, TICKS - 1, "every complete tick verified");
}

#[test]
fn tampered_outcome_is_caught() {
    // Record a single tick with one command, but log a wrong outcome.
    let mut sim = build_sim(SEED, MotionParams::DEFAULT);
    let mut log = LogWriter::<Schema, Vec<u8>>::create(Vec::new(), &header(), 1024).unwrap();
    let cmd = Grant {
        key: 1,
        entity: EntityId::new(0, 0),
        amount: 5,
    };
    log.append_command(START, &cmd).unwrap();
    let real = sim.execute(&cmd);
    assert_eq!(real, GrantOutcome::Applied { key: 1, balance: 105 });
    log.append_outcome(START, &GrantOutcome::Applied { key: 1, balance: 999 })
        .unwrap();
    sim.step(START).unwrap();
    log.end_tick(START, sim.state_hash()).unwrap();
    log.flush_segment().unwrap();
    let bytes = log.into_sink();
    let mut reader = LogReader::<Schema>::open(&bytes, BUILD, content()).unwrap();
    let mut fresh = build_sim(SEED, MotionParams::DEFAULT);
    assert_eq!(
        replay(&mut fresh, &mut reader),
        Err(ReplayError::Sim {
            tick: START,
            error: SimError::OutcomeMismatch
        })
    );
}
