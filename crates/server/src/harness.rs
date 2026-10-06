//! A one-cell test bed for modules: install a set of modules, join
//! sessions, send extensions and commands the way the host routes them, tick,
//! and read back what each client was told. Module crates use it in their
//! tests; it allocates freely and is not for the hot path.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::{Angle16, BoundedArray, ContentHash, EntityId, Tick, Vec3};
use mantis_adapter_contract::native::{NativeAdapter, ServerFrame, decode_server_frame};
use mantis_adapter_contract::{
    AppearanceId, Channel, ConnectionId, MovementMode, Outbound, SnapshotFrame, WireAdapter,
};
use mantis_core::ecs::World;
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::FlatGround;
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::module::{Discovered, parse_manifest, parse_package, resolve};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;

use crate::cell::{BoxedSink, Cell, CellConfig, MemoryLog, OutboundSink};
use crate::components::ReplicationId;
use crate::intent::{CellIntent, CellLogSchema};
use crate::modules::{
    ExtensionKind, ExtensionRefusal, ModuleCommand, ModuleOutcome, ModuleSet, ModuleStates, Payload, Route,
    ServerModule,
};

/// What one client was told.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Inbox {
    /// Module messages, in order.
    pub messages: Vec<(ExtensionKind, Vec<u8>)>,
    /// Refusals, in order.
    pub refusals: Vec<(ExtensionKind, ExtensionRefusal)>,
}

#[derive(Default)]
struct Capture(Vec<(ConnectionId, Vec<u8>)>);

impl OutboundSink for Capture {
    fn send(&mut self, _a: usize, conn: ConnectionId, _ch: Channel, bytes: &[u8]) {
        self.0.push((conn, bytes.to_vec()));
    }
}

/// The test bed.
pub struct Harness {
    /// The cell.
    pub cell: Cell,
    capture: Capture,
    clients: BTreeMap<u64, Inbox>,
    /// Every command outcome so far.
    pub outcomes: Vec<ModuleOutcome>,
    /// Every message modules queued for service roles so far: (topic, bytes).
    pub to_services: Vec<(u16, Vec<u8>)>,
    /// The social role, run in-process with the engine's own rules: party,
    /// friends, and guild operations are answered with logged service updates on
    /// the next tick, exactly as a cell host's link delivers them.
    pub parties: mantis_core::social::PartyBook,
    /// Friends, likewise.
    pub friends: mantis_core::social::FriendBook,
    /// Guilds, likewise (durability is the social role's; here every
    /// change lands at once).
    pub guilds: mantis_core::social::GuildBook,
    memory: MemoryLog,
    set: Arc<ModuleSet>,
    hashes: Vec<u64>,
}

const BUILD: BuildId = BuildId([7; 32]);

fn content() -> ContentHash {
    ContentHash::of(b"harness")
}

fn cell_with(set: &ModuleSet, log: Option<LogWriter<CellLogSchema, BoxedSink>>) -> Result<Cell, String> {
    let mut cfg = CellConfig::new(CellId(1), Seed(11));
    cfg.max_entities = 128;
    cfg.max_clients = 64;
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("harness.native"))];
    let mut cell = Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        log,
        Arc::new(GraphCatalog::new()),
        BTreeMap::new(),
    )
    .map_err(|e| format!("{e:?}"))?;
    cell.install_modules(set).map_err(|e| e.to_string())?;
    Ok(cell)
}

impl Harness {
    /// A cell running `modules`, resolved from `manifests` (all of origin
    /// `std`, as package `std` sees them) with `flags` overrides, and the
    /// given content `tables`.
    ///
    /// # Errors
    /// Why the modules could not start.
    pub fn new(
        modules: &[Arc<dyn ServerModule>],
        manifests: &[&str],
        flags: &[(&str, bool)],
        tables: &[(&str, &[u8])],
    ) -> Result<Self, String> {
        let found = manifests
            .iter()
            .map(|m| {
                parse_manifest(m).map(|manifest| Discovered {
                    origin: "std".to_owned(),
                    manifest,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let package = parse_package("[package]\nname = \"std\"\n").map_err(|e| e.to_string())?;
        let live = flags.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        let graph = resolve(&package, &found, &live).map_err(|e| e.to_string())?;
        let mut set = ModuleSet::new(graph, modules).map_err(|e| e.to_string())?;
        for (name, bytes) in tables {
            set = set.with_table(name, Arc::from(*bytes));
        }
        let memory = MemoryLog::default();
        let header = LogHeader {
            build: BUILD,
            content: content(),
            cell: CellId(1),
            seed: Seed(11),
            start_tick: Tick(1),
        };
        let sink: BoxedSink = Box::new(memory.clone());
        let log = LogWriter::create(sink, &header, 1 << 20).map_err(|e| format!("{e:?}"))?;
        let cell = cell_with(&set, Some(log))?;
        Ok(Self {
            cell,
            capture: Capture::default(),
            clients: BTreeMap::new(),
            outcomes: Vec::new(),
            memory,
            set: Arc::new(set),
            hashes: Vec::new(),
            to_services: Vec::new(),
            parties: mantis_core::social::PartyBook::default(),
            friends: mantis_core::social::FriendBook::default(),
            guilds: mantis_core::social::GuildBook::default(),
        })
    }

    /// Joins session `session` as `character`, standing at `(x, 0, z)`.
    /// The avatar replicates as entity `session`.
    pub fn join(&mut self, session: u64, character: u64, x: f32, z: f32) {
        let s = SessionId(session);
        self.cell.inbox().push(
            s,
            CellIntent::Join {
                repl: ReplicationId(Self::avatar(session)),
                spawn: Vec3::new(x, 0.0, z),
                yaw: Angle16(0),
                look: AppearanceId(1),
                mode: MovementMode::Predictive,
                epoch: 1,
                character,
            },
        );
        let _ = self.cell.attach_client(s, ConnectionId(session), 0, false);
        self.clients.entry(session).or_default();
    }

    /// The entity a session's avatar replicates as.
    #[must_use]
    pub fn avatar(session: u64) -> EntityId {
        EntityId::new(u32::try_from(session).unwrap_or(u32::MAX), 0)
    }

    /// Sends an extension from `session`, routed as the host routes it.
    pub fn send(&mut self, session: u64, kind: u16, payload: &[u8]) {
        let kind = ExtensionKind(kind);
        let Some(payload) = Payload::from_slice(payload) else {
            return;
        };
        match self.cell.extension_route(kind) {
            Some(Route::Intent) => {
                self.cell.inbox().push(
                    SessionId(session),
                    CellIntent::Extension {
                        kind,
                        request: 0,
                        payload,
                    },
                );
            }
            Some(Route::Command) => {
                self.cell.commands().push(ModuleCommand {
                    kind,
                    session: Some(SessionId(session)),
                    request: 0,
                    payload,
                });
            }
            None => self
                .clients
                .entry(session)
                .or_default()
                .refusals
                .push((kind, ExtensionRefusal::Invalid)),
        }
    }

    /// Answers one party, friends, or guild operation as the social role
    /// does.
    fn social(&mut self, topic: u16, bytes: &[u8], now_ms: u64) {
        use mantis_core::social::{
            FRIEND_OP, FRIEND_UPDATE, FriendOp, GUILD_OP, GUILD_UPDATE, GuildOp, PARTY_OP, PARTY_UPDATE,
            PartyOp,
        };
        use mantis_core::wire::{decode_exact, encode_into};
        let updates: Vec<(u16, Vec<u8>)> = match topic {
            PARTY_OP => decode_exact::<PartyOp>(bytes).map_or_else(
                |_| Vec::new(),
                |op| {
                    self.parties
                        .apply(&op, now_ms)
                        .iter()
                        .map(|u| {
                            let mut b = Vec::new();
                            encode_into(u, &mut b);
                            (PARTY_UPDATE, b)
                        })
                        .collect()
                },
            ),
            FRIEND_OP => decode_exact::<FriendOp>(bytes).map_or_else(
                |_| Vec::new(),
                |op| {
                    self.friends
                        .apply(&op)
                        .iter()
                        .map(|u| {
                            let mut b = Vec::new();
                            encode_into(u, &mut b);
                            (FRIEND_UPDATE, b)
                        })
                        .collect()
                },
            ),
            GUILD_OP => decode_exact::<GuildOp>(bytes).map_or_else(
                |_| Vec::new(),
                |op| {
                    self.guilds
                        .apply(&op, now_ms)
                        .updates
                        .iter()
                        .map(|u| {
                            let mut b = Vec::new();
                            encode_into(u, &mut b);
                            (GUILD_UPDATE, b)
                        })
                        .collect()
                },
            ),
            _ => Vec::new(),
        };
        for (topic, b) in updates {
            self.service_update(topic, &b);
        }
    }

    /// Delivers an update from a service role (a logged intent).
    pub fn service_update(&mut self, topic: u16, payload: &[u8]) -> bool {
        Payload::from_slice(payload).is_some_and(|payload| {
            self.cell
                .inbox()
                .push(SessionId(0), CellIntent::ServiceUpdate { topic, payload })
        })
    }

    /// Sends a command from a service (no session).
    pub fn service(&mut self, kind: u16, payload: &[u8]) {
        if let Some(payload) = Payload::from_slice(payload) {
            self.cell.commands().push(ModuleCommand {
                kind: ExtensionKind(kind),
                session: None,
                request: 0,
                payload,
            });
        }
    }

    /// Switches a module on or off (a logged intent, as Ops does it).
    pub fn set_enabled(&mut self, key: &str, enabled: bool) {
        let id = self
            .cell
            .world()
            .resource::<ModuleStates>()
            .and_then(|m| m.id(key));
        if let Some(id) = id {
            self.cell.inbox().push(
                SessionId(0),
                CellIntent::SetModule {
                    module: id.0,
                    enabled,
                },
            );
        }
    }

    /// Queues a live (Ops) flag or tunable change, as the cell host does
    /// after verifying its signature; it applies at the next tick.
    pub fn set_live(&mut self, name: &str, kind: u8, value: f32) -> bool {
        let Some(name) = mantis_adapter_contract::core_types::WireString::new(name) else {
            return false;
        };
        self.cell
            .inbox()
            .push(SessionId(0), CellIntent::SetLive { name, kind, value })
    }

    /// Queues an intent as `session` (tests of refusals).
    pub fn push_intent(&mut self, session: u64, intent: CellIntent) -> bool {
        self.cell.inbox().push(SessionId(session), intent)
    }

    /// Runs one tick and collects what every client was told.
    ///
    /// # Errors
    /// The cell's failure.
    pub fn tick(&mut self) -> Result<(), String> {
        let r = self
            .cell
            .tick(&mut self.capture, None)
            .map_err(|e| format!("{e:?}"))?;
        self.hashes.push(r.state_hash);
        let outcomes = &mut self.outcomes;
        self.cell.drain_outcomes(|o| outcomes.push(*o));
        let mut sent = Vec::new();
        self.cell
            .service_messages(|topic, p| sent.push((topic, p.as_slice().to_vec())));
        let now_ms = u64::try_from(crate::session::tick_ms(r.tick, self.cell.rate().hz())).unwrap_or(0);
        for (topic, bytes) in &sent {
            self.social(*topic, bytes, now_ms);
        }
        self.to_services.extend(sent);
        let mut scratch = SnapshotFrame::with_capacity(64, 128, 64, 64);
        let none: [SnapshotFrame; 0] = [];
        for (conn, bytes) in self.capture.0.drain(..) {
            let client = self.clients.entry(conn.0).or_default();
            match decode_server_frame(&bytes, &none[..], &mut scratch) {
                Ok(ServerFrame::Message(Outbound::ExtensionMessage(m))) => {
                    client
                        .messages
                        .push((m.kind, m.payload.iter().copied().collect()));
                }
                Ok(ServerFrame::Message(Outbound::ExtensionRefused(r))) => {
                    client.refusals.push((r.kind, r.reason));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Runs `n` ticks.
    ///
    /// # Errors
    /// The cell's failure.
    pub fn ticks(&mut self, n: usize) -> Result<(), String> {
        for _ in 0..n {
            self.tick()?;
        }
        Ok(())
    }

    /// What `session`'s client was told so far, and forgets it.
    pub fn take(&mut self, session: u64) -> Inbox {
        std::mem::take(self.clients.entry(session).or_default())
    }

    /// The cell's world.
    #[must_use]
    pub fn world(&self) -> &World {
        self.cell.world()
    }

    /// Replays the cell's log into a fresh cell with the same modules and
    /// checks every tick's state hash and every command outcome. Returns
    /// the number of ticks verified.
    ///
    /// # Errors
    /// Any divergence.
    pub fn replay(&self) -> Result<u64, String> {
        let bytes = self.memory.bytes();
        let mut reader =
            LogReader::<CellLogSchema>::open(&bytes, BUILD, content()).map_err(|e| format!("{e:?}"))?;
        let mut fresh = cell_with(&self.set, None)?;
        let report = replay(&mut fresh, &mut reader).map_err(|e| format!("{e:?}"))?;
        if Some(&fresh.world().state_hash()) != self.hashes.last() {
            return Err("final state differs".to_owned());
        }
        Ok(report.ticks)
    }
}

/// Encodes a payload with the module's codec: `Wire::encode` into bytes.
#[must_use]
pub fn bytes_of<T: mantis_core::wire::Wire>(msg: &T) -> Vec<u8> {
    let mut out = Vec::new();
    mantis_core::wire::encode_into(msg, &mut out);
    out
}

/// Builds a byte array for contract messages that carry lists.
#[must_use]
pub fn list<T: Copy, const N: usize>(items: &[T]) -> BoundedArray<T, N> {
    BoundedArray::from_slice(items).unwrap_or_default()
}
