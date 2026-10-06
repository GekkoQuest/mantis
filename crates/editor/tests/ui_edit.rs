//! UI layout editing on a copy of the toy's town screen: attribute and style edits
//! hot-reload into a live UI, a broken edit is refused at its line, and edits made
//! outside the editor are picked up.

use std::path::{Path, PathBuf};

use mantis_editor::ui_edit::{Attr, LayoutEditError, LayoutEditor};
use mantis_ui::{FontLibrary, Ui};

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

fn town() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/toy/content/ui/town")
}

fn ui(layout: &str, theme: &str) -> Result<Ui, Box<dyn std::error::Error>> {
    let mut fonts = FontLibrary::new();
    let id = fonts
        .add_font(mantis_ui::test_font::latin())
        .map_err(|e| format!("{e:?}"))?;
    fonts.define_stack("ui", &[id]).map_err(|e| format!("{e:?}"))?;
    Ok(Ui::new(fonts, layout, Some(theme))?)
}

#[test]
fn edits_hot_reload_into_the_live_ui() -> TestResult {
    let dir = TempDir::new("ui")?;
    let layout = dir.0.join("hud.layout");
    let theme = dir.0.join("hud.theme");
    std::fs::copy(town().join("hud.layout"), &layout)?;
    std::fs::copy(town().join("hud.theme"), &theme)?;
    let mut ed = LayoutEditor::open(&layout, Some(&theme))?;
    let mut live = ui(ed.layout(), ed.theme().unwrap_or(""))?;
    let _ = live.frame([800.0, 600.0], 1.0);
    assert_eq!(live.text_of("town_party_title"), Some("Party"));
    let before = live.rect_of("town_chat").ok_or("chat panel")?;

    ed.set_attr("town_party_title", "text", &Attr::Text("Group \"A\"".to_owned()))?;
    ed.set_attr("town_chat", "width", &Attr::Number(300.0))?;
    ed.set_style("town_title", "size", &Attr::Number(22.0))?;
    let report = ed.apply(&mut live)?;
    assert!(report.removed.is_empty() && report.added.is_empty(), "{report:?}");
    assert!(report.kept.iter().any(|k| k == "town_party_title"));
    let _ = live.frame([800.0, 600.0], 1.0);
    assert_eq!(live.text_of("town_party_title"), Some("Group \"A\""));
    let after = live.rect_of("town_chat").ok_or("chat panel")?;
    assert!(after.w < before.w, "narrower: {before:?} -> {after:?}");
    assert!(ed.theme().is_some_and(|t| t.contains("size = 22")));
    assert!(
        ed.layout()
            .contains("// The town reference scene's heads-up display"),
        "comments kept"
    );

    // A broken value is refused at its line; the text and the live UI are unchanged.
    let text = ed.layout().to_owned();
    match ed.set_attr("town_zone", "bind", &Attr::Word("{".to_owned())) {
        Err(LayoutEditError::Markup(e)) => {
            let expected = text
                .lines()
                .position(|l| l.contains("id=town_zone"))
                .map(|i| i + 1);
            assert_eq!(Some(e.line as usize), expected, "{e:?}");
        }
        other => return Err(format!("expected a markup error, got {other:?}").into()),
    }
    assert_eq!(ed.layout(), text);
    assert!(matches!(
        ed.set_attr("nobody", "text", &Attr::Text("x".to_owned())),
        Err(LayoutEditError::NotFound(_))
    ));

    // Saved: the files hold the edits and nothing reloads from our own write.
    ed.save()?;
    assert_eq!(std::fs::read_to_string(&layout)?, ed.layout());
    assert!(ed.poll(&mut live)?.is_none());

    // An edit made outside the editor reloads into the live UI.
    let outside = ed
        .layout()
        .replace("text=\"Party\"", "text=\"Roster\"")
        .replace("text=\"Group \\\"A\\\"\"", "text=\"Roster\"");
    std::fs::write(&layout, &outside)?;
    let reloaded = ed.poll(&mut live)?.ok_or("the outside edit reloads")?;
    assert!(reloaded.kept.iter().any(|k| k == "town_party_title"));
    let _ = live.frame([800.0, 600.0], 1.0);
    assert_eq!(live.text_of("town_party_title"), Some("Roster"));
    // A broken outside edit keeps the last good text live.
    std::fs::write(&layout, "panel id=broken {")?;
    assert!(matches!(ed.poll(&mut live), Err(LayoutEditError::Markup(_))));
    let _ = live.frame([800.0, 600.0], 1.0);
    assert_eq!(live.text_of("town_party_title"), Some("Roster"));
    Ok(())
}
