//! The editor's server inspector against the real Ops dashboard: a headless local toy
//! cluster with bots, read through `OpsInspector` (pinned certificate, operator token,
//! GET only). The cell's tick and its system run counts move between two reads a second
//! and a half apart; components and entity pages read as text; a wrong token is a 401.

use std::time::Duration;

use toy_client::editor::OpsInspector;
use toy_server::cluster::{LocalOptions, start_local};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn the_editor_reads_a_live_cell_through_ops() -> TestResult {
    let world = start_local(&LocalOptions { seed: 7, bots: 4 })?;
    let cell = *world.cells.first().ok_or("a cell")?;
    let ops = OpsInspector::new(world.ops_addr, &world.ops_cert_der, &world.token)?;
    // The host publishes every 30 ticks; wait for the first report.
    let mut first = ops.systems(cell);
    for _ in 0..40 {
        if first.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        first = ops.systems(cell);
    }
    let first = first?;
    assert!(!first.systems.is_empty());
    assert_eq!(
        first.systems.first().map(|s| s.name.as_str()),
        Some("server.graphs")
    );
    std::thread::sleep(Duration::from_millis(1500));
    let second = ops.systems(cell)?;
    assert!(second.tick > first.tick, "{} -> {}", first.tick, second.tick);
    let runs = |v: &toy_client::editor::OpsSystems, name: &str| {
        v.systems.iter().find(|s| s.name == name).map_or(0, |s| s.runs)
    };
    assert!(runs(&second, "server.graphs") > runs(&first, "server.graphs"));
    let summary = ops.cell(cell)?;
    assert!(
        summary.tick >= second.tick && summary.sessions >= 1,
        "{summary:?}"
    );
    let components = ops.components(cell)?;
    assert!(!components.is_empty());
    let component = components.first().cloned().ok_or("a component")?;
    let page = ops.entities(cell, &component, 0, 10)?;
    assert!(page.total >= 1 && !page.entities.is_empty(), "{page:?}");
    assert!(page.entities.iter().all(|e| e.id.contains(':')));
    let wrong = OpsInspector::new(world.ops_addr, &world.ops_cert_der, "not-the-operator-token")?;
    assert!(matches!(
        wrong.cell(cell),
        Err(toy_client::editor::OpsError::Status(401, _))
    ));
    world.stop()?;
    Ok(())
}
