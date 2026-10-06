//! The std.chat client half against its contract codec: lines drive a bounded
//! history; intents send the contract's `Say` on the selected channel; the `whispers`
//! flag and run-time refusals gate the module; the screen submits on Enter.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{Message, WireString, decode_message, encode_into};
use mantis_ui::{ListItem, Modifiers, Properties, UiEvent, UiKey, Value};
use std_chat_client::HISTORY;
use std_chat_contract::{GUILD, LOCAL, Line, PARTY, Say, WHISPER};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");
/// std.chat depends on the std.party contract, so the graph needs its manifest.
const PARTY_MANIFEST: &str = include_str!("../../../party/manifest.toml");

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = [
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(PARTY_MANIFEST)?,
        },
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(MANIFEST)?,
        },
    ];
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_chat_client::Module) as Arc<dyn ClientModule>],
    )?;
    let mut props = Properties::new();
    modules.start(&mut props);
    Ok((modules, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

fn line(channel: u8, from: u64, text: &str) -> Result<Line, Box<dyn std::error::Error>> {
    whisper_line(channel, from, 0, text)
}

fn whisper_line(channel: u8, from: u64, to: u64, text: &str) -> Result<Line, Box<dyn std::error::Error>> {
    Ok(Line {
        channel,
        from,
        to,
        text: WireString::new(text).ok_or("line text")?,
    })
}

fn say(channel: u8, target: u64, text: &str) -> Result<Say, Box<dyn std::error::Error>> {
    Ok(Say {
        channel,
        target,
        text: WireString::new(text).ok_or("say text")?,
    })
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

fn lines(props: &mut Properties) -> Result<Vec<ListItem>, Box<dyn std::error::Error>> {
    match prop(props, "std.chat.lines") {
        Some(Value::List(items)) => Ok(items),
        other => Err(format!("lines: {other:?}").into()),
    }
}

fn field(item: Option<&ListItem>, name: &str) -> Option<Value> {
    item.and_then(|i| i.field(name)).cloned()
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

fn sent_says(m: &mut ClientModules) -> Result<Vec<Say>, Box<dyn std::error::Error>> {
    sent(m)
        .iter()
        .map(|(kind, bytes)| {
            if *kind == Say::ID.0 {
                Ok(decode_message::<Say>(bytes)?)
            } else {
                Err(format!("unexpected kind {kind}").into())
            }
        })
        .collect()
}

#[test]
fn lines_on_every_channel_fill_the_history_newest_first() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &line(LOCAL, 42, "hello")?);
    deliver(&mut m, &mut props, &line(PARTY, 7, "ready")?);
    deliver(&mut m, &mut props, &whisper_line(WHISPER, 9, 42, "psst")?);
    assert_eq!(prop(&mut props, "std.chat.count"), Some(Value::Int(3)));
    let items = lines(&mut props)?;
    assert_eq!(items.len(), 3);
    let newest = items.first();
    assert_eq!(field(newest, "channel"), Some(text("whisper")));
    assert_eq!(field(newest, "from"), Some(text("9")));
    assert_eq!(field(newest, "text"), Some(text("psst")));
    assert_eq!(
        field(newest, "to"),
        Some(text("42")),
        "a whisper names its recipient"
    );
    assert_eq!(field(newest, "directed"), Some(Value::Bool(true)));
    assert_eq!(field(items.get(1), "channel"), Some(text("party")));
    let oldest = items.get(2);
    assert_eq!(field(oldest, "channel"), Some(text("local")));
    assert_eq!(field(oldest, "from"), Some(text("42")));
    assert_eq!(field(oldest, "text"), Some(text("hello")));
    assert_eq!(field(oldest, "to"), Some(text("")));
    assert_eq!(field(oldest, "directed"), Some(Value::Bool(false)));

    m.on_message(ExtensionKind(Line::ID.0), &[1, 2], &mut props);
    deliver(&mut m, &mut props, &line(9, 42, "unknown channel")?);
    assert_eq!(
        m.stats().malformed,
        2,
        "a malformed line and an unknown channel are counted, never errors"
    );
    assert_eq!(prop(&mut props, "std.chat.count"), Some(Value::Int(3)));
    Ok(())
}

#[test]
fn start_publishes_the_initial_view_model() -> TestResult {
    let (_, mut props) = install("")?;
    assert_eq!(prop(&mut props, "std.chat.enabled"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.chat.unavailable"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.chat.open"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.chat.count"), Some(Value::Int(0)));
    assert!(lines(&mut props)?.is_empty());
    assert_eq!(prop(&mut props, "std.chat.channel"), Some(text("local")));
    assert_eq!(prop(&mut props, "std.chat.whispering"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.chat.target"), Some(text("")));
    Ok(())
}

#[test]
fn the_history_stays_bounded() -> TestResult {
    let (mut m, mut props) = install("")?;
    for i in 0..(HISTORY * 10) {
        deliver(&mut m, &mut props, &line(LOCAL, 42, &format!("line {i}"))?);
    }
    let held = i64::try_from(HISTORY)?;
    assert_eq!(prop(&mut props, "std.chat.count"), Some(Value::Int(held)));
    let items = lines(&mut props)?;
    assert_eq!(items.len(), HISTORY);
    let last = HISTORY * 10 - 1;
    assert_eq!(field(items.first(), "text"), Some(text(&format!("line {last}"))));
    assert_eq!(
        field(items.last(), "text"),
        Some(text(&format!("line {}", HISTORY * 9))),
        "older lines are dropped"
    );
    Ok(())
}

#[test]
fn say_sends_the_contract_message_on_the_selected_channel() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.chat.say", Some("hello"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("std.chat.channel", Some("party"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.chat.channel"), Some(text("party")));
    assert_eq!(
        m.on_intent("std.chat.say", Some("ready"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("std.chat.whisper", Some("42"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.chat.channel"), Some(text("whisper")));
    assert_eq!(prop(&mut props, "std.chat.whispering"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.chat.target"), Some(text("42")));
    assert_eq!(
        m.on_intent("std.chat.say", Some("psst"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        sent_says(&mut m)?,
        [
            say(LOCAL, 0, "hello")?,
            say(PARTY, 0, "ready")?,
            say(WHISPER, 42, "psst")?
        ]
    );

    let too_long = "x".repeat(201);
    for (name, payload) in [
        ("std.chat.say", None),
        ("std.chat.say", Some("")),
        ("std.chat.say", Some("   ")),
        ("std.chat.say", Some(too_long.as_str())),
        ("std.chat.channel", Some("shout")),
        ("std.chat.channel", None),
        ("std.chat.whisper", Some("nobody")),
        ("std.chat.whisper", Some("0")),
        ("std.chat.whisper", None),
    ] {
        assert_eq!(
            m.on_intent(name, payload, &mut props),
            IntentRoute::Refused,
            "{name} {payload:?}"
        );
    }
    assert!(sent(&mut m).is_empty(), "refused intents send nothing");
    assert_eq!(
        m.on_intent("std.chat.channel", Some("local"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.chat.whispering"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn a_whisper_needs_a_target() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.chat.channel", Some("whisper"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.chat.target"), Some(text("")));
    assert_eq!(
        m.on_intent("std.chat.say", Some("psst"), &mut props),
        IntentRoute::Refused,
        "no target selected"
    );
    assert!(sent(&mut m).is_empty());
    Ok(())
}

#[test]
fn whispers_follow_the_flag_and_refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("\"std.chat.whispers\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.chat.flag.whispers"),
        Some(Value::Bool(false))
    );
    for (name, payload) in [("std.chat.channel", "whisper"), ("std.chat.whisper", "42")] {
        assert_eq!(
            m.on_intent(name, Some(payload), &mut props),
            IntentRoute::Refused,
            "whispers are off"
        );
    }
    assert_eq!(
        m.on_intent("std.chat.say", Some("hello"), &mut props),
        IntentRoute::Handled,
        "other channels still work"
    );
    assert_eq!(sent_says(&mut m)?, [say(LOCAL, 0, "hello")?]);

    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &line(LOCAL, 42, "hello")?);
    // A whisper the server will not deliver (or a party line with no party) is
    // refused as not allowed: the screen says so and chat keeps working.
    m.on_refused(ExtensionKind(Say::ID.0), ExtensionRefusal::NotAllowed, &mut props);
    assert_eq!(prop(&mut props, "std.chat.refusal"), Some(text("not allowed")));
    assert_eq!(prop(&mut props, "std.chat.enabled"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.chat.unavailable"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.chat.count"), Some(Value::Int(1)));
    assert_eq!(
        m.on_intent("std.chat.say", Some("hello"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(sent_says(&mut m)?, [say(LOCAL, 0, "hello")?]);

    m.on_refused(
        ExtensionKind(Say::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.chat.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.chat.unavailable"),
        Some(Value::Bool(true)),
        "the screen says so"
    );
    assert_eq!(
        prop(&mut props, "std.chat.count"),
        Some(Value::Int(0)),
        "the history is reset"
    );
    assert_eq!(
        m.on_intent("std.chat.say", Some("hello"), &mut props),
        IntentRoute::Refused
    );
    deliver(&mut m, &mut props, &line(LOCAL, 42, "dropped")?);
    assert_eq!(m.stats().disabled, 2, "the intent and the line are dropped");
    assert!(lines(&mut props)?.is_empty());
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("\"std.chat.enabled\" = false\n")?;
    assert_eq!(prop(&mut props, "std.chat.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        m.on_intent("std.chat.say", Some("hello"), &mut props),
        IntentRoute::Refused,
        "disabled by the package"
    );
    Ok(())
}

fn ui_for(m: &ClientModules) -> Result<mantis_ui::Ui, Box<dyn std::error::Error>> {
    let (layout, theme) = m.compose(&["std.chat.window"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    Ok(mantis_ui::Ui::new(fonts, &layout, Some(&theme))?)
}

#[test]
fn the_screen_composes_and_the_toggle_action_shows_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    let ui = ui_for(&m)?;
    assert!(ui.rect_of("std_chat_input").is_some());
    assert!(ui.rect_of("std_chat_lines").is_some());
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.chat.toggle", Some(KeyCode::Slash))]);
    m.on_actions(|name| name == "std.chat.toggle", &mut props);
    assert_eq!(
        prop(&mut props, "std.chat.open"),
        Some(Value::Bool(false)),
        "toggled closed"
    );
    m.on_actions(|name| name == "std.chat.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.chat.open"), Some(Value::Bool(true)));
    Ok(())
}

#[test]
fn enter_in_the_input_says_the_line() -> TestResult {
    let (mut m, _) = install("")?;
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(ui.set_focus("std_chat_input"));
    let _ = ui.handle(&UiEvent::Text("hello".to_owned()));
    let _ = ui.handle(&UiEvent::Key {
        key: UiKey::Enter,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    let mut intents = Vec::new();
    ui.drain_intents(&mut intents);
    let intent = intents.first().ok_or("no intent")?;
    let name = ui.intent_name(intent.intent).ok_or("intent name")?.to_owned();
    assert_eq!(name, "std.chat.say");
    assert_eq!(
        m.on_intent(&name, intent.payload.as_deref(), ui.properties_mut()),
        IntentRoute::Handled
    );
    assert_eq!(sent_says(&mut m)?, [say(LOCAL, 0, "hello")?]);
    Ok(())
}

#[test]
fn a_disabled_chat_renders_unavailable_not_as_dead_controls() -> TestResult {
    let (mut m, _) = install("\"std.chat.whispers\" = false\n")?;
    // The registry writes straight into the UI's properties.
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    deliver(&mut m, ui.properties_mut(), &line(LOCAL, 42, "hello")?);
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_chat_input"), Some(true));
    assert_eq!(ui.is_enabled("std_chat_local"), Some(true));
    assert_eq!(
        ui.is_enabled("std_chat_whisper"),
        Some(false),
        "whispers off shows as unavailable"
    );
    assert!(
        ui.rect_of("std_chat_unavailable").is_none_or(|r| r.h <= 0.0),
        "nothing to explain while enabled"
    );
    m.on_refused(
        ExtensionKind(Say::ID.0),
        ExtensionRefusal::NotAllowed,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(
        ui.is_enabled("std_chat_input"),
        Some(true),
        "a refused line leaves chat usable"
    );
    m.on_refused(
        ExtensionKind(Say::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    for id in ["std_chat_input", "std_chat_local", "std_chat_party"] {
        assert_eq!(ui.is_enabled(id), Some(false), "{id} shown as unavailable");
    }
    assert!(
        ui.rect_of("std_chat_unavailable").is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}

#[test]
fn guild_lines_show_on_the_guild_channel_and_say_sends_on_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &line(GUILD, 42, "evening, all")?);
    let items = lines(&mut props)?;
    assert_eq!(field(items.first(), "channel"), Some(text("guild")));
    assert_eq!(field(items.first(), "text"), Some(text("evening, all")));
    assert_eq!(
        m.on_intent("std.chat.channel", Some("guild"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.chat.channel"), Some(text("guild")));
    assert_eq!(
        m.on_intent("std.chat.say", Some("on my way"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(sent_says(&mut m)?, [say(GUILD, 0, "on my way")?]);
    // The server refuses a guild line from a character with no guild as not allowed:
    // shown as the refusal, and the rest of chat keeps working.
    m.on_refused(ExtensionKind(Say::ID.0), ExtensionRefusal::NotAllowed, &mut props);
    assert_eq!(prop(&mut props, "std.chat.refusal"), Some(text("not allowed")));
    assert_eq!(prop(&mut props, "std.chat.enabled"), Some(Value::Bool(true)));
    Ok(())
}
