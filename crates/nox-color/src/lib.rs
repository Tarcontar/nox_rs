//! Color management built on the underlying standards rather than the
//! OpenColorIO library: ITU-T H.273 color tags, ACES color spaces, ASC CDL,
//! `.cube`/CLF LUTs, camera log curves and a subset of the `.ocio` config
//! format. Pure Rust; `nox-gpu` turns the transforms into shader code.

pub mod cicp;
pub mod ycbcr;

pub use cicp::Cicp;
pub use ycbcr::{Coefficients, Matrix3x4, Quantization};
