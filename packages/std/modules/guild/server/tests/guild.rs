//! std.guild in one cell, with the social role's own rules answering on
//! the next tick (the harness): founding, invitations, ranks, removal, the
//! projection and its query, what the cell refuses before relaying, the
//! `invites` flag, the module switched off, and replay.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use mantis_core::module::ask;
use mantis_core::wire::{Message, WireString, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, LIVE_FLAG};
use std_guild_contract::{
    AcceptGuild, CreateGuild, GuildInvited, GuildJoined, GuildMemberChanged, GuildMemberGone, GuildOf,
    GuildRefused, GuildRoster, GuildView, InviteToGuild, OP_INVITE, RANK_LEADER, RANK_MEMBER, RANK_OFFICER,
    RemoveFromGuild, SetGuildRank,
};

const GUILD: &str = include_str!("../../manifest.toml");

fn bed() -> Harness {
    let mut h = Harness::new(&[Arc::new(std_guild_server::Module)], &[GUILD], &[], &[]).unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 1.0, 0.0);
    h.join(3, 13, 2.0, 0.0);
    h.tick().unwrap();
    h
}

fn got<M: Message>(h: &mut Harness, session: u64) -> Vec<M> {
    h.take(session)
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(M::ID.0))
        .map(|(_, b)| decode_message::<M>(b).unwrap())
        .collect()
}

fn create(name: &str) -> Vec<u8> {
    bytes_of(&CreateGuild {
        name: WireString::new(name).unwrap(),
    })
}

/// Founds "Lamplighters" led by 11, with 12 a member.
fn founded(h: &mut Harness) -> u32 {
    h.send(1, CreateGuild::ID.0, &create("Lamplighters"));
    h.ticks(2).unwrap();
    let guild = got::<GuildJoined>(h, 1)[0].guild;
    h.send(
        1,
        InviteToGuild::ID.0,
        &bytes_of(&InviteToGuild {
            target: Harness::avatar(2),
        }),
    );
    h.ticks(2).unwrap();
    h.send(2, AcceptGuild::ID.0, &bytes_of(&AcceptGuild { guild }));
    h.ticks(2).unwrap();
    guild
}

#[test]
fn a_guild_is_founded_joined_ranked_and_projected() {
    let mut h = bed();
    h.send(1, CreateGuild::ID.0, &create("Lamplighters"));
    h.ticks(2).unwrap();
    let joined = got::<GuildJoined>(&mut h, 1);
    assert_eq!(joined.len(), 1);
    let guild = joined[0].guild;
    assert_eq!(
        (joined[0].name.as_str(), joined[0].rank),
        ("Lamplighters", RANK_LEADER)
    );
    assert_eq!(
        ask(h.world(), &GuildOf(11)).unwrap(),
        Some(GuildView {
            id: guild,
            rank: RANK_LEADER
        })
    );

    // An invitation, then joining: the joiner gets its guild and roster,
    // the leader is told of the new member.
    h.send(
        1,
        InviteToGuild::ID.0,
        &bytes_of(&InviteToGuild {
            target: Harness::avatar(2),
        }),
    );
    h.ticks(2).unwrap();
    let invited = got::<GuildInvited>(&mut h, 2);
    assert_eq!((invited[0].guild, invited[0].from), (guild, 11));
    h.send(2, AcceptGuild::ID.0, &bytes_of(&AcceptGuild { guild }));
    h.ticks(2).unwrap();
    let inbox = h.take(2);
    let roster = inbox
        .messages
        .iter()
        .find(|(k, _)| *k == ExtensionKind(GuildRoster::ID.0))
        .map(|(_, b)| decode_message::<GuildRoster>(b).unwrap())
        .unwrap();
    assert!(roster.first);
    let members: Vec<(u64, u8)> = roster.members.iter().map(|m| (m.character, m.rank)).collect();
    assert_eq!(members, vec![(11, RANK_LEADER), (12, RANK_MEMBER)]);
    let told = got::<GuildMemberChanged>(&mut h, 1);
    assert_eq!((told[0].character, told[0].rank), (12, RANK_MEMBER));

    // A member may not invite: the authority refuses.
    h.send(
        2,
        InviteToGuild::ID.0,
        &bytes_of(&InviteToGuild {
            target: Harness::avatar(3),
        }),
    );
    h.ticks(2).unwrap();
    assert_eq!(got::<GuildRefused>(&mut h, 2)[0].op, OP_INVITE);

    // Promoted, 12 is an officer everywhere it is projected.
    h.send(
        1,
        SetGuildRank::ID.0,
        &bytes_of(&SetGuildRank {
            character: 12,
            rank: RANK_OFFICER,
        }),
    );
    h.ticks(2).unwrap();
    assert_eq!(ask(h.world(), &GuildOf(12)).unwrap().unwrap().rank, RANK_OFFICER);

    // Removed, 12 is told and leaves the projection.
    h.take(2);
    h.send(
        1,
        RemoveFromGuild::ID.0,
        &bytes_of(&RemoveFromGuild { character: 12 }),
    );
    h.ticks(2).unwrap();
    let gone = got::<GuildMemberGone>(&mut h, 2);
    assert_eq!((gone[0].guild, gone[0].character), (guild, 12));
    assert_eq!(ask(h.world(), &GuildOf(12)).unwrap(), None);
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn the_cell_refuses_what_it_can_check_before_relaying() {
    let mut h = bed();
    let guild = founded(&mut h);
    h.take(1);
    // A bad name, oneself, a bad rank: refused in the cell, never relayed.
    let relayed = h.to_services.len();
    h.send(3, CreateGuild::ID.0, &create("ab"));
    h.send(
        1,
        RemoveFromGuild::ID.0,
        &bytes_of(&RemoveFromGuild { character: 11 }),
    );
    h.send(
        1,
        SetGuildRank::ID.0,
        &bytes_of(&SetGuildRank {
            character: 12,
            rank: 7,
        }),
    );
    h.send(
        1,
        InviteToGuild::ID.0,
        &bytes_of(&InviteToGuild {
            target: Harness::avatar(1),
        }),
    );
    h.tick().unwrap();
    assert_eq!(h.to_services.len(), relayed);
    let reasons: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        [
            ExtensionRefusal::NotAllowed,
            ExtensionRefusal::Invalid,
            ExtensionRefusal::NotAllowed
        ]
    );
    assert_eq!(h.take(3).refusals[0].1, ExtensionRefusal::Invalid);

    // The invites flag, off: invitations are not allowed; the guild works.
    assert!(h.set_live("std.guild.invites", LIVE_FLAG, 0.0));
    h.tick().unwrap();
    h.send(
        1,
        InviteToGuild::ID.0,
        &bytes_of(&InviteToGuild {
            target: Harness::avatar(3),
        }),
    );
    h.tick().unwrap();
    assert_eq!(h.take(1).refusals[0].1, ExtensionRefusal::NotAllowed);
    assert_eq!(ask(h.world(), &GuildOf(12)).unwrap().unwrap().id, guild);

    // The whole module off: every request is refused as disabled, and the
    // query answers so.
    h.set_enabled("std.guild", false);
    h.tick().unwrap();
    h.send(3, CreateGuild::ID.0, &create("Other Guild"));
    h.tick().unwrap();
    assert_eq!(h.take(3).refusals[0].1, ExtensionRefusal::FeatureDisabled);
    assert!(ask(h.world(), &GuildOf(11)).is_err());
    assert!(h.replay().unwrap() > 0);
}
