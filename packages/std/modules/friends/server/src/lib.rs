//! std.friends, server half. See `RULES.md` beside the manifest.
//!
//! The social role owns every friend list and request (lead ruling, M8):
//! this module checks what a cell can check, relays the operation through
//! the service outbox, and keeps a read-only projection of the lists of
//! characters in this cell, fed by logged service updates.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::Events;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::social::{FRIEND_OP, FRIEND_UPDATE, FriendOp, FriendUpdate};
use mantis_core::wire::{BoundedArray, Message, MessageId, ValidationError, decode_exact};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, HandlerFn, Registrar, RegistryError, Require, ServerModule, caller,
    decode, session_of, tell_character,
};
use mantis_server::service::{ServiceOutbox, to_service_encoded};
use std_friends_contract::{
    AreFriends, Declined, FriendList, FriendRefused, FriendshipChanged, Pending, Remove, Request, Requested,
    Respond, ShowFriends,
};

/// The friend lists of characters in this cell, as the social role last
/// told them. Simulation state (fed only by logged updates).
#[derive(Debug, Default)]
pub struct Friends {
    /// Character -> its friends.
    pub lists: BTreeMap<u64, BTreeSet<u64>>,
}

impl Friends {
    fn are_friends(&self, a: u64, b: u64) -> bool {
        self.lists.get(&a).is_some_and(|l| l.contains(&b))
            || self.lists.get(&b).is_some_and(|l| l.contains(&a))
    }
}

impl StateHash for Friends {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.lists.len() as u64);
        for (k, set) in &self.lists {
            h.write_u64(*k);
            h.write_u64(set.len() as u64);
            for v in set {
                h.write_u64(*v);
            }
        }
    }
}

impl Resource for Friends {
    const NAME: &'static str = "std.friends.lists";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.lists.len()).unwrap_or(u32::MAX));
        for (c, l) in &self.lists {
            e.u64(*c);
            e.u32(u32::try_from(l.len()).unwrap_or(u32::MAX));
            for f in l {
                e.u64(*f);
            }
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.lists.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            let mut l = BTreeSet::new();
            for _ in 0..d.u32()? {
                l.insert(d.u64()?);
            }
            self.lists.insert(c, l);
        }
        Ok(())
    }
}

struct Checks;

impl std_friends_contract::Validators for Checks {
    fn validate_request(&self, msg: &Request) -> Result<(), ValidationError> {
        nonzero(msg.character)
    }
    fn validate_respond(&self, msg: &Respond) -> Result<(), ValidationError> {
        nonzero(msg.character)
    }
    fn validate_remove(&self, msg: &Remove) -> Result<(), ValidationError> {
        nonzero(msg.character)
    }
    fn validate_show_friends(&self, _msg: &ShowFriends) -> Result<(), ValidationError> {
        Ok(())
    }
}

fn nonzero(c: u64) -> Result<(), ValidationError> {
    if c == 0 {
        Err(ValidationError("no character"))
    } else {
        Ok(())
    }
}

fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_friends_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

/// Queues an operation for the social role (an output: the answer comes
/// back as a logged update).
fn relay(world: &mut World, op: &FriendOp) {
    let _ = to_service_encoded(world, FRIEND_OP, op);
}

fn request(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(Request::ID.0, payload)?;
    let msg: Request = decode(payload)?;
    let me = caller(world, session)?;
    if msg.character == me {
        return Err(ExtensionRefusal::NotAllowed);
    }
    relay(
        world,
        &FriendOp::Request {
            me,
            other: msg.character,
        },
    );
    Ok(())
}

fn respond(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(Respond::ID.0, payload)?;
    let msg: Respond = decode(payload)?;
    let me = caller(world, session)?;
    relay(
        world,
        &FriendOp::Respond {
            me,
            other: msg.character,
            accept: msg.accept,
        },
    );
    Ok(())
}

fn remove(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(Remove::ID.0, payload)?;
    let msg: Remove = decode(payload)?;
    let me = caller(world, session)?;
    relay(
        world,
        &FriendOp::Remove {
            me,
            other: msg.character,
        },
    );
    Ok(())
}

fn show(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(ShowFriends::ID.0, payload)?;
    let me = caller(world, session)?;
    relay(world, &FriendOp::Show { me });
    Ok(())
}

/// A logged update from the social role: the projection follows it and the
/// character's client is told (with a presence bit per friend in this cell).
fn updated(world: &mut World, _: &TickContext, payload: &[u8]) -> Result<(), &'static str> {
    let u: FriendUpdate = decode_exact(payload).map_err(|_| "not a friends update")?;
    let to = u.to();
    if session_of(world, to).is_none() {
        return Ok(());
    }
    match u {
        FriendUpdate::List { friends, .. } => {
            let now: BTreeSet<u64> = friends.iter().copied().collect();
            let lists = &mut world.resource_mut::<Friends>().ok_or("friends")?.lists;
            let before = lists.insert(to, now.clone()).unwrap_or_default();
            let changes: Vec<(u64, bool)> = now
                .difference(&before)
                .map(|c| (*c, true))
                .chain(before.difference(&now).map(|c| (*c, false)))
                .collect();
            if let Some(q) = world.resource_mut::<Events<FriendshipChanged>>() {
                for (b, friends) in changes {
                    q.send(FriendshipChanged { a: to, b, friends });
                }
            }
            let present = friends
                .iter()
                .enumerate()
                .filter(|(_, c)| session_of(world, **c).is_some())
                .fold(0u64, |bits, (i, _)| bits | (1u64 << i));
            let list = FriendList {
                friends: BoundedArray::from_slice(&friends).unwrap_or_default(),
                present,
            };
            tell_character(world, to, &list);
        }
        FriendUpdate::Pending {
            incoming, outgoing, ..
        } => {
            let msg = Pending {
                incoming: BoundedArray::from_slice(&incoming).unwrap_or_default(),
                outgoing: BoundedArray::from_slice(&outgoing).unwrap_or_default(),
            };
            tell_character(world, to, &msg);
        }
        FriendUpdate::Requested { from, .. } => {
            tell_character(world, to, &Requested { from });
        }
        FriendUpdate::Declined { by, .. } => {
            tell_character(world, to, &Declined { by });
        }
        FriendUpdate::Refused { op, .. } => {
            tell_character(world, to, &FriendRefused { op });
        }
    }
    Ok(())
}

/// Drops projected lists of characters no longer in this cell. (Lists need
/// no restoring: the social role keeps them durable through the writer.)
fn upkeep(world: &mut World, _: &TickContext) -> Result<(), SystemError> {
    let gone: Vec<u64> = world
        .resource::<Friends>()
        .ok_or(SystemError::Invariant("friends"))?
        .lists
        .keys()
        .copied()
        .filter(|c| session_of(world, *c).is_none())
        .collect();
    if let Some(f) = world.resource_mut::<Friends>() {
        for c in gone {
            f.lists.remove(&c);
        }
    }
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.friends"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Friends::default())?;
        r.event::<FriendshipChanged>(64)?;
        let handlers: [(u16, HandlerFn); 4] = [
            (ShowFriends::ID.0, show),
            (Request::ID.0, request),
            (Respond::ID.0, respond),
            (Remove::ID.0, remove),
        ];
        for (kind, run) in handlers {
            r.handler(ExtensionKind(kind), Require::Joined, run)?;
        }
        r.service(FRIEND_UPDATE, updated)?;
        r.query::<AreFriends>(|w, q| w.resource::<Friends>().is_some_and(|f| f.are_friends(q.0, q.1)))?;
        let access = r
            .access()
            .write_resource::<Friends>()
            .write_resource::<ServiceOutbox>()
            .build()?;
        r.system(
            SystemDesc {
                name: "std.friends.upkeep",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            upkeep,
        )?;
        Ok(())
    }
}
