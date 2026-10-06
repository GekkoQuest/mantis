//! The std.mail client half against its contract codec: the start-up ask, mail pages,
//! letters, and arrivals drive the view model; paging intents ask for pages; send, take,
//! and delete send requested commands whose `MailResult` is the authoritative answer;
//! the `attachments` flag gates attaching; run-time refusals gate the module; the
//! screen's inputs fill the draft on Enter.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, WireString, decode_message, encode_into};
use mantis_ui::{ListItem, Modifiers, Properties, UiEvent, UiKey, Value};
use std_mail_client::PAGE;
use std_mail_contract::{
    DeleteLetter, ListMailPage, MAIL_ATTACHMENTS, MAIL_ATTACHMENTS_OFF, MAIL_BAG_FULL, MAIL_INVALID,
    MAIL_NO_GOLD, MAIL_NO_ITEM, MAIL_NOT_FOUND, MAIL_OK, MAIL_RECIPIENT_FULL, MAIL_TO_SELF, MailEntry,
    MailPage, MailResult, MailText, NewMail, ReadMail, SendMail, TakeMail,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");
/// std.mail depends on the std.containers contract, so the graph needs its manifest.
const CONTAINERS_MANIFEST: &str = include_str!("../../../containers/manifest.toml");

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = [
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(CONTAINERS_MANIFEST)?,
        },
        Discovered {
            origin: "std".to_owned(),
            manifest: parse_manifest(MANIFEST)?,
        },
    ];
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

/// Installs and starts the module, leaving the start-up messages queued.
fn start(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_mail_client::Module) as Arc<dyn ClientModule>],
    )?;
    let mut props = Properties::new();
    modules.start(&mut props);
    Ok((modules, props))
}

/// Installs and starts the module, and drains the start-up messages.
fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let (mut m, props) = start(flags)?;
    let _ = sent(&mut m);
    Ok((m, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

/// One page entry: (mail, from, read, gold, (item, count)); the subject names the letter.
type Row = (u32, u64, bool, u64, (u32, u32));

fn page(total: u16, offset: u16, rows: &[Row]) -> Result<MailPage, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for &(mail, from, read, gold, (item, count)) in rows {
        entries.push(MailEntry {
            mail,
            from,
            subject: WireString::new(&format!("Letter {mail}")).ok_or("subject")?,
            read,
            gold,
            item,
            count,
        });
    }
    Ok(MailPage {
        total,
        offset,
        entries: BoundedArray::from_slice(&entries).ok_or("entries")?,
    })
}

/// A plain unread letter from character 42.
fn plain(mail: u32) -> Row {
    (mail, 42, false, 0, (0, 0))
}

fn letter(mail: u32, from: u64, gold: u64, item: (u32, u32)) -> Result<MailText, Box<dyn std::error::Error>> {
    Ok(MailText {
        mail,
        from,
        subject: WireString::new("Hello").ok_or("subject")?,
        body: WireString::new("A letter for the player.").ok_or("body")?,
        gold,
        item: item.0,
        count: item.1,
    })
}

fn result(request: u32, reason: u8, mail: u32) -> MailResult {
    MailResult {
        request,
        ok: reason == MAIL_OK,
        reason,
        mail,
    }
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

fn letters(props: &mut Properties) -> Result<Vec<ListItem>, Box<dyn std::error::Error>> {
    match prop(props, "std.mail.letters") {
        Some(Value::List(items)) => Ok(items),
        other => Err(format!("letters: {other:?}").into()),
    }
}

fn field(item: Option<&ListItem>, name: &str) -> Option<Value> {
    item.and_then(|i| i.field(name)).cloned()
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn flag(b: bool) -> Value {
    Value::Bool(b)
}

fn int(v: i64) -> Value {
    Value::Int(v)
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

fn kinds(out: &[(u16, Vec<u8>)]) -> Vec<u16> {
    out.iter().map(|(k, _)| *k).collect()
}

fn only<M: Message>(m: &mut ClientModules) -> Result<M, Box<dyn std::error::Error>> {
    let out = sent(m);
    if kinds(&out) != [M::ID.0] {
        return Err(format!("expected one {}, sent {:?}", M::NAME, kinds(&out)).into());
    }
    Ok(decode_message::<M>(&out.first().ok_or("sent")?.1)?)
}

fn handled(m: &mut ClientModules, props: &mut Properties, name: &str, payload: Option<&str>) -> TestResult {
    match m.on_intent(name, payload, props) {
        IntentRoute::Handled => Ok(()),
        other => Err(format!("{name} {payload:?}: {other:?}").into()),
    }
}

/// Fills a draft to character 42 with the given attachments.
fn draft(m: &mut ClientModules, props: &mut Properties, gold: &str, slot: &str, count: &str) -> TestResult {
    for (name, payload) in [
        ("std.mail.to", "42"),
        ("std.mail.subject", "Hello"),
        ("std.mail.body", "An item for you."),
        ("std.mail.gold", gold),
        ("std.mail.slot", slot),
        ("std.mail.count", count),
    ] {
        handled(m, props, name, Some(payload))?;
    }
    Ok(())
}

#[test]
fn start_publishes_every_value_and_asks_for_the_first_page() -> TestResult {
    let (mut m, mut props) = start("")?;
    assert_eq!(only::<ListMailPage>(&mut m)?, ListMailPage { offset: 0 });
    assert_eq!(prop(&mut props, "std.mail.enabled"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.unavailable"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.open"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.count"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.unread_count"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_offset"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_total"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_first"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_last"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.has_next"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.has_prev"), Some(flag(false)));
    assert!(letters(&mut props)?.is_empty());
    assert_eq!(prop(&mut props, "std.mail.reading"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.read_body"), Some(text("")));
    assert_eq!(prop(&mut props, "std.mail.confirm_delete"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.delete_unarmed"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.new_mail"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.draft_to"), Some(text("")));
    assert_eq!(prop(&mut props, "std.mail.draft_attached"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.attachments_off"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.result"), Some(text("")));
    assert_eq!(prop(&mut props, "std.mail.result_ok"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.result_shown"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(0)));

    let (mut m, _) = start("\"std.mail.enabled\" = false\n")?;
    assert!(sent(&mut m).is_empty(), "a disabled module asks nothing");
    Ok(())
}

#[test]
fn mail_pages_fill_the_inbox_and_the_page_intents_move_through_them() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(PAGE, 6);
    let first: Vec<Row> = vec![
        (12, 42, false, 0, (0, 0)),
        (11, 0, true, 25, (0, 0)),
        (10, 7, false, 0, (3, 2)),
        plain(9),
        plain(8),
        (7, 42, true, 0, (0, 0)),
    ];
    deliver(&mut m, &mut props, &page(9, 0, &first)?);
    assert_eq!(prop(&mut props, "std.mail.count"), Some(int(6)));
    assert_eq!(prop(&mut props, "std.mail.unread_count"), Some(int(4)));
    assert_eq!(prop(&mut props, "std.mail.page_total"), Some(int(9)));
    assert_eq!(prop(&mut props, "std.mail.page_offset"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_first"), Some(int(1)));
    assert_eq!(prop(&mut props, "std.mail.page_last"), Some(int(6)));
    assert_eq!(prop(&mut props, "std.mail.has_next"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.has_prev"), Some(flag(false)));
    let items = letters(&mut props)?;
    let newest = items.first();
    assert_eq!(field(newest, "mail"), Some(int(12)));
    assert_eq!(field(newest, "from"), Some(text("42")));
    assert_eq!(field(newest, "system"), Some(flag(false)));
    assert_eq!(
        field(newest, "subject"),
        Some(text("Letter 12")),
        "straight from the page"
    );
    assert_eq!(field(newest, "unread"), Some(flag(true)));
    assert_eq!(field(newest, "attached"), Some(flag(false)));
    let system = items.get(1);
    assert_eq!(field(system, "system"), Some(flag(true)));
    assert_eq!(field(system, "read"), Some(flag(true)));
    assert_eq!(field(system, "attached"), Some(flag(true)));
    assert_eq!(field(system, "gold"), Some(int(25)));
    let with_item = items.get(2);
    assert_eq!(
        field(with_item, "attached"),
        Some(flag(true)),
        "an item, from the page"
    );
    assert_eq!(field(with_item, "item"), Some(int(3)));
    assert_eq!(field(with_item, "count"), Some(int(2)));

    assert_eq!(
        m.on_intent("std.mail.prev_page", None, &mut props),
        IntentRoute::Refused,
        "no newer page"
    );
    handled(&mut m, &mut props, "std.mail.next_page", None)?;
    assert_eq!(only::<ListMailPage>(&mut m)?, ListMailPage { offset: 6 });
    assert_eq!(
        prop(&mut props, "std.mail.page_offset"),
        Some(int(0)),
        "the page moves when the server answers"
    );

    deliver(&mut m, &mut props, &page(9, 6, &[plain(6), plain(5), plain(4)])?);
    assert_eq!(prop(&mut props, "std.mail.count"), Some(int(3)));
    assert_eq!(prop(&mut props, "std.mail.page_offset"), Some(int(6)));
    assert_eq!(prop(&mut props, "std.mail.page_first"), Some(int(7)));
    assert_eq!(prop(&mut props, "std.mail.page_last"), Some(int(9)));
    assert_eq!(prop(&mut props, "std.mail.has_next"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.has_prev"), Some(flag(true)));
    assert_eq!(field(letters(&mut props)?.first(), "mail"), Some(int(6)));
    assert_eq!(
        m.on_intent("std.mail.next_page", None, &mut props),
        IntentRoute::Refused,
        "no older page"
    );
    assert!(sent(&mut m).is_empty());
    handled(&mut m, &mut props, "std.mail.list", None)?;
    assert_eq!(
        only::<ListMailPage>(&mut m)?,
        ListMailPage { offset: 6 },
        "refresh asks for the current page"
    );
    handled(&mut m, &mut props, "std.mail.prev_page", None)?;
    assert_eq!(only::<ListMailPage>(&mut m)?, ListMailPage { offset: 0 });

    // Letters went away under the page: ask for the last page that exists.
    deliver(&mut m, &mut props, &page(8, 12, &[])?);
    assert_eq!(only::<ListMailPage>(&mut m)?, ListMailPage { offset: 6 });
    deliver(&mut m, &mut props, &page(0, 0, &[])?);
    assert!(sent(&mut m).is_empty(), "an empty mailbox asks nothing more");
    assert_eq!(prop(&mut props, "std.mail.page_first"), Some(int(0)));

    Ok(())
}

#[test]
fn pages_are_bounded_and_malformed_messages_are_counted() -> TestResult {
    let (mut m, mut props) = install("")?;
    let full: Vec<Row> = (1..=6).map(plain).collect();
    deliver(&mut m, &mut props, &page(40, 0, &full)?);
    assert_eq!(
        letters(&mut props)?.len(),
        PAGE,
        "a page is bounded by the contract"
    );

    m.on_message(ExtensionKind(MailPage::ID.0), &[2, 0], &mut props);
    m.on_message(ExtensionKind(MailText::ID.0), &[1], &mut props);
    m.on_message(ExtensionKind(NewMail::ID.0), &[], &mut props);
    m.on_message(ExtensionKind(MailResult::ID.0), &[1, 0], &mut props);
    assert_eq!(
        m.stats().malformed,
        4,
        "malformed messages are counted, never errors"
    );
    assert_eq!(letters(&mut props)?.len(), PAGE);
    Ok(())
}

#[test]
fn reading_fills_the_pane_and_marks_the_letter_read() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &page(2, 0, &[plain(8), plain(7)])?);
    handled(&mut m, &mut props, "std.mail.read", Some("7"))?;
    assert_eq!(only::<ReadMail>(&mut m)?, ReadMail { mail: 7 });
    for payload in [None, Some(""), Some("0"), Some("letter")] {
        assert_eq!(
            m.on_intent("std.mail.read", payload, &mut props),
            IntentRoute::Refused,
            "{payload:?}"
        );
    }
    assert!(sent(&mut m).is_empty());

    deliver(&mut m, &mut props, &letter(7, 42, 30, (3, 2))?);
    assert_eq!(prop(&mut props, "std.mail.reading"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.read_mail"), Some(int(7)));
    assert_eq!(prop(&mut props, "std.mail.read_from"), Some(text("42")));
    assert_eq!(prop(&mut props, "std.mail.read_subject"), Some(text("Hello")));
    assert_eq!(
        prop(&mut props, "std.mail.read_body"),
        Some(text("A letter for the player."))
    );
    assert_eq!(prop(&mut props, "std.mail.read_gold"), Some(int(30)));
    assert_eq!(prop(&mut props, "std.mail.read_item"), Some(int(3)));
    assert_eq!(prop(&mut props, "std.mail.read_count"), Some(int(2)));
    assert_eq!(prop(&mut props, "std.mail.read_attached"), Some(flag(true)));
    assert_eq!(field(letters(&mut props)?.get(1), "read"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.unread_count"), Some(int(1)));

    handled(&mut m, &mut props, "std.mail.close", None)?;
    assert_eq!(prop(&mut props, "std.mail.reading"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.read_body"), Some(text("")));
    assert!(sent(&mut m).is_empty(), "closing is local");
    Ok(())
}

#[test]
fn new_mail_shows_a_notice_and_refreshes_the_current_page() -> TestResult {
    let (mut m, mut props) = install("")?;
    let rows: Vec<Row> = (1..=6).map(plain).collect();
    deliver(&mut m, &mut props, &page(12, 6, &rows)?);
    deliver(&mut m, &mut props, &NewMail { mail: 13, from: 42 });
    assert_eq!(prop(&mut props, "std.mail.new_mail"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.new_from"), Some(text("42")));
    assert_eq!(only::<ListMailPage>(&mut m)?, ListMailPage { offset: 6 });
    handled(&mut m, &mut props, "std.mail.dismiss", None)?;
    assert_eq!(prop(&mut props, "std.mail.new_mail"), Some(flag(false)));
    assert!(sent(&mut m).is_empty(), "dismissing is local");
    Ok(())
}

#[test]
fn compose_validates_cheaply_and_sends_a_requested_letter() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.mail.send", None, &mut props),
        IntentRoute::Refused,
        "no recipient"
    );
    let long_subject = "s".repeat(33);
    let long_body = "b".repeat(201);
    for (name, payload) in [
        ("std.mail.to", None),
        ("std.mail.to", Some("0")),
        ("std.mail.to", Some("player")),
        ("std.mail.subject", None),
        ("std.mail.subject", Some("   ")),
        ("std.mail.subject", Some(long_subject.as_str())),
        ("std.mail.body", Some(long_body.as_str())),
        ("std.mail.gold", Some("lots")),
        ("std.mail.slot", Some("256")),
        ("std.mail.count", Some("-1")),
    ] {
        assert_eq!(
            m.on_intent(name, payload, &mut props),
            IntentRoute::Refused,
            "{name} {payload:?}"
        );
    }
    handled(&mut m, &mut props, "std.mail.to", Some("42"))?;
    assert_eq!(
        m.on_intent("std.mail.send", None, &mut props),
        IntentRoute::Refused,
        "no subject"
    );
    draft(&mut m, &mut props, "15", "3", "2")?;
    assert!(sent(&mut m).is_empty(), "drafting is local");
    assert_eq!(prop(&mut props, "std.mail.draft_to"), Some(text("42")));
    assert_eq!(prop(&mut props, "std.mail.draft_subject"), Some(text("Hello")));
    assert_eq!(prop(&mut props, "std.mail.draft_gold"), Some(int(15)));
    assert_eq!(prop(&mut props, "std.mail.draft_attached"), Some(flag(true)));

    handled(&mut m, &mut props, "std.mail.send", None)?;
    let with_item = only::<SendMail>(&mut m)?;
    assert_ne!(with_item.request, 0);
    assert_eq!(
        with_item,
        SendMail {
            request: with_item.request,
            to: 42,
            subject: WireString::new("Hello").ok_or("subject")?,
            body: WireString::new("An item for you.").ok_or("body")?,
            gold: 15,
            has_item: true,
            slot: 3,
            count: 2,
        }
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(1)));
    assert_eq!(
        prop(&mut props, "std.mail.draft_to"),
        Some(text("42")),
        "the draft stays until the result"
    );
    deliver(
        &mut m,
        &mut props,
        &result(with_item.request, MAIL_RECIPIENT_FULL, 0),
    );
    assert_eq!(
        prop(&mut props, "std.mail.draft_to"),
        Some(text("42")),
        "a refused letter keeps the draft"
    );

    // Gold only: no item, whatever the slot says.
    handled(&mut m, &mut props, "std.mail.count", Some("0"))?;
    handled(&mut m, &mut props, "std.mail.send", None)?;
    let gold_only = only::<SendMail>(&mut m)?;
    assert_ne!(gold_only.request, 0);
    assert_ne!(gold_only.request, with_item.request, "every request is distinct");
    assert!(!gold_only.has_item);
    assert_eq!((gold_only.gold, gold_only.slot, gold_only.count), (15, 0, 0));
    deliver(&mut m, &mut props, &result(gold_only.request, MAIL_OK, 77));
    assert_eq!(prop(&mut props, "std.mail.result"), Some(text("Letter sent")));
    assert_eq!(prop(&mut props, "std.mail.result_ok"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.result_shown"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(0)));
    assert_eq!(
        prop(&mut props, "std.mail.draft_to"),
        Some(text("")),
        "a confirmed letter clears the draft"
    );
    assert_eq!(prop(&mut props, "std.mail.draft_attached"), Some(flag(false)));

    handled(&mut m, &mut props, "std.mail.to", Some("7"))?;
    handled(&mut m, &mut props, "std.mail.discard", None)?;
    assert_eq!(prop(&mut props, "std.mail.draft_to"), Some(text("")));
    assert!(sent(&mut m).is_empty());
    Ok(())
}

#[test]
fn attachments_follow_the_flag_and_not_allowed_keeps_mail_working() -> TestResult {
    let (mut m, mut props) = install("\"std.mail.attachments\" = false\n")?;
    assert_eq!(prop(&mut props, "std.mail.flag.attachments"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.attachments_off"), Some(flag(true)));
    for name in ["std.mail.gold", "std.mail.slot", "std.mail.count"] {
        assert_eq!(
            m.on_intent(name, Some("1"), &mut props),
            IntentRoute::Refused,
            "{name}: attachments are off"
        );
    }
    handled(&mut m, &mut props, "std.mail.to", Some("42"))?;
    handled(&mut m, &mut props, "std.mail.subject", Some("Hello"))?;
    handled(&mut m, &mut props, "std.mail.send", None)?;
    let plain_letter = only::<SendMail>(&mut m)?;
    assert_eq!(
        (plain_letter.gold, plain_letter.has_item, plain_letter.count),
        (0, false, 0),
        "plain letters still go"
    );

    deliver(&mut m, &mut props, &letter(7, 42, 30, (0, 0))?);
    handled(&mut m, &mut props, "std.mail.take", None)?;
    let take = only::<TakeMail>(&mut m)?;
    assert_eq!(take.mail, 7, "gold already attached can still be taken");

    // A refusal of a kind without a result (here a page) still shows.
    handled(&mut m, &mut props, "std.mail.list", None)?;
    let _ = sent(&mut m);
    m.on_refused(
        ExtensionKind(ListMailPage::ID.0),
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.mail.refusal"), Some(text("not allowed")));
    assert_eq!(prop(&mut props, "std.mail.enabled"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.unavailable"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.reading"),
        Some(flag(true)),
        "the view is kept"
    );
    Ok(())
}

#[test]
fn take_and_a_confirmed_delete_wait_for_their_results() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.mail.take", None, &mut props),
        IntentRoute::Refused,
        "no letter selected"
    );
    assert_eq!(
        m.on_intent("std.mail.delete", None, &mut props),
        IntentRoute::Refused
    );
    deliver(
        &mut m,
        &mut props,
        &page(2, 0, &[plain(8), (7, 42, true, 30, (3, 2))])?,
    );
    deliver(&mut m, &mut props, &letter(7, 42, 30, (3, 2))?);
    handled(&mut m, &mut props, "std.mail.take", None)?;
    let take = only::<TakeMail>(&mut m)?;
    assert_ne!(take.request, 0);
    assert_eq!(take.mail, 7);
    assert_eq!(
        prop(&mut props, "std.mail.read_attached"),
        Some(flag(true)),
        "nothing changes before the result"
    );
    assert_eq!(field(letters(&mut props)?.get(1), "attached"), Some(flag(true)));

    handled(&mut m, &mut props, "std.mail.delete", None)?;
    assert!(sent(&mut m).is_empty(), "the first delete only arms");
    assert_eq!(prop(&mut props, "std.mail.confirm_delete"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.delete_unarmed"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.delete_mail"), Some(int(7)));
    handled(&mut m, &mut props, "std.mail.cancel", None)?;
    assert_eq!(prop(&mut props, "std.mail.confirm_delete"), Some(flag(false)));
    handled(&mut m, &mut props, "std.mail.delete", Some("7"))?;
    handled(&mut m, &mut props, "std.mail.delete", Some("8"))?;
    assert_eq!(
        prop(&mut props, "std.mail.delete_mail"),
        Some(int(8)),
        "another letter re-arms"
    );
    assert!(sent(&mut m).is_empty());
    handled(&mut m, &mut props, "std.mail.delete", Some("7"))?;
    handled(&mut m, &mut props, "std.mail.delete", None)?;
    let delete = only::<DeleteLetter>(&mut m)?;
    assert_eq!(delete.mail, 7);
    assert_ne!(delete.request, 0);
    assert_ne!(delete.request, take.request, "every request is distinct");
    assert_eq!(prop(&mut props, "std.mail.confirm_delete"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.reading"),
        Some(flag(true)),
        "open until confirmed"
    );
    assert_eq!(
        prop(&mut props, "std.mail.count"),
        Some(int(2)),
        "the page waits for the server"
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(2)));

    deliver(&mut m, &mut props, &result(take.request, MAIL_OK, 7));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Attachments taken"))
    );
    assert_eq!(prop(&mut props, "std.mail.result_ok"), Some(flag(true)));
    assert_eq!(prop(&mut props, "std.mail.read_attached"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.reading"), Some(flag(true)));
    deliver(
        &mut m,
        &mut props,
        &page(2, 0, &[plain(8), (7, 42, true, 0, (0, 0))])?,
    );
    assert_eq!(field(letters(&mut props)?.get(1), "attached"), Some(flag(false)));

    deliver(&mut m, &mut props, &result(delete.request, MAIL_OK, 7));
    assert_eq!(prop(&mut props, "std.mail.result"), Some(text("Letter deleted")));
    assert_eq!(
        prop(&mut props, "std.mail.reading"),
        Some(flag(false)),
        "the pane closes"
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(0)));
    deliver(&mut m, &mut props, &page(1, 0, &[plain(8)])?);
    assert_eq!(prop(&mut props, "std.mail.count"), Some(int(1)));
    assert_eq!(field(letters(&mut props)?.first(), "mail"), Some(int(8)));
    assert!(
        sent(&mut m).is_empty(),
        "results ask nothing; the server sends the page"
    );
    assert_eq!(
        m.on_intent("std.mail.delete", Some("nothing"), &mut props),
        IntentRoute::Refused
    );
    Ok(())
}

#[test]
fn every_failure_reason_reads_for_its_own_request() -> TestResult {
    let (mut m, mut props) = install("")?;
    for (reason, expected) in [
        (MAIL_INVALID, "The request was not valid"),
        (MAIL_NOT_FOUND, "No such letter"),
        (MAIL_RECIPIENT_FULL, "Recipient's mailbox is full"),
        (MAIL_NO_GOLD, "Not enough gold for the attachment and postage"),
        (MAIL_NO_ITEM, "That bag slot does not hold that many"),
        (MAIL_BAG_FULL, "Your bag cannot hold the attachments"),
        (MAIL_ATTACHMENTS_OFF, "Attachments are switched off"),
        (MAIL_TO_SELF, "You cannot mail yourself"),
        (200, "Mail refused"),
    ] {
        draft(&mut m, &mut props, "0", "0", "0")?;
        handled(&mut m, &mut props, "std.mail.send", None)?;
        let request = only::<SendMail>(&mut m)?.request;
        assert_eq!(
            prop(&mut props, "std.mail.result_shown"),
            Some(flag(false)),
            "cleared by the send"
        );
        deliver(&mut m, &mut props, &result(request, reason, 0));
        assert_eq!(
            prop(&mut props, "std.mail.result"),
            Some(text(expected)),
            "{reason}"
        );
        assert_eq!(
            prop(&mut props, "std.mail.result_ok"),
            Some(flag(false)),
            "{reason}"
        );
        assert_eq!(
            prop(&mut props, "std.mail.result_shown"),
            Some(flag(true)),
            "{reason}"
        );
    }

    // Two commands in flight: each result answers only its own request.
    deliver(
        &mut m,
        &mut props,
        &page(2, 0, &[plain(8), (7, 42, true, 30, (0, 0))])?,
    );
    handled(&mut m, &mut props, "std.mail.take", Some("8"))?;
    let take = only::<TakeMail>(&mut m)?.request;
    handled(&mut m, &mut props, "std.mail.delete", Some("7"))?;
    handled(&mut m, &mut props, "std.mail.delete", Some("7"))?;
    let delete = only::<DeleteLetter>(&mut m)?.request;
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(2)));
    deliver(&mut m, &mut props, &result(delete, MAIL_ATTACHMENTS, 0));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Take the attachments before deleting"))
    );
    assert_eq!(
        prop(&mut props, "std.mail.pending"),
        Some(int(1)),
        "the take still waits"
    );
    deliver(&mut m, &mut props, &result(take, MAIL_ATTACHMENTS, 0));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Nothing attached to take"))
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(0)));
    Ok(())
}

#[test]
fn a_result_for_an_unknown_request_is_counted_and_ignored() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &page(1, 0, &[(7, 42, true, 30, (0, 0))])?);
    deliver(&mut m, &mut props, &letter(7, 42, 30, (0, 0))?);
    handled(&mut m, &mut props, "std.mail.take", None)?;
    let take = only::<TakeMail>(&mut m)?.request;
    for stray in [0, take.wrapping_add(1), take.wrapping_add(100)] {
        deliver(&mut m, &mut props, &result(stray, MAIL_OK, 7));
    }
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(3)));
    assert_eq!(prop(&mut props, "std.mail.result_shown"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.read_attached"),
        Some(flag(true)),
        "nothing changes"
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(1)));

    deliver(&mut m, &mut props, &result(take, MAIL_BAG_FULL, 0));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Your bag cannot hold the attachments"))
    );
    deliver(&mut m, &mut props, &result(take, MAIL_OK, 7));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Your bag cannot hold the attachments")),
        "a second answer to the same request is stray"
    );
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(4)));
    assert_eq!(prop(&mut props, "std.mail.read_attached"), Some(flag(true)));
    Ok(())
}

/// Sends a plain letter to character 42 and returns its request.
fn send_plain(m: &mut ClientModules, props: &mut Properties) -> Result<u32, Box<dyn std::error::Error>> {
    draft(m, props, "0", "0", "0")?;
    handled(m, props, "std.mail.send", None)?;
    Ok(only::<SendMail>(m)?.request)
}

/// Asks to take letter 7's attachments and returns the request.
fn take_seven(m: &mut ClientModules, props: &mut Properties) -> Result<u32, Box<dyn std::error::Error>> {
    handled(m, props, "std.mail.take", Some("7"))?;
    Ok(only::<TakeMail>(m)?.request)
}

#[test]
fn a_refusal_of_a_requested_command_does_not_overwrite_its_result() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(
        &mut m,
        &mut props,
        &page(2, 0, &[(8, 42, true, 0, (0, 0)), (7, 42, true, 30, (0, 0))])?,
    );
    let not_allowed = ExtensionRefusal::NotAllowed;

    // Each command's refusal follows its failed result and is absorbed.
    let send = send_plain(&mut m, &mut props)?;
    deliver(&mut m, &mut props, &result(send, MAIL_RECIPIENT_FULL, 0));
    m.on_refused_tracked(ExtensionKind(SendMail::ID.0), 901, not_allowed, &mut props);
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Recipient's mailbox is full"))
    );
    let take = take_seven(&mut m, &mut props)?;
    deliver(&mut m, &mut props, &result(take, MAIL_BAG_FULL, 0));
    m.on_refused_tracked(ExtensionKind(TakeMail::ID.0), 902, not_allowed, &mut props);
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Your bag cannot hold the attachments"))
    );
    handled(&mut m, &mut props, "std.mail.delete", Some("8"))?;
    handled(&mut m, &mut props, "std.mail.delete", Some("8"))?;
    let delete = only::<DeleteLetter>(&mut m)?.request;
    deliver(&mut m, &mut props, &result(delete, MAIL_NOT_FOUND, 0));
    m.on_refused_tracked(ExtensionKind(DeleteLetter::ID.0), 903, not_allowed, &mut props);
    assert_eq!(prop(&mut props, "std.mail.result"), Some(text("No such letter")));
    assert_eq!(prop(&mut props, "std.mail.result_ok"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.refusal"),
        None,
        "never repeated as a refusal"
    );
    assert_eq!(m.stats().refusals_answered, 3);
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(0)));

    // A refusal with no result answers the oldest pending command of its kind.
    let first = take_seven(&mut m, &mut props)?;
    let second = take_seven(&mut m, &mut props)?;
    m.on_refused_tracked(ExtensionKind(TakeMail::ID.0), 904, not_allowed, &mut props);
    assert_eq!(prop(&mut props, "std.mail.result"), Some(text("Not allowed")));
    assert_eq!(prop(&mut props, "std.mail.result_ok"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.pending"),
        Some(int(1)),
        "the second take still waits"
    );
    deliver(&mut m, &mut props, &result(second, MAIL_OK, 7));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Attachments taken"))
    );
    deliver(&mut m, &mut props, &result(first, MAIL_BAG_FULL, 0));
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("Your bag cannot hold the attachments")),
        "a late result for a refused command still says why"
    );
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(0)));

    let _ = send_plain(&mut m, &mut props)?;
    m.on_refused_tracked(
        ExtensionKind(SendMail::ID.0),
        905,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("The request was not valid"))
    );
    assert_eq!(prop(&mut props, "std.mail.pending"), Some(int(0)));

    // Nothing pending of that kind: counted, still not a refusal line.
    m.on_refused_tracked(ExtensionKind(DeleteLetter::ID.0), 906, not_allowed, &mut props);
    assert_eq!(prop(&mut props, "std.mail.stray_results"), Some(int(1)));
    assert_eq!(prop(&mut props, "std.mail.refusal"), None);
    assert_eq!(m.stats().refusals_answered, 6);

    // Refusals of other kinds still show as refusals.
    m.on_refused_tracked(
        ExtensionKind(ReadMail::ID.0),
        907,
        ExtensionRefusal::Invalid,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.mail.refusal"), Some(text("invalid")));
    assert_eq!(m.stats().refusals_answered, 6);
    assert_eq!(
        prop(&mut props, "std.mail.result"),
        Some(text("The request was not valid")),
        "the last result stays until the next command"
    );
    Ok(())
}

#[test]
fn feature_disabled_gates_the_module() -> TestResult {
    let (mut m, mut props) = install("")?;
    deliver(&mut m, &mut props, &page(7, 0, &[plain(7)])?);
    deliver(&mut m, &mut props, &letter(7, 42, 0, (0, 0))?);
    deliver(&mut m, &mut props, &NewMail { mail: 8, from: 5 });
    let _ = sent(&mut m);
    m.on_refused(
        ExtensionKind(ListMailPage::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.mail.enabled"), Some(flag(false)));
    assert_eq!(
        prop(&mut props, "std.mail.unavailable"),
        Some(flag(true)),
        "the screen says so"
    );
    assert_eq!(prop(&mut props, "std.mail.count"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.page_total"), Some(int(0)));
    assert_eq!(prop(&mut props, "std.mail.reading"), Some(flag(false)));
    assert_eq!(prop(&mut props, "std.mail.new_mail"), Some(flag(false)));
    for (name, payload) in [
        ("std.mail.list", None),
        ("std.mail.next_page", None),
        ("std.mail.prev_page", None),
        ("std.mail.read", Some("7")),
        ("std.mail.to", Some("42")),
        ("std.mail.send", None),
        ("std.mail.take", Some("7")),
        ("std.mail.delete", Some("7")),
    ] {
        assert_eq!(
            m.on_intent(name, payload, &mut props),
            IntentRoute::Refused,
            "{name}"
        );
    }
    deliver(&mut m, &mut props, &NewMail { mail: 9, from: 5 });
    deliver(&mut m, &mut props, &result(1, MAIL_OK, 9));
    assert_eq!(m.stats().disabled, 10, "eight intents and two messages dropped");
    assert!(sent(&mut m).is_empty());
    Ok(())
}

fn ui_for(m: &ClientModules) -> Result<mantis_ui::Ui, Box<dyn std::error::Error>> {
    let (layout, theme) = m.compose(&["std.mail.window"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    Ok(mantis_ui::Ui::new(fonts, &layout, Some(&theme))?)
}

fn shown(ui: &mantis_ui::Ui, id: &str) -> bool {
    ui.rect_of(id).is_some_and(|r| r.h > 0.0)
}

#[test]
fn the_screen_composes_and_the_toggle_action_shows_it() -> TestResult {
    let (mut m, mut props) = install("")?;
    let ui = ui_for(&m)?;
    assert!(ui.rect_of("std_mail_inbox").is_some());
    assert!(ui.rect_of("std_mail_send").is_some());
    assert!(ui.rect_of("std_mail_pages").is_some());
    let actions: Vec<_> = m.actions().map(|a| (a.name, a.default_key)).collect();
    assert_eq!(actions, [("std.mail.toggle", Some(KeyCode::M))]);
    m.on_actions(|name| name == "std.mail.toggle", &mut props);
    assert_eq!(
        prop(&mut props, "std.mail.open"),
        Some(flag(false)),
        "toggled closed"
    );
    Ok(())
}

#[test]
fn the_screen_shows_page_controls_and_the_result_line() -> TestResult {
    let (mut m, _) = install("")?;
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    let _ = sent(&mut m);
    let rows: Vec<Row> = (1..=6).map(plain).collect();
    deliver(&mut m, ui.properties_mut(), &page(9, 0, &rows)?);
    let _ = ui.frame([800.0, 900.0], 1.0);
    assert!(shown(&ui, "std_mail_next"), "an older page exists");
    assert!(!shown(&ui, "std_mail_prev"), "no newer page");
    assert!(!shown(&ui, "std_mail_result"), "nothing answered yet");

    draft(&mut m, ui.properties_mut(), "0", "0", "0")?;
    handled(&mut m, ui.properties_mut(), "std.mail.send", None)?;
    let request = only::<SendMail>(&mut m)?.request;
    deliver(
        &mut m,
        ui.properties_mut(),
        &result(request, MAIL_RECIPIENT_FULL, 0),
    );
    m.on_refused(
        ExtensionKind(SendMail::ID.0),
        ExtensionRefusal::NotAllowed,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 900.0], 1.0);
    assert!(shown(&ui, "std_mail_result"), "the result line answers");
    assert_eq!(
        prop(ui.properties_mut(), "std.mail.refusal"),
        None,
        "and the refusal does not repeat it"
    );
    assert_eq!(
        ui.is_enabled("std_mail_send"),
        Some(true),
        "a refused letter leaves mail usable"
    );
    Ok(())
}

#[test]
fn enter_in_the_subject_input_fills_the_draft() -> TestResult {
    let (mut m, _) = install("")?;
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    let _ = sent(&mut m);
    let _ = ui.frame([800.0, 900.0], 1.0);
    assert!(ui.set_focus("std_mail_subject"));
    let _ = ui.handle(&UiEvent::Text("Hello".to_owned()));
    let _ = ui.handle(&UiEvent::Key {
        key: UiKey::Enter,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    let mut intents = Vec::new();
    ui.drain_intents(&mut intents);
    let intent = intents.first().ok_or("no intent")?;
    let name = ui.intent_name(intent.intent).ok_or("intent name")?.to_owned();
    assert_eq!(name, "std.mail.subject");
    assert_eq!(
        m.on_intent(&name, intent.payload.as_deref(), ui.properties_mut()),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(ui.properties_mut(), "std.mail.draft_subject"),
        Some(text("Hello"))
    );
    Ok(())
}

#[test]
fn a_disabled_mail_screen_renders_unavailable_not_as_dead_controls() -> TestResult {
    let (mut m, _) = install("\"std.mail.attachments\" = false\n")?;
    // The registry writes straight into the UI's properties.
    let mut ui = ui_for(&m)?;
    m.start(ui.properties_mut());
    let _ = sent(&mut m);
    let _ = ui.frame([800.0, 900.0], 1.0);
    assert_eq!(ui.is_enabled("std_mail_send"), Some(true));
    assert_eq!(ui.is_enabled("std_mail_subject"), Some(true));
    assert_eq!(
        ui.is_enabled("std_mail_gold"),
        Some(false),
        "attachments off shows as unavailable"
    );
    assert!(shown(&ui, "std_mail_attachments_off"), "the form says why");
    assert!(
        ui.rect_of("std_mail_unavailable").is_none_or(|r| r.h <= 0.0),
        "nothing to explain while enabled"
    );
    m.on_refused(
        ExtensionKind(SendMail::ID.0),
        ExtensionRefusal::NotAllowed,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 900.0], 1.0);
    assert_eq!(
        ui.is_enabled("std_mail_send"),
        Some(true),
        "a refused letter leaves mail usable"
    );
    m.on_refused(
        ExtensionKind(SendMail::ID.0),
        ExtensionRefusal::FeatureDisabled,
        ui.properties_mut(),
    );
    let _ = ui.frame([800.0, 900.0], 1.0);
    for id in ["std_mail_send", "std_mail_subject", "std_mail_refresh"] {
        assert_eq!(ui.is_enabled(id), Some(false), "{id} shown as unavailable");
    }
    assert!(shown(&ui, "std_mail_unavailable"), "the panel says why");
    Ok(())
}
