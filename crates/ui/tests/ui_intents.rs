//! Widgets emit intents; input is consumed or ignored for routing.

mod common;

use common::{TestResult, VIEWPORT, center, click_at, sample_ui};
use mantis_ui::draw::KIND_RECT;
use mantis_ui::{Handled, Modifiers, PointerButton, UiEvent, UiIntent, UiKey};

fn key(k: UiKey) -> UiEvent {
    UiEvent::Key {
        key: k,
        pressed: true,
        modifiers: Modifiers::default(),
    }
}

fn shift_key(k: UiKey) -> UiEvent {
    UiEvent::Key {
        key: k,
        pressed: true,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    }
}

#[test]
fn clicking_a_button_emits_exactly_one_intent() -> TestResult {
    let mut ui = sample_ui()?;
    let scale = 2.0;
    ui.frame([VIEWPORT[0] * scale, VIEWPORT[1] * scale], scale);
    let ok = ui.rect_of("ok").ok_or("ok rect")?;
    assert!(ok.w > 0.0 && ok.h > 0.0);
    let (x, y) = center(ok, scale);

    assert_eq!(ui.handle(&UiEvent::PointerMove { x, y }), Handled::Consumed);
    assert_eq!(ui.hovered(), ui.widget_id("ok"));
    let press = UiEvent::PointerButton {
        button: PointerButton::Left,
        pressed: true,
    };
    let release = UiEvent::PointerButton {
        button: PointerButton::Left,
        pressed: false,
    };
    assert_eq!(ui.handle(&press), Handled::Consumed);
    let mut out: Vec<UiIntent> = Vec::new();
    ui.drain_intents(&mut out);
    assert!(out.is_empty(), "press alone emits nothing");
    assert_eq!(ui.handle(&release), Handled::Consumed);
    ui.drain_intents(&mut out);
    assert_eq!(out.len(), 1);
    let intent = out.first().ok_or("intent")?;
    assert_eq!(Some(intent.intent), ui.intent_id("dialog.confirm"));
    assert_eq!(Some(intent.widget), ui.widget_id("ok"));
    assert_eq!(ui.intent_name(intent.intent), Some("dialog.confirm"));
    assert_eq!(ui.widget_name(intent.widget), Some("ok"));
    assert_eq!(intent.payload.as_deref(), Some("7"), "payload template");

    // Press inside, release outside: no click.
    out.clear();
    ui.handle(&press);
    ui.handle(&UiEvent::PointerMove {
        x: 790.0 * scale,
        y: 590.0 * scale,
    });
    ui.handle(&release);
    ui.drain_intents(&mut out);
    assert!(out.is_empty());

    // Clicks outside the UI are not consumed.
    ui.handle(&UiEvent::PointerMove {
        x: 700.0 * scale,
        y: 500.0 * scale,
    });
    assert_eq!(ui.handle(&press), Handled::Ignored);
    assert_eq!(ui.handle(&release), Handled::Ignored);
    ui.drain_intents(&mut out);
    assert!(out.is_empty());
    Ok(())
}

#[test]
fn ime_preedit_commit_then_enter_submits() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    let chat = ui.rect_of("chat").ok_or("chat rect")?;
    let (x, y) = center(chat, 1.0);
    click_at(&mut ui, x, y);
    assert_eq!(ui.focused(), ui.widget_id("chat"));
    assert!(ui.ime_cursor_area().is_some());

    // Composition is shown inline, underlined, and not committed.
    let pre = UiEvent::ImePreedit {
        text: "한".to_owned(),
        cursor: Some((3, 3)),
    };
    assert_eq!(ui.handle(&pre), Handled::Consumed);
    ui.frame(VIEWPORT, 1.0);
    let state = ui.input_state("chat").ok_or("state")?;
    assert!(state.text.is_empty());
    assert_eq!(state.preedit, "한");
    assert_eq!(ui.text_of("chat"), Some("한"));
    let underline = ui.draw_list().quads.iter().any(|q| {
        q.params[2] == KIND_RECT
            && q.rect[3] <= 2.0
            && q.rect[2] > 4.0
            && q.rect[0] >= chat.x
            && q.rect[0] + q.rect[2] <= chat.x + chat.w
            && q.rect[1] > chat.y
            && q.rect[1] < chat.y + chat.h
    });
    assert!(underline, "preedit underline quad");
    let caret_with_preedit = ui.ime_cursor_area().ok_or("ime area")?;

    // Commit, then Enter submits the committed text.
    assert_eq!(
        ui.handle(&UiEvent::ImeCommit("한글".to_owned())),
        Handled::Consumed
    );
    ui.frame(VIEWPORT, 1.0);
    let state = ui.input_state("chat").ok_or("state")?;
    assert_eq!(state.text, "한글");
    assert!(state.preedit.is_empty());
    let caret_after = ui.ime_cursor_area().ok_or("ime area")?;
    assert!(caret_after.x > caret_with_preedit.x - 0.5);

    assert_eq!(ui.handle(&key(UiKey::Enter)), Handled::Consumed);
    let mut out = Vec::new();
    ui.drain_intents(&mut out);
    assert_eq!(out.len(), 1);
    let submit = out.first().ok_or("submit")?;
    assert_eq!(Some(submit.intent), ui.intent_id("chat.send"));
    assert_eq!(Some(submit.widget), ui.widget_id("chat"));
    assert_eq!(submit.payload.as_deref(), Some("한글"));
    assert_eq!(ui.input_state("chat").map(|s| s.text.as_str()), Some(""));
    Ok(())
}

#[test]
fn text_editing_by_grapheme() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    assert!(ui.set_focus("chat"));
    // "e" + combining acute accent is one grapheme.
    ui.handle(&UiEvent::Text("abe\u{301}c".to_owned()));
    assert_eq!(ui.input_state("chat").map(|s| s.caret), Some(6));
    ui.handle(&key(UiKey::Left));
    ui.handle(&key(UiKey::Left));
    assert_eq!(
        ui.input_state("chat").map(|s| s.caret),
        Some(2),
        "skips the whole cluster"
    );
    ui.handle(&key(UiKey::Delete));
    assert_eq!(ui.input_state("chat").map(|s| s.text.as_str()), Some("abc"));
    ui.handle(&key(UiKey::End));
    ui.handle(&key(UiKey::Backspace));
    assert_eq!(ui.input_state("chat").map(|s| s.text.as_str()), Some("ab"));
    ui.handle(&shift_key(UiKey::Home));
    assert_eq!(
        ui.input_state("chat").map(mantis_ui::tree::InputState::selection),
        Some(0..2)
    );
    ui.handle(&UiEvent::Text("xyz".to_owned()));
    assert_eq!(ui.input_state("chat").map(|s| s.text.as_str()), Some("xyz"));
    ui.handle(&key(UiKey::Home));
    ui.handle(&UiEvent::Text("\u{7}w".to_owned()));
    assert_eq!(
        ui.input_state("chat").map(|s| s.text.as_str()),
        Some("wxyz"),
        "control characters are dropped"
    );
    ui.frame(VIEWPORT, 1.0);
    assert_eq!(ui.text_of("chat"), Some("wxyz"));
    Ok(())
}

#[test]
fn keys_route_to_the_game_unless_the_ui_uses_them() -> TestResult {
    let mut ui = sample_ui()?;
    ui.frame(VIEWPORT, 1.0);
    let w = UiEvent::Key {
        key: UiKey::Char('w'),
        pressed: true,
        modifiers: Modifiers::default(),
    };
    assert_eq!(ui.handle(&w), Handled::Ignored, "no focus: game key");
    assert_eq!(ui.handle(&UiEvent::Text("w".to_owned())), Handled::Ignored);

    // Tab traversal: ok -> cancel -> chat -> ok; Shift+Tab goes back.
    assert_eq!(ui.handle(&key(UiKey::Tab)), Handled::Consumed);
    assert_eq!(ui.focused(), ui.widget_id("ok"));
    ui.handle(&key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("cancel"));
    ui.handle(&key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("chat"));
    assert_eq!(ui.handle(&w), Handled::Consumed, "typing swallows keys");
    ui.handle(&key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("ok"));
    ui.handle(&shift_key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("chat"));
    ui.handle(&shift_key(UiKey::Tab));
    assert_eq!(ui.focused(), ui.widget_id("cancel"));

    // Enter on a focused button clicks it.
    ui.handle(&key(UiKey::Enter));
    let mut out = Vec::new();
    ui.drain_intents(&mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out.first().map(|i| i.intent), ui.intent_id("dialog.cancel"));

    // Escape drops focus; then keys go to the game again.
    assert_eq!(ui.handle(&key(UiKey::Escape)), Handled::Consumed);
    assert_eq!(ui.focused(), None);
    assert_eq!(ui.handle(&key(UiKey::Escape)), Handled::Ignored);
    assert_eq!(ui.handle(&UiEvent::FocusLost), Handled::Ignored);
    Ok(())
}
