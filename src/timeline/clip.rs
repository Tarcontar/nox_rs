use super::ClipId;
use crate::media::MediaId;

/// A clip on the timeline referencing a portion of a source media file.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Unique identifier for this clip.
    pub id: ClipId,
    /// References the source [`MediaFile`](crate::media::MediaFile).
    pub media_id: MediaId,
    /// Position of the clip on the timeline (seconds from timeline start).
    pub position_secs: f64,
    /// Source in-point: where in the source file this clip starts.
    pub in_point_secs: f64,
    /// Source out-point: where in the source file this clip ends.
    pub out_point_secs: f64,
}

impl Clip {
    /// Duration of this clip on the timeline.
    pub fn duration_secs(&self) -> f64 {
        self.out_point_secs - self.in_point_secs
    }

    /// Absolute end time on the timeline.
    pub fn end_secs(&self) -> f64 {
        self.position_secs + self.duration_secs()
    }

    /// Whether the given timeline time falls within this clip.
    pub fn contains_time(&self, timeline_secs: f64) -> bool {
        timeline_secs >= self.position_secs && timeline_secs < self.end_secs()
    }

    /// Map a timeline time to the corresponding source-file time.
    /// Returns `None` if the time is outside this clip.
    pub fn timeline_to_source(&self, timeline_secs: f64) -> Option<f64> {
        if !self.contains_time(timeline_secs) {
            return None;
        }
        Some(self.in_point_secs + (timeline_secs - self.position_secs))
    }
}
