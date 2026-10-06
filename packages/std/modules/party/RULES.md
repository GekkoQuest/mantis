# std.party rules

Game-agnostic party rules, as most online games share them. A package that
needs different rules overrides `std.party` with a module implementing the
same contract.

- A party has at most 5 members (`MAX_MEMBERS`), the leader first.
- Any character not in a party may be invited. A character in a party may
  invite only if it leads the party and the party is not full. A character
  cannot invite itself or someone already in a party.
- An invitation names the inviting character, stays open for 60 seconds of
  the social role's clock, and is replaced by a newer one to the same invitee.
- Accepting an invitation from a character with no party forms a new party
  led by the inviter. Accepting fails if the party filled up meanwhile.
- When the leader leaves, the next member in order leads. A party left with
  one member disbands.
- Only the leader may remove (kick) a member, and not itself.
- Every member, in whichever cell, receives the roster after each change;
  a character that leaves or is removed receives `Disbanded`.
- Flag `invites` (default on) gates invitations; existing parties keep
  working when it is off.

The social role is the single authority for every party (lead ruling,
M8). This module checks what a cell can (the invitee is visible here, not
yourself, the `invites` flag), relays the request, and keeps a read-only
projection of the parties of characters in the cell, fed by logged service
updates; answers arrive on a later tick, and a refusal by the authority
arrives as `PartyRefused`. A party survives any transfer because no cell
owns it: a character arriving in a cell is sent its roster. The projection
is offered back to the social role every 10 seconds, so an authority that
restarted rebuilds every party still held by a cell.

A sub-feature switched off by its flag is refused as not allowed;
`FeatureDisabled` is reserved for the whole module being off (clients
grey out the module on it).
