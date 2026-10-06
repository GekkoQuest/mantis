//! std.party, server half. See `RULES.md` beside the manifest.
//!
//! The social role owns every party (lead ruling, M8): this module checks
//! what a cell can check, relays the operation through the service outbox,
//! and keeps a read-only projection of the parties its characters are in,
//! fed by logged service updates. A party therefore survives any transfer:
//! no cell owns it.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::Events;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::social::{PARTY_OP, PARTY_UPDATE, PartyOp, PartyUpdate};
use mantis_core::wire::{BoundedArray, Message, ValidationError, decode_exact};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, Registrar, RegistryError, Require, ServerModule, caller, decode,
    session_of, session_of_entity, tell_character,
};
use mantis_server::service::{ServiceOutbox, to_service_encoded};
use std_party_contract::{
    Accept, Disbanded, Invite, Invited, Kick, Leave, PartyChanged, PartyOf, PartyRefused, PartyView, Roster,
};

/// How often, in seconds, the projection is offered back to the social
/// role so an authority that restarted rebuilds it.
pub const RESTORE_SECONDS: u64 = 10;

/// One projected party.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Party {
    /// The leader's character.
    pub leader: u64,
    /// Members, the leader first.
    pub members: Vec<u64>,
}

/// The parties of characters in this cell, as the social role last told
/// them. Simulation state (fed only by logged updates).
#[derive(Debug, Default)]
pub struct Parties {
    /// By party id.
    pub parties: BTreeMap<u32, Party>,
}

impl Parties {
    /// The party `character` is in.
    #[must_use]
    pub fn party_of(&self, character: u64) -> Option<u32> {
        self.parties
            .iter()
            .find(|(_, p)| p.members.contains(&character))
            .map(|(id, _)| *id)
    }

    fn view(&self, id: u32) -> Option<PartyView> {
        self.parties
            .get(&id)
            .map(|p| PartyView::new(id, p.leader, &p.members))
    }
}

impl StateHash for Parties {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.parties.len() as u64);
        for (id, p) in &self.parties {
            h.write_u32(*id);
            h.write_u64(p.leader);
            p.members[..].state_hash(h);
        }
    }
}

impl Resource for Parties {
    const NAME: &'static str = "std.party.parties";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.parties.len()).unwrap_or(u32::MAX));
        for (id, p) in &self.parties {
            e.u32(*id);
            e.u64(p.leader);
            e.u32(u32::try_from(p.members.len()).unwrap_or(u32::MAX));
            for m in &p.members {
                e.u64(*m);
            }
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.parties.clear();
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            let leader = d.u64()?;
            let mut members = Vec::new();
            for _ in 0..d.u32()? {
                members.push(d.u64()?);
            }
            self.parties.insert(id, Party { leader, members });
        }
        Ok(())
    }
}

/// Validators: every inbound party message is checked before it is handled.
struct Checks;

impl std_party_contract::Validators for Checks {
    fn validate_invite(&self, _msg: &Invite) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_accept(&self, msg: &Accept) -> Result<(), ValidationError> {
        if msg.from == 0 {
            return Err(ValidationError("no inviter"));
        }
        Ok(())
    }
    fn validate_leave(&self, _msg: &Leave) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_kick(&self, msg: &Kick) -> Result<(), ValidationError> {
        if msg.character == 0 {
            return Err(ValidationError("no member"));
        }
        Ok(())
    }
}

/// Queues an operation for the social role (an output: the answer comes
/// back as a logged update).
fn relay(world: &mut World, op: &PartyOp) {
    let _ = to_service_encoded(world, PARTY_OP, op);
}

fn invite(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: Invite = decode(payload)?;
    let from = caller(world, session)?;
    let target_session = session_of_entity(world, msg.target).ok_or(ExtensionRefusal::NotAllowed)?;
    let to = caller(world, target_session)?;
    // Read when the invite is made, so a live change applies from the tick
    // it lands on.
    if !mantis_server::modules::flag(world, "std.party.invites") || to == from {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(world, &PartyOp::Invite { from, to });
    Ok(())
}

fn accept(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: Accept = decode(payload)?;
    let me = caller(world, session)?;
    relay(world, &PartyOp::Accept { me, from: msg.from });
    Ok(())
}

fn leave(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let _: Leave = decode(payload)?;
    let me = caller(world, session)?;
    relay(world, &PartyOp::Leave { me });
    Ok(())
}

fn kick(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    let msg: Kick = decode(payload)?;
    let me = caller(world, session)?;
    if msg.character == me {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(
        world,
        &PartyOp::Kick {
            me,
            who: msg.character,
        },
    );
    Ok(())
}

fn changed(world: &mut World, party: u32, character: u64, joined: bool) {
    if let Some(q) = world.resource_mut::<Events<PartyChanged>>() {
        q.send(PartyChanged {
            party,
            character,
            joined,
        });
    }
}

/// A logged update from the social role: the projection follows it and the
/// character's client is told.
fn updated(world: &mut World, _: &TickContext, payload: &[u8]) -> Result<(), &'static str> {
    let u: PartyUpdate = decode_exact(payload).map_err(|_| "not a party update")?;
    let to = u.to();
    if session_of(world, to).is_none() {
        return Ok(());
    }
    match u {
        PartyUpdate::Roster {
            party,
            leader,
            members,
            ..
        } => {
            let parties = world.resource_mut::<Parties>().ok_or("parties")?;
            let joined = !parties
                .parties
                .get(&party)
                .is_some_and(|p| p.members.contains(&to));
            parties.parties.insert(
                party,
                Party {
                    leader,
                    members: members.clone(),
                },
            );
            if joined {
                changed(world, party, to, true);
            }
            let roster = Roster {
                party,
                leader,
                members: BoundedArray::from_slice(&members).unwrap_or_default(),
            };
            tell_character(world, to, &roster);
        }
        PartyUpdate::Invited { from, .. } => {
            tell_character(world, to, &Invited { from });
        }
        PartyUpdate::Left { party, .. } => {
            if let Some(p) = world
                .resource_mut::<Parties>()
                .and_then(|p| p.parties.get_mut(&party))
            {
                p.members.retain(|m| *m != to);
            }
            changed(world, party, to, false);
            tell_character(world, to, &Disbanded { party });
        }
        PartyUpdate::Refused { op, .. } => {
            tell_character(world, to, &PartyRefused { op });
        }
    }
    Ok(())
}

/// Drops projected parties with no member in this cell, and from time to
/// time offers the rest back to the social role (outputs only).
fn upkeep(world: &mut World, ctx: &TickContext) -> Result<(), SystemError> {
    let local: Vec<u32> = {
        let p = world
            .resource::<Parties>()
            .ok_or(SystemError::Invariant("parties"))?;
        p.parties
            .iter()
            .filter(|(_, party)| party.members.iter().any(|m| session_of(world, *m).is_some()))
            .map(|(id, _)| *id)
            .collect()
    };
    if let Some(p) = world.resource_mut::<Parties>() {
        p.parties.retain(|id, _| local.contains(id));
    }
    let every = RESTORE_SECONDS * u64::from(ctx.rate.hz());
    if ctx.tick.0.is_multiple_of(every.max(1)) {
        let restore: Vec<PartyOp> = world
            .resource::<Parties>()
            .map(|p| {
                p.parties
                    .iter()
                    .map(|(id, party)| PartyOp::Restore {
                        party: *id,
                        leader: party.leader,
                        members: party.members.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        for op in &restore {
            relay(world, op);
        }
    }
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.party"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Parties::default())?;
        r.event::<PartyChanged>(64)?;
        let validated: [(u16, mantis_server::modules::HandlerFn); 4] = [
            (Invite::ID.0, |w, c, s, p| {
                check(Invite::ID.0, p)?;
                invite(w, c, s, p)
            }),
            (Accept::ID.0, |w, c, s, p| {
                check(Accept::ID.0, p)?;
                accept(w, c, s, p)
            }),
            (Leave::ID.0, |w, c, s, p| {
                check(Leave::ID.0, p)?;
                leave(w, c, s, p)
            }),
            (Kick::ID.0, |w, c, s, p| {
                check(Kick::ID.0, p)?;
                kick(w, c, s, p)
            }),
        ];
        for (kind, run) in validated {
            r.handler(ExtensionKind(kind), Require::Joined, run)?;
        }
        r.service(PARTY_UPDATE, updated)?;
        r.query::<PartyOf>(|w, q| {
            let p = w.resource::<Parties>()?;
            p.party_of(q.0).and_then(|id| p.view(id))
        })?;
        let access = r
            .access()
            .write_resource::<Parties>()
            .write_resource::<ServiceOutbox>()
            .build()?;
        r.system(
            SystemDesc {
                name: "std.party.upkeep",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            upkeep,
        )?;
        Ok(())
    }
}

/// Decodes by kind and runs the generated validators: every inbound party
/// message is checked before it is handled.
fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_party_contract::parse_inbound(mantis_core::wire::MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}
