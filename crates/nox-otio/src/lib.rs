//! OpenTimelineIO (`.otio`) reading and writing for `nox_project`
//! timelines.
//!
//! Reading accepts the schema versions found in the wild, including older
//! (`Sequence.1`, `Clip.1`, `Marker.1`) and newer (`Marker.3`) ones.
//! Writing produces the versions of the current OTIO release (0.18):
//! `Timeline.1`, `Stack.1`, `Track.1`, `Clip.2`, `Gap.1`, `Transition.1`,
//! `Marker.2`, the `*Reference.1` types and the effect types.
//!
//! OTIO stores times as floating point `(value, rate)` pairs. Rates are
//! mapped to exact fractions, recognizing NTSC rates (29.97 is read as
//! 30000/1001); non-integral values get a finer timebase so that nothing is
//! rounded. Nox's own settings are stored under `metadata.nox`.
//!
//! Fields a schema doesn't define (from other tools or OTIO versions) are
//! passed through, as OTIO does. Not preserved: the exact color of
//! `Marker.3` markers, which are written as `Marker.2` with the nearest
//! named color.

pub mod json;

use nox_project::{
    Clip, Composable, DEFAULT_MEDIA_KEY, Effect, EffectKind, Gap, Item, Marker, MediaReference,
    Object, Rational, ReferenceKind, Settings, Stack, Time, TimeRange, Timeline, Track, TrackKind,
    Transition, Value,
};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] json::ParseError),
    #[error("{context}: {message}")]
    Schema { context: String, message: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn read_file(path: &Path) -> Result<Timeline> {
    read_str(&std::fs::read_to_string(path)?)
}

/// Parses an `.otio` document. The root may be a timeline, a collection
/// (its first timeline is used) or a single track or stack.
pub fn read_str(text: &str) -> Result<Timeline> {
    let root = json::parse(text)?;
    Reader::default().root(&root)
}

pub fn write_file(path: &Path, timeline: &Timeline) -> Result<()> {
    // Write to a temporary file first so a failed save keeps the old file.
    let temp = path.with_extension("otio.tmp");
    std::fs::write(&temp, write_string(timeline))?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

pub fn write_string(timeline: &Timeline) -> String {
    json::to_string(&write_timeline(timeline))
}

// --- Reading -------------------------------------------------------------

/// Reads objects, tracking where we are for error messages.
#[derive(Default)]
struct Reader {
    context: Vec<String>,
}

/// `Name` and version of an `OTIO_SCHEMA` value like `Clip.2`.
fn schema(object: &Object) -> Option<(&str, u32)> {
    let schema = object.get("OTIO_SCHEMA")?.as_str()?;
    let (name, version) = schema.rsplit_once('.')?;
    Some((name, version.parse().ok()?))
}

impl Reader {
    fn error<T>(&self, message: impl Into<String>) -> Result<T> {
        Err(Error::Schema {
            context: if self.context.is_empty() {
                "document".into()
            } else {
                self.context.join(" > ")
            },
            message: message.into(),
        })
    }

    fn within<T>(&mut self, label: String, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.context.push(label);
        let result = f(self);
        self.context.pop();
        result
    }

    fn object<'a>(&self, value: &'a Value) -> Result<&'a Object> {
        match value.as_object() {
            Some(object) => Ok(object),
            None => self.error("expected an object"),
        }
    }

    fn schema<'a>(&self, object: &'a Object) -> Result<(&'a str, u32)> {
        match schema(object) {
            Some(schema) => Ok(schema),
            None => self.error("missing or invalid OTIO_SCHEMA"),
        }
    }

    fn string(&self, object: &Object, key: &str) -> Result<String> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => self.error(format!("'{key}' must be a string")),
        }
    }

    fn bool(&self, object: &Object, key: &str, default: bool) -> Result<bool> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => self.error(format!("'{key}' must be a boolean")),
        }
    }

    fn metadata(&self, object: &Object) -> Result<Object> {
        match object.get("metadata") {
            None | Some(Value::Null) => Ok(Object::new()),
            Some(Value::Object(o)) => Ok(o.clone()),
            Some(_) => self.error("'metadata' must be an object"),
        }
    }

    fn array<'a>(&self, object: &'a Object, key: &str) -> Result<&'a [Value]> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(&[]),
            Some(Value::Array(a)) => Ok(a),
            Some(_) => self.error(format!("'{key}' must be an array")),
        }
    }

    fn root(&mut self, value: &Value) -> Result<Timeline> {
        let object = self.object(value)?;
        let (name, _) = self.schema(object)?;
        match name {
            "Timeline" => self.timeline(object),
            "SerializableCollection" => {
                for child in self.array(object, "children")? {
                    if let Some(child) = child.as_object()
                        && schema(child).is_some_and(|(n, _)| n == "Timeline")
                    {
                        return self.timeline(child);
                    }
                }
                self.error("the collection contains no timeline")
            }
            "Stack" | "Track" | "Sequence" => {
                let tracks = match self.composable(object)? {
                    Composable::Stack(stack) => stack,
                    track => {
                        let mut stack = Stack::new("tracks");
                        stack.children.push(track);
                        stack
                    }
                };
                Ok(Timeline {
                    name: tracks.item.name.clone(),
                    metadata: Object::new(),
                    global_start_time: None,
                    tracks,
                    settings: Settings::default(),
                    extra: Object::new(),
                })
            }
            other => self.error(format!("cannot open a {other} as a timeline")),
        }
    }

    fn timeline(&mut self, object: &Object) -> Result<Timeline> {
        let name = self.string(object, "name")?;
        self.within(format!("Timeline '{name}'"), |r| {
            let mut metadata = r.metadata(object)?;
            let settings = take_settings(&mut metadata);
            let global_start_time = r.optional_time(object, "global_start_time")?;
            let tracks = match object.get("tracks") {
                None | Some(Value::Null) => Stack::new("tracks"),
                Some(value) => match r.composable(r.object(value)?)? {
                    Composable::Stack(stack) => stack,
                    _ => return r.error("'tracks' must be a Stack"),
                },
            };
            Ok(Timeline {
                name,
                metadata,
                global_start_time,
                tracks,
                settings,
                extra: extra_fields(object, TIMELINE_FIELDS),
            })
        })
    }

    /// The common item fields; `known` lists the type's own fields.
    fn item(&mut self, object: &Object, known: &[&str]) -> Result<Item> {
        Ok(Item {
            name: self.string(object, "name")?,
            metadata: self.metadata(object)?,
            source_range: self.optional_range(object, "source_range")?,
            effects: self
                .array(object, "effects")?
                .iter()
                .map(|e| self.effect(e))
                .collect::<Result<_>>()?,
            markers: self
                .array(object, "markers")?
                .iter()
                .map(|m| self.marker(m))
                .collect::<Result<_>>()?,
            enabled: self.bool(object, "enabled", true)?,
            color: object.get("color").cloned().unwrap_or_default(),
            extra: object
                .iter()
                .filter(|(key, _)| !ITEM_FIELDS.contains(key) && !known.contains(key))
                .map(|(key, value)| (key, value.clone()))
                .collect(),
        })
    }

    fn composable(&mut self, object: &Object) -> Result<Composable> {
        let (schema, version) = self.schema(object)?;
        let name = self.string(object, "name")?;
        self.within(format!("{schema} '{name}'"), |r| {
            Ok(match schema {
                "Clip" => Composable::Clip(r.clip(object, version)?),
                "Gap" => Composable::Gap(Gap {
                    item: r.item(object, &[])?,
                }),
                "Transition" => Composable::Transition(Transition {
                    name,
                    metadata: r.metadata(object)?,
                    transition_type: r.string(object, "transition_type")?,
                    in_offset: r.time_field(object, "in_offset")?,
                    out_offset: r.time_field(object, "out_offset")?,
                    extra: extra_fields(object, TRANSITION_FIELDS),
                }),
                "Track" | "Sequence" => Composable::Track(Track {
                    item: r.item(object, &["children", "kind"])?,
                    kind: TrackKind::parse(&r.string(object, "kind")?),
                    children: r.children(object)?,
                }),
                "Stack" => Composable::Stack(Stack {
                    item: r.item(object, &["children"])?,
                    children: r.children(object)?,
                }),
                other => return r.error(format!("unsupported item type {other}")),
            })
        })
    }

    fn children(&mut self, object: &Object) -> Result<Vec<Composable>> {
        let children = self.array(object, "children")?;
        let mut result = Vec::with_capacity(children.len());
        for (i, child) in children.iter().enumerate() {
            let child = self.within(format!("child {i}"), |r| {
                let object = r.object(child)?;
                r.composable(object)
            })?;
            result.push(child);
        }
        Ok(result)
    }

    fn clip(&mut self, object: &Object, version: u32) -> Result<Clip> {
        let item = self.item(
            object,
            &[
                "media_reference",
                "media_references",
                "active_media_reference_key",
            ],
        )?;
        let (mut media_references, active) = if version >= 2 {
            let references = match object.get("media_references") {
                None | Some(Value::Null) => Vec::new(),
                Some(value) => self
                    .object(value)?
                    .iter()
                    .map(|(key, value)| Ok((key.to_owned(), self.media_reference(value)?)))
                    .collect::<Result<Vec<_>>>()?,
            };
            let active = match object.get("active_media_reference_key") {
                Some(Value::String(key)) => key.clone(),
                _ => DEFAULT_MEDIA_KEY.to_owned(),
            };
            (references, active)
        } else {
            let references = match object.get("media_reference") {
                None | Some(Value::Null) => Vec::new(),
                Some(value) => vec![(DEFAULT_MEDIA_KEY.to_owned(), self.media_reference(value)?)],
            };
            (references, DEFAULT_MEDIA_KEY.to_owned())
        };
        // Like OTIO, give clips without media a missing reference.
        if media_references.is_empty() {
            media_references.push((
                DEFAULT_MEDIA_KEY.to_owned(),
                MediaReference::new(ReferenceKind::Missing, None),
            ));
        }
        Ok(Clip {
            item,
            media_references,
            active_media_reference_key: active,
        })
    }

    fn media_reference(&self, value: &Value) -> Result<MediaReference> {
        let object = self.object(value)?;
        let (schema, version) = self.schema(object)?;
        let (kind, known): (_, &[&str]) = match schema {
            "ExternalReference" => (
                ReferenceKind::External {
                    target_url: self.string(object, "target_url")?,
                },
                &["target_url"],
            ),
            "MissingReference" => (ReferenceKind::Missing, &[]),
            "GeneratorReference" => (
                ReferenceKind::Generator {
                    generator_kind: self.string(object, "generator_kind")?,
                    parameters: match object.get("parameters") {
                        Some(Value::Object(p)) => p.clone(),
                        _ => Object::new(),
                    },
                },
                &["generator_kind", "parameters"],
            ),
            _ => (
                ReferenceKind::Other {
                    schema: format!("{schema}.{version}"),
                },
                &[],
            ),
        };
        Ok(MediaReference {
            name: self.string(object, "name")?,
            metadata: self.metadata(object)?,
            available_range: self.optional_range(object, "available_range")?,
            available_image_bounds: object
                .get("available_image_bounds")
                .cloned()
                .unwrap_or_default(),
            kind,
            extra: extra_fields2(object, REFERENCE_FIELDS, known),
        })
    }

    fn effect(&self, value: &Value) -> Result<Effect> {
        let object = self.object(value)?;
        let (schema, version) = self.schema(object)?;
        let scalar = |default| {
            object
                .get("time_scalar")
                .and_then(Value::as_f64)
                .unwrap_or(default)
        };
        let (kind, known): (_, &[&str]) = match schema {
            "Effect" => (EffectKind::Generic, &[]),
            "LinearTimeWarp" => (
                EffectKind::LinearTimeWarp {
                    time_scalar: scalar(1.0),
                },
                &["time_scalar"],
            ),
            "FreezeFrame" => (EffectKind::FreezeFrame, &["time_scalar"]),
            _ => (
                EffectKind::Other {
                    schema: format!("{schema}.{version}"),
                },
                &[],
            ),
        };
        Ok(Effect {
            name: self.string(object, "name")?,
            metadata: self.metadata(object)?,
            effect_name: self.string(object, "effect_name")?,
            enabled: self.bool(object, "enabled", true)?,
            kind,
            extra: extra_fields2(object, EFFECT_FIELDS, known),
        })
    }

    fn marker(&self, value: &Value) -> Result<Marker> {
        let object = self.object(value)?;
        // Marker.1 called the range `range`.
        let key = if object.contains_key("marked_range") {
            "marked_range"
        } else {
            "range"
        };
        let marked_range = match self.optional_range(object, key)? {
            Some(range) => range,
            None => return self.error("marker without a range"),
        };
        let color = match object.get("color") {
            Some(Value::String(name)) => name.clone(),
            Some(Value::Object(color)) => color_name(color),
            _ => "RED".into(),
        };
        Ok(Marker {
            name: self.string(object, "name")?,
            metadata: self.metadata(object)?,
            color,
            marked_range,
            comment: self.string(object, "comment")?,
            extra: extra_fields(object, MARKER_FIELDS),
        })
    }

    fn time(&self, value: &Value) -> Result<Time> {
        let object = self.object(value)?;
        let number = |key| object.get(key).and_then(Value::as_f64);
        let (Some(value), Some(rate)) = (number("value"), number("rate")) else {
            return self.error("a RationalTime needs a numeric value and rate");
        };
        match time_from_otio(value, rate) {
            Some(time) => Ok(time),
            None => self.error(format!("cannot represent time {value} at rate {rate}")),
        }
    }

    fn time_field(&self, object: &Object, key: &str) -> Result<Time> {
        match object.get(key) {
            Some(value) if !value.is_null() => self.time(value),
            _ => self.error(format!("missing '{key}'")),
        }
    }

    fn optional_time(&self, object: &Object, key: &str) -> Result<Option<Time>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => self.time(value).map(Some),
        }
    }

    fn optional_range(&self, object: &Object, key: &str) -> Result<Option<TimeRange>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                let range = self.object(value)?;
                Ok(Some(TimeRange::new(
                    self.time_field(range, "start_time")?,
                    self.time_field(range, "duration")?,
                )))
            }
        }
    }
}

const REFERENCE_FIELDS: &[&str] = &[
    "OTIO_SCHEMA",
    "name",
    "metadata",
    "available_range",
    "available_image_bounds",
];
const EFFECT_FIELDS: &[&str] = &["OTIO_SCHEMA", "name", "metadata", "effect_name", "enabled"];
const TIMELINE_FIELDS: &[&str] = &[
    "OTIO_SCHEMA",
    "name",
    "metadata",
    "global_start_time",
    "tracks",
];
const ITEM_FIELDS: &[&str] = &[
    "OTIO_SCHEMA",
    "name",
    "metadata",
    "source_range",
    "effects",
    "markers",
    "enabled",
    "color",
];
const TRANSITION_FIELDS: &[&str] = &[
    "OTIO_SCHEMA",
    "name",
    "metadata",
    "transition_type",
    "in_offset",
    "out_offset",
];
const MARKER_FIELDS: &[&str] = &[
    "OTIO_SCHEMA",
    "name",
    "metadata",
    "color",
    "marked_range",
    "range",
    "comment",
];

/// The fields of `object` that are not in `known`.
fn extra_fields(object: &Object, known: &[&str]) -> Object {
    extra_fields2(object, known, &[])
}

/// The fields of `object` in neither `common` nor `specific`.
fn extra_fields2(object: &Object, common: &[&str], specific: &[&str]) -> Object {
    object
        .iter()
        .filter(|(key, _)| !common.contains(key) && !specific.contains(key))
        .map(|(key, value)| (key, value.clone()))
        .collect()
}

/// Appends pass-through fields after the known ones.
fn append(object: &mut Object, extra: &Object) {
    for (key, value) in extra.iter() {
        if !object.contains_key(key) {
            object.insert(key, value.clone());
        }
    }
}

/// Moves Nox's settings out of the timeline metadata.
fn take_settings(metadata: &mut Object) -> Settings {
    let mut settings = Settings::default();
    let Some(Value::Object(mut nox)) = metadata.remove("nox") else {
        return settings;
    };
    if let Some(drop_frame) = nox.remove("drop_frame").and_then(|v| v.as_bool()) {
        settings.drop_frame = drop_frame;
    }
    // Keep anything we don't know, e.g. from newer versions.
    if !nox.is_empty() {
        metadata.insert("nox", nox);
    }
    settings
}

/// OTIO's named marker colors and their RGB values.
const MARKER_COLORS: &[(&str, [f64; 3])] = &[
    ("RED", [1.0, 0.0, 0.0]),
    ("PINK", [1.0, 0.0, 1.0]),
    ("ORANGE", [1.0, 0.5, 0.0]),
    ("YELLOW", [1.0, 1.0, 0.0]),
    ("GREEN", [0.0, 1.0, 0.0]),
    ("CYAN", [0.0, 1.0, 1.0]),
    ("BLUE", [0.0, 0.0, 1.0]),
    ("PURPLE", [0.5, 0.0, 0.5]),
    ("MAGENTA", [1.0, 0.0, 1.0]),
    ("BLACK", [0.0, 0.0, 0.0]),
    ("WHITE", [1.0, 1.0, 1.0]),
];

/// The name of a `Color.1` object, or the nearest named color.
fn color_name(color: &Object) -> String {
    if let Some(name) = color.get("name").and_then(Value::as_str)
        && let Some((known, _)) = MARKER_COLORS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return (*known).to_owned();
    }
    let channel = |key| color.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let rgb = [channel("r"), channel("g"), channel("b")];
    let distance = |c: &[f64; 3]| (0..3).map(|i| (c[i] - rgb[i]).powi(2)).sum::<f64>();
    MARKER_COLORS
        .iter()
        .min_by(|a, b| distance(&a.1).total_cmp(&distance(&b.1)))
        .map_or("RED", |(name, _)| name)
        .to_owned()
}

/// Maps an OTIO frame rate to an exact fraction. NTSC rates are recognized
/// from their usual abbreviations (23.976, 29.97, 59.94, …).
pub fn rate_from_otio(rate: f64) -> Option<Rational> {
    if !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    let whole = rate.round();
    if whole >= 1.0 && (rate - whole).abs() < 1e-9 * rate {
        return Some(Rational::new(whole as i64, 1));
    }
    let base = (rate * 1.001).round();
    if base >= 1.0 && (rate - base * 1000.0 / 1001.0).abs() < 1e-3 {
        return Some(Rational::new(base as i64 * 1000, 1001));
    }
    Rational::approximate(rate, 1_000_000).filter(|r| r.num > 0)
}

/// Maps OTIO's `(value, rate)` to an exact time. Non-integral values are
/// represented with a finer timebase.
pub fn time_from_otio(value: f64, rate: f64) -> Option<Time> {
    let timebase = rate_from_otio(rate)?.inverse();
    if !value.is_finite() {
        return None;
    }
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        return Some(Time::new(value as i64, timebase));
    }
    let fraction = Rational::approximate(value, 1_000_000)?;
    let timebase = Rational::new(timebase.num, timebase.den.checked_mul(fraction.den)?);
    Some(Time::new(fraction.num, timebase))
}

/// The `(value, rate)` OTIO stores for `time`.
pub fn time_to_otio(time: Time) -> (f64, f64) {
    (time.ticks as f64, time.timebase.inverse().as_f64())
}

// --- Writing -------------------------------------------------------------

fn with_schema(schema: &str) -> Object {
    let mut object = Object::new();
    object.insert("OTIO_SCHEMA", schema);
    object
}

fn write_time(time: Time) -> Value {
    let (value, rate) = time_to_otio(time);
    let mut object = with_schema("RationalTime.1");
    object.insert("rate", rate);
    object.insert("value", value);
    object.into()
}

fn write_range(range: Option<TimeRange>) -> Value {
    let Some(range) = range else {
        return Value::Null;
    };
    let mut object = with_schema("TimeRange.1");
    object.insert("duration", write_time(range.duration));
    object.insert("start_time", write_time(range.start));
    object.into()
}

fn write_timeline(timeline: &Timeline) -> Value {
    let mut object = with_schema("Timeline.1");
    let mut metadata = timeline.metadata.clone();
    if timeline.settings != Settings::default() {
        let mut nox = match metadata.remove("nox") {
            Some(Value::Object(nox)) => nox,
            _ => Object::new(),
        };
        nox.insert("drop_frame", timeline.settings.drop_frame);
        metadata.insert("nox", nox);
    }
    object.insert("metadata", metadata);
    object.insert("name", timeline.name.as_str());
    object.insert(
        "global_start_time",
        timeline.global_start_time.map_or(Value::Null, write_time),
    );
    object.insert("tracks", write_stack(&timeline.tracks));
    append(&mut object, &timeline.extra);
    object.into()
}

/// The fields every item has, in OTIO's order.
fn write_item(schema: &str, item: &Item) -> Object {
    let mut object = with_schema(schema);
    object.insert("metadata", item.metadata.clone());
    object.insert("name", item.name.as_str());
    object.insert("source_range", write_range(item.source_range));
    object.insert(
        "effects",
        Value::Array(item.effects.iter().map(write_effect).collect()),
    );
    object.insert(
        "markers",
        Value::Array(item.markers.iter().map(write_marker).collect()),
    );
    object.insert("enabled", item.enabled);
    object.insert("color", item.color.clone());
    object
}

fn write_stack(stack: &Stack) -> Value {
    let mut object = write_item("Stack.1", &stack.item);
    object.insert("children", write_children(&stack.children));
    append(&mut object, &stack.item.extra);
    object.into()
}

fn write_children(children: &[Composable]) -> Value {
    Value::Array(children.iter().map(write_composable).collect())
}

fn write_composable(composable: &Composable) -> Value {
    match composable {
        Composable::Clip(clip) => {
            let mut object = write_item("Clip.2", &clip.item);
            let references: Object = clip
                .media_references
                .iter()
                .map(|(key, reference)| (key.as_str(), write_reference(reference)))
                .collect();
            object.insert("media_references", references);
            object.insert(
                "active_media_reference_key",
                clip.active_media_reference_key.as_str(),
            );
            append(&mut object, &clip.item.extra);
            object.into()
        }
        Composable::Gap(gap) => {
            let mut object = write_item("Gap.1", &gap.item);
            append(&mut object, &gap.item.extra);
            object.into()
        }
        Composable::Transition(t) => {
            let mut object = with_schema("Transition.1");
            object.insert("metadata", t.metadata.clone());
            object.insert("name", t.name.as_str());
            object.insert("in_offset", write_time(t.in_offset));
            object.insert("out_offset", write_time(t.out_offset));
            object.insert("transition_type", t.transition_type.as_str());
            append(&mut object, &t.extra);
            object.into()
        }
        Composable::Track(track) => {
            let mut object = write_item("Track.1", &track.item);
            object.insert("children", write_children(&track.children));
            object.insert("kind", track.kind.as_str());
            append(&mut object, &track.item.extra);
            object.into()
        }
        Composable::Stack(stack) => write_stack(stack),
    }
}

fn write_reference(reference: &MediaReference) -> Value {
    let schema = match &reference.kind {
        ReferenceKind::External { .. } => "ExternalReference.1",
        ReferenceKind::Missing => "MissingReference.1",
        ReferenceKind::Generator { .. } => "GeneratorReference.1",
        ReferenceKind::Other { schema, .. } => schema,
    };
    let mut object = with_schema(schema);
    object.insert("metadata", reference.metadata.clone());
    object.insert("name", reference.name.as_str());
    object.insert("available_range", write_range(reference.available_range));
    object.insert(
        "available_image_bounds",
        reference.available_image_bounds.clone(),
    );
    match &reference.kind {
        ReferenceKind::External { target_url } => object.insert("target_url", target_url.as_str()),
        ReferenceKind::Missing => {}
        ReferenceKind::Generator {
            generator_kind,
            parameters,
        } => {
            object.insert("generator_kind", generator_kind.as_str());
            object.insert("parameters", parameters.clone());
        }
        ReferenceKind::Other { .. } => {}
    }
    append(&mut object, &reference.extra);
    object.into()
}

fn write_effect(effect: &Effect) -> Value {
    let schema = match &effect.kind {
        EffectKind::Generic => "Effect.1",
        EffectKind::LinearTimeWarp { .. } => "LinearTimeWarp.1",
        EffectKind::FreezeFrame => "FreezeFrame.1",
        EffectKind::Other { schema, .. } => schema,
    };
    let mut object = with_schema(schema);
    object.insert("metadata", effect.metadata.clone());
    object.insert("name", effect.name.as_str());
    object.insert("effect_name", effect.effect_name.as_str());
    object.insert("enabled", effect.enabled);
    match &effect.kind {
        EffectKind::Generic => {}
        EffectKind::LinearTimeWarp { time_scalar } => object.insert("time_scalar", *time_scalar),
        EffectKind::FreezeFrame => object.insert("time_scalar", 0.0),
        EffectKind::Other { .. } => {}
    }
    append(&mut object, &effect.extra);
    object.into()
}

fn write_marker(marker: &Marker) -> Value {
    let mut object = with_schema("Marker.2");
    object.insert("metadata", marker.metadata.clone());
    object.insert("name", marker.name.as_str());
    object.insert("color", marker.color.as_str());
    object.insert("marked_range", write_range(Some(marker.marked_range)));
    object.insert("comment", marker.comment.as_str());
    append(&mut object, &marker.extra);
    object.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates() {
        assert_eq!(rate_from_otio(24.0), Some(Rational::new(24, 1)));
        assert_eq!(
            rate_from_otio(30000.0 / 1001.0),
            Some(Rational::new(30000, 1001))
        );
        assert_eq!(rate_from_otio(29.97), Some(Rational::new(30000, 1001)));
        assert_eq!(rate_from_otio(23.976), Some(Rational::new(24000, 1001)));
        assert_eq!(rate_from_otio(59.94), Some(Rational::new(60000, 1001)));
        assert_eq!(rate_from_otio(48000.0), Some(Rational::new(48000, 1)));
        assert_eq!(rate_from_otio(12.5), Some(Rational::new(25, 2)));
        assert_eq!(rate_from_otio(0.0), None);
        assert_eq!(rate_from_otio(f64::NAN), None);
    }

    #[test]
    fn times() {
        let t = time_from_otio(86400.0, 24.0).unwrap();
        assert_eq!((t.ticks, t.timebase), (86400, Rational::new(1, 24)));
        let half = time_from_otio(1.5, 24.0).unwrap();
        assert_eq!(half, Time::new(3, Rational::new(1, 48)));
        assert_eq!(time_to_otio(half), (3.0, 48.0));
        let ntsc = Time::new(5, Rational::new(1001, 30000));
        assert_eq!(time_to_otio(ntsc), (5.0, 30000.0 / 1001.0));
        assert_eq!(time_from_otio(f64::INFINITY, 24.0), None);
    }

    #[test]
    fn marker_colors() {
        let color = |r: f64, g: f64, b: f64, name: &str| {
            [("r", r), ("g", g), ("b", b)]
                .into_iter()
                .map(|(k, v)| (k, Value::Float(v)))
                .chain([("name", Value::from(name))])
                .collect::<Object>()
        };
        assert_eq!(color_name(&color(1.0, 0.0, 0.0, "Green")), "GREEN");
        assert_eq!(color_name(&color(0.1, 0.9, 0.1, "")), "GREEN");
    }

    #[test]
    fn settings_round_trip() {
        let mut timeline = Timeline::new("t", Rational::new(30000, 1001));
        timeline.settings.drop_frame = true;
        timeline.metadata.insert("other", 1);
        let text = write_string(&timeline);
        assert!(text.contains("\"drop_frame\": true"));
        assert_eq!(read_str(&text).unwrap(), timeline);
    }

    #[test]
    fn errors_name_the_item() {
        let text = r#"{"OTIO_SCHEMA": "Timeline.1", "name": "T", "tracks": {
            "OTIO_SCHEMA": "Stack.1", "children": [
                {"OTIO_SCHEMA": "Track.1", "name": "V1", "children": [
                    {"OTIO_SCHEMA": "Clip.2", "name": "c",
                     "source_range": {"OTIO_SCHEMA": "TimeRange.1",
                        "start_time": {"OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 0},
                        "duration": {"OTIO_SCHEMA": "RationalTime.1", "value": 1, "rate": 24}}}
                ]}
            ]}}"#;
        let err = read_str(text).unwrap_err().to_string();
        assert_eq!(
            err,
            "Timeline 'T' > Stack '' > child 0 > Track 'V1' > child 0 > Clip 'c': cannot represent time 0 at rate 0"
        );
    }
}
