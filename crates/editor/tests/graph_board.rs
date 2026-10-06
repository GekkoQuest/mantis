//! Gameplay and presentation graphs side by side: the board shows every marker node with
//! the bindings reacting to it, flags unbound markers, checks edits with the cook's
//! rules at their line, and hot-reloads presentation into a live library without
//! touching graphs it did not load.

use std::path::PathBuf;

use mantis_client::presentation::PresentationLibrary;
use mantis_core::module::toml::Value;
use mantis_editor::graphs::{GraphBoard, GraphEditError};
use mantis_formats::presentation::{Action, ActionOp, Anchor, Binding, MarkerFilter, PresentationGraph};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("mantis-editor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const GRAPH: &str = r#"# The bolt: cast, travel, hit, expire.
name = "pkg.ability.bolt"
entry = "cast"

[node.cast]
key = 1
kind = "marker"
marker = "cast_start"
next = "travel"

[node.travel]
key = 2
kind = "delay"
ticks = 4
next = "hit"

[node.hit]
key = 3
kind = "marker"
marker = "impact"
next = "end"

[node.end]
key = 4
kind = "marker"
marker = "expire"
"#;

const PRESENTATION: &str = r#"# How the bolt looks.
[binding.flash]
graph = "pkg.ability.bolt"
node = 1
filter = "cast_start"

[action.flash.0]
op = "spawn_effect"
effect = "effects/spark.particles.toml"

[binding.land]
graph = "pkg.ability.bolt"
node = 3
filter = "impact"

[action.land.0]
op = "play_sound"
sound = 7
delay = 0.1
"#;

const EFFECT: &str = "[emitter.s]\ncapacity = 8\nduration = 0.5\nlifetime_min = 0.2\nlifetime_max = 0.3\nburst.0 = [0.0, 8]\ncolor.0 = [0.0, 1.0, 1.0, 1.0, 1.0]\nsize.0 = [0.0, 0.2]\n";

fn content(dir: &TempDir) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let c = dir.0.join("content");
    for (path, text) in [
        ("graphs/bolt.graph.toml", GRAPH),
        ("presentation/bolt.presentation.toml", PRESENTATION),
        ("effects/spark.particles.toml", EFFECT),
    ] {
        let p = c.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    Ok(c)
}

#[test]
fn the_board_shows_markers_with_their_bindings() -> TestResult {
    let dir = TempDir::new("graphs")?;
    let board = GraphBoard::open(&content(&dir)?)?;
    let graphs = board.graphs();
    let [bolt] = graphs.as_slice() else {
        return Err(format!("one graph: {graphs:?}").into());
    };
    assert_eq!(bolt.name, "pkg.ability.bolt");
    assert_eq!(bolt.source, "graphs/bolt.graph.toml");
    let names: Vec<(&str, &str)> = bolt
        .nodes
        .iter()
        .map(|n| (n.name.as_str(), n.kind.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("cast", "marker cast_start"),
            ("travel", "delay 4"),
            ("hit", "marker impact"),
            ("end", "marker expire"),
        ]
    );
    let travel = bolt.nodes.get(1).ok_or("travel")?;
    assert_eq!(travel.next, ["hit"]);
    let cast = bolt.nodes.first().ok_or("cast")?;
    let [flash] = cast.bindings.as_slice() else {
        return Err("one binding on cast".into());
    };
    assert_eq!(
        (flash.name.as_str(), flash.filter.as_str()),
        ("flash", "cast_start")
    );
    assert_eq!(flash.source, "presentation/bolt.presentation.toml");
    assert!(
        flash.actions.first().is_some_and(|a| a.starts_with("effect ")),
        "{flash:?}"
    );
    let hit = bolt.nodes.get(2).ok_or("hit")?;
    assert_eq!(
        hit.bindings.first().map(|b| b.actions.clone()),
        Some(vec!["sound 7 at 1 on source after 0.1 s".to_owned()])
    );
    assert_eq!(
        board.unbound_markers(),
        [("pkg.ability.bolt".to_owned(), "end".to_owned())]
    );
    Ok(())
}

#[test]
fn edits_are_checked_by_the_cook_and_hot_reload() -> TestResult {
    let dir = TempDir::new("graphs-edit")?;
    let c = content(&dir)?;
    let mut board = GraphBoard::open(&c)?;
    let mut library = PresentationLibrary::new();
    // A module's own view stays loaded through every reload.
    library.load(&PresentationGraph {
        bindings: vec![Binding {
            graph: 99,
            node: 1,
            filter: MarkerFilter::Any,
            actions: vec![Action {
                delay: 0.0,
                anchor: Anchor::Source,
                offset: [0.0; 3],
                op: ActionOp::AnimTrigger { parameter: 1 },
            }],
        }],
    });
    assert_eq!(board.apply(&mut library), 2);
    assert_eq!(library.len(), 3);

    // Binding the expire marker: the board shows it, and a reload swaps the graph.
    board.set(
        "presentation/bolt.presentation.toml",
        "binding.land",
        "node",
        &Value::Int(4),
    )?;
    board.set(
        "presentation/bolt.presentation.toml",
        "binding.land",
        "filter",
        &Value::Str("expire".to_owned()),
    )?;
    board.check()?;
    assert!(board.unbound_markers().iter().any(|(_, n)| n == "hit"));
    assert_eq!(board.apply(&mut library), 2);
    assert_eq!(
        library.len(),
        3,
        "the old bolt bindings left, the new ones came, the module's stayed"
    );

    // A binding on a delay node fails at its line; the board keeps the last good state.
    board.set(
        "presentation/bolt.presentation.toml",
        "binding.land",
        "node",
        &Value::Int(2),
    )?;
    let text = board
        .source("presentation/bolt.presentation.toml")
        .ok_or("source")?;
    let line = text.lines().position(|l| l == "node = 2").map(|i| i + 1);
    match board.check() {
        Err(GraphEditError::Check(errors)) => {
            assert!(
                errors
                    .iter()
                    .any(|e| Some(e.line) == line && e.message.contains("not a marker node")),
                "{errors:?}"
            );
        }
        other => return Err(format!("expected a check failure, got {other:?}").into()),
    }
    assert!(board.graphs().first().is_some_and(|g| g.nodes.len() == 4));

    // A gameplay graph edit is checked with the catalog's rules.
    board.set(
        "presentation/bolt.presentation.toml",
        "binding.land",
        "node",
        &Value::Int(4),
    )?;
    board.set("graphs/bolt.graph.toml", "node.travel", "ticks", &Value::Int(9))?;
    board.check()?;
    assert!(
        board
            .graphs()
            .first()
            .and_then(|g| g.nodes.get(1))
            .is_some_and(|n| n.kind == "delay 9")
    );
    board.set("graphs/bolt.graph.toml", "node.travel", "ticks", &Value::Int(0))?;
    assert!(matches!(board.check(), Err(GraphEditError::Check(_))));
    board.set("graphs/bolt.graph.toml", "node.travel", "ticks", &Value::Int(9))?;
    board.check()?;

    // Saved sources keep their comments.
    let written = board.save()?;
    assert_eq!(written.len(), 2);
    let saved = std::fs::read_to_string(c.join("graphs/bolt.graph.toml"))?;
    assert!(saved.starts_with("# The bolt: cast, travel, hit, expire."));
    assert!(saved.contains("ticks = 9"));
    Ok(())
}
