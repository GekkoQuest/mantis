//! The client module registry: installation from the resolved graph, kind ownership,
//! message and intent routing, feature gating from flags and refusals, screens with
//! prefixed ids, actions, outbound messages, and the network link.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use mantis_adapter_contract::{ExtensionKind, ExtensionRefusal};
use mantis_client::modules::{
    ClientModule, ClientModules, ClientRegistrar, ClientRegistryError, EntityChange, IntentRoute,
    ModuleContext, ModuleError, decode,
};
use mantis_core::ecs::EntityId;
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{Message, MessageId, Wire};
use mantis_ui::{Properties, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A one-field message with a chosen id, standing in for a contract message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Count<const ID: u16>(u32);

impl<const ID: u16> Wire for Count<ID> {
    fn encode(&self, e: &mut mantis_core::wire::Encoder<'_>) {
        self.0.encode(e);
    }
    fn decode(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self(u32::decode(d)?))
    }
}

impl<const ID: u16> Message for Count<ID> {
    const ID: MessageId = MessageId(ID);
    const NAME: &'static str = "Count";
}

#[derive(Default)]
struct Tally {
    total: u32,
    entered: u32,
}

fn on_count(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: Count<5001> = decode(payload)?;
    let total = {
        let t = ctx.state::<Tally>().ok_or(ModuleError::Invalid)?;
        t.total += m.0;
        t.total
    };
    ctx.set_int("total", i64::from(total));
    Ok(())
}

fn on_ping(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let n: u32 = payload.and_then(|p| p.parse().ok()).ok_or(ModuleError::Invalid)?;
    ctx.send(&Count::<5000>(n))
}

fn on_entities(ctx: &mut ModuleContext<'_>, change: &EntityChange) {
    if matches!(change, EntityChange::Entered { .. })
        && let Some(t) = ctx.state::<Tally>()
    {
        t.entered += 1;
    }
}

fn on_open(ctx: &mut ModuleContext<'_>) {
    ctx.set_bool("open", true);
}

fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    if !enabled && let Some(t) = ctx.state::<Tally>() {
        *t = Tally::default();
    }
}

/// Kind 5002 is a command with its own result message: its refusals are answered by the
/// module (recorded as the last refused request) instead of the generic refusal text.
fn on_refused(ctx: &mut ModuleContext<'_>, kind: u16, request: u32, _reason: ExtensionRefusal) -> bool {
    if kind != 5002 {
        return false;
    }
    ctx.set_int("refused_request", i64::from(request));
    true
}

const SCREEN: &str =
    r#"panel id=test_counter_panel { text id=test_counter_total bind="test.counter.total" }"#;

struct Counter;

impl ClientModule for Counter {
    fn key(&self) -> &'static str {
        "test.counter"
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(5000..=5009)?;
        r.state(Tally::default());
        r.on_message(5001, on_count)?;
        r.on_intent("test.counter.ping", on_ping)?;
        r.on_entities(on_entities);
        r.on_enabled(on_enabled);
        r.on_refused(on_refused);
        r.screen(
            "test.counter.main",
            SCREEN,
            Some("theme { style counter { size = 14 } }"),
        )?;
        r.action(
            "test.counter.open",
            Some(mantis_client::input::device::KeyCode::C),
            on_open,
        )?;
        Ok(())
    }
}

/// Claims a kind the counter owns.
struct Greedy;

impl ClientModule for Greedy {
    fn key(&self) -> &'static str {
        "test.greedy"
    }
    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(5005..=5005)
    }
}

/// A screen whose ids collide with another module's.
struct Squatter;

impl ClientModule for Squatter {
    fn key(&self) -> &'static str {
        "test.greedy"
    }
    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.screen("test.greedy.main", SCREEN, None)
    }
}

fn manifest(key: &str, flags: &[(&str, bool)]) -> String {
    let mut s = format!("[module]\nkey = \"{key}\"\nversion = \"0.1.0\"\n[flags]\n");
    for (f, v) in flags {
        let _ = writeln!(s, "{f} = {v}");
    }
    s
}

fn graph(keys: &[&str], package_flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found: Vec<Discovered> = keys
        .iter()
        .map(|k| {
            Ok(Discovered {
                origin: "test".to_owned(),
                manifest: parse_manifest(&manifest(k, &[("enabled", true), ("loud", true)]))?,
            })
        })
        .collect::<Result<_, Box<dyn std::error::Error>>>()?;
    let package = parse_package(&format!("[package]\nname = \"test\"\n[flags]\n{package_flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

#[test]
fn messages_intents_entities_and_actions_route_to_their_module() -> TestResult {
    let g = graph(&["test.counter", "test.unlinked"], "")?;
    let mut m = ClientModules::new(&g, &[Arc::new(Counter) as Arc<dyn ClientModule>])?;
    assert_eq!(
        m.keys().collect::<Vec<_>>(),
        ["test.counter"],
        "server-only modules are skipped"
    );
    let mut props = Properties::new();
    m.publish_flags(&mut props);
    assert_eq!(prop(&mut props, "test.counter.enabled"), Some(Value::Bool(true)));
    assert_eq!(
        prop(&mut props, "test.counter.flag.loud"),
        Some(Value::Bool(true))
    );

    let mut payload = Vec::new();
    mantis_core::wire::encode_into(&Count::<5001>(3), &mut payload);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    assert_eq!(prop(&mut props, "test.counter.total"), Some(Value::Int(6)));
    m.on_message(ExtensionKind(5001), &[1], &mut props);
    m.on_message(ExtensionKind(7777), &payload, &mut props);
    let s = m.stats();
    assert_eq!(
        (s.messages, s.malformed, s.unknown_kind),
        (2, 1, 1),
        "nothing is an error"
    );

    assert_eq!(
        m.on_intent("test.counter.ping", Some("9"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        m.on_intent("test.counter.ping", Some("x"), &mut props),
        IntentRoute::Refused
    );
    assert_eq!(m.on_intent("host.menu", None, &mut props), IntentRoute::NotModule);
    let mut sent = Vec::new();
    m.drain_outbound(|kind, bytes| sent.push((kind, bytes.to_vec())));
    let mut expected = Vec::new();
    mantis_core::wire::encode_into(&Count::<5000>(9), &mut expected);
    assert_eq!(sent, [(ExtensionKind(5000), expected)]);

    m.on_entities(
        &EntityChange::Entered {
            entity: EntityId::new(4, 0),
            appearance: 1,
        },
        &mut props,
    );
    m.on_actions(|name| name == "test.counter.open", &mut props);
    assert_eq!(prop(&mut props, "test.counter.open"), Some(Value::Bool(true)));
    assert_eq!(
        m.actions().map(|a| a.name).collect::<Vec<_>>(),
        ["test.counter.open"]
    );

    let (layout, theme) = m.compose(&["test.counter.main", "nope"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    assert!(
        ui.rect_of("test_counter_total").is_some(),
        "the screen composes into one layout"
    );
    Ok(())
}

#[test]
fn feature_flags_and_refusals_gate_a_module() -> TestResult {
    let g = graph(&["test.counter"], "\"test.counter.enabled\" = false\n")?;
    let mut m = ClientModules::new(&g, &[Arc::new(Counter) as Arc<dyn ClientModule>])?;
    let mut props = Properties::new();
    m.publish_flags(&mut props);
    assert_eq!(
        prop(&mut props, "test.counter.enabled"),
        Some(Value::Bool(false)),
        "disabled by the package"
    );
    assert_eq!(
        prop(&mut props, "test.counter.flag.loud"),
        Some(Value::Bool(false)),
        "flags of a disabled module are off"
    );
    assert_eq!(
        m.on_intent("test.counter.ping", Some("1"), &mut props),
        IntentRoute::Refused
    );
    let mut payload = Vec::new();
    mantis_core::wire::encode_into(&Count::<5001>(3), &mut payload);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    assert_eq!(m.stats().disabled, 2);

    // Enabled, then the server refuses one of its kinds as disabled.
    let g = graph(&["test.counter"], "")?;
    let mut m = ClientModules::new(&g, &[Arc::new(Counter) as Arc<dyn ClientModule>])?;
    m.publish_flags(&mut props);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    m.on_refused(ExtensionKind(5000), ExtensionRefusal::NotAllowed, &mut props);
    assert_eq!(
        prop(&mut props, "test.counter.refusal"),
        Some(Value::Text("not allowed".to_owned()))
    );
    assert!(m.is_enabled("test.counter"));
    m.on_refused(ExtensionKind(5000), ExtensionRefusal::FeatureDisabled, &mut props);
    assert!(!m.is_enabled("test.counter"));
    assert_eq!(prop(&mut props, "test.counter.enabled"), Some(Value::Bool(false)));
    assert_eq!(m.presentation().count(), 0);
    // The module reset its view model when disabled; re-enabling starts clean.
    m.set_enabled("test.counter", true, &mut props);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    assert_eq!(prop(&mut props, "test.counter.total"), Some(Value::Int(3)));
    Ok(())
}

#[test]
fn registration_refuses_overlaps_and_foreign_names() -> TestResult {
    let g = graph(&["test.counter", "test.greedy"], "")?;
    let both: [Arc<dyn ClientModule>; 2] = [Arc::new(Counter), Arc::new(Greedy)];
    assert_eq!(
        ClientModules::new(&g, &both).err(),
        Some(ClientRegistryError::DuplicateKind(5005))
    );
    let both: [Arc<dyn ClientModule>; 2] = [Arc::new(Counter), Arc::new(Squatter)];
    assert!(
        matches!(ClientModules::new(&g, &both).err(), Some(ClientRegistryError::Screen(why)) if why.contains("must start with `test_greedy_`"))
    );
    let twice: [Arc<dyn ClientModule>; 2] = [Arc::new(Greedy), Arc::new(Squatter)];
    assert_eq!(
        ClientModules::new(&g, &twice).err(),
        Some(ClientRegistryError::DuplicateModule("test.greedy".to_owned()))
    );
    Ok(())
}

/// A transport that loops nothing back; module traffic is injected by hand.
#[derive(Default)]
struct Silent {
    sent: Vec<Vec<u8>>,
}

impl mantis_adapter_contract::Transport for Silent {
    fn poll(&mut self, _sink: &mut dyn FnMut(mantis_adapter_contract::TransportEvent<'_>)) {}
    fn send(
        &mut self,
        _conn: mantis_adapter_contract::ConnectionId,
        _channel: mantis_adapter_contract::Channel,
        bytes: &[u8],
    ) -> Result<(), mantis_adapter_contract::TransportError> {
        self.sent.push(bytes.to_vec());
        Ok(())
    }
    fn disconnect(&mut self, _conn: mantis_adapter_contract::ConnectionId) {}
    fn kind(&self) -> mantis_adapter_contract::TransportKind {
        mantis_adapter_contract::TransportKind::Quic
    }
    fn max_unreliable_payload(&self) -> usize {
        1100
    }
}

#[test]
fn the_link_carries_outbound_module_messages_to_the_session() -> TestResult {
    let g = graph(&["test.counter"], "")?;
    let mut m = ClientModules::new(&g, &[Arc::new(Counter) as Arc<dyn ClientModule>])?;
    let mut props = Properties::new();
    let clock: Arc<dyn mantis_client::time::HostClock> = Arc::new(mantis_client::time::ManualClock::new());
    let (snapshots, _inbox) = mantis_client::snapshot::snapshot_channel(2, 4);
    let (_outbox, moves) = mantis_client::net::move_channel(4);
    let mut session = mantis_client::net::NativeSession::new(
        Silent::default(),
        clock,
        mantis_client::net::NetConfig::new(mantis_core::content::ContentHash::ZERO),
        snapshots,
        moves,
    );
    let (mut net_end, mut ui_end) = mantis_client::modules::module_link(16);
    assert_eq!(
        m.on_intent("test.counter.ping", Some("4"), &mut props),
        IntentRoute::Handled
    );
    ui_end.pump(&mut m, &mut props);
    net_end.pump(&mut session);
    assert_eq!(session.stats().extensions_sent, 1);
    let frame = session
        .transport_mut()
        .sent
        .first()
        .cloned()
        .ok_or("nothing sent")?;
    let id = u16::from_le_bytes([*frame.get(1).ok_or("id")?, *frame.get(2).ok_or("id")?]);
    match mantis_adapter_contract::parse_inbound(MessageId(id), frame.get(3..).ok_or("payload")?)? {
        mantis_adapter_contract::Inbound::Extension(e) => {
            assert_eq!(e.kind, ExtensionKind(5000));
            assert_eq!(e.request, 1, "every send carries a tracked request");
            assert_eq!(session.last_request(), 1);
            let bytes: Vec<u8> = e.payload.iter().copied().collect();
            assert_eq!(decode::<Count<5000>>(&bytes)?, Count(4));
        }
        other => return Err(format!("unexpected {other:?}").into()),
    }
    // The next send gets the next request.
    assert_eq!(
        m.on_intent("test.counter.ping", Some("5"), &mut props),
        IntentRoute::Handled
    );
    ui_end.pump(&mut m, &mut props);
    net_end.pump(&mut session);
    assert_eq!(session.last_request(), 2);
    Ok(())
}

/// A package's own party implementation overriding the std one: its own key and id
/// prefix, the std contract's names.
struct OverrideParty;

const OVERRIDE_SCREEN: &str =
    r#"panel id=game_party_panel { text id=game_party_size bind="test.counter.total" }"#;

impl ClientModule for OverrideParty {
    fn key(&self) -> &'static str {
        "game.party"
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        assert_eq!(r.namespace(), "test.counter", "names come from the contract");
        r.kinds(5000..=5009)?;
        r.state(Tally::default());
        r.on_message(5001, on_count)?;
        r.on_intent("test.counter.ping", on_ping)?;
        // The contract's screen key, with this module's own element ids.
        r.screen("test.counter.main", OVERRIDE_SCREEN, None)?;
        Ok(())
    }
}

#[test]
fn an_override_answers_to_the_contracts_names_with_its_own_ids() -> TestResult {
    let original = Discovered {
        origin: "test".to_owned(),
        manifest: parse_manifest(
            "[module]\nkey = \"test.counter\"\nversion = \"0.1.0\"\n[flags]\nenabled = true\n",
        )?,
    };
    let replacement = Discovered {
        origin: "game".to_owned(),
        manifest: parse_manifest(
            "[module]\nkey = \"game.party\"\nversion = \"0.1.0\"\ncontract = \"test.counter\"\n[flags]\nenabled = true\n",
        )?,
    };
    let package =
        parse_package("[package]\nname = \"game\"\nuses = [\"test\"]\noverrides = [\"test.counter\"]\n")?;
    let g = resolve(&package, &[original, replacement], &BTreeMap::new())?;
    // Both halves are linked; only the override is in the graph.
    let linked: [Arc<dyn ClientModule>; 2] = [Arc::new(Counter), Arc::new(OverrideParty)];
    let mut m = ClientModules::new(&g, &linked)?;
    assert_eq!(m.keys().collect::<Vec<_>>(), ["game.party"]);
    let mut props = Properties::new();
    m.publish_flags(&mut props);
    assert_eq!(
        prop(&mut props, "test.counter.enabled"),
        Some(Value::Bool(true)),
        "flags under the contract"
    );
    let screens: Vec<_> = m.screens().iter().map(|s| (s.key, s.module)).collect();
    assert_eq!(
        screens,
        [("test.counter.main", "game.party")],
        "the contract's screen, no gap"
    );
    let mut payload = Vec::new();
    mantis_core::wire::encode_into(&Count::<5001>(2), &mut payload);
    m.on_message(ExtensionKind(5001), &payload, &mut props);
    assert_eq!(prop(&mut props, "test.counter.total"), Some(Value::Int(2)));
    assert_eq!(
        m.on_intent("test.counter.ping", Some("1"), &mut props),
        IntentRoute::Handled
    );
    // The composed screen renders the override's ids; no collision, no missing screen.
    let (layout, theme) = m.compose(&["test.counter.main"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    assert!(ui.rect_of("game_party_size").is_some());
    assert!(ui.rect_of("test_counter_total").is_none());
    // A server feature state names the implementing module; it gates the contract.
    m.set_enabled("game.party", false, &mut props);
    assert_eq!(prop(&mut props, "test.counter.enabled"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn a_module_answers_refusals_of_its_commands_with_the_request() -> TestResult {
    let g = graph(&["test.counter"], "")?;
    let mut m = ClientModules::new(&g, &[Arc::new(Counter) as Arc<dyn ClientModule>])?;
    let mut props = Properties::new();
    m.publish_flags(&mut props);
    m.on_refused_tracked(ExtensionKind(5002), 7, ExtensionRefusal::NotAllowed, &mut props);
    assert_eq!(
        prop(&mut props, "test.counter.refused_request"),
        Some(Value::Int(7))
    );
    assert_eq!(
        prop(&mut props, "test.counter.refusal"),
        None,
        "answered by the module"
    );
    assert_eq!(m.stats().refusals_answered, 1);
    // Other kinds keep the generic text; disabling still always applies.
    m.on_refused_tracked(ExtensionKind(5000), 8, ExtensionRefusal::Invalid, &mut props);
    assert_eq!(
        prop(&mut props, "test.counter.refusal"),
        Some(Value::Text("invalid".to_owned()))
    );
    m.on_refused_tracked(
        ExtensionKind(5002),
        9,
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert!(!m.is_enabled("test.counter"));
    Ok(())
}
