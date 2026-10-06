//! Client mods: folders checked under the module contract, the package's checks with
//! visible notices, `Hello` naming the mods with their hashes, the server's permitted list
//! re-tiering mods the moment it arrives (demote, promote, stop), the host API gated by
//! tier inside the VM, and a presentation mod unable to reach an intent sink even through
//! the widgets it draws.

#![allow(clippy::too_many_lines)] // Scenario tests read top to bottom.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use mantis_adapter_contract::native::encode_outbound_frame;
use mantis_adapter_contract::{
    Channel, ConnectionId, ExtensionKind, ModTier, Outbound, PermittedModules, Transport, TransportEvent,
};
use mantis_client::input::device::{ButtonSource, KeyCode, MouseButton, RawInput};
use mantis_client::mods::{ModEnvironment, ModHost, ModPackage, ModRoute, ModState, scan};
use mantis_client::modules::{
    ClientModule, ClientModules, ClientRegistrar, ClientRegistryError, ModuleContext, ModuleError,
    ModuleNetLink, ModuleUiLink, module_link,
};
use mantis_client::net::{NativeSession, NetConfig, move_channel};
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::time::{HostClock, ManualClock};
use mantis_client::ui_layer::UiLayer;
use mantis_core::content::ContentHash;
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, MessageId, Wire, WireString};
use mantis_ui::{FontLibrary, Properties, Value, test_font};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Files = BTreeMap<&'static str, String>;

// ---------------------------------------------------------------------------------------
// A module standing in for a first-party feature: its intent `test.sink.send` queues a
// client-to-server message (the intent sink a mod must not reach unless permitted).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Send(u32);

impl Wire for Send {
    fn encode(&self, e: &mut mantis_core::wire::Encoder<'_>) {
        self.0.encode(e);
    }
    fn decode(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self(u32::decode(d)?))
    }
}

impl Message for Send {
    const ID: MessageId = MessageId(6000);
    const NAME: &'static str = "Send";
}

fn on_send(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let n: u32 = payload.and_then(|p| p.parse().ok()).ok_or(ModuleError::Invalid)?;
    ctx.send(&Send(n))
}

fn on_enabled(ctx: &mut ModuleContext<'_>, _enabled: bool) {
    ctx.set_text("secret", "view model value");
}

struct Sink;

impl ClientModule for Sink {
    fn key(&self) -> &'static str {
        "test.sink"
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(6000..=6000)?;
        r.on_intent("test.sink.send", on_send)?;
        r.on_enabled(on_enabled);
        r.screen(
            "test.sink.main",
            r#"panel id=test_sink_panel { button id=test_sink_button text="Send" intent="test.sink.send" payload="1" }"#,
            None,
        )?;
        Ok(())
    }
}

fn graph() -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = vec![Discovered {
        origin: "test".to_owned(),
        manifest: parse_manifest("[module]\nkey = \"test.sink\"\nversion = \"0.2.0\"\n")?,
    }];
    let package = parse_package("[package]\nname = \"test\"\n")?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

// ---------------------------------------------------------------------------------------
// Mod folders, in memory.

const WATCH_LAYOUT: &str = r#"panel id=test_watch_panel {
  text id=test_watch_seen bind="test.watch.seen"
  button id=test_watch_send text="Send" intent="test.sink.send" payload="7"
  input id=test_watch_field placeholder="n" submit="test.sink.send"
  button id=test_watch_poke text="Poke" intent="test.watch.poke"
  button id=test_watch_fire text="Fire" intent="test.watch.fire"
}"#;

const WATCH_SCRIPT: &str = r#"
state.pokes = state.pokes or 0
function on_poke(p) state.pokes = state.pokes + 1 end
function on_fire(p) if host.intent ~= nil then host.intent("test.sink.send", "9") end end
mantis.on("test.watch.poke", "on_poke")
mantis.on("test.watch.fire", "on_fire")
function on_tick(t)
  host.set("has_intent", host.intent ~= nil)
  host.set("pokes", state.pokes)
  host.set("seen", host.get("test.sink.secret"))
end
"#;

/// A mod `key` asking for `tier` (`demote` for an automation mod), with the watch screen
/// and script renamed to its key.
fn mod_files(key: &str, tier: &str, demote: bool, deps: &str) -> Files {
    let prefix = key.replace('.', "_");
    let mut files = Files::new();
    files.insert(
        "manifest.toml",
        format!("[module]\nkey = \"{key}\"\nversion = \"0.1.0\"\n[dependencies]\n{deps}"),
    );
    let mut m =
        format!("[mod]\ntier = \"{tier}\"\nscreens = [\"panel\"]\ntheme = \"style\"\nscripts = [\"main\"]\n");
    if demote {
        m.push_str("demote = true\n");
    }
    files.insert("client/mod.toml", m);
    files.insert(
        "client/panel.layout",
        WATCH_LAYOUT
            .replace("test_watch", &prefix)
            .replace("test.watch", key),
    );
    files.insert(
        "client/style.theme",
        format!("theme {{ style {prefix}_panel {{ padding = 4 }} }}"),
    );
    files.insert("scripts/main.luau", WATCH_SCRIPT.replace("test.watch", key));
    files
}

fn load(origin: &str, files: &Files) -> Result<ModPackage, mantis_client::mods::ModError> {
    ModPackage::from_files(origin, &mut |path| {
        files.get(path).cloned().ok_or_else(|| "no such file".to_owned())
    })
}

fn refusal(files: &Files) -> String {
    match load("x", files) {
        Ok(_) => "loaded".to_owned(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn mod_folders_follow_the_module_contract_and_never_reach_the_server() -> TestResult {
    let ok = mod_files("test.watch", "automation", true, "");
    let m = load("watch", &ok)?;
    assert_eq!(m.key, "test.watch");
    assert_eq!(m.tier, ModTier::Automation);
    assert!(m.demote);
    assert_eq!(
        m.screens.first().map(|s| s.key.as_str()),
        Some("test.watch.panel")
    );
    assert_eq!(m.id_prefix(), "test_watch_");
    // The hash covers every file read: stable, and moved by any change.
    assert_eq!(load("watch", &ok)?.hash, m.hash);
    let mut changed = ok.clone();
    changed.insert("scripts/main.luau", format!("{WATCH_SCRIPT}\n-- edited"));
    assert_ne!(load("watch", &changed)?.hash, m.hash);

    let edit = |file: &'static str, f: &dyn Fn(&str) -> String| {
        let mut files = ok.clone();
        let text = files.get(file).cloned().unwrap_or_default();
        files.insert(file, f(&text));
        refusal(&files)
    };
    let cases: [(String, &str); 10] = [
        (
            edit("manifest.toml", &|t| {
                t.replace("[dependencies]", "schemas = [\"x.idl\"]\n[dependencies]")
            }),
            "mods never run on the server",
        ),
        (
            edit("manifest.toml", &|t| {
                t.replace("[dependencies]", "contract = \"test.sink\"\n[dependencies]")
            }),
            "no contract but its own key",
        ),
        (
            edit("manifest.toml", &|t| t.replace("test.watch", "client.watch")),
            "`client.*` keys",
        ),
        (
            edit("client/mod.toml", &|t| format!("{t}speed = 2\n")),
            "unknown key `speed`",
        ),
        (
            edit("client/mod.toml", &|t| t.replace("automation", "presentation")),
            "`demote` applies only to automation mods",
        ),
        (
            edit("client/panel.layout", &|t| {
                t.replace("id=test_watch_seen", "id=seen")
            }),
            "must start with `test_watch_`",
        ),
        (
            edit("client/panel.layout", &|t| {
                t.replace("button id=test_watch_poke", "button")
            }),
            "every button and input needs an id",
        ),
        (
            edit("client/style.theme", &|_| {
                "theme { style std_chat_panel { padding = 0 } }".to_owned()
            }),
            "style `std_chat_panel` must start with `test_watch_`",
        ),
        (
            edit("client/panel.layout", &|t| {
                format!("theme {{ style test_watch_x {{ padding = 1 }} }}\n{t}")
            }),
            "styles belong in the mod's theme file",
        ),
        (
            {
                let mut f = ok.clone();
                f.remove("scripts/main.luau");
                refusal(&f)
            },
            "scripts/main.luau: no such file",
        ),
    ];
    for (got, want) in &cases {
        assert!(got.contains(want), "expected `{want}`, got `{got}`");
    }
    // A presentation mod's widgets may emit only its own intents.
    let presentation = mod_files("test.watch", "presentation", false, "");
    let why = refusal(&presentation);
    assert!(
        why.contains("a presentation mod's widgets may emit only its own intents")
            && why.contains("test.sink.send"),
        "{why}"
    );
    Ok(())
}

#[test]
fn the_package_decides_which_mods_load_and_hello_names_only_those() -> TestResult {
    let g = graph()?;
    let permitted: Vec<String> = ["test.watch", "test.needy", "test.sink", "test.sink_x"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let watch = load(
        "watch",
        &mod_files("test.watch", "automation", true, "\"test.sink\" = \"0.2\""),
    )?;
    let needy = load(
        "needy",
        &mod_files("test.needy", "automation", true, "\"test.party\" = \"0.1\""),
    )?;
    let stranger = load("stranger", &mod_files("test.stranger", "automation", true, ""))?;
    let squatter = load("squatter", &mod_files("test.sink", "automation", true, ""))?;
    let overlap = load("overlap", &mod_files("test.sink_x", "automation", true, ""))?;
    let twin = load("twin", &mod_files("test.watch", "automation", true, ""))?;
    let hash = watch.hash;
    let host = ModHost::new(
        vec![watch, needy, stranger, squatter, overlap, twin],
        vec![mantis_client::mods::ModNotice {
            module: "broken".to_owned(),
            text: "not loaded: manifest.toml: missing `module.key`".to_owned(),
        }],
        &ModEnvironment {
            permitted: &permitted,
            graph: &g,
        },
    );
    let states: Vec<(&str, ModState)> = host.states().collect();
    assert_eq!(
        states,
        [
            ("test.watch", ModState::Waiting),
            ("test.needy", ModState::Refused),
            ("test.stranger", ModState::Refused),
            ("test.sink", ModState::Refused),
            ("test.sink_x", ModState::Refused),
            ("test.watch", ModState::Refused),
        ]
    );
    let notices = host.notices();
    let text = |module: &str| -> Vec<&str> {
        notices
            .iter()
            .filter(|n| n.module == module)
            .map(|n| n.text.as_str())
            .collect()
    };
    assert_eq!(
        text("broken"),
        ["not loaded: manifest.toml: missing `module.key`"]
    );
    assert!(
        text("test.needy")
            .iter()
            .any(|t| t.contains("needs `test.party` 0.1.0"))
    );
    assert!(
        text("test.stranger")
            .iter()
            .any(|t| t.contains("not permitted by this package"))
    );
    assert!(
        text("test.sink")
            .iter()
            .any(|t| t.contains("module `test.sink` of this package has this key"))
    );
    assert!(
        text("test.sink_x")
            .iter()
            .any(|t| t.contains("would overlap module `test.sink`"))
    );
    assert!(
        text("test.watch")
            .iter()
            .any(|t| t.contains("another mod already has the key"))
    );
    // Only the mod that passed is announced, with its content hash.
    let hello = host.hello_modules();
    assert_eq!(hello.len(), 1);
    assert_eq!(
        hello.first().map(|e| (e.name.as_str(), e.hash)),
        Some(("test.watch", hash))
    );

    // Hello carries the list exactly.
    let mut session = session(Scripted::default());
    session.start_with_modules(b"tok", &hello);
    let frame = session
        .transport_mut()
        .sent
        .first()
        .cloned()
        .ok_or("nothing sent")?;
    let id = u16::from_le_bytes([*frame.get(1).ok_or("id")?, *frame.get(2).ok_or("id")?]);
    match mantis_adapter_contract::parse_inbound(MessageId(id), frame.get(3..).ok_or("payload")?)? {
        mantis_adapter_contract::Inbound::Hello(h) => {
            let got: Vec<_> = h
                .modules
                .iter()
                .map(|m| (m.name.as_str().to_owned(), m.hash))
                .collect();
            assert_eq!(got, [("test.watch".to_owned(), hash)]);
        }
        other => return Err(format!("unexpected {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_mods_directory_is_scanned_in_name_order_with_notices_for_bad_folders() -> TestResult {
    let dir = std::env::temp_dir().join(format!("mantis-mods-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (folder, files) in [
        ("b_watch", mod_files("test.watch", "presentation", false, "")),
        ("a_ok", mod_files("test.ok", "automation", true, "")),
    ] {
        for (path, text) in &files {
            let at = path.split('/').fold(dir.join(folder), |p, s| p.join(s));
            std::fs::create_dir_all(at.parent().ok_or("parent")?)?;
            std::fs::write(at, text)?;
        }
    }
    let (mods, notices) = scan(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        mods.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
        ["test.ok"]
    );
    assert_eq!(notices.len(), 1);
    let n = notices.first().ok_or("notice")?;
    assert_eq!(n.module, "b_watch");
    assert!(n.text.contains("presentation mod's widgets"), "{}", n.text);
    // A missing directory holds no mods.
    let (none, quiet) = scan(&dir);
    assert!(none.is_empty() && quiet.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The session and the UI.

/// A transport that delivers queued server frames on poll and records what is sent.
#[derive(Default)]
struct Scripted {
    inbound: Vec<Vec<u8>>,
    sent: Vec<Vec<u8>>,
}

impl Transport for Scripted {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        for bytes in self.inbound.drain(..) {
            sink(TransportEvent::Frame {
                conn: ConnectionId(0),
                channel: Channel::Reliable,
                bytes: &bytes,
            });
        }
    }
    fn send(
        &mut self,
        _conn: ConnectionId,
        _channel: Channel,
        bytes: &[u8],
    ) -> Result<(), mantis_adapter_contract::TransportError> {
        self.sent.push(bytes.to_vec());
        Ok(())
    }
    fn disconnect(&mut self, _conn: ConnectionId) {}
    fn kind(&self) -> mantis_adapter_contract::TransportKind {
        mantis_adapter_contract::TransportKind::Quic
    }
    fn max_unreliable_payload(&self) -> usize {
        1100
    }
}

fn session(t: Scripted) -> NativeSession<Scripted> {
    let clock: Arc<dyn HostClock> = Arc::new(ManualClock::new());
    let (snapshots, _inbox) = mantis_client::snapshot::snapshot_channel(2, 4);
    let (_outbox, moves) = move_channel(4);
    NativeSession::new(t, clock, NetConfig::new(ContentHash::ZERO), snapshots, moves)
}

fn permitted(tier: ModTier, keys: &[&str]) -> PermittedModules {
    let keys: Vec<WireString<32>> = keys.iter().filter_map(|k| WireString::new(k)).collect();
    PermittedModules {
        tier,
        modules: BoundedArray::from_slice(&keys).unwrap_or_default(),
    }
}

/// A client: the session, both link ends, and a module UI running `mods`.
struct Client {
    session: NativeSession<Scripted>,
    net: ModuleNetLink,
    layer: UiLayer,
}

impl Client {
    fn new(mods: Vec<ModPackage>, allowed: &[&str]) -> Result<Self, Box<dyn std::error::Error>> {
        let g = graph()?;
        let registry = ClientModules::new(&g, &[Arc::new(Sink) as Arc<dyn ClientModule>])?;
        let (net, ui_end): (ModuleNetLink, ModuleUiLink) = module_link(16);
        let mut fonts = FontLibrary::new();
        let latin = fonts.add_font(test_font::latin())?;
        fonts.define_stack("ui", &[latin])?;
        let mut layer = UiLayer::with_modules(fonts, registry, ui_end, Vec::new(), &["test.sink.main"])?;
        let allowed: Vec<String> = allowed.iter().map(|s| (*s).to_owned()).collect();
        let host = ModHost::new(
            mods,
            Vec::new(),
            &ModEnvironment {
                permitted: &allowed,
                graph: &g,
            },
        );
        layer.set_mods(host)?;
        let mut c = Self {
            session: session(Scripted::default()),
            net,
            layer,
        };
        c.frame();
        Ok(c)
    }

    /// The server sends a permitted list.
    fn server_permits(&mut self, p: &PermittedModules) {
        let mut frame = Vec::new();
        encode_outbound_frame(&Outbound::PermittedModules(*p), &mut frame);
        self.session.transport_mut().inbound.push(frame);
    }

    /// One network step and one UI frame (intents drained into `host`), then the network
    /// sends what the modules queued.
    fn frame_with(&mut self, host: &mut Vec<String>) {
        self.session.step();
        self.net.pump(&mut self.session);
        self.layer.update();
        let _ = self.layer.ui_mut().frame([900.0, 900.0], 1.0);
        let ui = &mut self.layer;
        let mut seen = Vec::new();
        ui.drain_intents(|i| seen.push(i.intent));
        for id in seen {
            host.push(ui.ui().intent_name(id).unwrap_or("?").to_owned());
        }
        self.layer.update();
        self.net.pump(&mut self.session);
    }

    fn frame(&mut self) {
        let mut host = Vec::new();
        self.frame_with(&mut host);
        assert!(host.is_empty(), "the host saw intents: {host:?}");
    }

    fn click(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let r = self
            .layer
            .ui()
            .rect_of(id)
            .ok_or_else(|| format!("no widget {id}"))?;
        let _ = self.layer.handle(&PlatformEvent::CursorMoved {
            x: r.x + r.w * 0.5,
            y: r.y + r.h * 0.5,
        });
        for pressed in [true, false] {
            let _ = self.layer.handle(&PlatformEvent::Input(RawInput::Button {
                source: ButtonSource::Mouse(MouseButton::Left),
                pressed,
            }));
        }
        Ok(())
    }

    fn type_and_submit(&mut self, id: &str, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.click(id)?;
        let _ = self.layer.handle(&PlatformEvent::Text(text.to_owned()));
        let _ = self.layer.handle(&PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(KeyCode::Enter),
            pressed: true,
        }));
        let _ = self.layer.handle(&PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(KeyCode::Escape),
            pressed: true,
        }));
        Ok(())
    }

    fn prop(&mut self, name: &str) -> Option<Value> {
        let props: &mut Properties = self.layer.ui_mut().properties_mut();
        let id = props.intern(name);
        props.get(id).cloned()
    }

    /// Extensions the session sent: (kind, payload value).
    fn sent(&mut self) -> Vec<u32> {
        let mut out = Vec::new();
        for frame in &self.session.transport_mut().sent {
            let (Some(a), Some(b), Some(rest)) = (frame.get(1), frame.get(2), frame.get(3..)) else {
                continue;
            };
            if let Ok(mantis_adapter_contract::Inbound::Extension(e)) =
                mantis_adapter_contract::parse_inbound(MessageId(u16::from_le_bytes([*a, *b])), rest)
                && e.kind == ExtensionKind(6000)
            {
                let bytes: Vec<u8> = e.payload.iter().copied().collect();
                if let Ok(Send(n)) = mantis_core::wire::decode_message::<Send>(&bytes) {
                    out.push(n);
                }
            }
        }
        out
    }

    fn notices(&mut self) -> String {
        let mut s = String::new();
        if let Some(Value::List(items)) = self.prop("client.mods.notices") {
            for i in items {
                if let (Some(Value::Text(m)), Some(Value::Text(t))) = (i.field("module"), i.field("text")) {
                    let _ = writeln!(s, "{m}: {t}");
                }
            }
        }
        s
    }
}

#[test]
fn a_presentation_mod_cannot_reach_an_intent_sink_even_through_its_widgets() -> TestResult {
    let watch = load("watch", &mod_files("test.watch", "automation", true, ""))?;
    let mut c = Client::new(vec![watch], &["test.watch"])?;
    // Nothing runs before the server's list (fail closed): no screen, no VM.
    assert_eq!(
        c.layer.mods().and_then(|m| m.state("test.watch")),
        Some(ModState::Waiting)
    );
    assert!(c.layer.ui().rect_of("test_watch_send").is_none());
    assert!(c.layer.ui().rect_of("test_sink_button").is_some());

    // A competitive instance: presentation only. The automation mod is demoted.
    c.server_permits(&permitted(ModTier::Presentation, &["test.watch"]));
    c.frame();
    c.frame();
    let mods = c.layer.mods().ok_or("mods")?;
    assert_eq!(
        mods.state("test.watch"),
        Some(ModState::Running(ModTier::Presentation))
    );
    assert!(
        c.layer.ui().rect_of("test_watch_send").is_some(),
        "its screen is drawn"
    );
    assert_eq!(
        c.prop("test.watch.has_intent"),
        Some(Value::Bool(false)),
        "no host.intent at all"
    );
    assert_eq!(c.prop("test.watch.tier"), Some(Value::text("presentation")));
    assert_eq!(
        c.prop("test.watch.seen"),
        Some(Value::text("view model value")),
        "it reads view models"
    );
    assert!(
        c.notices()
            .contains("test.watch: running at the presentation tier"),
        "{}",
        c.notices()
    );

    // Its widgets that name the module's intent reach nothing: not the module, not the
    // host, not the network. Buttons and inputs alike.
    c.click("test_watch_send")?;
    let mut host = Vec::new();
    c.frame_with(&mut host);
    c.type_and_submit("test_watch_field", "5")?;
    c.frame_with(&mut host);
    assert!(host.is_empty(), "{host:?}");
    let registry = c.layer.modules().ok_or("modules")?.stats();
    assert_eq!((registry.intents, registry.intents_refused), (0, 0));
    assert!(c.sent().is_empty());
    let stats = c.layer.mods().ok_or("mods")?.stats();
    assert_eq!(stats.presentation_refused, 2);
    // A list item's widget is attributed by its element id too.
    let mut props = Properties::new();
    let route = c.layer.mods_mut().ok_or("mods")?.on_widget_intent(
        "test_sink_list[3].test_watch_send",
        "test.sink.send",
        Some("3"),
        None,
        &mut props,
    );
    assert_eq!(route, ModRoute::Refused);
    // The same intent from the module's own widget still works.
    c.click("test_sink_button")?;
    c.frame();
    assert_eq!(c.sent(), [1]);

    // Its own intents go to its VM; `host.intent` stays out of reach from there too.
    c.click("test_watch_poke")?;
    c.frame();
    c.click("test_watch_fire")?;
    c.frame();
    c.frame();
    assert_eq!(c.prop("test.watch.pokes"), Some(Value::Int(1)));
    assert_eq!(c.sent(), [1]);

    // Automation permitted: promoted in place, script state carried over.
    c.server_permits(&permitted(ModTier::Automation, &["test.watch"]));
    c.frame();
    c.frame();
    assert_eq!(
        c.layer.mods().and_then(|m| m.state("test.watch")),
        Some(ModState::Running(ModTier::Automation))
    );
    assert_eq!(c.prop("test.watch.has_intent"), Some(Value::Bool(true)));
    assert_eq!(
        c.prop("test.watch.pokes"),
        Some(Value::Int(1)),
        "state survives the restart"
    );
    c.click("test_watch_send")?;
    c.frame();
    c.click("test_watch_fire")?;
    c.frame();
    c.frame();
    assert_eq!(c.sent(), [1, 7, 9]);

    // The tier is lowered between a click and the next frame: the list is applied before
    // the mod runs again, so the click is handled at the presentation tier and nothing is
    // sent.
    c.click("test_watch_fire")?;
    c.server_permits(&permitted(ModTier::Presentation, &["test.watch"]));
    c.frame();
    c.frame();
    c.click("test_watch_send")?;
    c.frame();
    assert_eq!(c.sent(), [1, 7, 9]);
    let stats = c.layer.mods().ok_or("mods")?.stats();
    assert_eq!((stats.demotions, stats.promotions), (1, 1));

    // Dropped from the list: stopped, its screen removed, and the player is told.
    c.server_permits(&permitted(ModTier::Automation, &[]));
    c.frame();
    assert_eq!(
        c.layer.mods().and_then(|m| m.state("test.watch")),
        Some(ModState::Stopped)
    );
    assert!(c.layer.ui().rect_of("test_watch_send").is_none());
    assert!(
        c.notices()
            .contains("test.watch: stopped: not permitted in this instance"),
        "{}",
        c.notices()
    );
    assert_eq!(c.prop("test.watch.tier"), Some(Value::text("off")));
    Ok(())
}

#[test]
fn an_over_tier_mod_is_stopped_with_a_visible_notice() -> TestResult {
    let strict = load("strict", &mod_files("test.strict", "automation", false, ""))?;
    let stranger = load("stranger", &mod_files("test.stranger", "automation", true, ""))?;
    let mut c = Client::new(vec![strict, stranger], &["test.strict"])?;
    assert_eq!(c.prop("client.mods.has_notices"), Some(Value::Bool(true)));
    assert!(
        c.notices()
            .contains("test.stranger: not loaded: not permitted by this package")
    );
    assert!(
        c.layer.ui().rect_of("client_mods_notice_list").is_some(),
        "the notices panel is drawn"
    );

    c.server_permits(&permitted(ModTier::Presentation, &["test.strict"]));
    c.frame();
    assert_eq!(
        c.layer.mods().and_then(|m| m.state("test.strict")),
        Some(ModState::Stopped)
    );
    assert!(c.layer.ui().rect_of("test_strict_send").is_none());
    assert!(
        c.notices().contains(
            "test.strict: stopped: needs the automation tier; this instance permits presentation only"
        ),
        "{}",
        c.notices()
    );
    // Back where automation is permitted, it starts.
    c.server_permits(&permitted(ModTier::Automation, &["test.strict"]));
    c.frame();
    c.frame();
    assert_eq!(
        c.layer.mods().and_then(|m| m.state("test.strict")),
        Some(ModState::Running(ModTier::Automation))
    );
    assert!(c.layer.ui().rect_of("test_strict_send").is_some());
    assert!(!c.notices().contains("test.strict"), "{}", c.notices());
    Ok(())
}

#[test]
fn a_lowered_tier_is_never_lost_on_a_full_link() -> TestResult {
    let (mut net, mut ui_end) = module_link(1);
    let mut s = session(Scripted::default());
    let g = graph()?;
    let mut registry = ClientModules::new(&g, &[Arc::new(Sink) as Arc<dyn ClientModule>])?;
    let mut props = Properties::new();
    // A module message fills the one slot; then the cell lowers the tier.
    let mut frame = Vec::new();
    encode_outbound_frame(
        &Outbound::ExtensionMessage(mantis_adapter_contract::ExtensionMessage {
            kind: ExtensionKind(6000),
            payload: BoundedArray::new(),
        }),
        &mut frame,
    );
    s.transport_mut().inbound.push(frame);
    s.step();
    net.pump(&mut s);
    let mut frame = Vec::new();
    encode_outbound_frame(
        &Outbound::PermittedModules(permitted(ModTier::Presentation, &["test.watch"])),
        &mut frame,
    );
    s.transport_mut().inbound.push(frame);
    s.step();
    net.pump(&mut s);
    ui_end.pump(&mut registry, &mut props);
    assert!(ui_end.take_permitted().is_none(), "the channel had no room yet");
    net.pump(&mut s);
    ui_end.pump(&mut registry, &mut props);
    assert_eq!(
        ui_end.take_permitted().map(|p| p.tier),
        Some(ModTier::Presentation)
    );
    Ok(())
}
