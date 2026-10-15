//! The models Nox can use, and fetching them: each has a pinned URL and
//! SHA-256, and is downloaded the first time an effect needs it (after the
//! user agrees), into a folder the caller chooses (the user's data
//! folder). Downloads go to a temporary file, renamed when complete and
//! verified.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Result};

/// A model Nox knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: &'static str,
    /// For people.
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    /// Lowercase hex.
    pub sha256: &'static str,
    /// In bytes.
    pub size: u64,
    pub license: &'static str,
}

/// Robust Video Matting, MobileNetV3, 32-bit floats (background removal).
pub const ROBUST_VIDEO_MATTING: ModelInfo = ModelInfo {
    id: "rvm_mobilenetv3",
    name: "Robust Video Matting",
    file: "rvm_mobilenetv3_fp32.onnx",
    url: "https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3_fp32.onnx",
    sha256: "88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828",
    size: 14_975_696,
    license: "GPL-3.0",
};

pub const MODELS: [ModelInfo; 1] = [ROBUST_VIDEO_MATTING];

/// Where Nox keeps downloaded models: `NOX_MODELS_DIR` if set, else
/// `nox/models` in the user's data folder (`$XDG_DATA_HOME` or
/// `~/.local/share` on Linux and in the Flatpak, `%LOCALAPPDATA%` on
/// Windows, `~/Library/Application Support` on macOS).
pub fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NOX_MODELS_DIR") {
        return PathBuf::from(dir);
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let data = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/share"))
    };
    data.join("nox").join("models")
}

impl ModelInfo {
    pub fn by_id(id: &str) -> Option<Self> {
        MODELS.into_iter().find(|m| m.id == id)
    }

    /// Where the model goes in `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(self.file)
    }

    /// Whether it's in `dir` (of the right size; the hash is checked when
    /// downloading).
    pub fn is_available(&self, dir: &Path) -> bool {
        std::fs::metadata(self.path(dir)).is_ok_and(|m| m.len() == self.size)
    }

    /// Downloads the model into `dir` unless it's there, calling
    /// `progress(done, total)` in bytes as it goes, and stopping with
    /// `Error::Cancelled` once `cancel` is set. Returns its path.
    pub fn download(
        &self,
        dir: &Path,
        mut progress: impl FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let path = self.path(dir);
        if self.is_available(dir) {
            return Ok(path);
        }
        let io = |source: std::io::Error, path: &Path| Error::Io {
            path: path.to_owned(),
            source,
        };
        std::fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
        let partial = dir.join(format!("{}.part", self.file));
        let mut response = ureq::get(self.url)
            .call()
            .map_err(|e| Error::Download(format!("{}: {e}", self.url)))?;
        let mut body = response.body_mut().as_reader();
        let mut file = std::fs::File::create(&partial).map_err(|e| io(e, &partial))?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut chunk = vec![0u8; 1 << 16];
        let result = loop {
            if cancel.load(Ordering::Relaxed) {
                break Err(Error::Cancelled);
            }
            let n = match body.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) => break Err(Error::Download(format!("{}: {e}", self.url))),
            };
            hasher.update(&chunk[..n]);
            if let Err(e) = file.write_all(&chunk[..n]) {
                break Err(io(e, &partial));
            }
            done += n as u64;
            progress(done, self.size);
        };
        drop(file);
        let checked = result.and_then(|()| {
            let hash: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if hash == self.sha256 && done == self.size {
                Ok(())
            } else {
                Err(Error::Download(format!(
                    "{} isn't the expected file ({done} bytes, SHA-256 {hash})",
                    self.url
                )))
            }
        });
        match checked {
            Ok(()) => {
                std::fs::rename(&partial, &path).map_err(|e| io(e, &path))?;
                tracing::info!("downloaded {} to {}", self.name, path.display());
                Ok(path)
            }
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                Err(err)
            }
        }
    }
}
