//! std.mail contract: letters, and the [`Mailboxes`] resource other economy
//! modules deliver into (an auction house returning items, for example)
//! from inside their own commands.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/mail.idl`.
    #[rustfmt::skip]
    pub mod mail;
}

pub use generated::mail::*;

use std::collections::BTreeMap;

use mantis_core::ecs::Resource;
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::wire::WireString;
use std_containers_contract::{EconomyError, Stack};

/// Letters one mailbox holds.
pub const MAX_LETTERS: usize = 50;

/// Gold burned per letter a character sends.
pub const POSTAGE: u64 = 5;

/// `MailResult.reason`: done.
pub const MAIL_OK: u8 = 0;
/// `MailResult.reason`: malformed request.
pub const MAIL_INVALID: u8 = 1;
/// `MailResult.reason`: no such letter in your mailbox.
pub const MAIL_NOT_FOUND: u8 = 2;
/// `MailResult.reason`: the recipient's mailbox is full.
pub const MAIL_RECIPIENT_FULL: u8 = 3;
/// `MailResult.reason`: not enough gold for the attachment and postage.
pub const MAIL_NO_GOLD: u8 = 4;
/// `MailResult.reason`: the bag slot does not hold that many.
pub const MAIL_NO_ITEM: u8 = 5;
/// `MailResult.reason`: your bag cannot take the attachment.
pub const MAIL_BAG_FULL: u8 = 6;
/// `MailResult.reason`: attachments are switched off.
pub const MAIL_ATTACHMENTS_OFF: u8 = 7;
/// `MailResult.reason`: the letter still has attachments (delete) or has
/// none (take).
pub const MAIL_ATTACHMENTS: u8 = 8;
/// `MailResult.reason`: a letter to yourself.
pub const MAIL_TO_SELF: u8 = 9;

/// Letters per `MailPage`.
pub const PAGE: usize = 6;

/// One letter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Letter {
    /// Its id, unique in the cell.
    pub id: u32,
    /// The sender (0 is the system).
    pub from: u64,
    /// Subject.
    pub subject: WireString<32>,
    /// Body.
    pub body: WireString<200>,
    /// Attached gold.
    pub gold: u64,
    /// Attached items.
    pub item: Option<Stack>,
    /// Opened by its recipient.
    pub read: bool,
}

/// Every mailbox in the cell. Simulation state.
#[derive(Debug, Default)]
pub struct Mailboxes {
    next: u32,
    boxes: BTreeMap<u64, Vec<Letter>>,
    /// Letters delivered and not yet announced: (recipient, letter id).
    arrived: Vec<(u64, u32)>,
}

impl Mailboxes {
    /// `character`'s letters, oldest first.
    #[must_use]
    pub fn letters(&self, character: u64) -> &[Letter] {
        self.boxes.get(&character).map_or(&[], Vec::as_slice)
    }

    /// True when `character`'s mailbox can take another letter.
    #[must_use]
    pub fn has_room(&self, character: u64) -> bool {
        self.letters(character).len() < MAX_LETTERS
    }

    /// Delivers a letter. Call [`Mailboxes::has_room`] before taking any
    /// attachment from a bag, so delivery never fails after the debit.
    ///
    /// # Errors
    /// [`EconomyError::Full`] when the mailbox is full.
    pub fn deliver(
        &mut self,
        to: u64,
        from: u64,
        subject: WireString<32>,
        body: WireString<200>,
        gold: u64,
        item: Option<Stack>,
    ) -> Result<u32, EconomyError> {
        if !self.has_room(to) {
            return Err(EconomyError::Full);
        }
        self.next += 1;
        let id = self.next;
        self.boxes.entry(to).or_default().push(Letter {
            id,
            from,
            subject,
            body,
            gold,
            item,
            read: false,
        });
        self.arrived.push((to, id));
        Ok(id)
    }

    /// One of `character`'s letters, mutably.
    pub fn letter_mut(&mut self, character: u64, id: u32) -> Option<&mut Letter> {
        self.boxes.get_mut(&character)?.iter_mut().find(|l| l.id == id)
    }

    /// Removes one of `character`'s letters.
    pub fn remove(&mut self, character: u64, id: u32) -> Option<Letter> {
        let letters = self.boxes.get_mut(&character)?;
        let at = letters.iter().position(|l| l.id == id)?;
        Some(letters.remove(at))
    }

    /// Takes the list of letters delivered since the last call.
    pub fn take_arrivals(&mut self) -> Vec<(u64, u32)> {
        std::mem::take(&mut self.arrived)
    }
}

impl StateHash for Mailboxes {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.next);
        h.write_u64(self.boxes.len() as u64);
        for (c, letters) in &self.boxes {
            h.write_u64(*c);
            h.write_u64(letters.len() as u64);
            for l in letters {
                h.write_u32(l.id);
                h.write_u64(l.from);
                h.write(l.subject.as_str().as_bytes());
                h.write_u8(0);
                h.write(l.body.as_str().as_bytes());
                h.write_u8(0);
                h.write_u64(l.gold);
                h.write_u32(l.item.map_or(0, |s| s.item));
                h.write_u32(l.item.map_or(0, |s| s.count));
                h.write_u8(u8::from(l.read));
            }
        }
        h.write_u64(self.arrived.len() as u64);
        for (to, id) in &self.arrived {
            h.write_u64(*to);
            h.write_u32(*id);
        }
    }
}

impl Resource for Mailboxes {
    const NAME: &'static str = "std.mail.mailboxes";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(self.next);
        e.u32(u32::try_from(self.boxes.len()).unwrap_or(u32::MAX));
        for (c, letters) in &self.boxes {
            e.u64(*c);
            e.u32(u32::try_from(letters.len()).unwrap_or(u32::MAX));
            for l in letters {
                e.u32(l.id);
                e.u64(l.from);
                mantis_core::wire::Wire::encode(&l.subject, e);
                mantis_core::wire::Wire::encode(&l.body, e);
                e.u64(l.gold);
                e.u32(l.item.map_or(0, |s| s.item));
                e.u32(l.item.map_or(0, |s| s.count));
                e.bool(l.read);
            }
        }
        e.u32(u32::try_from(self.arrived.len()).unwrap_or(u32::MAX));
        for (to, id) in &self.arrived {
            e.u64(*to);
            e.u32(*id);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        use mantis_core::wire::Wire as _;
        self.next = d.u32()?;
        self.boxes.clear();
        for _ in 0..d.u32()? {
            let c = d.u64()?;
            let mut letters = Vec::new();
            for _ in 0..d.u32()? {
                let id = d.u32()?;
                let from = d.u64()?;
                let subject = WireString::decode(d)?;
                let body = WireString::decode(d)?;
                let gold = d.u64()?;
                let item = d.u32()?;
                let count = d.u32()?;
                letters.push(Letter {
                    id,
                    from,
                    subject,
                    body,
                    gold,
                    item: (count > 0).then_some(Stack { item, count }),
                    read: d.bool()?,
                });
            }
            self.boxes.insert(c, letters);
        }
        self.arrived.clear();
        for _ in 0..d.u32()? {
            let to = d.u64()?;
            self.arrived.push((to, d.u32()?));
        }
        Ok(())
    }
}
