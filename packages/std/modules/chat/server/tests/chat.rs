//! std.chat end to end: local range, party channel through the party
//! contract, whispers, validation, the rate limit, and flags. The party is a
//! stand-in implementing the `std.party` contract: a module depends on
//! contracts, never on another module's implementation.

#![expect(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_core::wire::{Message, WireString, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, Registrar, RegistryError, ServerModule};
use mantis_server::service::{SOCIAL_DELIVER, SOCIAL_PUBLISH, SocialLine, channel as social};
use std_chat_contract::{LOCAL, Line, PARTY, Say, WHISPER};
use std_party_contract::{PartyOf, PartyView};

const CHAT: &str = include_str!("../../manifest.toml");

/// Implements the std.party contract with one fixed party: 11 and 13.
struct StandInParty;

impl ServerModule for StandInParty {
    fn key(&self) -> &'static str {
        "test.party"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.query::<PartyOf>(|_, q| [11, 13].contains(&q.0).then(|| PartyView::new(1, 11, &[11, 13])))
    }
}

const STAND_IN: &str = "[module]\nkey = \"test.party\"\nversion = \"0.1.0\"\ncontract = \"std.party\"\n";

fn bed(flags: &[(&str, bool)]) -> Harness {
    let mut h = Harness::new(
        &[Arc::new(std_chat_server::Module), Arc::new(StandInParty)],
        &[CHAT, STAND_IN],
        flags,
        &[],
    )
    .unwrap();
    // 1 and 2 stand together; 3 is 100 m away.
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 10.0, 0.0);
    h.join(3, 13, 100.0, 0.0);
    h.tick().unwrap();
    h
}

fn say(h: &mut Harness, s: u64, channel: u8, target: u64, text: &str) {
    let msg = Say {
        channel,
        target,
        text: WireString::new(text).unwrap(),
    };
    h.send(s, Say::ID.0, &bytes_of(&msg));
}

fn lines(h: &mut Harness, s: u64) -> Vec<(u8, u64, String)> {
    h.take(s)
        .messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(Line::ID.0))
        .map(|(_, b)| {
            let l = decode_message::<Line>(b).unwrap();
            (l.channel, l.from, l.text.as_str().to_owned())
        })
        .collect()
}

#[test]
fn local_lines_reach_only_characters_in_range() {
    let mut h = bed(&[]);
    say(&mut h, 1, LOCAL, 0, "hello there");
    h.tick().unwrap();
    assert_eq!(lines(&mut h, 1), [(LOCAL, 11, "hello there".to_owned())]);
    assert_eq!(lines(&mut h, 2), [(LOCAL, 11, "hello there".to_owned())]);
    assert!(lines(&mut h, 3).is_empty());
}

#[test]
fn party_lines_use_the_party_contract() {
    let mut h = bed(&[]);
    say(&mut h, 2, PARTY, 0, "anyone?");
    h.tick().unwrap();
    assert_eq!(
        h.take(2).refusals,
        [(ExtensionKind(Say::ID.0), ExtensionRefusal::NotAllowed)],
        "12 has no party"
    );
    say(&mut h, 1, PARTY, 0, "regroup");
    h.tick().unwrap();
    assert_eq!(
        lines(&mut h, 3),
        [(PARTY, 11, "regroup".to_owned())],
        "range does not matter"
    );
    assert!(lines(&mut h, 2).is_empty(), "not in the party");
    // With the party provider off, the party channel is a disabled feature.
    h.set_enabled("test.party", false);
    h.tick().unwrap();
    h.take(1);
    say(&mut h, 1, PARTY, 0, "still there?");
    h.tick().unwrap();
    assert_eq!(
        h.take(1).refusals,
        [(ExtensionKind(Say::ID.0), ExtensionRefusal::NotAllowed)]
    );
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn whispers_reach_one_character_and_echo() {
    let mut h = bed(&[]);
    say(&mut h, 2, WHISPER, 13, "psst");
    h.tick().unwrap();
    assert_eq!(lines(&mut h, 3), [(WHISPER, 12, "psst".to_owned())]);
    assert_eq!(lines(&mut h, 2), [(WHISPER, 12, "psst".to_owned())]);
    assert!(lines(&mut h, 1).is_empty());
    // A character in no session here: the line crosses cells through the
    // social role (an output, not logged), and the speaker sees it.
    say(&mut h, 2, WHISPER, 99, "far away");
    h.tick().unwrap();
    assert!(h.take(2).refusals.is_empty());
    let out: Vec<SocialLine> = h
        .to_services
        .iter()
        .filter(|(t, _)| *t == SOCIAL_PUBLISH)
        .map(|(_, b)| SocialLine::parse(b).unwrap())
        .collect();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].channel, out[0].from, out[0].to),
        (social::WHISPER, 12, 99)
    );
    assert_eq!(out[0].text.as_str(), "far away");
    // A line social delivered here, from another cell, reaches its target.
    let from_afar = SocialLine {
        channel: social::WHISPER,
        from: 77,
        to: 13,
        text: WireString::new("hello from afar").unwrap(),
    };
    let mut bytes = Vec::new();
    mantis_core::wire::encode_into(&from_afar, &mut bytes);
    assert!(h.service_update(SOCIAL_DELIVER, &bytes));
    h.tick().unwrap();
    assert_eq!(lines(&mut h, 3), [(WHISPER, 77, "hello from afar".to_owned())]);
    assert!(h.replay().unwrap() > 0, "a cell replays without the services");
    let mut off = bed(&[("std.chat.whispers", false)]);
    say(&mut off, 2, WHISPER, 13, "psst");
    off.tick().unwrap();
    assert_eq!(
        off.take(2).refusals,
        [(ExtensionKind(Say::ID.0), ExtensionRefusal::NotAllowed)]
    );
}

#[test]
fn bad_lines_and_floods_are_refused() {
    let mut h = bed(&[]);
    say(&mut h, 1, LOCAL, 0, "   ");
    say(&mut h, 1, 7, 0, "where");
    say(&mut h, 1, LOCAL, 0, "bell\u{7}");
    h.tick().unwrap();
    assert_eq!(h.take(1).refusals.len(), 3);
    for i in 0..7 {
        say(&mut h, 2, LOCAL, 0, &format!("line {i}"));
    }
    h.tick().unwrap();
    let got = h.take(2);
    assert_eq!(got.messages.len(), 5, "five lines per window");
    assert_eq!(got.refusals.len(), 2);
    h.ticks(5 * 30).unwrap();
    say(&mut h, 2, LOCAL, 0, "again");
    h.tick().unwrap();
    assert_eq!(lines(&mut h, 2).len(), 1, "a new window");
    assert_eq!(h.cell.metrics().unwrap().get("std.chat.lines"), Some(6));
}

#[test]
fn a_whisper_names_its_recipient() {
    let mut h = bed(&[]);
    say(&mut h, 2, WHISPER, 13, "psst");
    say(&mut h, 2, LOCAL, 0, "hi");
    h.tick().unwrap();
    let got: Vec<(u8, u64)> = h
        .take(2)
        .messages
        .iter()
        .map(|(_, b)| {
            let l = decode_message::<Line>(b).unwrap();
            (l.channel, l.to)
        })
        .collect();
    assert_eq!(got, [(WHISPER, 13), (LOCAL, 0)]);
}

const GUILD_STAND_IN: &str =
    "[module]\nkey = \"test.guild\"\nversion = \"0.1.0\"\ncontract = \"std.guild\"\n";

/// Implements the std.guild contract: 11 and 12 are in guild 5.
struct StandInGuild;

impl ServerModule for StandInGuild {
    fn key(&self) -> &'static str {
        "test.guild"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.query::<std_guild_contract::GuildOf>(|_, q| {
            [11, 12]
                .contains(&q.0)
                .then_some(std_guild_contract::GuildView { id: 5, rank: 2 })
        })
    }
}

#[test]
fn the_guild_channel_follows_whether_the_package_has_guilds() {
    use mantis_server::service::SocialLine as Wire;
    // No guild module: resolved absent at start; the channel is refused,
    // and nothing is asked or sent.
    let mut h = bed(&[]);
    let graph = h.graph().render();
    assert!(graph.contains("optional std.guild: absent"), "{graph}");
    say(&mut h, 1, std_chat_contract::GUILD, 0, "anyone?");
    h.tick().unwrap();
    assert_eq!(h.take(1).refusals[0].1, ExtensionRefusal::NotAllowed);
    assert!(h.to_services.is_empty());

    // With one: the speaker sees the line and social carries it.
    let mut h = Harness::new(
        &[
            Arc::new(std_chat_server::Module),
            Arc::new(StandInParty),
            Arc::new(StandInGuild),
        ],
        &[CHAT, STAND_IN, GUILD_STAND_IN],
        &[],
        &[],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(3, 13, 100.0, 0.0);
    h.tick().unwrap();
    say(&mut h, 1, std_chat_contract::GUILD, 0, "meet at the gate");
    h.tick().unwrap();
    assert_eq!(
        lines(&mut h, 1),
        vec![(std_chat_contract::GUILD, 11, "meet at the gate".to_owned())]
    );
    let (topic, bytes) = h.to_services.last().unwrap().clone();
    assert_eq!(topic, SOCIAL_PUBLISH);
    let out = Wire::parse(&bytes).unwrap();
    assert_eq!((out.channel, out.to), (social::GUILD, 5));
    // 13 has no guild.
    say(&mut h, 3, std_chat_contract::GUILD, 0, "me too");
    h.tick().unwrap();
    assert_eq!(h.take(3).refusals[0].1, ExtensionRefusal::NotAllowed);
}
