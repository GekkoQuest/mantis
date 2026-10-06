# std.chat rules

- Channels: local (0), party (1), whisper (2), guild (3). Anything else is
  invalid.
- A line is 1 to 200 bytes of UTF-8 with no control characters and not only
  whitespace.
- Local lines reach every character in the cell within 40 m of the speaker
  (horizontal distance), the speaker included.
- Party lines reach every member of the speaker's party present in the cell,
  whatever the distance. The party is found through the `std.party`
  contract; with no party the line is refused, and with the party module
  disabled the party channel is refused as not allowed.
- Whispers reach one character in the cell and echo to the speaker; the
  line names the recipient. Flag
  `whispers` (default on) gates them.
- Guild lines reach every online member of the speaker's guild, in any
  cell, through the social role, and echo to the speaker. The guild is
  found through the `std.guild` contract, which the manifest names as
  `optional`: a package without a guild module resolves it absent at start
  and has no guild channel. With no guild, the guild module disabled, or
  no guild module, the guild channel is refused as not allowed.
- Rate limit: 5 lines per character per 5-second window; lines over the
  limit are refused. The metric `std.chat.lines` counts delivered lines.

Scope: one cell, except whispers to a character in another cell and guild
lines, which the social service carries (plan section 10) and delivers into
each recipient's cell as a logged update.

A sub-feature switched off by its flag is refused as not allowed;
`FeatureDisabled` is reserved for the whole module being off (clients
grey out the module on it).
