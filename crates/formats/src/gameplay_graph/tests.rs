//! Gameplay graph tests: round trip, conversion to the core graph (accepted by the core
//! catalog), and every rejection rule.

use super::*;
use mantis_core::graph::GraphCatalog;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Cast start, a repeat of three ticks (delay, tick marker), a chance of an action on the
/// target, then expire.
fn sample() -> GraphAsset {
    GraphAsset {
        name: "pkg.ability.burn".to_owned(),
        entry: 1,
        actions: vec!["pkg.damage".to_owned()],
        nodes: vec![
            GraphNode {
                key: 1,
                kind: GraphNodeKind::Marker {
                    marker: MarkerSpec::CastStart,
                    offset: 0,
                    next: Some(2),
                },
            },
            GraphNode {
                key: 2,
                kind: GraphNodeKind::Repeat {
                    counter: 0,
                    times: 3,
                    body: 3,
                    done: Some(6),
                },
            },
            GraphNode {
                key: 3,
                kind: GraphNodeKind::Delay {
                    ticks: 5,
                    next: Some(4),
                },
            },
            GraphNode {
                key: 4,
                kind: GraphNodeKind::Marker {
                    marker: MarkerSpec::Tick { counter: 0 },
                    offset: 0,
                    next: Some(5),
                },
            },
            GraphNode {
                key: 5,
                kind: GraphNodeKind::Chance {
                    numerator: 1,
                    denominator: 2,
                    then: Some(7),
                    otherwise: Some(2),
                },
            },
            GraphNode {
                key: 6,
                kind: GraphNodeKind::Marker {
                    marker: MarkerSpec::Expire,
                    offset: 2,
                    next: None,
                },
            },
            GraphNode {
                key: 7,
                kind: GraphNodeKind::Action {
                    action: 0,
                    target: Target::Target,
                    params: [10, -3, 0, i32::MAX],
                    next: Some(2),
                },
            },
        ],
    }
}

#[test]
fn round_trips_and_loads_into_the_core_catalog() -> TestResult {
    let g = sample();
    let bytes = g.encode();
    assert_eq!(GraphAsset::parse(&bytes)?, g);
    assert_eq!(g.id(), GraphId::named("pkg.ability.burn"));
    assert_eq!(g.marker_keys().collect::<Vec<_>>(), vec![1, 4, 6]);
    let mut catalog = GraphCatalog::new();
    let damage = catalog.register_action("pkg.damage")?;
    let core = g.to_core(|name| (name == "pkg.damage").then_some(damage))?;
    assert_eq!(core.nodes.len(), 7);
    catalog.insert(core)?;
    assert!(catalog.get(g.id()).is_some());
    Ok(())
}

#[test]
fn unknown_actions_are_refused_by_name() {
    assert_eq!(
        sample().to_core(|_| None),
        Err(GraphLoadError::UnknownAction("pkg.damage".to_owned()))
    );
}

#[test]
fn every_rule_is_enforced() {
    let refused = |edit: fn(&mut GraphAsset), expected: FormatError| {
        let mut g = sample();
        edit(&mut g);
        assert_eq!(g.validate(), Err(expected), "{g:?}");
        // The parser applies the same rules to encoded bytes.
        assert!(GraphAsset::parse(&g.encode()).is_err());
    };
    refused(|g| g.name.clear(), FormatError::Validity);
    refused(|g| g.name = "x".repeat(MAX_NAME + 1), FormatError::Validity);
    refused(|g| g.entry = 99, FormatError::Inconsistent);
    refused(|g| g.nodes.swap(0, 1), FormatError::Inconsistent);
    refused(|g| g.nodes.clear(), FormatError::Dimensions);
    refused(
        |g| {
            if let Some(n) = g.nodes.get_mut(2) {
                n.kind = GraphNodeKind::Delay {
                    ticks: 0,
                    next: Some(4),
                };
            }
        },
        FormatError::Validity,
    );
    refused(
        |g| {
            if let Some(n) = g.nodes.get_mut(4) {
                n.kind = GraphNodeKind::Chance {
                    numerator: 1,
                    denominator: 0,
                    then: None,
                    otherwise: None,
                };
            }
        },
        FormatError::Validity,
    );
    refused(
        |g| {
            if let Some(n) = g.nodes.get_mut(1) {
                n.kind = GraphNodeKind::Repeat {
                    counter: u8::try_from(MAX_COUNTERS).unwrap_or(u8::MAX),
                    times: 3,
                    body: 3,
                    done: None,
                };
            }
        },
        FormatError::Validity,
    );
    refused(
        |g| {
            if let Some(n) = g.nodes.get_mut(0) {
                n.kind = GraphNodeKind::Marker {
                    marker: MarkerSpec::CastStart,
                    offset: 0,
                    next: Some(42),
                };
            }
        },
        FormatError::Inconsistent,
    );
    refused(
        |g| g.actions.push("pkg.unused".to_owned()),
        FormatError::Inconsistent,
    );
    refused(
        |g| g.actions.push("pkg.damage".to_owned()),
        FormatError::Inconsistent,
    );
    refused(|g| g.actions.clear(), FormatError::Inconsistent);
}

#[test]
fn malformed_bytes_fail_closed() {
    let good = sample().encode();
    let mut bad_magic = good.clone();
    if let Some(b) = bad_magic.first_mut() {
        *b = b'X';
    }
    assert_eq!(GraphAsset::parse(&bad_magic), Err(FormatError::Magic));
    let mut bad_version = good.clone();
    if let Some(b) = bad_version.get_mut(4) {
        *b = 2;
    }
    assert_eq!(GraphAsset::parse(&bad_version), Err(FormatError::Version(2)));
    let mut flags = good.clone();
    if let Some(b) = flags.get_mut(6) {
        *b = 1;
    }
    assert_eq!(GraphAsset::parse(&flags), Err(FormatError::Flags(1)));
    // A nonzero byte in a node's unused fields.
    let mut padding = good.clone();
    if let Some(b) = padding.last_mut() {
        *b = 7;
    }
    assert_eq!(GraphAsset::parse(&padding), Err(FormatError::Reserved));
    for cut in 0..good.len() {
        assert!(
            GraphAsset::parse(good.get(..cut).unwrap_or(&[])).is_err(),
            "cut at {cut}"
        );
    }
    let mut kind = good;
    // The first node's kind byte sits after its key; make it unknown.
    let header = 4 + 2 + 2 + 2 + "pkg.ability.burn".len() + 2 + 2 + 2 + "pkg.damage".len() + 2;
    if let Some(b) = kind.get_mut(header + 2) {
        *b = 9;
    }
    assert_eq!(GraphAsset::parse(&kind), Err(FormatError::Encoding(9)));
}
