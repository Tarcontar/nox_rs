//! Test clips generated with the `ffmpeg` CLI.

// Not every test binary uses every helper.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Whether there's a Vulkan GPU for tests that need one (GPU compositing,
/// HDR). Without one they skip, unless `NOX_REQUIRE_GPU` is set (CI on
/// Linux, where lavapipe must work), which makes it a failure.
pub fn have_gpu() -> bool {
    static GPU: OnceLock<Result<(), String>> = OnceLock::new();
    match GPU.get_or_init(|| {
        nox_media::Accel::new()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }) {
        Ok(()) => true,
        Err(err) if std::env::var_os("NOX_REQUIRE_GPU").is_some() => {
            panic!("no Vulkan GPU ({err}), but NOX_REQUIRE_GPU is set")
        }
        Err(err) => {
            eprintln!("skipping: no Vulkan GPU ({err})");
            false
        }
    }
}

/// Generates `name` in the test temp dir with the given ffmpeg arguments, or
/// returns `None` (after printing why) if ffmpeg or an encoder is missing.
pub fn fixture(name: &str, args: &[&str]) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let output = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(&path)
        .output();
    match output {
        Ok(output) if output.status.success() => Some(path),
        // FFmpeg without that encoder (e.g. builds without libwebp).
        Ok(output)
            if ["Unknown encoder", "Encoder not found"]
                .iter()
                .any(|m| String::from_utf8_lossy(&output.stderr).contains(m)) =>
        {
            eprintln!("skipping: encoder not available for {name}");
            None
        }
        Ok(output) => panic!(
            "ffmpeg failed to generate {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(err) => {
            eprintln!("skipping: ffmpeg not available ({err})");
            None
        }
    }
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
