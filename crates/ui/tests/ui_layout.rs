//! Flex-lite layout: grow weights, justify, align, min/max, padding,
//! visibility, wrapping, and dirty tracking.

mod common;

use common::{TestResult, VIEWPORT, fonts};
use mantis_ui::{Rect, Ui};

fn ui(src: &str) -> Result<Ui, Box<dyn std::error::Error>> {
    let mut ui = Ui::new(fonts()?, src, None)?;
    ui.frame(VIEWPORT, 1.0);
    Ok(ui)
}

fn rect(ui: &Ui, id: &str) -> Result<Rect, Box<dyn std::error::Error>> {
    Ok(ui.rect_of(id).ok_or_else(|| format!("no rect for {id}"))?)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn grow_weights_share_free_space() -> TestResult {
    let ui = ui("row id=r width=300 height=20 {
        spacer id=s width=grow:1
        panel id=a width=grow:2 height=10
        panel id=b width=50 height=10
    }")?;
    let (s, a, b) = (rect(&ui, "s")?, rect(&ui, "a")?, rect(&ui, "b")?);
    assert!(near(s.w, 250.0 / 3.0), "{s:?}");
    assert!(near(a.w, 500.0 / 3.0), "{a:?}");
    assert!(near(a.x, s.w) && near(b.x, 250.0) && near(b.w, 50.0));
    assert!(near(s.h, 20.0), "a cross-axis grow fills");
    Ok(())
}

#[test]
fn justify_distributes_leftover_space() -> TestResult {
    let src = |j: &str| {
        format!(
            "row id=r width=300 height=10 justify={j} {{
                panel id=a width=50 height=10 panel id=b width=50 height=10 panel id=c width=50 height=10 }}"
        )
    };
    let xs = |ui: &Ui| -> Result<[f32; 3], Box<dyn std::error::Error>> {
        Ok([rect(ui, "a")?.x, rect(ui, "b")?.x, rect(ui, "c")?.x])
    };
    assert_eq!(xs(&ui(&src("start"))?)?, [0.0, 50.0, 100.0]);
    assert_eq!(xs(&ui(&src("center"))?)?, [75.0, 125.0, 175.0]);
    assert_eq!(xs(&ui(&src("end"))?)?, [150.0, 200.0, 250.0]);
    assert_eq!(xs(&ui(&src("space-between"))?)?, [0.0, 125.0, 250.0]);
    Ok(())
}

#[test]
fn align_places_children_on_the_cross_axis() -> TestResult {
    let src = |a: &str| {
        format!("column id=c width=200 align={a} {{ panel id=p width=50 height=10 panel id=f height=10 }}")
    };
    let p = |ui: &Ui| rect(ui, "p");
    assert!(near(p(&ui(&src("start"))?)?.x, 0.0));
    assert!(near(p(&ui(&src("center"))?)?.x, 75.0));
    assert!(near(p(&ui(&src("end"))?)?.x, 150.0));
    let stretched = ui(&src("stretch"))?;
    assert!(near(rect(&stretched, "f")?.w, 200.0), "fit width stretches");
    assert!(near(rect(&stretched, "p")?.w, 50.0), "fixed width stays");
    Ok(())
}

#[test]
fn padding_gap_min_and_max() -> TestResult {
    let ui = ui("column id=c padding=10 gap=5 {
        panel id=a width=grow max_width=40 height=10
        panel id=b width=20 min_height=30
        row id=r padding_x=3 padding_y=1 { panel id=x width=7 height=7 }
    }")?;
    let (outer, grown, tall, row, inner) = (
        rect(&ui, "c")?,
        rect(&ui, "a")?,
        rect(&ui, "b")?,
        rect(&ui, "r")?,
        rect(&ui, "x")?,
    );
    assert!(near(grown.x, 10.0) && near(grown.y, 10.0));
    assert!(grown.w <= 40.0 + 0.01);
    assert!(near(tall.y, 25.0) && near(tall.h, 30.0));
    assert!(near(row.w, 13.0) && near(row.h, 9.0));
    assert!(near(inner.x, row.x + 3.0) && near(inner.y, row.y + 1.0));
    assert!(
        near(outer.h, 10.0 + 10.0 + 5.0 + 30.0 + 5.0 + 9.0 + 10.0),
        "{outer:?}"
    );
    Ok(())
}

#[test]
fn hidden_elements_take_no_space() -> TestResult {
    let mut ui = ui("column id=c gap=4 {
        panel id=a width=10 height=10
        panel id=b width=10 height=10 visible=\"dialog.open\"
        panel id=d width=10 height=10
    }")?;
    assert!(near(rect(&ui, "d")?.y, 28.0), "unset flag means visible");
    let flag = ui.properties_mut().intern("dialog.open");
    ui.properties_mut().set_bool(flag, false);
    ui.frame(VIEWPORT, 1.0);
    assert!(near(rect(&ui, "d")?.y, 14.0));
    assert!(near(rect(&ui, "c")?.h, 24.0));
    ui.properties_mut().set_bool(flag, true);
    ui.frame(VIEWPORT, 1.0);
    assert!(near(rect(&ui, "d")?.y, 28.0));
    Ok(())
}

#[test]
fn text_wraps_to_the_available_width() -> TestResult {
    // Latin glyphs advance 0.6 em: 9.6 px at size 16; each line is 16 px.
    let mut ui = ui(r#"column id=c width=60 { text id=t text="aaaa bbbb cccc dddd" }"#)?;
    let t = rect(&ui, "t")?;
    assert!(t.w <= 60.0);
    assert!(near(t.h, 64.0), "four lines: {t:?}");
    let shapes = ui.reshape_count("t");
    // Unchanged frames neither reshape nor relayout text.
    ui.frame(VIEWPORT, 1.0);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.reshape_count("t"), shapes);
    // A new viewport relayouts but does not reshape.
    ui.frame([1000.0, 700.0], 1.0);
    assert_eq!(ui.reshape_count("t"), shapes);
    Ok(())
}

#[test]
fn list_items_bind_fields_and_rebuild_on_length_change() -> TestResult {
    use mantis_ui::{ListItem, Value};
    let mut ui = ui(r#"list id=inv bind="inventory.items" {
        row { text id=label bind="item.name" button id=use text="Use" intent="item.use" payload="{item.slot}" }
    }"#)?;
    let items = ui.properties_mut().intern("inventory.items");
    let make = |names: &[&str]| {
        Value::List(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    ListItem::new()
                        .with("name", Value::text(n))
                        .with("slot", Value::Int(i64::try_from(i).unwrap_or(0)))
                })
                .collect(),
        )
    };
    ui.properties_mut().set(items, make(&["sword", "shield"]));
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("inv[0].label"), Some("sword"));
    assert_eq!(ui.text_of("inv[1].label"), Some("shield"));
    // Same length: fields refresh in place.
    ui.properties_mut().set(items, make(&["axe", "shield"]));
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("inv[0].label"), Some("axe"));
    // A longer list instantiates more items; a button in an item carries its payload.
    ui.properties_mut().set(items, make(&["axe", "shield", "bow"]));
    ui.frame(VIEWPORT, 1.0);
    let use2 = rect(&ui, "inv[2].use")?;
    let (x, y) = common::center(use2, 1.0);
    common::click_at(&mut ui, x, y);
    let mut out = Vec::new();
    ui.drain_intents(&mut out);
    assert_eq!(out.len(), 1);
    let intent = out.first().ok_or("intent")?;
    assert_eq!(ui.widget_name(intent.widget), Some("inv[2].use"));
    assert_eq!(intent.payload.as_deref(), Some("2"));
    ui.properties_mut().set(items, make(&[]));
    ui.frame(VIEWPORT, 1.0);
    assert!(ui.rect_of("inv[0].label").is_none());
    Ok(())
}
