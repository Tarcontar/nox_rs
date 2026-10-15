//! Project and timeline data model: timelines, tracks and clips, editing
//! operations and undo.
//!
//! This crate is deliberately free of GTK, FFmpeg and Vulkan so it can be
//! tested headless and shared by the GUI, engine and CLI. File formats live
//! in their own crates (`nox-otio`).

pub mod document;
pub mod edit;
pub mod keyframes;
pub mod metadata;
pub mod relink;
pub mod time;
pub mod timeline;

pub use document::Document;
pub use edit::{Edge, EditError, ItemRef, MARKER_COLORS};
pub use keyframes::{Ease, GradeKeyframe, Keyframe, Param};
pub use metadata::{Object, Value};
/// Clip grades (`Item::grade`).
pub use nox_color::{Grade, Wheel};
pub use time::{
    Rational, Time, TimeRange, format_timecode, nominal_fps, parse_timecode, supports_drop_frame,
    timecode,
};
pub use timeline::{
    AudioSegment, BACKGROUND_REMOVAL, BlendMode, Clip, Composable, DEFAULT_MEDIA_KEY, Dip,
    DipColor, Effect, EffectKind, Gap, Item, Marker, MediaReference, Mix, ReferenceKind,
    ResolvedClip, ScalingMode, Settings, Side, Stack, Timeline, Track, TrackKind, Transform,
    Transition, TransitionKind,
};
