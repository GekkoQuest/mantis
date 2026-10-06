//! The cell-host role, run by a package's server binary.
//!
//! A cell host is package code (its world, its modules, its adapters), and
//! engine crates never depend on packages, so `mantisd` does not host
//! cells. A package's binary adopts the role in a few lines:
//!
//! ```text
//! // packages/<p>/server/src/main.rs: `<binary> node <role> --config FILE`
//! "node" => mantis_deploy::cli::main(rest, Some(&MyCells)),
//!
//! struct MyCells;
//! impl mantis_deploy::cell::CellHost for MyCells {
//!     fn command(&self) -> &'static str { "<binary> node" }
//!     fn run(&self, node: mantis_deploy::cell::CellNode) -> Result<(), String> {
//!         let settings = node.settings().read(|f, base| { /* the package's keys */ })?;
//!         // Recover every cell from node.state_dir() (snapshot, then the log after it).
//!         let link = node.link(&world_cells, &instance_cells)?;   // registers with the realm
//!         node.ready();
//!         while node.drain_requested().is_none() {
//!             // tick, after_tick(.., &link, ..), snapshot every node.snapshot_every_ticks()
//!             node.observe(tick, &link);
//!         }
//!         // A clean shutdown always ends in a snapshot (decision 0007).
//!         node.finish(link)                                       // flushes, then stops
//!     }
//! }
//! ```
//!
//! The same binary then runs every service role too (`node persist`, ...),
//! through the same library, so a package can ship one executable.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mantis_services::cluster::{CellLink, CellLinkConfig};
use mantis_services::host::Role;

use crate::config::{CellHostConfig, PackageSettings};
use crate::node::Node;

/// A package's cell host.
pub trait CellHost {
    /// How an operator runs this binary as a node, for messages
    /// (`"<binary> node"`).
    fn command(&self) -> &'static str;

    /// Runs the cells until a drain is requested, ending in a snapshot and
    /// [`CellNode::finish`].
    ///
    /// # Errors
    /// Why the cells could not run.
    fn run(&self, node: CellNode) -> Result<(), String>;
}

/// A started cell-host node whose dependencies are ready.
pub struct CellNode {
    node: Node,
    config: CellHostConfig,
}

impl CellNode {
    /// Wraps a started node of the cell-host role.
    ///
    /// # Errors
    /// The node is not a cell host.
    pub fn new(node: Node) -> Result<Self, String> {
        let config = node
            .config
            .cell_host
            .clone()
            .ok_or("a cell-host node needs [cell_host]")?;
        Ok(Self { node, config })
    }

    /// The package's `[package]` keys, read strictly.
    #[must_use]
    pub fn settings(&self) -> PackageSettings {
        self.node.config.package_settings()
    }

    /// The cells the registry says this host serves.
    #[must_use]
    pub fn cells(&self) -> &[u64] {
        &self.node.me.cells
    }

    /// Where snapshots and logs live across restarts.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.config.state
    }

    /// Ticks between snapshots.
    #[must_use]
    pub fn snapshot_every_ticks(&self) -> u64 {
        self.config.snapshot_every_ticks
    }

    /// The game address clients are sent to.
    #[must_use]
    pub fn advertise(&self) -> &str {
        &self.config.advertise
    }

    /// The runtime the link runs on.
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.node.handle()
    }

    /// The underlying node (registry, key, status).
    #[must_use]
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// The link configuration for these world cells (id, x range) and
    /// instance cells, with every address from the verified registry.
    ///
    /// # Errors
    /// The cells are not exactly the ones the registry lists for this host,
    /// or a role it calls is missing from the registry.
    pub fn link_config(
        &self,
        world: &[(u64, (f32, f32))],
        instances: &[u64],
    ) -> Result<CellLinkConfig, String> {
        let mut mine: Vec<u64> = world
            .iter()
            .map(|c| c.0)
            .chain(instances.iter().copied())
            .collect();
        mine.sort_unstable();
        let mut listed = self.cells().to_vec();
        listed.sort_unstable();
        if mine != listed {
            return Err(format!(
                "this host runs cells {mine:?}, the registry lists {listed:?} for {}",
                self.node.me.name
            ));
        }
        let address = self.config.advertise.clone();
        Ok(CellLinkConfig {
            key: self.node.key.clone(),
            persist: self.node.rpc_of(Role::Persist)?,
            ops: self.node.rpc_of(Role::Ops)?,
            social: self.node.rpc_of(Role::Social)?,
            matchmaking: self.node.rpc_of(Role::Matchmaking)?,
            realm: self.node.rpc_of(Role::Realm)?,
            live_key: self.node.registry.live_key.to_vec(),
            cells: world
                .iter()
                .map(|(id, range)| (*id, address.clone(), *range))
                .collect(),
            instances: instances.iter().map(|id| (*id, address.clone())).collect(),
            poll: self.config.poll,
            inspector: self.node.config.listen_rpc,
            tls: Some(std::sync::Arc::clone(&self.node.tls)),
        })
    }

    /// Starts the link: registers the cells with the realm and serves the
    /// inspector Ops calls on this node's RPC listener.
    ///
    /// # Errors
    /// [`CellNode::link_config`], or the link could not start.
    pub fn link(&self, world: &[(u64, (f32, f32))], instances: &[u64]) -> Result<CellLink, String> {
        let config = self.link_config(world, instances)?;
        let link = CellLink::start(&self.node.handle(), &config)?;
        self.node.say(&format!(
            "cells {:?} registered with the realm at {}; inspector on {}",
            self.cells(),
            config.realm,
            config.inspector
        ));
        Ok(link)
    }

    /// Marks the node ready (after recovery, with the link started).
    pub fn ready(&self) {
        self.node.ready();
    }

    /// Why a drain was requested, if it was: stop ticking, snapshot, and
    /// call [`CellNode::finish`].
    #[must_use]
    pub fn drain_requested(&self) -> Option<&'static str> {
        self.node.drain.requested()
    }

    /// Publishes the tick and the link's counters on `/metrics`.
    pub fn observe(&self, tick: u64, link: &CellLink) {
        let m = &self.node.status.metrics;
        let s = &link.stats;
        let get =
            |a: &std::sync::atomic::AtomicU64| i64::try_from(a.load(Ordering::Relaxed)).unwrap_or(i64::MAX);
        m.set("cell_tick", i64::try_from(tick).unwrap_or(i64::MAX));
        for (name, v) in [
            ("link_durable_batches", &s.durable_batches),
            ("link_push_retries", &s.push_retries),
            ("link_pending", &s.pending),
            ("link_relayed", &s.relayed),
            ("link_relay_retries", &s.relay_retries),
            ("link_projected", &s.projected),
            ("link_projected_duplicates", &s.projected_duplicates),
            ("link_projected_gaps", &s.projected_gaps),
            ("link_social_restarts", &s.social_restarts),
            ("link_restores", &s.restores),
            ("link_live_applied", &s.live_applied),
            ("link_live_refused", &s.live_refused),
        ] {
            m.set(name, get(v));
        }
    }

    /// Ends a drain: the cells have stopped and snapshotted; waits (up to
    /// the drain grace) until every queued outcome and relay is
    /// acknowledged, then stops the link.
    ///
    /// # Errors
    /// Some were still pending at the end of the grace: the log keeps them
    /// and a recovery on the same build pushes them again, but a recovery
    /// on another build (snapshot only) would not.
    pub fn finish(self, link: CellLink) -> Result<(), String> {
        self.node.draining("snapshotted; flushing the link");
        let grace = self.node.config.drain_grace;
        let flushed = link.flush(grace);
        {
            let _guard = self.node.handle().enter();
            drop(link);
        }
        match flushed {
            Ok(()) => {
                self.node.say("drained: every outcome durable, link closed");
                Ok(())
            }
            Err(left) => Err(format!(
                "{left} outcome or relay messages were still unacknowledged after {} ms; the cell \
                 logs keep them for a same-build recovery",
                grace.as_millis()
            )),
        }
    }

    /// How long the drain may take.
    #[must_use]
    pub fn drain_grace(&self) -> Duration {
        self.node.config.drain_grace
    }
}
