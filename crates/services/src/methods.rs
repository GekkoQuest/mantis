//! Every RPC method, with its caller matrix: which roles may call it.
//! A method's id is its request message id in `schema/services.registry`.

use mantis_core::wire::{Message, MessageId, ValidationError};

use crate::generated::services as m;
use crate::host::Role::{self, Account, Cell, Gateway, Matchmaking, Ops, Realm, Social};
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
method!(/// A cell host asks which run of the realm is serving.
    RealmRun: PollRealm -> RealmEpoch, callers [Cell]);
method!(/// Ops makes a live value durable before publishing it.
    StoreLiveValue: StoreLive -> Empty, callers [Ops]);
method!(/// Ops reads the durable live values at start.
    ReadLiveValues: ReadLive -> LiveValues, callers [Ops]);
method!(/// Ops opens an audit row before it runs a command.
    AuditOpen: AuditBegin -> AuditId, callers [Ops]);
method!(/// Ops completes an audit row.
    AuditClose: AuditComplete -> Empty, callers [Ops]);
method!(/// Ops reads the audit trail.
    AuditTrail: ReadAudit -> AuditRows, callers [Ops]);
method!(/// Social makes friend changes durable.
    WriteFriends: StoreFriendRows -> Durable, callers [Social]);
method!(/// Social reads the friend rows back.
    LoadFriends: ReadFriendRows -> FriendRows, callers [Social]);
method!(/// The account role makes account rows durable.
    WriteAccounts: StoreAccountRows -> Durable, callers [Account]);
method!(/// The account role reads the account rows back.
    LoadAccounts: ReadAccountRows -> AccountRows, callers [Account]);
method!(/// The realm makes character rows durable.
    WriteCharacters: StoreCharacterRows -> Durable, callers [Realm]);
method!(/// The realm reads the character rows back.
    LoadCharacters: ReadCharacterRows -> CharacterRows, callers [Realm]);
method!(/// Change an account's password.
    ChangeAccountPassword: ChangePassword -> Empty, callers [Gateway]);
method!(/// Delete one of an account's characters.
    RemoveCharacter: DeleteCharacter -> Empty, callers [Gateway, Ops]);
method!(/// A cell host reports where a character left or arrived.
    PlaceCharacter: CharacterPlaced -> Empty, callers [Cell]);
method!(/// A gateway learns where an entry token's session goes, without
    /// redeeming the token.
    RouteEntry: RouteToken -> EntryRoute, callers [Gateway]);
method!(/// A role instance takes or renews its role's lease (role failover).
    Lease: AcquireLease -> LeaseState, callers [Account, Realm, Social, Matchmaking, Ops]);

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
    fn validate_route_token(&self, msg: &m::RouteToken) -> Result<(), ValidationError> {
        if msg.token.len() != 32 {
            return Err(ValidationError("an entry token is 32 bytes"));
        }
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
    fn validate_store_friend_rows(&self, msg: &m::StoreFriendRows) -> Result<(), ValidationError> {
        if msg.seq == 0 {
            return Err(ValidationError("friend batches number from 1"));
        }
        if msg.rows.iter().any(|r| !(1..=4).contains(&r.kind)) {
            return Err(ValidationError("unknown friend row kind"));
        }
        Ok(())
    }
    fn validate_poll_realm(&self, _msg: &m::PollRealm) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_store_live(&self, msg: &m::StoreLive) -> Result<(), ValidationError> {
        if msg.name.as_str().is_empty() || msg.kind > 1 || !msg.value.is_finite() {
            return Err(ValidationError(
                "a live value has a name, a kind of 0 or 1, and a finite value",
            ));
        }
        Ok(())
    }
    fn validate_read_live(&self, _msg: &m::ReadLive) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_audit_begin(&self, msg: &m::AuditBegin) -> Result<(), ValidationError> {
        if msg.actor.as_str().is_empty() || msg.command.as_str().is_empty() {
            return Err(ValidationError("an audit row names its actor and command"));
        }
        Ok(())
    }
    fn validate_audit_complete(&self, msg: &m::AuditComplete) -> Result<(), ValidationError> {
        if !matches!(msg.status.as_str(), "done" | "failed") {
            return Err(ValidationError("an audit row completes as done or failed"));
        }
        Ok(())
    }
    fn validate_read_audit(&self, msg: &m::ReadAudit) -> Result<(), ValidationError> {
        if !(1..=AUDIT_PAGE).contains(&msg.limit) {
            return Err(ValidationError("audit pages are 1 to 16 rows"));
        }
        Ok(())
    }
    fn validate_read_friend_rows(&self, _msg: &m::ReadFriendRows) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_store_account_rows(&self, msg: &m::StoreAccountRows) -> Result<(), ValidationError> {
        if msg.seq == 0 {
            return Err(ValidationError("account batches number from 1"));
        }
        for r in msg.rows.iter() {
            named(r.name.as_str())?;
            if r.id == 0 || r.salt.len() != 16 || r.hash.len() != 32 || r.iterations == 0 {
                return Err(ValidationError(
                    "an account row has an id, a salt, a key and iterations",
                ));
            }
        }
        Ok(())
    }
    fn validate_read_account_rows(&self, _msg: &m::ReadAccountRows) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_store_character_rows(&self, msg: &m::StoreCharacterRows) -> Result<(), ValidationError> {
        if msg.seq == 0 {
            return Err(ValidationError("character batches number from 1"));
        }
        for r in msg.rows.iter() {
            named(r.name.as_str())?;
            if r.id == 0 || r.account == 0 || ![r.x, r.y, r.z].iter().all(|v| v.is_finite()) {
                return Err(ValidationError(
                    "a character row has an id, an account and a finite position",
                ));
            }
        }
        Ok(())
    }
    fn validate_read_character_rows(&self, _msg: &m::ReadCharacterRows) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_change_password(&self, msg: &m::ChangePassword) -> Result<(), ValidationError> {
        named(msg.name.as_str())?;
        if msg.new.as_str().len() < 8 {
            return Err(ValidationError("passwords have at least 8 bytes"));
        }
        Ok(())
    }
    fn validate_delete_character(&self, _msg: &m::DeleteCharacter) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_acquire_lease(&self, msg: &m::AcquireLease) -> Result<(), ValidationError> {
        let role = Role::from_u8(msg.role);
        if !matches!(
            role,
            Some(Role::Account | Role::Realm | Role::Social | Role::Matchmaking | Role::Ops)
        ) {
            return Err(ValidationError(
                "a lease is for account, realm, social, matchmaking or ops",
            ));
        }
        if msg.owner.as_str().is_empty() || msg.ttl_ms == 0 {
            return Err(ValidationError("a lease names its owner and lasts a while"));
        }
        Ok(())
    }
    fn validate_character_placed(&self, msg: &m::CharacterPlaced) -> Result<(), ValidationError> {
        if msg.cell.0 == 0 || ![msg.x, msg.y, msg.z].iter().all(|v| v.is_finite()) {
            return Err(ValidationError("a placement names a cell and a finite position"));
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

/// The most audit rows one `ReadAudit` page holds.
pub const AUDIT_PAGE: u16 = 16;

/// The router's validator: decodes by id and runs [`Checks`].
///
/// # Errors
/// [`RpcError::Malformed`] or [`RpcError::Refused`] with the reason.
pub fn validate(id: u16, payload: &[u8]) -> Result<(), RpcError> {
    let msg = m::parse_inbound(MessageId(id), payload).map_err(|_| RpcError::Malformed)?;
    msg.validate(&Checks)
        .map_err(|e| RpcError::Refused(e.to_string()))
}

/// The role that serves method `id`: its handler lives in that role's
/// router (the cell host serves the inspector, kick, and drain). A process
/// is given the addresses of exactly the roles it calls; `None` for an
/// unknown id. A test builds every router and holds this to them.
#[must_use]
pub fn server_of(id: u16) -> Option<Role> {
    fn of<M: Method>(id: u16) -> bool {
        M::ID == id
    }
    macro_rules! serving {
        ($role:ident: $($m:ident),* $(,)?) => {
            if $(of::<$m>(id))||* {
                return Some(Role::$role);
            }
        };
    }
    serving!(Account: RegisterAccount, LoginAccount, VerifySession, BanAccount, Maintenance, ChangeAccountPassword);
    serving!(Realm: RegisterCellHost, Withdraw, ListAccountCharacters, NewCharacter, Select, RedeemToken,
        RedeemForHost, Transfer, NewInstance, RealmRun, RemoveCharacter, PlaceCharacter, RouteEntry);
    serving!(Social: PublishLine, Presence, Poll, RelayOp, Restored, Projection, NewGuild, EnterGuild);
    serving!(Persist: Push, Ledger, StoreLiveValue, ReadLiveValues, AuditOpen, AuditClose, AuditTrail, WriteGuilds, LoadGuilds, WriteFriends,
        LoadFriends, WriteAccounts, LoadAccounts, WriteCharacters, LoadCharacters, Lease);
    serving!(Matchmaking: Queue, MatchFor);
    serving!(Ops: Live);
    serving!(Cell: InspectCell, Kick, Drain, InspectSystemTimes, InspectComponentNames, InspectEntityPage);
    None
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
        row::<RouteEntry>(),
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
        row::<RealmRun>(),
        row::<StoreLiveValue>(),
        row::<ReadLiveValues>(),
        row::<AuditOpen>(),
        row::<AuditClose>(),
        row::<AuditTrail>(),
        row::<WriteFriends>(),
        row::<LoadFriends>(),
        row::<WriteAccounts>(),
        row::<LoadAccounts>(),
        row::<WriteCharacters>(),
        row::<LoadCharacters>(),
        row::<ChangeAccountPassword>(),
        row::<RemoveCharacter>(),
        row::<PlaceCharacter>(),
        row::<Lease>(),
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
