//! std.titles contract. Extension messages are generated from
//! `schema/titles.idl` (their ids are the extension kinds); other modules
//! award titles with the [`AwardTitle`] event and read them with the
//! [`ActiveTitle`] query.

#![forbid(unsafe_code)]

pub mod generated {
    //! Generated from `schema/titles.idl`.
    #[rustfmt::skip]
    pub mod titles;
}

pub use generated::titles::*;

use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::module::{Event, Query};

/// The content table of titles: one `id name` line per title.
pub const TITLES_TABLE: &str = "std.titles.list";

/// Titles one character may hold.
pub const MAX_TITLES: usize = 64;

/// Ask the titles module to award a title (any module may send it; it is
/// granted next tick if the title exists).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AwardTitle {
    /// The character.
    pub character: u64,
    /// The title.
    pub title: u32,
}

impl StateHash for AwardTitle {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.character);
        h.write_u32(self.title);
    }
}

impl Event for AwardTitle {
    const NAME: &'static str = "std.titles.award";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> bool {
        e.u64(self.character);
        e.u32(self.title);
        true
    }

    fn load(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            character: d.u64()?,
            title: d.u32()?,
        })
    }
}

/// A character earned a title.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TitleEarned {
    /// The character.
    pub character: u64,
    /// The title.
    pub title: u32,
}

impl StateHash for TitleEarned {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.character);
        h.write_u32(self.title);
    }
}

impl Event for TitleEarned {
    const NAME: &'static str = "std.titles.earned";

    fn save(&self, e: &mut mantis_core::wire::Encoder<'_>) -> bool {
        e.u64(self.character);
        e.u32(self.title);
        true
    }

    fn load(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            character: d.u64()?,
            title: d.u32()?,
        })
    }
}

/// The title a character shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActiveTitle(pub u64);

impl Query for ActiveTitle {
    type Response = Option<u32>;
    const NAME: &'static str = "std.titles.active";
}
