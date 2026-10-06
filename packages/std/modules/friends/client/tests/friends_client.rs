//! The std.friends client half against its contract codec: the start-up ask, requests,
//! pending lists, declines, and friend lists drive the view model; intents send the
//! contract's requests; run-time refusals gate the module; the screen's input asks on
//! Enter.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{ListItem, Modifiers, Properties, UiEvent, UiKey, Value};
use std_friends_client::PROMPTS;
use std_friends_contract::{Declined, FriendList, Pending, Remove, Request, Requested, Respond, ShowFriends};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = [Discovered {
        origin: "std".to_owned(),
        manifest: parse_manifest(MANIFEST)?,
    }];
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

/// Installs and starts the module, leaving the start-up messages queued.
fn start(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_friends_client::Module) as Arc<dyn ClientModule>],
    )?;
    let mut props = Properties::new();
    modules.start(&mut props);
    Ok((modules, props))
}

/// Installs and starts the module, and drains the start-up messages.
fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let (mut m, props) = start(flags)?;
    let _ = sent(&mut m);
    Ok((m, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

fn friend_list(friends: &[u64], present: u64) -> Result<FriendList, Box<dyn std::error::Error>> {
    Ok(FriendList {
        friends: BoundedArray::from_slice(friends).ok_or("friends")?,
        present,
    })
}

fn pending(incoming: &[u64], outgoing: &[u64]) -> Result<Pending, Box<dyn std::error::Error>> {
    Ok(Pending {
        incoming: BoundedArray::from_slice(incoming).ok_or("incoming")?,
        outgoing: BoundedArray::from_slice(outgoing).ok_or("outgoing")?,
    })
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

fn list(props: &mut Properties, name: &str) -> Result<Vec<ListItem>, Box<dyn std::error::Error>> {
    match prop(props, name) {
        Some(Value::List(items)) => Ok(items),
        other => Err(format!("{name}: {other:?}").into()),
    }
}

/// The `character` fields of a list property.
fn characters(props: &mut Properties, name: &str) -> Result<Vec<Option<Value>>, Box<dyn std::error::Error>> {
    Ok(list(props, name)?
        .iter()
        .map(|i| i.field("character").cloned())
        .collect())
}

fn field(item: Option<&ListItem>, name: &str) -> Option<Value> {
    item.and_then(|i| i.field(name)).cloned()
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn texts(list: &[&str]) -> Vec<Option<Value>> {
    list.iter().map(|s| Some(text(s))).collect()
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

#[test]
fn start_asks_for_the_list_and_the_requests() -> TestResult {
    let (mut m, mut props) = start("")?;
    let out = sent(&mut m);
    let kinds: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [ShowFriends::ID.0]);
    assert_eq!(
        decode_message::<ShowFriends>(&out.first().ok_or("show")?.1)?,
        ShowFriends {}
    );
    assert_eq!(prop(&mut props, "std.friends.enabled"), Some(Value::Bool(true)));
    assert_eq!(
        prop(&mut props, "std.friends.unavailable"),
        Some(Value::Bool(false))
    );
    assert_eq!(prop(&mut props, "std.friends.open"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.friends.count"), Some(Value::Int(0)));
    assert_eq!(
        prop(&mut props, "std.friends.requested"),
        Some(Value::Bool(false))
    );
    assert_eq!(prop(&mut props, "std.friends.declined"), Some(Value::Bool(false)));
    assert!(list(&mut props, "std.friends.incoming")?.is_empty());
    assert!(list(&mut props, "std.friends.outgoing")?.is_empty());

    assert_eq!(
        m.on_intent("std.friends.show", None, &mut props),
        IntentRoute::Handled,
        "the list can be asked for again"
    );
    let out = sent(&mut m);
    assert_eq!(
        out.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        [ShowFriends::ID.0]
    );

    let (mut m, _) = start("\"std.friends.enabled\" = false\n")?;
    assert!(sent(&mut m).is_empty(), "a disabled module asks nothing");
    Ok(())
}

#[test]
fn pending_lists_are_the_source_of_truth() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &Requested { from: 42 });
    assert_eq!(
        prop(&mut props, "std.friends.requested"),
        Some(Value::Bool(true)),
        "the prompt shows before Pending arrives"
    );
    assert_eq!(prop(&mut props, "std.friends.request_from"), Some(text("42")));
    deliver(&mut m, &mut props, &Requested { from: 42 });
    assert_eq!(
        prop(&mut props, "std.friends.pending"),
        Some(Value::Int(1)),
        "a repeated request is one prompt"
    );

    deliver(&mut m, &mut props, &pending(&[7, 42], &[9, 11, 13])?);
    assert_eq!(prop(&mut props, "std.friends.pending"), Some(Value::Int(2)));
    assert_eq!(prop(&mut props, "std.friends.asked"), Some(Value::Int(3)));
    assert_eq!(prop(&mut props, "std.friends.request_from"), Some(text("7")));
    assert_eq!(
        characters(&mut props, "std.friends.incoming")?,
        texts(&["7", "42"])
    );
    assert_eq!(
        characters(&mut props, "std.friends.outgoing")?,
        texts(&["9", "11", "13"])
    );

    deliver(&mut m, &mut props, &pending(&[], &[11])?);
    assert_eq!(
        prop(&mut props, "std.friends.requested"),
        Some(Value::Bool(false))
    );
    assert_eq!(prop(&mut props, "std.friends.request_from"), Some(text("")));
    assert_eq!(prop(&mut props, "std.friends.asked"), Some(Value::Int(1)));

    m.on_message(ExtensionKind(Pending::ID.0), &[3, 0], &mut props);
    m.on_message(ExtensionKind(Requested::ID.0), &[1], &mut props);
    assert_eq!(
        m.stats().malformed,
        2,
        "malformed messages are counted, never errors"
    );
    assert_eq!(prop(&mut props, "std.friends.asked"), Some(Value::Int(1)));
    Ok(())
}

#[test]
fn answers_send_respond_for_waiting_requests_only() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &pending(&[42, 7], &[])?);
    assert_eq!(
        m.on_intent("std.friends.accept", None, &mut props),
        IntentRoute::Handled,
        "without a payload, the first request"
    );
    assert_eq!(prop(&mut props, "std.friends.request_from"), Some(text("7")));
    assert_eq!(
        m.on_intent("std.friends.decline", Some("7"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.friends.requested"),
        Some(Value::Bool(false))
    );
    let out = sent(&mut m);
    let kinds: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [Respond::ID.0, Respond::ID.0]);
    assert_eq!(
        decode_message::<Respond>(&out.first().ok_or("accept")?.1)?,
        Respond {
            character: 42,
            accept: true
        }
    );
    assert_eq!(
        decode_message::<Respond>(&out.get(1).ok_or("decline")?.1)?,
        Respond {
            character: 7,
            accept: false
        }
    );

    deliver(&mut m, &mut props, &pending(&[5], &[])?);
    for (name, payload) in [
        ("std.friends.accept", Some("99")),
        ("std.friends.decline", Some("nobody")),
    ] {
        assert_eq!(
            m.on_intent(name, payload, &mut props),
            IntentRoute::Refused,
            "{name} {payload:?}: not a waiting request"
        );
    }
    assert_eq!(
        m.on_intent("std.friends.decline", None, &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("std.friends.accept", None, &mut props),
        IntentRoute::Refused,
        "nothing waiting"
    );
    assert_eq!(sent(&mut m).len(), 1);
    Ok(())
}

#[test]
fn a_decline_shows_a_notice_until_dismissed() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &pending(&[], &[9, 11])?);
    deliver(&mut m, &mut props, &Declined { by: 9 });
    assert_eq!(prop(&mut props, "std.friends.declined"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.friends.declined_by"), Some(text("9")));
    assert_eq!(
        characters(&mut props, "std.friends.outgoing")?,
        texts(&["11"]),
        "the declined request no longer waits"
    );
    assert_eq!(
        m.on_intent("std.friends.dismiss", None, &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.friends.declined"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.friends.declined_by"), Some(text("")));
    assert!(sent(&mut m).is_empty(), "dismissing is local");
    m.on_message(ExtensionKind(Declined::ID.0), &[], &mut props);
    assert_eq!(m.stats().malformed, 1);
    Ok(())
}

#[test]
fn the_friend_list_drives_the_view_model() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &friend_list(&[7, 9, 42], 0b101)?);
    assert_eq!(prop(&mut props, "std.friends.count"), Some(Value::Int(3)));
    assert_eq!(prop(&mut props, "std.friends.here"), Some(Value::Int(2)));
    let friends = list(&mut props, "std.friends.friends")?;
    assert_eq!(friends.len(), 3);
    for (i, (character, present)) in [("7", true), ("9", false), ("42", true)].into_iter().enumerate() {
        let item = friends.get(i);
        assert_eq!(field(item, "character"), Some(text(character)));
        assert_eq!(field(item, "present"), Some(Value::Bool(present)));
        assert_eq!(
            field(item, "presence"),
            Some(text(if present { "here" } else { "away" }))
        );
    }

    deliver(&mut m, &mut props, &friend_list(&[], 0)?);
    assert_eq!(prop(&mut props, "std.friends.count"), Some(Value::Int(0)));
    assert_eq!(prop(&mut props, "std.friends.here"), Some(Value::Int(0)));
    assert!(list(&mut props, "std.friends.friends")?.is_empty());

    m.on_message(ExtensionKind(FriendList::ID.0), &[200, 0, 0], &mut props);
    assert_eq!(
        m.stats().malformed,
        1,
        "a malformed list is counted, never an error"
    );
    Ok(())
}

#[test]
fn add_and_remove_send_the_contract_requests() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.friends.add", Some("42"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("std.friends.remove", Some(" 7 "), &mut props),
        IntentRoute::Handled
    );
    let out = sent(&mut m);
    let kinds: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [Request::ID.0, Remove::ID.0]);
    assert_eq!(
        decode_message::<Request>(&out.first().ok_or("request")?.1)?,
        Request { character: 42 }
    );
    assert_eq!(
        decode_message::<Remove>(&out.get(1).ok_or("remove")?.1)?,
        Remove { character: 7 }
    );
    for name in ["std.friends.add", "std.friends.remove"] {
        for payload in [None, Some(""), Some("player"), Some("0"), Some("-1")] {
            assert_eq!(
                m.on_intent(name, payload, &mut props),
                IntentRoute::Refused,
                "{name} {payload:?}"
            );
        }
    }
    assert!(sent(&mut m).is_empty(), "refused intents send nothing");
    Ok(())
}

#[test]
fn request_prompts_stay_bounded() -> TestResult {
    let (mut m, mut props) = install("")?;
    let total = u64::try_from(PROMPTS * 5)?;
    for from in 1..=total {
        deliver(&mut m, &mut props, &Requested { from });
    }
    let held = i64::try_from(PROMPTS)?;
    assert_eq!(prop(&mut props, "std.friends.pending"), Some(Value::Int(held)));
    assert_eq!(list(&mut props, "std.friends.incoming")?.len(), PROMPTS);
    assert_eq!(
        prop(&mut props, "std.friends.request_from"),
        Some(text("1")),
        "the first prompts stay until Pending says otherwise"
    );
    Ok(())
}

#[test]
fn refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &friend_list(&[7], 1)?);
    deliver(&mut m, &mut props, &pending(&[42], &[9])?);
    deliver(&mut m, &mut props, &Declined { by: 11 });
    m.on_refused(
        ExtensionKind(Request::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.friends.refusal"), Some(text("not allowed")));
    assert_eq!(prop(&mut props, "std.friends.enabled"), Some(Value::Bool(true)));

    m.on_refused(
        ExtensionKind(Remove::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.friends.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.friends.unavailable"),
        Some(Value::Bool(true)),
        "the screen says so"
    );
    assert_eq!(prop(&mut props, "std.friends.count"), Some(Value::Int(0)));
    assert_eq!(
        prop(&mut props, "std.friends.requested"),
        Some(Value::Bool(false))
    );
    assert_eq!(prop(&mut props, "std.friends.asked"), Some(Value::Int(0)));
    assert_eq!(prop(&mut props, "std.friends.declined"), Some(Value::Bool(false)));
    for (name, payload) in [
        ("std.friends.add", Some("42")),
        ("std.friends.remove", Some("7")),
        ("std.friends.accept", None),
        ("std.friends.decline", None),
        ("std.friends.show", None),
    ] {
        assert_eq!(
            m.on_intent(name, payload, &mut props),
            IntentRoute::Refused,
            "{name}"
        );
    }
    deliver(&mut m, &mut props, &Requested { from: 5 });
    assert_eq!(m.stats().disabled, 6, "five intents and one message dropped");
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("\"std.friends.enabled\" = false\n")?;
    assert_eq!(prop(&mut props, "std.friends.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        m.on_intent("std.friends.add", Some("42"), &mut props),
        IntentRoute::Refused,
        "disabled by the package"
    );
    Ok(())
}

fn ui_for(m: &ClientModules) -> Result<mantis_ui::Ui, Box<dyn std::error::Error>> {
    let (layout, theme) = m.compose(&["std.friends.list"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    Ok(mantis_ui::Ui::new(fonts, &layout, Some(&theme))?)
}

#[test]
fn the_screen_composes_and_the_toggle_action_shows_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    let ui = ui_for(&m)?;
    assert!(ui.rect_of("std_friends_add").is_some());
    assert!(ui.rect_of("std_friends_list").is_some());
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.friends.toggle", Some(KeyCode::O))]);
    m.on_actions(|name| name == "std.friends.toggle", &mut props);
    assert_eq!(
        prop(&mut props, "std.friends.open"),
        Some(Value::Bool(false)),
        "toggled closed"
    );
    Ok(())
}

#[test]
fn enter_in_the_add_input_asks_the_character() -> TestResult {
    let (mut m, _) = install("")?;
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    let _ = sent(&mut m);
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(ui.set_focus("std_friends_add"));
    let _ = ui.handle(&UiEvent::Text("42".to_owned()));
    let _ = ui.handle(&UiEvent::Key {
        key: UiKey::Enter,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    let mut intents = Vec::new();
    ui.drain_intents(&mut intents);
    let intent = intents.first().ok_or("no intent")?;
    let name = ui.intent_name(intent.intent).ok_or("intent name")?.to_owned();
    assert_eq!(name, "std.friends.add");
    assert_eq!(
        m.on_intent(&name, intent.payload.as_deref(), ui.properties_mut()),
        IntentRoute::Handled
    );
    let out = sent(&mut m);
    let (kind, bytes) = out.first().ok_or("nothing sent")?;
    assert_eq!(*kind, Request::ID.0);
    assert_eq!(decode_message::<Request>(bytes)?, Request { character: 42 });
    Ok(())
}

#[test]
fn a_disabled_friends_screen_renders_unavailable_not_as_dead_buttons() -> TestResult {
    let (mut m, _) = install("")?;
    // The registry writes straight into the UI's properties.
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    deliver(&mut m, ui.properties_mut(), &pending(&[42], &[])?);
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_friends_accept"), Some(true));
    assert_eq!(ui.is_enabled("std_friends_add"), Some(true));
    assert!(
        ui.rect_of("std_friends_unavailable").is_none_or(|r| r.h <= 0.0),
        "nothing to explain while enabled"
    );
    m.on_refused(
        ExtensionKind(Request::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(
        ui.is_enabled("std_friends_add"),
        Some(false),
        "shown as unavailable"
    );
    assert!(
        ui.rect_of("std_friends_unavailable").is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}
