//! std.auction, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::time::Tick;
use mantis_core::wire::{BoundedArray, Message, MessageId, ValidationError, WireString, encode_into};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, Payload, Registrar, RegistryError,
    Require, ServerModule, SystemOutcomes, caller, character_of, decode, emit_outcome, outcome_room, tell,
    tell_character,
};
use std_auction_contract::{
    AUCTION_INVALID, AUCTION_MAILBOX_FULL, AUCTION_NO_GOLD, AUCTION_NO_ITEM, AUCTION_NOT_FOUND,
    AUCTION_NOT_YOURS, AUCTION_OK, AUCTION_OWN_LISTING, AUCTION_TOO_MANY, AuctionResult, BuyLot, CancelLot,
    LotEntry, LotPage, LotPageOf, PAGE, PostLot, RULES_TABLE, SearchLots,
};
use std_containers_contract::{EconomyError, Inventories, Ledger, Stack};
use std_mail_contract::Mailboxes;

/// `percent` of `amount`, rounded down, or `None` when it does not fit.
fn percent_of(amount: u64, percent: u64) -> Option<u64> {
    u128::from(amount)
        .checked_mul(u128::from(percent))
        .and_then(|v| u64::try_from(v / 100).ok())
}

/// Auction rules from content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rules {
    /// How long a listing stays up, in seconds.
    pub duration_seconds: u64,
    /// What the house keeps of a sale, in percent.
    pub cut_percent: u64,
    /// Listings one seller may have up.
    pub max_listings: usize,
    /// The deposit, in percent of the price, paid when posting: returned
    /// with the proceeds of a sale, kept by the house on a withdrawal or an
    /// expiry.
    pub deposit_percent: u64,
}

impl Rules {
    /// Defaults for keys a table leaves out.
    pub const DEFAULT: Self = Self {
        duration_seconds: 24 * 3600,
        cut_percent: 5,
        max_listings: 20,
        deposit_percent: 0,
    };

    /// The deposit for a listing at `price`, or `None` when it does not fit.
    #[must_use]
    pub fn deposit(&self, price: u64) -> Option<u64> {
        percent_of(price, self.deposit_percent)
    }

    /// Parses `key value` lines (`#` comments allowed).
    ///
    /// # Errors
    /// The first malformed or unknown line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut r = Self::DEFAULT;
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once(char::is_whitespace)
                .ok_or(format!("line {}: expected `key value`", n + 1))?;
            let v: u64 = value
                .trim()
                .parse()
                .map_err(|_| format!("line {}: not a number", n + 1))?;
            match key {
                "duration_seconds" if v > 0 => r.duration_seconds = v,
                "cut_percent" if v <= 100 => r.cut_percent = v,
                "max_listings" => r.max_listings = usize::try_from(v).map_err(|_| "range".to_owned())?,
                "deposit_percent" if v <= 100 => r.deposit_percent = v,
                _ => return Err(format!("line {}: unknown key or bad value", n + 1)),
            }
        }
        Ok(r)
    }
}

/// One listing; its items are held in escrow here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Listing {
    /// The seller.
    pub seller: u64,
    /// The lot.
    pub lot: Stack,
    /// Its price.
    pub price: u64,
    /// The deposit the seller paid.
    pub deposit: u64,
    /// When it comes down.
    pub expires: Tick,
}

/// The house. Simulation state.
#[derive(Debug)]
pub struct House {
    next: u32,
    /// By id.
    pub listings: BTreeMap<u32, Listing>,
    /// The rules (content).
    pub rules: Rules,
}

impl StateHash for House {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.next);
        h.write_u64(self.listings.len() as u64);
        for (id, l) in &self.listings {
            h.write_u32(*id);
            h.write_u64(l.seller);
            h.write_u32(l.lot.item);
            h.write_u32(l.lot.count);
            h.write_u64(l.price);
            h.write_u64(l.deposit);
            h.write_u64(l.expires.0);
        }
    }
}

impl Resource for House {
    const NAME: &'static str = "std.auction.house";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(self.next);
        e.u32(u32::try_from(self.listings.len()).unwrap_or(u32::MAX));
        for (id, l) in &self.listings {
            e.u32(*id);
            e.u64(l.seller);
            e.u32(l.lot.item);
            e.u32(l.lot.count);
            e.u64(l.price);
            e.u64(l.deposit);
            e.u64(l.expires.0);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(&mut self, d: &mut mantis_core::wire::Decoder<'_>) -> Result<(), mantis_core::wire::DecodeError> {
        self.next = d.u32()?;
        self.listings.clear();
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            let l = Listing {
                seller: d.u64()?,
                lot: Stack {
                    item: d.u32()?,
                    count: d.u32()?,
                },
                price: d.u64()?,
                deposit: d.u64()?,
                expires: Tick(d.u64()?),
            };
            self.listings.insert(id, l);
        }
        Ok(())
    }
}

struct Checks;

impl std_auction_contract::Validators for Checks {
    fn validate_post_lot(&self, msg: &PostLot) -> Result<(), ValidationError> {
        if msg.count == 0 || msg.price == 0 {
            return Err(ValidationError("empty lot or free"));
        }
        Ok(())
    }
    fn validate_buy_lot(&self, _msg: &BuyLot) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_cancel_lot(&self, _msg: &CancelLot) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_search_lots(&self, _msg: &SearchLots) -> Result<(), ValidationError> {
        Ok(())
    }
}

fn check(kind: u16, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    std_auction_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| ExtensionRefusal::Invalid)
}

/// Why a command failed: the refusal for the client, and the
/// `AuctionResult` reason.
type Failed = (ExtensionRefusal, u8);

const INVALID: Failed = (ExtensionRefusal::Invalid, AUCTION_INVALID);

fn no(reason: u8) -> Failed {
    (ExtensionRefusal::NotAllowed, reason)
}

fn economy(e: EconomyError, insufficient: u8) -> Failed {
    match e {
        EconomyError::Overflow => INVALID,
        EconomyError::Full | EconomyError::BadSlot => no(AUCTION_NO_ITEM),
        EconomyError::Insufficient => no(insufficient),
    }
}

fn subject(text: &str) -> WireString<32> {
    WireString::new(text).unwrap_or_default()
}

fn body(text: &str) -> WireString<200> {
    WireString::new(text).unwrap_or_default()
}

/// What a successful command moved: its ledger, the listing, the gold.
type Done = (Ledger, u32, u64);

fn post_lot(
    world: &mut World,
    ctx: &TickContext,
    me: u64,
    slot: u8,
    count: u32,
    price: u64,
) -> Result<Done, Failed> {
    let house = world.resource::<House>().ok_or(INVALID)?;
    let mine = house.listings.values().filter(|l| l.seller == me).count();
    if mine >= house.rules.max_listings {
        return Err(no(AUCTION_TOO_MANY));
    }
    // Checked: content or a client must never wrap an economy number.
    let expires = house
        .rules
        .duration_seconds
        .checked_mul(u64::from(ctx.rate.hz()))
        .and_then(|d| ctx.tick.0.checked_add(d))
        .ok_or(INVALID)?;
    let deposit = house.rules.deposit(price).ok_or(INVALID)?;
    let mut lot = None;
    let inv = world.resource_mut::<Inventories>().ok_or(INVALID)?;
    let short = if inv.gold(me) < deposit {
        AUCTION_NO_GOLD
    } else {
        AUCTION_NO_ITEM
    };
    let ledger = inv
        .transaction(&[me], |tx| {
            if deposit > 0 {
                tx.debit(me, deposit)?;
            }
            lot = Some(tx.take(me, slot, count)?);
            Ok(())
        })
        .map_err(|e| economy(e, short))?;
    let lot = lot.ok_or(INVALID)?;
    let house = world.resource_mut::<House>().ok_or(INVALID)?;
    house.next += 1;
    let id = house.next;
    house.listings.insert(
        id,
        Listing {
            seller: me,
            lot,
            price,
            deposit,
            expires: Tick(expires),
        },
    );
    Ok((ledger, id, deposit))
}

fn buy_lot(world: &mut World, me: u64, listing: u32) -> Result<Done, Failed> {
    let house = world.resource::<House>().ok_or(INVALID)?;
    let l = *house.listings.get(&listing).ok_or(no(AUCTION_NOT_FOUND))?;
    // Checked: the cut, and the seller's proceeds with the deposit, must
    // fit before anything moves; a lot that would overflow is refused.
    let cut = percent_of(l.price, house.rules.cut_percent).ok_or(INVALID)?;
    let proceeds = (l.price - cut).checked_add(l.deposit).ok_or(INVALID)?;
    if l.seller == me {
        return Err(no(AUCTION_OWN_LISTING));
    }
    let boxes = world.resource::<Mailboxes>().ok_or(INVALID)?;
    if !boxes.has_room(me) || !boxes.has_room(l.seller) {
        return Err(no(AUCTION_MAILBOX_FULL));
    }
    let ledger = world
        .resource_mut::<Inventories>()
        .ok_or(INVALID)?
        .transaction(&[me], |tx| tx.debit(me, l.price))
        .map_err(|e| economy(e, AUCTION_NO_GOLD))?;
    world
        .resource_mut::<House>()
        .ok_or(INVALID)?
        .listings
        .remove(&listing);
    let boxes = world.resource_mut::<Mailboxes>().ok_or(INVALID)?;
    boxes
        .deliver(
            me,
            0,
            subject("Auction won"),
            body("Your purchase."),
            0,
            Some(l.lot),
        )
        .map_err(|_| no(AUCTION_MAILBOX_FULL))?;
    boxes
        .deliver(
            l.seller,
            0,
            subject("Auction sold"),
            body("Your sale, less the house cut, with your deposit."),
            proceeds,
            None,
        )
        .map_err(|_| no(AUCTION_MAILBOX_FULL))?;
    Ok((ledger, listing, l.price))
}

fn cancel_lot(world: &mut World, me: u64, listing: u32) -> Result<Done, Failed> {
    let l = world
        .resource::<House>()
        .and_then(|h| h.listings.get(&listing).copied())
        .ok_or(no(AUCTION_NOT_FOUND))?;
    if l.seller != me {
        return Err(no(AUCTION_NOT_YOURS));
    }
    let boxes = world.resource_mut::<Mailboxes>().ok_or(INVALID)?;
    boxes
        .deliver(
            me,
            0,
            subject("Auction cancelled"),
            body("Your items."),
            0,
            Some(l.lot),
        )
        .map_err(|_| no(AUCTION_MAILBOX_FULL))?;
    world
        .resource_mut::<House>()
        .ok_or(INVALID)?
        .listings
        .remove(&listing);
    Ok((Ledger::default(), listing, 0))
}

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

fn seller(world: &World, cmd: &ModuleCommand) -> Option<u64> {
    cmd.session.and_then(|s| character_of(world, s))
}

/// One page of listings: of `item` (0 any), only `seller`'s if given.
fn page(world: &World, now: Tick, rate: u32, item: u32, seller: Option<u64>, offset: u16) -> LotPage {
    let Some(house) = world.resource::<House>() else {
        return LotPage {
            total: 0,
            offset,
            cut_percent: 0,
            deposit_percent: 0,
            duration_seconds: 0,
            entries: BoundedArray::default(),
        };
    };
    let mut found: Vec<(u32, Listing)> = house
        .listings
        .iter()
        .filter(|(_, l)| (item == 0 || l.lot.item == item) && seller.is_none_or(|s| l.seller == s))
        .map(|(id, l)| (*id, *l))
        .collect();
    found.sort_by_key(|(id, l)| (l.price, *id));
    let entries: Vec<LotEntry> = found
        .iter()
        .skip(usize::from(offset))
        .take(PAGE)
        .map(|(id, l)| LotEntry {
            listing: *id,
            seller: l.seller,
            item: l.lot.item,
            count: l.lot.count,
            price: l.price,
            deposit: l.deposit,
            expires_in: u32::try_from(l.expires.0.saturating_sub(now.0) / u64::from(rate.max(1)))
                .unwrap_or(u32::MAX),
        })
        .collect();
    LotPage {
        total: u16::try_from(found.len()).unwrap_or(u16::MAX),
        offset,
        cut_percent: u8::try_from(house.rules.cut_percent).unwrap_or(100),
        deposit_percent: u8::try_from(house.rules.deposit_percent).unwrap_or(100),
        duration_seconds: u32::try_from(house.rules.duration_seconds).unwrap_or(u32::MAX),
        entries: BoundedArray::from_slice(&entries).unwrap_or_default(),
    }
}

/// Runs a requested command: the result and a fresh page of the
/// requester's listings go to them, the outcome to the log.
fn answered(
    world: &mut World,
    ctx: &TickContext,
    cmd: &ModuleCommand,
    run: impl FnOnce(&mut World, u64, &[u8]) -> Result<(u32, Result<Done, Failed>), Failed>,
) -> ModuleOutcome {
    let Some(me) = seller(world, cmd) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::NotAllowed);
    };
    let (request, result) = match run(world, me, cmd.payload.as_slice()) {
        Ok(r) => r,
        Err((refusal, _)) => return ModuleOutcome::refused(cmd, refusal),
    };
    let (reply, outcome) = match result {
        Ok((ledger, listing, gold)) => (
            AuctionResult {
                request,
                ok: true,
                reason: AUCTION_OK,
                listing,
                gold,
            },
            finish(world, cmd, me, &Ok(ledger)),
        ),
        Err((refusal, reason)) => (
            AuctionResult {
                request,
                ok: false,
                reason,
                listing: 0,
                gold: 0,
            },
            ModuleOutcome::refused(cmd, refusal),
        ),
    };
    tell_character(world, me, &reply);
    let mine = page(world, ctx.tick, ctx.rate.hz(), 0, Some(me), 0);
    let of = LotPageOf {
        item: 0,
        mine: true,
        offset: 0,
    };
    tell_character(world, me, &of);
    tell_character(world, me, &mine);
    outcome
}

fn post_requested(world: &mut World, ctx: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, ctx, cmd, |world, me, payload| {
        check(PostLot::ID.0, payload).map_err(|_| INVALID)?;
        let msg: PostLot = decode(payload).map_err(|_| INVALID)?;
        Ok((
            msg.request,
            post_lot(world, ctx, me, msg.slot, msg.count, msg.price),
        ))
    })
}

fn buy_requested(world: &mut World, ctx: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, ctx, cmd, |world, me, payload| {
        check(BuyLot::ID.0, payload).map_err(|_| INVALID)?;
        let msg: BuyLot = decode(payload).map_err(|_| INVALID)?;
        Ok((msg.request, buy_lot(world, me, msg.listing)))
    })
}

fn cancel_requested(world: &mut World, ctx: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    answered(world, ctx, cmd, |world, me, payload| {
        check(CancelLot::ID.0, payload).map_err(|_| INVALID)?;
        let msg: CancelLot = decode(payload).map_err(|_| INVALID)?;
        Ok((msg.request, cancel_lot(world, me, msg.listing)))
    })
}

fn search_lots(
    world: &mut World,
    ctx: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(SearchLots::ID.0, payload)?;
    let msg: SearchLots = decode(payload)?;
    let me = caller(world, session)?;
    let reply = page(
        world,
        ctx.tick,
        ctx.rate.hz(),
        msg.item,
        msg.mine.then_some(me),
        msg.offset,
    );
    let of = LotPageOf {
        item: msg.item,
        mine: msg.mine,
        offset: msg.offset,
    };
    tell(world, session, &of);
    tell(world, session, &reply);
    Ok(())
}

/// Returns expired lots to their sellers by mail (retried while a seller's
/// mailbox is full).
fn expire(world: &mut World, ctx: &TickContext) -> Result<(), SystemError> {
    let due: Vec<(u32, Listing)> = world
        .resource::<House>()
        .map(|h| {
            h.listings
                .iter()
                .filter(|(_, l)| l.expires <= ctx.tick)
                .map(|(id, l)| (*id, *l))
                .collect()
        })
        .unwrap_or_default();
    for (id, l) in due {
        // The return is recorded as an outcome in this tick (lead ruling,
        // M4); with no room it waits for the next tick.
        if !outcome_room(world) {
            break;
        }
        let Some(boxes) = world.resource_mut::<Mailboxes>() else {
            return Err(SystemError::Invariant("mailboxes"));
        };
        if boxes
            .deliver(
                l.seller,
                0,
                subject("Auction expired"),
                body("Your unsold items."),
                0,
                Some(l.lot),
            )
            .is_ok()
            && let Some(h) = world.resource_mut::<House>()
        {
            h.listings.remove(&id);
            emit_outcome(world, expired(id, &l))?;
        }
    }
    Ok(())
}

/// The outcome of an expiry: the house cancelling the listing for its
/// seller. Its ledger is empty (the lot moved from the house to the
/// seller's mail, not into a bag); the listing id follows it.
fn expired(id: u32, l: &Listing) -> ModuleOutcome {
    let mut bytes = Vec::new();
    encode_into(&Ledger::default(), &mut bytes);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&l.seller.to_le_bytes());
    ModuleOutcome {
        kind: ExtensionKind(CancelLot::ID.0),
        session: None,
        result: Ok(()),
        payload: Payload::from_slice(&bytes).unwrap_or(Payload::EMPTY),
    }
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.auction"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let table = r.table(RULES_TABLE)?;
        let text =
            core::str::from_utf8(&table).map_err(|_| RegistryError::Module("rules table is not UTF-8"))?;
        let rules = Rules::parse(text).map_err(|_| RegistryError::Module("malformed auction rules"))?;
        r.resource(House {
            next: 0,
            listings: BTreeMap::new(),
            rules,
        })?;
        r.command(ExtensionKind(PostLot::ID.0), post_requested)?;
        r.command(ExtensionKind(BuyLot::ID.0), buy_requested)?;
        r.command(ExtensionKind(CancelLot::ID.0), cancel_requested)?;
        r.handler(ExtensionKind(SearchLots::ID.0), Require::Joined, search_lots)?;
        let access = r
            .access()
            .write_resource::<House>()
            .write_resource::<Mailboxes>()
            .write_resource::<SystemOutcomes>()
            .build()?;
        r.system(
            SystemDesc {
                name: "std.auction.expire",
                phase: Phase::Timers,
                priority: -10,
                access,
            },
            expire,
        )?;
        Ok(())
    }
}
