//! Client mods end to end: the toy package's own mods (`packages/toy/mods`) loaded by the
//! real toy client against the whole toy server in-process.
//!
//! - `Hello` names both mods with the hashes of their files, and the server records them;
//! - on joining the world the cell permits both at the automation tier: the presentation
//!   mod (`toy.hud`) and the automation mod (`toy.helper`) run;
//! - the automation mod's intent goes through the chat module to the server, whose echo
//!   comes back as a chat line the presentation mod reads from the chat view model;
//! - the cell lowers the tier to presentation: the automation mod is demoted at once and
//!   neither its script nor its widgets reach the network; raised again, it is promoted
//!   with its script state.

#![expect(clippy::too_many_lines)] // The scenario reads top to bottom.

use std::sync::Arc;

use mantis_adapter_contract::ModTier;
use mantis_client::mods::{ModPackage, ModRoute, ModState};
use mantis_client::modules::IntentRoute;
use mantis_client::net::SessionState;
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_core::log::SessionId;
use mantis_core::time::Tick;
use mantis_server::intent::CellIntent;
use mantis_server::simnet::LinkConfig;
use toy_client::{HeadlessSink, ToyClient};
use toy_server::sim::Sim;
use toy_server::tunables::Tunables;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Client = ToyClient<mantis_server::simnet::SimClient, HeadlessSink>;

struct World {
    sim: Sim,
    client: Client,
    clock: Arc<ManualClock>,
    rate: mantis_core::time::TickRate,
    tick: u64,
}

impl World {
    fn step(&mut self) -> TestResult {
        let _ = self.sim.step_server().map_err(|e| format!("{e:?}"))?;
        self.tick += 1;
        self.clock
            .set(HostInstant::from_nanos(u64::try_from(tick_start_nanos(
                self.rate,
                Tick(self.tick),
            ))?));
        self.client.step(1);
        self.sim.step_bots();
        Ok(())
    }

    fn until(&mut self, what: &str, done: &dyn Fn(&Client) -> bool) -> TestResult {
        for _ in 0..600 {
            if done(&self.client) {
                return Ok(());
            }
            self.step()?;
        }
        Err(format!("timed out: {what}").into())
    }

    fn run(&mut self, ticks: u32) -> TestResult {
        for _ in 0..ticks {
            self.step()?;
        }
        Ok(())
    }

    /// A press on a mod's widget, as the UI reports it.
    fn press(&mut self, widget: &str, intent: &str, payload: Option<&str>) -> Result<ModRoute, String> {
        let c = &mut self.client;
        let (Some(mods), Some((registry, _))) = (c.mods.as_mut(), c.modules.as_mut()) else {
            return Err("no mods or modules".to_owned());
        };
        Ok(mods.on_widget_intent(widget, intent, payload, Some(registry), &mut c.module_props))
    }

    fn set_tier(&mut self, tier: ModTier) {
        for i in 0..self.sim.zone.cells().len() {
            if let Some(cell) = self.sim.zone.cell_mut(i) {
                cell.inbox().push(SessionId(0), CellIntent::SetModTier { tier });
            }
        }
    }
}

fn state(c: &Client, key: &str) -> Option<ModState> {
    c.mods.as_ref().and_then(|m| m.state(key))
}

/// A property in its display form (text as is, numbers in decimal, flags as `true`/`false`).
fn prop(c: &Client, name: &str) -> Option<String> {
    let p = &c.module_props;
    p.id(name).and_then(|id| p.get(id)).map(|v| {
        let mut s = String::new();
        v.write_display(&mut s);
        s
    })
}

fn sent(c: &Client) -> u64 {
    c.net.stats().extensions_sent
}

#[test]
fn the_package_mods_run_at_the_tier_each_cell_permits() -> TestResult {
    let tunables = Tunables::defaults()?;
    let rate = tunables.tick_rate;
    let sim = Sim::new(tunables, 23, |_| None).map_err(|e| format!("{e:?}"))?;
    let clock = Arc::new(ManualClock::new());
    let transport = sim.native_net.connect(LinkConfig::PERFECT);
    let mut client = ToyClient::new(transport, Arc::clone(&clock) as Arc<dyn HostClock>, HeadlessSink)?;

    // Both package mods load and pass the package's checks; Hello will name them.
    let dir = toy_client::package::package_mods_dir();
    let notices = client.load_mods(&dir)?;
    assert!(notices.is_empty(), "{notices:?}");
    let expected: Vec<(String, mantis_core::content::ContentHash)> = ["helper", "hud"]
        .iter()
        .map(|f| ModPackage::load(&dir.join(f)).map(|m| (m.key, m.hash)))
        .collect::<Result<_, _>>()?;
    let hello: Vec<(String, mantis_core::content::ContentHash)> = client
        .hello_mods
        .iter()
        .map(|m| (m.name.as_str().to_owned(), m.hash))
        .collect();
    assert_eq!(hello, expected);
    assert_eq!(
        expected.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["toy.helper", "toy.hud"]
    );
    client.start(b"mods");
    let mut w = World {
        sim,
        client,
        clock,
        rate,
        tick: 0,
    };

    // The open world permits both at the automation tier.
    w.until("in the world with both mods running", &|c| {
        matches!(c.net.state(), SessionState::Welcomed { avatar: Some(_), .. })
            && state(c, "toy.helper") == Some(ModState::Running(ModTier::Automation))
            && state(c, "toy.hud") == Some(ModState::Running(ModTier::Presentation))
    })?;
    let session = *w.sim.host.sessions().first().ok_or("no session")?;
    let recorded: Vec<(String, mantis_core::content::ContentHash)> = w
        .sim
        .host
        .client_modules(session)
        .ok_or("no modules recorded")?
        .iter()
        .map(|m| (m.name.as_str().to_owned(), m.hash))
        .collect();
    assert_eq!(recorded, expected, "the server recorded the announced mods");
    w.run(3)?;
    assert_eq!(prop(&w.client, "toy.helper.automation"), Some("true".to_owned()));
    assert!(
        prop(&w.client, "toy.hud.character").is_some_and(|t| t.starts_with("character ")),
        "the hud reads the client's view model"
    );

    // The automation mod greets through the chat module; the server's echo comes back
    // as a chat line, which the presentation mod reads from the chat view model.
    let before = sent(&w.client);
    assert_eq!(
        w.press("toy_helper_greet", "toy.helper.greet", None)?,
        ModRoute::Delivered
    );
    w.run(2)?;
    assert_eq!(sent(&w.client), before + 1);
    assert_eq!(
        w.press("toy_helper_wave", "std.chat.say", Some("waves"))?,
        ModRoute::Forwarded(IntentRoute::Handled)
    );
    w.until("the hud sees both chat lines", &|c| {
        prop(c, "toy.hud.lines") == Some("chat lines: 2".to_owned())
    })?;
    assert_eq!(sent(&w.client), before + 2);
    assert_eq!(prop(&w.client, "toy.helper.greetings"), Some("1".to_owned()));

    // The cell lowers the tier: the helper is demoted the moment the list arrives, and
    // nothing it does reaches the network.
    w.set_tier(ModTier::Presentation);
    w.until("the helper demoted", &|c| {
        state(c, "toy.helper") == Some(ModState::Running(ModTier::Presentation))
    })?;
    assert_eq!(
        state(&w.client, "toy.hud"),
        Some(ModState::Running(ModTier::Presentation))
    );
    let before = sent(&w.client);
    assert_eq!(
        w.press("toy_helper_greet", "toy.helper.greet", None)?,
        ModRoute::Delivered
    );
    assert_eq!(
        w.press("toy_helper_wave", "std.chat.say", Some("waves"))?,
        ModRoute::Refused
    );
    w.run(10)?;
    assert_eq!(sent(&w.client), before, "no intent left the demoted mod");
    assert_eq!(prop(&w.client, "toy.helper.refused"), Some("1".to_owned()));
    assert_eq!(
        prop(&w.client, "toy.helper.status"),
        Some("presentation only: greeting is off here".to_owned())
    );
    let notices = w
        .client
        .mods
        .as_ref()
        .map(mantis_client::mods::ModHost::notices)
        .unwrap_or_default();
    assert!(
        notices
            .iter()
            .any(|n| n.module == "toy.helper" && n.text.contains("presentation tier")),
        "{notices:?}"
    );

    // Raised again: promoted with its script state, and greeting works.
    w.set_tier(ModTier::Automation);
    w.until("the helper promoted", &|c| {
        state(c, "toy.helper") == Some(ModState::Running(ModTier::Automation))
    })?;
    assert_eq!(
        w.press("toy_helper_greet", "toy.helper.greet", None)?,
        ModRoute::Delivered
    );
    w.run(3)?;
    assert_eq!(sent(&w.client), before + 1);
    assert_eq!(prop(&w.client, "toy.helper.greetings"), Some("2".to_owned()));
    let stats = w
        .client
        .mods
        .as_ref()
        .map(mantis_client::mods::ModHost::stats)
        .unwrap_or_default();
    assert_eq!((stats.demotions, stats.promotions), (1, 1), "{stats:?}");
    assert_eq!(stats.presentation_refused, 1, "{stats:?}");
    assert_eq!(stats.script_errors, 0, "{stats:?}");
    println!(
        "client mods: {} permitted lists applied, {} intents handled, {} refused at the presentation tier",
        stats.permitted, stats.intents_handled, stats.presentation_refused
    );
    Ok(())
}
