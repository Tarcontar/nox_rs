use std::path::{Path, PathBuf};

use crate::media::StreamInfo;

/// Unique identifier for a media file within the project.
pub type MediaId = u64;

/// Represents a single media file that has been imported into the project.
#[derive(Debug, Clone)]
pub struct MediaFile {
    /// Unique identifier.
    pub id: MediaId,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Display name (usually the file name).
    pub display_name: String,
    /// Video stream metadata (if available).
    pub stream_info: Option<StreamInfo>,
    /// Whether a thumbnail has been generated.
    pub has_thumbnail: bool,
}

impl MediaFile {
    /// Create a new `MediaFile` entry from a path.
    pub fn from_path(id: MediaId, path: &Path) -> Self {
        let display_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unnamed".into());

        Self {
            id,
            path: path.to_path_buf(),
            display_name,
            stream_info: None,
            has_thumbnail: false,
        }
    }

    /// Duration in seconds, or 0 if unknown.
    pub fn duration_secs(&self) -> f64 {
        self.stream_info
            .as_ref()
            .map(|s| s.duration_secs)
            .unwrap_or(0.0)
    }

    /// Formatted duration string "MM:SS".
    pub fn duration_display(&self) -> String {
        let secs = self.duration_secs();
        let m = (secs / 60.0) as u32;
        let s = (secs % 60.0) as u32;
        format!("{m:02}:{s:02}")
    }
}
