//! std.titles, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::Events;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::wire::{BoundedArray, Message, MessageId, ValidationError};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, Registrar, RegistryError, Require,
    ServerModule, SystemOutcomes, caller, decode, emit_outcome, tell_character,
};
use std_titles_contract::{
    ActiveTitle, AwardTitle, GrantTitle, MAX_TITLES, SetActiveTitle, ShowTitles, TITLES_TABLE, TitleEarned,
    Titles,
};

/// Titles held and shown. Simulation state.
#[derive(Debug, Default)]
pub struct Holders {
    /// Character -> titles held.
    pub owned: BTreeMap<u64, BTreeSet<u32>>,
    /// Character -> title shown.
    pub active: BTreeMap<u64, u32>,
    /// Titles that exist (content).
    pub known: BTreeSet<u32>,
}

impl Holders {
    /// Parses the titles table (`id name` lines, `#` comments).
    ///
    /// # Errors
    /// The first malformed line.
    pub fn parse(text: &str) -> Result<BTreeSet<u32>, String> {
        let mut known = BTreeSet::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let id = line
                .split_whitespace()
                .next()
                .and_then(|id| id.parse::<u32>().ok())
                .filter(|id| *id > 0)
                .ok_or(format!("line {}: expected `id name`", n + 1))?;
            known.insert(id);
        }
        Ok(known)
    }

    /// Awards `title`; false when unknown, already held, or the list is full.
    fn award(&mut self, character: u64, title: u32) -> bool {
        if !self.known.contains(&title) {
            return false;
        }
        let owned = self.owned.entry(character).or_default();
        if owned.len() >= MAX_TITLES {
            return false;
        }
        owned.insert(title)
    }
}

impl StateHash for Holders {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.owned.len() as u64);
        for (c, set) in &self.owned {
            h.write_u64(*c);
            h.write_u64(set.len() as u64);
            for t in set {
                h.write_u32(*t);
            }
        }
        h.write_u64(self.active.len() as u64);
        for (c, t) in &self.active {
            h.write_u64(*c);
            h.write_u32(*t);
        }
    }
}

impl Resource for Holders {
    const NAME: &'static str = "std.titles.holders";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.owned.len()).unwrap_or(u32::MAX));
        for (c, titles) in &self.owned {
            e.u64(*c);
            e.u32(u32::try_from(titles.len()).unwrap_or(u32::MAX));
            for t in titles {
                e.u32(*t);
            }
        }
        e.u32(u32::try_from(self.active.len()).unwrap_or(u32::MAX));
        for (c, t) in &self.active {
            e.u64(*c);
            e.u32(*t);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.owned.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            let mut titles = BTreeSet::new();
            for _ in 0..d.u32()? {
                titles.insert(d.u32()?);
            }
            self.owned.insert(c, titles);
        }
        self.active.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            self.active.insert(c, d.u32()?);
        }
        Ok(())
    }
}

struct Checks;

impl std_titles_contract::Validators for Checks {
    fn validate_set_active_title(&self, _msg: &SetActiveTitle) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_grant_title(&self, msg: &GrantTitle) -> Result<(), ValidationError> {
        if msg.character == 0 || msg.title == 0 {
            return Err(ValidationError("malformed grant"));
        }
        Ok(())
    }
    fn validate_show_titles(&self, _msg: &ShowTitles) -> Result<(), ValidationError> {
        Ok(())
    }
}

fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_titles_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

fn send_titles(world: &mut World, character: u64) {
    let Some(h) = world.resource::<Holders>() else {
        return;
    };
    let owned: Vec<u32> = h
        .owned
        .get(&character)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    let msg = Titles {
        owned: BoundedArray::from_slice(&owned).unwrap_or_default(),
        active: h.active.get(&character).copied().unwrap_or(0),
    };
    tell_character(world, character, &msg);
}

fn earned(world: &mut World, character: u64, title: u32) {
    if let Some(q) = world.resource_mut::<Events<TitleEarned>>() {
        q.send(TitleEarned { character, title });
    }
    send_titles(world, character);
}

fn set_active(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(SetActiveTitle::ID.0, payload)?;
    let msg: SetActiveTitle = decode(payload)?;
    let me = caller(world, session)?;
    let h = world.resource_mut::<Holders>().ok_or(ExtensionRefusal::Invalid)?;
    if msg.title == 0 {
        h.active.remove(&me);
    } else if h.owned.get(&me).is_some_and(|o| o.contains(&msg.title)) {
        h.active.insert(me, msg.title);
    } else {
        return Err(ExtensionRefusal::NotAllowed);
    }
    send_titles(world, me);
    Ok(())
}

fn show(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(ShowTitles::ID.0, payload)?;
    let me = caller(world, session)?;
    send_titles(world, me);
    Ok(())
}

fn grant(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let result = (|| {
        if cmd.session.is_some() {
            return Err(ExtensionRefusal::NotAllowed);
        }
        check(GrantTitle::ID.0, cmd.payload.as_slice())?;
        let msg: GrantTitle = decode(cmd.payload.as_slice())?;
        let h = world.resource_mut::<Holders>().ok_or(ExtensionRefusal::Invalid)?;
        if !h.award(msg.character, msg.title) {
            return Err(ExtensionRefusal::NotAllowed);
        }
        earned(world, msg.character, msg.title);
        Ok(())
    })();
    match result {
        Ok(()) => ModuleOutcome {
            kind: cmd.kind,
            session: cmd.session,
            result: Ok(()),
            payload: cmd.payload,
        },
        Err(r) => ModuleOutcome::refused(cmd, r),
    }
}

/// Grants titles other modules asked for last tick.
fn awards(world: &mut World, _: &TickContext) -> Result<(), SystemError> {
    let asked: Vec<AwardTitle> = world
        .resource::<Events<AwardTitle>>()
        .map(|q| q.read().to_vec())
        .unwrap_or_default();
    for a in asked {
        let granted = world
            .resource_mut::<Holders>()
            .ok_or(SystemError::Invariant("holders"))?
            .award(a.character, a.title);
        if granted {
            earned(world, a.character, a.title);
            // Durable: recorded as an outcome in this tick (lead ruling,
            // M4), shaped like the outcome of a granting command. The
            // award queue (64) is smaller than the tick's sink.
            let grant = GrantTitle {
                character: a.character,
                title: a.title,
            };
            emit_outcome(
                world,
                ModuleOutcome::ok(ExtensionKind(GrantTitle::ID.0), None, &grant),
            )?;
        }
    }
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.titles"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let table = r.table(TITLES_TABLE)?;
        let text =
            core::str::from_utf8(&table).map_err(|_| RegistryError::Module("titles table is not UTF-8"))?;
        let known = Holders::parse(text).map_err(|_| RegistryError::Module("malformed titles table"))?;
        r.resource(Holders {
            known,
            ..Holders::default()
        })?;
        r.event::<AwardTitle>(64)?;
        r.event::<TitleEarned>(64)?;
        r.handler(ExtensionKind(SetActiveTitle::ID.0), Require::Joined, set_active)?;
        r.handler(ExtensionKind(ShowTitles::ID.0), Require::Joined, show)?;
        r.command(ExtensionKind(GrantTitle::ID.0), grant)?;
        r.query::<ActiveTitle>(|w, q| w.resource::<Holders>().and_then(|h| h.active.get(&q.0).copied()))?;
        let access = r
            .access()
            .read_resource::<Events<AwardTitle>>()
            .write_resource::<Holders>()
            .write_resource::<SystemOutcomes>()
            .build()?;
        r.system(
            SystemDesc {
                name: "std.titles.awards",
                phase: Phase::Effects,
                priority: 0,
                access,
            },
            awards,
        )?;
        Ok(())
    }
}
