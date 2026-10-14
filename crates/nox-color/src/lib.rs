//! Color management built on the underlying standards rather than the
//! OpenColorIO library: ITU-T H.273 color tags, ACES color spaces, ASC CDL,
//! `.cube`/CLF LUTs, camera log curves and a subset of the `.ocio` config
//! format. Pure Rust; `nox-gpu` turns the transforms into shader code.

pub mod cicp;
pub mod grade;
pub mod hdr;
pub mod primaries;
pub mod transfer;
pub mod working;
pub mod ycbcr;

pub use cicp::Cicp;
pub use grade::{Grade, GradeCoefficients, Wheel};
pub use hdr::ToneMapping;
pub use primaries::Primaries;
pub use transfer::Transfer;
pub use working::{InputTransform, OutputEncoding, OutputTransform, WORKING_PRIMARIES};
pub use ycbcr::{Coefficients, Matrix3x4, Quantization};

/// A 3×3 matrix, row-major, applied to column vectors.
pub type Mat3 = [[f32; 3]; 3];
