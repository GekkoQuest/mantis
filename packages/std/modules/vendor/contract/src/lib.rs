//! std.vendor contract. Extension messages are generated from
//! `schema/vendor.idl` (their ids are the extension kinds).

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/vendor.idl`.
    #[rustfmt::skip]
    pub mod vendor;
}

pub use generated::vendor::*;

/// The content table of vendor stock: one `vendor item price` line per
/// listing, `#` comments allowed.
pub const STOCK_TABLE: &str = "std.vendor.stock";

/// Vendors buy back at the best listed price divided by this.
pub const SELL_DIVISOR: u64 = 4;

/// Trade reason: done.
pub const TRADE_OK: u8 = 0;
/// Trade reason: not enough gold or items.
pub const TRADE_INSUFFICIENT: u8 = 1;
/// Trade reason: no room in the bag.
pub const TRADE_BAG_FULL: u8 = 2;
/// Trade reason: the vendor does not sell that item.
pub const TRADE_NOT_LISTED: u8 = 3;
/// Trade reason: nothing in that bag slot.
pub const TRADE_NOTHING: u8 = 4;
/// Trade reason: no vendor buys that item back.
pub const TRADE_NOT_BOUGHT: u8 = 5;
/// Trade reason: selling is switched off.
pub const TRADE_SELLING_OFF: u8 = 6;
/// Trade reason: malformed request.
pub const TRADE_MALFORMED: u8 = 7;
