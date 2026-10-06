//! Shared fixtures for the UI integration tests.

#![allow(dead_code)] // Shared by several test targets; each uses a different subset, so `expect` would fail in some.

use mantis_ui::{FontLibrary, PointerButton, Rect, Ui, UiEvent, test_font};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

pub const THEME: &str = r#"
theme {
  style title { font = "ui" size = 24 color = #ffffff }
  style panel { background = #20242cee radius = 6 padding = 8 border = 1 border_color = #ffffff22 }
  style button { background = #303846 }
  style button:hover { background = #4a5468 }
  style button:pressed { background = #1c2028 }
}
"#;

pub const LAYOUT: &str = r#"
panel id=root style=panel direction=column gap=4 width=400 height=300 {
  text id=name bind="player.name" style=title
  text id=hp template="HP {hp} / {hp_max}"
  row id=buttons gap=8 {
    button id=ok text="OK" intent="dialog.confirm" payload="{dialog.id}" enabled="dialog.can_confirm"
    button id=cancel text="Cancel" intent="dialog.cancel"
  }
  input id=chat placeholder="Say something" submit="chat.send"
  list id=items bind="inventory.items" height=40 width=grow { text bind="item.name" }
}
"#;

/// Latin, CJK, and RTL test fonts in a stack named `ui` (also the default).
pub fn fonts() -> Result<FontLibrary, Box<dyn std::error::Error>> {
    let mut lib = FontLibrary::new();
    let latin = lib.add_font(test_font::latin())?;
    let cjk = lib.add_font(test_font::cjk())?;
    let rtl = lib.add_font(test_font::rtl())?;
    lib.define_stack("ui", &[latin, cjk, rtl])?;
    Ok(lib)
}

/// The sample UI with a few properties set.
pub fn sample_ui() -> Result<Ui, Box<dyn std::error::Error>> {
    let mut ui = Ui::new(fonts()?, LAYOUT, Some(THEME))?;
    let props = ui.properties_mut();
    let name = props.intern("player.name");
    props.set_text(name, "player");
    let hp = props.intern("hp");
    props.set_int(hp, 90);
    let hp_max = props.intern("hp_max");
    props.set_int(hp_max, 100);
    let dialog = props.intern("dialog.id");
    props.set_int(dialog, 7);
    Ok(ui)
}

pub const VIEWPORT: [f32; 2] = [800.0, 600.0];

/// Center of a logical rectangle in physical pixels.
pub fn center(r: Rect, scale: f32) -> (f32, f32) {
    ((r.x + r.w * 0.5) * scale, (r.y + r.h * 0.5) * scale)
}

/// Moves to a physical point, presses and releases the left button.
pub fn click_at(ui: &mut Ui, x: f32, y: f32) {
    ui.handle(&UiEvent::PointerMove { x, y });
    ui.handle(&UiEvent::PointerButton {
        button: PointerButton::Left,
        pressed: true,
    });
    ui.handle(&UiEvent::PointerButton {
        button: PointerButton::Left,
        pressed: false,
    });
}
