//! The ground model: queries and the outdoor heightfield.

use core::fmt;

/// Ground height under a point. Implementations must be pure and
/// deterministic. `None` means "no ground here" (outside the model), and
/// movement treats it as a wall.
pub trait GroundQuery {
    /// The ground height at `(x, z)`.
    fn height_at(&self, x: f32, z: f32) -> Option<f32>;
}

/// An infinite flat plane at a fixed height.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FlatGround(pub f32);

impl GroundQuery for FlatGround {
    fn height_at(&self, _x: f32, _z: f32) -> Option<f32> {
        Some(self.0)
    }
}

/// Why a heightfield was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeightfieldError {
    /// Fewer than 2 samples along an axis, or more than `u32::MAX`.
    BadDimensions,
    /// `heights.len()` is not `width * depth`.
    SampleCountMismatch,
    /// Cell size not finite and positive, or origin not finite.
    BadSpacing,
    /// A height is not finite.
    NonFiniteHeight,
}

impl fmt::Display for HeightfieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadDimensions => "heightfield needs at least 2x2 samples",
            Self::SampleCountMismatch => "heightfield sample count does not match its dimensions",
            Self::BadSpacing => "heightfield spacing or origin is invalid",
            Self::NonFiniteHeight => "heightfield contains a non-finite height",
        })
    }
}

impl std::error::Error for HeightfieldError {}

/// A regular grid of height samples, bilinearly interpolated.
///
/// Sample `(i, j)` sits at `(origin_x + i * cell, origin_z + j * cell)` and is
/// stored at `heights[j * width + i]`. The model covers
/// `[origin_x, origin_x + (width-1)*cell] x [origin_z, origin_z + (depth-1)*cell]`;
/// outside it, [`GroundQuery::height_at`] returns `None`.
#[derive(Clone, PartialEq, Debug)]
pub struct Heightfield {
    origin_x: f32,
    origin_z: f32,
    cell: f32,
    inv_cell: f32,
    width: u32,
    depth: u32,
    heights: Vec<f32>,
}

impl Heightfield {
    /// Validates and builds a heightfield (load time; allocates).
    ///
    /// # Errors
    /// [`HeightfieldError`] for bad dimensions, spacing, or samples.
    pub fn new(
        origin_x: f32,
        origin_z: f32,
        cell: f32,
        width: u32,
        depth: u32,
        heights: Vec<f32>,
    ) -> Result<Self, HeightfieldError> {
        if width < 2 || depth < 2 {
            return Err(HeightfieldError::BadDimensions);
        }
        let count = (width as usize)
            .checked_mul(depth as usize)
            .ok_or(HeightfieldError::BadDimensions)?;
        if heights.len() != count {
            return Err(HeightfieldError::SampleCountMismatch);
        }
        if !(cell.is_finite() && cell > 0.0 && origin_x.is_finite() && origin_z.is_finite()) {
            return Err(HeightfieldError::BadSpacing);
        }
        if heights.iter().any(|h| !h.is_finite()) {
            return Err(HeightfieldError::NonFiniteHeight);
        }
        Ok(Self {
            origin_x,
            origin_z,
            cell,
            inv_cell: 1.0 / cell,
            width,
            depth,
            heights,
        })
    }

    /// Samples along x.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Samples along z.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    fn sample(&self, i: u32, j: u32) -> Option<f32> {
        self.heights
            .get(j as usize * self.width as usize + i as usize)
            .copied()
    }

    /// Splits a coordinate into a cell index and a fraction in `[0, 1]`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn locate(coord: f32, origin: f32, inv_cell: f32, samples: u32) -> Option<(u32, f32)> {
        let g = (coord - origin) * inv_cell;
        let last = (samples - 1) as f32;
        if !(g >= 0.0 && g <= last) {
            return None; // outside, or NaN
        }
        let cell = g.floor().min(last - 1.0);
        Some((cell as u32, g - cell))
    }
}

impl GroundQuery for Heightfield {
    fn height_at(&self, x: f32, z: f32) -> Option<f32> {
        let (col, fx) = Self::locate(x, self.origin_x, self.inv_cell, self.width)?;
        let (row, fz) = Self::locate(z, self.origin_z, self.inv_cell, self.depth)?;
        let h00 = self.sample(col, row)?;
        let h10 = self.sample(col + 1, row)?;
        let h01 = self.sample(col, row + 1)?;
        let h11 = self.sample(col + 1, row + 1)?;
        let near = h00 + (h10 - h00) * fx;
        let far = h01 + (h11 - h01) * fx;
        Some(near + (far - near) * fz)
    }
}
