//! Zero engine allocation on the hot path (CLAUDE.md rule 4, plan 6.3).
//!
//! Builds a 500-entity world with systems in every hot phase, warms it up,
//! then runs each phase from `Movement` through `Outbound` inside
//! `assert_no_alloc`. Also checks every core primitive meant for the hot path:
//! queries, component access, change ticks, structural changes within
//! reserved capacity, RNG streams, deterministic math, the tick arena, pools,
//! bounded vectors, and state hashing.

// Test code: helper functions may unwrap and cast freely; the library rules
// (no unwrap, checked casts) apply to library code.
#![expect(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use mantis_core::ecs::{Access, Component, EntityId, Query, Read, Resource, World, Write};
use mantis_core::graph::{
    ActionCall, ActionError, ActionHandler, ActionParams, GameplayGraph, GraphCatalog, GraphId, GraphRuntime,
    MarkerSpec, Node, NodeKey, NodeKind, Target,
};
use mantis_core::hash::StableHasher;
use mantis_core::kinematics::{
    Angle16, GroundQuery, Heightfield, InputSeq, Motion, MotionModifiers, MotionParams, MotionState,
    MoveButtons, MoveInput,
};
use mantis_core::math::{self, Vec3};
use mantis_core::mem::{BoundedVec, Pool, TickArena};
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::schedule::{Phase, Schedule, SystemDesc, SystemError, TickContext};
use mantis_core::time::{Tick, TickRate};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Position(Vec3);
mantis_core::impl_state_hash!(Position { 0 });
impl Component for Position {
    const NAME: &'static str = "test.position";
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Velocity(Vec3);
mantis_core::impl_state_hash!(Velocity { 0 });
impl Component for Velocity {
    const NAME: &'static str = "test.velocity";
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Health(u32);
mantis_core::impl_state_hash!(Health { 0 });
impl Component for Health {
    const NAME: &'static str = "test.health";
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Marked;
mantis_core::impl_state_hash!(Marked {});
impl Component for Marked {
    const NAME: &'static str = "test.marked";
}

/// Per-tick scratch the systems use: pre-sized at startup.
struct Scratch {
    hits: BoundedVec<EntityId>,
    arena: TickArena<u32>,
}
// Per-tick scratch, rebuilt every tick: not simulation state.
impl mantis_core::hash::StateHash for Scratch {
    fn state_hash(&self, _h: &mut StableHasher) {}
}
impl Resource for Scratch {
    const NAME: &'static str = "test.scratch";
}

const CRIT: Salt = Salt::named("test.combat.crit");
const ENTITIES: u32 = 500;

fn ctx(tick: u64) -> TickContext {
    TickContext {
        tick: Tick(tick),
        rate: TickRate::HZ_30,
        seed: Seed(0x5EED),
    }
}

fn desc(name: &'static str, phase: Phase, access: Access) -> SystemDesc {
    SystemDesc {
        name,
        phase,
        priority: 0,
        access,
    }
}

fn build() -> (World, Schedule) {
    let mut w = World::new();
    w.register::<Position>().unwrap();
    w.register::<Velocity>().unwrap();
    w.register::<Health>().unwrap();
    w.register::<Marked>().unwrap();
    w.components
        .reserve::<(Position, Velocity, Health)>(ENTITIES as usize + 64)
        .unwrap();
    w.components
        .reserve::<(Position, Velocity, Health, Marked)>(ENTITIES as usize + 64)
        .unwrap();
    for i in 0..ENTITIES {
        let f = i as f32;
        w.spawn((
            Position(Vec3::new(f, 0.0, -f)),
            Velocity(Vec3::new(1.0, 0.0, 0.5)),
            Health(100),
        ))
        .unwrap();
    }
    w.insert_resource(Scratch {
        hits: BoundedVec::with_capacity(ENTITIES as usize),
        arena: TickArena::with_capacity(4096),
    })
    .unwrap();

    let mut s = Schedule::new();

    // Movement: integrate with deterministic math.
    let mut mv: Query<(Write<Position>, Read<Velocity>)> = w.query().unwrap();
    let access = Access::builder(&mut w).query(&mv).build().unwrap();
    s.add(
        desc("movement.integrate", Phase::Movement, access),
        move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
            let dt = c.rate.dt_seconds();
            mv.for_each(&mut w.components, |_, (mut p, v)| {
                let (sn, cs) = math::sin_cos(p.0.x * 0.01);
                p.0 += v.0 * dt + Vec3::new(sn, 0.0, cs) * 1.0e-3;
                p.0.y = math::sqrt(p.0.x.abs()) * 0.0;
            })?;
            Ok(())
        },
    )
    .unwrap();

    // Combat: per-entity RNG streams, hits into a bounded scratch list.
    let mut hp: Query<(Write<Health>,)> = w.query().unwrap();
    let access = Access::builder(&mut w)
        .query(&hp)
        .write_resource::<Scratch>()
        .build()
        .unwrap();
    s.add(
        desc("combat.resolve", Phase::Combat, access),
        move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
            let scratch = w
                .resources
                .get_mut::<Scratch>()
                .ok_or(SystemError::Invariant("scratch"))?;
            scratch.hits.clear();
            hp.for_each(&mut w.components, |e, (mut h,)| {
                let mut rng = Rng::for_entity(c.seed, c.tick, e, CRIT);
                if rng.chance(1, 10) {
                    h.0 = h.0.saturating_sub(1);
                    let _ = scratch.hits.push(e);
                }
            })?;
            Ok(())
        },
    )
    .unwrap();

    // Effects: structural changes within reserved capacity (archetype moves
    // along cached edges) plus a despawn/spawn pair.
    let access = Access::builder(&mut w)
        .write::<Marked>()
        .read_resource::<Scratch>()
        .build()
        .unwrap();
    s.add(
        desc("effects.mark", Phase::Effects, access),
        move |w: &mut World, c: &TickContext| -> Result<(), SystemError> {
            let World {
                components,
                resources,
            } = w;
            let scratch = resources
                .get::<Scratch>()
                .ok_or(SystemError::Invariant("scratch"))?;
            for e in scratch.hits.iter().take(8) {
                if components.get::<Marked>(*e).is_some() {
                    components.remove::<Marked>(*e)?;
                } else {
                    components.insert(*e, Marked)?;
                }
            }
            if let Some(first) = scratch.hits.first() {
                components.despawn(*first)?;
                let f = c.tick.get() as f32;
                components.spawn((Position(Vec3::new(f, 0.0, f)), Velocity(Vec3::X), Health(100)))?;
            }
            Ok(())
        },
    )
    .unwrap();

    // AI: arena scratch with reset.
    let access = Access::builder(&mut w)
        .write_resource::<Scratch>()
        .build()
        .unwrap();
    s.add(
        desc("ai.think", Phase::Ai, access),
        move |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
            let scratch = w
                .resources
                .get_mut::<Scratch>()
                .ok_or(SystemError::Invariant("scratch"))?;
            scratch.arena.reset();
            let r = scratch
                .arena
                .alloc(7)
                .map_err(|_| SystemError::Invariant("arena full"))?;
            let s2 = scratch
                .arena
                .alloc_slice_fill(16, 1)
                .map_err(|_| SystemError::Invariant("arena full"))?;
            let total: u32 = scratch.arena.slice(s2).map_or(0, |s| s.iter().sum());
            if scratch.arena.get(r).copied() != Some(7) || total != 16 {
                return Err(SystemError::Invariant("arena contents"));
            }
            Ok(())
        },
    )
    .unwrap();

    // Interest and Outbound: read-only, as the per-client jobs will be.
    let mut near: Query<(Read<Position>,)> = w.query().unwrap();
    let access = Access::builder(&mut w).query(&near).build().unwrap();
    s.add(
        desc("interest.scan", Phase::Interest, access),
        move |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
            let mut count = 0u32;
            near.for_each(&mut w.components, |_, (p,)| {
                if p.0.length_squared() < 1.0e6 {
                    count += 1;
                }
            })?;
            if count == 0 {
                return Err(SystemError::Invariant("nobody near"));
            }
            Ok(())
        },
    )
    .unwrap();

    let mut enc: Query<(Read<Position>, Read<Health>)> = w.query().unwrap();
    let access = Access::builder(&mut w).query(&enc).build().unwrap();
    s.add(
        desc("outbound.encode", Phase::Outbound, access),
        move |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
            let mut h = StableHasher::new();
            enc.for_each(&mut w.components, |e, (p, hp)| {
                h.write_u64(e.to_bits());
                h.write_f32(math::atan2(p.0.z, p.0.x));
                h.write_u32(hp.0);
            })?;
            let mut whole = StableHasher::new();
            w.components.state_hash(&mut whole);
            let _ = (h.finish(), whole.finish());
            Ok(())
        },
    )
    .unwrap();

    (w, s)
}

#[test]
fn hot_phases_allocate_nothing() {
    let (mut w, mut s) = build();
    // Warm-up: create every archetype and edge the systems will use.
    for t in 1..=30 {
        s.run_tick(&mut w, &ctx(t)).unwrap();
    }
    for t in 31..=120u64 {
        let c = ctx(t);
        w.components.set_change_tick(c.tick);
        s.run_phase(Phase::Inbound, &mut w, &c).unwrap();
        s.run_phase(Phase::Timers, &mut w, &c).unwrap();
        for phase in Phase::ALL.into_iter().filter(|p| p.is_hot()) {
            assert_no_alloc(phase.name(), || s.run_phase(phase, &mut w, &c)).unwrap();
        }
        s.run_phase(Phase::Persist, &mut w, &c).unwrap();
    }
    assert_eq!(w.components.len(), ENTITIES);
}

#[test]
fn whole_tick_allocates_nothing_after_warm_up() {
    let (mut w, mut s) = build();
    for t in 1..=30 {
        s.run_tick(&mut w, &ctx(t)).unwrap();
    }
    for t in 31..=60 {
        assert_no_alloc("run_tick", || s.run_tick(&mut w, &ctx(t))).unwrap();
    }
}

#[test]
fn primitives_allocate_nothing() {
    let mut pool: Pool<Vec<u8>> = Pool::new(4, || Vec::with_capacity(1500));
    let mut arena: TickArena<u8> = TickArena::with_capacity(1 << 16);
    let mut list: BoundedVec<u32> = BoundedVec::with_capacity(64);
    let sum = assert_no_alloc("primitives", || {
        let mut acc = 0u64;
        for i in 0..100u32 {
            let h = pool.acquire().unwrap();
            pool.get_mut(h).unwrap().extend_from_slice(&[1, 2, 3, 4]);
            acc += pool.get(h).map_or(0, |b| b.len() as u64);
            assert!(pool.release(h));
            let s = arena.alloc_slice_copy(&[i as u8; 32]).unwrap();
            acc += arena.slice(s).map_or(0, |b| u64::from(b[0]));
            if i % 10 == 0 {
                arena.reset();
            }
            list.clear();
            for j in 0..64 {
                list.push(j).unwrap();
            }
            assert!(list.push(65).is_err());
            let mut rng = Rng::for_cell(Seed(1), Tick(u64::from(i)), Salt::named("x"));
            acc += u64::from(rng.below(10));
            acc += u64::from(math::pow(1.5, 2.0).to_bits() & 1);
            acc += u64::from(math::exp(0.5).to_bits() & 1) + u64::from(math::ln(2.0).to_bits() & 1);
            acc += StableHasher::hash_bytes(b"abcdefghijklmnopqrstuvwxyz0123456789") & 1;
        }
        acc
    });
    assert!(sum > 0);
}

#[test]
fn motion_step_allocates_nothing() {
    let motion = Motion::new(MotionParams::DEFAULT).unwrap();
    let heights: Vec<f32> = (0..32 * 32).map(|i| (i % 7) as f32 * 0.05).collect();
    let ground = Heightfield::new(-16.0, -16.0, 1.0, 32, 32, heights).unwrap();
    let dt = TickRate::HZ_30.dt_seconds();
    let mut state = MotionState::at_rest(
        Vec3::new(0.0, ground.height_at(0.0, 0.0).unwrap(), 0.0),
        Angle16(0),
    );
    let end = assert_no_alloc("Motion::step", || {
        for n in 0..600u32 {
            let input = MoveInput {
                seq: InputSeq(n),
                tick: Tick(u64::from(n)),
                buttons: if n % 50 == 0 {
                    MoveButtons::FORWARD.with(MoveButtons::JUMP)
                } else {
                    MoveButtons::FORWARD
                },
                yaw: Angle16((n * 97) as u16),
                ..MoveInput::default()
            };
            state = motion.step(&ground, &state, &input, &MotionModifiers::NONE, dt);
        }
        state
    });
    assert!(end.position.is_finite());
}

struct CountingHandler(u64);
impl ActionHandler for CountingHandler {
    fn apply(&mut self, call: &ActionCall) -> Result<(), ActionError> {
        self.0 += u64::from(call.params.0[0].unsigned_abs());
        Ok(())
    }
}

#[test]
fn graph_evaluation_and_world_hash_allocate_nothing() {
    let mut catalog = GraphCatalog::new();
    let act = catalog.register_action("test.tick_damage").unwrap();
    let id = GraphId::named("test.dot");
    let node = |key, kind| Node {
        key: NodeKey(key),
        kind,
    };
    catalog
        .insert(GameplayGraph {
            id,
            entry: NodeKey(1),
            nodes: vec![
                node(
                    1,
                    NodeKind::Marker {
                        marker: MarkerSpec::CastStart,
                        offset: 3,
                        next: Some(NodeKey(2)),
                    },
                ),
                node(
                    2,
                    NodeKind::Repeat {
                        counter: 0,
                        times: 5,
                        body: NodeKey(3),
                        done: Some(NodeKey(6)),
                    },
                ),
                node(
                    3,
                    NodeKind::Chance {
                        numerator: 1,
                        denominator: 2,
                        then: Some(NodeKey(4)),
                        otherwise: Some(NodeKey(5)),
                    },
                ),
                node(
                    4,
                    NodeKind::Action {
                        action: act,
                        target: Target::Target,
                        params: ActionParams([3, 0, 0, 0]),
                        next: Some(NodeKey(5)),
                    },
                ),
                node(
                    5,
                    NodeKind::Marker {
                        marker: MarkerSpec::Tick { counter: 0 },
                        offset: 0,
                        next: Some(NodeKey(7)),
                    },
                ),
                node(
                    7,
                    NodeKind::Delay {
                        ticks: 2,
                        next: Some(NodeKey(2)),
                    },
                ),
                node(
                    6,
                    NodeKind::Marker {
                        marker: MarkerSpec::Expire,
                        offset: 0,
                        next: None,
                    },
                ),
            ],
        })
        .unwrap();
    let mut rt = GraphRuntime::with_capacity(128, 512);
    let mut handler = CountingHandler(0);
    let (w, _) = build();
    let total = assert_no_alloc("graph evaluation", || {
        let mut markers = 0usize;
        for t in 0..200u64 {
            if t % 3 == 0 {
                let src = EntityId::new((t % 50) as u32, 0);
                let _ = rt.start(&catalog, id, src, Some(EntityId::new(1, 0)), Tick(t));
            }
            let report = rt.evaluate(&catalog, Tick(t), Seed(5), &mut handler);
            assert_eq!(report.failed, 0);
            markers += rt.markers().len();
        }
        let _ = w.state_hash();
        markers
    });
    assert!(total > 0);
    assert!(handler.0 > 0);
}

/// The harness is real: the same test binary does detect an allocation.
#[test]
fn harness_detects_allocation_here() {
    let ((), stats) = count_allocs(|| {
        let mut w = World::new();
        w.register::<Health>().unwrap();
    });
    assert!(stats.allocs > 0);
}
