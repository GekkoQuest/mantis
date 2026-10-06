//! The std.party client half against its contract codec: roster, invitation, and
//! disband messages drive the view model; intents send the contract's requests; the
//! `invites` flag and run-time refusals gate the invitation.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, decode_message, encode_into};
use mantis_ui::{Properties, Value};
use std_party_contract::{Accept, Disbanded, Invited, Kick, Leave, Roster};

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
        &[Arc::new(std_party_client::Module) as Arc<dyn ClientModule>],
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

#[test]
fn roster_invitation_and_disband_drive_the_view_model() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &Invited { from: 42 });
    assert_eq!(prop(&mut props, "std.party.invited"), Some(Value::Bool(true)));
    assert_eq!(
        prop(&mut props, "std.party.invite_from"),
        Some(Value::Text("42".to_owned()))
    );
    assert_eq!(
        m.on_intent("std.party.accept", None, &mut props),
        IntentRoute::Handled
    );
    let out = sent(&mut m);
    let (kind, bytes) = out.first().cloned().ok_or("nothing sent")?;
    assert_eq!(kind, Accept::ID.0);
    assert_eq!(decode_message::<Accept>(&bytes)?, Accept { from: 42 });
    assert_eq!(
        prop(&mut props, "std.party.invited"),
        Some(Value::Bool(false)),
        "accepting closes the prompt"
    );
    assert_eq!(
        m.on_intent("std.party.accept", None, &mut props),
        IntentRoute::Refused,
        "no open invitation"
    );

    let members = BoundedArray::from_slice(&[42, 7, 9]).ok_or("members")?;
    deliver(
        &mut m,
        &mut props,
        &Roster {
            party: 3,
            leader: 42,
            members,
        },
    );
    assert_eq!(prop(&mut props, "std.party.in_party"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.party.size"), Some(Value::Int(3)));
    match prop(&mut props, "std.party.members") {
        Some(Value::List(items)) => assert_eq!(items.len(), 3),
        other => return Err(format!("members: {other:?}").into()),
    }
    assert_eq!(
        m.on_intent("std.party.kick", Some("9"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("std.party.kick", Some("nobody"), &mut props),
        IntentRoute::Refused
    );
    assert_eq!(
        m.on_intent("std.party.leave", None, &mut props),
        IntentRoute::Handled
    );
    let out = sent(&mut m);
    let kinds: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, [Kick::ID.0, Leave::ID.0]);
    assert_eq!(
        decode_message::<Kick>(&out.first().ok_or("kick")?.1)?,
        Kick { character: 9 }
    );

    deliver(&mut m, &mut props, &Disbanded { party: 3 });
    assert_eq!(prop(&mut props, "std.party.in_party"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.party.size"), Some(Value::Int(0)));
    m.on_message(ExtensionKind(Roster::ID.0), &[1, 2], &mut props);
    assert_eq!(
        m.stats().malformed,
        1,
        "a malformed roster is counted, never an error"
    );
    Ok(())
}

#[test]
fn invitations_follow_the_flag_and_refusals() -> TestResult {
    let (mut m, mut props) = install("\"std.party.invites\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.party.flag.invites"),
        Some(Value::Bool(false))
    );
    deliver(&mut m, &mut props, &Invited { from: 5 });
    assert_eq!(
        m.on_intent("std.party.accept", None, &mut props),
        IntentRoute::Refused,
        "invites are off"
    );
    assert_eq!(
        m.on_intent("std.party.invite", Some("77"), &mut props),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());

    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.party.invite", Some("77"), &mut props),
        IntentRoute::Handled
    );
    m.on_refused(ExtensionKind(1000), ExtensionRefusal::FeatureDisabled, &mut props);
    assert_eq!(prop(&mut props, "std.party.enabled"), Some(Value::Bool(false)));
    assert_eq!(
        prop(&mut props, "std.party.unavailable"),
        Some(Value::Bool(true)),
        "the screen says so"
    );
    assert_eq!(
        m.on_intent("std.party.leave", None, &mut props),
        IntentRoute::Refused
    );
    Ok(())
}

#[test]
fn the_roster_screen_composes_and_the_toggle_action_shows_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    let (layout, theme) = m.compose(&["std.party.roster"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    assert!(ui.rect_of("std_party_leave").is_some());
    assert_eq!(
        m.actions().map(|a| a.name).collect::<Vec<_>>(),
        ["std.party.toggle"]
    );
    m.on_actions(|name| name == "std.party.toggle", &mut props);
    assert_eq!(
        prop(&mut props, "std.party.open"),
        Some(Value::Bool(false)),
        "toggled closed"
    );
    Ok(())
}

#[test]
fn a_disabled_party_renders_unavailable_not_as_dead_buttons() -> TestResult {
    let (mut m, _) = install("")?;
    let (layout, theme) = m.compose(&["std.party.roster"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    // The registry writes straight into the UI's properties.
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    m.on_actions(|name| name == "std.party.toggle", ui.properties_mut());
    m.on_actions(|name| name == "std.party.toggle", ui.properties_mut());
    deliver(&mut m, ui.properties_mut(), &Invited { from: 42 });
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(ui.is_enabled("std_party_accept"), Some(true));
    m.on_refused(
        ExtensionKind(1001),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 600.0], 1.0);
    assert_eq!(
        ui.is_enabled("std_party_leave"),
        Some(false),
        "shown as unavailable"
    );
    assert!(
        ui.rect_of("std_party_unavailable").is_some_and(|r| r.h > 0.0),
        "the panel says why"
    );
    Ok(())
}
