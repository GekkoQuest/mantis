//! The std.titles client half against its contract codec: `Titles` drives the view
//! model; intents send the contract's requests; starting asks for the titles; the
//! module's flag and run-time refusals gate it.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{Properties, Value};
use std_titles_contract::{SetActiveTitle, ShowTitles, Titles};

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

fn modules(flags: &str) -> Result<ClientModules, Box<dyn std::error::Error>> {
    Ok(ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_titles_client::Module) as Arc<dyn ClientModule>],
    )?)
}

/// Installs and starts the module, and drains the `ShowTitles` start sends.
fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = modules(flags)?;
    let mut props = Properties::new();
    modules.start(&mut props);
    let _ = sent(&mut modules);
    Ok((modules, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

fn intent(m: &mut ClientModules, props: &mut Properties, name: &str, payload: Option<&str>) -> IntentRoute {
    m.on_intent(name, payload, props)
}

fn only<M: Message + std::fmt::Debug>(m: &mut ClientModules) -> Result<M, Box<dyn std::error::Error>> {
    let out = sent(m);
    let [(kind, bytes)] = out.as_slice() else {
        return Err(format!("expected one message, got {out:?}").into());
    };
    assert_eq!(*kind, M::ID.0);
    Ok(decode_message::<M>(bytes)?)
}

/// Titles 3, 5, and 9 held; 5 shown.
fn titles() -> Result<Titles, Box<dyn std::error::Error>> {
    Ok(Titles {
        owned: BoundedArray::from_slice(&[3, 5, 9]).ok_or("owned")?,
        active: 5,
    })
}

#[test]
fn start_publishes_every_starting_value_and_asks_for_the_titles() -> TestResult {
    let mut m = modules("")?;
    let mut props = Properties::new();
    m.start(&mut props);
    assert_eq!(
        only::<ShowTitles>(&mut m)?,
        ShowTitles {},
        "start asks for the titles"
    );
    let expected = [
        ("std.titles.open", Value::Bool(false)),
        ("std.titles.unavailable", Value::Bool(false)),
        ("std.titles.titles", Value::List(Vec::new())),
        ("std.titles.count", Value::Int(0)),
        ("std.titles.active", Value::Int(0)),
        ("std.titles.has_active", Value::Bool(false)),
    ];
    for (name, value) in expected {
        assert_eq!(prop(&mut props, name), Some(value), "{name}");
    }

    let mut m = modules("\"std.titles.enabled\" = false\n")?;
    let mut props = Properties::new();
    m.start(&mut props);
    assert!(sent(&mut m).is_empty(), "a disabled module asks nothing");
    assert_eq!(
        prop(&mut props, "std.titles.unavailable"),
        Some(Value::Bool(true))
    );
    assert_eq!(prop(&mut props, "std.titles.open"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn the_titles_message_drives_the_view_model() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &titles()?);
    assert_eq!(prop(&mut props, "std.titles.count"), Some(Value::Int(3)));
    assert_eq!(prop(&mut props, "std.titles.active"), Some(Value::Int(5)));
    assert_eq!(prop(&mut props, "std.titles.has_active"), Some(Value::Bool(true)));
    let Some(Value::List(items)) = prop(&mut props, "std.titles.titles") else {
        return Err("titles is not a list".into());
    };
    let fields: Vec<_> = items
        .iter()
        .map(|i| (i.field("title").cloned(), i.field("active").cloned()))
        .collect();
    assert_eq!(
        fields,
        [
            (Some(Value::Int(3)), Some(Value::Bool(false))),
            (Some(Value::Int(5)), Some(Value::Bool(true))),
            (Some(Value::Int(9)), Some(Value::Bool(false))),
        ]
    );

    let none = Titles {
        owned: BoundedArray::from_slice(&[3]).ok_or("owned")?,
        active: 0,
    };
    deliver(&mut m, &mut props, &none);
    assert_eq!(
        prop(&mut props, "std.titles.has_active"),
        Some(Value::Bool(false))
    );
    m.on_message(ExtensionKind(Titles::ID.0), &[200], &mut props);
    assert_eq!(
        m.stats().malformed,
        1,
        "a malformed list is counted, never an error"
    );
    assert_eq!(prop(&mut props, "std.titles.count"), Some(Value::Int(1)));
    Ok(())
}

#[test]
fn set_and_show_send_the_contract_requests() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.set", Some("3")),
        IntentRoute::Refused,
        "no titles known yet"
    );
    deliver(&mut m, &mut props, &titles()?);
    for bad in ["4", "x", ""] {
        assert_eq!(
            intent(&mut m, &mut props, "std.titles.set", Some(bad)),
            IntentRoute::Refused,
            "not a held title: {bad:?}"
        );
    }
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.set", None),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.set", Some("9")),
        IntentRoute::Handled
    );
    assert_eq!(only::<SetActiveTitle>(&mut m)?, SetActiveTitle { title: 9 });
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.set", Some("0")),
        IntentRoute::Handled,
        "showing none"
    );
    assert_eq!(only::<SetActiveTitle>(&mut m)?, SetActiveTitle { title: 0 });
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.show", None),
        IntentRoute::Handled
    );
    assert_eq!(only::<ShowTitles>(&mut m)?, ShowTitles {});
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.grant", Some("9")),
        IntentRoute::NotModule,
        "titles are granted only by services"
    );
    Ok(())
}

#[test]
fn the_flag_and_refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("\"std.titles.enabled\" = false\n")?;
    deliver(&mut m, &mut props, &titles()?);
    assert_eq!(
        m.stats().disabled,
        1,
        "messages for a disabled module are dropped"
    );
    assert_eq!(prop(&mut props, "std.titles.count"), Some(Value::Int(0)));
    assert_eq!(
        intent(&mut m, &mut props, "std.titles.show", None),
        IntentRoute::Refused
    );

    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &titles()?);
    m.on_refused(
        ExtensionKind(SetActiveTitle::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert!(m.is_enabled("std.titles"), "not allowed keeps the module");
    assert_eq!(
        prop(&mut props, "std.titles.refusal"),
        Some(Value::Text("not allowed".to_owned()))
    );
    assert_eq!(prop(&mut props, "std.titles.count"), Some(Value::Int(3)));
    m.on_refused(
        ExtensionKind(ShowTitles::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.titles.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.titles.unavailable"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        prop(&mut props, "std.titles.titles"),
        Some(Value::List(Vec::new())),
        "the view model is reset"
    );
    assert!(sent(&mut m).is_empty(), "disabling asks nothing");

    m.set_enabled("std.titles", true, &mut props);
    assert_eq!(
        only::<ShowTitles>(&mut m)?,
        ShowTitles {},
        "enabled again, it asks for the titles"
    );
    Ok(())
}

#[test]
fn the_titles_screen_composes_and_the_toggle_action_shows_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    let (layout, theme) = m.compose(&["std.titles.list"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    assert!(ui.rect_of("std_titles_none").is_some());
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.titles.toggle", Some(KeyCode::T))]);
    m.on_actions(|name| name == "std.titles.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.titles.open"), Some(Value::Bool(true)));
    m.on_actions(|name| name == "std.titles.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.titles.open"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn a_disabled_titles_screen_renders_unavailable_not_as_dead_buttons() -> TestResult {
    let mut m = modules("")?;
    let (layout, theme) = m.compose(&["std.titles.list"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    // The registry writes straight into the UI's properties.
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(
        ui.rect_of("std_titles_panel").is_some_and(|r| r.h <= 0.0),
        "closed at start"
    );
    m.on_actions(|name| name == "std.titles.toggle", ui.properties_mut());
    deliver(&mut m, ui.properties_mut(), &titles()?);
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_titles_refresh"), Some(true));
    assert_eq!(ui.is_enabled("std_titles_none"), Some(true), "a title is shown");
    m.on_refused(
        ExtensionKind(SetActiveTitle::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_titles_refresh"), Some(false));
    assert_eq!(ui.is_enabled("std_titles_none"), Some(false));
    assert!(
        ui.rect_of("std_titles_unavailable").is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}
