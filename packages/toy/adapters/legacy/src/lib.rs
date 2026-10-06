//! The toy package's legacy-shaped adapter (plan 18 step 2).
//!
//! It speaks a deliberately old-fashioned protocol over TCP, in
//! **Validated** movement mode, so the server's envelope, tolerances, and
//! cheat counter are exercised by honest and cheating bots before any real
//! legacy client exists. Its shape is the one such clients share:
//!
//! - **opcode packets**: `[u16 size][u16 opcode][body]`, little-endian, where
//!   `size` counts the whole packet; one transport frame may carry several;
//! - **positions as reports**: the client says where it is (`POS_REPORT`) and
//!   the server answers a violation with a hard `SET_POS`;
//! - **no deltas, no acknowledgements**: every snapshot lists what changed
//!   by entity-enter, entity-move, and entity-leave packets;
//! - **build numbers instead of content hashes**: the client announces its
//!   build; the adapter maps the build it was configured for to the content
//!   hash the server checks;
//! - **echoed server time**: the client echoes the last world tick it saw in
//!   targeted actions, which becomes the intent's view tick;
//! - **object ids from 1**: an entity travels as its bits plus one, and 0
//!   means none.
//!
//! The adapter depends only on the adapter contract (plan 9). [`client`]
//! holds the client half of the protocol for bots and tests.

#![forbid(unsafe_code)]

pub mod client;

use mantis_adapter_contract::core_types::{
    BoundedArray, ContentHash, DecodeError, Decoder, Encoder, EntityId, MarkerKind, Tick, Vec3,
};
use mantis_adapter_contract::{
    AbilityId, AdapterError, Cast, Choose, EntityIdRange, Extension, ExtensionKind, Goodbye, Hello, Inbound,
    Interact, MoveClaim, MovementMode, Outbound, PROTOCOL_VERSION, PromptId, RefuseReason, SnapshotFrame,
    TransportKind, WireAdapter,
};

/// Client-to-server opcodes.
pub mod op {
    /// `[u32 build][u8 token_len][token]`: open a session.
    pub const LOGIN: u16 = 0x0001;
    /// `[f32 x][f32 y][f32 z][u16 heading][u32 client_ms]`: where the
    /// client's avatar is.
    pub const POS_REPORT: u16 = 0x0002;
    /// `[u32 skill][u64 target or 0][u32 echoed_tick]`: use a skill.
    pub const USE_SKILL: u16 = 0x0003;
    /// `[u64 target][u32 echoed_tick]`: interact with an entity.
    pub const INTERACT: u16 = 0x0004;
    /// `[u32 prompt][u16 option]`: answer a prompt.
    pub const SELECT: u16 = 0x0005;
    /// Empty: leave.
    pub const LOGOUT: u16 = 0x0006;
    /// `[u16 kind][u16 len][len bytes]`: a package feature request.
    pub const FEATURE: u16 = 0x0007;

    /// `[u64 session][u64 avatar or 0][u16 tick_rate][u32 tick][u64 character]`: accepted.
    pub const LOGIN_OK: u16 = 0x8001;
    /// `[u8 reason]`: refused (reason codes as the contract's).
    pub const LOGIN_FAIL: u16 = 0x8002;
    /// `[u64 entity][f32 x][f32 y][f32 z][u16 heading][u32 tick]`: hard
    /// position set.
    pub const SET_POS: u16 = 0x8003;
    /// `[u16 kind][u8 reason]`: a package intent was refused (reason codes
    /// as the contract's: 0 feature disabled, 1 not allowed, 2 invalid).
    pub const EXTENSION_REFUSED: u16 = 0x8004;
    /// `[u16 kind][u16 len][len bytes]`: a package feature message.
    pub const FEATURE_DATA: u16 = 0x8005;
    /// `[u8 len][len bytes module key][u8 enabled]`: a feature switched.
    pub const FEATURE_STATE: u16 = 0x8006;
    /// `[u32 tick]`: the world tick of the packets that follow.
    pub const WORLD_TICK: u16 = 0x8010;
    /// `[u64 id][f32 x][f32 y][f32 z][u16 heading]`: the client's own avatar.
    pub const SELF_STATE: u16 = 0x8011;
    /// `[u64 id][u32 look]`: an entity came into view.
    pub const ENTITY_ENTER: u16 = 0x8012;
    /// `[u64 id][f32 x][f32 y][f32 z][u16 heading]`: an entity's position.
    pub const ENTITY_MOVE: u16 = 0x8013;
    /// `[u64 id]`: an entity left view.
    pub const ENTITY_LEAVE: u16 = 0x8014;
    /// `[u64 source][u64 target or 0][u32 graph][u16 node][u8 kind][u16 value][u32 tick]`:
    /// a timeline marker (kind 0 cast start, 1 impact, 2 periodic tick with
    /// its count in `value`, 3 expire, 4 package kind in `value`).
    pub const EFFECT: u16 = 0x8015;
}

/// Packet header bytes.
pub const HEADER: usize = 4;

/// Largest token a `LOGIN` carries.
pub const MAX_TOKEN: usize = 64;

/// The adapter's configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LegacyConfig {
    /// The client build this server serves.
    pub build: u32,
    /// The content hash that build corresponds to.
    pub content: ContentHash,
}

/// The legacy-shaped adapter.
#[derive(Clone, Copy, Debug)]
pub struct LegacyAdapter {
    cfg: LegacyConfig,
}

impl LegacyAdapter {
    /// An adapter serving `cfg.build`.
    #[must_use]
    pub const fn new(cfg: LegacyConfig) -> Self {
        Self { cfg }
    }
}

/// The adapter's name in logs and configuration.
pub const NAME: &str = "toy.legacy";

/// Writes one packet: header, then whatever `body` writes.
pub fn packet(out: &mut Vec<u8>, opcode: u16, body: impl FnOnce(&mut Encoder<'_>)) {
    let start = out.len();
    {
        let mut e = Encoder::new(out);
        e.u16(0);
        e.u16(opcode);
        body(&mut e);
    }
    let size = u16::try_from(out.len() - start).unwrap_or(u16::MAX);
    if let Some(slot) = out.get_mut(start..start + 2) {
        slot.copy_from_slice(&size.to_le_bytes());
    }
}

/// Splits a frame into `(opcode, body)` packets.
///
/// # Errors
/// [`DecodeError`] when a header or size is malformed; packets before it
/// were already delivered.
pub fn packets<'a>(
    frame: &'a [u8],
    mut each: impl FnMut(u16, &'a [u8]) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    let mut d = Decoder::new(frame);
    while d.remaining() > 0 {
        let size = usize::from(d.u16()?);
        let opcode = d.u16()?;
        let body_len = size
            .checked_sub(HEADER)
            .ok_or(DecodeError::Invalid("packet size"))?;
        let body = d.take(body_len)?;
        each(opcode, body)?;
    }
    Ok(())
}

/// The legacy wire carries ticks as `u32` (it wraps after years at any
/// tick rate the engine allows).
#[must_use]
pub fn tick32(t: Tick) -> u32 {
    u32::try_from(t.0 & u64::from(u32::MAX)).unwrap_or(0)
}

/// The ids the legacy wire carries: an object id is the entity's bits plus
/// one, so the all-ones id has none ([`object_id`]).
pub const ENTITY_IDS: EntityIdRange = EntityIdRange {
    max_bits: u64::MAX - 1,
};

/// An entity's object id on the legacy wire: its bits plus one, so that 0
/// is free to mean "none", as legacy protocols have it.
///
/// # Errors
/// [`AdapterError::Unrepresentable`] for the all-ones id, which has no
/// object id (it would wrap to 0, "none"). The encoder refuses the whole
/// packet rather than send a wrong entity.
pub fn object_id(e: EntityId) -> Result<u64, AdapterError> {
    e.to_bits().checked_add(1).ok_or(AdapterError::Unrepresentable(
        "entity id beyond the legacy object id range",
    ))
}

/// An optional entity on the wire: its object id, or 0 for none.
///
/// # Errors
/// [`object_id`]'s.
pub fn entity_bits(e: Option<EntityId>) -> Result<u64, AdapterError> {
    e.map_or(Ok(0), object_id)
}

/// Back from the wire: 0 is none.
#[must_use]
pub fn entity_from(id: u64) -> Option<EntityId> {
    id.checked_sub(1).map(EntityId::from_bits)
}

/// The contract's extension refusal for a code (unknown codes read as
/// invalid).
#[must_use]
pub fn refusal_from(code: u8) -> mantis_adapter_contract::ExtensionRefusal {
    use mantis_adapter_contract::ExtensionRefusal;
    match code {
        0 => ExtensionRefusal::FeatureDisabled,
        1 => ExtensionRefusal::NotAllowed,
        _ => ExtensionRefusal::Invalid,
    }
}

/// The contract's refusal reason for a code (unknown codes read as a
/// version mismatch, the reason a client cannot act on anyway).
#[must_use]
pub fn reason_from(code: u8) -> RefuseReason {
    match code {
        1 => RefuseReason::ContentMismatch,
        2 => RefuseReason::ModuleRefused,
        3 => RefuseReason::Full,
        4 => RefuseReason::BadToken,
        5 => RefuseReason::Maintenance,
        _ => RefuseReason::VersionMismatch,
    }
}

fn vec3(d: &mut Decoder<'_>) -> Result<Vec3, DecodeError> {
    Ok(Vec3::new(d.finite_f32()?, d.finite_f32()?, d.finite_f32()?))
}

fn put_vec3(e: &mut Encoder<'_>, v: Vec3) {
    e.f32(v.x);
    e.f32(v.y);
    e.f32(v.z);
}

impl LegacyAdapter {
    fn inbound(&self, opcode: u16, body: &[u8]) -> Result<Inbound, AdapterError> {
        let mut d = Decoder::new(body);
        let msg = match opcode {
            op::LOGIN => {
                let build = d.u32()?;
                let len = usize::from(d.u8()?);
                if len > MAX_TOKEN {
                    return Err(DecodeError::Invalid("token length").into());
                }
                let token =
                    BoundedArray::from_slice(d.take(len)?).ok_or(DecodeError::Invalid("token length"))?;
                let known = build == self.cfg.build;
                Inbound::Hello(Hello {
                    // An unknown build is refused by the handshake as a
                    // version mismatch.
                    protocol: if known { PROTOCOL_VERSION } else { 0 },
                    capabilities: 0,
                    content: self.cfg.content,
                    modules: BoundedArray::new(),
                    token,
                })
            }
            op::POS_REPORT => {
                let position = vec3(&mut d)?;
                let _heading = d.u16()?;
                let client_time_ms = d.u32()?;
                Inbound::MoveClaim(MoveClaim {
                    position,
                    client_time_ms,
                })
            }
            op::USE_SKILL => {
                let ability = AbilityId(d.u32()?);
                let target = entity_from(d.u64()?);
                let echoed = d.u32()?;
                Inbound::Cast(Cast {
                    ability,
                    target,
                    view_tick: Tick(u64::from(echoed)),
                    view_frac: 0,
                })
            }
            op::INTERACT => {
                let entity =
                    entity_from(d.u64()?).ok_or(AdapterError::Protocol("interact without a target"))?;
                let echoed = d.u32()?;
                Inbound::Interact(Interact {
                    entity,
                    view_tick: Tick(u64::from(echoed)),
                    view_frac: 0,
                })
            }
            op::SELECT => Inbound::Choose(Choose {
                prompt: PromptId(d.u32()?),
                option: d.u16()?,
            }),
            op::LOGOUT => Inbound::Goodbye(Goodbye {}),
            op::FEATURE => {
                let kind = ExtensionKind(d.u16()?);
                let len = usize::from(d.u16()?);
                let payload =
                    BoundedArray::from_slice(d.take(len)?).ok_or(DecodeError::Invalid("feature length"))?;
                // The legacy protocol has no request ids.
                Inbound::Extension(Extension {
                    kind,
                    request: 0,
                    payload,
                })
            }
            _ => return Err(AdapterError::Protocol("unknown opcode")),
        };
        d.finish()?;
        Ok(msg)
    }
}

/// Writes one marker as an `EFFECT` packet.
fn effect(
    out: &mut Vec<u8>,
    m: &mantis_adapter_contract::core_types::TimelineMarker,
) -> Result<(), AdapterError> {
    let (kind, value) = match m.kind {
        MarkerKind::CastStart => (0, 0),
        MarkerKind::Impact { .. } => (1, 0),
        MarkerKind::TickN(n) => (2, n),
        MarkerKind::Expire => (3, 0),
        MarkerKind::Package(p) => (4, p.0),
    };
    let target = match m.kind {
        MarkerKind::Impact { target } => Some(target),
        _ => m.target,
    };
    let (source, target) = (object_id(m.source)?, entity_bits(target)?);
    packet(out, op::EFFECT, |e| {
        e.u64(source);
        e.u64(target);
        e.u32(m.id.graph.0);
        e.u16(m.id.node.0);
        e.u8(kind);
        e.u16(value);
        e.u32(tick32(m.at));
    });
    Ok(())
}

/// Checks that every entity a snapshot names has an object id, before any
/// byte is written.
fn representable(frame: &SnapshotFrame) -> Result<(), AdapterError> {
    if let Some(l) = frame.header.local {
        object_id(l.id)?;
    }
    for id in frame.removed.iter() {
        object_id(*id)?;
    }
    for (id, _) in frame.entered.iter() {
        object_id(*id)?;
    }
    for r in frame.remotes.iter() {
        object_id(r.id)?;
    }
    for m in frame.markers.iter() {
        object_id(m.source)?;
        entity_bits(m.target)?;
        if let MarkerKind::Impact { target } = m.kind {
            object_id(target)?;
        }
    }
    Ok(())
}

impl WireAdapter for LegacyAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn movement_mode(&self) -> MovementMode {
        MovementMode::Validated
    }

    fn transport(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn entity_ids(&self) -> EntityIdRange {
        ENTITY_IDS
    }

    fn decode(&self, frame: &[u8], out: &mut dyn FnMut(Inbound)) -> Result<(), AdapterError> {
        packets(frame, |opcode, body| {
            out(self.inbound(opcode, body)?);
            Ok(())
        })
    }

    fn encode_outbound(&self, msg: &Outbound, out: &mut Vec<u8>) -> Result<(), AdapterError> {
        match msg {
            Outbound::Welcome(w) => {
                let avatar = entity_bits(w.avatar)?;
                packet(out, op::LOGIN_OK, |e| {
                    e.u64(w.session);
                    e.u64(avatar);
                    e.u16(w.tick_rate);
                    e.u32(tick32(w.tick));
                    e.u64(w.character);
                });
            }
            Outbound::Refuse(r) => packet(out, op::LOGIN_FAIL, |e| e.u8(r.reason as u8)),
            Outbound::ExtensionRefused(r) => packet(out, op::EXTENSION_REFUSED, |e| {
                e.u16(r.kind.0);
                e.u8(r.reason as u8);
            }),
            Outbound::ExtensionMessage(m) => {
                let bytes: Vec<u8> = m.payload.iter().copied().collect();
                packet(out, op::FEATURE_DATA, |e| {
                    e.u16(m.kind.0);
                    e.u16(u16::try_from(bytes.len()).unwrap_or(0));
                    e.bytes(&bytes);
                });
            }
            // The legacy client runs no client modules: nothing to permit.
            Outbound::PermittedModules(_) => return Err(AdapterError::Unsupported("permitted modules")),
            Outbound::FeatureState(f) => packet(out, op::FEATURE_STATE, |e| {
                let key = f.module.as_str().as_bytes();
                e.u8(u8::try_from(key.len()).unwrap_or(0));
                e.bytes(
                    key.get(..usize::from(u8::try_from(key.len()).unwrap_or(0)))
                        .unwrap_or(&[]),
                );
                e.bool(f.enabled);
            }),
            Outbound::SetPosition(p) => {
                let entity = object_id(p.entity)?;
                packet(out, op::SET_POS, |e| {
                    e.u64(entity);
                    put_vec3(e, p.position);
                    e.u16(p.yaw.0);
                    e.u32(tick32(p.tick));
                });
            }
        }
        Ok(())
    }

    fn encode_snapshot(
        &self,
        frame: &SnapshotFrame,
        _baseline: Option<&SnapshotFrame>,
        out: &mut Vec<u8>,
    ) -> Result<(), AdapterError> {
        representable(frame)?;
        let h = &frame.header;
        packet(out, op::WORLD_TICK, |e| e.u32(tick32(h.server_tick)));
        if let Some(l) = h.local {
            let id = object_id(l.id)?;
            packet(out, op::SELF_STATE, |e| {
                e.u64(id);
                put_vec3(e, l.state.position);
                e.u16(l.state.yaw.0);
            });
        }
        for id in frame.removed.iter() {
            let id = object_id(*id)?;
            packet(out, op::ENTITY_LEAVE, |e| e.u64(id));
        }
        for (id, look) in frame.entered.iter() {
            let id = object_id(*id)?;
            packet(out, op::ENTITY_ENTER, |e| {
                e.u64(id);
                e.u32(look.0);
            });
        }
        for r in frame.remotes.iter() {
            let id = object_id(r.id)?;
            packet(out, op::ENTITY_MOVE, |e| {
                e.u64(id);
                put_vec3(e, r.position);
                e.u16(r.yaw.0);
            });
        }
        for m in frame.markers.iter() {
            effect(out, m)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
