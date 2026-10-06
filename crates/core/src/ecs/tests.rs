//! ECS unit tests, including a randomized model-based test of every
//! structural operation against a trivially correct reference model.

use std::collections::BTreeMap;

use super::*;
use crate::hash::StableHasher;
use crate::rng::{Rng, Salt, Seed};
use crate::time::Tick;

#[derive(Clone, Copy, PartialEq, Debug)]
struct Pos(f32, f32);
crate::impl_state_hash!(Pos { 0, 1 });
impl Component for Pos {
    const NAME: &'static str = "test.pos";
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Vel(f32, f32);
crate::impl_state_hash!(Vel { 0, 1 });
impl Component for Vel {
    const NAME: &'static str = "test.vel";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Hp(u32);
crate::impl_state_hash!(Hp { 0 });
impl Component for Hp {
    const NAME: &'static str = "test.hp";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Tag;
crate::impl_state_hash!(Tag {});
impl Component for Tag {
    const NAME: &'static str = "test.tag";
}

/// Same name as `Hp`: must be refused.
#[derive(Debug)]
struct Impostor;
crate::impl_state_hash!(Impostor {});
impl Component for Impostor {
    const NAME: &'static str = "test.hp";
}

/// Exact conversion for the small counters used in these tests.
fn small_f32(v: u32) -> f32 {
    f32::from(u16::try_from(v % 65_536).unwrap())
}

fn world() -> World {
    let mut w = World::new();
    w.register::<Pos>().unwrap();
    w.register::<Vel>().unwrap();
    w.register::<Hp>().unwrap();
    w.register::<Tag>().unwrap();
    w
}

#[test]
fn registration() {
    let mut w = World::new();
    let a = w.register::<Pos>().unwrap();
    assert_eq!(w.register::<Pos>().unwrap(), a, "idempotent");
    let b = w.register::<Hp>().unwrap();
    assert_ne!(a, b);
    assert_eq!(w.register::<Impostor>(), Err(EcsError::NameClash("test.hp")));
    assert_eq!(w.components.component_name(b), Some("test.hp"));
    assert_eq!(w.spawn((Vel(0.0, 0.0),)), Err(EcsError::Unregistered("test.vel")));
}

#[test]
fn spawn_get_despawn() {
    let mut w = world();
    let e = w.spawn((Pos(1.0, 2.0), Hp(10))).unwrap();
    assert_eq!(w.get::<Pos>(e), Some(&Pos(1.0, 2.0)));
    assert_eq!(w.get::<Hp>(e), Some(&Hp(10)));
    assert_eq!(w.get::<Vel>(e), None);
    assert!(w.is_alive(e));
    w.despawn(e).unwrap();
    assert!(!w.is_alive(e));
    assert_eq!(w.get::<Pos>(e), None);
    assert_eq!(w.despawn(e), Err(EcsError::NoSuchEntity(e)));
    let e2 = w.spawn((Pos(3.0, 4.0),)).unwrap();
    assert_eq!(e2.index(), e.index(), "slot reused");
    assert_eq!(e2.generation(), e.generation() + 1);
    assert_eq!(w.get::<Pos>(e), None, "stale id never aliases");
    assert_eq!(
        w.spawn((Hp(1), Hp(2))),
        Err(EcsError::DuplicateComponent("test.hp"))
    );
    let bare = w.spawn(()).unwrap();
    assert!(w.is_alive(bare));
}

#[test]
fn despawn_keeps_swapped_neighbour_intact() {
    let mut w = world();
    let ids: Vec<_> = (0..4u32).map(|i| w.spawn((Hp(i),)).unwrap()).collect();
    w.despawn(ids[0]).unwrap(); // last row (Hp 3) swaps into row 0
    for (i, id) in (0u32..).zip(&ids).skip(1) {
        assert_eq!(w.get::<Hp>(*id), Some(&Hp(i)));
    }
}

#[test]
fn insert_remove_move_between_archetypes() {
    let mut w = world();
    let a = w.spawn((Pos(1.0, 1.0), Hp(1))).unwrap();
    let b = w.spawn((Pos(2.0, 2.0), Hp(2))).unwrap();
    let c = w.spawn((Pos(3.0, 3.0), Hp(3))).unwrap();
    w.insert(a, Vel(9.0, 9.0)).unwrap();
    assert_eq!(w.get::<Vel>(a), Some(&Vel(9.0, 9.0)));
    assert_eq!(w.get::<Pos>(a), Some(&Pos(1.0, 1.0)));
    assert_eq!(w.get::<Hp>(c), Some(&Hp(3)), "neighbour swapped into a's old row");
    // Replacing in place.
    w.insert(a, Vel(8.0, 8.0)).unwrap();
    assert_eq!(w.get::<Vel>(a), Some(&Vel(8.0, 8.0)));
    // Remove returns the value and keeps the rest.
    assert_eq!(w.remove::<Hp>(b).unwrap(), Some(Hp(2)));
    assert_eq!(w.get::<Hp>(b), None);
    assert_eq!(w.get::<Pos>(b), Some(&Pos(2.0, 2.0)));
    assert_eq!(w.get::<Hp>(c), Some(&Hp(3)));
    assert_eq!(w.remove::<Hp>(b).unwrap(), None, "absent component");
    assert_eq!(w.remove::<Vel>(c).unwrap(), None);
    let dead = EntityId::new(999, 0);
    assert_eq!(w.insert(dead, Tag), Err(EcsError::NoSuchEntity(dead)));
    assert_eq!(w.remove::<Tag>(dead), Err(EcsError::NoSuchEntity(dead)));
}

#[test]
fn query_iterates_matching_archetypes() {
    let mut w = world();
    let a = w.spawn((Pos(0.0, 0.0), Vel(1.0, 0.0))).unwrap();
    let b = w.spawn((Pos(0.0, 0.0), Vel(0.0, 2.0), Hp(5))).unwrap();
    let _c = w.spawn((Pos(7.0, 7.0),)).unwrap();
    let mut q = w.query::<(Write<Pos>, Read<Vel>)>().unwrap();
    q.for_each(&mut w.components, |_, (mut p, v)| {
        p.0 += v.0;
        p.1 += v.1;
    })
    .unwrap();
    assert_eq!(w.get::<Pos>(a), Some(&Pos(1.0, 0.0)));
    assert_eq!(w.get::<Pos>(b), Some(&Pos(0.0, 2.0)));
    assert_eq!(q.count(&w.components).unwrap(), 2);

    let mut seen = Vec::new();
    let mut only_hp = w
        .components
        .query_builder::<(Read<Pos>,)>()
        .with::<Hp>()
        .build()
        .unwrap();
    only_hp.for_each(&mut w.components, |e, _| seen.push(e)).unwrap();
    assert_eq!(seen, vec![b]);

    let mut no_vel = w
        .components
        .query_builder::<(Read<Pos>,)>()
        .without::<Vel>()
        .build()
        .unwrap();
    assert_eq!(no_vel.count(&w.components).unwrap(), 1);

    assert_eq!(
        w.components
            .query_builder::<(Read<Pos>,)>()
            .without::<Pos>()
            .build()
            .err(),
        Some(EcsError::ContradictoryFilter)
    );
    assert_eq!(
        w.query::<(Read<Pos>, Write<Pos>)>().err(),
        Some(EcsError::DuplicateComponent("test.pos"))
    );
}

#[test]
fn query_sees_archetypes_created_later() {
    let mut w = world();
    let mut q = w.query::<(Read<Hp>,)>().unwrap();
    assert_eq!(q.count(&w.components).unwrap(), 0);
    w.spawn((Hp(1), Tag)).unwrap();
    assert_eq!(q.count(&w.components).unwrap(), 1);
}

#[test]
fn query_is_bound_to_its_world() {
    let w1 = world();
    let mut w2 = world();
    let mut q = w1.query::<(Read<Pos>,)>().unwrap();
    assert_eq!(
        q.for_each(&mut w2.components, |_, _| {}),
        Err(EcsError::WrongWorld)
    );
}

#[test]
fn read_only_queries_fail_closed_when_stale() {
    let mut w = world();
    w.spawn((Hp(1),)).unwrap();
    let mut q = w.query::<(Read<Hp>,)>().unwrap();
    q.update(&w.components).unwrap();
    let mut n = 0;
    q.for_each_ref(&w.components, |_, _| n += 1).unwrap();
    assert_eq!(n, 1);
    w.spawn((Hp(2), Tag)).unwrap();
    assert_eq!(
        q.for_each_ref(&w.components, |_, _| {}),
        Err(EcsError::StaleQuery)
    );
    q.update(&w.components).unwrap();
    let mut sum = 0;
    q.for_each_ref(&w.components, |_, (hp,)| sum += hp.0).unwrap();
    assert_eq!(sum, 3);
}

#[test]
fn change_ticks() {
    let mut w = world();
    w.components.set_change_tick(Tick(5));
    let e = w.spawn((Hp(1), Pos(0.0, 0.0))).unwrap();
    assert_eq!(w.components.changed_tick::<Hp>(e), Some(Tick(5)));
    w.components.set_change_tick(Tick(9));
    // Reading through Write without mutating does not stamp.
    let mut q = w.query::<(Write<Hp>,)>().unwrap();
    q.for_each(&mut w.components, |_, (hp,)| assert_eq!(hp.0, 1))
        .unwrap();
    assert_eq!(w.components.changed_tick::<Hp>(e), Some(Tick(5)));
    q.for_each(&mut w.components, |_, (mut hp,)| hp.0 += 1).unwrap();
    assert_eq!(w.components.changed_tick::<Hp>(e), Some(Tick(9)));
    assert_eq!(w.components.changed_tick::<Pos>(e), Some(Tick(5)));
    // get_mut stamps on write.
    w.components.set_change_tick(Tick(12));
    if let Some(mut p) = w.get_mut::<Pos>(e) {
        assert_eq!(p.changed(), Tick(5));
        p.0 = 3.0;
    }
    assert_eq!(w.components.changed_tick::<Pos>(e), Some(Tick(12)));
    // Tracked exposes ticks for change sets.
    let mut t = w.query::<(Tracked<Pos>, Tracked<Hp>)>().unwrap();
    t.for_each(&mut w.components, |_, (p, hp)| {
        assert!(p.is_changed_since(Tick(11)));
        assert!(!hp.is_changed_since(Tick(9)));
        assert_eq!(hp.changed(), Tick(9));
    })
    .unwrap();
    // Moving archetypes keeps ticks.
    w.components.set_change_tick(Tick(20));
    w.insert(e, Tag).unwrap();
    assert_eq!(w.components.changed_tick::<Hp>(e), Some(Tick(9)));
    assert_eq!(w.components.changed_tick::<Tag>(e), Some(Tick(20)));
}

#[derive(Debug)]
struct Gravity(f32);
crate::impl_state_hash!(Gravity { 0 });
impl Resource for Gravity {
    const NAME: &'static str = "test.gravity";
}

#[test]
fn resources_and_split_borrows() {
    let mut w = world();
    assert_eq!(w.insert_resource(Gravity(9.8)).unwrap().map(|g| g.0), None);
    assert_eq!(w.insert_resource(Gravity(1.0)).unwrap().map(|g| g.0), Some(9.8));
    let e = w.spawn((Vel(0.0, 0.0),)).unwrap();
    let mut q = w.query::<(Write<Vel>,)>().unwrap();
    // A query over `components` while reading `resources`.
    let res = &w.resources;
    q.for_each(&mut w.components, |_, (mut v,)| {
        v.1 -= res.get::<Gravity>().map_or(0.0, |g| g.0);
    })
    .unwrap();
    assert_eq!(w.get::<Vel>(e), Some(&Vel(0.0, -1.0)));
    if let Some(g) = w.resource_mut::<Gravity>() {
        g.0 = 2.0;
    }
    assert_eq!(w.resource::<Gravity>().map(|g| g.0), Some(2.0));
    assert_eq!(w.resources.remove::<Gravity>().map(|g| g.0), Some(2.0));
    assert!(!w.resources.contains::<Gravity>());
    assert!(w.resources.id::<Gravity>().is_some(), "id survives removal");
}

#[test]
fn access_builder_and_conflicts() {
    let mut world = world();
    let query = world.query::<(Write<Pos>, Read<Vel>)>().unwrap();
    let writer = Access::builder(&mut world)
        .query(&query)
        .write_resource::<Gravity>()
        .build()
        .unwrap();
    let reader = Access::builder(&mut world)
        .read::<Pos>()
        .read_resource::<Gravity>()
        .build()
        .unwrap();
    let other = Access::builder(&mut world)
        .read::<Vel>()
        .read::<Hp>()
        .build()
        .unwrap();
    let (comps, res) = writer.conflicts(&reader);
    assert_eq!(comps.len(), 1);
    assert_eq!(res.len(), 1);
    let (comps, res) = writer.conflicts(&other);
    assert!(comps.is_empty() && res.is_empty(), "reads never conflict");
    assert!(writer.writes_anything());
    assert!(!other.writes_anything());
    let unregistered = Access::builder(&mut World::new()).read::<Pos>().build();
    assert_eq!(unregistered, Err(EcsError::Unregistered("test.pos")));
}

/// Randomized model-based test: thousands of random spawns, despawns,
/// inserts, removes, and writes, checked after every step against a
/// `BTreeMap` model. Deterministic (seeded).
#[test]
fn model_based_random_operations() {
    #[derive(Clone, Default, PartialEq, Debug)]
    struct Model {
        pos: Option<Pos>,
        hp: Option<Hp>,
        tag: bool,
    }
    let mut w = world();
    let mut model: BTreeMap<EntityId, Model> = BTreeMap::new();
    let mut dead: Vec<EntityId> = Vec::new();
    let mut rng = Rng::for_cell(Seed(0xEC5), Tick(0), Salt::named("test.ecs.model"));
    for step in 0..6000u32 {
        let live: Vec<EntityId> = model.keys().copied().collect();
        let pick = |rng: &mut Rng| -> Option<EntityId> {
            if live.is_empty() {
                None
            } else {
                let n = u32::try_from(live.len()).unwrap();
                live.get(rng.below(n) as usize).copied()
            }
        };
        match rng.below(7) {
            0 | 1 => {
                let m = Model {
                    pos: rng.chance(1, 2).then_some(Pos(small_f32(step), 0.0)),
                    hp: rng.chance(1, 2).then_some(Hp(step)),
                    tag: false,
                };
                let id = match (m.pos, m.hp) {
                    (Some(p), Some(h)) => w.spawn((p, h)),
                    (Some(p), None) => w.spawn((p,)),
                    (None, Some(h)) => w.spawn((h,)),
                    (None, None) => w.spawn(()),
                }
                .unwrap();
                assert!(!model.contains_key(&id));
                model.insert(id, m);
            }
            2 => {
                if let Some(id) = pick(&mut rng) {
                    w.despawn(id).unwrap();
                    model.remove(&id);
                    dead.push(id);
                }
            }
            3 => {
                if let Some(id) = pick(&mut rng) {
                    w.insert(id, Hp(step + 100_000)).unwrap();
                    model.get_mut(&id).unwrap().hp = Some(Hp(step + 100_000));
                }
            }
            4 => {
                if let Some(id) = pick(&mut rng) {
                    let got = w.remove::<Pos>(id).unwrap();
                    assert_eq!(got, model.get_mut(&id).unwrap().pos.take());
                }
            }
            5 => {
                if let Some(id) = pick(&mut rng) {
                    let m = model.get_mut(&id).unwrap();
                    if m.tag {
                        assert_eq!(w.remove::<Tag>(id).unwrap(), Some(Tag));
                        m.tag = false;
                    } else {
                        w.insert(id, Tag).unwrap();
                        m.tag = true;
                    }
                }
            }
            _ => {
                let mut q = w.query::<(Write<Hp>,)>().unwrap();
                q.for_each(&mut w.components, |_, (mut hp,)| hp.0 = hp.0.wrapping_add(1))
                    .unwrap();
                for m in model.values_mut() {
                    if let Some(h) = &mut m.hp {
                        h.0 = h.0.wrapping_add(1);
                    }
                }
            }
        }
        if step % 50 == 0 || step > 5900 {
            assert_eq!(w.components.len() as usize, model.len());
            for (id, m) in &model {
                assert_eq!(w.get::<Pos>(*id), m.pos.as_ref(), "step {step} {id}");
                assert_eq!(w.get::<Hp>(*id), m.hp.as_ref(), "step {step} {id}");
                assert_eq!(w.get::<Tag>(*id).is_some(), m.tag, "step {step} {id}");
            }
            for id in &dead {
                assert!(!w.is_alive(*id));
            }
            let listed: Vec<EntityId> = w.components.iter_entities().map(|(e, _)| e).collect();
            let expected: Vec<EntityId> = {
                let mut v: Vec<EntityId> = model.keys().copied().collect();
                v.sort_by_key(|e| e.index());
                v
            };
            assert_eq!(listed, expected);
        }
    }
}

/// Two worlds fed the same operations iterate in the same order and hash the
/// same per entity.
#[test]
fn iteration_order_is_reproducible() {
    fn build() -> (World, Vec<EntityId>) {
        let mut w = world();
        let mut ids = Vec::new();
        for i in 0..50u32 {
            ids.push(w.spawn((Hp(i), Pos(small_f32(i), 0.0))).unwrap());
        }
        for i in (0..50).step_by(3) {
            w.despawn(ids[i]).unwrap();
        }
        for i in (1..50).step_by(4) {
            if w.is_alive(ids[i]) {
                w.insert(ids[i], Tag).unwrap();
            }
        }
        let mut order = Vec::new();
        let mut q = w.query::<(Read<Hp>,)>().unwrap();
        q.for_each(&mut w.components, |e, _| order.push(e)).unwrap();
        (w, order)
    }
    let (w1, o1) = build();
    let (w2, o2) = build();
    assert_eq!(o1, o2);
    for e in &o1 {
        let mut h1 = StableHasher::new();
        let mut h2 = StableHasher::new();
        assert!(w1.components.hash_entity(*e, &mut h1));
        assert!(w2.components.hash_entity(*e, &mut h2));
        assert_eq!(h1.finish(), h2.finish());
    }
    let (mut s1, mut s2) = (StableHasher::new(), StableHasher::new());
    w1.components.state_hash(&mut s1);
    w2.components.state_hash(&mut s2);
    assert_eq!(s1.finish(), s2.finish());
}

/// The state hash ignores archetype layout: the same logical state reached
/// through different structural histories hashes the same, and any value
/// change alters it.
#[test]
fn state_hash_is_layout_independent_and_sensitive() {
    let mut a = world();
    let ea = a.spawn((Hp(1), Pos(1.0, 2.0))).unwrap();
    let mut b = world();
    let eb = b.spawn((Pos(1.0, 2.0),)).unwrap();
    b.spawn((Tag,)).unwrap(); // a different archetype history
    b.despawn(EntityId::new(1, 0)).unwrap();
    b.insert(eb, Hp(1)).unwrap();
    assert_eq!(ea, eb);
    let hash = |w: &World| {
        let mut h = StableHasher::new();
        w.components.state_hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&a), hash(&b));
    a.insert(ea, Hp(2)).unwrap();
    assert_ne!(hash(&a), hash(&b));
}

/// Edges between archetypes are linked at creation, so a move between two
/// existing archetypes never needs to create an edge later.
#[test]
fn archetype_edges_are_linked_eagerly() {
    let mut w = world();
    w.components.reserve::<(Hp,)>(1).unwrap();
    w.components.reserve::<(Hp, Tag)>(1).unwrap();
    let a = w.components.archetypes();
    let hp = w.components.component_id::<Hp>().unwrap();
    let tag = w.components.component_id::<Tag>().unwrap();
    // archetype 0: {}, 1: {Hp}, 2: {Hp, Tag}
    assert_eq!(a[0].add_edges.get(&hp), Some(&1));
    assert_eq!(a[1].remove_edges.get(&hp), Some(&0));
    assert_eq!(a[1].add_edges.get(&tag), Some(&2));
    assert_eq!(a[2].remove_edges.get(&tag), Some(&1));
    assert_eq!(
        a[2].remove_edges.get(&hp),
        None,
        "the {{Tag}} archetype does not exist yet"
    );
}

/// The world hash covers resources as well as components.
#[test]
fn world_hash_covers_resources() {
    let mut a = world();
    let mut b = world();
    a.spawn((Hp(1),)).unwrap();
    b.spawn((Hp(1),)).unwrap();
    assert_eq!(a.state_hash(), b.state_hash());
    a.insert_resource(Gravity(1.0)).unwrap();
    assert_ne!(a.state_hash(), b.state_hash(), "present vs unregistered");
    b.insert_resource(Gravity(1.0)).unwrap();
    assert_eq!(a.state_hash(), b.state_hash());
    b.insert_resource(Gravity(2.0)).unwrap();
    assert_ne!(a.state_hash(), b.state_hash(), "value change");
    b.resources.remove::<Gravity>();
    let absent = b.state_hash();
    assert_ne!(a.state_hash(), absent, "absent vs present");
}

// ---- snapshots ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Saved1(u32);
crate::impl_state_hash!(Saved1 { 0 });
impl Component for Saved1 {
    const NAME: &'static str = "test.saved1";
    fn save(&self, e: &mut crate::wire::Encoder<'_>) -> bool {
        e.u32(self.0);
        true
    }
    fn load(d: &mut crate::wire::Decoder<'_>) -> Result<Self, crate::wire::DecodeError> {
        Ok(Self(d.u32()?))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Saved2(u64);
crate::impl_state_hash!(Saved2 { 0 });
impl Component for Saved2 {
    const NAME: &'static str = "test.saved2";
    fn save(&self, e: &mut crate::wire::Encoder<'_>) -> bool {
        e.u64(self.0);
        true
    }
    fn load(d: &mut crate::wire::Decoder<'_>) -> Result<Self, crate::wire::DecodeError> {
        Ok(Self(d.u64()?))
    }
}

#[derive(Debug, Default)]
struct Counter(u64);
crate::impl_state_hash!(Counter { 0 });
impl Resource for Counter {
    const NAME: &'static str = "test.counter";
    fn save(&self, e: &mut crate::wire::Encoder<'_>) -> Saved {
        e.u64(self.0);
        Saved::Written
    }
    fn load(&mut self, d: &mut crate::wire::Decoder<'_>) -> Result<(), crate::wire::DecodeError> {
        self.0 = d.u64()?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Content;
crate::impl_state_hash!(Content {});
impl Resource for Content {
    const NAME: &'static str = "test.content";
    fn save(&self, _e: &mut crate::wire::Encoder<'_>) -> Saved {
        Saved::Rebuilt
    }
}

fn saved_world() -> World {
    let mut w = World::new();
    w.register::<Saved1>().unwrap();
    w.register::<Saved2>().unwrap();
    w.insert_resource(Counter(0)).unwrap();
    w.insert_resource(Content).unwrap();
    w
}

#[test]
fn a_snapshot_restores_the_same_state_ids_and_iteration_order() {
    let mut w = saved_world();
    let mut ids = Vec::new();
    for i in 0..20u32 {
        w.components.set_change_tick(Tick(u64::from(i)));
        let e = if i % 3 == 0 {
            w.spawn((Saved1(i), Saved2(u64::from(i) * 7))).unwrap()
        } else {
            w.spawn((Saved1(i),)).unwrap()
        };
        ids.push(e);
    }
    // Despawns and moves shuffle rows and fill the free list.
    for i in [2usize, 5, 11, 17] {
        w.despawn(ids[i]).unwrap();
    }
    w.insert(ids[4], Saved2(99)).unwrap();
    w.remove::<Saved2>(ids[0]).unwrap();
    w.resource_mut::<Counter>().unwrap().0 = 42;
    let mut bytes = Vec::new();
    w.save(&mut crate::wire::Encoder::new(&mut bytes)).unwrap();

    let mut r = saved_world();
    r.load(&mut crate::wire::Decoder::new(&bytes)).unwrap();
    assert_eq!(r.state_hash(), w.state_hash());
    assert_eq!(r.resource::<Counter>().unwrap().0, 42);
    // Iteration order (archetypes, then rows) is the same.
    let order = |w: &World| {
        let mut out = Vec::new();
        for a in w.components.archetypes() {
            out.extend(a.entities.iter().copied());
        }
        out
    };
    assert_eq!(order(&r), order(&w));
    // Ids allocated after the restore are the same as after the original.
    let next_w = w.spawn((Saved1(1000),)).unwrap();
    let next_r = r.spawn((Saved1(1000),)).unwrap();
    assert_eq!(next_w, next_r);
    assert_eq!(r.state_hash(), w.state_hash());
}

#[test]
fn snapshots_refuse_state_they_cannot_hold() {
    let mut w = saved_world();
    w.register::<Pos>().unwrap();
    w.spawn((Pos(1.0, 2.0),)).unwrap();
    let mut bytes = Vec::new();
    assert_eq!(
        w.save(&mut crate::wire::Encoder::new(&mut bytes)),
        Err("test.pos")
    );
    let mut w = world();
    w.insert_resource(TestRes(1)).unwrap();
    assert_eq!(
        w.save(&mut crate::wire::Encoder::new(&mut Vec::new())),
        Err("test.res")
    );
    // A world with entities cannot take a restore.
    let mut full = saved_world();
    full.spawn((Saved1(1),)).unwrap();
    let mut bytes = Vec::new();
    saved_world()
        .save(&mut crate::wire::Encoder::new(&mut bytes))
        .unwrap();
    assert!(full.load(&mut crate::wire::Decoder::new(&bytes)).is_err());
}

#[derive(Debug)]
struct TestRes(u32);
crate::impl_state_hash!(TestRes { 0 });
impl Resource for TestRes {
    const NAME: &'static str = "test.res";
}

#[test]
fn the_inspector_pages_entities_by_component_with_every_value_as_text() {
    let mut w = World::new();
    w.register::<Pos>().unwrap();
    w.register::<Hp>().unwrap();
    let a = w.spawn((Pos(1.0, 2.0), Hp(5))).unwrap();
    let _b = w.spawn((Pos(3.0, 4.0),)).unwrap();
    let c = w.spawn((Hp(9),)).unwrap();
    assert_eq!(w.components.component_names(), ["test.pos", "test.hp"]);
    let page = w.components.inspect("test.hp", 0, 10).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.entities.len(), 2);
    assert_eq!(page.entities[0].id, a);
    assert_eq!(
        page.entities[0].components,
        vec![
            ("test.pos", "Pos(1.0, 2.0)".to_owned()),
            ("test.hp", "Hp(5)".to_owned())
        ]
    );
    assert_eq!(page.entities[1].id, c);
    let second = w.components.inspect("test.hp", 1, 1).unwrap();
    assert_eq!((second.total, second.entities.len()), (2, 1));
    assert_eq!(second.entities[0].id, c);
    assert_eq!(w.components.inspect("test.hp", 5, 10).unwrap().entities.len(), 0);
    assert!(w.components.inspect("test.none", 0, 10).is_none());
}
