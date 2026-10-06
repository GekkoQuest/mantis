//! std.mail, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use mantis_core::ecs::World;
use mantis_core::log::SessionId;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::wire::{BoundedArray, Message, MessageId, ValidationError};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, Registrar, RegistryError, Require,
    ServerModule, caller, character_of, decode, tell, tell_character,
};
use std_containers_contract::{EconomyError, Inventories, Ledger};
use std_mail_contract::{
    DeleteLetter, ListMailPage, MAIL_ATTACHMENTS, MAIL_ATTACHMENTS_OFF, MAIL_BAG_FULL, MAIL_INVALID,
    MAIL_NO_GOLD, MAIL_NO_ITEM, MAIL_NOT_FOUND, MAIL_OK, MAIL_RECIPIENT_FULL, MAIL_TO_SELF, MailEntry,
    MailPage, MailResult, MailText, Mailboxes, NewMail, PAGE, POSTAGE, ReadMail, SendMail, TakeMail,
};

struct Checks;

impl std_mail_contract::Validators for Checks {
    fn validate_read_mail(&self, _msg: &ReadMail) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_list_mail_page(&self, _msg: &ListMailPage) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_send_mail(&self, msg: &SendMail) -> Result<(), ValidationError> {
        if msg.to == 0 {
            return Err(ValidationError("no recipient"));
        }
        if msg.subject.as_str().trim().is_empty() {
            return Err(ValidationError("empty subject"));
        }
        if msg.has_item != (msg.count > 0) {
            return Err(ValidationError("an attached item has a count"));
        }
        Ok(())
    }
    fn validate_take_mail(&self, _msg: &TakeMail) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_delete_letter(&self, _msg: &DeleteLetter) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Why a mail command failed: the refusal for the client, and the
/// `MailResult` reason.
type Failed = (ExtensionRefusal, u8);

const INVALID: Failed = (ExtensionRefusal::Invalid, MAIL_INVALID);

/// `insufficient`: the reason when the gold or items were not there.
fn economy(e: EconomyError, insufficient: u8) -> Failed {
    match e {
        EconomyError::Overflow => INVALID,
        EconomyError::Full => (ExtensionRefusal::NotAllowed, MAIL_BAG_FULL),
        EconomyError::BadSlot => (ExtensionRefusal::NotAllowed, MAIL_NO_ITEM),
        EconomyError::Insufficient => (ExtensionRefusal::NotAllowed, insufficient),
    }
}

/// A letter to send, from either request message.
struct Outgoing {
    to: u64,
    subject: mantis_core::wire::WireString<32>,
    body: mantis_core::wire::WireString<200>,
    gold: u64,
    item: Option<(u8, u32)>,
}

fn send_letter(world: &mut World, me: u64, msg: &Outgoing) -> Result<(Ledger, u32), Failed> {
    let attaching = msg.gold > 0 || msg.item.is_some();
    if attaching && !mantis_server::modules::flag(world, ATTACHMENTS) {
        return Err((ExtensionRefusal::NotAllowed, MAIL_ATTACHMENTS_OFF));
    }
    if msg.to == me {
        return Err((ExtensionRefusal::NotAllowed, MAIL_TO_SELF));
    }
    if !world.resource::<Mailboxes>().is_some_and(|m| m.has_room(msg.to)) {
        return Err((ExtensionRefusal::NotAllowed, MAIL_RECIPIENT_FULL));
    }
    let cost = msg.gold.checked_add(POSTAGE).ok_or(INVALID)?;
    let mut item = None;
    let inv = world.resource_mut::<Inventories>().ok_or(INVALID)?;
    let short = if inv.gold(me) < cost {
        MAIL_NO_GOLD
    } else {
        MAIL_NO_ITEM
    };
    let ledger = inv
        .transaction(&[me], |tx| {
            tx.debit(me, cost)?;
            if let Some((slot, count)) = msg.item {
                item = Some(tx.take(me, slot, count)?);
            }
            Ok(())
        })
        .map_err(|e| economy(e, short))?;
    let id = world
        .resource_mut::<Mailboxes>()
        .ok_or(INVALID)?
        .deliver(msg.to, me, msg.subject, msg.body, msg.gold, item)
        .map_err(|e| economy(e, MAIL_RECIPIENT_FULL))?;
    Ok((ledger, id))
}

fn take_letter(world: &mut World, me: u64, mail: u32) -> Result<(Ledger, u32), Failed> {
    let letter = world
        .resource_mut::<Mailboxes>()
        .and_then(|m| m.letter_mut(me, mail).copied())
        .ok_or((ExtensionRefusal::NotAllowed, MAIL_NOT_FOUND))?;
    if letter.gold == 0 && letter.item.is_none() {
        return Err((ExtensionRefusal::NotAllowed, MAIL_ATTACHMENTS));
    }
    let inv = world.resource_mut::<Inventories>().ok_or(INVALID)?;
    let ledger = inv
        .transaction(&[me], |tx| {
            tx.credit(me, letter.gold)?;
            if let Some(s) = letter.item {
                tx.add(me, s.item, s.count)?;
            }
            Ok(())
        })
        .map_err(|e| economy(e, MAIL_BAG_FULL))?;
    if let Some(l) = world
        .resource_mut::<Mailboxes>()
        .and_then(|m| m.letter_mut(me, mail))
    {
        l.gold = 0;
        l.item = None;
    }
    Ok((ledger, mail))
}

fn delete_letter(world: &mut World, me: u64, mail: u32) -> Result<(Ledger, u32), Failed> {
    let boxes = world.resource_mut::<Mailboxes>().ok_or(INVALID)?;
    let letter = boxes
        .letter_mut(me, mail)
        .copied()
        .ok_or((ExtensionRefusal::NotAllowed, MAIL_NOT_FOUND))?;
    if letter.gold > 0 || letter.item.is_some() {
        return Err((ExtensionRefusal::NotAllowed, MAIL_ATTACHMENTS));
    }
    boxes.remove(me, mail);
    Ok((Ledger::default(), mail))
}

/// One page of `me`'s mailbox, newest first.
fn page(world: &World, me: u64, offset: u16) -> MailPage {
    let letters = world.resource::<Mailboxes>().map_or(&[][..], |m| m.letters(me));
    let entries: Vec<MailEntry> = letters
        .iter()
        .rev()
        .skip(usize::from(offset))
        .take(PAGE)
        .map(|l| MailEntry {
            mail: l.id,
            from: l.from,
            subject: l.subject,
            read: l.read,
            gold: l.gold,
            item: l.item.map_or(0, |s| s.item),
            count: l.item.map_or(0, |s| s.count),
        })
        .collect();
    MailPage {
        total: u16::try_from(letters.len()).unwrap_or(u16::MAX),
        offset,
        entries: BoundedArray::from_slice(&entries).unwrap_or_default(),
    }
}

/// Runs a requested command: the result and a fresh first page go to the
/// requester, the outcome (with its ledger) to the log.
fn answered(
    world: &mut World,
    cmd: &ModuleCommand,
    run: impl FnOnce(&mut World, u64, &[u8]) -> Result<(u32, Result<(Ledger, u32), Failed>), Failed>,
) -> ModuleOutcome {
    let Some(me) = cmd.session.and_then(|s| character_of(world, s)) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::NotAllowed);
    };
    let (request, result) = match run(world, me, cmd.payload.as_slice()) {
        Ok(r) => r,
        Err((refusal, _)) => return ModuleOutcome::refused(cmd, refusal),
    };
    let (reply, outcome) = match result {
        Ok((ledger, mail)) => (
            MailResult {
                request,
                ok: true,
                reason: MAIL_OK,
                mail,
            },
            finish(world, cmd, me, &Ok(ledger)),
        ),
        Err((refusal, reason)) => (
            MailResult {
                request,
                ok: false,
                reason,
                mail: 0,
            },
            ModuleOutcome::refused(cmd, refusal),
        ),
    };
    tell_character(world, me, &reply);
    let first = page(world, me, 0);
    tell_character(world, me, &first);
    outcome
}

fn send_mail(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, cmd, |world, me, payload| {
        check(SendMail::ID.0, payload).map_err(|_| INVALID)?;
        let msg: SendMail = decode(payload).map_err(|_| INVALID)?;
        let out = Outgoing {
            to: msg.to,
            subject: msg.subject,
            body: msg.body,
            gold: msg.gold,
            item: msg.has_item.then_some((msg.slot, msg.count)),
        };
        Ok((msg.request, send_letter(world, me, &out)))
    })
}

fn take_mail(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, cmd, |world, me, payload| {
        check(TakeMail::ID.0, payload).map_err(|_| INVALID)?;
        let msg: TakeMail = decode(payload).map_err(|_| INVALID)?;
        Ok((msg.request, take_letter(world, me, msg.mail)))
    })
}

fn delete_mail(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, cmd, |world, me, payload| {
        check(DeleteLetter::ID.0, payload).map_err(|_| INVALID)?;
        let msg: DeleteLetter = decode(payload).map_err(|_| INVALID)?;
        Ok((msg.request, delete_letter(world, me, msg.mail)))
    })
}

fn list_page(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(ListMailPage::ID.0, payload)?;
    let msg: ListMailPage = decode(payload)?;
    let me = caller(world, session)?;
    let reply = page(world, me, msg.offset);
    tell(world, session, &reply);
    Ok(())
}

fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_mail_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

/// The attachments flag, read when mail is sent so a live change applies
/// from the tick it lands on.
const ATTACHMENTS: &str = "std.mail.attachments";

fn finish(
    world: &mut World,
    cmd: &ModuleCommand,
    me: u64,
    result: &Result<Ledger, ExtensionRefusal>,
) -> ModuleOutcome {
    match result {
        Ok(ledger) => {
            let bag = world.resource::<Inventories>().map(|i| i.bag_message(me));
            if let Some(bag) = bag {
                tell_character(world, me, &bag);
            }
            ModuleOutcome::done(cmd, ledger)
        }
        Err(r) => ModuleOutcome::refused(cmd, *r),
    }
}

fn read(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(ReadMail::ID.0, payload)?;
    let msg: ReadMail = decode(payload)?;
    let me = caller(world, session)?;
    let l = world
        .resource_mut::<Mailboxes>()
        .and_then(|m| m.letter_mut(me, msg.mail))
        .map(|l| {
            l.read = true;
            *l
        })
        .ok_or(ExtensionRefusal::NotAllowed)?;
    let reply = MailText {
        mail: l.id,
        from: l.from,
        subject: l.subject,
        body: l.body,
        gold: l.gold,
        item: l.item.map_or(0, |s| s.item),
        count: l.item.map_or(0, |s| s.count),
    };
    tell(world, session, &reply);
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.mail"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Mailboxes::default())?;
        r.handler(ExtensionKind(ReadMail::ID.0), Require::Joined, read)?;
        r.command(ExtensionKind(SendMail::ID.0), send_mail)?;
        r.command(ExtensionKind(TakeMail::ID.0), take_mail)?;
        r.command(ExtensionKind(DeleteLetter::ID.0), delete_mail)?;
        r.handler(ExtensionKind(ListMailPage::ID.0), Require::Joined, list_page)?;
        let access = r.access().write_resource::<Mailboxes>().build()?;
        r.system(
            SystemDesc {
                name: "std.mail.announce",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
                let arrived = w
                    .resource_mut::<Mailboxes>()
                    .map(Mailboxes::take_arrivals)
                    .unwrap_or_default();
                for (to, id) in arrived {
                    let from = w
                        .resource::<Mailboxes>()
                        .and_then(|m| m.letters(to).iter().find(|l| l.id == id))
                        .map_or(0, |l| l.from);
                    tell_character(w, to, &NewMail { mail: id, from });
                }
                Ok(())
            },
        )?;
        Ok(())
    }
}
