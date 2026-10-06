//! std.mail client: the mailbox view model, the mail screen (a paged inbox, the reading
//! pane, the compose form, and the result line), and the mail intents (plan 13).
//!
//! **View model** (bindable properties, all under `std.mail.`):
//!
//! | property | value |
//! |---|---|
//! | `letters` | the current page, newest first, at most [`PAGE`]; each item has `mail` (the letter id), `from` (the sender's character, as text), `system` (flag: sent by the system), `subject`, `read` and `unread` (flags), `attached` (flag: gold or an item is attached), `gold`, `item` (item id, 0 for none), and `count` |
//! | `count` | letters on the page |
//! | `unread_count` | unread letters on the page |
//! | `page_offset` | letters skipped from the newest |
//! | `page_total` | letters in the mailbox |
//! | `page_first`, `page_last` | the page's first and last position, from 1 (both 0 for an empty page) |
//! | `has_next`, `has_prev` | whether an older or a newer page exists (the page buttons show) |
//! | `reading` | whether the reading pane shows a letter |
//! | `read_mail`, `read_from`, `read_subject`, `read_body` | the letter in the reading pane |
//! | `read_gold`, `read_item`, `read_count` | its attached gold, item id, and item count |
//! | `read_attached` | whether it has anything attached (the take button shows) |
//! | `confirm_delete` | whether a delete is armed and waits for confirmation |
//! | `delete_unarmed` | its negation (the plain delete button shows) |
//! | `delete_mail` | the letter a delete is armed for (0 when none) |
//! | `new_mail` | whether the "new mail" notice is shown |
//! | `new_from` | the new letter's sender, as text |
//! | `draft_to` | the draft's recipient, as text (empty when none) |
//! | `draft_subject`, `draft_body` | the draft's subject and body |
//! | `draft_gold`, `draft_slot`, `draft_count` | the draft's attached gold, bag slot, and item count |
//! | `draft_attached` | whether the draft attaches anything |
//! | `attachments_off` | whether the `attachments` flag is off (the compose form says so) |
//! | `result` | the answer to the last send, take, or delete (empty until one is answered) |
//! | `result_ok` | whether that answer was a success |
//! | `result_shown` | whether the result line shows |
//! | `pending` | sends, takes, and deletes not answered yet |
//! | `stray_results` | results and command refusals that answered no pending request (counted and ignored) |
//! | `open` | whether the mail screen is shown (toggled by `std.mail.toggle`) |
//!
//! The registry adds `std.mail.enabled`, `std.mail.unavailable`,
//! `std.mail.flag.attachments`, and `std.mail.refusal`; the screen binds `enabled=` to
//! them. The compose form's attachment inputs bind `enabled="std.mail.flag.attachments"`.
//! The take button stays available with the flag off: the flag gates attaching, and
//! letters sent before it was switched off may still carry gold. Only a
//! `FeatureDisabled` refusal (the whole module off) greys mail out.
//!
//! **Requests and results.** Sending, taking, and deleting carry a request number from
//! the module's own non-zero counter; each is held as pending until a `MailResult` with
//! that number answers it. The result is authoritative: success shows a short
//! confirmation, failure a readable reason per `MAIL_*` code, in `result`. Nothing is
//! changed before the answer: the inbox comes from the fresh `MailPage` the server sends
//! after every result, a confirmed delete closes the reading pane on that letter, a
//! confirmed take empties its attachments there, and a confirmed send clears the draft
//! (a refused one keeps it for another try). A result for a request that is not pending
//! is counted in `stray_results` and changes nothing.
//!
//! A failed command also draws an `ExtensionRefused` of its kind (`SendMail`,
//! `TakeMail`, `DeleteLetter`), which the module answers itself through the registry's
//! refusal hook, so `std.mail.refusal` never repeats a result. The server sends the
//! `MailResult` before the refusal, so a failed result expects one refusal of its kind,
//! which is absorbed. A refusal nothing expects is a command refused before it could be
//! answered (malformed, or no character): it answers the oldest pending request of that
//! kind as a failure (`Not allowed`, or the invalid-request text), and should a
//! `MailResult` for that request still come, its reason replaces the text. Refusals of
//! other kinds show in `std.mail.refusal` as before.
//!
//! **Messages**: `MailPage` replaces the inbox page (an empty page past the end asks for
//! the last page); `MailText` fills the reading pane and marks the letter read on the
//! page; `NewMail` shows a notice and asks for the current page again; `MailResult`
//! answers a pending request.
//!
//! **Intents**:
//!
//! - `std.mail.list` asks for the current page again; `std.mail.next_page` and
//!   `std.mail.prev_page` ask for the older or newer page (`ListMailPage`), refused
//!   when there is none; `std.mail.read` (payload: the letter id) asks for one letter;
//!   `std.mail.close` (local) closes the reading pane; `std.mail.dismiss` (local) hides
//!   the new-mail notice.
//! - Compose (local, as the inputs submit them): `std.mail.to` (a character),
//!   `std.mail.subject` (1 to 32 bytes, not blank), `std.mail.body` (up to 200 bytes),
//!   and, with attachments on, `std.mail.gold`, `std.mail.slot`, and `std.mail.count`.
//!   `std.mail.discard` clears the draft; `std.mail.send` sends `SendMail` (an item is
//!   attached exactly when the count is not 0).
//! - `std.mail.take` sends `TakeMail`; `std.mail.delete` arms a delete on its first use
//!   and sends `DeleteLetter` on the second for the same letter; `std.mail.cancel`
//!   disarms. Both take the letter id as payload, or the letter in the reading pane
//!   without one.
//!
//! When the module starts (or is enabled again) it asks for the first page.
//!
//! The server holds every rule (`RULES.md`): postage, mailbox room, who may receive,
//! and what may be taken or deleted. This half only refuses what cannot be sent at all
//! (no recipient, a blank or over-long field, no letter selected, attachments while
//! they are off, no page in that direction). Characters are shown by id until a name
//! service exists.

#![forbid(unsafe_code)]

use mantis_client::input::device::KeyCode;
use mantis_client::modules::{
    ClientModule, ClientRegistrar, ClientRegistryError, ExtensionRefusal, IntentFn, ModuleContext,
    ModuleError, decode,
};
use mantis_core::wire::{Message, WireString};
use mantis_ui::{ListItem, Value};
pub use std_mail_contract::PAGE;
use std_mail_contract::{
    DeleteLetter, ListMailPage, MAIL_ATTACHMENTS, MAIL_ATTACHMENTS_OFF, MAIL_BAG_FULL, MAIL_INVALID,
    MAIL_NO_GOLD, MAIL_NO_ITEM, MAIL_NOT_FOUND, MAIL_RECIPIENT_FULL, MAIL_TO_SELF, MailPage, MailResult,
    MailText, NewMail, ReadMail, SendMail, TakeMail,
};

/// The module key.
pub const KEY: &str = "std.mail";

/// Requests held as pending at most; past it the oldest is forgotten (its result then
/// counts as stray).
pub const PENDING: usize = 16;

/// The mail screen: the paged inbox, the result line, the reading pane, and the
/// compose form.
pub const MAIL_SCREEN: &str = r#"
panel id=std_mail_panel style=std_mail_panel visible="std.mail.open" enabled="std.mail.enabled" {
  row id=std_mail_header gap=8 {
    text id=std_mail_title template="Mail ({std.mail.page_total} letters)"
    button id=std_mail_refresh text="Refresh" intent="std.mail.list"
  }
  text id=std_mail_unavailable text="Mail unavailable" visible="std.mail.unavailable"
  row id=std_mail_notice visible="std.mail.new_mail" gap=8 {
    text id=std_mail_notice_text template="New mail from {std.mail.new_from}"
    button id=std_mail_notice_ok text="OK" intent="std.mail.dismiss"
  }
  list id=std_mail_inbox bind="std.mail.letters" height=120 clip=true {
    row gap=6 {
      text text="*" visible="item.unread"
      text template="{item.from}"
      text template="{item.subject}"
      text text="(attached)" visible="item.attached"
      button text="Read" intent="std.mail.read" payload="{item.mail}"
    }
  }
  row id=std_mail_pages gap=8 {
    button id=std_mail_prev text="Newer" intent="std.mail.prev_page" visible="std.mail.has_prev"
    text id=std_mail_page_text template="{std.mail.page_first}-{std.mail.page_last} of {std.mail.page_total}"
    button id=std_mail_next text="Older" intent="std.mail.next_page" visible="std.mail.has_next"
  }
  text id=std_mail_result bind="std.mail.result" visible="std.mail.result_shown"
  text id=std_mail_refusal bind="std.mail.refusal"
  panel id=std_mail_reader visible="std.mail.reading" gap=4 {
    text id=std_mail_reader_head template="From {std.mail.read_from}: {std.mail.read_subject}"
    text id=std_mail_reader_body bind="std.mail.read_body"
    text id=std_mail_reader_attached template="Attached: {std.mail.read_gold} gold, item {std.mail.read_item} x{std.mail.read_count}" visible="std.mail.read_attached"
    row id=std_mail_reader_buttons gap=8 {
      button id=std_mail_take text="Take" intent="std.mail.take" visible="std.mail.read_attached"
      button id=std_mail_delete text="Delete" intent="std.mail.delete" visible="std.mail.delete_unarmed"
      button id=std_mail_close text="Close" intent="std.mail.close"
    }
    row id=std_mail_confirm visible="std.mail.confirm_delete" gap=8 {
      text id=std_mail_confirm_text text="Delete this letter?"
      button id=std_mail_confirm_yes text="Delete" intent="std.mail.delete"
      button id=std_mail_confirm_no text="Cancel" intent="std.mail.cancel"
    }
  }
  panel id=std_mail_compose gap=4 {
    text id=std_mail_compose_title template="To {std.mail.draft_to}: {std.mail.draft_subject}"
    input id=std_mail_to placeholder="Recipient" submit="std.mail.to"
    input id=std_mail_subject placeholder="Subject" submit="std.mail.subject" max_length=32
    input id=std_mail_body placeholder="Message" submit="std.mail.body" max_length=200
    row id=std_mail_attach enabled="std.mail.flag.attachments" gap=4 {
      input id=std_mail_gold placeholder="Gold" submit="std.mail.gold" width=60
      input id=std_mail_slot placeholder="Slot" submit="std.mail.slot" width=40
      input id=std_mail_count placeholder="Count" submit="std.mail.count" width=40
    }
    text id=std_mail_attachments_off text="Attachments off" visible="std.mail.attachments_off"
    text id=std_mail_draft_attached template="Attaching {std.mail.draft_gold} gold, slot {std.mail.draft_slot} x{std.mail.draft_count}" visible="std.mail.draft_attached"
    row id=std_mail_compose_buttons gap=8 {
      button id=std_mail_send text="Send" intent="std.mail.send"
      button id=std_mail_discard text="Discard" intent="std.mail.discard"
    }
  }
}
"#;

/// Styles for the mail screen.
pub const THEME: &str = "
theme {
  style std_mail_panel { background = #1a1e26e0 radius = 6 padding = 8 gap = 4 width = 320 }
}
";

/// The client half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Module;

/// One letter on the current page.
#[derive(Clone, Debug, Default)]
struct Entry {
    mail: u32,
    from: u64,
    subject: String,
    read: bool,
    gold: u64,
    item: u32,
    count: u32,
}

impl Entry {
    fn attached(&self) -> bool {
        self.gold > 0 || (self.item != 0 && self.count > 0)
    }
}

/// The letter in the reading pane.
#[derive(Clone, Debug, Default)]
struct Reading {
    mail: u32,
    from: u64,
    subject: String,
    body: String,
    gold: u64,
    item: u32,
    count: u32,
}

/// The letter being written.
#[derive(Clone, Debug, Default)]
struct Draft {
    to: Option<u64>,
    subject: String,
    body: String,
    gold: u64,
    slot: u8,
    count: u32,
}

/// What a pending request asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    Send,
    Take,
    Delete,
}

/// A request waiting for its `MailResult`.
#[derive(Clone, Copy, Debug)]
struct Pending {
    request: u32,
    what: Command,
    /// The letter taken or deleted (0 for a send).
    mail: u32,
}

/// View-model state.
#[derive(Clone, Debug)]
struct Mail {
    /// The current page, newest first; at most [`PAGE`].
    page: Vec<Entry>,
    total: u16,
    offset: u16,
    reading: Option<Reading>,
    draft: Draft,
    /// The letter a delete is armed for.
    armed: Option<u32>,
    open: bool,
    /// The last request number used (0 before the first).
    request: u32,
    /// Oldest first; at most [`PENDING`].
    pending: Vec<Pending>,
    /// Commands whose failed result came and whose refusal has not; at most [`PENDING`].
    awaiting: Vec<Command>,
    /// Commands answered by a refusal alone, should their result still come; at most
    /// [`PENDING`].
    refused: Vec<Pending>,
    stray: u64,
    /// The last answer: success, and its text.
    result: Option<(bool, &'static str)>,
}

impl Default for Mail {
    fn default() -> Self {
        Self {
            page: Vec::with_capacity(PAGE),
            total: 0,
            offset: 0,
            reading: None,
            draft: Draft::default(),
            armed: None,
            open: true,
            request: 0,
            pending: Vec::with_capacity(PENDING),
            awaiting: Vec::with_capacity(PENDING),
            refused: Vec::with_capacity(PENDING),
            stray: 0,
            result: None,
        }
    }
}

impl Mail {
    fn has_next(&self) -> bool {
        usize::from(self.offset) + self.page.len() < usize::from(self.total)
    }

    /// The next request number: never 0, wrapping past `u32::MAX`.
    fn next_request(&mut self) -> u32 {
        self.request = self.request.checked_add(1).unwrap_or(1);
        self.request
    }

    fn await_refusal(&mut self, what: Command) {
        if self.awaiting.len() >= PENDING {
            self.awaiting.remove(0);
        }
        self.awaiting.push(what);
    }

    fn keep_refused(&mut self, pending: Pending) {
        if self.refused.len() >= PENDING {
            self.refused.remove(0);
        }
        self.refused.push(pending);
    }

    fn hold(&mut self, pending: Pending) {
        if self.pending.len() >= PENDING {
            self.pending.remove(0);
        }
        self.pending.push(pending);
    }
}

fn state<'a>(ctx: &'a mut ModuleContext<'_>) -> Result<&'a mut Mail, ModuleError> {
    ctx.state::<Mail>().ok_or(ModuleError::Invalid)
}

fn int<T: TryInto<i64>>(v: T) -> i64 {
    v.try_into().unwrap_or(i64::MAX)
}

fn parse<T: core::str::FromStr>(payload: Option<&str>) -> Result<T, ModuleError> {
    payload
        .and_then(|p| p.trim().parse::<T>().ok())
        .ok_or(ModuleError::Invalid)
}

/// Letters per page as an offset step.
fn step() -> u16 {
    u16::try_from(PAGE).unwrap_or(u16::MAX)
}

/// The confirmation shown for a successful command.
fn success_text(what: Command) -> &'static str {
    match what {
        Command::Send => "Letter sent",
        Command::Take => "Attachments taken",
        Command::Delete => "Letter deleted",
    }
}

/// A readable reason for a `MailResult` failure code.
fn reason_text(reason: u8, what: Command) -> &'static str {
    match reason {
        MAIL_INVALID => "The request was not valid",
        MAIL_NOT_FOUND => "No such letter",
        MAIL_RECIPIENT_FULL => "Recipient's mailbox is full",
        MAIL_NO_GOLD => "Not enough gold for the attachment and postage",
        MAIL_NO_ITEM => "That bag slot does not hold that many",
        MAIL_BAG_FULL => "Your bag cannot hold the attachments",
        MAIL_ATTACHMENTS_OFF => "Attachments are switched off",
        MAIL_ATTACHMENTS => match what {
            Command::Take => "Nothing attached to take",
            Command::Delete => "Take the attachments before deleting",
            Command::Send => "Mail refused",
        },
        MAIL_TO_SELF => "You cannot mail yourself",
        _ => "Mail refused",
    }
}

/// The letter id from the payload, or the letter in the reading pane without one.
fn selected(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<u32, ModuleError> {
    let mail = match payload.filter(|p| !p.trim().is_empty()) {
        Some(p) => parse::<u32>(Some(p))?,
        None => state(ctx)?
            .reading
            .as_ref()
            .map(|r| r.mail)
            .ok_or(ModuleError::Invalid)?,
    };
    if mail == 0 {
        return Err(ModuleError::Invalid);
    }
    Ok(mail)
}

fn publish_page(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let mail = state(ctx)?;
    let unread = mail.page.iter().filter(|e| !e.read).count();
    let (offset, total, len) = (mail.offset, mail.total, mail.page.len());
    let (has_next, has_prev) = (mail.has_next(), offset > 0);
    let items: Vec<ListItem> = mail
        .page
        .iter()
        .map(|e| {
            ListItem::new()
                .with("mail", Value::Int(i64::from(e.mail)))
                .with("from", Value::Text(e.from.to_string()))
                .with("system", Value::Bool(e.from == 0))
                .with("subject", Value::text(&e.subject))
                .with("read", Value::Bool(e.read))
                .with("unread", Value::Bool(!e.read))
                .with("attached", Value::Bool(e.attached()))
                .with("gold", Value::Int(int(e.gold)))
                .with("item", Value::Int(i64::from(e.item)))
                .with("count", Value::Int(i64::from(e.count)))
        })
        .collect();
    let (first, last) = if len == 0 {
        (0, 0)
    } else {
        (i64::from(offset) + 1, i64::from(offset) + int(len))
    };
    ctx.set_int("count", int(len));
    ctx.set_int("unread_count", int(unread));
    ctx.set_int("page_offset", i64::from(offset));
    ctx.set_int("page_total", i64::from(total));
    ctx.set_int("page_first", first);
    ctx.set_int("page_last", last);
    ctx.set_bool("has_next", has_next);
    ctx.set_bool("has_prev", has_prev);
    ctx.set("letters", Value::List(items));
    Ok(())
}

fn publish_reader(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let r = state(ctx)?.reading.clone();
    let r_or = r.clone().unwrap_or_default();
    ctx.set_bool("reading", r.is_some());
    ctx.set_int("read_mail", i64::from(r_or.mail));
    ctx.set_text(
        "read_from",
        &r.as_ref().map(|r| r.from.to_string()).unwrap_or_default(),
    );
    ctx.set_text("read_subject", &r_or.subject);
    ctx.set_text("read_body", &r_or.body);
    ctx.set_int("read_gold", int(r_or.gold));
    ctx.set_int("read_item", i64::from(r_or.item));
    ctx.set_int("read_count", i64::from(r_or.count));
    ctx.set_bool(
        "read_attached",
        r_or.gold > 0 || (r_or.item != 0 && r_or.count > 0),
    );
    Ok(())
}

fn publish_armed(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let armed = state(ctx)?.armed;
    ctx.set_bool("confirm_delete", armed.is_some());
    ctx.set_bool("delete_unarmed", armed.is_none());
    ctx.set_int("delete_mail", i64::from(armed.unwrap_or(0)));
    Ok(())
}

fn publish_draft(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let d = state(ctx)?.draft.clone();
    ctx.set_text("draft_to", &d.to.map(|t| t.to_string()).unwrap_or_default());
    ctx.set_text("draft_subject", &d.subject);
    ctx.set_text("draft_body", &d.body);
    ctx.set_int("draft_gold", int(d.gold));
    ctx.set_int("draft_slot", i64::from(d.slot));
    ctx.set_int("draft_count", i64::from(d.count));
    ctx.set_bool("draft_attached", d.gold > 0 || d.count > 0);
    Ok(())
}

fn publish_result(ctx: &mut ModuleContext<'_>) -> Result<(), ModuleError> {
    let mail = state(ctx)?;
    let result = mail.result;
    let (pending, stray) = (mail.pending.len(), mail.stray);
    ctx.set_text("result", result.map_or("", |r| r.1));
    ctx.set_bool("result_ok", result.is_some_and(|r| r.0));
    ctx.set_bool("result_shown", result.is_some());
    ctx.set_int("pending", int(pending));
    ctx.set_int("stray_results", int(stray));
    Ok(())
}

fn ask_page(ctx: &mut ModuleContext<'_>, offset: u16) -> Result<(), ModuleError> {
    ctx.send(&ListMailPage { offset })
}

/// Sends one command and holds it as pending; the previous result line clears.
fn command<M: Message>(
    ctx: &mut ModuleContext<'_>,
    what: Command,
    mail: u32,
    build: impl FnOnce(u32) -> M,
) -> Result<(), ModuleError> {
    let request = state(ctx)?.next_request();
    ctx.send(&build(request))?;
    let m = state(ctx)?;
    m.hold(Pending { request, what, mail });
    m.result = None;
    publish_result(ctx)
}

fn on_mail_page(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: MailPage = decode(payload)?;
    let mail = state(ctx)?;
    mail.page.clear();
    mail.page.extend(m.entries.iter().take(PAGE).map(|e| Entry {
        mail: e.mail,
        from: e.from,
        subject: e.subject.as_str().to_owned(),
        read: e.read,
        gold: e.gold,
        item: e.item,
        count: e.count,
    }));
    mail.total = m.total;
    mail.offset = m.offset;
    let past_end = mail.page.is_empty() && m.total > 0 && m.offset >= m.total;
    publish_page(ctx)?;
    if past_end {
        // Letters went away under this page: ask for the last one (a strictly smaller
        // offset, so this cannot repeat). A full queue drops it; `std.mail.list` asks again.
        let last = (m.total - 1) / step() * step();
        let _ = ctx.send(&ListMailPage { offset: last });
    }
    Ok(())
}

fn on_mail_text(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: MailText = decode(payload)?;
    let mail = state(ctx)?;
    if let Some(e) = mail.page.iter_mut().find(|e| e.mail == m.mail) {
        e.read = true;
        m.subject.as_str().clone_into(&mut e.subject);
        e.gold = m.gold;
        e.item = m.item;
        e.count = m.count;
    }
    if mail.armed != Some(m.mail) {
        mail.armed = None;
    }
    mail.reading = Some(Reading {
        mail: m.mail,
        from: m.from,
        subject: m.subject.as_str().to_owned(),
        body: m.body.as_str().to_owned(),
        gold: m.gold,
        item: m.item,
        count: m.count,
    });
    publish_page(ctx)?;
    publish_armed(ctx)?;
    publish_reader(ctx)
}

fn on_new_mail(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let m: NewMail = decode(payload)?;
    let offset = state(ctx)?.offset;
    ctx.set_bool("new_mail", true);
    ctx.set_text("new_from", &m.from.to_string());
    // A full queue drops the refresh; `std.mail.list` asks again.
    let _ = ctx.send(&ListMailPage { offset });
    Ok(())
}

/// Applies what a confirmed command did to the reading pane and the draft.
fn confirmed(mail: &mut Mail, p: Pending) {
    match p.what {
        Command::Send => mail.draft = Draft::default(),
        Command::Take => {
            if let Some(rd) = mail.reading.as_mut().filter(|rd| rd.mail == p.mail) {
                rd.gold = 0;
                rd.item = 0;
                rd.count = 0;
            }
        }
        Command::Delete => {
            if mail.reading.as_ref().is_some_and(|rd| rd.mail == p.mail) {
                mail.reading = None;
            }
            if mail.armed == Some(p.mail) {
                mail.armed = None;
            }
        }
    }
}

fn on_mail_result(ctx: &mut ModuleContext<'_>, _kind: u16, payload: &[u8]) -> Result<(), ModuleError> {
    let r: MailResult = decode(payload)?;
    let mail = state(ctx)?;
    let p = if let Some(at) = mail.pending.iter().position(|p| p.request == r.request) {
        let p = mail.pending.remove(at);
        if !r.ok {
            // Its refusal follows; the result already said it.
            mail.await_refusal(p.what);
        }
        p
    } else if let Some(at) = mail.refused.iter().position(|p| p.request == r.request) {
        // Answered by its refusal already; the result says why.
        mail.refused.remove(at)
    } else {
        mail.stray += 1;
        return publish_result(ctx);
    };
    let text = if r.ok {
        confirmed(mail, p);
        success_text(p.what)
    } else {
        reason_text(r.reason, p.what)
    };
    mail.result = Some((r.ok, text));
    publish_result(ctx)?;
    publish_draft(ctx)?;
    publish_armed(ctx)?;
    publish_reader(ctx)
}

/// Answers refusals of the requested commands (see the crate docs); other kinds go to
/// `std.mail.refusal`.
fn on_refused(ctx: &mut ModuleContext<'_>, kind: u16, _request: u32, reason: ExtensionRefusal) -> bool {
    let what = match kind {
        k if k == SendMail::ID.0 => Command::Send,
        k if k == TakeMail::ID.0 => Command::Take,
        k if k == DeleteLetter::ID.0 => Command::Delete,
        _ => return false,
    };
    let Ok(mail) = state(ctx) else {
        return false;
    };
    if let Some(at) = mail.awaiting.iter().position(|w| *w == what) {
        mail.awaiting.remove(at);
        return true;
    }
    if let Some(at) = mail.pending.iter().position(|p| p.what == what) {
        let p = mail.pending.remove(at);
        mail.keep_refused(p);
        let text = if reason == ExtensionRefusal::Invalid {
            reason_text(MAIL_INVALID, what)
        } else {
            "Not allowed"
        };
        mail.result = Some((false, text));
    } else {
        mail.stray += 1;
    }
    let _ = publish_result(ctx);
    true
}

fn on_list(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let offset = state(ctx)?.offset;
    ask_page(ctx, offset)
}

fn on_next_page(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let mail = state(ctx)?;
    if !mail.has_next() {
        return Err(ModuleError::Invalid);
    }
    let offset = mail.offset.checked_add(step()).ok_or(ModuleError::Invalid)?;
    ask_page(ctx, offset)
}

fn on_prev_page(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let offset = state(ctx)?.offset;
    if offset == 0 {
        return Err(ModuleError::Invalid);
    }
    ask_page(ctx, offset.saturating_sub(step()))
}

fn on_read(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let mail = parse::<u32>(payload)?;
    if mail == 0 {
        return Err(ModuleError::Invalid);
    }
    ctx.send(&ReadMail { mail })
}

fn on_close(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let mail = state(ctx)?;
    mail.reading = None;
    mail.armed = None;
    publish_armed(ctx)?;
    publish_reader(ctx)
}

fn on_dismiss(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?;
    ctx.set_bool("new_mail", false);
    ctx.set_text("new_from", "");
    Ok(())
}

fn on_to(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let to = parse::<u64>(payload)?;
    if to == 0 {
        return Err(ModuleError::Invalid);
    }
    state(ctx)?.draft.to = Some(to);
    publish_draft(ctx)
}

fn on_subject(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let subject = payload
        .filter(|s| !s.trim().is_empty() && WireString::<32>::new(s).is_some())
        .ok_or(ModuleError::Invalid)?;
    subject.clone_into(&mut state(ctx)?.draft.subject);
    publish_draft(ctx)
}

fn on_body(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let body = payload.unwrap_or("");
    if WireString::<200>::new(body).is_none() {
        return Err(ModuleError::Invalid);
    }
    body.clone_into(&mut state(ctx)?.draft.body);
    publish_draft(ctx)
}

fn attachments_on(ctx: &ModuleContext<'_>) -> Result<(), ModuleError> {
    if ctx.flag("attachments") {
        Ok(())
    } else {
        Err(ModuleError::Invalid)
    }
}

fn on_gold(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    attachments_on(ctx)?;
    state(ctx)?.draft.gold = parse(payload)?;
    publish_draft(ctx)
}

fn on_slot(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    attachments_on(ctx)?;
    state(ctx)?.draft.slot = parse(payload)?;
    publish_draft(ctx)
}

fn on_count(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    attachments_on(ctx)?;
    state(ctx)?.draft.count = parse(payload)?;
    publish_draft(ctx)
}

fn on_discard(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?.draft = Draft::default();
    publish_draft(ctx)
}

fn on_send(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    let d = state(ctx)?.draft.clone();
    let to = d.to.ok_or(ModuleError::Invalid)?;
    if d.subject.trim().is_empty() {
        return Err(ModuleError::Invalid);
    }
    let has_item = d.count > 0;
    if d.gold > 0 || has_item {
        attachments_on(ctx)?;
    }
    let subject = WireString::new(&d.subject).ok_or(ModuleError::Invalid)?;
    let body = WireString::new(&d.body).ok_or(ModuleError::Invalid)?;
    // The draft stays until the result confirms the letter went.
    command(ctx, Command::Send, 0, |request| SendMail {
        request,
        to,
        subject,
        body,
        gold: d.gold,
        has_item,
        slot: if has_item { d.slot } else { 0 },
        count: d.count,
    })
}

fn on_take(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let mail = selected(ctx, payload)?;
    command(ctx, Command::Take, mail, |request| TakeMail { request, mail })
}

fn on_delete(ctx: &mut ModuleContext<'_>, payload: Option<&str>) -> Result<(), ModuleError> {
    let mail = selected(ctx, payload)?;
    if state(ctx)?.armed != Some(mail) {
        state(ctx)?.armed = Some(mail);
        return publish_armed(ctx);
    }
    command(ctx, Command::Delete, mail, |request| DeleteLetter {
        request,
        mail,
    })?;
    state(ctx)?.armed = None;
    publish_armed(ctx)
}

fn on_cancel(ctx: &mut ModuleContext<'_>, _payload: Option<&str>) -> Result<(), ModuleError> {
    state(ctx)?.armed = None;
    publish_armed(ctx)
}

fn on_toggle(ctx: &mut ModuleContext<'_>) {
    let open = match state(ctx) {
        Ok(m) => {
            m.open = !m.open;
            m.open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
}

/// Publishes every starting value (resetting the view model, but never the request
/// counter, so a late result cannot match a new request), and when enabled (including
/// at start) asks for the first page.
fn on_enabled(ctx: &mut ModuleContext<'_>, enabled: bool) {
    let open = match state(ctx) {
        Ok(m) => {
            let (open, request) = (m.open, m.request);
            *m = Mail {
                open,
                request,
                ..Mail::default()
            };
            open
        }
        Err(_) => false,
    };
    ctx.set_bool("open", open);
    ctx.set_bool("new_mail", false);
    ctx.set_text("new_from", "");
    let off = !ctx.flag("attachments");
    ctx.set_bool("attachments_off", off);
    let _ = publish_page(ctx);
    let _ = publish_reader(ctx);
    let _ = publish_armed(ctx);
    let _ = publish_draft(ctx);
    let _ = publish_result(ctx);
    if enabled {
        // A full queue drops the ask; `std.mail.list` asks again.
        let _ = ctx.send(&ListMailPage { offset: 0 });
    }
}

impl ClientModule for Module {
    fn key(&self) -> &'static str {
        KEY
    }

    fn register(&self, r: &mut ClientRegistrar<'_>) -> Result<(), ClientRegistryError> {
        r.kinds(1030..=1039)?;
        r.kinds(1130..=1139)?;
        r.state(Mail::default());
        r.on_message(MailPage::ID.0, on_mail_page)?;
        r.on_message(MailText::ID.0, on_mail_text)?;
        r.on_message(NewMail::ID.0, on_new_mail)?;
        r.on_message(MailResult::ID.0, on_mail_result)?;
        r.on_refused(on_refused);
        let intents: [(&'static str, IntentFn); 17] = [
            ("std.mail.list", on_list),
            ("std.mail.next_page", on_next_page),
            ("std.mail.prev_page", on_prev_page),
            ("std.mail.read", on_read),
            ("std.mail.close", on_close),
            ("std.mail.dismiss", on_dismiss),
            ("std.mail.to", on_to),
            ("std.mail.subject", on_subject),
            ("std.mail.body", on_body),
            ("std.mail.gold", on_gold),
            ("std.mail.slot", on_slot),
            ("std.mail.count", on_count),
            ("std.mail.discard", on_discard),
            ("std.mail.send", on_send),
            ("std.mail.take", on_take),
            ("std.mail.delete", on_delete),
            ("std.mail.cancel", on_cancel),
        ];
        for (name, run) in intents {
            r.on_intent(name, run)?;
        }
        r.on_enabled(on_enabled);
        r.screen("std.mail.window", MAIL_SCREEN, Some(THEME))?;
        r.action("std.mail.toggle", Some(KeyCode::M), on_toggle)?;
        Ok(())
    }
}
