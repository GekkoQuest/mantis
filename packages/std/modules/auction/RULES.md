# std.auction rules

- Rules come from the content table `std.auction.rules` (`key value` lines):
  `duration_seconds` (default 86400), `cut_percent` (default 5),
  `max_listings` per seller (default 20), `deposit_percent` (default 0).
  The server refuses to start without the table (an empty table means all
  defaults).
- A deposit of `deposit_percent` of the price (rounded down) is paid when
  posting. A sale returns it to the seller with the proceeds; a withdrawal
  or an expiry keeps it (it is burned).
- Posting lists part of one bag stack at a fixed price (at least 1 gold).
  The items leave the bag into escrow in the same command; a seller may have
  at most `max_listings` listings up.
- Buying debits the price from the buyer's bag. The lot goes to the buyer
  by mail and the price less the house cut goes to the seller by mail, in
  the same command. Both mailboxes must have room, or the purchase is
  refused before anything moves. Sellers cannot buy their own listings.
- A seller may cancel a listing; the items come back by mail.
- Expired listings come back to their sellers by mail; while a seller's
  mailbox is full the listing stays and is retried each tick.
- Posting (`PostLot`), buying (`BuyLot`), and withdrawing (`CancelLot`)
  answer the requester with an
  `AuctionResult` (the `request` echoed, an `AUCTION_*` reason code that
  tells a full listing allowance, missing gold, a full mailbox, your own
  listing, and a missing listing apart, and the gold moved) and a fresh
  page of their own listings. `SearchLots` pages listings (10 a page) with
  the seller, deposit, and time left of each, only your own if asked, and
  the house rules (cut, deposit, duration) so a seller sees their proceeds
  before posting. The first messages (`Post`, `BuyListing`,
  `CancelListing`, `Search`, `Listings`) are retired in the registry.
- Every amount is computed exactly and checked: the cut and the deposit in
  128 bits, the seller's proceeds with the deposit checked; a lot whose
  numbers do not fit is refused before anything moves (`AUCTION_INVALID`).

Expiry returns run as a system and record a logged outcome in the same
tick (the systems outcome sink), so the persistence writer sees them and
replay re-checks them. Cross-cell auction houses belong to a
service (plan section 10).
