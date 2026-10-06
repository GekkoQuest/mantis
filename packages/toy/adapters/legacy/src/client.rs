//! The client half of the legacy-shaped protocol, for bots and tests: what
//! a legacy client sends, and the packets it understands.

use mantis_adapter_contract::core_types::{DecodeError, Decoder, EntityId, Vec3};
use mantis_adapter_contract::{AdapterError, AppearanceId, Inbound, RefuseReason};

use crate::{entity_bits, entity_from, object_id, op, packet, packets, put_vec3, reason_from, tick32, vec3};

/// One packet from the server.
#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(clippy::large_enum_variant)] // decoded one at a time and handed to a callback
pub enum ServerPacket {
    /// The session was accepted.
    LoginOk {
        /// Session id.
        session: u64,
        /// The client's avatar.
        avatar: Option<EntityId>,
        /// Server ticks per second.
        tick_rate: u16,
        /// The server tick at acceptance.
        tick: u32,
        /// The player's character.
        character: u64,
    },
    /// The session was refused.
    LoginFail(RefuseReason),
    /// Hard position set.
    SetPos {
        /// The entity (the client's avatar).
        entity: EntityId,
        /// Where it is.
        position: Vec3,
        /// Its heading.
        heading: u16,
        /// The server tick of the correction.
        tick: u32,
    },
    /// A package intent was refused.
    ExtensionRefused {
        /// The extension kind.
        kind: u16,
        /// The contract's refusal code.
        reason: u8,
    },
    /// A package feature message.
    FeatureData {
        /// The extension kind.
        kind: u16,
        /// Payload length.
        len: u16,
        /// Payload bytes (the first `len`).
        bytes: [u8; 512],
    },
    /// A feature was switched on or off.
    FeatureState {
        /// Module key length.
        len: u8,
        /// Module key bytes (the first `len`).
        key: [u8; 64],
        /// Enabled.
        enabled: bool,
    },
    /// The world tick of the packets that follow.
    WorldTick(u32),
    /// The client's own avatar.
    SelfState {
        /// The avatar.
        id: EntityId,
        /// Where it is.
        position: Vec3,
        /// Its heading.
        heading: u16,
    },
    /// An entity came into view.
    Enter {
        /// The entity.
        id: EntityId,
        /// How it looks.
        look: AppearanceId,
    },
    /// An entity's position.
    Move {
        /// The entity.
        id: EntityId,
        /// Where it is.
        position: Vec3,
        /// Its heading.
        heading: u16,
    },
    /// An entity left view.
    Leave {
        /// The entity.
        id: EntityId,
    },
    /// A timeline marker.
    Effect {
        /// Who runs the effect.
        source: EntityId,
        /// Its target.
        target: Option<EntityId>,
        /// The gameplay graph.
        graph: u32,
        /// The emitting node.
        node: u16,
        /// Marker kind code (see [`op::EFFECT`]).
        kind: u8,
        /// Count or package kind.
        value: u16,
        /// The tick the marker is for.
        tick: u32,
    },
}

fn entity(d: &mut Decoder<'_>) -> Result<EntityId, DecodeError> {
    entity_from(d.u64()?).ok_or(DecodeError::Invalid("entity"))
}

fn server_packet(opcode: u16, body: &[u8]) -> Result<ServerPacket, AdapterError> {
    let mut d = Decoder::new(body);
    let p = match opcode {
        op::LOGIN_OK => ServerPacket::LoginOk {
            session: d.u64()?,
            avatar: entity_from(d.u64()?),
            tick_rate: d.u16()?,
            tick: d.u32()?,
            character: d.u64()?,
        },
        op::LOGIN_FAIL => ServerPacket::LoginFail(reason_from(d.u8()?)),
        op::SET_POS => ServerPacket::SetPos {
            entity: entity(&mut d)?,
            position: vec3(&mut d)?,
            heading: d.u16()?,
            tick: d.u32()?,
        },
        op::EXTENSION_REFUSED => ServerPacket::ExtensionRefused {
            kind: d.u16()?,
            reason: d.u8()?,
        },
        op::FEATURE_DATA => {
            let kind = d.u16()?;
            let len = d.u16()?;
            let mut bytes = [0u8; 512];
            bytes
                .get_mut(..usize::from(len))
                .ok_or(DecodeError::Invalid("feature length"))?
                .copy_from_slice(d.take(usize::from(len))?);
            ServerPacket::FeatureData { kind, len, bytes }
        }
        op::FEATURE_STATE => {
            let len = d.u8()?;
            let mut key = [0u8; 64];
            key.get_mut(..usize::from(len))
                .ok_or(DecodeError::Invalid("feature key length"))?
                .copy_from_slice(d.take(usize::from(len))?);
            ServerPacket::FeatureState {
                len,
                key,
                enabled: d.bool()?,
            }
        }
        op::WORLD_TICK => ServerPacket::WorldTick(d.u32()?),
        op::SELF_STATE => ServerPacket::SelfState {
            id: entity(&mut d)?,
            position: vec3(&mut d)?,
            heading: d.u16()?,
        },
        op::ENTITY_ENTER => ServerPacket::Enter {
            id: entity(&mut d)?,
            look: AppearanceId(d.u32()?),
        },
        op::ENTITY_MOVE => ServerPacket::Move {
            id: entity(&mut d)?,
            position: vec3(&mut d)?,
            heading: d.u16()?,
        },
        op::ENTITY_LEAVE => ServerPacket::Leave { id: entity(&mut d)? },
        op::EFFECT => ServerPacket::Effect {
            source: entity(&mut d)?,
            target: entity_from(d.u64()?),
            graph: d.u32()?,
            node: d.u16()?,
            kind: d.u8()?,
            value: d.u16()?,
            tick: d.u32()?,
        },
        _ => return Err(AdapterError::Protocol("unknown opcode")),
    };
    d.finish()?;
    Ok(p)
}

/// Decodes every packet of a frame from the server.
///
/// # Errors
/// [`AdapterError`] at the first malformed packet.
pub fn decode_server(frame: &[u8], mut each: impl FnMut(ServerPacket)) -> Result<(), AdapterError> {
    packets(frame, |opcode, body| {
        each(server_packet(opcode, body)?);
        Ok(())
    })
}

/// Encodes a contract message the way a legacy client sends it, announcing
/// `build` in a login. Returns false (writing nothing) when the protocol has
/// no form for the message: native movement inputs, snapshot
/// acknowledgements, and extensions.
pub fn encode_inbound(msg: &Inbound, build: u32, out: &mut Vec<u8>) -> bool {
    match msg {
        Inbound::Hello(h) => {
            let token: Vec<u8> = h.token.iter().copied().collect();
            let len = u8::try_from(token.len()).unwrap_or(u8::MAX);
            packet(out, op::LOGIN, |e| {
                e.u32(build);
                e.u8(len);
                e.bytes(token.get(..usize::from(len)).unwrap_or(&[]));
            });
        }
        Inbound::MoveClaim(c) => packet(out, op::POS_REPORT, |e| {
            put_vec3(e, c.position);
            e.u16(0);
            e.u32(c.client_time_ms);
        }),
        Inbound::Cast(c) => {
            let Ok(target) = entity_bits(c.target) else {
                return false;
            };
            packet(out, op::USE_SKILL, |e| {
                e.u32(c.ability.0);
                e.u64(target);
                e.u32(tick32(c.view_tick));
            });
        }
        Inbound::Interact(i) => {
            let Ok(entity) = object_id(i.entity) else {
                return false;
            };
            packet(out, op::INTERACT, |e| {
                e.u64(entity);
                e.u32(tick32(i.view_tick));
            });
        }
        Inbound::Choose(c) => packet(out, op::SELECT, |e| {
            e.u32(c.prompt.0);
            e.u16(c.option);
        }),
        Inbound::Goodbye(_) => packet(out, op::LOGOUT, |_| {}),
        Inbound::Extension(x) => {
            let bytes: Vec<u8> = x.payload.iter().copied().collect();
            packet(out, op::FEATURE, |e| {
                e.u16(x.kind.0);
                e.u16(u16::try_from(bytes.len()).unwrap_or(0));
                e.bytes(&bytes);
            });
        }
        Inbound::Move(_) | Inbound::SnapshotAck(_) => return false,
    }
    true
}

/// Encodes one server packet exactly as the server writes it: the inverse
/// of [`decode_server`], for tests and fuzzing (`decode_server` of the
/// output yields `p` again).
///
/// # Errors
/// [`AdapterError::Unrepresentable`] for an entity with no object id;
/// nothing is written.
pub fn encode_server(p: &ServerPacket, out: &mut Vec<u8>) -> Result<(), AdapterError> {
    match p {
        ServerPacket::LoginOk {
            session,
            avatar,
            tick_rate,
            tick,
            character,
        } => {
            let avatar = entity_bits(*avatar)?;
            packet(out, op::LOGIN_OK, |e| {
                e.u64(*session);
                e.u64(avatar);
                e.u16(*tick_rate);
                e.u32(*tick);
                e.u64(*character);
            });
        }
        ServerPacket::LoginFail(r) => packet(out, op::LOGIN_FAIL, |e| e.u8(*r as u8)),
        ServerPacket::SetPos {
            entity,
            position,
            heading,
            tick,
        } => {
            let entity = object_id(*entity)?;
            packet(out, op::SET_POS, |e| {
                e.u64(entity);
                put_vec3(e, *position);
                e.u16(*heading);
                e.u32(*tick);
            });
        }
        ServerPacket::ExtensionRefused { kind, reason } => packet(out, op::EXTENSION_REFUSED, |e| {
            e.u16(*kind);
            e.u8(*reason);
        }),
        ServerPacket::FeatureData { kind, len, bytes } => packet(out, op::FEATURE_DATA, |e| {
            let body = bytes.get(..usize::from(*len)).unwrap_or(&[]);
            e.u16(*kind);
            e.u16(u16::try_from(body.len()).unwrap_or(0));
            e.bytes(body);
        }),
        ServerPacket::FeatureState { len, key, enabled } => packet(out, op::FEATURE_STATE, |e| {
            let body = key.get(..usize::from(*len)).unwrap_or(&[]);
            e.u8(u8::try_from(body.len()).unwrap_or(0));
            e.bytes(body);
            e.bool(*enabled);
        }),
        ServerPacket::WorldTick(t) => packet(out, op::WORLD_TICK, |e| e.u32(*t)),
        ServerPacket::SelfState { .. }
        | ServerPacket::Move { .. }
        | ServerPacket::Enter { .. }
        | ServerPacket::Leave { .. }
        | ServerPacket::Effect { .. } => encode_view(p, out)?,
    }
    Ok(())
}

/// The view packets of [`encode_server`]: entities and markers.
fn encode_view(p: &ServerPacket, out: &mut Vec<u8>) -> Result<(), AdapterError> {
    match p {
        ServerPacket::SelfState {
            id,
            position,
            heading,
        }
        | ServerPacket::Move {
            id,
            position,
            heading,
        } => {
            let opcode = if matches!(p, ServerPacket::SelfState { .. }) {
                op::SELF_STATE
            } else {
                op::ENTITY_MOVE
            };
            let id = object_id(*id)?;
            packet(out, opcode, |e| {
                e.u64(id);
                put_vec3(e, *position);
                e.u16(*heading);
            });
        }
        ServerPacket::Enter { id, look } => {
            let id = object_id(*id)?;
            packet(out, op::ENTITY_ENTER, |e| {
                e.u64(id);
                e.u32(look.0);
            });
        }
        ServerPacket::Leave { id } => {
            let id = object_id(*id)?;
            packet(out, op::ENTITY_LEAVE, |e| e.u64(id));
        }
        ServerPacket::Effect {
            source,
            target,
            graph,
            node,
            kind,
            value,
            tick,
        } => {
            let (source, target) = (object_id(*source)?, entity_bits(*target)?);
            packet(out, op::EFFECT, |e| {
                e.u64(source);
                e.u64(target);
                e.u32(*graph);
                e.u16(*node);
                e.u8(*kind);
                e.u16(*value);
                e.u32(*tick);
            });
        }
        _ => {}
    }
    Ok(())
}
