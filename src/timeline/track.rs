use super::clip::Clip;

/// A single track on the timeline (e.g. "Video 1", "Audio 1").
#[derive(Debug, Clone)]
pub struct Track {
    /// Human-readable label.
    pub name: String,
    /// Whether this track is muted.
    pub muted: bool,
    /// Whether this track is locked (prevents edits).
    pub locked: bool,
    /// Ordered list of clips on this track.
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            muted: false,
            locked: false,
            clips: Vec::new(),
        }
    }

    /// Total duration spanned by clips on this track.
    pub fn duration_secs(&self) -> f64 {
        self.clips
            .iter()
            .map(|c| c.end_secs())
            .fold(0.0_f64, f64::max)
    }
}
