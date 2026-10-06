//! std.containers, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use mantis_core::ecs::World;
use mantis_core::log::SessionId;
use mantis_core::schedule::TickContext;
use mantis_core::wire::{Message, MessageId, ValidationError};
use mantis_server::modules::{
    CommandFn, ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, Registrar, RegistryError,
    Require, ServerModule, caller, character_of, decode, tell, tell_character,
};
use std_containers_contract::{
    DestroyItem, EconomyError, Grant, Inventories, Ledger, MAX_STACK, MoveItem, SLOTS, ShowBag, SplitStack,
    Stack,
};

struct Checks;

fn slot(s: u8) -> Result<(), ValidationError> {
    if usize::from(s) < SLOTS {
        Ok(())
    } else {
        Err(ValidationError("no such slot"))
    }
}

impl std_containers_contract::Validators for Checks {
    fn validate_move_item(&self, msg: &MoveItem) -> Result<(), ValidationError> {
        slot(msg.from)?;
        slot(msg.to)?;
        if msg.from == msg.to {
            return Err(ValidationError("same slot"));
        }
        Ok(())
    }
    fn validate_split_stack(&self, msg: &SplitStack) -> Result<(), ValidationError> {
        self.validate_move_item(&MoveItem {
            from: msg.from,
            to: msg.to,
        })?;
        if msg.count == 0 {
            return Err(ValidationError("nothing to split"));
        }
        Ok(())
    }
    fn validate_destroy_item(&self, msg: &DestroyItem) -> Result<(), ValidationError> {
        slot(msg.slot)?;
        if msg.count == 0 {
            return Err(ValidationError("nothing to destroy"));
        }
        Ok(())
    }
    fn validate_grant(&self, msg: &Grant) -> Result<(), ValidationError> {
        if msg.character == 0 || (msg.item == 0) != (msg.count == 0) {
            return Err(ValidationError("malformed grant"));
        }
        Ok(())
    }
    fn validate_show_bag(&self, _msg: &ShowBag) -> Result<(), ValidationError> {
        Ok(())
    }
}

fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_containers_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

/// The refusal a client sees for an economy error.
#[must_use]
pub fn refusal(e: EconomyError) -> ExtensionRefusal {
    match e {
        EconomyError::Overflow => ExtensionRefusal::Invalid,
        EconomyError::Insufficient | EconomyError::Full | EconomyError::BadSlot => {
            ExtensionRefusal::NotAllowed
        }
    }
}

/// Builds an outcome from a transaction's result, with the ledger as payload.
#[must_use]
pub fn outcome(command: &ModuleCommand, result: Result<Ledger, ExtensionRefusal>) -> ModuleOutcome {
    match result {
        Ok(ledger) => ModuleOutcome::done(command, &ledger),
        Err(reason) => ModuleOutcome::refused(command, reason),
    }
}

/// Sends `character` its bag, if it is in the cell.
pub fn send_bag(world: &mut World, character: u64) {
    let msg = world.resource::<Inventories>().map(|i| i.bag_message(character));
    if let Some(msg) = msg {
        tell_character(world, character, &msg);
    }
}

/// The acting character of a client command.
fn actor(world: &World, command: &ModuleCommand) -> Result<u64, ExtensionRefusal> {
    command
        .session
        .and_then(|s| character_of(world, s))
        .ok_or(ExtensionRefusal::NotAllowed)
}

fn run(
    world: &mut World,
    command: &ModuleCommand,
    characters: &[u64],
    f: impl FnOnce(&mut std_containers_contract::Tx<'_>) -> Result<(), EconomyError>,
) -> Result<Ledger, ExtensionRefusal> {
    let inv = world
        .resource_mut::<Inventories>()
        .ok_or(ExtensionRefusal::Invalid)?;
    let ledger = inv.transaction(characters, f).map_err(refusal)?;
    for c in characters {
        send_bag(world, *c);
    }
    let _ = command;
    Ok(ledger)
}

fn move_item(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let result = (|| {
        check(MoveItem::ID.0, cmd.payload.as_slice())?;
        let msg: MoveItem = decode(cmd.payload.as_slice())?;
        let me = actor(world, cmd)?;
        run(world, cmd, &[me], |tx| {
            tx.rearrange(me, |slots| {
                let (src, dst) = (usize::from(msg.from), usize::from(msg.to));
                let (from, to) = (
                    slots.get(src).copied().flatten(),
                    slots.get(dst).copied().flatten(),
                );
                match (from, to) {
                    (None, _) => return Err(EconomyError::BadSlot),
                    (Some(here), Some(there)) if here.item == there.item => {
                        let n = (MAX_STACK - there.count).min(here.count);
                        set(
                            slots,
                            dst,
                            Some(Stack {
                                item: there.item,
                                count: there.count + n,
                            }),
                        )?;
                        set(
                            slots,
                            src,
                            (here.count > n).then_some(Stack {
                                item: here.item,
                                count: here.count - n,
                            }),
                        )?;
                    }
                    (Some(here), there) => {
                        set(slots, dst, Some(here))?;
                        set(slots, src, there)?;
                    }
                }
                Ok(())
            })
        })
    })();
    outcome(cmd, result)
}

fn set(slots: &mut [Option<Stack>; SLOTS], i: usize, v: Option<Stack>) -> Result<(), EconomyError> {
    *slots.get_mut(i).ok_or(EconomyError::BadSlot)? = v;
    Ok(())
}

fn split(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let result = (|| {
        check(SplitStack::ID.0, cmd.payload.as_slice())?;
        let msg: SplitStack = decode(cmd.payload.as_slice())?;
        let me = actor(world, cmd)?;
        run(world, cmd, &[me], |tx| {
            tx.rearrange(me, |slots| {
                let (src, dst) = (usize::from(msg.from), usize::from(msg.to));
                let stack = slots.get(src).copied().flatten().ok_or(EconomyError::BadSlot)?;
                if slots.get(dst).copied().flatten().is_some() || msg.count >= stack.count {
                    return Err(EconomyError::BadSlot);
                }
                set(
                    slots,
                    src,
                    Some(Stack {
                        item: stack.item,
                        count: stack.count - msg.count,
                    }),
                )?;
                set(
                    slots,
                    dst,
                    Some(Stack {
                        item: stack.item,
                        count: msg.count,
                    }),
                )
            })
        })
    })();
    outcome(cmd, result)
}

fn destroy(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let result = (|| {
        check(DestroyItem::ID.0, cmd.payload.as_slice())?;
        let msg: DestroyItem = decode(cmd.payload.as_slice())?;
        let me = actor(world, cmd)?;
        run(world, cmd, &[me], |tx| {
            tx.take(me, msg.slot, msg.count).map(|_| ())
        })
    })();
    outcome(cmd, result)
}

fn grant(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let result = (|| {
        // Services only: a client cannot grant itself anything.
        if cmd.session.is_some() {
            return Err(ExtensionRefusal::NotAllowed);
        }
        check(Grant::ID.0, cmd.payload.as_slice())?;
        let msg: Grant = decode(cmd.payload.as_slice())?;
        run(world, cmd, &[msg.character], |tx| {
            tx.credit(msg.character, msg.gold)?;
            if msg.item != 0 {
                tx.add(msg.character, msg.item, msg.count)?;
            }
            Ok(())
        })
    })();
    outcome(cmd, result)
}

fn show(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(ShowBag::ID.0, payload)?;
    let me = caller(world, session)?;
    let msg = world
        .resource::<Inventories>()
        .ok_or(ExtensionRefusal::Invalid)?
        .bag_message(me);
    tell(world, session, &msg);
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.containers"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Inventories::default())?;
        let commands: [(u16, CommandFn); 4] = [
            (MoveItem::ID.0, move_item),
            (SplitStack::ID.0, split),
            (DestroyItem::ID.0, destroy),
            (Grant::ID.0, grant),
        ];
        for (kind, f) in commands {
            r.command(ExtensionKind(kind), f)?;
        }
        r.handler(ExtensionKind(ShowBag::ID.0), Require::Joined, show)?;
        Ok(())
    }
}
