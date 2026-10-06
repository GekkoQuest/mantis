//! std.auction contract. Extension messages are generated from
//! `schema/auction.idl` (their ids are the extension kinds).

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/auction.idl`.
    #[rustfmt::skip]
    pub mod auction;
}

pub use generated::auction::*;

/// The content table of auction rules: `key value` lines.
/// Keys: `duration_seconds` (how long a listing stays up),
/// `cut_percent` (what the house keeps of a sale), `max_listings` (per seller),
/// `deposit_percent` (of the price, paid when posting; default 0).
pub const RULES_TABLE: &str = "std.auction.rules";

/// `AuctionResult.reason`: done.
pub const AUCTION_OK: u8 = 0;
/// `AuctionResult.reason`: malformed request (an empty lot, a free price).
pub const AUCTION_INVALID: u8 = 1;
/// `AuctionResult.reason`: you already have `max_listings` listings up.
pub const AUCTION_TOO_MANY: u8 = 2;
/// `AuctionResult.reason`: not enough gold (the price, or the deposit).
pub const AUCTION_NO_GOLD: u8 = 3;
/// `AuctionResult.reason`: a mailbox involved is full.
pub const AUCTION_MAILBOX_FULL: u8 = 4;
/// `AuctionResult.reason`: your own listing (you cannot buy it).
pub const AUCTION_OWN_LISTING: u8 = 5;
/// `AuctionResult.reason`: no such listing.
pub const AUCTION_NOT_FOUND: u8 = 6;
/// `AuctionResult.reason`: not your listing (you cannot withdraw it).
pub const AUCTION_NOT_YOURS: u8 = 7;
/// `AuctionResult.reason`: the bag slot does not hold that many.
pub const AUCTION_NO_ITEM: u8 = 8;

/// Listings per `LotPage`.
pub const PAGE: usize = 10;
