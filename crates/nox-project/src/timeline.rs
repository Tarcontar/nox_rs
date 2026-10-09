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
}

/// Nox's own sequence settings, stored under `metadata.nox` in `.otio`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Show drop-frame timecode (only for 29.97 and 59.94 fps).
    pub drop_frame: bool,
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
        }
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
        self.tracks
            .resolve(time, Some(&TrackKind::Video))
            .into_iter()
            .next_back()
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

    /// Clips with the same link id move and trim together (e.g. the video
    /// and audio of one file). Stored as `metadata.nox.link`.
    pub fn link(&self) -> Option<i64> {
        self.metadata.get("nox")?.as_object()?.get("link")?.as_i64()
    }

    pub fn set_link(&mut self, link: Option<i64>) {
        let mut nox = match self.metadata.remove("nox") {
            Some(Value::Object(nox)) => nox,
            _ => Object::new(),
        };
        match link {
            Some(link) => nox.insert("link", link),
            None => {
                nox.remove("link");
            }
        }
        if !nox.is_empty() {
            self.metadata.insert("nox", nox);
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
                vec![ResolvedClip { clip, source_time }]
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
        match self.child_at(time) {
            Some((index, range)) => self.children[index].resolve(time - range.start, kind),
            None => Vec::new(),
        }
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
/// unreserved characters and `/`.
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

/// The local path of a `file:` URL (with or without host) or of a plain
/// path. `None` for other schemes.
pub fn url_to_path(url: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = url.strip_prefix("file://") {
        // Skip an authority such as `localhost`.
        let slash = rest.find('/').unwrap_or(rest.len());
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
        Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }
    #[cfg(not(unix))]
    {
        // Windows: `file:///C:/x` decodes to `/C:/x`.
        let text = String::from_utf8_lossy(&decoded).into_owned();
        let text = match text.as_bytes() {
            [b'/', _, b':', ..] => text[1..].to_owned(),
            _ => text,
        };
        Some(PathBuf::from(text))
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
    fn links() {
        let mut item = Item::new("x");
        assert_eq!(item.link(), None);
        item.set_link(Some(7));
        assert_eq!(item.link(), Some(7));
        item.set_link(None);
        assert!(item.metadata.is_empty());
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
}
