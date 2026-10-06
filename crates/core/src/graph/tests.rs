use super::*;
use crate::rng::Seed;

const SRC: EntityId = EntityId::new(1, 0);
const TGT: EntityId = EntityId::new(2, 0);

#[derive(Default)]
struct Recorder {
    calls: Vec<ActionCall>,
    fail_on: Option<ActionId>,
}

impl ActionHandler for Recorder {
    fn apply(&mut self, call: &ActionCall) -> Result<(), ActionError> {
        if Some(call.action) == self.fail_on {
            return Err(ActionError("refused"));
        }
        self.calls.push(*call);
        Ok(())
    }
}

fn node(key: u16, kind: NodeKind) -> Node {
    Node {
        key: NodeKey(key),
        kind,
    }
}

/// A periodic effect: cast start, impact, 3 ticks of damage 10 ticks apart,
/// expire. Keys deliberately non-contiguous and out of order.
fn periodic(catalog: &mut GraphCatalog) -> (GraphId, ActionId) {
    let damage = catalog.register_action("test.damage").unwrap();
    let id = GraphId::named("test.periodic");
    catalog
        .insert(GameplayGraph {
            id,
            entry: NodeKey(10),
            nodes: vec![
                node(
                    40,
                    NodeKind::Marker {
                        marker: MarkerSpec::Tick { counter: 0 },
                        offset: 0,
                        next: Some(NodeKey(50)),
                    },
                ),
                node(
                    10,
                    NodeKind::Marker {
                        marker: MarkerSpec::CastStart,
                        offset: 0,
                        next: Some(NodeKey(20)),
                    },
                ),
                node(
                    20,
                    NodeKind::Marker {
                        marker: MarkerSpec::Impact,
                        offset: 5,
                        next: Some(NodeKey(25)),
                    },
                ),
                node(
                    25,
                    NodeKind::Delay {
                        ticks: 5,
                        next: Some(NodeKey(30)),
                    },
                ),
                node(
                    30,
                    NodeKind::Repeat {
                        counter: 0,
                        times: 3,
                        body: NodeKey(40),
                        done: Some(NodeKey(70)),
                    },
                ),
                node(
                    50,
                    NodeKind::Action {
                        action: damage,
                        target: Target::Target,
                        params: ActionParams([12, 0, 0, 0]),
                        next: Some(NodeKey(60)),
                    },
                ),
                node(
                    60,
                    NodeKind::Delay {
                        ticks: 10,
                        next: Some(NodeKey(30)),
                    },
                ),
                node(
                    70,
                    NodeKind::Marker {
                        marker: MarkerSpec::Expire,
                        offset: 0,
                        next: None,
                    },
                ),
            ],
        })
        .unwrap();
    (id, damage)
}

#[test]
fn graph_ids_are_content_derived() {
    assert_eq!(GraphId::named(""), GraphId(0x811C_9DC5));
    assert_eq!(GraphId::named("a"), GraphId(0xE40C_292C));
    assert_eq!(GraphId::named("test.periodic"), GraphId::named("test.periodic"));
    assert_ne!(GraphId::named("test.periodic"), GraphId::named("test.periodic2"));
}

#[test]
fn periodic_graph_emits_markers_and_actions_on_schedule() {
    let mut catalog = GraphCatalog::new();
    let (id, damage) = periodic(&mut catalog);
    let mut rt = GraphRuntime::with_capacity(8, 16);
    let mut h = Recorder::default();
    let inst = rt.start(&catalog, id, SRC, Some(TGT), Tick(100)).unwrap();

    let mut log: Vec<(u64, MarkerKind, u64, u32)> = Vec::new();
    for t in 100..=140 {
        let r = rt.evaluate(&catalog, Tick(t), Seed(1), &mut h);
        assert_eq!(r.failed, 0);
        for m in rt.markers() {
            assert_eq!(m.instance, inst);
            assert_eq!((m.source, m.target), (SRC, Some(TGT)));
            assert_eq!(m.id.graph, id);
            log.push((t, m.kind, m.at.0, m.offset));
        }
    }
    assert_eq!(
        log,
        vec![
            (100, MarkerKind::CastStart, 100, 0),
            (100, MarkerKind::Impact { target: TGT }, 105, 5),
            (105, MarkerKind::TickN(1), 105, 5),
            (115, MarkerKind::TickN(2), 115, 15),
            (125, MarkerKind::TickN(3), 125, 25),
            (135, MarkerKind::Expire, 135, 35),
        ]
    );
    let ticks: Vec<_> = h
        .calls
        .iter()
        .map(|c| (c.tick.0, c.target, c.counters[0], c.params))
        .collect();
    assert_eq!(
        ticks,
        vec![
            (105, TGT, 1, ActionParams([12, 0, 0, 0])),
            (115, TGT, 2, ActionParams([12, 0, 0, 0])),
            (125, TGT, 3, ActionParams([12, 0, 0, 0])),
        ]
    );
    assert!(
        h.calls
            .iter()
            .all(|c| c.action == damage && c.node.node == NodeKey(50))
    );
    assert!(rt.is_empty(), "finished instance removed");
}

#[test]
fn marker_ids_are_stable_node_keys() {
    let mut catalog = GraphCatalog::new();
    let (id, _) = periodic(&mut catalog);
    let mut rt = GraphRuntime::with_capacity(1, 4);
    rt.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    rt.evaluate(&catalog, Tick(0), Seed(0), &mut Recorder::default());
    let keys: Vec<_> = rt.markers().iter().map(|m| m.id).collect();
    assert_eq!(
        keys,
        vec![
            MarkerId {
                graph: id,
                node: NodeKey(10)
            },
            MarkerId {
                graph: id,
                node: NodeKey(20)
            }
        ]
    );
}

#[test]
fn validation_refuses_bad_graphs() {
    let mut c = GraphCatalog::new();
    let act = c.register_action("test.act").unwrap();
    assert_eq!(
        c.register_action("test.act"),
        Err(GraphError::DuplicateAction(act))
    );
    assert_eq!(c.register_action(String::from("test.owned")), Ok(ActionId(1)));
    assert_eq!(c.action_id("test.owned"), Some(ActionId(1)));
    assert_eq!(c.action_name(act), Some("test.act"));
    assert_eq!(c.action_id("test.none"), None);
    let g = |nodes: Vec<Node>| GameplayGraph {
        id: GraphId::named("test.bad"),
        entry: NodeKey(1),
        nodes,
    };
    let end = |key| {
        node(
            key,
            NodeKind::Marker {
                marker: MarkerSpec::Expire,
                offset: 0,
                next: None,
            },
        )
    };
    assert_eq!(c.insert(g(vec![])), Err(GraphError::BadSize));
    assert_eq!(
        c.insert(g(vec![end(1), end(1)])),
        Err(GraphError::DuplicateNode(NodeKey(1)))
    );
    assert_eq!(
        c.insert(g(vec![end(2)])),
        Err(GraphError::UnknownNode(NodeKey(1)))
    );
    assert_eq!(
        c.insert(g(vec![node(
            1,
            NodeKind::Delay {
                ticks: 1,
                next: Some(NodeKey(9))
            }
        )])),
        Err(GraphError::UnknownNode(NodeKey(9)))
    );
    assert_eq!(
        c.insert(g(vec![node(1, NodeKind::Delay { ticks: 0, next: None })])),
        Err(GraphError::ZeroDelay(NodeKey(1)))
    );
    assert_eq!(
        c.insert(g(vec![node(
            1,
            NodeKind::Chance {
                numerator: 1,
                denominator: 0,
                then: None,
                otherwise: None
            }
        )])),
        Err(GraphError::ZeroDenominator(NodeKey(1)))
    );
    assert_eq!(
        c.insert(g(vec![node(
            1,
            NodeKind::Repeat {
                counter: 4,
                times: 1,
                body: NodeKey(1),
                done: None
            }
        )])),
        Err(GraphError::BadCounter(NodeKey(1)))
    );
    assert_eq!(
        c.insert(g(vec![node(
            1,
            NodeKind::Action {
                action: ActionId(9),
                target: Target::Source,
                params: ActionParams::default(),
                next: None
            }
        )])),
        Err(GraphError::UnknownAction(ActionId(9)))
    );
}

#[test]
fn validation_refuses_zero_time_cycles_and_duplicates() {
    let mut c = GraphCatalog::new();
    let act = c.register_action("test.act").unwrap();
    let g = |nodes: Vec<Node>| GameplayGraph {
        id: GraphId::named("test.bad"),
        entry: NodeKey(1),
        nodes,
    };
    let end = |key| {
        node(
            key,
            NodeKind::Marker {
                marker: MarkerSpec::Expire,
                offset: 0,
                next: None,
            },
        )
    };
    // A loop with no Delay would spin forever within one tick.
    assert_eq!(
        c.insert(g(vec![
            node(
                1,
                NodeKind::Repeat {
                    counter: 0,
                    times: 3,
                    body: NodeKey(2),
                    done: None
                }
            ),
            node(
                2,
                NodeKind::Action {
                    action: act,
                    target: Target::Source,
                    params: ActionParams::default(),
                    next: Some(NodeKey(1))
                }
            ),
        ])),
        Err(GraphError::ZeroTimeCycle(NodeKey(1)))
    );
    // The same loop with a Delay is fine.
    c.insert(g(vec![
        node(
            1,
            NodeKind::Repeat {
                counter: 0,
                times: 3,
                body: NodeKey(2),
                done: None,
            },
        ),
        node(
            2,
            NodeKind::Delay {
                ticks: 1,
                next: Some(NodeKey(1)),
            },
        ),
    ]))
    .unwrap();
    assert_eq!(
        c.insert(g(vec![end(1)])),
        Err(GraphError::DuplicateGraph(GraphId::named("test.bad")))
    );
    assert_eq!(c.len(), 1);
}

#[test]
fn start_fails_closed() {
    let mut catalog = GraphCatalog::new();
    let (id, _) = periodic(&mut catalog);
    let mut rt = GraphRuntime::with_capacity(1, 4);
    assert_eq!(
        rt.start(&catalog, GraphId(7), SRC, None, Tick(0)),
        Err(GraphError::UnknownGraph(GraphId(7)))
    );
    assert!(catalog.get(id).unwrap().needs_target());
    assert_eq!(
        rt.start(&catalog, id, SRC, None, Tick(0)),
        Err(GraphError::MissingTarget(id))
    );
    rt.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    assert_eq!(
        rt.start(&catalog, id, SRC, Some(TGT), Tick(0)),
        Err(GraphError::InstancesFull)
    );
}

#[test]
fn action_failure_stops_only_that_instance() {
    let mut catalog = GraphCatalog::new();
    let (id, damage) = periodic(&mut catalog);
    let mut rt = GraphRuntime::with_capacity(4, 16);
    rt.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    let mut h = Recorder {
        fail_on: Some(damage),
        ..Recorder::default()
    };
    rt.evaluate(&catalog, Tick(0), Seed(0), &mut h);
    let r = rt.evaluate(&catalog, Tick(5), Seed(0), &mut h);
    assert_eq!(r.failed, 1);
    assert!(rt.is_empty());
}

#[test]
fn marker_overflow_is_counted() {
    let mut catalog = GraphCatalog::new();
    let (id, _) = periodic(&mut catalog);
    let mut rt = GraphRuntime::with_capacity(4, 1);
    rt.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    let r = rt.evaluate(&catalog, Tick(0), Seed(0), &mut Recorder::default());
    assert_eq!(rt.markers().len(), 1);
    assert_eq!(r.markers_dropped, 1);
}

#[test]
fn chance_is_deterministic_and_distributed() {
    let mut catalog = GraphCatalog::new();
    let hit = catalog.register_action("test.hit").unwrap();
    let miss = catalog.register_action("test.miss").unwrap();
    let id = GraphId::named("test.chance");
    let act = |a| NodeKind::Action {
        action: a,
        target: Target::Source,
        params: ActionParams::default(),
        next: None,
    };
    catalog
        .insert(GameplayGraph {
            id,
            entry: NodeKey(1),
            nodes: vec![
                node(
                    1,
                    NodeKind::Chance {
                        numerator: 1,
                        denominator: 4,
                        then: Some(NodeKey(2)),
                        otherwise: Some(NodeKey(3)),
                    },
                ),
                node(2, act(hit)),
                node(3, act(miss)),
            ],
        })
        .unwrap();
    let run = |seed: u64| {
        let mut rt = GraphRuntime::with_capacity(512, 4);
        let mut h = Recorder::default();
        for i in 0..400 {
            rt.start(&catalog, id, EntityId::new(i, 0), None, Tick(9))
                .unwrap();
        }
        rt.evaluate(&catalog, Tick(9), Seed(seed), &mut h);
        h.calls.iter().map(|c| (c.source, c.action)).collect::<Vec<_>>()
    };
    let a = run(42);
    assert_eq!(a, run(42), "same seed, same rolls");
    assert_ne!(a, run(43));
    let hits = a.iter().filter(|(_, act)| *act == hit).count();
    assert!((70..130).contains(&hits), "about a quarter: {hits}");
}

#[test]
fn cancel_and_state_hash() {
    let mut catalog = GraphCatalog::new();
    let (id, _) = periodic(&mut catalog);
    let mut a = GraphRuntime::with_capacity(4, 8);
    let mut b = GraphRuntime::with_capacity(4, 8);
    let ia = a.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    b.start(&catalog, id, SRC, Some(TGT), Tick(0)).unwrap();
    assert_eq!(a.stable_hash(), b.stable_hash());
    a.evaluate(&catalog, Tick(0), Seed(0), &mut Recorder::default());
    assert_ne!(a.stable_hash(), b.stable_hash(), "cursor moved");
    b.evaluate(&catalog, Tick(0), Seed(0), &mut Recorder::default());
    assert_eq!(a.stable_hash(), b.stable_hash());
    let listed: Vec<_> = a.instances(&catalog).collect();
    assert_eq!(listed, vec![(ia, id, Some(NodeKey(30)), Tick(5))]);
    assert!(a.cancel(ia));
    assert!(!a.cancel(ia));
    assert!(a.is_empty());
}
