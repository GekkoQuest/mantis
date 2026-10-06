//! Hot reload: state survives by id, errors keep the old tree, and the file
//! watcher notices edits.

mod common;

use common::{TestResult, VIEWPORT, sample_ui};
use mantis_ui::{SourceWatcher, UiEvent};

const EDITED: &str = r#"
panel id=root style=panel direction=column gap=6 width=400 height=300 {
  text id=hp template="HP {hp} of {hp_max}"
  row id=buttons gap=8 {
    button id=ok text="Confirm" intent="dialog.confirm"
    button id=help text="Help" intent="dialog.help"
  }
  input id=chat placeholder="Say something" submit="chat.send"
}
"#;

#[test]
fn reload_preserves_state_by_id() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    assert!(ui.set_focus("chat"));
    ui.handle(&UiEvent::Text("hello".to_owned()));
    ui.handle(&UiEvent::Key {
        key: mantis_ui::UiKey::Left,
        pressed: true,
        modifiers: mantis_ui::Modifiers::default(),
    });
    let ok = ui.rect_of("ok").ok_or("ok")?;
    let (x, y) = common::center(ok, 1.0);
    ui.handle(&UiEvent::PointerMove { x, y });
    ui.frame(VIEWPORT, 1.0);

    // A broken edit: the error has a position and nothing changes.
    let broken = EDITED.replace("gap=8 {", "gap=8 {{");
    let err = ui.reload(&broken, None).err().ok_or("expected an error")?;
    assert_eq!(err.line, 4);
    assert!(err.column > 1);
    assert_eq!(ui.text_of("ok"), Some("OK"));
    assert!(ui.rect_of("cancel").is_some());

    let report = ui.reload(EDITED, None)?;
    assert_eq!(report.added, vec!["help".to_owned()]);
    assert_eq!(
        report.removed,
        vec!["cancel".to_owned(), "items".to_owned(), "name".to_owned()]
    );
    assert_eq!(
        report.kept,
        vec!["buttons", "chat", "hp", "ok", "root"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
    ui.frame(VIEWPORT, 1.0);
    // Focus, contents, caret, and hover survive.
    assert_eq!(ui.focused(), ui.widget_id("chat"));
    let state = ui.input_state("chat").ok_or("chat")?;
    assert_eq!(state.text, "hello");
    assert_eq!(state.caret, 4);
    assert_eq!(ui.hovered(), ui.widget_id("ok"));
    // The new layout is live and still bound.
    assert_eq!(ui.text_of("ok"), Some("Confirm"));
    assert_eq!(ui.text_of("hp"), Some("HP 90 of 100"));
    assert!(ui.rect_of("cancel").is_none());
    ui.handle(&UiEvent::Text("!".to_owned()));
    assert_eq!(ui.input_state("chat").map(|s| s.text.as_str()), Some("hell!o"));

    // A new theme replaces the package theme.
    let theme = r"theme { style panel { background = #ff0000 padding = 2 } }";
    ui.reload(EDITED, Some(theme))?;
    ui.frame(VIEWPORT, 1.0);
    let root = ui.tree().get(ui.tree().root()).ok_or("root")?;
    assert!((root.style.background.r - 1.0).abs() < 1e-6);
    Ok(())
}

/// Removes the directory on drop, even when an assertion fails.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn source_watcher_sees_edits_only() -> TestResult {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let dir = TempDir(std::env::temp_dir().join(format!("mantis-ui-watch-{}-{nanos}", std::process::id())));
    std::fs::create_dir_all(&dir.0)?;
    let file = dir.0.join("layout.ui");
    let mut watcher = SourceWatcher::new(&file);
    assert!(watcher.poll().is_err(), "missing file is an error");

    std::fs::write(&file, "panel { }")?;
    assert_eq!(watcher.poll()?.as_deref(), Some("panel { }"));
    assert_eq!(watcher.poll()?, None);

    std::fs::write(&file, "panel id=root { }")?;
    assert_eq!(watcher.poll()?.as_deref(), Some("panel id=root { }"));
    assert_eq!(watcher.poll()?, None);

    // Rewriting identical contents is not a change.
    std::fs::write(&file, "panel id=root { }")?;
    assert_eq!(watcher.poll()?, None);

    // Drive a UI from the watcher.
    std::fs::write(&file, common::LAYOUT)?;
    let mut ui = sample_ui()?;
    if let Some(src) = watcher.poll()? {
        ui.reload(&src, None)?;
    }
    ui.frame(VIEWPORT, 1.0);
    assert!(ui.rect_of("ok").is_some());
    assert_eq!(watcher.path(), file.as_path());
    drop(dir);
    Ok(())
}
