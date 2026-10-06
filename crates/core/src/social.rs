//! Parties and friends as the social role holds them (plan section 10,
//! lead ruling M8): one authority outside every cell, so a party survives
//! any transfer because no cell owns it. Cells hold read-only projections,
//! fed by logged service updates.
//!
//! This module is the authority's data and rules ([`PartyBook`],
//! [`FriendBook`], [`GuildBook`]) and the wire forms cells and the social
//! role exchange ([`PartyOp`], [`PartyUpdate`], [`FriendOp`],
//! [`FriendUpdate`], [`GuildOp`], [`GuildUpdate`], and the durable
//! [`GuildChange`] rows). It lives
//! in the core so the social role and a cell's test bed run the very same
//! rules. Times are Unix milliseconds supplied by the caller; nothing here
//! reads a clock.

use std::collections::{BTreeMap, BTreeSet};

use crate::wire::{DecodeError, Decoder, Encoder, Wire};

/// Outbound topic: a party operation for the social role.
pub const PARTY_OP: u16 = 3;
/// Inbound topic: a party update for a character in this cell.
pub const PARTY_UPDATE: u16 = 4;
/// Outbound topic: a friends operation for the social role.
pub const FRIEND_OP: u16 = 5;
/// Inbound topic: a friends update for a character in this cell.
pub const FRIEND_UPDATE: u16 = 6;

/// Members a party may have.
pub const PARTY_MAX: usize = 5;
/// How long an invitation stays open, in milliseconds.
pub const INVITE_MS: u64 = 60_000;
/// Friends one character may have.
pub const FRIENDS_MAX: usize = 50;
/// Open requests one character may have waiting.
pub const PENDING_MAX: usize = 20;

fn list(e: &mut Encoder<'_>, items: &[u64]) {
    e.u8(u8::try_from(items.len()).unwrap_or(u8::MAX));
    for i in items {
        e.u64(*i);
    }
}

fn read_list(d: &mut Decoder<'_>, max: usize) -> Result<Vec<u64>, DecodeError> {
    let n = usize::from(d.u8()?);
    if n > max {
        return Err(DecodeError::Invalid("list too long"));
    }
    (0..n).map(|_| d.u64()).collect()
}

// ---- parties ------------------------------------------------------------

/// What a cell asks the social role to do with a party.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PartyOp {
    /// `from` invites `to`.
    Invite {
        /// The inviter.
        from: u64,
        /// The invited.
        to: u64,
    },
    /// `me` accepts `from`'s invitation.
    Accept {
        /// The accepting character.
        me: u64,
        /// The inviter.
        from: u64,
    },
    /// `me` leaves its party.
    Leave {
        /// The leaving character.
        me: u64,
    },
    /// The leader `me` removes `who`.
    Kick {
        /// The leader.
        me: u64,
        /// The removed member.
        who: u64,
    },
    /// A cell's projection of a party, re-sent from time to time so an
    /// authority that restarted rebuilds it. Ignored for a known party.
    Restore {
        /// The party.
        party: u32,
        /// Its leader.
        leader: u64,
        /// Its members, the leader first.
        members: Vec<u64>,
    },
}

/// Operation codes, for refusals.
pub mod party_op {
    /// Invite.
    pub const INVITE: u8 = 1;
    /// Accept.
    pub const ACCEPT: u8 = 2;
    /// Leave.
    pub const LEAVE: u8 = 3;
    /// Kick.
    pub const KICK: u8 = 4;
}

impl Wire for PartyOp {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Invite { from, to } => {
                e.u8(party_op::INVITE);
                e.u64(*from);
                e.u64(*to);
            }
            Self::Accept { me, from } => {
                e.u8(party_op::ACCEPT);
                e.u64(*me);
                e.u64(*from);
            }
            Self::Leave { me } => {
                e.u8(party_op::LEAVE);
                e.u64(*me);
            }
            Self::Kick { me, who } => {
                e.u8(party_op::KICK);
                e.u64(*me);
                e.u64(*who);
            }
            Self::Restore {
                party,
                leader,
                members,
            } => {
                e.u8(5);
                e.u32(*party);
                e.u64(*leader);
                list(e, members);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            party_op::INVITE => Self::Invite {
                from: d.u64()?,
                to: d.u64()?,
            },
            party_op::ACCEPT => Self::Accept {
                me: d.u64()?,
                from: d.u64()?,
            },
            party_op::LEAVE => Self::Leave { me: d.u64()? },
            party_op::KICK => Self::Kick {
                me: d.u64()?,
                who: d.u64()?,
            },
            5 => Self::Restore {
                party: d.u32()?,
                leader: d.u64()?,
                members: read_list(d, PARTY_MAX)?,
            },
            _ => return Err(DecodeError::Invalid("party op")),
        })
    }
}

/// What the social role tells one character about its party.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PartyUpdate {
    /// `to`'s party now has this roster (also sent when `to` arrives in a
    /// cell, so the projection follows the character).
    Roster {
        /// The character told.
        to: u64,
        /// The party.
        party: u32,
        /// Its leader.
        leader: u64,
        /// Its members, the leader first.
        members: Vec<u64>,
    },
    /// `to` was invited by `from`.
    Invited {
        /// The character told.
        to: u64,
        /// The inviter.
        from: u64,
    },
    /// `to` is out of `party` (left, removed, or the party disbanded).
    Left {
        /// The character told.
        to: u64,
        /// The party.
        party: u32,
    },
    /// `to`'s operation was refused.
    Refused {
        /// The character told.
        to: u64,
        /// The operation ([`party_op`]).
        op: u8,
    },
}

impl PartyUpdate {
    /// The character this update is for.
    #[must_use]
    pub fn to(&self) -> u64 {
        match self {
            Self::Roster { to, .. }
            | Self::Invited { to, .. }
            | Self::Left { to, .. }
            | Self::Refused { to, .. } => *to,
        }
    }
}

impl Wire for PartyUpdate {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Roster {
                to,
                party,
                leader,
                members,
            } => {
                e.u8(1);
                e.u64(*to);
                e.u32(*party);
                e.u64(*leader);
                list(e, members);
            }
            Self::Invited { to, from } => {
                e.u8(2);
                e.u64(*to);
                e.u64(*from);
            }
            Self::Left { to, party } => {
                e.u8(3);
                e.u64(*to);
                e.u32(*party);
            }
            Self::Refused { to, op } => {
                e.u8(4);
                e.u64(*to);
                e.u8(*op);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Roster {
                to: d.u64()?,
                party: d.u32()?,
                leader: d.u64()?,
                members: read_list(d, PARTY_MAX)?,
            },
            2 => Self::Invited {
                to: d.u64()?,
                from: d.u64()?,
            },
            3 => Self::Left {
                to: d.u64()?,
                party: d.u32()?,
            },
            4 => Self::Refused {
                to: d.u64()?,
                op: d.u8()?,
            },
            _ => return Err(DecodeError::Invalid("party update")),
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Party {
    leader: u64,
    members: Vec<u64>,
}

/// Every party and open invitation: the authority.
#[derive(Clone, Debug, Default)]
pub struct PartyBook {
    next: u32,
    parties: BTreeMap<u32, Party>,
    invites: BTreeMap<u64, (u64, u64)>,
}

impl PartyBook {
    /// The party `character` is in.
    #[must_use]
    pub fn party_of(&self, character: u64) -> Option<u32> {
        self.parties
            .iter()
            .find(|(_, p)| p.members.contains(&character))
            .map(|(id, _)| *id)
    }

    /// `character`'s roster update, if it is in a party.
    #[must_use]
    pub fn roster_for(&self, character: u64) -> Option<PartyUpdate> {
        let id = self.party_of(character)?;
        let p = self.parties.get(&id)?;
        Some(PartyUpdate::Roster {
            to: character,
            party: id,
            leader: p.leader,
            members: p.members.clone(),
        })
    }

    fn rosters(&self, id: u32, out: &mut Vec<PartyUpdate>) {
        if let Some(p) = self.parties.get(&id) {
            for m in &p.members {
                out.push(PartyUpdate::Roster {
                    to: *m,
                    party: id,
                    leader: p.leader,
                    members: p.members.clone(),
                });
            }
        }
    }

    /// Applies one operation at `now_ms`; returns what to tell whom.
    pub fn apply(&mut self, op: &PartyOp, now_ms: u64) -> Vec<PartyUpdate> {
        self.invites.retain(|_, (_, until)| *until > now_ms);
        let mut out = Vec::new();
        let refused = |to: u64, op: u8| vec![PartyUpdate::Refused { to, op }];
        match *op {
            PartyOp::Invite { from, to } => {
                if to == from || to == 0 || self.party_of(to).is_some() {
                    return refused(from, party_op::INVITE);
                }
                if let Some(p) = self.party_of(from).and_then(|id| self.parties.get(&id))
                    && (p.leader != from || p.members.len() >= PARTY_MAX)
                {
                    return refused(from, party_op::INVITE);
                }
                self.invites.insert(to, (from, now_ms + INVITE_MS));
                out.push(PartyUpdate::Invited { to, from });
            }
            PartyOp::Accept { me, from } => {
                let invited = self.invites.get(&me).is_some_and(|(f, _)| *f == from);
                if !invited || self.party_of(me).is_some() {
                    return refused(me, party_op::ACCEPT);
                }
                self.invites.remove(&me);
                let id = if let Some(id) = self.party_of(from) {
                    let Some(p) = self.parties.get_mut(&id) else {
                        return refused(me, party_op::ACCEPT);
                    };
                    if p.members.len() >= PARTY_MAX {
                        return refused(me, party_op::ACCEPT);
                    }
                    p.members.push(me);
                    id
                } else {
                    self.next += 1;
                    self.parties.insert(
                        self.next,
                        Party {
                            leader: from,
                            members: vec![from, me],
                        },
                    );
                    self.next
                };
                self.rosters(id, &mut out);
            }
            PartyOp::Leave { me } => match self.party_of(me) {
                Some(id) => self.remove(id, me, &mut out),
                None => return refused(me, party_op::LEAVE),
            },
            PartyOp::Kick { me, who } => {
                let id = self.party_of(me);
                let ok = id
                    .and_then(|id| self.parties.get(&id))
                    .is_some_and(|p| p.leader == me && who != me && p.members.contains(&who));
                match (id, ok) {
                    (Some(id), true) => self.remove(id, who, &mut out),
                    _ => return refused(me, party_op::KICK),
                }
            }
            PartyOp::Restore {
                party,
                leader,
                ref members,
            } => {
                let clash = members.iter().any(|m| self.party_of(*m).is_some());
                if !self.parties.contains_key(&party) && !clash && members.len() >= 2 {
                    self.parties.insert(
                        party,
                        Party {
                            leader,
                            members: members.clone(),
                        },
                    );
                    self.next = self.next.max(party);
                }
            }
        }
        out
    }

    fn remove(&mut self, id: u32, who: u64, out: &mut Vec<PartyUpdate>) {
        let Some(p) = self.parties.get_mut(&id) else {
            return;
        };
        p.members.retain(|m| *m != who);
        if p.leader == who {
            p.leader = p.members.first().copied().unwrap_or(0);
        }
        out.push(PartyUpdate::Left { to: who, party: id });
        if p.members.len() <= 1 {
            for m in &p.members {
                out.push(PartyUpdate::Left { to: *m, party: id });
            }
            self.parties.remove(&id);
        } else {
            self.rosters(id, out);
        }
    }
}

// ---- friends ------------------------------------------------------------

/// What a cell asks the social role to do with friends.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FriendOp {
    /// `me` asks `other` (asking someone who asked you accepts).
    Request {
        /// The asking character.
        me: u64,
        /// The asked.
        other: u64,
    },
    /// `me` answers `other`'s request.
    Respond {
        /// The answering character.
        me: u64,
        /// The asker.
        other: u64,
        /// Accepted.
        accept: bool,
    },
    /// `me` ends its friendship with `other`.
    Remove {
        /// The character.
        me: u64,
        /// The former friend.
        other: u64,
    },
    /// `me` wants its list and open requests.
    Show {
        /// The character.
        me: u64,
    },
}

/// Operation codes, for refusals.
pub mod friend_op {
    /// Request.
    pub const REQUEST: u8 = 1;
    /// Respond.
    pub const RESPOND: u8 = 2;
    /// Remove.
    pub const REMOVE: u8 = 3;
}

impl Wire for FriendOp {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Request { me, other } => {
                e.u8(friend_op::REQUEST);
                e.u64(*me);
                e.u64(*other);
            }
            Self::Respond { me, other, accept } => {
                e.u8(friend_op::RESPOND);
                e.u64(*me);
                e.u64(*other);
                e.bool(*accept);
            }
            Self::Remove { me, other } => {
                e.u8(friend_op::REMOVE);
                e.u64(*me);
                e.u64(*other);
            }
            Self::Show { me } => {
                e.u8(4);
                e.u64(*me);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            friend_op::REQUEST => Self::Request {
                me: d.u64()?,
                other: d.u64()?,
            },
            friend_op::RESPOND => Self::Respond {
                me: d.u64()?,
                other: d.u64()?,
                accept: d.bool()?,
            },
            friend_op::REMOVE => Self::Remove {
                me: d.u64()?,
                other: d.u64()?,
            },
            4 => Self::Show { me: d.u64()? },
            _ => return Err(DecodeError::Invalid("friend op")),
        })
    }
}

/// What the social role tells one character about its friends.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FriendUpdate {
    /// `to`'s friends, in order.
    List {
        /// The character told.
        to: u64,
        /// Its friends.
        friends: Vec<u64>,
    },
    /// `to`'s open requests both ways.
    Pending {
        /// The character told.
        to: u64,
        /// Characters asking `to`.
        incoming: Vec<u64>,
        /// Characters `to` asked.
        outgoing: Vec<u64>,
    },
    /// `from` asked `to`.
    Requested {
        /// The character told.
        to: u64,
        /// The asker.
        from: u64,
    },
    /// `by` declined `to`'s request.
    Declined {
        /// The character told.
        to: u64,
        /// Who declined.
        by: u64,
    },
    /// `to`'s operation was refused.
    Refused {
        /// The character told.
        to: u64,
        /// The operation ([`friend_op`]).
        op: u8,
    },
}

impl FriendUpdate {
    /// The character this update is for.
    #[must_use]
    pub fn to(&self) -> u64 {
        match self {
            Self::List { to, .. }
            | Self::Pending { to, .. }
            | Self::Requested { to, .. }
            | Self::Declined { to, .. }
            | Self::Refused { to, .. } => *to,
        }
    }
}

impl Wire for FriendUpdate {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::List { to, friends } => {
                e.u8(1);
                e.u64(*to);
                list(e, friends);
            }
            Self::Pending {
                to,
                incoming,
                outgoing,
            } => {
                e.u8(2);
                e.u64(*to);
                list(e, incoming);
                list(e, outgoing);
            }
            Self::Requested { to, from } => {
                e.u8(3);
                e.u64(*to);
                e.u64(*from);
            }
            Self::Declined { to, by } => {
                e.u8(4);
                e.u64(*to);
                e.u64(*by);
            }
            Self::Refused { to, op } => {
                e.u8(5);
                e.u64(*to);
                e.u8(*op);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::List {
                to: d.u64()?,
                friends: read_list(d, FRIENDS_MAX)?,
            },
            2 => Self::Pending {
                to: d.u64()?,
                incoming: read_list(d, PENDING_MAX)?,
                outgoing: read_list(d, PENDING_MAX)?,
            },
            3 => Self::Requested {
                to: d.u64()?,
                from: d.u64()?,
            },
            4 => Self::Declined {
                to: d.u64()?,
                by: d.u64()?,
            },
            5 => Self::Refused {
                to: d.u64()?,
                op: d.u8()?,
            },
            _ => return Err(DecodeError::Invalid("friend update")),
        })
    }
}

/// One durable change to the friend rows (the persistence writer applies
/// them in order; loading every row back rebuilds the book).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FriendChange {
    /// `a` and `b` are friends (stored once, `a < b`).
    Friends {
        /// The lower character id.
        a: u64,
        /// The higher.
        b: u64,
    },
    /// `a` and `b` are no longer friends (`a < b`).
    NoFriends {
        /// The lower character id.
        a: u64,
        /// The higher.
        b: u64,
    },
    /// `asker` asked `asked`: an open request.
    Asked {
        /// The asking character.
        asker: u64,
        /// The asked.
        asked: u64,
    },
    /// The request from `asker` to `asked` is closed.
    NoAsk {
        /// The asking character.
        asker: u64,
        /// The asked.
        asked: u64,
    },
}

impl Wire for FriendChange {
    fn encode(&self, e: &mut Encoder<'_>) {
        let (kind, x, y) = match *self {
            Self::Friends { a, b } => (1, a, b),
            Self::NoFriends { a, b } => (2, a, b),
            Self::Asked { asker, asked } => (3, asker, asked),
            Self::NoAsk { asker, asked } => (4, asker, asked),
        };
        e.u8(kind);
        e.u64(x);
        e.u64(y);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let kind = d.u8()?;
        let (x, y) = (d.u64()?, d.u64()?);
        Ok(match kind {
            1 => Self::Friends { a: x, b: y },
            2 => Self::NoFriends { a: x, b: y },
            3 => Self::Asked { asker: x, asked: y },
            4 => Self::NoAsk { asker: x, asked: y },
            _ => return Err(DecodeError::Invalid("friend change")),
        })
    }
}

/// What one friends operation did: updates to project, and the durable
/// changes to write before any of them is sent.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FriendOutcome {
    /// What to tell whom.
    pub updates: Vec<FriendUpdate>,
    /// Rows to make durable first.
    pub changes: Vec<FriendChange>,
}

/// Friend lists and open requests: the authority. Both are durable: the
/// book is rebuilt from its rows ([`FriendBook::from_rows`]).
#[derive(Clone, Debug, Default)]
pub struct FriendBook {
    lists: BTreeMap<u64, BTreeSet<u64>>,
    /// Asked character -> characters asking.
    pending: BTreeMap<u64, BTreeSet<u64>>,
}

impl FriendBook {
    /// A book rebuilt from durable rows, in the order they were written.
    #[must_use]
    pub fn from_rows(rows: &[FriendChange]) -> Self {
        let mut b = Self::default();
        for r in rows {
            b.change(r);
        }
        b
    }

    /// Every durable row of the book: each friendship once, then each open
    /// request.
    #[must_use]
    pub fn rows(&self) -> Vec<FriendChange> {
        let mut out: Vec<FriendChange> = self
            .lists
            .iter()
            .flat_map(|(a, l)| {
                l.iter()
                    .filter(move |b| *b > a)
                    .map(move |b| FriendChange::Friends { a: *a, b: *b })
            })
            .collect();
        for (asked, askers) in &self.pending {
            out.extend(askers.iter().map(|asker| FriendChange::Asked {
                asker: *asker,
                asked: *asked,
            }));
        }
        out
    }

    /// Friendships and open requests in the book.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows().len()
    }

    /// True with no friendship and no open request.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lists.is_empty() && self.pending.is_empty()
    }

    /// Applies one durable change.
    fn change(&mut self, c: &FriendChange) {
        match *c {
            FriendChange::Friends { a, b } => {
                self.lists.entry(a).or_default().insert(b);
                self.lists.entry(b).or_default().insert(a);
            }
            FriendChange::NoFriends { a, b } => {
                for (x, y) in [(a, b), (b, a)] {
                    if let Some(l) = self.lists.get_mut(&x) {
                        l.remove(&y);
                        if l.is_empty() {
                            self.lists.remove(&x);
                        }
                    }
                }
            }
            FriendChange::Asked { asker, asked } => {
                self.pending.entry(asked).or_default().insert(asker);
            }
            FriendChange::NoAsk { asker, asked } => {
                if let Some(p) = self.pending.get_mut(&asked) {
                    p.remove(&asker);
                    if p.is_empty() {
                        self.pending.remove(&asked);
                    }
                }
            }
        }
    }

    fn record(&mut self, c: FriendChange, out: &mut FriendOutcome) {
        self.change(&c);
        out.changes.push(c);
    }

    /// True when `a` and `b` are friends.
    #[must_use]
    pub fn are_friends(&self, a: u64, b: u64) -> bool {
        self.lists.get(&a).is_some_and(|l| l.contains(&b))
    }

    /// True when `from` has an open request to `to`.
    fn asked(&self, from: u64, to: u64) -> bool {
        self.pending.get(&to).is_some_and(|p| p.contains(&from))
    }

    /// `character`'s list and open requests.
    #[must_use]
    pub fn snapshot(&self, character: u64) -> [FriendUpdate; 2] {
        let friends = self
            .lists
            .get(&character)
            .map(|l| l.iter().copied().collect())
            .unwrap_or_default();
        let incoming = self
            .pending
            .get(&character)
            .map(|p| p.iter().copied().collect())
            .unwrap_or_default();
        let outgoing = self
            .pending
            .iter()
            .filter(|(_, askers)| askers.contains(&character))
            .map(|(asked, _)| *asked)
            .collect();
        [
            FriendUpdate::List {
                to: character,
                friends,
            },
            FriendUpdate::Pending {
                to: character,
                incoming,
                outgoing,
            },
        ]
    }

    fn count(&self, c: u64) -> usize {
        self.lists.get(&c).map_or(0, BTreeSet::len)
    }

    fn befriend(&mut self, x: u64, y: u64, out: &mut FriendOutcome) {
        for (asker, asked) in [(x, y), (y, x)] {
            if self.asked(asker, asked) {
                self.record(FriendChange::NoAsk { asker, asked }, out);
            }
        }
        self.record(
            FriendChange::Friends {
                a: x.min(y),
                b: x.max(y),
            },
            out,
        );
        out.updates.extend(self.snapshot(x));
        out.updates.extend(self.snapshot(y));
    }

    /// Applies one operation. The caller makes the outcome's changes
    /// durable before sending any of its updates.
    pub fn apply(&mut self, op: &FriendOp) -> FriendOutcome {
        let mut out = FriendOutcome::default();
        let refused = |to: u64, op: u8| FriendOutcome {
            updates: vec![FriendUpdate::Refused { to, op }],
            changes: Vec::new(),
        };
        match *op {
            FriendOp::Request { me, other } => {
                if other == me || other == 0 || self.are_friends(me, other) || self.count(me) >= FRIENDS_MAX {
                    return refused(me, friend_op::REQUEST);
                }
                if self.asked(other, me) {
                    if self.count(other) >= FRIENDS_MAX {
                        return refused(me, friend_op::REQUEST);
                    }
                    self.befriend(me, other, &mut out);
                } else {
                    let waiting = self.pending.get(&other).map_or(0, BTreeSet::len);
                    if !self.asked(me, other) {
                        if waiting >= PENDING_MAX {
                            return refused(me, friend_op::REQUEST);
                        }
                        self.record(
                            FriendChange::Asked {
                                asker: me,
                                asked: other,
                            },
                            &mut out,
                        );
                    }
                    out.updates.push(FriendUpdate::Requested { to: other, from: me });
                    out.updates.extend(self.snapshot(other).into_iter().skip(1));
                    out.updates.extend(self.snapshot(me).into_iter().skip(1));
                }
            }
            FriendOp::Respond { me, other, accept } => {
                if !self.asked(other, me) {
                    return refused(me, friend_op::RESPOND);
                }
                if accept {
                    if self.count(me) >= FRIENDS_MAX || self.count(other) >= FRIENDS_MAX {
                        return refused(me, friend_op::RESPOND);
                    }
                    self.befriend(me, other, &mut out);
                } else {
                    self.record(
                        FriendChange::NoAsk {
                            asker: other,
                            asked: me,
                        },
                        &mut out,
                    );
                    out.updates.push(FriendUpdate::Declined { to: other, by: me });
                    out.updates.extend(self.snapshot(me).into_iter().skip(1));
                    out.updates.extend(self.snapshot(other).into_iter().skip(1));
                }
            }
            FriendOp::Remove { me, other } => {
                if !self.are_friends(me, other) {
                    return refused(me, friend_op::REMOVE);
                }
                self.record(
                    FriendChange::NoFriends {
                        a: me.min(other),
                        b: me.max(other),
                    },
                    &mut out,
                );
                out.updates.extend(self.snapshot(me).into_iter().take(1));
                out.updates.extend(self.snapshot(other).into_iter().take(1));
            }
            FriendOp::Show { me } => out.updates.extend(self.snapshot(me)),
        }
        out
    }
}

// ---- guilds -------------------------------------------------------------

/// Outbound topic: a guild operation for the social role.
pub const GUILD_OP: u16 = 7;
/// Inbound topic: a guild update for a character in this cell.
pub const GUILD_UPDATE: u16 = 8;

/// Members a guild may have.
pub const GUILD_MAX: usize = 500;
/// Shortest guild name, in bytes.
pub const GUILD_NAME_MIN: usize = 3;
/// Longest guild name, in bytes.
pub const GUILD_NAME_MAX: usize = 24;
/// How long a guild invitation stays open, in milliseconds.
pub const GUILD_INVITE_MS: u64 = 300_000;
/// Members per roster update (a full roster is sent in chunks).
pub const ROSTER_CHUNK: usize = 40;

/// Guild ranks: a lower number outranks a higher one.
pub mod guild_rank {
    /// The one leader.
    pub const LEADER: u8 = 0;
    /// Officers: may invite, and remove members.
    pub const OFFICER: u8 = 1;
    /// Members.
    pub const MEMBER: u8 = 2;
}

/// Guild operation codes, for refusals.
pub mod guild_op {
    /// Create a guild.
    pub const CREATE: u8 = 1;
    /// Invite.
    pub const INVITE: u8 = 2;
    /// Accept an invitation.
    pub const ACCEPT: u8 = 3;
    /// Leave.
    pub const LEAVE: u8 = 4;
    /// Remove a member.
    pub const KICK: u8 = 5;
    /// Set a member's rank.
    pub const SET_RANK: u8 = 6;
    /// Disband.
    pub const DISBAND: u8 = 7;
}

/// True for a valid guild name: 3 to 24 bytes of ASCII letters, digits,
/// spaces, and hyphens, starting and ending with a letter or digit, with no
/// two spaces in a row. Names are unique ignoring ASCII case.
#[must_use]
pub fn guild_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (GUILD_NAME_MIN..=GUILD_NAME_MAX).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == b'-')
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
        && !name.contains("  ")
}

fn put_name(e: &mut Encoder<'_>, name: &str) {
    let b = name.as_bytes();
    let n = b.len().min(GUILD_NAME_MAX);
    e.u8(u8::try_from(n).unwrap_or(0));
    e.bytes(b.get(..n).unwrap_or(&[]));
}

fn read_name(d: &mut Decoder<'_>) -> Result<String, DecodeError> {
    let n = usize::from(d.u8()?);
    if n > GUILD_NAME_MAX {
        return Err(DecodeError::Invalid("guild name too long"));
    }
    String::from_utf8(d.take(n)?.to_vec()).map_err(|_| DecodeError::Invalid("guild name"))
}

fn read_rank(d: &mut Decoder<'_>) -> Result<u8, DecodeError> {
    let r = d.u8()?;
    if r > guild_rank::MEMBER {
        return Err(DecodeError::Invalid("guild rank"));
    }
    Ok(r)
}

/// What a cell asks the social role to do with a guild.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GuildOp {
    /// `me` founds a guild named `name` and leads it.
    Create {
        /// The founder.
        me: u64,
        /// The name.
        name: String,
    },
    /// `me` invites `to` into its guild.
    Invite {
        /// The inviter (leader or officer).
        me: u64,
        /// The invited.
        to: u64,
    },
    /// `me` accepts the invitation into `guild`.
    Accept {
        /// The accepting character.
        me: u64,
        /// The guild.
        guild: u32,
    },
    /// `me` leaves its guild.
    Leave {
        /// The leaving character.
        me: u64,
    },
    /// `me` removes `who` (who must rank below `me`).
    Kick {
        /// The remover.
        me: u64,
        /// The removed.
        who: u64,
    },
    /// The leader `me` sets `who`'s rank; [`guild_rank::LEADER`] passes the
    /// lead (the old leader becomes an officer).
    SetRank {
        /// The leader.
        me: u64,
        /// The member.
        who: u64,
        /// The new rank.
        rank: u8,
    },
    /// The leader `me` disbands its guild.
    Disband {
        /// The leader.
        me: u64,
    },
}

impl Wire for GuildOp {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Create { me, name } => {
                e.u8(guild_op::CREATE);
                e.u64(*me);
                put_name(e, name);
            }
            Self::Invite { me, to } => {
                e.u8(guild_op::INVITE);
                e.u64(*me);
                e.u64(*to);
            }
            Self::Accept { me, guild } => {
                e.u8(guild_op::ACCEPT);
                e.u64(*me);
                e.u32(*guild);
            }
            Self::Leave { me } => {
                e.u8(guild_op::LEAVE);
                e.u64(*me);
            }
            Self::Kick { me, who } => {
                e.u8(guild_op::KICK);
                e.u64(*me);
                e.u64(*who);
            }
            Self::SetRank { me, who, rank } => {
                e.u8(guild_op::SET_RANK);
                e.u64(*me);
                e.u64(*who);
                e.u8(*rank);
            }
            Self::Disband { me } => {
                e.u8(guild_op::DISBAND);
                e.u64(*me);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            guild_op::CREATE => Self::Create {
                me: d.u64()?,
                name: read_name(d)?,
            },
            guild_op::INVITE => Self::Invite {
                me: d.u64()?,
                to: d.u64()?,
            },
            guild_op::ACCEPT => Self::Accept {
                me: d.u64()?,
                guild: d.u32()?,
            },
            guild_op::LEAVE => Self::Leave { me: d.u64()? },
            guild_op::KICK => Self::Kick {
                me: d.u64()?,
                who: d.u64()?,
            },
            guild_op::SET_RANK => Self::SetRank {
                me: d.u64()?,
                who: d.u64()?,
                rank: read_rank(d)?,
            },
            guild_op::DISBAND => Self::Disband { me: d.u64()? },
            _ => return Err(DecodeError::Invalid("guild op")),
        })
    }
}

/// What the social role tells one character about its guild.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GuildUpdate {
    /// `to` is in `guild` at `rank` (on founding, joining, and arriving in
    /// a cell); its roster follows in [`GuildUpdate::Members`] chunks.
    Joined {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
        /// Its name.
        name: String,
        /// `to`'s rank.
        rank: u8,
    },
    /// Part of `guild`'s roster: `first` starts a new roster.
    Members {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
        /// The first chunk of a roster.
        first: bool,
        /// Members and ranks (at most [`ROSTER_CHUNK`]).
        members: Vec<(u64, u8)>,
    },
    /// `member` joined `guild`, or its rank changed.
    Member {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
        /// The member.
        member: u64,
        /// Its rank.
        rank: u8,
    },
    /// `member` is out of `guild` (left or removed; `to` itself when
    /// `member == to`).
    Gone {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
        /// The former member.
        member: u64,
    },
    /// `guild` was disbanded.
    Disbanded {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
    },
    /// `from` invited `to` into `guild`.
    Invited {
        /// The character told.
        to: u64,
        /// The guild.
        guild: u32,
        /// Its name.
        name: String,
        /// The inviter.
        from: u64,
    },
    /// `to`'s operation was refused.
    Refused {
        /// The character told.
        to: u64,
        /// The operation ([`guild_op`]).
        op: u8,
    },
}

impl GuildUpdate {
    /// The character this update is for.
    #[must_use]
    pub fn to(&self) -> u64 {
        match self {
            Self::Joined { to, .. }
            | Self::Members { to, .. }
            | Self::Member { to, .. }
            | Self::Gone { to, .. }
            | Self::Disbanded { to, .. }
            | Self::Invited { to, .. }
            | Self::Refused { to, .. } => *to,
        }
    }
}

impl Wire for GuildUpdate {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Joined {
                to,
                guild,
                name,
                rank,
            } => {
                e.u8(1);
                e.u64(*to);
                e.u32(*guild);
                put_name(e, name);
                e.u8(*rank);
            }
            Self::Members {
                to,
                guild,
                first,
                members,
            } => {
                e.u8(2);
                e.u64(*to);
                e.u32(*guild);
                e.bool(*first);
                let n = members.len().min(ROSTER_CHUNK);
                e.u8(u8::try_from(n).unwrap_or(0));
                for (m, r) in members.iter().take(n) {
                    e.u64(*m);
                    e.u8(*r);
                }
            }
            Self::Member {
                to,
                guild,
                member,
                rank,
            } => {
                e.u8(3);
                e.u64(*to);
                e.u32(*guild);
                e.u64(*member);
                e.u8(*rank);
            }
            Self::Gone { to, guild, member } => {
                e.u8(4);
                e.u64(*to);
                e.u32(*guild);
                e.u64(*member);
            }
            Self::Disbanded { to, guild } => {
                e.u8(5);
                e.u64(*to);
                e.u32(*guild);
            }
            Self::Invited {
                to,
                guild,
                name,
                from,
            } => {
                e.u8(6);
                e.u64(*to);
                e.u32(*guild);
                put_name(e, name);
                e.u64(*from);
            }
            Self::Refused { to, op } => {
                e.u8(7);
                e.u64(*to);
                e.u8(*op);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Joined {
                to: d.u64()?,
                guild: d.u32()?,
                name: read_name(d)?,
                rank: read_rank(d)?,
            },
            2 => {
                let (to, guild, first) = (d.u64()?, d.u32()?, d.bool()?);
                let n = usize::from(d.u8()?);
                if n > ROSTER_CHUNK {
                    return Err(DecodeError::Invalid("roster chunk too long"));
                }
                let mut members = Vec::with_capacity(n);
                for _ in 0..n {
                    members.push((d.u64()?, read_rank(d)?));
                }
                Self::Members {
                    to,
                    guild,
                    first,
                    members,
                }
            }
            3 => Self::Member {
                to: d.u64()?,
                guild: d.u32()?,
                member: d.u64()?,
                rank: read_rank(d)?,
            },
            4 => Self::Gone {
                to: d.u64()?,
                guild: d.u32()?,
                member: d.u64()?,
            },
            5 => Self::Disbanded {
                to: d.u64()?,
                guild: d.u32()?,
            },
            6 => Self::Invited {
                to: d.u64()?,
                guild: d.u32()?,
                name: read_name(d)?,
                from: d.u64()?,
            },
            7 => Self::Refused {
                to: d.u64()?,
                op: d.u8()?,
            },
            _ => return Err(DecodeError::Invalid("guild update")),
        })
    }
}

/// One durable change to the guild rows (the persistence writer applies
/// them in order; loading every row back rebuilds the book).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GuildChange {
    /// A guild exists with this name.
    Guild {
        /// The guild.
        id: u32,
        /// Its name.
        name: String,
    },
    /// The guild is gone, and every membership in it.
    NoGuild {
        /// The guild.
        id: u32,
    },
    /// `character` is in `guild` at `rank`, a member since `since` (the
    /// book's join counter: older members succeed a leader first).
    Member {
        /// The guild.
        guild: u32,
        /// The character.
        character: u64,
        /// Its rank.
        rank: u8,
        /// Join order.
        since: u64,
    },
    /// `character` is no longer in `guild`.
    NoMember {
        /// The guild.
        guild: u32,
        /// The character.
        character: u64,
    },
}

impl Wire for GuildChange {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Guild { id, name } => {
                e.u8(1);
                e.u32(*id);
                put_name(e, name);
            }
            Self::NoGuild { id } => {
                e.u8(2);
                e.u32(*id);
            }
            Self::Member {
                guild,
                character,
                rank,
                since,
            } => {
                e.u8(3);
                e.u32(*guild);
                e.u64(*character);
                e.u8(*rank);
                e.u64(*since);
            }
            Self::NoMember { guild, character } => {
                e.u8(4);
                e.u32(*guild);
                e.u64(*character);
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Guild {
                id: d.u32()?,
                name: read_name(d)?,
            },
            2 => Self::NoGuild { id: d.u32()? },
            3 => Self::Member {
                guild: d.u32()?,
                character: d.u64()?,
                rank: read_rank(d)?,
                since: d.u64()?,
            },
            4 => Self::NoMember {
                guild: d.u32()?,
                character: d.u64()?,
            },
            _ => return Err(DecodeError::Invalid("guild change")),
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Guild {
    name: String,
    /// Member to (rank, since).
    members: BTreeMap<u64, (u8, u64)>,
}

/// What one guild operation did: updates to project, and the durable
/// changes to write before any of them is sent.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GuildOutcome {
    /// What to tell whom.
    pub updates: Vec<GuildUpdate>,
    /// Rows to make durable first.
    pub changes: Vec<GuildChange>,
}

/// Every guild and open invitation: the authority. Guilds are durable:
/// the book is rebuilt from its rows ([`GuildBook::from_rows`]);
/// invitations are not (they lapse with a restart).
#[derive(Clone, Debug, Default)]
pub struct GuildBook {
    next: u32,
    since: u64,
    guilds: BTreeMap<u32, Guild>,
    of: BTreeMap<u64, u32>,
    invites: BTreeMap<u64, (u32, u64, u64)>,
}

impl GuildBook {
    /// A book rebuilt from durable rows, in the order they were written.
    #[must_use]
    pub fn from_rows(rows: &[GuildChange]) -> Self {
        let mut b = Self::default();
        for r in rows {
            b.change(r);
        }
        b
    }

    /// Applies one durable change (loading, and every operation).
    fn change(&mut self, c: &GuildChange) {
        match c {
            GuildChange::Guild { id, name } => {
                self.guilds.entry(*id).or_default().name.clone_from(name);
                self.next = self.next.max(*id);
            }
            GuildChange::NoGuild { id } => {
                if let Some(g) = self.guilds.remove(id) {
                    for m in g.members.keys() {
                        self.of.remove(m);
                    }
                }
            }
            GuildChange::Member {
                guild,
                character,
                rank,
                since,
            } => {
                if let Some(g) = self.guilds.get_mut(guild) {
                    g.members.insert(*character, (*rank, *since));
                    self.of.insert(*character, *guild);
                    self.since = self.since.max(*since);
                }
            }
            GuildChange::NoMember { guild, character } => {
                if let Some(g) = self.guilds.get_mut(guild) {
                    g.members.remove(character);
                }
                if self.of.get(character) == Some(guild) {
                    self.of.remove(character);
                }
            }
        }
    }

    /// The guild `character` is in.
    #[must_use]
    pub fn guild_of(&self, character: u64) -> Option<u32> {
        self.of.get(&character).copied()
    }

    /// `character`'s rank in its guild.
    #[must_use]
    pub fn rank_of(&self, character: u64) -> Option<u8> {
        let g = self.guilds.get(&self.guild_of(character)?)?;
        g.members.get(&character).map(|(r, _)| *r)
    }

    /// A guild's name.
    #[must_use]
    pub fn name_of(&self, guild: u32) -> Option<&str> {
        self.guilds.get(&guild).map(|g| g.name.as_str())
    }

    /// A guild's members, in character order.
    #[must_use]
    pub fn members(&self, guild: u32) -> Vec<u64> {
        self.guilds
            .get(&guild)
            .map(|g| g.members.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Guilds in the book.
    #[must_use]
    pub fn len(&self) -> usize {
        self.guilds.len()
    }

    /// True with no guild.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guilds.is_empty()
    }

    /// Every durable row of the book (what the writer holds).
    #[must_use]
    pub fn rows(&self) -> Vec<GuildChange> {
        let mut out = Vec::new();
        for (id, g) in &self.guilds {
            out.push(GuildChange::Guild {
                id: *id,
                name: g.name.clone(),
            });
            for (m, (rank, since)) in &g.members {
                out.push(GuildChange::Member {
                    guild: *id,
                    character: *m,
                    rank: *rank,
                    since: *since,
                });
            }
        }
        out
    }

    /// What `character` is told about its guild on arriving in a cell:
    /// membership, then the roster in chunks. Empty with no guild.
    #[must_use]
    pub fn arrived(&self, character: u64) -> Vec<GuildUpdate> {
        let mut out = Vec::new();
        let Some(id) = self.guild_of(character) else {
            return out;
        };
        let Some(g) = self.guilds.get(&id) else {
            return out;
        };
        let rank = g.members.get(&character).map_or(guild_rank::MEMBER, |(r, _)| *r);
        out.push(GuildUpdate::Joined {
            to: character,
            guild: id,
            name: g.name.clone(),
            rank,
        });
        let all: Vec<(u64, u8)> = g.members.iter().map(|(m, (r, _))| (*m, *r)).collect();
        for (i, chunk) in all.chunks(ROSTER_CHUNK).enumerate() {
            out.push(GuildUpdate::Members {
                to: character,
                guild: id,
                first: i == 0,
                members: chunk.to_vec(),
            });
        }
        out
    }

    /// Ops: puts `character` into `guild` as a member without an
    /// invitation. `None` when it is in a guild, or the guild is missing or
    /// full.
    pub fn admit(&mut self, character: u64, guild: u32) -> Option<GuildOutcome> {
        let open = self
            .guilds
            .get(&guild)
            .is_some_and(|g| g.members.len() < GUILD_MAX);
        if !open || self.guild_of(character).is_some() || character == 0 {
            return None;
        }
        self.since = self.since.saturating_add(1);
        let join = GuildChange::Member {
            guild,
            character,
            rank: guild_rank::MEMBER,
            since: self.since,
        };
        let mut out = GuildOutcome::default();
        self.commit(guild, vec![join], character, &mut out);
        out.updates.extend(self.arrived(character));
        Some(out)
    }

    fn name_taken(&self, name: &str) -> bool {
        self.guilds.values().any(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// Records `changes` and tells every member of `guild` but `except`
    /// about each changed membership.
    fn commit(&mut self, guild: u32, changes: Vec<GuildChange>, except: u64, out: &mut GuildOutcome) {
        for c in &changes {
            self.change(c);
        }
        let members = self.members(guild);
        for c in &changes {
            match *c {
                GuildChange::Member { character, rank, .. } => {
                    for to in members.iter().filter(|m| **m != except) {
                        out.updates.push(GuildUpdate::Member {
                            to: *to,
                            guild,
                            member: character,
                            rank,
                        });
                    }
                }
                GuildChange::NoMember { character, .. } => {
                    for to in members.iter().chain(core::iter::once(&character)) {
                        out.updates.push(GuildUpdate::Gone {
                            to: *to,
                            guild,
                            member: character,
                        });
                    }
                }
                GuildChange::Guild { .. } | GuildChange::NoGuild { .. } => {}
            }
        }
        out.changes.extend(changes);
    }

    /// The member who leads after `leaving`: the highest rank, then the
    /// longest standing.
    fn successor(&self, guild: u32, leaving: u64) -> Option<(u64, u64)> {
        let g = self.guilds.get(&guild)?;
        g.members
            .iter()
            .filter(|(m, _)| **m != leaving)
            .min_by_key(|(m, (rank, since))| (*rank, *since, **m))
            .map(|(m, (_, since))| (*m, *since))
    }

    fn disband(&mut self, id: u32, out: &mut GuildOutcome) {
        for to in self.members(id) {
            out.updates.push(GuildUpdate::Disbanded { to, guild: id });
        }
        let c = GuildChange::NoGuild { id };
        self.change(&c);
        out.changes.push(c);
        self.invites.retain(|_, (g, _, _)| *g != id);
    }

    /// Applies one operation at `now_ms`. The caller makes the outcome's
    /// changes durable before sending any of its updates.
    pub fn apply(&mut self, op: &GuildOp, now_ms: u64) -> GuildOutcome {
        self.invites.retain(|_, (_, _, until)| *until > now_ms);
        let (me, code, done) = match op {
            GuildOp::Create { me, name } => (*me, guild_op::CREATE, self.create(*me, name)),
            GuildOp::Invite { me, to } => (*me, guild_op::INVITE, self.invite(*me, *to, now_ms)),
            GuildOp::Accept { me, guild } => (*me, guild_op::ACCEPT, self.accept(*me, *guild)),
            GuildOp::Leave { me } => (*me, guild_op::LEAVE, self.leave(*me)),
            GuildOp::Kick { me, who } => (*me, guild_op::KICK, self.kick(*me, *who)),
            GuildOp::SetRank { me, who, rank } => (*me, guild_op::SET_RANK, self.set_rank(*me, *who, *rank)),
            GuildOp::Disband { me } => (*me, guild_op::DISBAND, self.disband_by(*me)),
        };
        done.unwrap_or_else(|| GuildOutcome {
            updates: vec![GuildUpdate::Refused { to: me, op: code }],
            changes: Vec::new(),
        })
    }

    fn create(&mut self, me: u64, name: &str) -> Option<GuildOutcome> {
        if !guild_name_ok(name) || self.guild_of(me).is_some() || self.name_taken(name) {
            return None;
        }
        let id = self.next.saturating_add(1);
        self.since = self.since.saturating_add(1);
        let changes = vec![
            GuildChange::Guild {
                id,
                name: name.to_owned(),
            },
            GuildChange::Member {
                guild: id,
                character: me,
                rank: guild_rank::LEADER,
                since: self.since,
            },
        ];
        for c in &changes {
            self.change(c);
        }
        Some(GuildOutcome {
            updates: self.arrived(me),
            changes,
        })
    }

    fn invite(&mut self, me: u64, to: u64, now_ms: u64) -> Option<GuildOutcome> {
        let id = self.guild_of(me)?;
        let officer = self.rank_of(me).is_some_and(|r| r <= guild_rank::OFFICER);
        let full = self.members(id).len() >= GUILD_MAX;
        if to == me || to == 0 || self.guild_of(to).is_some() || !officer || full {
            return None;
        }
        self.invites.insert(to, (id, me, now_ms + GUILD_INVITE_MS));
        Some(GuildOutcome {
            updates: vec![GuildUpdate::Invited {
                to,
                guild: id,
                name: self.name_of(id).unwrap_or("").to_owned(),
                from: me,
            }],
            changes: Vec::new(),
        })
    }

    fn accept(&mut self, me: u64, guild: u32) -> Option<GuildOutcome> {
        let invited = self.invites.get(&me).is_some_and(|(g, _, _)| *g == guild);
        let open = self
            .guilds
            .get(&guild)
            .is_some_and(|g| g.members.len() < GUILD_MAX);
        if !invited || !open || self.guild_of(me).is_some() {
            return None;
        }
        self.invites.remove(&me);
        self.since = self.since.saturating_add(1);
        let join = GuildChange::Member {
            guild,
            character: me,
            rank: guild_rank::MEMBER,
            since: self.since,
        };
        let mut out = GuildOutcome::default();
        self.commit(guild, vec![join], me, &mut out);
        out.updates.extend(self.arrived(me));
        Some(out)
    }

    fn leave(&mut self, me: u64) -> Option<GuildOutcome> {
        let id = self.guild_of(me)?;
        let mut out = GuildOutcome::default();
        if self.members(id).len() <= 1 {
            self.disband(id, &mut out);
            return Some(out);
        }
        let mut changes = vec![GuildChange::NoMember {
            guild: id,
            character: me,
        }];
        if self.rank_of(me) == Some(guild_rank::LEADER)
            && let Some((next, since)) = self.successor(id, me)
        {
            changes.push(GuildChange::Member {
                guild: id,
                character: next,
                rank: guild_rank::LEADER,
                since,
            });
        }
        self.commit(id, changes, 0, &mut out);
        Some(out)
    }

    fn kick(&mut self, me: u64, who: u64) -> Option<GuildOutcome> {
        let id = self.guild_of(me)?;
        let outranks = matches!(
            (self.rank_of(me), self.rank_of(who)),
            (Some(a), Some(b)) if a <= guild_rank::OFFICER && a < b
        );
        if who == me || self.guild_of(who) != Some(id) || !outranks {
            return None;
        }
        let mut out = GuildOutcome::default();
        let gone = GuildChange::NoMember {
            guild: id,
            character: who,
        };
        self.commit(id, vec![gone], 0, &mut out);
        Some(out)
    }

    fn set_rank(&mut self, me: u64, who: u64, rank: u8) -> Option<GuildOutcome> {
        let id = self.guild_of(me)?;
        let leads = self.rank_of(me) == Some(guild_rank::LEADER);
        if who == me || self.guild_of(who) != Some(id) || !leads || rank > guild_rank::MEMBER {
            return None;
        }
        let since = |b: &Self, c: u64| {
            b.guilds
                .get(&id)
                .and_then(|g| g.members.get(&c))
                .map_or(0, |(_, s)| *s)
        };
        let mut changes = vec![GuildChange::Member {
            guild: id,
            character: who,
            rank,
            since: since(self, who),
        }];
        if rank == guild_rank::LEADER {
            changes.push(GuildChange::Member {
                guild: id,
                character: me,
                rank: guild_rank::OFFICER,
                since: since(self, me),
            });
        }
        let mut out = GuildOutcome::default();
        self.commit(id, changes, 0, &mut out);
        Some(out)
    }

    fn disband_by(&mut self, me: u64) -> Option<GuildOutcome> {
        let id = self.guild_of(me)?;
        if self.rank_of(me) != Some(guild_rank::LEADER) {
            return None;
        }
        let mut out = GuildOutcome::default();
        self.disband(id, &mut out);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{decode_exact, encode_into};

    fn round<T: Wire + PartialEq + Clone + core::fmt::Debug>(v: &T) {
        let mut b = Vec::new();
        encode_into(v, &mut b);
        assert_eq!(decode_exact::<T>(&b).ok(), Some(v.clone()));
    }

    #[test]
    fn wire_forms_round_trip() {
        round(&PartyOp::Restore {
            party: 3,
            leader: 1,
            members: vec![1, 2],
        });
        round(&PartyUpdate::Roster {
            to: 2,
            party: 3,
            leader: 1,
            members: vec![1, 2],
        });
        round(&FriendOp::Respond {
            me: 1,
            other: 2,
            accept: true,
        });
        round(&FriendUpdate::Pending {
            to: 1,
            incoming: vec![4],
            outgoing: vec![5, 6],
        });
    }

    #[test]
    fn parties_form_pass_the_lead_and_disband() {
        let mut b = PartyBook::default();
        assert_eq!(
            b.apply(&PartyOp::Invite { from: 1, to: 2 }, 0),
            vec![PartyUpdate::Invited { to: 2, from: 1 }]
        );
        assert_eq!(b.apply(&PartyOp::Accept { me: 2, from: 1 }, 1).len(), 2);
        assert_eq!(b.party_of(2), Some(1));
        // An expired invitation cannot be accepted.
        b.apply(&PartyOp::Invite { from: 1, to: 3 }, 10);
        let late = b.apply(&PartyOp::Accept { me: 3, from: 1 }, 10 + INVITE_MS + 1);
        assert_eq!(
            late,
            vec![PartyUpdate::Refused {
                to: 3,
                op: party_op::ACCEPT
            }]
        );
        // The leader leaves: the lead passes, and a party of one disbands.
        let out = b.apply(&PartyOp::Leave { me: 1 }, 20);
        assert!(out.contains(&PartyUpdate::Left { to: 1, party: 1 }));
        assert!(out.contains(&PartyUpdate::Left { to: 2, party: 1 }));
        assert_eq!(b.party_of(2), None);
        // A restored projection is adopted once, and ids move past it.
        b.apply(
            &PartyOp::Restore {
                party: 9,
                leader: 4,
                members: vec![4, 5],
            },
            30,
        );
        assert_eq!(b.party_of(5), Some(9));
        b.apply(&PartyOp::Invite { from: 6, to: 7 }, 31);
        b.apply(&PartyOp::Accept { me: 7, from: 6 }, 32);
        assert_eq!(b.party_of(7), Some(10));
    }

    #[test]
    fn friendship_is_mutual_and_requests_are_bounded() {
        let mut b = FriendBook::default();
        b.apply(&FriendOp::Request { me: 1, other: 2 });
        let out = b.apply(&FriendOp::Request { me: 2, other: 1 });
        assert!(b.are_friends(1, 2) && b.are_friends(2, 1));
        assert!(out.updates.contains(&FriendUpdate::List {
            to: 1,
            friends: vec![2]
        }));
        for asker in 10..10 + PENDING_MAX as u64 {
            b.apply(&FriendOp::Request { me: asker, other: 3 });
        }
        assert_eq!(
            b.apply(&FriendOp::Request { me: 99, other: 3 }).updates,
            vec![FriendUpdate::Refused {
                to: 99,
                op: friend_op::REQUEST
            }]
        );
        let out = b.apply(&FriendOp::Respond {
            me: 3,
            other: 10,
            accept: false,
        });
        assert!(out.updates.contains(&FriendUpdate::Declined { to: 10, by: 3 }));
        // Every change is a durable row; the rows rebuild the book.
        let mut rows = b.rows();
        let out = b.apply(&FriendOp::Remove { me: 1, other: 2 });
        assert_eq!(out.changes, vec![FriendChange::NoFriends { a: 1, b: 2 }]);
        rows.extend(out.changes);
        assert!(!b.are_friends(2, 1));
        let rebuilt = FriendBook::from_rows(&rows);
        assert_eq!(rebuilt.rows(), b.rows());
        assert_eq!(
            rebuilt.snapshot(3),
            b.snapshot(3),
            "open requests are durable too"
        );
    }

    fn refused_op(out: &GuildOutcome) -> Option<u8> {
        match out.updates.as_slice() {
            [GuildUpdate::Refused { op, .. }] if out.changes.is_empty() => Some(*op),
            _ => None,
        }
    }

    #[test]
    fn guild_wire_forms_round_trip_and_bound_their_lists() {
        round(&GuildOp::Create {
            me: 1,
            name: "Night Owls".to_owned(),
        });
        round(&GuildOp::SetRank {
            me: 1,
            who: 2,
            rank: guild_rank::OFFICER,
        });
        round(&GuildUpdate::Members {
            to: 3,
            guild: 4,
            first: true,
            members: vec![(1, 0), (2, 2)],
        });
        round(&GuildUpdate::Invited {
            to: 3,
            guild: 4,
            name: "abc".to_owned(),
            from: 1,
        });
        round(&GuildChange::Member {
            guild: 4,
            character: 9,
            rank: guild_rank::MEMBER,
            since: 12,
        });
        // A rank past MEMBER, or an over-long name, is refused.
        let mut b = Vec::new();
        crate::wire::encode_into(
            &GuildChange::Member {
                guild: 1,
                character: 1,
                rank: 1,
                since: 1,
            },
            &mut b,
        );
        b[13] = 9;
        assert!(decode_exact::<GuildChange>(&b).is_err());
        assert!(decode_exact::<GuildOp>(&[guild_op::CREATE, 1, 0, 0, 0, 0, 0, 0, 0, 30]).is_err());
    }

    #[test]
    fn guild_names_are_checked_and_unique_ignoring_case() {
        for ok in ["abc", "Night Owls", "a-b-c 12", "X".repeat(24).as_str()] {
            assert!(guild_name_ok(ok), "{ok}");
        }
        for bad in [
            "ab",
            " abc",
            "abc ",
            "a  b",
            "a_b",
            "abc!",
            "é-name",
            "X".repeat(25).as_str(),
        ] {
            assert!(!guild_name_ok(bad), "{bad}");
        }
        let mut b = GuildBook::default();
        let out = b.apply(
            &GuildOp::Create {
                me: 1,
                name: "Night Owls".to_owned(),
            },
            0,
        );
        assert_eq!(out.changes.len(), 2, "the guild and its leader are durable rows");
        assert!(matches!(
            out.updates[0],
            GuildUpdate::Joined {
                to: 1,
                guild: 1,
                rank: guild_rank::LEADER,
                ..
            }
        ));
        let again = b.apply(
            &GuildOp::Create {
                me: 2,
                name: "night owls".to_owned(),
            },
            0,
        );
        assert_eq!(refused_op(&again), Some(guild_op::CREATE));
        let twice = b.apply(
            &GuildOp::Create {
                me: 1,
                name: "Other".to_owned(),
            },
            0,
        );
        assert_eq!(
            refused_op(&twice),
            Some(guild_op::CREATE),
            "one guild per character"
        );
    }

    #[test]
    fn guild_ranks_govern_invites_removal_and_the_lead() {
        let mut b = GuildBook::default();
        b.apply(
            &GuildOp::Create {
                me: 1,
                name: "Guild".to_owned(),
            },
            0,
        );
        for c in [2, 3, 4] {
            b.apply(&GuildOp::Invite { me: 1, to: c }, 0);
            let out = b.apply(&GuildOp::Accept { me: c, guild: 1 }, 1);
            assert_eq!(out.changes.len(), 1);
            assert!(
                out.updates
                    .iter()
                    .any(|u| matches!(u, GuildUpdate::Joined { to, .. } if *to == c))
            );
        }
        assert_eq!(b.members(1), vec![1, 2, 3, 4]);
        // A member may not invite; an officer may.
        assert_eq!(
            refused_op(&b.apply(&GuildOp::Invite { me: 2, to: 5 }, 2)),
            Some(guild_op::INVITE)
        );
        b.apply(
            &GuildOp::SetRank {
                me: 1,
                who: 2,
                rank: guild_rank::OFFICER,
            },
            2,
        );
        assert!(refused_op(&b.apply(&GuildOp::Invite { me: 2, to: 5 }, 3)).is_none());
        // An expired invitation cannot be accepted.
        let late = b.apply(&GuildOp::Accept { me: 5, guild: 1 }, 3 + GUILD_INVITE_MS + 1);
        assert_eq!(refused_op(&late), Some(guild_op::ACCEPT));
        // An officer removes members but not the leader or another officer.
        assert_eq!(
            refused_op(&b.apply(&GuildOp::Kick { me: 2, who: 1 }, 4)),
            Some(guild_op::KICK)
        );
        let out = b.apply(&GuildOp::Kick { me: 2, who: 4 }, 4);
        assert!(out.updates.contains(&GuildUpdate::Gone {
            to: 4,
            guild: 1,
            member: 4
        }));
        assert_eq!(b.guild_of(4), None);
        // The leader leaves: the officer leads, before an older member.
        let out = b.apply(&GuildOp::Leave { me: 1 }, 5);
        assert_eq!(b.rank_of(2), Some(guild_rank::LEADER));
        assert!(out.updates.contains(&GuildUpdate::Member {
            to: 3,
            guild: 1,
            member: 2,
            rank: guild_rank::LEADER
        }));
        // Passing the lead makes the old leader an officer.
        b.apply(
            &GuildOp::SetRank {
                me: 2,
                who: 3,
                rank: guild_rank::LEADER,
            },
            6,
        );
        assert_eq!(
            (b.rank_of(3), b.rank_of(2)),
            (Some(guild_rank::LEADER), Some(guild_rank::OFFICER))
        );
        // Only the leader disbands; everyone is told.
        assert_eq!(
            refused_op(&b.apply(&GuildOp::Disband { me: 2 }, 7)),
            Some(guild_op::DISBAND)
        );
        let out = b.apply(&GuildOp::Disband { me: 3 }, 7);
        assert_eq!(out.changes, vec![GuildChange::NoGuild { id: 1 }]);
        assert_eq!(out.updates.len(), 2);
        assert!(b.is_empty() && b.guild_of(2).is_none());
    }

    #[test]
    fn a_guild_book_rebuilds_from_its_rows_and_rosters_come_in_chunks() {
        let mut b = GuildBook::default();
        let mut rows = Vec::new();
        rows.extend(
            b.apply(
                &GuildOp::Create {
                    me: 1,
                    name: "Big Guild".to_owned(),
                },
                0,
            )
            .changes,
        );
        for c in 2..=(ROSTER_CHUNK as u64 + 10) {
            rows.extend(b.apply(&GuildOp::Invite { me: 1, to: c }, 0).changes);
            rows.extend(b.apply(&GuildOp::Accept { me: c, guild: 1 }, 0).changes);
        }
        rows.extend(b.apply(&GuildOp::Leave { me: 1 }, 0).changes);
        let rebuilt = GuildBook::from_rows(&rows);
        assert_eq!(rebuilt.rows(), b.rows());
        assert_eq!(rebuilt.rank_of(2), Some(guild_rank::LEADER));
        // New ids and join order continue past the rows.
        let mut rebuilt = rebuilt;
        let out = rebuilt.apply(
            &GuildOp::Create {
                me: 999,
                name: "Next".to_owned(),
            },
            0,
        );
        assert!(matches!(out.updates[0], GuildUpdate::Joined { guild: 2, .. }));
        let roster: Vec<_> = rebuilt.arrived(2);
        let chunks: Vec<usize> = roster
            .iter()
            .filter_map(|u| match u {
                GuildUpdate::Members { members, .. } => Some(members.len()),
                _ => None,
            })
            .collect();
        assert_eq!(chunks, vec![ROSTER_CHUNK, 9]);
        assert!(matches!(roster[1], GuildUpdate::Members { first: true, .. }));
    }
}
