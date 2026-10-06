//! std.vendor, server half. See `RULES.md` beside the manifest.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::schedule::TickContext;
use mantis_core::wire::{BoundedArray, Message, MessageId, ValidationError};
use mantis_server::modules::{
    ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, Registrar, RegistryError, Require,
    ServerModule, caller, character_of, decode, tell, tell_character,
};
use std_containers_contract::{EconomyError, Inventories, Ledger, MAX_STACK};
use std_vendor_contract::{
    Browse, BuyItems, SELL_DIVISOR, STOCK_TABLE, SellItems, Stock, TRADE_BAG_FULL, TRADE_INSUFFICIENT,
    TRADE_MALFORMED, TRADE_NOT_BOUGHT, TRADE_NOT_LISTED, TRADE_NOTHING, TRADE_OK, TRADE_SELLING_OFF, Traded,
};

/// Parsed vendor stock (content: covered by the content hash).
#[derive(Debug, Default)]
pub struct Vendors {
    /// Vendor -> item -> price.
    pub stock: BTreeMap<u32, BTreeMap<u32, u64>>,
}

impl Vendors {
    /// Parses the stock table.
    ///
    /// # Errors
    /// The first malformed line.
    pub fn parse(text: &str) -> Result<BTreeMap<u32, BTreeMap<u32, u64>>, String> {
        let mut stock: BTreeMap<u32, BTreeMap<u32, u64>> = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut f = line.split_whitespace();
            let (Some(v), Some(i), Some(p), None) = (f.next(), f.next(), f.next(), f.next()) else {
                return Err(format!("line {}: expected `vendor item price`", n + 1));
            };
            let parse = |s: &str| {
                s.parse::<u64>()
                    .map_err(|_| format!("line {}: not a number", n + 1))
            };
            let (v, i, p) = (parse(v)?, parse(i)?, parse(p)?);
            let (Ok(v), Ok(i)) = (u32::try_from(v), u32::try_from(i)) else {
                return Err(format!("line {}: id out of range", n + 1));
            };
            if i == 0 || p == 0 {
                return Err(format!("line {}: item and price must be positive", n + 1));
            }
            stock.entry(v).or_default().insert(i, p);
        }
        Ok(stock)
    }

    /// What a vendor pays for one `item`: the best listed price over the
    /// divisor, if anyone lists it.
    #[must_use]
    pub fn buy_back(&self, item: u32) -> Option<u64> {
        self.stock
            .values()
            .filter_map(|s| s.get(&item))
            .max()
            .map(|p| p / SELL_DIVISOR)
            .filter(|p| *p > 0)
    }
}

/// Explicitly not hashed: stock is content, covered by the content hash.
impl StateHash for Vendors {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for Vendors {
    const NAME: &'static str = "std.vendor.stock";

    /// Content, parsed at installation.
    fn save(&self, _e: &mut mantis_core::wire::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

struct Checks;

impl std_vendor_contract::Validators for Checks {
    fn validate_browse(&self, _msg: &Browse) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_buy_items(&self, msg: &BuyItems) -> Result<(), ValidationError> {
        if msg.count == 0 || msg.count > MAX_STACK {
            return Err(ValidationError("count out of range"));
        }
        Ok(())
    }
    fn validate_sell_items(&self, msg: &SellItems) -> Result<(), ValidationError> {
        if msg.count == 0 {
            return Err(ValidationError("nothing to sell"));
        }
        Ok(())
    }
}

fn check(kind: u16, payload: &[u8]) -> Result<(), Fail> {
    std_vendor_contract::parse_inbound(MessageId(kind), payload)
        .and_then(|m| m.validate(&Checks))
        .map_err(|_| MALFORMED)
}

/// Why a trade failed: the refusal and the [`TradeResult`] reason code.
type Fail = (ExtensionRefusal, u8);

const MALFORMED: Fail = (ExtensionRefusal::Invalid, TRADE_MALFORMED);
const NOT_LISTED: Fail = (ExtensionRefusal::NotAllowed, TRADE_NOT_LISTED);
const NOTHING: Fail = (ExtensionRefusal::NotAllowed, TRADE_NOTHING);
const NOT_BOUGHT: Fail = (ExtensionRefusal::NotAllowed, TRADE_NOT_BOUGHT);
const SELLING_OFF: Fail = (ExtensionRefusal::NotAllowed, TRADE_SELLING_OFF);

fn economy(e: EconomyError) -> Fail {
    match e {
        EconomyError::Insufficient => (ExtensionRefusal::NotAllowed, TRADE_INSUFFICIENT),
        EconomyError::Full => (ExtensionRefusal::NotAllowed, TRADE_BAG_FULL),
        EconomyError::BadSlot => NOTHING,
        EconomyError::Overflow => MALFORMED,
    }
}

fn actor(world: &World, cmd: &ModuleCommand) -> Option<u64> {
    cmd.session.and_then(|s| character_of(world, s))
}

/// A completed trade: its ledger, the item, and the gold moved.
type Trade = (Ledger, u32, u64);

fn buy_items(world: &mut World, me: u64, vendor: u32, item: u32, count: u32) -> Result<Trade, Fail> {
    let price = world
        .resource::<Vendors>()
        .and_then(|v| v.stock.get(&vendor))
        .and_then(|s| s.get(&item))
        .copied()
        .ok_or(NOT_LISTED)?;
    let cost = price.checked_mul(u64::from(count)).ok_or(MALFORMED)?;
    let inv = world.resource_mut::<Inventories>().ok_or(MALFORMED)?;
    let ledger = inv
        .transaction(&[me], |tx| {
            tx.debit(me, cost)?;
            tx.add(me, item, count)
        })
        .map_err(economy)?;
    Ok((ledger, item, cost))
}

fn sell_items(world: &mut World, me: u64, slot: u8, count: u32) -> Result<Trade, Fail> {
    // Read at the sale, so a live change applies from the tick it lands on.
    if !mantis_server::modules::flag(world, "std.vendor.selling") {
        return Err(SELLING_OFF);
    }
    let item = world
        .resource::<Inventories>()
        .and_then(|i| i.bag(me))
        .and_then(|b| b.slots.get(usize::from(slot)).copied().flatten())
        .ok_or(NOTHING)?
        .item;
    let each = world
        .resource::<Vendors>()
        .and_then(|v| v.buy_back(item))
        .ok_or(NOT_BOUGHT)?;
    let pay = each.checked_mul(u64::from(count)).ok_or(MALFORMED)?;
    let inv = world.resource_mut::<Inventories>().ok_or(MALFORMED)?;
    let ledger = inv
        .transaction(&[me], |tx| {
            tx.take(me, slot, count)?;
            tx.credit(me, pay)
        })
        .map_err(economy)?;
    Ok((ledger, item, pay))
}

/// A requested trade: `Traded` (with the total) to the trader, the outcome
/// to the log.
fn traded(
    world: &mut World,
    cmd: &ModuleCommand,
    me: u64,
    head: (u32, u32, u32),
    result: &Result<Trade, Fail>,
) -> ModuleOutcome {
    let (request, vendor, count) = head;
    let reply = match result {
        Ok((_, item, total)) => Traded {
            request,
            vendor,
            ok: true,
            reason: TRADE_OK,
            item: *item,
            count,
            total: *total,
        },
        Err((_, reason)) => Traded {
            request,
            vendor,
            ok: false,
            reason: *reason,
            item: 0,
            count,
            total: 0,
        },
    };
    tell_character(world, me, &reply);
    match result {
        Ok((ledger, _, _)) => {
            let bag = world.resource::<Inventories>().map(|i| i.bag_message(me));
            if let Some(bag) = bag {
                tell_character(world, me, &bag);
            }
            ModuleOutcome::done(cmd, ledger)
        }
        Err((r, _)) => ModuleOutcome::refused(cmd, *r),
    }
}

fn buy_requested(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let Some(me) = actor(world, cmd) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::NotAllowed);
    };
    if check(BuyItems::ID.0, cmd.payload.as_slice()).is_err() {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::Invalid);
    }
    let Ok(msg) = decode::<BuyItems>(cmd.payload.as_slice()) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::Invalid);
    };
    let result = buy_items(world, me, msg.vendor, msg.item, msg.count);
    traded(world, cmd, me, (msg.request, msg.vendor, msg.count), &result)
}

fn sell_requested(world: &mut World, _: &TickContext, cmd: &ModuleCommand) -> ModuleOutcome {
    let Some(me) = actor(world, cmd) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::NotAllowed);
    };
    if check(SellItems::ID.0, cmd.payload.as_slice()).is_err() {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::Invalid);
    }
    let Ok(msg) = decode::<SellItems>(cmd.payload.as_slice()) else {
        return ModuleOutcome::refused(cmd, ExtensionRefusal::Invalid);
    };
    let result = sell_items(world, me, msg.slot, msg.count);
    traded(world, cmd, me, (msg.request, msg.vendor, msg.count), &result)
}

fn browse(
    world: &mut World,
    _: &TickContext,
    session: SessionId,
    payload: &[u8],
) -> Result<(), ExtensionRefusal> {
    check(Browse::ID.0, payload).map_err(|(r, _)| r)?;
    let msg: Browse = decode(payload)?;
    caller(world, session)?;
    let vendors = world.resource::<Vendors>().ok_or(ExtensionRefusal::Invalid)?;
    let stock = vendors
        .stock
        .get(&msg.vendor)
        .ok_or(ExtensionRefusal::NotAllowed)?;
    let items: Vec<u32> = stock.keys().copied().take(20).collect();
    let prices: Vec<u64> = stock.values().copied().take(20).collect();
    let sell: Vec<u64> = items.iter().map(|i| vendors.buy_back(*i).unwrap_or(0)).collect();
    let reply = Stock {
        vendor: msg.vendor,
        items: BoundedArray::from_slice(&items).unwrap_or_default(),
        prices: BoundedArray::from_slice(&prices).unwrap_or_default(),
        sell_prices: BoundedArray::from_slice(&sell).unwrap_or_default(),
    };
    tell(world, session, &reply);
    Ok(())
}

/// The module.
pub struct Module;

impl ServerModule for Module {
    fn key(&self) -> &'static str {
        "std.vendor"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        let table = r.table(STOCK_TABLE)?;
        let text =
            core::str::from_utf8(&table).map_err(|_| RegistryError::Module("stock table is not UTF-8"))?;
        let stock = Vendors::parse(text).map_err(|_| RegistryError::Module("malformed stock table"))?;
        r.resource(Vendors { stock })?;
        r.handler(ExtensionKind(Browse::ID.0), Require::Joined, browse)?;
        r.command(ExtensionKind(BuyItems::ID.0), buy_requested)?;
        r.command(ExtensionKind(SellItems::ID.0), sell_requested)?;
        Ok(())
    }
}
