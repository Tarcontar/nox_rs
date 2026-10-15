//! The timeline model. Types mirror OpenTimelineIO's (`Timeline`, `Stack`,
//! `Track`, `Clip`, `Gap`, `Transition`, media references, effects and
//! markers) so `.otio` files map onto them one to one, but all times are
//! exact rationals.
//!
//! As in OTIO, a track is a sequence: each child starts where the previous
//! one ends, and empty space is an explicit `Gap`. Transitions overlap their
//! neighbors and take up no time of their own. In a stack the first child is
//! the bottom layer.

use std::path::{Path, PathBuf};

use crate::metadata::{Object, Value};
use crate::time::{Rational, Time, TimeRange};
use nox_color::{Grade, Wheel};

/// Key of the default media reference of a clip.
pub const DEFAULT_MEDIA_KEY: &str = "DEFAULT_MEDIA";

/// Frame rate of new timelines and of files that don't specify one.
pub const DEFAULT_FRAME_RATE: Rational = Rational::new(24, 1);

#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    pub name: String,
    pub metadata: Object,
    /// Time of the first frame, e.g. 01:00:00:00. Its rate is the sequence
    /// frame rate.
    pub global_start_time: Option<Time>,
    pub tracks: Stack,
    pub settings: Settings,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
    /// The folder relative media paths are resolved against: the one the
    /// `.otio` file was opened from. Not saved.
    pub media_dir: Option<PathBuf>,
}

/// Nox's own sequence settings, stored under `metadata.nox` in `.otio`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Show drop-frame timecode (only for 29.97 and 59.94 fps).
    pub drop_frame: bool,
    /// Picture size of the sequence.
    pub width: u32,
    pub height: u32,
    /// How sources of another size or shape are placed.
    pub scaling: ScalingMode,
    /// In and Out marks (timeline time, from zero): the range to export.
    /// Out is exclusive, the end of the last marked frame.
    pub mark_in: Option<Time>,
    pub mark_out: Option<Time>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            drop_frame: false,
            width: 1920,
            height: 1080,
            scaling: ScalingMode::Fit,
            mark_in: None,
            mark_out: None,
        }
    }
}

impl Settings {
    /// The range between the In and Out marks, if both are set.
    pub fn marked_range(&self) -> Option<TimeRange> {
        match (self.mark_in, self.mark_out) {
            (Some(start), Some(end)) if start < end => Some(TimeRange::from_start_end(start, end)),
            _ => None,
        }
    }

    /// Sets (or clears) the In mark. An Out mark at or before it is
    /// cleared, so the marks never describe an empty range.
    pub fn set_mark_in(&mut self, time: Option<Time>) {
        self.mark_in = time;
        if let (Some(start), Some(end)) = (self.mark_in, self.mark_out)
            && end <= start
        {
            self.mark_out = None;
        }
    }

    /// Sets (or clears) the Out mark, clearing an In mark at or after it.
    pub fn set_mark_out(&mut self, time: Option<Time>) {
        self.mark_out = time;
        if let (Some(start), Some(end)) = (self.mark_in, self.mark_out)
            && end <= start
        {
            self.mark_in = None;
        }
    }
}

/// How a source that doesn't match the sequence resolution is placed (as
/// in Resolve's "mismatched resolution files" setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScalingMode {
    /// Scale the entire image to fit, with black bars.
    #[default]
    Fit,
    /// Scale to fill the frame, cropping the overhang.
    Fill,
    /// Keep pixels 1:1, centered (crop or border).
    CenterCrop,
    /// Stretch to the frame, ignoring the aspect ratio.
    Stretch,
}

impl ScalingMode {
    /// Where a picture of `source` size goes in a sequence of `sequence`
    /// size before its transform: x, y, width, height in sequence pixels.
    /// The same placement as the compositor's (`nox_gpu::Rect::place`).
    pub fn place(self, source: (u32, u32), sequence: (u32, u32)) -> [f64; 4] {
        let (sw, sh) = (f64::from(source.0.max(1)), f64::from(source.1.max(1)));
        let (w, h) = (f64::from(sequence.0.max(1)), f64::from(sequence.1.max(1)));
        let aspect = sw / sh;
        let centered = |pw: f64, ph: f64| [(w - pw) / 2.0, (h - ph) / 2.0, pw, ph];
        match self {
            Self::Fit if aspect >= w / h => centered(w, w / aspect),
            Self::Fit => centered(h * aspect, h),
            Self::Fill if aspect >= w / h => centered(h * aspect, h),
            Self::Fill => centered(w, w / aspect),
            Self::CenterCrop => centered(sw, sh),
            Self::Stretch => [0.0, 0.0, w, h],
        }
    }

    pub const ALL: [ScalingMode; 4] = [Self::Fit, Self::Fill, Self::CenterCrop, Self::Stretch];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fit => "fit",
            Self::Fill => "fill",
            Self::CenterCrop => "center",
            Self::Stretch => "stretch",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Scale entire image to fit",
            Self::Fill => "Scale full frame with crop",
            Self::CenterCrop => "Center crop with no resizing",
            Self::Stretch => "Stretch frame to all corners",
        }
    }
}

impl Timeline {
    /// An empty timeline with one video and one audio track.
    pub fn new(name: impl Into<String>, frame_rate: Rational) -> Self {
        let mut tracks = Stack::new("tracks");
        tracks.children = vec![
            Composable::Track(Track::new("V1", TrackKind::Video)),
            Composable::Track(Track::new("A1", TrackKind::Audio)),
        ];
        Self {
            name: name.into(),
            metadata: Object::new(),
            global_start_time: Some(Time::zero(frame_rate.inverse())),
            tracks,
            settings: Settings::default(),
            extra: Object::new(),
            media_dir: None,
        }
    }

    /// The local file of `reference`, with a relative path resolved
    /// against `media_dir`, as OTIO tools do with paths relative to the
    /// `.otio` file.
    pub fn media_path(&self, reference: &MediaReference) -> Option<PathBuf> {
        let path = reference.path()?;
        Some(match &self.media_dir {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path,
        })
    }

    /// Turns relative media paths into absolute `file://` URLs, for saving
    /// the timeline away from `media_dir`.
    pub fn make_media_paths_absolute(&mut self) {
        let Some(dir) = self.media_dir.clone() else {
            return;
        };
        self.for_each_reference_mut(|reference| {
            if let Some(path) = reference.path().filter(|p| p.is_relative())
                && let ReferenceKind::External { target_url } = &mut reference.kind
            {
                *target_url = file_url(&dir.join(path));
            }
        });
    }

    /// Calls `f` with every media reference of every clip (active or not),
    /// also inside nested stacks.
    pub fn for_each_reference_mut(&mut self, mut f: impl FnMut(&mut MediaReference)) {
        fn visit(composable: &mut Composable, f: &mut dyn FnMut(&mut MediaReference)) {
            match composable {
                Composable::Clip(clip) => {
                    for (_, reference) in &mut clip.media_references {
                        f(reference);
                    }
                }
                Composable::Track(track) => track.children.iter_mut().for_each(|c| visit(c, f)),
                Composable::Stack(stack) => stack.children.iter_mut().for_each(|c| visit(c, f)),
                _ => {}
            }
        }
        self.tracks
            .children
            .iter_mut()
            .for_each(|c| visit(c, &mut f));
    }

    /// The sequence frame rate: the rate of `global_start_time` if set,
    /// otherwise the rate of the first clip, otherwise the default.
    pub fn frame_rate(&self) -> Rational {
        if let Some(start) = self.global_start_time {
            return start.timebase.inverse();
        }
        self.clips()
            .find_map(|clip| clip.item.source_range.or(clip.available_range()))
            .map_or(DEFAULT_FRAME_RATE, |range| {
                range.duration.timebase.inverse()
            })
    }

    /// Changes the sequence frame rate. Clips keep their position in time;
    /// the start time is rounded to a frame at the new rate.
    pub fn set_frame_rate(&mut self, frame_rate: Rational) {
        let start = self
            .global_start_time
            .unwrap_or(Time::zero(frame_rate.inverse()));
        self.global_start_time = Some(start.rescale(frame_rate.inverse()));
    }

    pub fn duration(&self) -> Time {
        self.tracks.duration()
    }

    /// The top-level tracks, bottom first, with their index in the stack.
    pub fn tracks(&self) -> impl DoubleEndedIterator<Item = (usize, &Track)> {
        self.tracks
            .children
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_track().map(|t| (i, t)))
    }

    pub fn track(&self, index: usize) -> Option<&Track> {
        self.tracks.children.get(index)?.as_track()
    }

    pub fn track_mut(&mut self, index: usize) -> Option<&mut Track> {
        self.tracks.children.get_mut(index)?.as_track_mut()
    }

    /// All clips, depth first.
    pub fn clips(&self) -> impl Iterator<Item = &Clip> {
        let mut clips = Vec::new();
        collect_clips(&self.tracks.children, &mut clips);
        clips.into_iter()
    }

    /// The clip that supplies the picture at `time` (timeline time, starting
    /// at zero): the topmost enabled video track with an enabled clip there.
    pub fn video_at(&self, time: Time) -> Option<ResolvedClip<'_>> {
        self.video_layers(time).into_iter().next_back()
    }

    /// Everything that makes up the picture at `time`, bottom layer first:
    /// the clip of every enabled video track, and both clips of a dissolve
    /// (the incoming one with its opacity scaled by the dissolve's
    /// progress).
    pub fn video_layers(&self, time: Time) -> Vec<ResolvedClip<'_>> {
        self.tracks.resolve(time, Some(&TrackKind::Video))
    }

    /// Every boundary between items on every top-level track, plus zero,
    /// sorted and deduplicated. Used for snapping.
    pub fn edit_points(&self) -> Vec<Time> {
        let mut points = vec![Time::zero(self.frame_rate().inverse())];
        for (_, track) in self.tracks() {
            for (i, child) in track.children.iter().enumerate() {
                if !matches!(child, Composable::Transition(_)) {
                    let range = track.range_of_child(i);
                    points.extend([range.start, range.end()]);
                }
            }
        }
        points.sort();
        points.dedup();
        points
    }
}

fn collect_clips<'a>(children: &'a [Composable], clips: &mut Vec<&'a Clip>) {
    for child in children {
        match child {
            Composable::Clip(clip) => clips.push(clip),
            Composable::Track(track) => collect_clips(&track.children, clips),
            Composable::Stack(stack) => collect_clips(&stack.children, clips),
            Composable::Gap(_) | Composable::Transition(_) => {}
        }
    }
}

/// A clip at some point of the timeline and the matching time in its media.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedClip<'a> {
    pub clip: &'a Clip,
    /// Time in the clip's media reference (same time space as
    /// `source_range` and `available_range`).
    pub source_time: Time,
    /// The clip's opacity, times the progress of a dissolve it fades in by.
    pub opacity: f64,
    /// Cut off by a wipe it comes in with (left, top, right, bottom, as
    /// fractions of the picture).
    pub wipe: [f64; 4],
    /// A color drawn with it, in a dip.
    pub dip: Option<Dip>,
}

impl ResolvedClip<'_> {
    pub fn blend_mode(&self) -> BlendMode {
        self.clip.item.blend_mode()
    }

    /// Source time from the start of the media file (decoders count from
    /// zero, while `source_time` counts from the available range's start,
    /// e.g. a start timecode).
    pub fn file_time(&self) -> Time {
        match self.clip.available_range() {
            Some(range) => self.source_time - range.start,
            None => self.source_time,
        }
    }
}

/// Where a video clip's picture goes, applied after it is fitted to the
/// sequence (Resolve's Transform and Cropping): crop, zoom and rotate
/// around the picture's center, then move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Movement as a fraction of the sequence's width (right is positive).
    pub x: f64,
    /// Movement as a fraction of the sequence's height (down is positive).
    pub y: f64,
    /// 1 is the fitted size.
    pub zoom: f64,
    /// Degrees, clockwise.
    pub rotation: f64,
    /// Cut off the left, top, right and bottom, as fractions of the
    /// picture; what remains stays in place.
    pub crop: [f64; 4],
}

impl Transform {
    /// The corners (top left, top right, bottom right, bottom left) of the
    /// cropped picture in sequence pixels, for a picture placed at `placed`
    /// (x, y, width, height) in a sequence of `sequence` size; as the
    /// compositor draws it.
    pub fn corners(&self, placed: [f64; 4], sequence: (u32, u32)) -> [(f64, f64); 4] {
        let [left, top, right, bottom] = self.crop;
        [
            (left, top),
            (1.0 - right, top),
            (1.0 - right, 1.0 - bottom),
            (left, 1.0 - bottom),
        ]
        .map(|uv| self.apply(placed, sequence, uv))
    }

    /// Where the picture's center goes, in sequence pixels.
    pub fn center(&self, placed: [f64; 4], sequence: (u32, u32)) -> (f64, f64) {
        self.apply(placed, sequence, (0.5, 0.5))
    }

    /// The sequence point of the picture point `(u, v)` (0–1 across it).
    pub fn apply(&self, placed: [f64; 4], sequence: (u32, u32), (u, v): (f64, f64)) -> (f64, f64) {
        let [x, y, w, h] = placed;
        let center = (
            x + w / 2.0 + self.x * f64::from(sequence.0),
            y + h / 2.0 + self.y * f64::from(sequence.1),
        );
        let (qx, qy) = ((u - 0.5) * w * self.zoom, (v - 0.5) * h * self.zoom);
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        (
            center.0 + cos * qx - sin * qy,
            center.1 + sin * qx + cos * qy,
        )
    }

    pub const IDENTITY: Self = Self {
        x: 0.0,
        y: 0.0,
        zoom: 1.0,
        rotation: 0.0,
        crop: [0.0; 4],
    };

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// How a layer combines with the layers below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Add,
    Multiply,
    Screen,
}

impl BlendMode {
    pub const ALL: [BlendMode; 4] = [Self::Normal, Self::Add, Self::Multiply, Self::Screen];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Add => "add",
            Self::Multiply => "multiply",
            Self::Screen => "screen",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

/// Fields shared by clips, gaps, tracks and stacks.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub metadata: Object,
    /// For clips, the used part of the media. For gaps, the duration. For
    /// tracks and stacks, an optional trim of their contents.
    pub source_range: Option<TimeRange>,
    pub effects: Vec<Effect>,
    pub markers: Vec<Marker>,
    pub enabled: bool,
    /// OTIO's item color, passed through.
    pub color: Value,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
}

impl Item {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            metadata: Object::new(),
            source_range: None,
            effects: Vec::new(),
            markers: Vec::new(),
            enabled: true,
            color: Value::Null,
            extra: Object::new(),
        }
    }

    /// A value of Nox's own metadata, `metadata.nox.<key>`.
    pub fn nox_value(&self, key: &str) -> Option<&Value> {
        self.metadata.get("nox")?.as_object()?.get(key)
    }

    /// Sets or (with `None`) removes `metadata.nox.<key>`.
    pub fn set_nox_value(&mut self, key: &str, value: Option<Value>) {
        let mut nox = match self.metadata.remove("nox") {
            Some(Value::Object(nox)) => nox,
            _ => Object::new(),
        };
        match value {
            Some(value) => nox.insert(key, value),
            None => {
                nox.remove(key);
            }
        }
        if !nox.is_empty() {
            self.metadata.insert("nox", nox);
        }
    }

    /// Clips with the same link id move and trim together (e.g. the video
    /// and audio of one file). Stored as `metadata.nox.link`.
    pub fn link(&self) -> Option<i64> {
        self.nox_value("link")?.as_i64()
    }

    pub fn set_link(&mut self, link: Option<i64>) {
        self.set_nox_value("link", link.map(Value::Int));
    }

    /// Whether a track is locked against edits (`metadata.nox.locked`).
    pub fn locked(&self) -> bool {
        self.nox_value("locked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.set_nox_value("locked", locked.then_some(Value::Bool(true)));
    }

    /// Whether a track follows ripple edits on other tracks
    /// (`metadata.nox.sync_lock`, default off), so it stays in sync.
    pub fn sync_locked(&self) -> bool {
        self.nox_value("sync_lock")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn set_sync_locked(&mut self, on: bool) {
        self.set_nox_value("sync_lock", on.then_some(Value::Bool(true)));
    }

    /// Whether an audio track is soloed (`metadata.nox.solo`): while any
    /// track is, only soloed tracks are heard.
    pub fn solo(&self) -> bool {
        self.nox_value("solo")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn set_solo(&mut self, on: bool) {
        self.set_nox_value("solo", on.then_some(Value::Bool(true)));
    }

    /// Opacity of a video clip, 0–1 (`metadata.nox.opacity`, default 1).
    pub fn opacity(&self) -> f64 {
        self.nox_value("opacity")
            .and_then(Value::as_f64)
            .map_or(1.0, |o| o.clamp(0.0, 1.0))
    }

    pub fn set_opacity(&mut self, opacity: f64) {
        let opacity = opacity.clamp(0.0, 1.0);
        self.set_nox_value("opacity", (opacity < 1.0).then_some(Value::Float(opacity)));
    }

    /// Length of the fade-in in seconds (`metadata.nox.fade_in`, default
    /// 0): video fades from transparent, audio from silence. Stored in
    /// seconds, so it keeps its length if the frame rate changes.
    pub fn fade_in(&self) -> f64 {
        self.fade("fade_in")
    }

    /// Length of the fade-out in seconds (`metadata.nox.fade_out`).
    pub fn fade_out(&self) -> f64 {
        self.fade("fade_out")
    }

    pub fn set_fade_in(&mut self, seconds: f64) {
        self.set_fade("fade_in", seconds);
    }

    pub fn set_fade_out(&mut self, seconds: f64) {
        self.set_fade("fade_out", seconds);
    }

    fn fade(&self, key: &str) -> f64 {
        self.nox_value(key)
            .and_then(Value::as_f64)
            .map_or(0.0, |s| s.max(0.0))
    }

    fn set_fade(&mut self, key: &str, seconds: f64) {
        self.set_nox_value(key, (seconds > 0.0).then_some(Value::Float(seconds)));
    }

    /// How far the fades let the item through at `local` seconds into an
    /// item `duration` seconds long: 0 at its very start or end, rising
    /// linearly to 1 over the fade.
    pub fn fade_gain(&self, local: f64, duration: f64) -> f64 {
        let local = local.clamp(0.0, duration);
        let (fade_in, fade_out) = (self.fade_in(), self.fade_out());
        let mut gain: f64 = 1.0;
        if fade_in > 0.0 {
            gain = gain.min(local / fade_in);
        }
        if fade_out > 0.0 {
            gain = gain.min((duration - local) / fade_out);
        }
        gain.clamp(0.0, 1.0)
    }

    /// Blend mode of a video clip (`metadata.nox.blend`, default normal).
    pub fn blend_mode(&self) -> BlendMode {
        self.nox_value("blend")
            .and_then(Value::as_str)
            .and_then(BlendMode::parse)
            .unwrap_or_default()
    }

    pub fn set_blend_mode(&mut self, mode: BlendMode) {
        let value = (mode != BlendMode::Normal).then(|| Value::from(mode.as_str()));
        self.set_nox_value("blend", value);
    }

    /// Volume of an audio clip as a linear factor (`metadata.nox.volume`,
    /// default 1).
    pub fn volume(&self) -> f64 {
        self.nox_value("volume")
            .and_then(Value::as_f64)
            .map_or(1.0, |g| g.max(0.0))
    }

    pub fn set_volume(&mut self, volume: f64) {
        let volume = volume.max(0.0);
        self.set_nox_value("volume", (volume != 1.0).then_some(Value::Float(volume)));
    }

    /// Primary grade of a video clip (`metadata.nox.grade`, default
    /// neutral): `lift`, `gamma`, `gain` and `offset`, each `[r, g, b,
    /// master]`.
    pub fn grade(&self) -> Grade {
        self.nox_value("grade")
            .map_or(Grade::NEUTRAL, grade_from_value)
    }

    pub fn set_grade(&mut self, grade: Grade) {
        let value = (!grade.is_neutral()).then(|| grade_to_value(&grade));
        self.set_nox_value("grade", value);
    }

    /// Position, zoom, rotation and crop of a video clip
    /// (`metadata.nox.transform`, default none).
    pub fn transform(&self) -> Transform {
        let Some(object) = self.nox_value("transform").and_then(Value::as_object) else {
            return Transform::IDENTITY;
        };
        let number =
            |key: &str, default: f64| object.get(key).and_then(Value::as_f64).unwrap_or(default);
        let mut crop = [0.0; 4];
        if let Some(values) = object.get("crop").and_then(Value::as_array) {
            for (c, v) in crop.iter_mut().zip(values) {
                *c = v.as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
            }
        }
        Transform {
            x: number("x", 0.0),
            y: number("y", 0.0),
            zoom: number("zoom", 1.0).max(0.0),
            rotation: number("rotation", 0.0),
            crop,
        }
    }

    pub fn set_transform(&mut self, transform: Transform) {
        let value = (!transform.is_identity()).then(|| {
            let mut object = Object::new();
            object.insert("x", Value::Float(transform.x));
            object.insert("y", Value::Float(transform.y));
            object.insert("zoom", Value::Float(transform.zoom));
            object.insert("rotation", Value::Float(transform.rotation));
            object.insert(
                "crop",
                Value::Array(transform.crop.iter().map(|&c| Value::Float(c)).collect()),
            );
            Value::Object(object)
        });
        self.set_nox_value("transform", value);
    }

    /// How fast the item plays its media: the product of its linear time
    /// warps (1 is normal speed), 0 if it is a freeze frame.
    pub fn speed(&self) -> f64 {
        let mut speed = 1.0;
        for effect in self.effects.iter().filter(|e| e.enabled) {
            match effect.kind {
                EffectKind::LinearTimeWarp { time_scalar } => speed *= time_scalar,
                EffectKind::FreezeFrame => return 0.0,
                _ => {}
            }
        }
        speed
    }

    /// Whether the item's background is removed (`BACKGROUND_REMOVAL`).
    pub fn removes_background(&self) -> bool {
        self.effects
            .iter()
            .any(|e| e.enabled && e.effect_name == BACKGROUND_REMOVAL)
    }

    /// Turns background removal on or off; other effects stay.
    pub fn set_background_removal(&mut self, on: bool) {
        self.effects.retain(|e| e.effect_name != BACKGROUND_REMOVAL);
        if on {
            self.effects.push(Effect {
                name: "Remove Background".into(),
                metadata: Object::new(),
                effect_name: BACKGROUND_REMOVAL.into(),
                enabled: true,
                kind: EffectKind::Generic,
                extra: Object::new(),
            });
        }
    }

    /// Whether the item holds one frame (a freeze frame effect).
    pub fn is_frozen(&self) -> bool {
        self.effects
            .iter()
            .any(|e| e.enabled && e.kind == EffectKind::FreezeFrame)
    }

    /// Replaces the item's time effects with a linear time warp of `speed`
    /// (none at 1) or a freeze frame; other effects stay.
    pub(crate) fn set_time_effects(&mut self, speed: f64, frozen: bool) {
        self.effects.retain(|e| {
            !matches!(
                e.kind,
                EffectKind::LinearTimeWarp { .. } | EffectKind::FreezeFrame
            )
        });
        let effect = |name: &str, kind| Effect {
            name: name.into(),
            metadata: Object::new(),
            effect_name: name.into(),
            enabled: true,
            kind,
            extra: Object::new(),
        };
        if frozen {
            self.effects
                .push(effect("FreezeFrame", EffectKind::FreezeFrame));
        } else if speed != 1.0 {
            self.effects.push(effect(
                "LinearTimeWarp",
                EffectKind::LinearTimeWarp { time_scalar: speed },
            ));
        }
    }

    /// Maps a time inside the item's trimmed range through its time effects
    /// (linear time warps and freeze frames). `offset` is relative to the
    /// start of the item.
    fn warp(&self, start: Time, offset: Time) -> Time {
        let mut offset = offset;
        for effect in &self.effects {
            if let EffectKind::LinearTimeWarp { time_scalar } = effect.kind {
                offset = scale(offset, time_scalar);
            } else if matches!(effect.kind, EffectKind::FreezeFrame) {
                offset = Time::zero(offset.timebase);
            }
        }
        start + offset
    }
}

/// A grade from its stored form: `lift`, `gamma`, `gain` and `offset`, each
/// `[r, g, b, master]`; missing or broken wheels are neutral.
pub(crate) fn grade_from_value(value: &Value) -> Grade {
    let Some(grade) = value.as_object() else {
        return Grade::NEUTRAL;
    };
    let wheel = |key: &str, neutral: Wheel| {
        let values: Vec<f32> = grade
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_f64().map(|v| v as f32))
                    .collect()
            })
            .unwrap_or_default();
        match values[..] {
            [r, g, b, master] if [r, g, b, master].iter().all(|v| v.is_finite()) => Wheel {
                rgb: [r, g, b],
                master,
            },
            _ => neutral,
        }
    };
    Grade {
        lift: wheel("lift", Wheel::ZERO),
        gamma: wheel("gamma", Wheel::ONE),
        gain: wheel("gain", Wheel::ONE),
        offset: wheel("offset", Wheel::ZERO),
    }
}

pub(crate) fn grade_to_value(grade: &Grade) -> Value {
    let mut object = Object::new();
    for (key, wheel) in [
        ("lift", grade.lift),
        ("gamma", grade.gamma),
        ("gain", grade.gain),
        ("offset", grade.offset),
    ] {
        let [r, g, b] = wheel.rgb;
        let values = [r, g, b, wheel.master]
            .iter()
            .map(|&v| Value::Float(f64::from(v)))
            .collect();
        object.insert(key, Value::Array(values));
    }
    Value::Object(object)
}

/// `time * factor`, with the factor approximated as a fraction.
fn scale(time: Time, factor: f64) -> Time {
    let Some(factor) = Rational::approximate(factor, 10_000) else {
        return time;
    };
    let ticks = time.ticks as i128 * factor.num as i128;
    let timebase = Rational::new(time.timebase.num, time.timebase.den * factor.den);
    Time::new(ticks.try_into().unwrap_or(i64::MAX), timebase)
}

/// An element of a track or stack.
#[derive(Debug, Clone, PartialEq)]
pub enum Composable {
    Clip(Clip),
    Gap(Gap),
    Transition(Transition),
    Track(Track),
    Stack(Stack),
}

impl Composable {
    /// Time the element occupies in its parent; zero for transitions.
    pub fn duration(&self) -> Time {
        match self {
            Self::Clip(clip) => clip.duration(),
            Self::Gap(gap) => gap.duration(),
            Self::Transition(t) => Time::zero(t.in_offset.timebase),
            Self::Track(track) => track.duration(),
            Self::Stack(stack) => stack.duration(),
        }
    }

    pub fn item(&self) -> Option<&Item> {
        match self {
            Self::Clip(c) => Some(&c.item),
            Self::Gap(g) => Some(&g.item),
            Self::Track(t) => Some(&t.item),
            Self::Stack(s) => Some(&s.item),
            Self::Transition(_) => None,
        }
    }

    pub fn item_mut(&mut self) -> Option<&mut Item> {
        match self {
            Self::Clip(c) => Some(&mut c.item),
            Self::Gap(g) => Some(&mut g.item),
            Self::Track(t) => Some(&mut t.item),
            Self::Stack(s) => Some(&mut s.item),
            Self::Transition(_) => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Transition(t) => &t.name,
            _ => &self.item().expect("item").name,
        }
    }

    pub fn as_clip(&self) -> Option<&Clip> {
        match self {
            Self::Clip(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_clip_mut(&mut self) -> Option<&mut Clip> {
        match self {
            Self::Clip(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_track(&self) -> Option<&Track> {
        match self {
            Self::Track(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_track_mut(&mut self) -> Option<&mut Track> {
        match self {
            Self::Track(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_gap(&self) -> bool {
        matches!(self, Self::Gap(_))
    }

    /// Clips at `time` (relative to the start of this element), bottom
    /// first. `kind` restricts nested tracks to one kind.
    fn resolve(&self, time: Time, kind: Option<&TrackKind>) -> Vec<ResolvedClip<'_>> {
        if self.item().is_some_and(|item| !item.enabled) {
            return Vec::new();
        }
        match self {
            Self::Clip(clip) => {
                let Some(range) = clip.trimmed_range() else {
                    return Vec::new();
                };
                let source_time = clip.item.warp(range.start, time);
                let fade = clip
                    .item
                    .fade_gain(time.as_seconds(), range.duration.as_seconds());
                let opacity = clip
                    .item
                    .value_at(crate::Param::Opacity, source_time.as_seconds());
                vec![ResolvedClip {
                    clip,
                    source_time,
                    opacity: opacity * fade,
                    wipe: [0.0; 4],
                    dip: None,
                }]
            }
            Self::Track(track) => track.resolve(time, kind),
            Self::Stack(stack) => stack.resolve(time, kind),
            Self::Gap(_) | Self::Transition(_) => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    pub item: Item,
}

impl Gap {
    pub fn new(duration: Time) -> Self {
        let mut item = Item::new("");
        item.source_range = Some(TimeRange::new(Time::zero(duration.timebase), duration));
        Self { item }
    }

    pub fn duration(&self) -> Time {
        self.item
            .source_range
            .map_or(Time::zero(DEFAULT_FRAME_RATE.inverse()), |r| r.duration)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub item: Item,
    /// Alternative media (e.g. proxies) by key, in file order.
    pub media_references: Vec<(String, MediaReference)>,
    pub active_media_reference_key: String,
}

impl Clip {
    pub fn new(name: impl Into<String>, media: MediaReference, source_range: TimeRange) -> Self {
        let mut item = Item::new(name);
        item.source_range = Some(source_range);
        Self {
            item,
            media_references: vec![(DEFAULT_MEDIA_KEY.to_owned(), media)],
            active_media_reference_key: DEFAULT_MEDIA_KEY.to_owned(),
        }
    }

    pub fn media_reference(&self) -> Option<&MediaReference> {
        self.media_references
            .iter()
            .find(|(key, _)| *key == self.active_media_reference_key)
            .map(|(_, r)| r)
    }

    pub fn available_range(&self) -> Option<TimeRange> {
        self.media_reference()?.available_range
    }

    /// The media time `local` into the clip (from its start on the
    /// timeline), through its time effects; keyframes are in this time.
    pub fn media_time(&self, local: Time) -> Option<Time> {
        let range = self.trimmed_range()?;
        Some(self.item.warp(range.start, local))
    }

    /// The used part of the media: `source_range`, or all of it.
    pub fn trimmed_range(&self) -> Option<TimeRange> {
        self.item.source_range.or_else(|| self.available_range())
    }

    pub fn duration(&self) -> Time {
        self.trimmed_range()
            .map_or(Time::zero(DEFAULT_FRAME_RATE.inverse()), |r| r.duration)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub name: String,
    pub metadata: Object,
    /// E.g. `SMPTE_Dissolve`.
    pub transition_type: String,
    /// How far the transition reaches into the preceding item.
    pub in_offset: Time,
    /// How far the transition reaches into the following item.
    pub out_offset: Time,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
}

/// What a transition does. Dissolves are OTIO's `SMPTE_Dissolve`; the
/// others are `Custom_Transition`s named in `metadata.nox.transition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransitionKind {
    #[default]
    Dissolve,
    /// The incoming picture slides in over the outgoing one from a side,
    /// revealed by a moving edge.
    Wipe(Side),
    /// The outgoing picture fades to a color, then the incoming one fades
    /// in from it.
    Dip(DipColor),
}

/// The color a dip goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DipColor {
    Black,
    White,
}

impl DipColor {
    /// The color as display R'G'B' (BT.709), 0–1.
    pub fn rgb(self) -> [f64; 3] {
        match self {
            Self::Black => [0.0; 3],
            Self::White => [1.0; 3],
        }
    }
}

/// A solid color drawn with a layer during a dip: just before it (below)
/// or just after it (above), at `opacity`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dip {
    pub color: DipColor,
    pub opacity: f64,
    pub before: bool,
}

/// The side a wipe starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl TransitionKind {
    pub const ALL: [TransitionKind; 7] = [
        Self::Dissolve,
        Self::Wipe(Side::Left),
        Self::Wipe(Side::Right),
        Self::Wipe(Side::Top),
        Self::Wipe(Side::Bottom),
        Self::Dip(DipColor::Black),
        Self::Dip(DipColor::White),
    ];

    /// The name under `metadata.nox.transition` (and in `as_str`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dissolve => "dissolve",
            Self::Wipe(Side::Left) => "wipe-left",
            Self::Wipe(Side::Right) => "wipe-right",
            Self::Wipe(Side::Top) => "wipe-top",
            Self::Wipe(Side::Bottom) => "wipe-bottom",
            Self::Dip(DipColor::Black) => "dip-black",
            Self::Dip(DipColor::White) => "dip-white",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }

    /// English name, also the transition's OTIO `name`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dissolve => "Cross Dissolve",
            Self::Wipe(Side::Left) => "Wipe from Left",
            Self::Wipe(Side::Right) => "Wipe from Right",
            Self::Wipe(Side::Top) => "Wipe from Top",
            Self::Wipe(Side::Bottom) => "Wipe from Bottom",
            Self::Dip(DipColor::Black) => "Dip to Black",
            Self::Dip(DipColor::White) => "Dip to White",
        }
    }
}

impl Transition {
    /// A transition of `kind` reaching `in_offset` and `out_offset` across
    /// the cut.
    pub fn new(kind: TransitionKind, in_offset: Time, out_offset: Time) -> Self {
        let mut transition = Self {
            name: String::new(),
            metadata: Object::new(),
            transition_type: String::new(),
            in_offset,
            out_offset,
            extra: Object::new(),
        };
        transition.set_kind(kind);
        transition
    }

    pub fn kind(&self) -> TransitionKind {
        if self.transition_type == "SMPTE_Dissolve" {
            return TransitionKind::Dissolve;
        }
        self.metadata
            .get("nox")
            .and_then(Value::as_object)
            .and_then(|nox| nox.get("transition"))
            .and_then(Value::as_str)
            .and_then(TransitionKind::parse)
            // Unknown types play as dissolves.
            .unwrap_or_default()
    }

    pub fn set_kind(&mut self, kind: TransitionKind) {
        self.name = kind.label().into();
        let mut nox = match self.metadata.remove("nox") {
            Some(Value::Object(nox)) => nox,
            _ => Object::new(),
        };
        if kind == TransitionKind::Dissolve {
            self.transition_type = "SMPTE_Dissolve".into();
            nox.remove("transition");
        } else {
            self.transition_type = "Custom_Transition".into();
            nox.insert("transition", kind.as_str());
        }
        if !nox.is_empty() {
            self.metadata.insert("nox", nox);
        }
    }

    /// How the two pictures show at `progress` (0–1) through the
    /// transition.
    pub fn mix(&self, progress: f64) -> Mix {
        let p = progress.clamp(0.0, 1.0);
        let simple = |outgoing, incoming, wipe| Mix {
            outgoing,
            incoming,
            wipe,
            dip_out: None,
            dip_in: None,
        };
        match self.kind() {
            TransitionKind::Dissolve => simple(1.0, p, [0.0; 4]),
            // The color covers the outgoing picture, then the incoming one
            // covers the color.
            TransitionKind::Dip(color) if p < 0.5 => Mix {
                dip_out: Some(Dip {
                    color,
                    opacity: 2.0 * p,
                    before: false,
                }),
                ..simple(1.0, 0.0, [0.0; 4])
            },
            TransitionKind::Dip(color) => Mix {
                dip_in: Some(Dip {
                    color,
                    opacity: 1.0,
                    before: true,
                }),
                ..simple(0.0, 2.0 * p - 1.0, [0.0; 4])
            },
            TransitionKind::Wipe(side) => {
                let hidden = 1.0 - p;
                let crop = match side {
                    Side::Left => [0.0, 0.0, hidden, 0.0],
                    Side::Right => [hidden, 0.0, 0.0, 0.0],
                    Side::Top => [0.0, 0.0, 0.0, hidden],
                    Side::Bottom => [0.0, hidden, 0.0, 0.0],
                };
                simple(1.0, 1.0, crop)
            }
        }
    }
}

/// How a transition shows its two pictures at some point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mix {
    /// Opacity factors of the outgoing and incoming picture.
    pub outgoing: f64,
    pub incoming: f64,
    /// Cut off the incoming picture (left, top, right, bottom).
    pub wipe: [f64; 4],
    /// Solid colors drawn with the outgoing and the incoming picture.
    pub dip_out: Option<Dip>,
    pub dip_in: Option<Dip>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackKind {
    Video,
    Audio,
    Other(String),
}

impl TrackKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Other(kind) => kind,
        }
    }

    pub fn parse(kind: &str) -> Self {
        match kind {
            "Video" => Self::Video,
            "Audio" => Self::Audio,
            _ => Self::Other(kind.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub item: Item,
    pub kind: TrackKind,
    pub children: Vec<Composable>,
}

impl Track {
    pub fn new(name: impl Into<String>, kind: TrackKind) -> Self {
        Self {
            item: Item::new(name),
            kind,
            children: Vec::new(),
        }
    }

    /// Length of the contents, ignoring the track's own `source_range`.
    pub fn content_duration(&self) -> Time {
        self.children
            .iter()
            .fold(Time::zero(DEFAULT_FRAME_RATE.inverse()), |sum, c| {
                sum + c.duration()
            })
    }

    pub fn duration(&self) -> Time {
        self.item
            .source_range
            .map_or_else(|| self.content_duration(), |r| r.duration)
    }

    /// Where child `index` sits in the track's contents. For transitions,
    /// the overlap with their neighbors.
    pub fn range_of_child(&self, index: usize) -> TimeRange {
        let start = self.children[..index]
            .iter()
            .fold(Time::zero(DEFAULT_FRAME_RATE.inverse()), |sum, c| {
                sum + c.duration()
            });
        match &self.children[index] {
            Composable::Transition(t) => {
                TimeRange::from_start_end(start - t.in_offset, start + t.out_offset)
            }
            child => TimeRange::new(start, child.duration()),
        }
    }

    /// Ranges of all children, in order.
    pub fn child_ranges(&self) -> impl Iterator<Item = (TimeRange, &Composable)> {
        let mut start = Time::zero(DEFAULT_FRAME_RATE.inverse());
        self.children.iter().map(move |child| {
            let range = match child {
                Composable::Transition(t) => {
                    TimeRange::from_start_end(start - t.in_offset, start + t.out_offset)
                }
                _ => TimeRange::new(start, child.duration()),
            };
            start += child.duration();
            (range, child)
        })
    }

    /// The non-transition child covering `time` (in content time).
    pub fn child_at(&self, time: Time) -> Option<(usize, TimeRange)> {
        self.child_ranges()
            .enumerate()
            .find(|(_, (range, child))| {
                !matches!(child, Composable::Transition(_)) && range.contains(time)
            })
            .map(|(i, (range, _))| (i, range))
    }

    fn resolve(&self, time: Time, kind: Option<&TrackKind>) -> Vec<ResolvedClip<'_>> {
        if !self.item.enabled || kind.is_some_and(|kind| *kind != self.kind) {
            return Vec::new();
        }
        let time = match self.item.source_range {
            Some(range) if !range.contains(range.start + time) => return Vec::new(),
            Some(range) => range.start + time,
            None => time,
        };
        // Inside a dissolve both neighbors show: the outgoing item runs on
        // past the cut, the incoming one starts early and fades in.
        let ranges: Vec<_> = self.child_ranges().map(|(r, _)| r).collect();
        for (i, child) in self.children.iter().enumerate() {
            let Composable::Transition(_) = child else {
                continue;
            };
            let range = ranges[i];
            if !range.contains(time) || i == 0 || i + 1 >= self.children.len() {
                continue;
            }
            let (before, after) = (&self.children[i - 1], &self.children[i + 1]);
            if matches!(before, Composable::Transition(_))
                || matches!(after, Composable::Transition(_))
            {
                continue;
            }
            let Composable::Transition(transition) = child else {
                unreachable!("checked above")
            };
            let progress = (time - range.start).as_seconds() / range.duration.as_seconds();
            let mix = transition.mix(progress);
            let mut layers: Vec<ResolvedClip<'_>> = before
                .resolve(time - ranges[i - 1].start, kind)
                .into_iter()
                .map(|mut layer| {
                    layer.opacity *= mix.outgoing;
                    layer.dip = mix.dip_out;
                    layer
                })
                .collect();
            layers.extend(
                after
                    .resolve(time - ranges[i + 1].start, kind)
                    .into_iter()
                    .map(|mut layer| {
                        layer.opacity *= mix.incoming;
                        layer.wipe = mix.wipe;
                        layer.dip = mix.dip_in;
                        layer
                    }),
            );
            return layers;
        }
        match self.child_at(time) {
            Some((index, range)) => self.children[index].resolve(time - range.start, kind),
            None => Vec::new(),
        }
    }

    /// The audio of this track overlapping `range` (content time), with
    /// crossfades for transitions.
    fn audio_segments(&self, range: TimeRange, out: &mut Vec<AudioSegment>) {
        let ranges: Vec<_> = self.child_ranges().map(|(r, _)| r).collect();
        let transition = |i: usize| match self.children.get(i) {
            Some(Composable::Transition(t)) => Some(t),
            _ => None,
        };
        for (i, child) in self.children.iter().enumerate() {
            let Composable::Clip(clip) = child else {
                continue;
            };
            let (Some(trimmed), Some(path)) = (
                clip.trimmed_range(),
                clip.media_reference().and_then(MediaReference::path),
            ) else {
                continue;
            };
            if !clip.item.enabled {
                continue;
            }
            let mut place = ranges[i];
            let mut source_start = trimmed.start;
            let mut fade_in = Time::zero(place.start.timebase);
            let mut fade_out = Time::zero(place.start.timebase);
            if let Some(t) = i.checked_sub(1).and_then(transition) {
                place = TimeRange::from_start_end(place.start - t.in_offset, place.end());
                source_start -= t.in_offset;
                fade_in = t.in_offset + t.out_offset;
            }
            if let Some(t) = transition(i + 1) {
                place = TimeRange::from_start_end(place.start, place.end() + t.out_offset);
                fade_out = t.in_offset + t.out_offset;
            }
            // The clip's own fades, unless a crossfade is longer.
            let seconds = |s: f64| Time::from_seconds(s, place.start.timebase);
            fade_in = fade_in.max(seconds(clip.item.fade_in()));
            fade_out = fade_out.max(seconds(clip.item.fade_out()));
            if !place.overlaps(range) {
                continue;
            }
            let file_start = clip
                .available_range()
                .map_or(Time::zero(source_start.timebase), |r| r.start);
            // Volume keyframes, from media time to timeline time.
            let shift = place.start.as_seconds() - source_start.as_seconds();
            let volume_keys = clip
                .item
                .keyframes(crate::Param::Volume)
                .into_iter()
                .map(|k| crate::Keyframe {
                    time: k.time + shift,
                    ..k
                })
                .collect();
            out.push(AudioSegment {
                path,
                place,
                source_start: source_start - file_start,
                volume: clip.item.volume(),
                volume_keys,
                speed: clip.item.speed(),
                fade_in,
                fade_out,
            });
        }
    }
}

/// A piece of audio to play: `place` in timeline time, starting at
/// `source_start` in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSegment {
    pub path: PathBuf,
    pub place: TimeRange,
    /// From the start of the file.
    pub source_start: Time,
    pub volume: f64,
    /// Volume keyframes in timeline seconds; if any, they replace `volume`.
    pub volume_keys: Vec<crate::Keyframe>,
    /// How fast the source plays (1 normal; 0 for a freeze frame: silent).
    /// Changed speed changes the pitch, as varispeed on tape.
    pub speed: f64,
    /// Length of the fade at the start (a crossfade with the previous clip).
    pub fade_in: Time,
    /// Length of the fade at the end.
    pub fade_out: Time,
}

impl AudioSegment {
    /// The gain at timeline time `time`: the clip's volume with equal-power
    /// fades, zero outside the segment.
    pub fn gain_at(&self, time: f64) -> f64 {
        let start = self.place.start.as_seconds();
        let end = self.place.end().as_seconds();
        if time < start || time >= end {
            return 0.0;
        }
        let mut gain = if self.volume_keys.is_empty() {
            self.volume
        } else {
            crate::keyframes::interpolate(&self.volume_keys, time).max(0.0)
        };
        let fade_in = self.fade_in.as_seconds();
        if fade_in > 0.0 && time < start + fade_in {
            gain *= (std::f64::consts::FRAC_PI_2 * (time - start) / fade_in).sin();
        }
        let fade_out = self.fade_out.as_seconds();
        if fade_out > 0.0 && time > end - fade_out {
            gain *= (std::f64::consts::FRAC_PI_2 * (end - time) / fade_out).sin();
        }
        gain
    }
}

impl Timeline {
    /// The audio playing during `range`: clips of every enabled top-level
    /// audio track.
    pub fn audio_segments(&self, range: TimeRange) -> Vec<AudioSegment> {
        let mut segments = Vec::new();
        let audible = |track: &Track| track.kind == TrackKind::Audio && track.item.enabled;
        let soloing = self
            .tracks()
            .any(|(_, track)| audible(track) && track.item.solo());
        for (_, track) in self.tracks() {
            if audible(track) && (!soloing || track.item.solo()) {
                track.audio_segments(range, &mut segments);
            }
        }
        if let Some(dir) = &self.media_dir {
            for segment in &mut segments {
                if segment.path.is_relative() {
                    segment.path = dir.join(&segment.path);
                }
            }
        }
        segments
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stack {
    pub item: Item,
    pub children: Vec<Composable>,
}

impl Stack {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            item: Item::new(name),
            children: Vec::new(),
        }
    }

    pub fn duration(&self) -> Time {
        self.item.source_range.map_or_else(
            || {
                self.children
                    .iter()
                    .map(Composable::duration)
                    .max()
                    .unwrap_or(Time::zero(DEFAULT_FRAME_RATE.inverse()))
            },
            |r| r.duration,
        )
    }

    fn resolve(&self, time: Time, kind: Option<&TrackKind>) -> Vec<ResolvedClip<'_>> {
        if !self.item.enabled {
            return Vec::new();
        }
        let time = match self.item.source_range {
            Some(range) if !range.contains(range.start + time) => return Vec::new(),
            Some(range) => range.start + time,
            None => time,
        };
        self.children
            .iter()
            .filter(|child| time < child.duration())
            .flat_map(|child| child.resolve(time, kind))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaReference {
    pub name: String,
    pub metadata: Object,
    /// The media's extent. `None` if unknown or unlimited (stills).
    pub available_range: Option<TimeRange>,
    /// OTIO's `Box2d` image bounds, passed through.
    pub available_image_bounds: Value,
    pub kind: ReferenceKind,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceKind {
    /// A file or URL.
    External { target_url: String },
    /// Media that couldn't be found when the timeline was written.
    Missing,
    /// Synthesized media such as color bars.
    Generator {
        generator_kind: String,
        parameters: Object,
    },
    /// Any other reference type (e.g. `ImageSequenceReference.1`); its
    /// fields are in `extra`.
    Other { schema: String },
}

impl MediaReference {
    pub fn new(kind: ReferenceKind, available_range: Option<TimeRange>) -> Self {
        Self {
            name: String::new(),
            metadata: Object::new(),
            available_range,
            available_image_bounds: Value::Null,
            kind,
            extra: Object::new(),
        }
    }

    /// A reference to a local file.
    pub fn from_path(path: &Path, available_range: Option<TimeRange>) -> Self {
        let mut reference = Self::new(
            ReferenceKind::External {
                target_url: file_url(path),
            },
            available_range,
        );
        if let Some(name) = path.file_name() {
            reference.name = name.to_string_lossy().into_owned();
        }
        reference
    }

    /// The local file for `file://` URLs and plain paths.
    pub fn path(&self) -> Option<PathBuf> {
        match &self.kind {
            ReferenceKind::External { target_url } => url_to_path(target_url),
            _ => None,
        }
    }
}

/// `file://` URL for an absolute path, percent-encoding everything except
/// unreserved characters and `/`. Windows paths take RFC 8089's forms:
/// `C:\a\b` becomes `file:///C:/a/b`, `\\server\share\a` becomes
/// `file://server/share/a`.
pub fn file_url(path: &Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().replace('\\', "/");
        // Verbatim paths (`\\?\C:\…`, `\\?\UNC\server\…`) as plain ones.
        let text = match text.strip_prefix("//?/") {
            Some(rest) => match rest.strip_prefix("UNC/") {
                Some(unc) => format!("//{unc}"),
                None => rest.to_owned(),
            },
            None => text,
        };
        match text.strip_prefix("//") {
            // A share: its server is the URL's host.
            Some(unc) => format!("file://{}", percent_encode(unc.as_bytes(), b"/")),
            None if text.as_bytes().get(1) == Some(&b':') => {
                format!("file:///{}", percent_encode(text.as_bytes(), b"/:"))
            }
            None => format!("file://{}", percent_encode(text.as_bytes(), b"/")),
        }
    }
    #[cfg(not(windows))]
    {
        format!(
            "file://{}",
            percent_encode(path.as_os_str().as_encoded_bytes(), b"/")
        )
    }
}

/// Percent-encodes all bytes but unreserved characters and `keep`.
fn percent_encode(bytes: &[u8], keep: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || keep.contains(&byte) {
            text.push(byte as char);
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

/// The local path of a `file:` URL (with or without host) or of a plain
/// path. `None` for other schemes.
pub fn url_to_path(url: &str) -> Option<PathBuf> {
    // The host of `file://host/…`, if it names a machine (Windows shares).
    let mut host = "";
    let path = if let Some(rest) = url.strip_prefix("file://") {
        let slash = rest.find('/').unwrap_or(rest.len());
        if &rest[..slash] != "localhost" {
            host = &rest[..slash];
        }
        &rest[slash..]
    } else if let Some(rest) = url.strip_prefix("file:") {
        rest
    } else if url.contains("://") {
        return None;
    } else {
        return Some(PathBuf::from(url));
    };
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&h), Some(&l)) if hex(h).is_some() && hex(l).is_some() => {
                decoded.push((hex(h)? * 16 + hex(l)?) as u8);
                i += 3;
            }
            (b, _, _) => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let _ = host; // Unix has no network paths in URLs.
        Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }
    #[cfg(not(unix))]
    {
        // Windows: `file:///C:/x` decodes to `/C:/x`; shares keep their host.
        let text = String::from_utf8_lossy(&decoded).into_owned();
        let text = match text.as_bytes() {
            _ if !host.is_empty() => format!("//{host}{text}"),
            [b'/', _, b':', ..] => text[1..].to_owned(),
            _ => text,
        };
        Some(PathBuf::from(text.replace('/', "\\")))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    pub name: String,
    pub metadata: Object,
    pub effect_name: String,
    pub enabled: bool,
    pub kind: EffectKind,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
}

/// The `effect_name` of background removal (nox-ai's matting): an OTIO
/// effect with only a name, so other tools keep it.
pub const BACKGROUND_REMOVAL: &str = "nox.remove_background";

#[derive(Debug, Clone, PartialEq)]
pub enum EffectKind {
    /// `Effect.1`: described only by its name and metadata.
    Generic,
    /// Plays the media `time_scalar` times as fast.
    LinearTimeWarp { time_scalar: f64 },
    /// Holds the first frame.
    FreezeFrame,
    /// Any other effect schema; its fields are in `extra`.
    Other { schema: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub name: String,
    pub metadata: Object,
    /// One of OTIO's color names: `RED`, `PINK`, `ORANGE`, `YELLOW`,
    /// `GREEN`, `CYAN`, `BLUE`, `PURPLE`, `MAGENTA`, `BLACK`, `WHITE`.
    pub color: String,
    /// In the time space of the item the marker is attached to.
    pub marked_range: TimeRange,
    pub comment: String,
    /// Fields from other tools or OTIO versions that Nox doesn't know,
    /// passed through.
    pub extra: Object,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: i64) -> Time {
        Time::from_frames(n, Rational::new(24, 1))
    }

    fn clip(name: &str, start: i64, duration: i64) -> Composable {
        let reference = MediaReference::from_path(
            Path::new(&format!("/media/{name}.mov")),
            Some(TimeRange::new(frames(0), frames(1000))),
        );
        Composable::Clip(Clip::new(
            name,
            reference,
            TimeRange::new(frames(start), frames(duration)),
        ))
    }

    #[test]
    fn background_removal_is_an_effect() {
        let Composable::Clip(mut c) = clip("a", 0, 10) else {
            unreachable!()
        };
        c.item.set_time_effects(2.0, false);
        assert!(!c.item.removes_background());
        c.item.set_background_removal(true);
        c.item.set_background_removal(true);
        assert!(c.item.removes_background());
        assert_eq!(c.item.effects.len(), 2, "one of each");
        assert_eq!(c.item.speed(), 2.0);
        c.item.set_background_removal(false);
        assert!(!c.item.removes_background());
        assert_eq!(c.item.speed(), 2.0, "the speed stays");
    }

    #[test]
    fn track_ranges() {
        let mut track = Track::new("V1", TrackKind::Video);
        track.children = vec![
            clip("a", 10, 5),
            Composable::Gap(Gap::new(frames(3))),
            clip("b", 0, 4),
        ];
        assert_eq!(track.duration(), frames(12));
        assert_eq!(
            track.range_of_child(2),
            TimeRange::new(frames(8), frames(4))
        );
        assert_eq!(track.child_at(frames(4)).map(|(i, _)| i), Some(0));
        assert_eq!(track.child_at(frames(5)).map(|(i, _)| i), Some(1));
        assert_eq!(track.child_at(frames(12)), None);
    }

    #[test]
    fn video_at_picks_topmost_enabled_clip() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        let v1 = timeline.track_mut(0).unwrap();
        v1.children = vec![clip("low", 100, 10)];
        let mut v2 = Track::new("V2", TrackKind::Video);
        v2.children = vec![Composable::Gap(Gap::new(frames(5))), clip("high", 0, 2)];
        timeline.tracks.children.push(Composable::Track(v2));

        let at = |timeline: &Timeline, f| {
            let resolved = timeline.video_at(frames(f))?;
            Some((resolved.clip.item.name.clone(), resolved.source_time))
        };
        assert_eq!(at(&timeline, 0), Some(("low".into(), frames(100))));
        assert_eq!(at(&timeline, 5), Some(("high".into(), frames(0))));
        assert_eq!(at(&timeline, 7), Some(("low".into(), frames(107))));
        assert_eq!(at(&timeline, 10), None);

        timeline.track_mut(2).unwrap().item.enabled = false;
        assert_eq!(at(&timeline, 5), Some(("low".into(), frames(105))));
    }

    #[test]
    fn time_warp_and_freeze_frame() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        timeline.track_mut(0).unwrap().children = vec![clip("a", 10, 10)];
        let warp = |kind| Effect {
            name: String::new(),
            metadata: Object::new(),
            effect_name: String::new(),
            enabled: true,
            kind,
            extra: Object::new(),
        };
        let set_effects = |timeline: &mut Timeline, effects| {
            timeline.track_mut(0).unwrap().children[0]
                .item_mut()
                .unwrap()
                .effects = effects;
        };
        set_effects(
            &mut timeline,
            vec![warp(EffectKind::LinearTimeWarp { time_scalar: 2.0 })],
        );
        assert_eq!(
            timeline.video_at(frames(3)).unwrap().source_time,
            frames(16)
        );
        set_effects(&mut timeline, vec![warp(EffectKind::FreezeFrame)]);
        assert_eq!(
            timeline.video_at(frames(3)).unwrap().source_time,
            frames(10)
        );
    }

    #[test]
    fn muted_audio_tracks_are_not_heard() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        timeline.track_mut(1).unwrap().children = vec![clip("a", 0, 10)];
        let range = TimeRange::new(frames(0), frames(10));
        assert_eq!(timeline.audio_segments(range).len(), 1);
        timeline.track_mut(1).unwrap().item.enabled = false;
        assert!(timeline.audio_segments(range).is_empty());
    }

    #[test]
    fn fades_scale_opacity_and_volume() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        timeline.track_mut(0).unwrap().children = vec![clip("v", 0, 48)];
        timeline.track_mut(1).unwrap().children = vec![clip("a", 0, 48)];
        for track in [0, 1] {
            let Composable::Clip(c) = &mut timeline.track_mut(track).unwrap().children[0] else {
                unreachable!()
            };
            c.item.set_fade_in(0.5);
            c.item.set_fade_out(1.0);
        }
        let opacity = |f| timeline.video_at(frames(f)).unwrap().opacity;
        assert_eq!(opacity(0), 0.0);
        assert_eq!(opacity(6), 0.5);
        assert_eq!(opacity(12), 1.0);
        assert_eq!(opacity(24), 1.0);
        assert_eq!(opacity(36), 0.5);
        assert!(opacity(47) < 0.05);
        // Audio fades the same way (with equal-power curves).
        let segment = &timeline.audio_segments(TimeRange::new(frames(0), frames(48)))[0];
        assert_eq!(
            (segment.fade_in, segment.fade_out),
            (frames(12), frames(24))
        );
        assert_eq!(segment.gain_at(0.0), 0.0);
        assert!((segment.gain_at(1.0) - 1.0).abs() < 1e-9);
        assert!(segment.gain_at(1.5) < 1.0);
        // Removing a fade removes its metadata.
        let Composable::Clip(c) = &mut timeline.track_mut(0).unwrap().children[0] else {
            unreachable!()
        };
        c.item.set_fade_in(0.0);
        c.item.set_fade_out(0.0);
        assert!(c.item.metadata.is_empty());
    }

    #[test]
    fn transforms_are_stored_and_left_out_when_neutral() {
        let mut item = Item::new("c");
        assert!(item.transform().is_identity());
        let transform = Transform {
            x: 0.25,
            y: -0.1,
            zoom: 0.5,
            rotation: 15.0,
            crop: [0.1, 0.0, 0.2, 0.05],
        };
        item.set_transform(transform);
        assert_eq!(item.transform(), transform);
        item.set_transform(Transform::IDENTITY);
        assert!(item.metadata.is_empty());
    }

    #[test]
    fn keyframes_animate_layers_and_audio() {
        use crate::Param;
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        // Media frames 24–72 at timeline 0–48.
        timeline.track_mut(0).unwrap().children = vec![clip("v", 24, 48)];
        timeline.track_mut(1).unwrap().children = vec![clip("a", 24, 48)];
        for track in [0, 1] {
            let Composable::Clip(c) = &mut timeline.track_mut(track).unwrap().children[0] else {
                unreachable!()
            };
            // Media seconds 1 → 3: from 0 to 1.
            for param in [Param::Opacity, Param::Volume] {
                c.item.toggle_keyframe(param, 1.0);
                c.item.set_value_at(param, 1.0, 0.0);
                c.item.set_value_at(param, 3.0, 1.0);
            }
        }
        assert_eq!(timeline.video_at(frames(0)).unwrap().opacity, 0.0);
        assert_eq!(timeline.video_at(frames(24)).unwrap().opacity, 0.5);
        // Audio keyframes are in timeline time: media 1 s is timeline 0.
        let segment = &timeline.audio_segments(TimeRange::new(frames(0), frames(48)))[0];
        assert!((segment.gain_at(1.0) - 0.5).abs() < 1e-9);
        assert!((segment.gain_at(1.99) - 1.0).abs() < 0.01);
    }

    #[test]
    fn placement_and_transform_geometry() {
        // A 16:9 picture fitted into a 9:16 sequence: full width, centered.
        let placed = ScalingMode::Fit.place((1920, 1080), (1080, 1920));
        assert_eq!(placed, [0.0, 656.25, 1080.0, 607.5]);
        assert_eq!(
            ScalingMode::Fill.place((1920, 1080), (1080, 1920))[3],
            1920.0
        );
        let sequence = (1080, 1920);
        // Moved right by a tenth, zoomed to half, turned a quarter.
        let t = Transform {
            x: 0.1,
            zoom: 0.5,
            rotation: 90.0,
            ..Transform::IDENTITY
        };
        let (cx, cy) = t.center(placed, sequence);
        assert!((cx - 648.0).abs() < 1e-9 && (cy - 960.0).abs() < 1e-9);
        // The top left corner turns to the top right (clockwise).
        let [top_left, ..] = t.corners(placed, sequence);
        assert!(
            (top_left.0 - (648.0 + 607.5 / 4.0)).abs() < 1e-9,
            "{top_left:?}"
        );
        assert!((top_left.1 - (960.0 - 270.0)).abs() < 1e-9, "{top_left:?}");
    }

    #[test]
    fn soloed_tracks_are_heard_alone() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        let a2 = timeline.add_track(TrackKind::Audio);
        timeline.track_mut(1).unwrap().children = vec![clip("a", 0, 10)];
        timeline.track_mut(a2).unwrap().children = vec![clip("b", 0, 10)];
        let range = TimeRange::new(frames(0), frames(10));
        let heard = |t: &Timeline| -> Vec<_> {
            t.audio_segments(range)
                .into_iter()
                .map(|s| s.path.file_stem().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(heard(&timeline), ["a", "b"]);
        timeline.track_mut(a2).unwrap().item.set_solo(true);
        assert_eq!(heard(&timeline), ["b"]);
        // A muted soloed track doesn't count: the others play.
        timeline.track_mut(a2).unwrap().item.enabled = false;
        assert_eq!(heard(&timeline), ["a"]);
        timeline.track_mut(a2).unwrap().item.enabled = true;
        timeline.track_mut(1).unwrap().item.set_solo(true);
        assert_eq!(heard(&timeline), ["a", "b"]);
    }

    #[test]
    fn in_and_out_marks() {
        let mut settings = Settings::default();
        assert_eq!(settings.marked_range(), None);
        settings.set_mark_in(Some(frames(10)));
        assert_eq!(settings.marked_range(), None);
        settings.set_mark_out(Some(frames(20)));
        assert_eq!(
            settings.marked_range(),
            Some(TimeRange::new(frames(10), frames(10)))
        );
        // A mark on the wrong side of the other one clears it.
        settings.set_mark_in(Some(frames(20)));
        assert_eq!(
            (settings.mark_in, settings.mark_out),
            (Some(frames(20)), None)
        );
        settings.set_mark_out(Some(frames(5)));
        assert_eq!(
            (settings.mark_in, settings.mark_out),
            (None, Some(frames(5)))
        );
    }

    #[test]
    fn links() {
        let mut item = Item::new("x");
        assert_eq!(item.link(), None);
        item.set_link(Some(7));
        assert_eq!(item.link(), Some(7));
        item.set_link(None);
        assert!(item.metadata.is_empty());
    }

    /// An absolute path on this platform (`/x` on Unix, `C:\\x` on Windows).
    fn absolute(unix: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", unix.replace('/', "\\")))
        } else {
            PathBuf::from(unix)
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_file_urls() {
        for (path, url) in [
            (r"C:\My Clips\a.mov", "file:///C:/My%20Clips/a.mov"),
            (r"\\server\share\a.mov", "file://server/share/a.mov"),
        ] {
            assert_eq!(file_url(Path::new(path)), url);
            assert_eq!(url_to_path(url).as_deref(), Some(Path::new(path)));
        }
    }

    #[test]
    fn file_urls() {
        let path = Path::new("/home/me/My Clips/ä#1.mov");
        let url = file_url(path);
        assert_eq!(url, "file:///home/me/My%20Clips/%C3%A4%231.mov");
        assert_eq!(url_to_path(&url).as_deref(), Some(path));
        assert_eq!(
            url_to_path("file://localhost/a/b.mov").as_deref(),
            Some(Path::new("/a/b.mov"))
        );
        assert_eq!(
            url_to_path("relative/b.mov").as_deref(),
            Some(Path::new("relative/b.mov"))
        );
        assert_eq!(url_to_path("https://example.com/x.mov"), None);
    }

    #[test]
    fn relative_media_paths_follow_the_project_folder() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        let range = TimeRange::new(frames(0), frames(10));
        let relative = |url: &str| {
            MediaReference::new(
                ReferenceKind::External {
                    target_url: url.into(),
                },
                Some(range),
            )
        };
        let media = absolute("/media/b.mov");
        let media = media.to_str().unwrap();
        timeline.track_mut(0).unwrap().children = vec![
            Composable::Clip(Clip::new("rel", relative("clips/a.mov"), range)),
            Composable::Clip(Clip::new("abs", relative(media), range)),
        ];
        let paths = |t: &Timeline| -> Vec<_> {
            t.clips()
                .map(|c| t.media_path(c.media_reference().unwrap()).unwrap())
                .collect()
        };
        // Without a folder, paths are taken as they are.
        assert_eq!(
            paths(&timeline),
            [Path::new("clips/a.mov"), Path::new(media)]
        );
        timeline.media_dir = Some(absolute("/projects/x"));
        assert_eq!(
            paths(&timeline),
            [
                absolute("/projects/x").join("clips/a.mov"),
                PathBuf::from(media)
            ]
        );
        // Saving elsewhere writes absolute URLs; absolute ones stay as they are.
        let mut copy = timeline.clone();
        copy.make_media_paths_absolute();
        let urls: Vec<_> = copy
            .clips()
            .map(|c| match &c.media_reference().unwrap().kind {
                ReferenceKind::External { target_url } => target_url.clone(),
                _ => unreachable!(),
            })
            .collect();
        let expected = if cfg!(windows) {
            "file:///C:/projects/x/clips/a.mov"
        } else {
            "file:///projects/x/clips/a.mov"
        };
        assert_eq!(urls, [expected, media]);
        assert_eq!(paths(&copy), paths(&timeline));
    }
}
