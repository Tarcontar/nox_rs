pub mod clip;
pub mod track;

pub use clip::Clip;
pub use track::Track;

use std::cell::RefCell;

/// Unique identifier for timeline elements.
pub type ClipId = u64;

/// The central timeline model.
///
/// Contains an ordered list of tracks, each holding zero or more clips.
/// The timeline is the authoritative source for the current edit state.
#[derive(Debug)]
pub struct Timeline {
    pub tracks: RefCell<Vec<Track>>,
    /// Current playhead position in seconds.
    pub playhead_secs: RefCell<f64>,
    /// Pixels-per-second zoom factor for the UI.
    pub zoom: RefCell<f64>,
    next_clip_id: RefCell<u64>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            tracks: RefCell::new(vec![Track::new("Video 1"), Track::new("Audio 1")]),
            playhead_secs: RefCell::new(0.0),
            zoom: RefCell::new(100.0),
            next_clip_id: RefCell::new(1),
        }
    }

    /// Total duration of the timeline (max end of any clip on any track).
    pub fn duration_secs(&self) -> f64 {
        self.tracks
            .borrow()
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.end_secs())
            .fold(0.0_f64, f64::max)
    }

    /// Get the next unique clip ID and increment the counter.
    pub fn next_clip_id(&self) -> ClipId {
        let mut id = self.next_clip_id.borrow_mut();
        let current = *id;
        *id += 1;
        current
    }

    /// Add a clip to a track by index.  Returns the assigned clip ID.
    pub fn add_clip(
        &self,
        track_idx: usize,
        media_id: u64,
        source_duration_secs: f64,
        position_secs: f64,
    ) -> Option<ClipId> {
        let mut tracks = self.tracks.borrow_mut();
        let track = tracks.get_mut(track_idx)?;
        let clip_id = self.next_clip_id();
        let clip = Clip {
            id: clip_id,
            media_id,
            position_secs,
            in_point_secs: 0.0,
            out_point_secs: source_duration_secs,
        };
        track.clips.push(clip);
        Some(clip_id)
    }

    /// Remove a clip by its ID from all tracks.  Returns `true` if found.
    pub fn remove_clip(&self, clip_id: ClipId) -> bool {
        let mut tracks = self.tracks.borrow_mut();
        for track in tracks.iter_mut() {
            if let Some(pos) = track.clips.iter().position(|c| c.id == clip_id) {
                track.clips.remove(pos);
                return true;
            }
        }
        false
    }

    /// Split a clip at the given absolute timeline position.
    /// The original clip is trimmed and a new clip is created after the split point.
    pub fn split_clip_at(&self, clip_id: ClipId, split_secs: f64) -> Option<ClipId> {
        let mut tracks = self.tracks.borrow_mut();
        for track in tracks.iter_mut() {
            if let Some(pos) = track.clips.iter().position(|c| c.id == clip_id) {
                let clip = &track.clips[pos];
                // Ensure split is within clip bounds
                if split_secs <= clip.position_secs || split_secs >= clip.end_secs() {
                    return None;
                }

                let offset_in_source = clip.in_point_secs + (split_secs - clip.position_secs);

                // Right half
                let new_id = self.next_clip_id();
                let right = Clip {
                    id: new_id,
                    media_id: clip.media_id,
                    position_secs: split_secs,
                    in_point_secs: offset_in_source,
                    out_point_secs: clip.out_point_secs,
                };

                // Trim original (left half)
                track.clips[pos].out_point_secs = offset_in_source;

                track.clips.insert(pos + 1, right);
                return Some(new_id);
            }
        }
        None
    }
}
