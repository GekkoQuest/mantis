//! The draw list rasterized with the CPU reference rasterizer: labels are
//! visible inside their backgrounds, lists clip their overflow, states change
//! colors, and the scale factor maps logical to physical pixels.

#![expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

mod common;

use common::{TestResult, VIEWPORT, center, sample_ui};
use mantis_ui::draw::reference::{Image, rasterize};
use mantis_ui::draw::{KIND_GLYPH, KIND_RECT};
use mantis_ui::{ListItem, Rect, Ui, UiEvent, Value};

fn render(ui: &mut Ui, scale: f32) -> Image {
    let w = VIEWPORT[0] * scale;
    let h = VIEWPORT[1] * scale;
    ui.frame([w, h], scale);
    let atlas = ui.atlas();
    rasterize(ui.draw_list(), atlas.pixels(), atlas.size(), w as u32, h as u32)
}

/// Pixels inside a physical rectangle whose color is near white.
fn bright_pixels(img: &Image, r: Rect) -> usize {
    let mut n = 0;
    for y in r.y.ceil() as u32..(r.y + r.h).floor() as u32 {
        for x in r.x.ceil() as u32..(r.x + r.w).floor() as u32 {
            let p = img.get(x, y);
            if p[0] > 0.8 && p[1] > 0.8 && p[2] > 0.8 {
                n += 1;
            }
        }
    }
    n
}

fn set_items(ui: &mut Ui, count: usize) {
    let props = ui.properties_mut();
    let items = props.intern("inventory.items");
    let list = (0..count)
        .map(|i| ListItem::new().with("name", Value::Text(format!("item {i}"))))
        .collect();
    props.set(items, Value::List(list));
}

#[test]
fn button_label_is_visible_inside_its_background() -> TestResult {
    let mut ui = sample_ui()?;
    let scale = 2.0;
    let img = render(&mut ui, scale);
    let ok = ui.rect_of("ok").ok_or("ok")?.scaled(scale);
    let label_pixels = bright_pixels(&img, ok);
    assert!(label_pixels > 30, "label pixels: {label_pixels}");
    // The button's corner area shows the background, not the label.
    let bg = img.get((ok.x + 4.0) as u32, (ok.y + ok.h * 0.5) as u32);
    assert!(bg[3] > 0.99 && bg[0] < 0.1, "background {bg:?}");
    // Every glyph quad of the label lies inside the button.
    let glyphs_inside = ui
        .draw_list()
        .quads
        .iter()
        .filter(|q| q.params[2] == KIND_GLYPH)
        .filter(|q| {
            let cx = q.rect[0] + q.rect[2] * 0.5;
            let cy = q.rect[1] + q.rect[3] * 0.5;
            ok.contains(cx, cy)
        })
        .count();
    assert_eq!(glyphs_inside, 2, "\"OK\" is two glyphs");
    // Text is drawn after (above) its button background.
    let quads = &ui.draw_list().quads;
    let bg_index = quads
        .iter()
        .position(|q| {
            q.params[2] == KIND_RECT && (q.rect[0] - ok.x).abs() < 0.01 && (q.rect[1] - ok.y).abs() < 0.01
        })
        .ok_or("button background quad")?;
    let glyph_index = quads
        .iter()
        .position(|q| {
            q.params[2] == KIND_GLYPH && ok.contains(q.rect[0] + q.rect[2] * 0.5, q.rect[1] + q.rect[3] * 0.5)
        })
        .ok_or("label glyph")?;
    assert!(bg_index < glyph_index);
    Ok(())
}

#[test]
fn overflowing_list_is_clipped_and_scrolls() -> TestResult {
    let mut ui = sample_ui()?;
    set_items(&mut ui, 10);
    let scale = 1.0;
    let img = render(&mut ui, scale);
    let list = ui.rect_of("items").ok_or("items")?;
    assert!((list.h - 40.0).abs() < 0.01);
    // Ten items of 16 px text overflow a 40 px list.
    let tail_item = ui.rect_of("items[9]").ok_or("item 9")?;
    assert!(
        tail_item.y > list.y + list.h,
        "item 9 at {tail_item:?}, list {list:?}"
    );
    // Visible items draw inside the list...
    assert!(bright_pixels(&img, list) > 50);
    // ...and nothing draws below it, although the item quads extend there.
    let below = Rect::new(list.x, list.y + list.h + 1.0, list.w, 60.0);
    assert_eq!(bright_pixels(&img, below), 0);
    let clipped_glyphs = ui
        .draw_list()
        .quads
        .iter()
        .filter(|q| q.params[2] == KIND_GLYPH && q.clip[3] <= list.y + list.h + 0.01)
        .count();
    assert!(clipped_glyphs > 0);
    for q in &ui.draw_list().quads {
        assert!(
            q.rect[1] < q.clip[3] && q.rect[1] + q.rect[3] > q.clip[1],
            "culled outside clip"
        );
    }

    // The wheel over the list scrolls it.
    let (x, y) = center(list, scale);
    ui.handle(&UiEvent::PointerMove { x, y });
    let first_before = ui.rect_of("items[0]").ok_or("item 0")?;
    assert!(ui.handle(&UiEvent::Wheel { dx: 0.0, dy: -1.0 }).consumed());
    ui.frame(VIEWPORT, scale);
    let first_after = ui.rect_of("items[0]").ok_or("item 0")?;
    assert!(first_after.y < first_before.y);
    // Scrolling stops at the end of the content.
    for _ in 0..20 {
        ui.handle(&UiEvent::Wheel { dx: 0.0, dy: -1.0 });
    }
    ui.frame(VIEWPORT, scale);
    let tail_item = ui.rect_of("items[9]").ok_or("item 9")?;
    assert!(
        (tail_item.y + tail_item.h - (list.y + list.h)).abs() < 0.01,
        "{tail_item:?} {list:?}"
    );
    Ok(())
}

#[test]
fn hover_and_press_change_colors_without_relayout() -> TestResult {
    let mut ui = sample_ui()?;
    let img = render(&mut ui, 1.0);
    let ok = ui.rect_of("ok").ok_or("ok")?;
    let probe = ((ok.x + 3.0) as u32, (ok.y + ok.h * 0.5) as u32);
    let normal = img.get(probe.0, probe.1);
    let (x, y) = center(ok, 1.0);
    ui.handle(&UiEvent::PointerMove { x, y });
    let reshapes = ui.reshape_count("ok");
    let img = render(&mut ui, 1.0);
    let hovered = img.get(probe.0, probe.1);
    assert!(hovered[0] > normal[0] + 0.01, "{normal:?} -> {hovered:?}");
    assert_eq!(ui.reshape_count("ok"), reshapes);
    ui.handle(&UiEvent::PointerButton {
        button: mantis_ui::PointerButton::Left,
        pressed: true,
    });
    let img = render(&mut ui, 1.0);
    let pressed = img.get(probe.0, probe.1);
    assert!(pressed[0] < normal[0], "{normal:?} -> {pressed:?}");
    Ok(())
}

#[test]
fn scale_maps_logical_to_physical() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    let at1: Vec<[f32; 4]> = ui.draw_list().quads.iter().map(|q| q.rect).collect();
    ui.frame([VIEWPORT[0] * 2.0, VIEWPORT[1] * 2.0], 2.0);
    let at2: Vec<[f32; 4]> = ui.draw_list().quads.iter().map(|q| q.rect).collect();
    assert_eq!(at1.len(), at2.len());
    for (a, b) in at1.iter().zip(&at2) {
        for (x, y) in a.iter().zip(b) {
            assert!((x * 2.0 - y).abs() < 0.01, "{a:?} vs {b:?}");
        }
    }
    // Glyph screen-pixel range scales with the glyph.
    let r1 = ui
        .draw_list()
        .quads
        .iter()
        .find(|q| q.params[2] == KIND_GLYPH)
        .map(|q| q.params[3]);
    assert!(r1.is_some_and(|r| r > 0.0));
    Ok(())
}

#[test]
fn templates_and_binds_render_property_values() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("hp"), Some("HP 90 / 100"));
    assert_eq!(ui.text_of("name"), Some("player"));
    let hp = ui.properties_mut().intern("hp");
    ui.properties_mut().set(
        hp,
        Value::Fixed {
            value: 455,
            decimals: 1,
        },
    );
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("hp"), Some("HP 45.5 / 100"));
    // Unset properties render as nothing.
    let doc = r#"panel { text id=t template="[{missing}]" }"#;
    let mut ui = Ui::new(common::fonts()?, doc, None)?;
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("t"), Some("[]"));
    Ok(())
}
