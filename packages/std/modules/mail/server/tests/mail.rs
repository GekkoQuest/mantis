//! std.mail end to end: sending with gold and items, postage, taking
//! attachments, deleting, notifications, refusals that change nothing, the
//! `attachments` flag, and replay. Bags come from a stand-in implementing
//! the `std.containers` contract.

#![expect(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_core::wire::{Message, WireString, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, Registrar, RegistryError, ServerModule};
use std_containers_contract::Inventories;
use std_mail_contract::{
    DeleteLetter, ListMailPage, MAIL_ATTACHMENTS, MAIL_ATTACHMENTS_OFF, MailPage, MailText, Mailboxes,
    NewMail, POSTAGE, ReadMail, SendMail, TakeMail,
};

const MAIL: &str = include_str!("../../manifest.toml");
const BAGS: &str = "[module]
key = \"test.bags\"
version = \"0.1.0\"
contract = \"std.containers\"
";

/// Bags: 11 has 100 gold and 10 of item 7.
struct Bags;

impl ServerModule for Bags {
    fn key(&self) -> &'static str {
        "test.bags"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let mut inv = Inventories::default();
        inv.transaction(&[11], |tx| {
            tx.credit(11, 100)?;
            tx.add(11, 7, 10)
        })
        .map_err(|_| RegistryError::Module("seed"))?;
        r.resource(inv)
    }
}

fn bed(flags: &[(&str, bool)]) -> Harness {
    let mut h = Harness::new(
        &[Arc::new(std_mail_server::Module), Arc::new(Bags)],
        &[MAIL, BAGS],
        flags,
        &[],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.tick().unwrap();
    h.take(1);
    h.take(2);
    h
}

fn letter(to: u64, gold: u64, count: u32) -> Vec<u8> {
    bytes_of(&SendMail {
        request: 0,
        to,
        subject: WireString::new("supplies").unwrap(),
        body: WireString::new("as promised").unwrap(),
        gold,
        has_item: count > 0,
        slot: 0,
        count,
    })
}

fn inv(h: &Harness) -> &Inventories {
    h.world().resource::<Inventories>().unwrap()
}

#[test]
fn a_letter_with_attachments_travels_and_is_collected() {
    let mut h = bed(&[]);
    h.send(1, SendMail::ID.0, &letter(12, 40, 3));
    h.ticks(2).unwrap();
    assert_eq!(inv(&h).gold(11), 100 - 40 - POSTAGE);
    assert_eq!(inv(&h).count_of(11, 7), 7);
    let news = h.take(2);
    let new = news
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(NewMail::ID.0))
        .unwrap();
    let id = decode_message::<NewMail>(&new.1).unwrap().mail;
    h.send(2, ListMailPage::ID.0, &bytes_of(&ListMailPage { offset: 0 }));
    h.send(2, ReadMail::ID.0, &bytes_of(&ReadMail { mail: id }));
    h.tick().unwrap();
    let got = h.take(2);
    let page = decode_message::<MailPage>(&got.messages[0].1).unwrap();
    assert_eq!(page.entries.iter().map(|e| e.gold).collect::<Vec<_>>(), [40]);
    let text = decode_message::<MailText>(&got.messages[1].1).unwrap();
    assert_eq!(
        (text.from, text.item, text.count, text.body.as_str()),
        (11, 7, 3, "as promised")
    );
    // Delete is refused while something is attached; take, then delete.
    h.send(
        2,
        DeleteLetter::ID.0,
        &bytes_of(&DeleteLetter { request: 1, mail: id }),
    );
    h.send(2, TakeMail::ID.0, &bytes_of(&TakeMail { request: 2, mail: id }));
    h.send(
        2,
        DeleteLetter::ID.0,
        &bytes_of(&DeleteLetter { request: 3, mail: id }),
    );
    h.tick().unwrap();
    assert_eq!((inv(&h).gold(12), inv(&h).count_of(12, 7)), (40, 3));
    assert!(h.world().resource::<Mailboxes>().unwrap().letters(12).is_empty());
    let (r, _) = results(&mut h, 2);
    let codes: Vec<(u32, bool, u8)> = r.iter().map(|x| (x.request, x.ok, x.reason)).collect();
    assert_eq!(codes, [(1, false, MAIL_ATTACHMENTS), (2, true, 0), (3, true, 0)]);
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn refused_letters_cost_nothing() {
    let mut h = bed(&[]);
    h.send(1, SendMail::ID.0, &letter(12, 200, 0)); // too much gold
    h.send(1, SendMail::ID.0, &letter(12, 0, 50)); // too many items
    h.send(1, SendMail::ID.0, &letter(11, 0, 0)); // to self
    h.send(2, SendMail::ID.0, &letter(11, 0, 0)); // cannot pay postage
    h.tick().unwrap();
    assert_eq!((inv(&h).gold(11), inv(&h).count_of(11, 7)), (100, 10));
    assert_eq!(h.take(1).refusals.len(), 3);
    assert_eq!(h.take(2).refusals.len(), 1);
    assert!(h.world().resource::<Mailboxes>().unwrap().letters(12).is_empty());
}

#[test]
fn attachments_can_be_switched_off() {
    let mut h = bed(&[("std.mail.attachments", false)]);
    h.send(1, SendMail::ID.0, &letter(12, 1, 0));
    h.send(1, SendMail::ID.0, &letter(12, 0, 0));
    h.ticks(2).unwrap();
    let got = h.take(1);
    assert_eq!(
        got.refusals,
        [(ExtensionKind(SendMail::ID.0), ExtensionRefusal::NotAllowed)]
    );
    assert_eq!(
        h.world().resource::<Mailboxes>().unwrap().letters(12).len(),
        1,
        "plain letters still go"
    );
    let reasons: Vec<u8> = got
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(std_mail_contract::MailResult::ID.0))
        .map(|(_, b)| decode_message::<std_mail_contract::MailResult>(b).unwrap().reason)
        .collect();
    assert_eq!(reasons, [MAIL_ATTACHMENTS_OFF, 0]);
}

fn results(
    h: &mut Harness,
    session: u64,
) -> (
    Vec<std_mail_contract::MailResult>,
    Vec<std_mail_contract::MailPage>,
) {
    use std_mail_contract::{MailPage, MailResult};
    let got = h.take(session);
    let results = got
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(MailResult::ID.0))
        .map(|(_, b)| decode_message::<MailResult>(b).unwrap())
        .collect();
    let pages = got
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(MailPage::ID.0))
        .map(|(_, b)| decode_message::<MailPage>(b).unwrap())
        .collect();
    (results, pages)
}

#[test]
fn requested_commands_answer_with_results_pages_and_reasons() {
    use std_mail_contract::{
        DeleteLetter, ListMailPage, MAIL_ATTACHMENTS, MAIL_NO_GOLD, MAIL_NOT_FOUND, MAIL_OK, MAIL_TO_SELF,
        SendMail, TakeMail,
    };
    let mut h = bed(&[]);
    let send = |request: u32, to: u64, gold: u64, has_item: bool, count: u32| {
        bytes_of(&SendMail {
            request,
            to,
            subject: WireString::new("crates").unwrap(),
            body: WireString::new("two of them").unwrap(),
            gold,
            has_item,
            slot: 0,
            count,
        })
    };
    h.send(1, SendMail::ID.0, &send(1, 12, 10, true, 2));
    h.send(1, SendMail::ID.0, &send(2, 11, 0, false, 0));
    h.send(1, SendMail::ID.0, &send(3, 12, 1_000, false, 0));
    h.ticks(2).unwrap();
    let (r, pages) = results(&mut h, 1);
    let codes: Vec<(u32, bool, u8)> = r.iter().map(|x| (x.request, x.ok, x.reason)).collect();
    assert_eq!(
        codes,
        vec![
            (1, true, MAIL_OK),
            (2, false, MAIL_TO_SELF),
            (3, false, MAIL_NO_GOLD)
        ]
    );
    assert_eq!(pages.len(), 3, "a fresh page after every command");
    assert_eq!(inv(&h).gold(11), 100 - 10 - POSTAGE, "refusals cost nothing");
    let letter_id = r[0].mail;

    // The recipient's page: subject, unread, attachments.
    h.send(2, ListMailPage::ID.0, &bytes_of(&ListMailPage { offset: 0 }));
    h.tick().unwrap();
    let (_, pages) = results(&mut h, 2);
    let e = pages[0].entries.iter().next().copied().unwrap();
    assert_eq!(pages[0].total, 1);
    assert_eq!(
        (e.mail, e.from, e.subject.as_str(), e.read),
        (letter_id, 11, "crates", false)
    );
    assert_eq!((e.gold, e.item, e.count), (10, 7, 2));
    h.send(2, ReadMail::ID.0, &bytes_of(&ReadMail { mail: letter_id }));
    // Commands run before the tick's intents: read first.
    h.tick().unwrap();
    h.take(2);
    h.send(
        2,
        DeleteLetter::ID.0,
        &bytes_of(&DeleteLetter {
            request: 7,
            mail: letter_id,
        }),
    );
    h.send(
        2,
        TakeMail::ID.0,
        &bytes_of(&TakeMail {
            request: 8,
            mail: letter_id,
        }),
    );
    h.send(
        2,
        TakeMail::ID.0,
        &bytes_of(&TakeMail {
            request: 9,
            mail: 999,
        }),
    );
    h.ticks(2).unwrap();
    let (r, pages) = results(&mut h, 2);
    let codes: Vec<(u32, u8)> = r.iter().map(|x| (x.request, x.reason)).collect();
    assert_eq!(
        codes,
        vec![(7, MAIL_ATTACHMENTS), (8, MAIL_OK), (9, MAIL_NOT_FOUND)]
    );
    let last = pages.last().unwrap().entries.iter().next().copied().unwrap();
    assert!(last.read);
    assert_eq!((last.gold, last.count), (0, 0));
    assert_eq!(inv(&h).gold(12), 10);

    // Paging over more letters than a page holds.
    for _ in 0..8 {
        h.send(1, SendMail::ID.0, &letter(12, 0, 0));
    }
    h.ticks(2).unwrap();
    h.send(2, ListMailPage::ID.0, &bytes_of(&ListMailPage { offset: 6 }));
    h.tick().unwrap();
    let (_, pages) = results(&mut h, 2);
    assert_eq!((pages[0].total, pages[0].offset), (9, 6));
    assert_eq!(pages[0].entries.iter().count(), 3);
    assert!(h.replay().unwrap() > 0);
}
