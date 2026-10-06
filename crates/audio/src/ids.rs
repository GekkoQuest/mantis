//! Ids the game posts, and caller-allocated voice handles.

use std::sync::atomic::{AtomicU64, Ordering};

/// A sound in the bank (the bank's stable sound id).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SoundId(pub u32);

/// A bus in the mixer graph (the graph's stable bus id).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BusId(pub u32);

/// A game-side sound source that voices can follow (for example an entity).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EmitterId(pub u32);

/// Names one played voice so the game can stop it later.
///
/// Handles are allocated by the caller ([`HandleAllocator`]), not by the mixer, so a play
/// is fire-and-forget across threads: the game knows the handle before the audio thread
/// has seen the event. A handle is a 64-bit serial that is never reused, which makes every
/// handle its own generation: a handle whose voice has ended (or was stolen, or refused)
/// can never alias a newer voice, and the mixer ignores it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VoiceHandle(u64);

impl VoiceHandle {
    /// No handle: the voice plays untracked and cannot be stopped individually.
    pub const NONE: VoiceHandle = VoiceHandle(0);

    /// True for [`VoiceHandle::NONE`].
    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The raw serial (for logging or a wire format).
    pub fn to_bits(self) -> u64 {
        self.0
    }

    /// A handle from a raw serial previously returned by [`VoiceHandle::to_bits`].
    pub fn from_bits(bits: u64) -> VoiceHandle {
        VoiceHandle(bits)
    }
}

/// Allocates [`VoiceHandle`]s on any thread without locking.
///
/// Use one allocator per mixer (share it by reference or `Arc`); two allocators would hand
/// out the same serials.
#[derive(Debug)]
pub struct HandleAllocator {
    next: AtomicU64,
}

impl HandleAllocator {
    /// A fresh allocator; the first handle is serial 1.
    pub const fn new() -> HandleAllocator {
        HandleAllocator {
            next: AtomicU64::new(1),
        }
    }

    /// The next unused handle. Never returns [`VoiceHandle::NONE`] (the 64-bit serial does
    /// not wrap in practice; if it ever did, zero is skipped).
    pub fn allocate(&self) -> VoiceHandle {
        loop {
            let v = self.next.fetch_add(1, Ordering::Relaxed);
            if v != 0 {
                return VoiceHandle(v);
            }
        }
    }
}

impl Default for HandleAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_unique_and_never_none() {
        let a = HandleAllocator::new();
        let first = a.allocate();
        let second = a.allocate();
        assert_ne!(first, second);
        assert!(!first.is_none() && !second.is_none());
        assert_eq!(VoiceHandle::from_bits(first.to_bits()), first);
        assert!(VoiceHandle::NONE.is_none());
    }
}
