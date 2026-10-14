// Sizes of the scope images and the layout of their count buffer. Shared
// by build.rs, which writes them into `scope_layout.glsl` for the shaders,
// and by `scopes.rs`.

/// Signal levels on the vertical axis of the waveform and parade.
pub const LEVELS: u32 = 256;
/// Columns of the waveform image.
pub const WAVEFORM_WIDTH: u32 = 512;
/// Columns of the parade image: R, G and B side by side.
pub const PARADE_WIDTH: u32 = 3 * 256;
/// Width and height of the vectorscope image.
pub const VECTOR_SIZE: u32 = 256;
/// Bins per channel of the histogram (R, G, B, luma).
pub const HISTOGRAM_BINS: u32 = 256;
/// Height of the histogram image.
pub const HISTOGRAM_HEIGHT: u32 = 128;

/// Offsets of each scope's counts in the count buffer, in `u32`s.
pub const WAVEFORM_OFFSET: u32 = 0;
pub const PARADE_OFFSET: u32 = WAVEFORM_OFFSET + WAVEFORM_WIDTH * LEVELS;
pub const VECTOR_OFFSET: u32 = PARADE_OFFSET + PARADE_WIDTH * LEVELS;
pub const HISTOGRAM_OFFSET: u32 = VECTOR_OFFSET + VECTOR_SIZE * VECTOR_SIZE;
/// The largest histogram bin, for scaling the bars.
pub const HISTOGRAM_MAX_OFFSET: u32 = HISTOGRAM_OFFSET + 4 * HISTOGRAM_BINS;
pub const COUNT_LEN: u32 = HISTOGRAM_MAX_OFFSET + 1;
