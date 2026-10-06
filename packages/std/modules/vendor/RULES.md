# std.vendor rules

- Vendors and their prices come from the content table `std.vendor.stock`:
  one `vendor item price` line per listing (prices in gold, all positive).
  The server refuses to start without a well-formed table.
- Buying takes 1 to 99 of a listed item at the listed price. It is an
  economy command: the gold is debited and the items added in one
  transaction, or nothing happens (not enough gold, bag full).
- Selling takes items from one bag slot. Vendors pay, per item, the best
  price any vendor lists for it divided by 4 (rounded down); items no vendor
  lists, or worth nothing after the division, are refused. Flag `selling`
  (default on) gates selling.
- Browsing a vendor sends its stock (up to 20 listings) with both prices:
  what it charges and what vendors pay, so clients never compute prices.
- Buying (`BuyItems`) and selling (`SellItems`) answer with `Traded`: the
  `request` echoed, the vendor, the item, the count, the gold paid or
  received, and the reason a trade failed (`TRADE_*`: not enough gold, bag
  full, not for sale, nothing to sell, not bought by vendors, selling off,
  malformed). A total that does not fit in 64 bits is refused as malformed.
  The first messages (`Buy`, `Sell`, `TradeResult`) are retired in the
  registry.
- Every trade's outcome carries its ledger rows; the buyer receives its bag.

Proximity to a vendor is not checked here: vendors are not yet entities a
cell places. A package with placed vendors overrides this module (or a
later version adds a range check against the vendor entity).

A sub-feature switched off by its flag is refused as not allowed;
`FeatureDisabled` is reserved for the whole module being off (clients
grey out the module on it).
