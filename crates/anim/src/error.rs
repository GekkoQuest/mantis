//! The runtime's error type.

use mantis_core::content::ContentHash;
use mantis_formats::FormatError;

/// Why an animation asset could not be bound or a request was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnimError {
    /// The asset breaks a format rule.
    Format(FormatError),
    /// An asset targets a skeleton with a different bone count.
    BoneCountMismatch {
        /// The skeleton's bone count.
        expected: usize,
        /// The asset's bone count.
        actual: usize,
    },
    /// The graph references a clip the resolver did not supply.
    MissingClip(ContentHash),
    /// A foot chain's bones are not an ancestor line (`root` above `mid` above `tip`).
    IkChain(usize),
    /// No parameter with that id.
    UnknownParameter,
    /// The parameter has another kind (for example a float set as a trigger).
    ParameterKind,
    /// A value is not finite.
    NonFinite,
    /// A layer, chain, or bone index is out of range.
    OutOfRange,
    /// The graph has no look-at chain.
    NoLookAt,
    /// A mesh vertex is malformed (index of the first bad vertex).
    InvalidMesh(usize),
    /// The bake frame rate is not finite and positive.
    InvalidFrameRate,
    /// The bake would exceed [`crate::vat::MAX_VAT_TEXELS`].
    VatTooLarge,
}

impl From<FormatError> for AnimError {
    fn from(e: FormatError) -> Self {
        AnimError::Format(e)
    }
}

impl core::fmt::Display for AnimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "animation error: {self:?}")
    }
}

impl std::error::Error for AnimError {}
