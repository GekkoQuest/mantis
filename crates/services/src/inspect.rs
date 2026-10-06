//! The Ops inspector's view of a cell host (plan 13): per-system run times,
//! component names, and pages of entities with their values as text.
//!
//! Run times and names are published by the host every few ticks and read
//! from here; an entity page is asked of the host's tick loop, answered
//! between ticks, and awaited without blocking the runtime. The log lag is
//! measured here: ticks whose outcomes the writer has not yet made durable.
//! Never served to clients: only the Ops role may call these methods.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mantis_core::ecs::InspectPage;
use mantis_core::schedule::{Phase, Timing};
use mantis_core::wire::{BoundedArray, WireString};

use crate::generated::services as m;
use crate::host::lock;
use crate::host::rpc::{Router, RpcError};
use crate::methods;

/// How long an entity page may wait for the host's tick loop.
pub const PAGE_WAIT: Duration = Duration::from_secs(2);

/// Longest inspector text of one value, in bytes.
pub const VALUE_TEXT: usize = 200;

/// A request for a page of a cell's entities, answered by the host between
/// ticks ([`EntityQuery::answer`]).
#[derive(Debug)]
pub struct EntityQuery {
    /// The cell.
    pub cell: u64,
    /// The component.
    pub component: String,
    /// Entities to skip.
    pub offset: usize,
    /// Entities to return.
    pub limit: usize,
    reply: tokio::sync::oneshot::Sender<Option<m::EntityPage>>,
}

impl EntityQuery {
    /// Answers with `page` (`None`: no such component).
    pub fn answer(self, page: Option<m::EntityPage>) {
        let _ = self.reply.send(page);
    }
}

/// What the inspector knows of the host's cells.
#[derive(Debug)]
pub struct InspectorState {
    systems: Mutex<BTreeMap<u64, m::SystemTimes>>,
    names: Mutex<BTreeMap<u64, Vec<String>>>,
    pending: Mutex<BTreeMap<u64, VecDeque<u64>>>,
    queries: Mutex<std::sync::mpsc::Sender<EntityQuery>>,
}

impl InspectorState {
    /// A state whose entity queries go to `queries`.
    #[must_use]
    pub fn new(queries: std::sync::mpsc::Sender<EntityQuery>) -> Self {
        Self {
            systems: Mutex::default(),
            names: Mutex::default(),
            pending: Mutex::default(),
            queries: Mutex::new(queries),
        }
    }

    /// Publishes `cell`'s run times and component names.
    pub fn publish(&self, times: m::SystemTimes, names: Vec<String>) {
        let cell = times.cell.0;
        lock(&self.systems).insert(cell, times);
        lock(&self.names).insert(cell, names);
    }

    /// Notes that `tick`'s outcomes of `cell` went to the writer.
    pub fn pushed(&self, cell: u64, tick: u64) {
        let mut all = lock(&self.pending);
        let q = all.entry(cell).or_default();
        if q.back() != Some(&tick) {
            q.push_back(tick);
        }
    }

    /// Notes that the writer made `tick`'s outcomes of `cell` durable.
    pub fn durable(&self, cell: u64, tick: u64) {
        if let Some(q) = lock(&self.pending).get_mut(&cell) {
            while q.front().is_some_and(|t| *t <= tick) {
                q.pop_front();
            }
        }
    }

    /// Ticks the oldest outcome of `cell` not yet durable is behind `now`
    /// (0 when everything is durable).
    #[must_use]
    pub fn log_lag(&self, cell: u64, now: u64) -> u32 {
        lock(&self.pending)
            .get(&cell)
            .and_then(|q| q.front().copied())
            .map_or(0, |oldest| {
                u32::try_from(now.saturating_sub(oldest)).unwrap_or(u32::MAX)
            })
    }

    /// Serves the inspector methods on `router`.
    pub fn serve(self: &Arc<Self>, router: &mut Router) {
        let me = Arc::clone(self);
        router.serve::<methods::InspectSystemTimes>(move |_, req| {
            let mut times = lock(&me.systems)
                .get(&req.cell.0)
                .copied()
                .ok_or_else(|| RpcError::Refused(format!("no cell {} on this host", req.cell.0)))?;
            times.log_lag_ticks = me.log_lag(req.cell.0, times.tick);
            Ok(times)
        });
        let me = Arc::clone(self);
        router.serve::<methods::InspectComponentNames>(move |_, req| {
            let names = lock(&me.names)
                .get(&req.cell.0)
                .cloned()
                .ok_or_else(|| RpcError::Refused(format!("no cell {} on this host", req.cell.0)))?;
            let names: Vec<WireString<64>> = names.iter().filter_map(|n| WireString::new(n)).collect();
            Ok(m::ComponentNames {
                names: BoundedArray::from_slice(&names).unwrap_or_default(),
            })
        });
        let me = Arc::clone(self);
        router.serve_later::<methods::InspectEntityPage, _, _>(move |_, req| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let query = EntityQuery {
                cell: req.cell.0,
                component: req.component.as_str().to_owned(),
                offset: usize::try_from(req.offset).unwrap_or(usize::MAX),
                limit: usize::from(req.limit),
                reply: tx,
            };
            let sent = lock(&me.queries).send(query).is_ok();
            async move {
                if !sent {
                    return Err(RpcError::Refused("the host is stopping".to_owned()));
                }
                match tokio::time::timeout(PAGE_WAIT, rx).await {
                    Ok(Ok(Some(page))) => Ok(page),
                    Ok(Ok(None)) => Err(RpcError::Refused(format!(
                        "no component {} in cell {}",
                        req.component.as_str(),
                        req.cell.0
                    ))),
                    Ok(Err(_)) => Err(RpcError::Refused(format!("no cell {} on this host", req.cell.0))),
                    Err(_) => Err(RpcError::Refused("the cell did not answer in time".to_owned())),
                }
            }
        });
    }
}

fn micros(nanos: u32) -> u32 {
    nanos.div_ceil(1000)
}

/// A cell's run times as the inspector serves them (the log lag is filled
/// in when served).
#[must_use]
pub fn system_times<'a>(
    cell: u64,
    tick: u64,
    systems: impl IntoIterator<Item = (&'a str, Phase, &'a Timing)>,
    inbox_depth: usize,
    encode: &Timing,
) -> m::SystemTimes {
    let rows: Vec<m::SystemTime> = systems
        .into_iter()
        .take(128)
        .map(|(name, phase, t)| m::SystemTime {
            name: WireString::new(name).unwrap_or_default(),
            phase: u8::try_from(phase.index()).unwrap_or(u8::MAX),
            micros_last: micros(t.last_nanos()),
            micros_p99: micros(t.percentile_nanos(99)),
            runs: t.runs(),
        })
        .collect();
    m::SystemTimes {
        cell: m::CellNo(cell),
        tick,
        systems: BoundedArray::from_slice(&rows).unwrap_or_default(),
        inbox_depth: u32::try_from(inbox_depth).unwrap_or(u32::MAX),
        encode_micros_p99: micros(encode.percentile_nanos(99)),
        log_lag_ticks: 0,
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
#[must_use]
pub fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

/// Bytes of page text at most (the wire bound).
pub const PAGE_TEXT: usize = 60_000;

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A page of entities as the inspector serves it: one text line per
/// entity (see `EntityPage.text`), values cut to [`VALUE_TEXT`] bytes, at
/// most 16 components each, and as many entities as fit in
/// [`PAGE_TEXT`] bytes.
#[must_use]
pub fn entity_page(cell: u64, tick: u64, component: &str, page: &InspectPage) -> m::EntityPage {
    let mut text = String::new();
    let mut count = 0u16;
    let mut line = String::new();
    for e in page.entities.iter().take(usize::from(methods::MAX_INSPECT_LIMIT)) {
        line.clear();
        let _ = write!(line, "{}:{}", e.id.index(), e.id.generation());
        for (name, value) in e.components.iter().take(16) {
            line.push('\t');
            line.push_str(name);
            line.push('=');
            escape_into(&mut line, cut(value, VALUE_TEXT));
        }
        line.push('\n');
        if text.len() + line.len() > PAGE_TEXT {
            break;
        }
        text.push_str(&line);
        count += 1;
    }
    m::EntityPage {
        cell: m::CellNo(cell),
        tick,
        component: WireString::new(component).unwrap_or_default(),
        total: u32::try_from(page.total).unwrap_or(u32::MAX),
        count,
        text: WireString::new(&text).unwrap_or_default(),
    }
}

/// One entity of a page: its id (`index:generation`) and each component's
/// name and value.
pub type PageRow = (String, Vec<(String, String)>);

/// The rows of a page's text ([`entity_page`]'s format).
#[must_use]
pub fn page_rows(text: &str) -> Vec<PageRow> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().unwrap_or("").to_owned();
            let components = fields
                .map(|f| {
                    let (name, value) = f.split_once('=').unwrap_or((f, ""));
                    (name.to_owned(), unescape(value))
                })
                .collect();
            (id, components)
        })
        .collect()
}

/// The phase names the dashboard shows, by index.
#[must_use]
pub fn phase_name(index: u8) -> &'static str {
    match Phase::ALL.get(usize::from(index)) {
        Some(Phase::Inbound) => "Inbound",
        Some(Phase::Timers) => "Timers",
        Some(Phase::Movement) => "Movement",
        Some(Phase::Combat) => "Combat",
        Some(Phase::Effects) => "Effects",
        Some(Phase::Ai) => "Ai",
        Some(Phase::Scripts) => "Scripts",
        Some(Phase::Interest) => "Interest",
        Some(Phase::Outbound) => "Outbound",
        Some(Phase::Persist) => "Persist",
        None => "?",
    }
}
