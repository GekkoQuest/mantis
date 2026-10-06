//! Lighting (plan 8.3, decision 0003): clustered forward+ with a dynamic sun and cascaded
//! shadow maps; indirect light is cooked (probe volumes, lightmaps) and blended across
//! time-of-day keyframes, plus SSAO and a sky ambient term. No runtime bounce.

pub mod clusters;
pub mod csm;
pub mod indirect;
pub mod sh;
