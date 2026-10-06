//! Color grading and UI markup importers: valid sources cook and parse with the runtime
//! parsers, every rule fails at its file and line, and cooking is deterministic.

use mantis_cook::importer::CookError;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::color_grading::ColorGrading;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRADING: &str = "grading/dusk.grading.toml";

const DUSK: &str = "# a warm evening grade
contrast = 1.1
saturation = 0.9
temperature = 0.2
tint = -0.1
lift = [0.02, 0.0, 0.03]
gamma = [1.0, 1.0, 0.95]
gain = [1.05, 1, 0.98]
";

const LAYOUT: &str = r#"// the heads-up display
theme {
  style title { font = "ui" size = 24 color = #ffffff }
}
panel id=root style=title direction=column gap=4 {
  text id=name bind="player.name"
  row gap=8 {
    button id=ok text="OK" intent="dialog.confirm"
  }
}
"#;

const THEME: &str = r"theme {
  style panel { background = #20242cee radius = 6 padding = 8 }
  style button:hover { background = #3c4658 }
}
";

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    Cook::new(importers::builtin()).map_err(|e| vec![e])?.run(tree)
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn single_error(path: &str, text: &str) -> Result<CookError, Box<dyn std::error::Error>> {
    let mut t = ContentTree::new();
    t.insert(path, text);
    let errors = cook(&t).err().ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    assert_eq!(e.file, path, "{e}");
    Ok(e)
}

fn assert_at(path: &str, text: &str, needle: &str, contains: &str) -> TestResult {
    let e = single_error(path, text)?;
    assert_eq!(e.line, line_of(text, needle), "{e} (expected at `{needle}`)");
    assert!(e.message.contains(contains), "{e} (expected `{contains}`)");
    Ok(())
}

#[test]
fn a_grading_cooks_and_defaults_are_neutral() -> TestResult {
    let mut t = ContentTree::new();
    t.insert(GRADING, DUSK);
    t.insert(
        "grading/neutral.grading.toml",
        "# every key defaults to neutral\n",
    );
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let dusk = out.get("grading/dusk.grd").ok_or("dusk")?;
    assert_eq!(dusk.kind, AssetKind::ColorGrading);
    assert_eq!(dusk.domain, Domain::Presentation);
    let g = ColorGrading::parse(&dusk.bytes)?;
    assert_eq!(
        g,
        ColorGrading {
            contrast: 1.1,
            saturation: 0.9,
            temperature: 0.2,
            tint: -0.1,
            lift: [0.02, 0.0, 0.03],
            gamma: [1.0, 1.0, 0.95],
            gain: [1.05, 1.0, 0.98],
        }
    );
    let neutral = out.get("grading/neutral.grd").ok_or("neutral")?;
    assert_eq!(ColorGrading::parse(&neutral.bytes)?, ColorGrading::NEUTRAL);
    Ok(())
}

#[test]
fn grading_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| DUSK.replacen(from, to, 1);
    assert_at(
        GRADING,
        &r("contrast = 1.1", "contrast = 5"),
        "contrast = 5",
        "outside 0 to 4",
    )?;
    assert_at(
        GRADING,
        &r("saturation = 0.9", "saturation = -1"),
        "saturation",
        "outside 0 to 4",
    )?;
    assert_at(
        GRADING,
        &r("temperature = 0.2", "temperature = 2"),
        "temperature",
        "outside -1 to 1",
    )?;
    assert_at(
        GRADING,
        &r("tint = -0.1", "tint = -1.5"),
        "tint",
        "outside -1 to 1",
    )?;
    assert_at(
        GRADING,
        &r("lift = [0.02, 0.0, 0.03]", "lift = [0.02, 2.0, 0.03]"),
        "lift",
        "outside -1 to 1",
    )?;
    assert_at(
        GRADING,
        &r("gamma = [1.0, 1.0, 0.95]", "gamma = [1.0, 0.05, 0.95]"),
        "gamma",
        "outside 0.1 to 10",
    )?;
    assert_at(
        GRADING,
        &r("gain = [1.05, 1, 0.98]", "gain = [1.05, 1]"),
        "gain",
        "3 numbers",
    )?;
    assert_at(
        GRADING,
        &r("tint = -0.1", "tint = \"green\""),
        "tint",
        "must be a finite number",
    )?;
    assert_at(
        GRADING,
        &r("tint = -0.1", "hue = 0.1"),
        "hue",
        "unknown key `hue`",
    )?;
    assert_at(GRADING, &format!("{DUSK}[extra]\n"), "[extra]", "unknown table")?;
    assert_at(
        GRADING,
        &r("tint = -0.1", "tint = -0.1 -0.2"),
        "tint",
        "unexpected text",
    )?;
    Ok(())
}

#[test]
fn ui_layouts_and_themes_cook_as_validated_text() -> TestResult {
    let mut t = ContentTree::new();
    t.insert("ui/hud.layout", LAYOUT);
    t.insert("ui/menus/main.theme", THEME);
    t.insert("ui/README.md", "notes are skipped");
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let hud = out.get("ui/hud.layout").ok_or("layout")?;
    assert_eq!(hud.kind, AssetKind::Ui);
    assert_eq!(hud.domain, Domain::Presentation);
    assert_eq!(hud.bytes, LAYOUT.as_bytes());
    mantis_ui::markup::parse_layout(core::str::from_utf8(&hud.bytes)?)?;
    let theme = out.get("ui/menus/main.theme").ok_or("theme")?;
    assert_eq!(theme.bytes, THEME.as_bytes());
    mantis_ui::markup::parse_theme(core::str::from_utf8(&theme.bytes)?)?;
    // Deterministic.
    let again = cook(&t).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        again.bundle(Domain::Presentation, 1).hash(),
        out.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}

#[test]
fn ui_errors_carry_the_markup_line_and_column() -> TestResult {
    // A duplicate id, at the second element's line.
    let dup = LAYOUT.replace("button id=ok", "button id=name");
    let e = single_error("ui/hud.layout", &dup)?;
    assert_eq!(e.line, line_of(&dup, "button id=name"), "{e}");
    assert!(
        e.message.contains("duplicate id") && e.message.starts_with("column "),
        "{e}"
    );
    // A formula in a template.
    let formula = LAYOUT.replace(r#"bind="player.name""#, r#"template="{hp * 2}""#);
    assert_at("ui/hud.layout", &formula, "template=", "column")?;
    // A theme file holding an element.
    let e = single_error("ui/main.theme", LAYOUT)?;
    assert_eq!(e.line, line_of(LAYOUT, "panel id=root"), "{e}");
    // An unterminated string.
    let open = THEME.replace("#3c4658 }", "#3c4658 }\n  style x { font = \"ui }");
    assert_at("ui/main.theme", &open, "style x", "unterminated string")?;
    // Markup outside `ui/` is not this importer's.
    let e = single_error("screens/hud.layout", LAYOUT)?;
    assert!(e.message.contains("no importer"), "{e}");
    Ok(())
}

#[test]
fn gradings_cook_deterministically() -> TestResult {
    let mut t = ContentTree::new();
    t.insert(GRADING, DUSK);
    let a = cook(&t).map_err(|e| format!("{e:?}"))?;
    let b = cook(&t).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        a.get("grading/dusk.grd").map(|x| x.hash),
        b.get("grading/dusk.grd").map(|x| x.hash)
    );
    Ok(())
}
