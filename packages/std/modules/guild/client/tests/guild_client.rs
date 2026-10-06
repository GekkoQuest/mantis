//! The std.guild client half against its contract codec: joining, roster pages, rank
//! changes, and departures drive the view model and the controls each rank sees;
//! intents send the contract's requests; invitations follow the `invites` flag; cell
//! and authority refusals render as text naming the operation; the screen composes.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_client::modules::{ClientModule, ClientModules, ExtensionKind, ExtensionRefusal, IntentRoute};
use mantis_core::ecs::EntityId;
use mantis_core::module::{Discovered, ModuleGraph, parse_manifest, parse_package, resolve};
use mantis_core::wire::{BoundedArray, Message, WireString, decode_message, encode_into};
use mantis_ui::{ListItem, Modifiers, Properties, UiEvent, UiKey, Value};
use std_guild_contract::{
    AcceptGuild, CreateGuild, DisbandGuild, GuildDisbanded, GuildInvited, GuildJoined, GuildMember,
    GuildMemberChanged, GuildMemberGone, GuildRefused, GuildRoster, InviteToGuild, LeaveGuild, OP_SET_RANK,
    RANK_LEADER, RANK_MEMBER, RANK_OFFICER, RemoveFromGuild, SetGuildRank,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str = include_str!("../../manifest.toml");
const ME: u64 = 7;
const GUILD: u32 = 5;

fn graph(flags: &str) -> Result<ModuleGraph, Box<dyn std::error::Error>> {
    let found = [Discovered {
        origin: "std".to_owned(),
        manifest: parse_manifest(MANIFEST)?,
    }];
    let package = parse_package(&format!("[package]\nname = \"std\"\n[flags]\n{flags}"))?;
    Ok(resolve(&package, &found, &BTreeMap::new())?)
}

fn install(flags: &str) -> Result<(ClientModules, Properties), Box<dyn std::error::Error>> {
    let mut modules = ClientModules::new(
        &graph(flags)?,
        &[Arc::new(std_guild_client::Module) as Arc<dyn ClientModule>],
    )?;
    let mut props = Properties::new();
    modules.start(&mut props);
    modules.set_character(ME, &mut props);
    Ok((modules, props))
}

fn deliver<M: Message>(m: &mut ClientModules, props: &mut Properties, msg: &M) {
    let mut bytes = Vec::new();
    encode_into(msg, &mut bytes);
    m.on_message(ExtensionKind(M::ID.0), &bytes, props);
}

fn prop(props: &mut Properties, name: &str) -> Option<Value> {
    let id = props.intern(name);
    props.get(id).cloned()
}

fn text(s: &str) -> Value {
    Value::text(s)
}

fn sent(m: &mut ClientModules) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    m.drain_outbound(|k, b| out.push((k.0, b.to_vec())));
    out
}

fn name(s: &str) -> Result<WireString<24>, Box<dyn std::error::Error>> {
    Ok(WireString::new(s).ok_or("name")?)
}

fn roster(first: bool, members: &[(u64, u8)]) -> Result<GuildRoster, Box<dyn std::error::Error>> {
    let members: Vec<GuildMember> = members
        .iter()
        .map(|(character, rank)| GuildMember {
            character: *character,
            rank: *rank,
        })
        .collect();
    Ok(GuildRoster {
        guild: GUILD,
        first,
        members: BoundedArray::from_slice(&members).ok_or("members")?,
    })
}

fn members(props: &mut Properties) -> Result<Vec<ListItem>, Box<dyn std::error::Error>> {
    match prop(props, "std.guild.members") {
        Some(Value::List(items)) => Ok(items),
        other => Err(format!("members: {other:?}").into()),
    }
}

/// Each member as `character:rank:controls`, controls being the flags set among
/// `removable`, `can_promote`, `can_demote`, `can_lead` (r, p, d, l), and `you` (y).
fn summary(props: &mut Properties) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let flag = |i: &ListItem, f: &str| i.field(f) == Some(&Value::Bool(true));
    Ok(members(props)?
        .iter()
        .map(|i| {
            let mut s = String::new();
            if let (Some(Value::Text(c)), Some(Value::Text(r))) = (i.field("character"), i.field("rank")) {
                s = format!("{c}:{r}:");
            }
            for (f, c) in [
                ("removable", 'r'),
                ("can_promote", 'p'),
                ("can_demote", 'd'),
                ("can_lead", 'l'),
                ("you", 'y'),
            ] {
                if flag(i, f) {
                    s.push(c);
                }
            }
            s
        })
        .collect())
}

fn join(m: &mut ClientModules, props: &mut Properties, rank: u8) -> TestResult {
    deliver(
        m,
        props,
        &GuildJoined {
            guild: GUILD,
            name: name("North Watch")?,
            rank,
        },
    );
    Ok(())
}

#[test]
fn joining_roster_pages_and_rank_changes_drive_the_view_model() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(prop(&mut props, "std.guild.in_guild"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.guild.solo"), Some(Value::Bool(true)));
    join(&mut m, &mut props, RANK_LEADER)?;
    deliver(
        &mut m,
        &mut props,
        &roster(true, &[(ME, RANK_LEADER), (9, RANK_MEMBER), (11, RANK_OFFICER)])?,
    );
    deliver(&mut m, &mut props, &roster(false, &[(13, RANK_MEMBER)])?);
    assert_eq!(prop(&mut props, "std.guild.in_guild"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.guild.name"), Some(text("North Watch")));
    assert_eq!(prop(&mut props, "std.guild.rank"), Some(text("leader")));
    assert_eq!(prop(&mut props, "std.guild.is_leader"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.guild.size"), Some(Value::Int(4)));
    // Leader first, then officers, then members; the leader may act on everyone else.
    assert_eq!(
        summary(&mut props)?,
        ["7:leader:y", "11:officer:rdl", "9:member:rpl", "13:member:rpl"]
    );
    // A roster for another guild is ignored; a new first page replaces the roster.
    let mut other = roster(true, &[(99, RANK_LEADER)])?;
    other.guild = GUILD + 1;
    deliver(&mut m, &mut props, &other);
    assert_eq!(prop(&mut props, "std.guild.size"), Some(Value::Int(4)));

    // The lead passes to 11: two changes, and the controls follow your new rank.
    deliver(
        &mut m,
        &mut props,
        &GuildMemberChanged {
            guild: GUILD,
            character: 11,
            rank: RANK_LEADER,
        },
    );
    deliver(
        &mut m,
        &mut props,
        &GuildMemberChanged {
            guild: GUILD,
            character: ME,
            rank: RANK_OFFICER,
        },
    );
    deliver(
        &mut m,
        &mut props,
        &GuildMemberChanged {
            guild: GUILD,
            character: 9,
            rank: RANK_OFFICER,
        },
    );
    assert_eq!(prop(&mut props, "std.guild.rank"), Some(text("officer")));
    assert_eq!(prop(&mut props, "std.guild.is_leader"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.guild.can_invite"), Some(Value::Bool(true)));
    // An officer removes members only: not another officer, never the leader.
    assert_eq!(
        summary(&mut props)?,
        ["11:leader:", "7:officer:y", "9:officer:", "13:member:r"]
    );
    // A new member joins; another leaves.
    deliver(
        &mut m,
        &mut props,
        &GuildMemberChanged {
            guild: GUILD,
            character: 15,
            rank: RANK_MEMBER,
        },
    );
    deliver(
        &mut m,
        &mut props,
        &GuildMemberGone {
            guild: GUILD,
            character: 13,
        },
    );
    assert_eq!(prop(&mut props, "std.guild.size"), Some(Value::Int(4)));
    // You are removed: the guild is gone from the view model.
    deliver(
        &mut m,
        &mut props,
        &GuildMemberGone {
            guild: GUILD,
            character: ME,
        },
    );
    assert_eq!(prop(&mut props, "std.guild.in_guild"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.guild.size"), Some(Value::Int(0)));
    assert!(members(&mut props)?.is_empty());

    // Joined again, then disbanded.
    join(&mut m, &mut props, RANK_MEMBER)?;
    deliver(
        &mut m,
        &mut props,
        &roster(true, &[(11, RANK_LEADER), (ME, RANK_MEMBER)])?,
    );
    assert_eq!(summary(&mut props)?, ["11:leader:", "7:member:y"]);
    deliver(&mut m, &mut props, &GuildDisbanded { guild: GUILD });
    assert_eq!(prop(&mut props, "std.guild.in_guild"), Some(Value::Bool(false)));
    Ok(())
}

#[test]
fn intents_send_the_contract_requests() -> TestResult {
    let (mut m, mut props) = install("")?;
    // Not in a guild: only founding means anything, with a valid name.
    for (intent, payload) in [
        ("std.guild.leave", None),
        ("std.guild.remove", Some("9")),
        ("std.guild.rank", Some("9 1")),
        ("std.guild.disband", None),
        ("std.guild.create", Some("x")),
        ("std.guild.create", Some("two  spaces")),
    ] {
        assert_eq!(
            m.on_intent(intent, payload, &mut props),
            IntentRoute::Refused,
            "{intent}"
        );
    }
    assert!(sent(&mut m).is_empty());
    assert_eq!(
        m.on_intent("std.guild.create", Some(" North Watch "), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.guild.pending"),
        Some(text("Founding the guild..."))
    );
    let out = sent(&mut m);
    let (kind, bytes) = out.first().cloned().ok_or("nothing sent")?;
    assert_eq!(kind, CreateGuild::ID.0);
    assert_eq!(
        decode_message::<CreateGuild>(&bytes)?,
        CreateGuild {
            name: name("North Watch")?
        }
    );

    join(&mut m, &mut props, RANK_LEADER)?;
    assert_eq!(prop(&mut props, "std.guild.pending"), Some(text("")), "answered");
    assert_eq!(
        m.on_intent("std.guild.create", Some("Second One"), &mut props),
        IntentRoute::Refused,
        "already in a guild"
    );
    for (intent, payload) in [
        ("std.guild.rank", Some("9")),
        ("std.guild.rank", Some("9 3")),
        ("std.guild.rank", Some("nine 1")),
        ("std.guild.rank", Some("9 1 2")),
        ("std.guild.remove", Some("0")),
        ("std.guild.invite", Some("someone")),
    ] {
        assert_eq!(
            m.on_intent(intent, payload, &mut props),
            IntentRoute::Refused,
            "{intent} {payload:?}"
        );
    }
    let target = EntityId::new(4, 2);
    let bits = target.to_bits().to_string();
    for (intent, payload) in [
        ("std.guild.rank", Some("9 1")),
        ("std.guild.remove", Some("13")),
        ("std.guild.invite", Some(bits.as_str())),
        ("std.guild.leave", None),
        ("std.guild.disband", None),
    ] {
        assert_eq!(
            m.on_intent(intent, payload, &mut props),
            IntentRoute::Handled,
            "{intent}"
        );
    }
    let out = sent(&mut m);
    let kinds: Vec<u16> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [
            SetGuildRank::ID.0,
            RemoveFromGuild::ID.0,
            InviteToGuild::ID.0,
            LeaveGuild::ID.0,
            DisbandGuild::ID.0
        ]
    );
    let bytes = |i: usize| out.get(i).map(|(_, b)| b.clone()).ok_or("sent");
    assert_eq!(
        decode_message::<SetGuildRank>(&bytes(0)?)?,
        SetGuildRank {
            character: 9,
            rank: RANK_OFFICER
        }
    );
    assert_eq!(
        decode_message::<RemoveFromGuild>(&bytes(1)?)?,
        RemoveFromGuild { character: 13 }
    );
    assert_eq!(
        decode_message::<InviteToGuild>(&bytes(2)?)?,
        InviteToGuild { target }
    );
    Ok(())
}

#[test]
fn invitations_follow_the_flag() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.guild.accept", None, &mut props),
        IntentRoute::Refused,
        "nothing to accept"
    );
    let invite = GuildInvited {
        guild: GUILD,
        name: name("North Watch")?,
        from: 11,
    };
    deliver(&mut m, &mut props, &invite);
    assert_eq!(prop(&mut props, "std.guild.invited"), Some(Value::Bool(true)));
    assert_eq!(
        prop(&mut props, "std.guild.invite_name"),
        Some(text("North Watch"))
    );
    assert_eq!(prop(&mut props, "std.guild.invite_from"), Some(text("11")));
    assert_eq!(
        m.on_intent("std.guild.accept", None, &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.guild.invited"), Some(Value::Bool(false)));
    let out = sent(&mut m);
    let (kind, bytes) = out.first().cloned().ok_or("nothing sent")?;
    assert_eq!(kind, AcceptGuild::ID.0);
    assert_eq!(
        decode_message::<AcceptGuild>(&bytes)?,
        AcceptGuild { guild: GUILD }
    );

    // Declined locally; a disbanded guild's invitation closes by itself.
    deliver(&mut m, &mut props, &invite);
    assert_eq!(
        m.on_intent("std.guild.decline", None, &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.guild.invited"), Some(Value::Bool(false)));
    deliver(&mut m, &mut props, &invite);
    deliver(&mut m, &mut props, &GuildDisbanded { guild: GUILD });
    assert_eq!(prop(&mut props, "std.guild.invited"), Some(Value::Bool(false)));
    assert!(sent(&mut m).is_empty());

    // With invitations switched off, neither inviting nor accepting is sent.
    let (mut m, mut props) = install("\"std.guild.invites\" = false\n")?;
    assert_eq!(
        prop(&mut props, "std.guild.flag.invites"),
        Some(Value::Bool(false))
    );
    join(&mut m, &mut props, RANK_LEADER)?;
    deliver(&mut m, &mut props, &invite);
    assert_eq!(
        m.on_intent("std.guild.accept", None, &mut props),
        IntentRoute::Refused
    );
    assert_eq!(
        m.on_intent("std.guild.invite", Some("1"), &mut props),
        IntentRoute::Refused
    );
    assert!(sent(&mut m).is_empty());
    Ok(())
}

#[test]
fn refusals_name_the_operation_and_a_disabled_module_greys_out() -> TestResult {
    let (mut m, mut props) = install("")?;
    assert_eq!(
        m.on_intent("std.guild.create", Some("North Watch"), &mut props),
        IntentRoute::Handled
    );
    // The cell refuses the request; the echoed kind names it.
    m.on_refused_tracked(
        ExtensionKind(CreateGuild::ID.0),
        1,
        ExtensionRefusal::NotAllowed,
        &mut props,
    );
    assert_eq!(
        prop(&mut props, "std.guild.refusal"),
        Some(text("Could not found the guild: not allowed"))
    );
    assert_eq!(prop(&mut props, "std.guild.pending"), Some(text("")));
    assert_eq!(m.stats().refusals_answered, 1);
    // The authority refuses a rank change.
    join(&mut m, &mut props, RANK_OFFICER)?;
    assert_eq!(
        m.on_intent("std.guild.rank", Some("9 0"), &mut props),
        IntentRoute::Handled
    );
    assert_eq!(
        prop(&mut props, "std.guild.refusal"),
        Some(text("")),
        "a new request clears it"
    );
    deliver(&mut m, &mut props, &GuildRefused { op: OP_SET_RANK });
    assert_eq!(
        prop(&mut props, "std.guild.refusal"),
        Some(text("Could not change the rank: refused"))
    );
    assert_eq!(
        m.on_intent("std.guild.dismiss", None, &mut props),
        IntentRoute::Handled
    );
    assert_eq!(prop(&mut props, "std.guild.refusal"), Some(text("")));

    // The whole module switched off: unavailable, emptied, and its intents refused.
    m.on_refused(
        ExtensionKind(LeaveGuild::ID.0),
        ExtensionRefusal::FeatureDisabled,
        &mut props,
    );
    assert_eq!(prop(&mut props, "std.guild.enabled"), Some(Value::Bool(false)));
    assert_eq!(prop(&mut props, "std.guild.unavailable"), Some(Value::Bool(true)));
    assert_eq!(prop(&mut props, "std.guild.in_guild"), Some(Value::Bool(false)));
    assert_eq!(
        m.on_intent("std.guild.leave", None, &mut props),
        IntentRoute::Refused
    );
    Ok(())
}

#[test]
fn the_screen_composes_and_enter_founds_a_guild() -> TestResult {
    let (mut m, _) = install("")?;
    let (layout, theme) = m.compose(&["std.guild.roster"]);
    let mut fonts = mantis_ui::FontLibrary::new();
    let latin = fonts.add_font(mantis_ui::test_font::latin())?;
    fonts.define_stack("ui", &[latin])?;
    let mut ui = mantis_ui::Ui::new(fonts, &layout, Some(&theme))?;
    m.start(ui.properties_mut());
    m.set_character(ME, ui.properties_mut());
    let _ = ui.frame([800.0, 600.0], 1.0);
    for id in [
        "std_guild_create",
        "std_guild_members",
        "std_guild_leave",
        "std_guild_accept",
    ] {
        assert!(ui.rect_of(id).is_some(), "{id}");
    }
    let actions: Vec<_> = m.actions().map(|a| a.name).collect();
    assert_eq!(actions, ["std.guild.toggle"]);
    assert!(ui.set_focus("std_guild_create"));
    let _ = ui.handle(&UiEvent::Text("North Watch".to_owned()));
    let _ = ui.handle(&UiEvent::Key {
        key: UiKey::Enter,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    let mut intents = Vec::new();
    ui.drain_intents(&mut intents);
    let intent = intents.first().ok_or("no intent")?;
    let name = ui.intent_name(intent.intent).ok_or("intent name")?.to_owned();
    assert_eq!(name, "std.guild.create");
    assert_eq!(
        m.on_intent(&name, intent.payload.as_deref(), ui.properties_mut()),
        IntentRoute::Handled
    );
    assert_eq!(sent(&mut m).first().map(|(k, _)| *k), Some(CreateGuild::ID.0));
    Ok(())
}
