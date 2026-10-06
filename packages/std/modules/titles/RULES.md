# std.titles rules

- Titles come from the content table `std.titles.list` (`id name` lines,
  ids positive). The server refuses to start without it.
- Titles are awarded by services (a command with no client session) or by
  other modules through the contract's `AwardTitle` event, which is granted
  the tick after it is sent. Unknown titles, titles already held, and
  awards past 64 titles per character are refused.
- A character shows at most one title it holds; 0 shows none.
- Every change sends the character its titles. Other modules read the shown
  title through the `ActiveTitle` query and hear new titles through the
  `TitleEarned` event.
