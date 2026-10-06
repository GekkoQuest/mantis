//! The std.containers client half against its contract codec: `Bag` drives the view
//! model; intents send the contract's requests (destroy only after a confirmation);
//! the module's flag and run-time refusals gate it.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{Properties, Value};
use std_containers_contract::{Bag, DestroyItem, MoveItem, SLOTS, ShowBag, SplitStack};

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

fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_containers_client::Module) as Arc<dyn ClientModule>],
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

/// A full bag message: slot 0 holds 5 of item 7, slot 2 holds 30 of item 9, the rest
/// are empty.
fn bag(gold: u64) -> Result<Bag, Box<dyn std::error::Error>> {
    let mut items = [0u32; SLOTS];
    let mut counts = [0u32; SLOTS];
    for (slot, item, count) in [(0, 7, 5), (2, 9, 30)] {
        *items.get_mut(slot).ok_or("slot")? = item;
        *counts.get_mut(slot).ok_or("slot")? = count;
    }
    Ok(Bag {
        gold,
        items: BoundedArray::from_slice(&items).ok_or("items")?,
        counts: BoundedArray::from_slice(&counts).ok_or("counts")?,
    })
}

/// Delivers [`bag`] so slots exist to act on.
fn with_bag(m: &mut ClientModules, props: &mut Properties) -> TestResult {
    deliver(m, props, &bag(120)?);
    Ok(())
}

fn only<M: Message + std::fmt::Debug>(m: &mut ClientModules) -> Result<M, Box<dyn std::error::Error>> {
    let out = sent(m);
    let [(kind, bytes)] = out.as_slice() else {
        return Err(format!("expected one message, got {out:?}").into());
    };
    assert_eq!(*kind, M::ID.0);
    Ok(decode_message::<M>(bytes)?)
}

#[test]
fn start_publishes_every_starting_value() -> TestResult {
    let (_, mut props) = install("")?;
    let expected = [
        ("std.containers.open", Value::Bool(false)),
        ("std.containers.unavailable", Value::Bool(false)),
        ("std.containers.confirming", Value::Bool(false)),
        ("std.containers.has_selection", Value::Bool(false)),
        ("std.containers.selected", Value::Int(-1)),
        ("std.containers.quantity", Value::Int(1)),
        ("std.containers.capacity", Value::Int(0)),
        ("std.containers.items", Value::List(Vec::new())),
        ("std.containers.gold", Value::Text(String::new())),
    ];
    for (name, value) in expected {
        assert_eq!(prop(&mut props, name), Some(value), "{name}");
    }
    let (_, mut props) = install(
        "\"std.containers.enabled\" = false
",
    )?;
    assert_eq!(
        prop(&mut props, "std.containers.unavailable"),
        Some(Value::Bool(true)),
        "a module disabled at start says so"
    );
    assert_eq!(prop(&mut props, "std.containers.open"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn the_bag_message_drives_the_view_model() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &bag(18_446_744_073_709_551_615)?);
    assert_eq!(
        prop(&mut props, "std.containers.gold"),
        Some(Value::Text("18446744073709551615".to_owned())),
        "gold is shown verbatim, even past an int property's range"
    );
    assert_eq!(prop(&mut props, "std.containers.capacity"), Some(Value::Int(20)));
    let Some(Value::List(items)) = prop(&mut props, "std.containers.items") else {
        return Err("items is not a list".into());
    };
    assert_eq!(items.len(), SLOTS);
    let first = items.first().ok_or("slot 0")?;
    assert_eq!(first.field("slot"), Some(&Value::Int(0)));
    assert_eq!(first.field("item"), Some(&Value::Int(7)));
    assert_eq!(first.field("count"), Some(&Value::Int(5)));
    assert_eq!(first.field("filled"), Some(&Value::Bool(true)));
    let empty = items.get(1).ok_or("slot 1")?;
    assert_eq!(empty.field("item"), Some(&Value::Int(0)));
    assert_eq!(empty.field("filled"), Some(&Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.containers.has_selection"),
        Some(Value::Bool(false))
    );

    // A selection survives a refresh while its slot is filled, and not once it empties.
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("2")),
        IntentRoute::Handled
    );
    deliver(&mut m, &mut props, &bag(120)?);
    assert_eq!(prop(&mut props, "std.containers.selected"), Some(Value::Int(2)));
    let emptied = Bag {
        gold: 0,
        items: BoundedArray::from_slice(&[7, 0, 0]).ok_or("items")?,
        counts: BoundedArray::from_slice(&[5, 0, 0]).ok_or("counts")?,
    };
    deliver(&mut m, &mut props, &emptied);
    assert_eq!(prop(&mut props, "std.containers.selected"), Some(Value::Int(-1)));
    assert_eq!(prop(&mut props, "std.containers.capacity"), Some(Value::Int(3)));
    assert_eq!(m.stats().malformed, 0);

    m.on_message(ExtensionKind(Bag::ID.0), &[1, 2], &mut props);
    let uneven = Bag {
        gold: 1,
        items: BoundedArray::from_slice(&[7, 8]).ok_or("items")?,
        counts: BoundedArray::from_slice(&[1]).ok_or("counts")?,
    };
    deliver(&mut m, &mut props, &uneven);
    assert_eq!(
        m.stats().malformed,
        2,
        "a malformed or uneven bag is counted, never an error"
    );
    assert_eq!(
        prop(&mut props, "std.containers.capacity"),
        Some(Value::Int(3)),
        "and changes nothing"
    );
    Ok(())
}

#[test]
fn show_move_and_split_send_the_contract_requests() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.show", None),
        IntentRoute::Handled
    );
    assert_eq!(only::<ShowBag>(&mut m)?, ShowBag {});

    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("0")),
        IntentRoute::Refused,
        "no bag shown yet"
    );
    with_bag(&mut m, &mut props)?;
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.move", Some("4")),
        IntentRoute::Refused,
        "nothing selected"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("1")),
        IntentRoute::Refused,
        "an empty slot"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("sword")),
        IntentRoute::Refused
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("0")),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.containers.selected"), Some(Value::Int(0)));
    assert_eq!(
        prop(&mut props, "std.containers.has_selection"),
        Some(Value::Bool(true))
    );
    for target in ["0", "20", "x"] {
        assert_eq!(
            intent(&mut m, &mut props, "std.containers.move", Some(target)),
            IntentRoute::Refused,
            "same slot, outside the bag, or not a slot: {target}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.move", Some("4")),
        IntentRoute::Handled
    );
    assert_eq!(only::<MoveItem>(&mut m)?, MoveItem { from: 0, to: 4 });
    assert_eq!(
        prop(&mut props, "std.containers.has_selection"),
        Some(Value::Bool(false)),
        "a sent move drops the selection"
    );

    // Quantity: a positive number, or refused and unchanged.
    for bad in ["0", "-3", "potion", ""] {
        assert_eq!(
            intent(&mut m, &mut props, "std.containers.quantity", Some(bad)),
            IntentRoute::Refused,
            "quantity {bad:?}"
        );
    }
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.quantity", None),
        IntentRoute::Refused
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.quantity", Some(" 12 ")),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.containers.quantity"), Some(Value::Int(12)));
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.split", Some("5")),
        IntentRoute::Refused,
        "nothing selected"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.split", Some("5")),
        IntentRoute::Handled
    );
    assert_eq!(
        only::<SplitStack>(&mut m)?,
        SplitStack {
            from: 2,
            to: 5,
            count: 12
        }
    );
    Ok(())
}

#[test]
fn destroy_needs_a_confirmation() -> TestResult {
    let (mut m, mut props) = install("")?;
    with_bag(&mut m, &mut props)?;
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.destroy", None),
        IntentRoute::Refused,
        "nothing selected"
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.quantity", Some("3")),
        IntentRoute::Handled
    );

    // The first destroy arms; nothing is sent.
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.destroy", None),
        IntentRoute::Handled
    );
    assert!(sent(&mut m).is_empty(), "arming sends nothing");
    assert_eq!(
        prop(&mut props, "std.containers.confirming"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        prop(&mut props, "std.containers.confirm_slot"),
        Some(Value::Int(2))
    );
    assert_eq!(
        prop(&mut props, "std.containers.confirm_count"),
        Some(Value::Int(3))
    );

    // Cancelling disarms.
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.cancel", None),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.containers.confirming"),
        Some(Value::Bool(false))
    );
    assert!(sent(&mut m).is_empty());

    // Changing the selection or the quantity disarms too.
    for change in [("std.containers.select", "0"), ("std.containers.quantity", "4")] {
        assert_eq!(
            intent(&mut m, &mut props, "std.containers.select", Some("2")),
            IntentRoute::Handled
        );
        assert_eq!(
            intent(&mut m, &mut props, "std.containers.destroy", None),
            IntentRoute::Handled
        );
        assert_eq!(
            intent(&mut m, &mut props, change.0, Some(change.1)),
            IntentRoute::Handled
        );
        assert_eq!(
            prop(&mut props, "std.containers.confirming"),
            Some(Value::Bool(false)),
            "{change:?} disarms"
        );
    }
    assert!(sent(&mut m).is_empty());

    // Arm, then confirm: exactly one destroy of what the confirmation showed.
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("2")),
        IntentRoute::Handled
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.destroy", None),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.containers.confirm_count"),
        Some(Value::Int(4))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.destroy", None),
        IntentRoute::Handled
    );
    assert_eq!(only::<DestroyItem>(&mut m)?, DestroyItem { slot: 2, count: 4 });
    assert_eq!(
        prop(&mut props, "std.containers.confirming"),
        Some(Value::Bool(false))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.destroy", None),
        IntentRoute::Refused,
        "the selection went with the destroy"
    );
    Ok(())
}

#[test]
fn grants_have_no_intent() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.grant", Some("1")),
        IntentRoute::NotModule,
        "grants come only from services"
    );
    assert!(sent(&mut m).is_empty());
    Ok(())
}

#[test]
fn the_flag_and_refusals_gate_the_module() -> TestResult {
    let (mut m, mut props) = install("\"std.containers.enabled\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.containers.enabled"),
        Some(Value::Bool(false))
    );
    assert!(!m.is_enabled("std.containers"));
    with_bag(&mut m, &mut props)?;
    assert_eq!(
        m.stats().disabled,
        1,
        "messages for a disabled module are dropped"
    );
    // Start-up published the empty (reset) view model; the dropped bag changed nothing.
    assert_eq!(
        prop(&mut props, "std.containers.items"),
        Some(Value::List(Vec::new()))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.show", None),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("")?;
    with_bag(&mut m, &mut props)?;
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("0")),
        IntentRoute::Handled
    );
    m.on_refused(
        ExtensionKind(MoveItem::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.containers.refusal"),
        Some(Value::Text("not allowed".to_owned()))
    );
    assert!(m.is_enabled("std.containers"), "other refusals keep it enabled");
    m.on_refused(
        ExtensionKind(DestroyItem::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.containers.enabled"),
        Some(Value::Bool(false))
    );
    assert_eq!(
        prop(&mut props, "std.containers.unavailable"),
        Some(Value::Bool(true)),
        "the screen says so"
    );
    assert_eq!(
        prop(&mut props, "std.containers.items"),
        Some(Value::List(Vec::new())),
        "the view model is reset"
    );
    assert_eq!(
        prop(&mut props, "std.containers.has_selection"),
        Some(Value::Bool(false))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.show", None),
        IntentRoute::Refused
    );

    m.set_enabled("std.containers", true, &mut props);
    assert_eq!(
        prop(&mut props, "std.containers.unavailable"),
        Some(Value::Bool(false))
    );
    assert_eq!(
        intent(&mut m, &mut props, "std.containers.select", Some("0")),
        IntentRoute::Refused,
        "re-enabled with an empty view until the next bag"
    );
    Ok(())
}

#[test]
fn the_bag_screen_composes_and_the_toggle_action_asks_for_the_bag() -> TestResult {
    let (mut m, mut props) = install("")?;
    let (layout, theme) = m.compose(&["std.containers.bag"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    assert!(ui.rect_of("std_containers_destroy").is_some());
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.containers.toggle", Some(KeyCode::I))]);

    assert_eq!(
        prop(&mut props, "std.containers.open"),
        Some(Value::Bool(false)),
        "closed at start"
    );
    m.on_actions(|name| name == "std.containers.toggle", &mut props);
    assert_eq!(prop(&mut props, "std.containers.open"), Some(Value::Bool(true)));
    assert_eq!(only::<ShowBag>(&mut m)?, ShowBag {}, "opening asks for the bag");
    m.on_actions(|name| name == "std.containers.toggle", &mut props);
    assert_eq!(
        prop(&mut props, "std.containers.open"),
        Some(Value::Bool(false)),
        "toggled closed"
    );
    assert!(sent(&mut m).is_empty(), "closing asks nothing");
    Ok(())
}

#[test]
fn a_disabled_bag_renders_unavailable_not_as_dead_buttons() -> TestResult {
    let (mut m, _) = install("")?;
    let (layout, theme) = m.compose(&["std.containers.bag"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    // The registry writes straight into the UI's properties.
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(
        ui.rect_of("std_containers_confirm").is_some_and(|r| r.h <= 0.0),
        "no confirmation before the first bag"
    );
    m.on_actions(|name| name == "std.containers.toggle", ui.properties_mut());
    deliver(&mut m, ui.properties_mut(), &bag(120)?);
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert!(
        ui.rect_of("std_containers_unavailable")
            .is_some_and(|r| r.h <= 0.0),
        "enabled: no notice"
    );
    assert_eq!(ui.is_enabled("std_containers_refresh"), Some(true));
    assert_eq!(
        ui.is_enabled("std_containers_destroy"),
        Some(false),
        "nothing selected yet"
    );
    assert_eq!(
        m.on_intent("std.containers.select", Some("0"), ui.properties_mut()),
        IntentRoute::Handled
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_containers_destroy"), Some(true));

    m.on_refused(
        ExtensionKind(ShowBag::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    for id in [
        "std_containers_refresh",
        "std_containers_destroy",
        "std_containers_quantity",
    ] {
        assert_eq!(ui.is_enabled(id), Some(false), "{id} shown as unavailable");
    }
    assert!(
        ui.rect_of("std_containers_unavailable")
            .is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}
