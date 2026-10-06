//! Every RPC method, with its caller matrix: which roles may call it.
//! A method's id is its request message id in `schema/services.registry`.

use mantis_core::wire::{Message, MessageId, ValidationError};

use crate::generated::services as m;
use crate::host::Role::{self, Cell, Gateway, Matchmaking, Ops, Realm, Social};
use crate::host::rpc::{Method, RpcError};

macro_rules! method {
    ($(#[$doc:meta])* $name:ident: $req:ident -> $resp:ident, callers [$($role:ident),*]) => {
        $(#[$doc])*
        pub struct $name;
        impl Method for $name {
            const ID: u16 = <m::$req as Message>::ID.0;
            const NAME: &'static str = stringify!($name);
            const CALLERS: &'static [Role] = &[$($role),*];
            type Request = m::$req;
            type Response = m::$resp;
        }
    };
}

method!(/// Create an account.
    RegisterAccount: Register -> Registered, callers [Ops, Gateway]);
method!(/// Log in.
    LoginAccount: Login -> Session, callers [Gateway]);
method!(/// Verify and consume a session token.
    VerifySession: VerifyToken -> Verified, callers [Cell, Realm]);
method!(/// Ban an account.
    BanAccount: Ban -> Banned, callers [Ops]);
method!(/// Maintenance mode.
    Maintenance: SetMaintenance -> MaintenanceWas, callers [Ops]);

method!(/// A cell host registers a cell.
    RegisterCellHost: RegisterCell -> Empty, callers [Cell]);
method!(/// A draining host takes a cell out of the directory.
    Withdraw: WithdrawCell -> Empty, callers [Cell, Ops]);
method!(/// An account's characters.
    ListAccountCharacters: ListCharacters -> Characters, callers [Gateway, Ops]);
method!(/// Create a character.
    NewCharacter: CreateCharacter -> Created, callers [Gateway, Ops]);
method!(/// Select a character: placement and entry token.
    Select: SelectCharacter -> Placement, callers [Gateway]);
method!(/// Create an instance.
    NewInstance: CreateInstance -> InstanceCell, callers [Matchmaking, Cell]);
method!(/// Issue a transfer token.
    Transfer: IssueTransfer -> TransferToken, callers [Cell]);
method!(/// Redeem an entry or transfer token.
    RedeemToken: Redeem -> Redeemed, callers [Cell]);
method!(/// Redeem a token naming any of the calling host's cells (admission).
    RedeemForHost: RedeemOnHost -> Redeemed, callers [Cell]);

method!(/// Publish a line on a cross-cell channel.
    PublishLine: Publish -> Empty, callers [Cell]);
method!(/// A cell tells social which characters it hosts.
    Presence: Present -> Empty, callers [Cell]);
method!(/// A cell polls its projection.
    Poll: PollSocial -> SocialUpdates, callers [Cell]);
method!(/// Create a guild.
    NewGuild: CreateGuild -> Guild, callers [Cell, Ops]);
method!(/// Join a guild.
    EnterGuild: JoinGuild -> Empty, callers [Cell, Ops]);
method!(/// A cell relays a party or friends operation.
    RelayOp: Relay -> RelayAck, callers [Cell]);
method!(/// A host finished restoring a cell's projection after a restart.
    Restored: RestoredCell -> Empty, callers [Cell]);
method!(/// A cell polls the updates projected onto it.
    Projection: PollProjections -> Projections, callers [Cell]);

method!(/// Push a batch of outcomes to the writer.
    Push: PushOutcomes -> Durable, callers [Cell]);
method!(/// Read a character ledger.
    Ledger: LedgerOf -> LedgerRows, callers [Ops]);
method!(/// Social makes guild changes durable.
    WriteGuilds: StoreGuildRows -> Durable, callers [Social]);
method!(/// Social reads the guild rows back.
    LoadGuilds: ReadGuildRows -> GuildRows, callers [Social]);

method!(/// Join a matchmaking queue.
    Queue: Enqueue -> Empty, callers [Gateway, Cell]);
method!(/// Poll for a match.
    MatchFor: PollMatch -> Match, callers [Gateway, Cell]);

method!(/// A cell host polls signed live changes.
    Live: PollLive -> LiveChanges, callers [Cell]);
method!(/// The Ops inspector reads a cell summary (read-only).
    InspectCell: Inspect -> CellSummary, callers [Ops]);
method!(/// Ops ends a character's session on a cell host.
    Kick: KickCharacter -> Kicked, callers [Ops]);
method!(/// Ops drains a cell host for maintenance, or lifts the drain.
    Drain: DrainHost -> Empty, callers [Ops]);
method!(/// The Ops inspector reads a cell's system run times.
    InspectSystemTimes: InspectSystems -> SystemTimes, callers [Ops]);
method!(/// The Ops inspector reads a cell's component names.
    InspectComponentNames: InspectComponents -> ComponentNames, callers [Ops]);
method!(/// The Ops inspector reads a page of a cell's entities.
    InspectEntityPage: InspectEntities -> EntityPage, callers [Ops]);

/// Field checks on every request, before any handler runs.
pub struct Checks;

fn named(s: &str) -> Result<(), ValidationError> {
    let ok = !s.trim().is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ValidationError("names are letters, digits, - and _"))
    }
}

impl m::Validators for Checks {
    fn validate_register(&self, msg: &m::Register) -> Result<(), ValidationError> {
        named(msg.name.as_str())?;
        if msg.password.as_str().len() < 8 {
            return Err(ValidationError("passwords have at least 8 bytes"));
        }
        Ok(())
    }
    fn validate_login(&self, msg: &m::Login) -> Result<(), ValidationError> {
        named(msg.name.as_str())
    }
    fn validate_verify_token(&self, _msg: &m::VerifyToken) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_ban(&self, _msg: &m::Ban) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_set_maintenance(&self, _msg: &m::SetMaintenance) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_withdraw_cell(&self, _msg: &m::WithdrawCell) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_register_cell(&self, msg: &m::RegisterCell) -> Result<(), ValidationError> {
        if msg.cell.0 == 0 || (!msg.instance && msg.lo >= msg.hi) {
            return Err(ValidationError("a world cell owns a non-empty range"));
        }
        Ok(())
    }
    fn validate_list_characters(&self, _msg: &m::ListCharacters) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_create_character(&self, msg: &m::CreateCharacter) -> Result<(), ValidationError> {
        named(msg.name.as_str())
    }
    fn validate_select_character(&self, _msg: &m::SelectCharacter) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_create_instance(&self, _msg: &m::CreateInstance) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_issue_transfer(&self, msg: &m::IssueTransfer) -> Result<(), ValidationError> {
        if msg.from == msg.to {
            return Err(ValidationError("a transfer goes somewhere else"));
        }
        Ok(())
    }
    fn validate_redeem(&self, _msg: &m::Redeem) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_redeem_on_host(&self, msg: &m::RedeemOnHost) -> Result<(), ValidationError> {
        if msg.cells.iter().next().is_none() {
            return Err(ValidationError("a host has at least one cell"));
        }
        Ok(())
    }
    fn validate_publish(&self, msg: &m::Publish) -> Result<(), ValidationError> {
        if !(2..=4).contains(&msg.channel) || msg.text.as_str().trim().is_empty() {
            return Err(ValidationError("channel 2 to 4 and a non-empty line"));
        }
        Ok(())
    }
    fn validate_present(&self, _msg: &m::Present) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_poll_social(&self, _msg: &m::PollSocial) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_create_guild(&self, msg: &m::CreateGuild) -> Result<(), ValidationError> {
        named(msg.name.as_str())
    }
    fn validate_join_guild(&self, _msg: &m::JoinGuild) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_relay(&self, _msg: &m::Relay) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_restored_cell(&self, _msg: &m::RestoredCell) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_poll_projections(&self, _msg: &m::PollProjections) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_push_outcomes(&self, _msg: &m::PushOutcomes) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_ledger_of(&self, _msg: &m::LedgerOf) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_enqueue(&self, _msg: &m::Enqueue) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_poll_match(&self, _msg: &m::PollMatch) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_poll_live(&self, _msg: &m::PollLive) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_inspect(&self, _msg: &m::Inspect) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_kick_character(&self, _msg: &m::KickCharacter) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_drain_host(&self, _msg: &m::DrainHost) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_store_guild_rows(&self, msg: &m::StoreGuildRows) -> Result<(), ValidationError> {
        if msg.seq == 0 {
            return Err(ValidationError("guild batches number from 1"));
        }
        if msg.rows.iter().any(|r| !(1..=4).contains(&r.kind)) {
            return Err(ValidationError("unknown guild row kind"));
        }
        Ok(())
    }
    fn validate_read_guild_rows(&self, _msg: &m::ReadGuildRows) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_inspect_systems(&self, _msg: &m::InspectSystems) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_inspect_components(&self, _msg: &m::InspectComponents) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_inspect_entities(&self, msg: &m::InspectEntities) -> Result<(), ValidationError> {
        if !(1..=MAX_INSPECT_LIMIT).contains(&msg.limit) {
            return Err(ValidationError("an entity page holds 1 to 100 entities"));
        }
        if msg.component.as_str().is_empty() {
            return Err(ValidationError("name a component"));
        }
        Ok(())
    }
}

/// The most entities one inspector page holds.
pub const MAX_INSPECT_LIMIT: u16 = 100;

/// The router's validator: decodes by id and runs [`Checks`].
///
/// # Errors
/// [`RpcError::Malformed`] or [`RpcError::Refused`] with the reason.
pub fn validate(id: u16, payload: &[u8]) -> Result<(), RpcError> {
    let msg = m::parse_inbound(MessageId(id), payload).map_err(|_| RpcError::Malformed)?;
    msg.validate(&Checks)
        .map_err(|e| RpcError::Refused(e.to_string()))
}

/// Every method with its callers, for the service graph printed at
/// start-up: `(name, id, callers)`.
#[must_use]
pub fn matrix() -> Vec<(&'static str, u16, &'static [Role])> {
    fn row<M: Method>() -> (&'static str, u16, &'static [Role]) {
        (M::NAME, M::ID, M::CALLERS)
    }
    vec![
        row::<RegisterAccount>(),
        row::<LoginAccount>(),
        row::<VerifySession>(),
        row::<BanAccount>(),
        row::<Maintenance>(),
        row::<RegisterCellHost>(),
        row::<Withdraw>(),
        row::<ListAccountCharacters>(),
        row::<NewCharacter>(),
        row::<Select>(),
        row::<NewInstance>(),
        row::<Transfer>(),
        row::<RedeemToken>(),
        row::<RedeemForHost>(),
        row::<PublishLine>(),
        row::<Presence>(),
        row::<Poll>(),
        row::<NewGuild>(),
        row::<EnterGuild>(),
        row::<RelayOp>(),
        row::<Restored>(),
        row::<Projection>(),
        row::<Push>(),
        row::<Ledger>(),
        row::<WriteGuilds>(),
        row::<LoadGuilds>(),
        row::<Queue>(),
        row::<MatchFor>(),
        row::<Live>(),
        row::<InspectCell>(),
        row::<Kick>(),
        row::<Drain>(),
        row::<InspectSystemTimes>(),
        row::<InspectComponentNames>(),
        row::<InspectEntityPage>(),
    ]
}
