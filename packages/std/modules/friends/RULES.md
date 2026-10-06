# std.friends rules

- Friendship is mutual: it exists on both lists or on neither.
- A character may ask another character. It cannot ask itself, a friend,
  or anyone while its list is full (50 friends).
- At most 20 requests may wait for one character; more are refused.
- Answering accepts or declines one waiting request. Asking a character who
  already asked you accepts that request.
- Either friend may end the friendship.
- After every change both characters receive their lists, with a bit per
  friend saying whether that friend is in the cell, and their open
  requests both ways. A declined requester is told. `ShowFriends` (sent at
  login) returns the list and the open requests, so nothing is lost on a
  reconnect.

The social role is the single authority for every list and request (lead
ruling, M8): characters may ask characters anywhere. This module relays
requests and keeps a read-only projection of the lists of characters in
the cell, fed by logged service updates; answers arrive on a later tick,
and a refusal by the authority arrives as `FriendRefused`. Presence bits
in `FriendList` say whether each friend is in this cell. Lists are offered
back to the social role every 10 seconds, so an authority that restarted
rebuilds them; open requests are not rebuilt.
