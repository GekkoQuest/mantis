//! std.chat, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::{QueryError, ask};
use mantis_core::schedule::TickContext;
use mantis_core::time::Tick;
use mantis_core::wire::{Message, MessageId, ValidationError};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, MetricId, Metrics, Registrar, RegistryError, Require, ServerModule,
    caller, decode, position_of, session_of, sessions, tell,
};
use mantis_server::service::{SOCIAL_DELIVER, SOCIAL_PUBLISH, SocialLine, channel as social, to_service};
use std_chat_contract::{GUILD, LOCAL, LOCAL_RANGE, Line, PARTY, Say, WHISPER};
use std_guild_contract::GuildOf;
use std_party_contract::PartyOf;

/// Lines one character may say per window.
pub const BURST: u32 = 5;

/// The rate-limit window, in seconds.
pub const WINDOW_SECONDS: u64 = 5;

/// Chat state: rate limits per character. Simulation state.
#[derive(Debug, Default)]
pub struct Chat {
    /// Character -> (window start tick, lines in the window).
    pub windows: BTreeMap<u64, (Tick, u32)>,
    lines: Option<MetricId>,
}

impl StateHash for Chat {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.windows.len() as u64);
        for (c, (start, n)) in &self.windows {
            h.write_u64(*c);
            h.write_u64(start.0);
            h.write_u32(*n);
        }
    }
}

impl Resource for Chat {
    const NAME: &'static str = "std.chat.state";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.windows.len()).unwrap_or(u32::MAX));
        for (c, (start, n)) in &self.windows {
            e.u64(*c);
            e.u64(start.0);
            e.u32(*n);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.windows.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            let start = Tick(d.u64()?);
            let n = d.u32()?;
            self.windows.insert(c, (start, n));
        }
        Ok(())
    }
}

struct Checks;

impl std_chat_contract::Validators for Checks {
    fn validate_say(&self, msg: &Say) -> Result<(), ValidationError> {
        if msg.channel > GUILD {
            return Err(ValidationError("unknown channel"));
        }
        if msg.text.as_str().trim().is_empty() {
            return Err(ValidationError("empty line"));
        }
        if msg.text.as_str().chars().any(char::is_control) {
            return Err(ValidationError("control characters"));
        }
        Ok(())
    }
}

/// Counts the line against the speaker's window; false when over the limit.
fn admit(chat: &mut Chat, speaker: u64, ctx: &TickContext) -> bool {
    let window = WINDOW_SECONDS * u64::from(ctx.rate.hz());
    let slot = chat.windows.entry(speaker).or_insert((ctx.tick, 0));
    if ctx.tick.0 >= slot.0.0 + window {
        *slot = (ctx.tick, 0);
    }
    if slot.1 >= BURST {
        return false;
    }
    slot.1 += 1;
    true
}

fn recipients(
    world: &World,
    me: SessionId,
    from: u64,
    msg: &Say,
) -> Result<Vec<SessionId>, ExtensionRefusal> {
    match msg.channel {
        LOCAL => {
            let here = position_of(world, me).ok_or(ExtensionRefusal::NotAllowed)?;
            Ok(sessions(world)
                .filter(|s| {
                    position_of(world, *s).is_some_and(|p| (p - here).horizontal().length() <= LOCAL_RANGE)
                })
                .collect())
        }
        PARTY => match ask(world, &PartyOf(from)) {
            Ok(Some(party)) => Ok(party
                .members()
                .iter()
                .filter_map(|c| session_of(world, *c))
                .collect()),
            // No party, or the party module is off: not allowed (the chat
            // module itself is still on).
            Ok(None) | Err(QueryError::FeatureDisabled(_)) => Err(ExtensionRefusal::NotAllowed),
            Err(QueryError::NoProvider(_)) => Err(ExtensionRefusal::Invalid),
        },
        _ => {
            let to = session_of(world, msg.target).ok_or(ExtensionRefusal::NotAllowed)?;
            Ok(if to == me { vec![me] } else { vec![to, me] })
        }
    }
}

fn say(
    world: &mut World,
    ctx: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    std_chat_contract::parse_inbound(MessageId(Say::ID.0), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)?;
    let msg: Say = decode(payload)?;
    let from = caller(world, session)?;
    // Read when the line is said, so a live (Ops) change applies from the
    // tick it lands on.
    if msg.channel == WHISPER && !mantis_server::modules::flag(world, "std.chat.whispers") {
        return Err(ExtensionRefusal::NotAllowed);
    }
    // A whisper to a character in no session here, and every guild line,
    // crosses cells through the social role. Whether anything carries it
    // is not this cell's state: the line is queued as an output either way,
    // and the speaker is shown it here.
    let guild = if msg.channel == GUILD {
        Some(guild_of(world, from)?)
    } else {
        None
    };
    let elsewhere = msg.channel == WHISPER && session_of(world, msg.target).is_none();
    let to = if elsewhere || guild.is_some() {
        vec![session]
    } else {
        recipients(world, session, from, &msg)?
    };
    let chat = world.resource_mut::<Chat>().ok_or(ExtensionRefusal::Invalid)?;
    if !admit(chat, from, ctx) {
        return Err(ExtensionRefusal::NotAllowed);
    }
    let metric = chat.lines;
    let line = Line {
        channel: msg.channel,
        from,
        to: if msg.channel == WHISPER { msg.target } else { 0 },
        text: msg.text,
    };
    if elsewhere {
        let out = SocialLine {
            channel: social::WHISPER,
            from,
            to: msg.target,
            text: msg.text,
        };
        let _ = to_service(world, SOCIAL_PUBLISH, out.payload());
    }
    if let Some(guild) = guild {
        let out = SocialLine {
            channel: social::GUILD,
            from,
            to: u64::from(guild),
            text: msg.text,
        };
        let _ = to_service(world, SOCIAL_PUBLISH, out.payload());
    }
    for s in to {
        tell(world, s, &line);
    }
    if let (Some(id), Some(m)) = (metric, world.resource_mut::<Metrics>()) {
        m.add(id, 1);
    }
    Ok(())
}

/// The speaker's guild, through the `std.guild` contract. The guild
/// module is optional (chat does not depend on it): a package without one
/// has no guild channel.
fn guild_of(world: &World, from: u64) -> Result<u32, ExtensionRefusal> {
    match ask(world, &GuildOf(from)) {
        Ok(Some(g)) => Ok(g.id),
        // No guild, the guild module off, or no guild module in this
        // package: not allowed (the chat module itself is still on).
        Ok(None) | Err(QueryError::FeatureDisabled(_) | QueryError::NoProvider(_)) => {
            Err(ExtensionRefusal::NotAllowed)
        }
    }
}

/// A line the social role delivered to a character in this cell (a logged
/// service update). The sender's own copy is skipped: it was shown when the
/// line was said.
fn delivered(world: &mut World, _: &TickContext, payload: &[u8]) -> Result<(), &'static str> {
    let l = SocialLine::parse(payload).map_err(|_| "not a social line")?;
    if l.to == l.from {
        return Ok(());
    }
    let channel = match l.channel {
        social::WHISPER if mantis_server::modules::flag(world, "std.chat.whispers") => WHISPER,
        social::GUILD => GUILD,
        _ => return Ok(()),
    };
    if let Some(s) = session_of(world, l.to) {
        let line = Line {
            channel,
            from: l.from,
            to: if channel == WHISPER { l.to } else { 0 },
            text: l.text,
        };
        tell(world, s, &line);
    }
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.chat"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let lines = r.metric("lines");
        r.resource(Chat {
            lines: Some(lines),
            ..Chat::default()
        })?;
        r.handler(ExtensionKind(Say::ID.0), Require::Avatar, say)?;
        r.service(SOCIAL_DELIVER, delivered)?;
        Ok(())
    }
}
