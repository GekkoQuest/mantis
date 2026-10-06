//! Disabled widgets render as unavailable and never act.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

mod common;

use common::{TestResult, VIEWPORT, center, click_at, fonts};
use mantis_ui::draw::reference::rasterize;
use mantis_ui::{Handled, Modifiers, PointerButton, Ui, UiEvent, UiIntent, UiKey};

const LAYOUT: &str = r#"
theme {
  style button { background = #3060c0 }
  style button:hover { background = #ffffff }
}
panel id=root width=400 height=300 padding=8 gap=6 {
  button id=buy text="Buy" intent="shop.buy" enabled="feature.on" disabled_text="Unavailable"
  input id=name submit="name.set" enabled="name.editable"
  button id=after text="After" intent="after.press"
  row id=group enabled="group.on" {
    button id=inner text="Inner" intent="inner.press"
  }
}
"#;

fn key(k: UiKey) -> UiEvent {
    UiEvent::Key {
        key: k,
        pressed: true,
        modifiers: Modifiers::default(),
    }
}

const PRESS: UiEvent = UiEvent::PointerButton {
    button: PointerButton::Left,
    pressed: true,
};

const RELEASE: UiEvent = UiEvent::PointerButton {
    button: PointerButton::Left,
    pressed: false,
};

fn ui() -> Result<Ui, Box<dyn std::error::Error>> {
    let mut ui = Ui::new(fonts()?, LAYOUT, None)?;
    ui.frame(VIEWPORT, 1.0);
    Ok(ui)
}

fn set_flag(ui: &mut Ui, name: &str, on: bool) {
    let id = ui.properties_mut().intern(name);
    ui.properties_mut().set_bool(id, on);
}

fn drain(ui: &mut Ui) -> Vec<UiIntent> {
    let mut out = Vec::new();
    ui.drain_intents(&mut out);
    out
}

/// The rasterized color near the left edge of an element (its background).
fn background_of(ui: &mut Ui, id: &str) -> Result<[f32; 4], Box<dyn std::error::Error>> {
    ui.frame(VIEWPORT, 1.0);
    let r = ui.rect_of(id).ok_or("no rect")?;
    let a = ui.atlas();
    let img = rasterize(ui.draw_list(), a.pixels(), a.size(), 800, 600);
    Ok(img.get((r.x + 3.0) as u32, (r.y + r.h * 0.5) as u32))
}

#[test]
fn disabled_button_never_emits() -> TestResult {
    let mut ui = ui()?;
    assert_eq!(ui.is_enabled("buy"), Some(true), "unset flag means enabled");
    set_flag(&mut ui, "feature.on", true);
    ui.frame(VIEWPORT, 1.0);
    let (x, y) = center(ui.rect_of("buy").ok_or("buy")?, 1.0);
    click_at(&mut ui, x, y);
    assert_eq!(drain(&mut ui).len(), 1, "enabled: click emits");
    assert!(ui.set_focus("buy"));
    ui.handle(&key(UiKey::Enter));
    ui.handle(&key(UiKey::Space));
    assert_eq!(drain(&mut ui).len(), 2, "enabled: keyboard activation emits");

    // Disabled between frames: keyboard activation already does nothing.
    set_flag(&mut ui, "feature.on", false);
    assert_eq!(ui.is_enabled("buy"), Some(false));
    assert_eq!(ui.handle(&key(UiKey::Enter)), Handled::Ignored);
    assert_eq!(ui.focused(), None, "a disabled button loses focus");
    ui.frame(VIEWPORT, 1.0);
    let (x, y) = center(ui.rect_of("buy").ok_or("buy")?, 1.0);

    // Pointer presses are still consumed so they never reach the game.
    assert_eq!(ui.handle(&UiEvent::PointerMove { x, y }), Handled::Consumed);
    assert_eq!(ui.hovered(), None, "no hover on a disabled button");
    assert_eq!(ui.handle(&PRESS), Handled::Consumed);
    assert_eq!(ui.handle(&RELEASE), Handled::Consumed);
    assert!(!ui.set_focus("buy"));
    ui.handle(&key(UiKey::Enter));
    ui.handle(&key(UiKey::Space));
    assert!(drain(&mut ui).is_empty(), "disabled: nothing is emitted");

    // A press that started while enabled does not click once disabled.
    set_flag(&mut ui, "feature.on", true);
    ui.frame(VIEWPORT, 1.0);
    ui.handle(&UiEvent::PointerMove { x, y });
    ui.handle(&PRESS);
    set_flag(&mut ui, "feature.on", false);
    ui.handle(&RELEASE);
    assert!(drain(&mut ui).is_empty());
    Ok(())
}

#[test]
fn disabled_style_is_drawn() -> TestResult {
    let mut ui = ui()?;
    let enabled = background_of(&mut ui, "buy")?;
    let other = background_of(&mut ui, "after")?;
    set_flag(&mut ui, "feature.on", false);
    let disabled = background_of(&mut ui, "buy")?;
    assert_ne!(enabled, disabled);
    for (e, d) in enabled.iter().zip(disabled) {
        assert!(
            (d - e * 0.4).abs() < 0.01,
            "built-in 40% fade: {enabled:?} -> {disabled:?}"
        );
    }
    assert_eq!(
        background_of(&mut ui, "after")?,
        other,
        "enabled widgets keep their colors"
    );

    // Hovering a disabled button shows no hover style.
    let (x, y) = center(ui.rect_of("buy").ok_or("buy")?, 1.0);
    ui.handle(&UiEvent::PointerMove { x, y });
    assert_eq!(background_of(&mut ui, "buy")?, disabled);

    // A theme `:disabled` color wins over the built-in fade.
    let themed = LAYOUT.replace(
        "style button:hover { background = #ffffff }",
        "style button:hover { background = #ffffff }\n  style button:disabled { background = #ff0000 }",
    );
    ui.reload(&themed, None)?;
    let red = background_of(&mut ui, "buy")?;
    assert!(red[0] > 0.99 && red[1] < 0.01 && red[2] < 0.01, "{red:?}");
    Ok(())
}

#[test]
fn tab_skips_a_disabled_input_and_focus_is_lost_when_disabled() -> TestResult {
    let mut ui = ui()?;
    set_flag(&mut ui, "name.editable", false);
    ui.frame(VIEWPORT, 1.0);
    ui.handle(&key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("buy"));
    ui.handle(&key(UiKey::Tab));
    assert_eq!(
        ui.focused(),
        ui.widget_id("after"),
        "the disabled input is skipped"
    );

    set_flag(&mut ui, "name.editable", true);
    ui.frame(VIEWPORT, 1.0);
    let (x, y) = center(ui.rect_of("name").ok_or("name")?, 1.0);
    click_at(&mut ui, x, y);
    assert_eq!(ui.focused(), ui.widget_id("name"));
    ui.handle(&UiEvent::Text("abc".to_owned()));
    assert_eq!(ui.input_state("name").map(|s| s.text.as_str()), Some("abc"));

    set_flag(&mut ui, "name.editable", false);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.focused(), None, "focus is lost when disabled");
    assert!(ui.ime_cursor_area().is_none());
    // A disabled input ignores typing, IME, keys, and clicks.
    assert_eq!(ui.handle(&UiEvent::Text("x".to_owned())), Handled::Ignored);
    assert_eq!(
        ui.handle(&UiEvent::ImePreedit {
            text: "x".to_owned(),
            cursor: None
        }),
        Handled::Ignored
    );
    assert_eq!(ui.handle(&UiEvent::ImeCommit("x".to_owned())), Handled::Ignored);
    click_at(&mut ui, x, y);
    assert_eq!(ui.focused(), None);
    assert_eq!(ui.handle(&key(UiKey::Backspace)), Handled::Ignored);
    assert_eq!(ui.handle(&key(UiKey::Enter)), Handled::Ignored);
    assert_eq!(ui.input_state("name").map(|s| s.text.as_str()), Some("abc"));
    assert!(drain(&mut ui).is_empty());
    Ok(())
}

#[test]
fn disabled_text_swaps_the_label() -> TestResult {
    let mut ui = ui()?;
    assert_eq!(ui.text_of("buy"), Some("Buy"));
    set_flag(&mut ui, "feature.on", false);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("buy"), Some("Unavailable"));
    set_flag(&mut ui, "feature.on", true);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("buy"), Some("Buy"));
    // Without disabled_text the normal label stays.
    set_flag(&mut ui, "group.on", false);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("inner"), Some("Inner"));
    Ok(())
}

#[test]
fn a_disabled_container_disables_its_children() -> TestResult {
    let mut ui = ui()?;
    set_flag(&mut ui, "group.on", false);
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.is_enabled("group"), Some(false));
    assert_eq!(ui.is_enabled("inner"), Some(false));
    assert_eq!(ui.is_enabled("after"), Some(true));
    assert_eq!(ui.is_enabled("missing"), None);
    let (x, y) = center(ui.rect_of("inner").ok_or("inner")?, 1.0);
    ui.handle(&UiEvent::PointerMove { x, y });
    assert_eq!(ui.handle(&PRESS), Handled::Consumed);
    ui.handle(&RELEASE);
    assert!(drain(&mut ui).is_empty());
    assert!(!ui.set_focus("inner"));
    // Tab cycles over the enabled widgets only.
    for _ in 0..6 {
        ui.handle(&key(UiKey::Tab));
        assert_ne!(ui.focused(), ui.widget_id("inner"));
    }
    set_flag(&mut ui, "group.on", true);
    ui.frame(VIEWPORT, 1.0);
    click_at(&mut ui, x, y);
    let out = drain(&mut ui);
    assert_eq!(out.len(), 1);
    assert_eq!(out.first().map(|i| i.intent), ui.intent_id("inner.press"));
    Ok(())
}
