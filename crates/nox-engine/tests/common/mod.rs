//! Test clips generated with the `ffmpeg` CLI.

// Not every test binary uses every helper.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

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
        Ok(output) if String::from_utf8_lossy(&output.stderr).contains("Unknown encoder") => {
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
