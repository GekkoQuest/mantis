//! Source editing: every untouched byte is kept, comments survive, and broken edits are
//! refused.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SECTOR: &str = r#"# A sector.
[sector]
x = 0
z = 0
size = 32.0

# The first crate.
[placement.crate_01]
mesh = "meshes/crate.obj"
position = [8.0, 0.0, 6.0]   # near the gate
yaw = 0.0

# The second crate.
[placement.crate_02]
mesh = "meshes/crate.obj"
position = [20.0, 0.0, 18.0]
"#;

#[test]
fn setting_a_key_rewrites_only_its_line() -> TestResult {
    let mut d = SourceDoc::parse(SECTOR)?;
    d.set("placement.crate_01", "position", &floats(&[9.5, 0.0, 6.25]))?;
    let expected = SECTOR.replace(
        "position = [8.0, 0.0, 6.0]   # near the gate",
        "position = [9.5, 0.0, 6.25]  # near the gate",
    );
    assert_eq!(d.text(), expected);
    assert_eq!(
        d.get("placement.crate_01", "position"),
        Some(&floats(&[9.5, 0.0, 6.25]))
    );
    Ok(())
}

#[test]
fn a_new_key_goes_after_the_last_entry_of_its_table() -> TestResult {
    let mut d = SourceDoc::parse(SECTOR)?;
    d.set("placement.crate_01", "scale", &float(2.0))?;
    assert_eq!(
        d.text(),
        SECTOR.replace("yaw = 0.0\n", "yaw = 0.0\nscale = 2.0\n")
    );
    d.set("placement.crate_02", "yaw", &float(45.0))?;
    assert!(d.text().ends_with("position = [20.0, 0.0, 18.0]\nyaw = 45.0\n"));
    Ok(())
}

#[test]
fn tables_are_added_and_removed_with_their_comments() -> TestResult {
    let mut d = SourceDoc::parse(SECTOR)?;
    assert_eq!(d.tables_under("placement."), ["crate_01", "crate_02"]);
    d.remove_table("placement.crate_01")?;
    let without = SECTOR.replace(
        "# The first crate.\n[placement.crate_01]\nmesh = \"meshes/crate.obj\"\nposition = [8.0, 0.0, 6.0]   # near the gate\nyaw = 0.0\n\n",
        "",
    );
    assert_eq!(d.text(), without);
    d.add_table(
        "placement.crate_03",
        &[
            ("mesh", text("meshes/crate.obj")),
            ("position", floats(&[1.0, 0.0, 2.0])),
        ],
    )?;
    assert!(d.text().ends_with(
        "position = [20.0, 0.0, 18.0]\n\n[placement.crate_03]\nmesh = \"meshes/crate.obj\"\nposition = [1.0, 0.0, 2.0]\n"
    ));
    d.remove_table("placement.crate_03")?;
    assert_eq!(d.text(), without, "removing the last table restores the text");
    assert_eq!(
        d.add_table("placement.crate_02", &[]),
        Err(EditError::TableExists("placement.crate_02".to_owned()))
    );
    Ok(())
}

#[test]
fn bad_edits_are_refused_and_undone() -> TestResult {
    let mut d = SourceDoc::parse(SECTOR)?;
    assert_eq!(
        d.set("placement.nope", "x", &float(1.0)),
        Err(EditError::NoTable("placement.nope".to_owned()))
    );
    assert!(matches!(
        d.set("sector", "bad key", &float(1.0)),
        Err(EditError::BadName(_))
    ));
    assert_eq!(d.text(), SECTOR);
    assert!(d.remove("sector", "size")?);
    assert!(!d.remove("sector", "size")?);
    assert!(SourceDoc::parse("[a\n").is_err());
    Ok(())
}

#[test]
fn strings_and_floats_format_as_the_subset_reads_them() -> TestResult {
    assert_eq!(format_value(&text(r#"a "b" \ c"#)), r#""a \"b\" \\ c""#);
    assert_eq!(format_value(&float(2.0)), "2.0");
    assert_eq!(format_value(&float(0.1)), "0.1");
    assert_eq!(format_value(&float(-12.5)), "-12.5");
    let mut d = SourceDoc::parse("[t]\n")?;
    d.set("t", "s", &text(r#"quote " and \ slash"#))?;
    assert_eq!(d.get("t", "s"), Some(&text(r#"quote " and \ slash"#)));
    Ok(())
}
