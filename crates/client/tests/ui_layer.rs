//! The UI sees platform events before game actions: pointing at a widget, typing into a
//! focused text field (including IME), and clicking buttons are consumed; everything else
//! and every release reach game actions.

use mantis_client::input::device::{ButtonSource, KeyCode, MouseButton, RawInput};
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::ui_layer::UiLayer;
use mantis_ui::{FontLibrary, Ui, test_font};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const LAYOUT: &str = r#"
panel id=root direction=column gap=8 width=300 height=200 {
  button id=ok text="OK" intent="dialog.confirm"
  input id=chat placeholder="Say something" submit="chat.send"
}
"#;

fn layer() -> Result<UiLayer, Box<dyn std::error::Error>> {
    let mut fonts = FontLibrary::new();
    let latin = fonts.add_font(test_font::latin())?;
    let cjk = fonts.add_font(test_font::cjk())?;
    fonts.define_stack("ui", &[latin, cjk])?;
    let mut ui = Ui::new(fonts, LAYOUT, None)?;
    let _ = ui.frame([800.0, 600.0], 1.0);
    Ok(UiLayer::new(ui))
}

fn key(k: KeyCode, pressed: bool) -> PlatformEvent {
    PlatformEvent::Input(RawInput::Button {
        source: ButtonSource::Key(k),
        pressed,
    })
}

fn mouse(pressed: bool) -> PlatformEvent {
    PlatformEvent::Input(RawInput::Button {
        source: ButtonSource::Mouse(MouseButton::Left),
        pressed,
    })
}

fn center_of(l: &UiLayer, id: &str) -> Result<PlatformEvent, Box<dyn std::error::Error>> {
    let r = l.ui().rect_of(id).ok_or("widget")?;
    Ok(PlatformEvent::CursorMoved {
        x: r.x + r.w * 0.5,
        y: r.y + r.h * 0.5,
    })
}

#[test]
fn the_ui_consumes_what_it_uses_and_releases_always_reach_actions() -> TestResult {
    let mut l = layer()?;
    // Nothing focused, pointer over empty space: movement keys are game input.
    let _ = l.handle(&PlatformEvent::CursorMoved { x: 700.0, y: 500.0 });
    assert!(!l.handle(&key(KeyCode::W, true)));
    assert!(!l.handle(&key(KeyCode::W, false)));
    // Clicking the text field focuses it; the press is the UI's, the release is shared.
    let at = center_of(&l, "chat")?;
    let _ = l.handle(&at);
    assert!(l.handle(&mouse(true)));
    assert!(!l.handle(&mouse(false)));
    // Typing goes to the field, not to movement.
    assert!(l.handle(&key(KeyCode::W, true)));
    assert!(l.handle(&PlatformEvent::Text("w".to_owned())));
    assert!(
        !l.handle(&key(KeyCode::W, false)),
        "releases always reach actions"
    );
    // IME composition and commit go to the field.
    assert!(l.handle(&PlatformEvent::ImePreedit {
        text: "한".to_owned(),
        cursor: Some((0, 3))
    }));
    assert!(l.handle(&PlatformEvent::ImeCommit("한".to_owned())));
    assert_eq!(l.ui().text_of("chat"), Some("w한"));
    // Enter submits the field as an intent.
    assert!(l.handle(&key(KeyCode::Enter, true)));
    let mut intents = Vec::new();
    l.drain_intents(|i| intents.push(i.payload.clone()));
    assert_eq!(intents, [Some("w한".to_owned())]);
    // Escape leaves the field; movement keys are game input again.
    assert!(l.handle(&key(KeyCode::Escape, true)));
    assert!(!l.handle(&key(KeyCode::W, true)));
    // A button click is an intent.
    let at = center_of(&l, "ok")?;
    let _ = l.handle(&at);
    assert!(l.handle(&mouse(true)));
    let _ = l.handle(&mouse(false));
    let mut names = Vec::new();
    l.drain_intents(|i| names.push(i.intent));
    assert_eq!(names.len(), 1);
    assert_eq!(
        l.ui().intent_name(names.first().copied().ok_or("intent")?),
        Some("dialog.confirm")
    );
    Ok(())
}
