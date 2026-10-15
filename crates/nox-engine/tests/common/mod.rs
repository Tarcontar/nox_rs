//! Test clips generated with the `ffmpeg` CLI.

// Not every test binary uses every helper.
#![allow(dead_code)]

use nox_project::{Clip, Composable, MediaReference, Rational, Time, TimeRange};
use std::path::{Path, PathBuf};

/// Clips from the `ffmpeg` CLI and the shared GPU, as nox-media's tests
/// make them.
#[path = "../../../nox-media/tests/common/mod.rs"]
mod media;
pub use media::fixture;

pub const FPS: Rational = Rational::new(25, 1);

/// Whether there's a Vulkan GPU for tests that need one (GPU compositing,
/// HDR). Without one they skip, unless `NOX_REQUIRE_GPU` is set (CI on
/// Linux, where lavapipe must work), which makes it a failure.
pub fn have_gpu() -> bool {
    media::gpu().is_some()
}

/// Frame `n` at 25 fps.
pub fn f(n: i64) -> Time {
    Time::from_frames(n, FPS)
}

/// A 4 s clip of one color (lossless 4:4:4).
pub fn solid(name: &str, color: &str, size: &str) -> Option<PathBuf> {
    let input = format!("color=c={color}:size={size}:rate=25:duration=4,format=yuv444p");
    fixture(name, &["-f", "lavfi", "-i", &input, "-c:v", "ffv1"])
}

/// A clip of `path` using frames `start..start + length` of 100 available.
pub fn clip(path: &Path, start: i64, length: i64) -> Composable {
    let reference = MediaReference::from_path(path, Some(TimeRange::new(f(0), f(100))));
    Composable::Clip(Clip::new(
        "c",
        reference,
        TimeRange::new(f(start), f(length)),
    ))
}

/// Polls `condition` until it holds or `timeout` passes.
pub fn wait_for(timeout: std::time::Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    condition()
}
