//! The module-to-service bridge (lead ruling, M7).
//!
//! Outbound: a module queues a message for a service role (a line for the
//! social role, say) in the [`ServiceOutbox`]. That is an output, like a
//! message to a client: it is not logged and not hashed, the cell clears it
//! at the start of every tick, and the host's link to the services drains
//! it after the tick. A module's behaviour must never depend on whether
//! anything drains it, so a cell replays identically with no services.
//!
//! Inbound: what a service sends back (a line delivered here, a presence
//! change) enters the cell as a logged [`crate::intent::CellIntent::ServiceUpdate`],
//! exactly like a client intent, and is dispatched to the module that
//! registered its topic ([`crate::modules::Registrar::service`]). Replay
//! needs only the log.

use mantis_adapter_contract::core_types::{DecodeError, Decoder, Encoder, Wire, WireString};
use mantis_core::ecs::{Resource, World};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::mem::BoundedVec;

use crate::modules::Payload;

/// Outbound: a line for the social role to carry across cells.
pub const SOCIAL_PUBLISH: u16 = 1;
/// Inbound: a line the social role delivered to a character in this cell.
pub const SOCIAL_DELIVER: u16 = 2;

/// Social channels (the social role's numbering).
pub mod channel {
    /// A guild's members, wherever they are.
    pub const GUILD: u8 = 2;
    /// One character, anywhere.
    pub const WHISPER: u8 = 3;
    /// Everyone online.
    pub const WORLD: u8 = 4;
}

/// One line on a social channel, both ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocialLine {
    /// The channel ([`channel`]).
    pub channel: u8,
    /// The speaking character.
    pub from: u64,
    /// The target: a character (whisper), a guild, or 0 (world).
    pub to: u64,
    /// The text.
    pub text: WireString<200>,
}

impl Wire for SocialLine {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u8(self.channel);
        e.u64(self.from);
        e.u64(self.to);
        self.text.encode(e);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            channel: d.u8()?,
            from: d.u64()?,
            to: d.u64()?,
            text: WireString::decode(d)?,
        })
    }
}

impl SocialLine {
    /// As a payload.
    #[must_use]
    pub fn payload(&self) -> Payload {
        let mut bytes = Vec::with_capacity(224);
        mantis_adapter_contract::core_types::encode_into(self, &mut bytes);
        Payload::from_slice(&bytes).unwrap_or(Payload::EMPTY)
    }

    /// From a payload.
    ///
    /// # Errors
    /// The payload is not a line.
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        mantis_adapter_contract::core_types::decode_exact(bytes)
    }
}

/// Messages for service roles, queued this tick. Output, not state.
#[derive(Debug)]
pub struct ServiceOutbox {
    items: BoundedVec<(u16, Payload)>,
    dropped: u64,
}

impl ServiceOutbox {
    /// Messages per tick.
    pub const CAPACITY: usize = 256;

    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: BoundedVec::with_capacity(Self::CAPACITY),
            dropped: 0,
        }
    }

    /// Queues one message; false (and counted) when the tick's outbox is full.
    pub fn push(&mut self, topic: u16, payload: Payload) -> bool {
        let ok = self.items.push((topic, payload)).is_ok();
        if !ok {
            self.dropped += 1;
        }
        ok
    }

    /// This tick's messages.
    #[must_use]
    pub fn pending(&self) -> &[(u16, Payload)] {
        &self.items
    }

    /// Messages dropped for capacity.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Forgets this tick's messages.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

impl Default for ServiceOutbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Not hashed: an output.
impl StateHash for ServiceOutbox {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for ServiceOutbox {
    const NAME: &'static str = "server.service_outbox";

    /// Output: cleared at the start of every tick.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

/// Encodes `msg` and queues it for a service role ([`to_service`]); false
/// when it does not fit a payload or the outbox is full.
pub fn to_service_encoded<T: mantis_core::wire::Wire>(world: &mut World, topic: u16, msg: &T) -> bool {
    let mut bytes = Vec::new();
    mantis_core::wire::encode_into(msg, &mut bytes);
    Payload::from_slice(&bytes).is_some_and(|p| to_service(world, topic, p))
}

/// Queues a message for a service role. Its result must not change what
/// the module does next (the outbox is not logged).
pub fn to_service(world: &mut World, topic: u16, payload: Payload) -> bool {
    world
        .resource_mut::<ServiceOutbox>()
        .is_some_and(|o| o.push(topic, payload))
}
