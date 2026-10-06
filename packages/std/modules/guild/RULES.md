# std.guild rules

Game-agnostic guild rules, as most online games share them. A package that
needs different rules overrides `std.guild` with a module implementing the
same contract.

- A guild has a unique name and at most 500 members (`MAX_MEMBERS`). A name
  is 3 to 24 bytes of ASCII letters, digits, spaces, and hyphens. It starts
  and ends with a letter or digit, and has no two spaces in a row.
  Uniqueness ignores letter case.
- A character is in at most one guild. Any character not in a guild may
  found one, and leads it.
- Ranks: leader (0), officer (1), member (2). A lower number outranks a
  higher one. There is exactly one leader.
- The leader and officers may invite a character that is not in a guild,
  while the guild is not full. An invitation stays open for 5 minutes of
  the social role's clock. A newer invitation to the same character
  replaces it. Accepting joins the guild as a member, and fails if the
  guild filled up or was disbanded meanwhile.
- A member may leave at any time. When the leader leaves, the
  highest-ranked member leads, the longest-standing first. The last member
  leaving disbands the guild.
- The leader and officers may remove a member who ranks below them. An
  officer cannot remove another officer, and nobody can remove the leader.
- Only the leader sets ranks. Making a member the leader passes the lead,
  and the old leader becomes an officer. Only the leader may disband the
  guild.
- Every member, in whichever cell, is told each membership and rank change.
  A member is told its guild and full roster when it joins, and again when
  it arrives in a cell. The roster comes in pages of 40.
- Flag `invites` (default on) gates invitations. Existing guilds keep
  working when it is off.

The social role is the single authority for every guild (lead ruling, M8),
and guilds are durable. Every change is written through the persistence
writer before any member is told of it. A social role that restarts reads
every guild back from the writer. Invitations are not durable and lapse
with a restart.

This module checks what a cell can: the name, that the invitee is visible
here, that a character does not invite or remove itself, and the `invites`
flag. It then relays the request and keeps a read-only projection of the
guilds of characters in the cell, fed by logged service updates. Answers
arrive on a later tick. A refusal by the authority arrives as
`GuildRefused`.

A guild survives any transfer and any restart, because no cell owns it.

Guild chat is `std.chat`'s guild channel (3). The social role carries a
line to every online member, wherever they are.

A sub-feature switched off by its flag is refused as not allowed.
`FeatureDisabled` is reserved for the whole module being off; clients grey
out the module on it.
