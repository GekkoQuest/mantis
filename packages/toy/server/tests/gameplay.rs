//! The toy's gameplay content comes from its verified cooked bundle
//! (decision 0021). The Rust constructor the package used before is kept
//! here as a fixture: the cooked graph must match it key for key.

use std::collections::BTreeMap;
use std::path::Path;

use mantis_adapter_contract::AbilityId;
use mantis_core::graph::{
    GameplayGraph, GraphCatalog, GraphError, GraphId, MarkerSpec, Node, NodeKey, NodeKind,
};
use mantis_server::gameplay::{Gameplay, GameplayError};
use toy_server::world::{self, PULSE, PULSE_GRAPH};

/// The class's abilities: a pulse that starts, lasts 6 ticks, and expires.
///
/// # Errors
/// [`GraphError`] if the graph is malformed.
pub fn abilities() -> Result<(GraphCatalog, BTreeMap<AbilityId, GraphId>), GraphError> {
    let mut catalog = GraphCatalog::new();
    catalog.insert(GameplayGraph {
        id: PULSE_GRAPH,
        entry: NodeKey(1),
        nodes: vec![
            Node {
                key: NodeKey(1),
                kind: NodeKind::Marker {
                    marker: MarkerSpec::CastStart,
                    offset: 0,
                    next: Some(NodeKey(2)),
                },
            },
            Node {
                key: NodeKey(2),
                kind: NodeKind::Delay {
                    ticks: 6,
                    next: Some(NodeKey(3)),
                },
            },
            Node {
                key: NodeKey(3),
                kind: NodeKind::Marker {
                    marker: MarkerSpec::Expire,
                    offset: 0,
                    next: None,
                },
            },
        ],
    })?;
    Ok((catalog, BTreeMap::from([(PULSE, PULSE_GRAPH)])))
}

#[test]
fn cells_load_the_cooked_pulse_graph_and_it_matches_the_rust_fixture() {
    let cooked = world::gameplay().unwrap();
    let (fixture, fixture_abilities) = abilities().unwrap();
    assert_eq!(cooked.abilities, fixture_abilities);
    assert_eq!(cooked.abilities.get(&PULSE), Some(&PULSE_GRAPH));
    assert_eq!(cooked.catalog.len(), 1);
    assert_eq!(cooked.catalog.get(PULSE_GRAPH), fixture.get(PULSE_GRAPH));
    // The same bundle the handshake hash names.
    let hash = world::cooked_content(Path::new(world::COOKED_PATH), None).unwrap();
    assert_eq!(cooked.hash, hash);
}

#[test]
fn a_wrong_key_or_a_missing_cook_refuses_to_start() {
    let cooked = Path::new(world::COOKED_PATH);
    assert!(matches!(
        Gameplay::load(cooked, &[7u8; 32], &[]),
        Err(GameplayError::Bundle(_))
    ));
    assert!(matches!(
        Gameplay::load(&cooked.join("missing"), &[7u8; 32], &[]),
        Err(GameplayError::Io(_))
    ));
}
