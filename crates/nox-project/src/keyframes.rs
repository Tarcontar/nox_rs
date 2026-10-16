//! Keyframes: clip properties that change over time (opacity, volume and
//! the transform), stored per property under `metadata.nox.keyframes`;
//! effects keep their parameters' the same way (`effects.rs`).
//!
//! Keyframe times are seconds in the clip's media (the time space of its
//! `source_range`), as in Resolve, so keyframes stay on the same picture
//! when the clip is trimmed or split. Between two keyframes the value moves
//! linearly, or eased (slow out of the first and into the second, a
//! smoothstep) if the first one says so. Before the first and after the
//! last keyframe it holds.

use crate::metadata::{Object, Value};
use crate::timeline::{Item, Transform, grade_from_value, grade_to_value};
use nox_color::{Grade, Wheel};

/// The properties that can be keyframed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Param {
    Opacity,
    Volume,
    X,
    Y,
    Zoom,
    Rotation,
    CropLeft,
    CropTop,
    CropRight,
    CropBottom,
}

impl Param {
    pub const ALL: [Param; 10] = [
        Self::Opacity,
        Self::Volume,
        Self::X,
        Self::Y,
        Self::Zoom,
        Self::Rotation,
        Self::CropLeft,
        Self::CropTop,
        Self::CropRight,
        Self::CropBottom,
    ];
    /// The transform's parameters, in `Transform` field order.
    pub const TRANSFORM: [Param; 8] = [
        Self::X,
        Self::Y,
        Self::Zoom,
        Self::Rotation,
        Self::CropLeft,
        Self::CropTop,
        Self::CropRight,
        Self::CropBottom,
    ];

    /// The key under `metadata.nox.keyframes`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Opacity => "opacity",
            Self::Volume => "volume",
            Self::X => "x",
            Self::Y => "y",
            Self::Zoom => "zoom",
            Self::Rotation => "rotation",
            Self::CropLeft => "crop_left",
            Self::CropTop => "crop_top",
            Self::CropRight => "crop_right",
            Self::CropBottom => "crop_bottom",
        }
    }

    /// The parameter's value in `transform` (for transform parameters).
    pub fn of(self, transform: &Transform) -> f64 {
        match self {
            Self::X => transform.x,
            Self::Y => transform.y,
            Self::Zoom => transform.zoom,
            Self::Rotation => transform.rotation,
            Self::CropLeft => transform.crop[0],
            Self::CropTop => transform.crop[1],
            Self::CropRight => transform.crop[2],
            Self::CropBottom => transform.crop[3],
            Self::Opacity | Self::Volume => 0.0,
        }
    }

    fn set(self, transform: &mut Transform, value: f64) {
        match self {
            Self::X => transform.x = value,
            Self::Y => transform.y = value,
            Self::Zoom => transform.zoom = value.max(0.0),
            Self::Rotation => transform.rotation = value,
            Self::CropLeft => transform.crop[0] = value.clamp(0.0, 1.0),
            Self::CropTop => transform.crop[1] = value.clamp(0.0, 1.0),
            Self::CropRight => transform.crop[2] = value.clamp(0.0, 1.0),
            Self::CropBottom => transform.crop[3] = value.clamp(0.0, 1.0),
            Self::Opacity | Self::Volume => {}
        }
    }
}

/// How the value moves from a keyframe to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ease {
    #[default]
    Linear,
    /// Slow out of this keyframe and into the next (smoothstep).
    Smooth,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    /// Seconds in the clip's media.
    pub time: f64,
    pub value: f64,
    pub ease: Ease,
}

/// A keyframe of a clip's whole grade (`metadata.nox.keyframes.grade`):
/// all four wheels move together, as Resolve's dynamic keyframes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeKeyframe {
    /// Seconds in the clip's media.
    pub time: f64,
    pub grade: Grade,
    pub ease: Ease,
}

/// The grade of `keys` (sorted, not empty) at `time`, each value
/// interpolated as `interpolate` does.
pub fn interpolate_grade(keys: &[GradeKeyframe], time: f64) -> Grade {
    let next = keys.partition_point(|k| k.time <= time);
    match (next.checked_sub(1).map(|i| keys[i]), keys.get(next)) {
        (None, Some(first)) => first.grade,
        (Some(last), None) => last.grade,
        (Some(a), Some(b)) => {
            let t = ((time - a.time) / (b.time - a.time)).clamp(0.0, 1.0);
            let t = match a.ease {
                Ease::Linear => t,
                Ease::Smooth => t * t * (3.0 - 2.0 * t),
            } as f32;
            let mix = |x: f32, y: f32| x + (y - x) * t;
            let wheel = |x: Wheel, y: Wheel| Wheel {
                rgb: [0, 1, 2].map(|c| mix(x.rgb[c], y.rgb[c])),
                master: mix(x.master, y.master),
            };
            Grade {
                lift: wheel(a.grade.lift, b.grade.lift),
                gamma: wheel(a.grade.gamma, b.grade.gamma),
                gain: wheel(a.grade.gain, b.grade.gain),
                offset: wheel(a.grade.offset, b.grade.offset),
            }
        }
        (None, None) => Grade::NEUTRAL,
    }
}

/// Keyframes closer than this (seconds) are the same keyframe.
pub const SAME_TIME: f64 = 1e-4;

/// The value of `keys` (sorted by time, not empty) at `time`.
pub fn interpolate(keys: &[Keyframe], time: f64) -> f64 {
    let next = keys.partition_point(|k| k.time <= time);
    match (next.checked_sub(1).map(|i| keys[i]), keys.get(next)) {
        (None, Some(first)) => first.value,
        (Some(last), None) => last.value,
        (Some(a), Some(b)) => {
            let t = ((time - a.time) / (b.time - a.time)).clamp(0.0, 1.0);
            let t = match a.ease {
                Ease::Linear => t,
                Ease::Smooth => t * t * (3.0 - 2.0 * t),
            };
            a.value + (b.value - a.value) * t
        }
        (None, None) => 0.0,
    }
}

/// Keyframes from their stored form (`[{t, v, ease?}]`), sorted; broken
/// ones are left out.
pub(crate) fn keyframes_from(list: &Value) -> Vec<Keyframe> {
    let mut keys: Vec<Keyframe> = list
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter_map(|k| {
            let k = k.as_object()?;
            Some(Keyframe {
                time: k.get("t")?.as_f64()?,
                value: k.get("v")?.as_f64()?,
                ease: match k.get("ease").and_then(Value::as_str) {
                    Some("smooth") => Ease::Smooth,
                    _ => Ease::Linear,
                },
            })
        })
        .collect();
    keys.sort_by(|a, b| a.time.total_cmp(&b.time));
    keys
}

/// The stored form of `keys`, sorted and one per time (none if empty).
pub(crate) fn keyframes_value(mut keys: Vec<Keyframe>) -> Option<Value> {
    keys.sort_by(|a, b| a.time.total_cmp(&b.time));
    keys.dedup_by(|b, a| (a.time - b.time).abs() < SAME_TIME);
    if keys.is_empty() {
        return None;
    }
    let list = keys
        .iter()
        .map(|k| {
            let mut object = Object::new();
            object.insert("t", Value::Float(k.time));
            object.insert("v", Value::Float(k.value));
            if k.ease == Ease::Smooth {
                object.insert("ease", "smooth");
            }
            Value::Object(object)
        })
        .collect();
    Some(Value::Array(list))
}

impl Item {
    /// The keyframes of `param`, sorted by time (empty if it is constant).
    pub fn keyframes(&self, param: Param) -> Vec<Keyframe> {
        self.nox_value("keyframes")
            .and_then(Value::as_object)
            .and_then(|k| k.get(param.key()))
            .map(keyframes_from)
            .unwrap_or_default()
    }

    /// Replaces the keyframes of `param` (none: the property is constant
    /// again).
    pub fn set_keyframes(&mut self, param: Param, keys: Vec<Keyframe>) {
        let mut all = match self.nox_value("keyframes") {
            Some(Value::Object(all)) => all.clone(),
            _ => Object::new(),
        };
        match keyframes_value(keys) {
            Some(list) => all.insert(param.key(), list),
            None => {
                all.remove(param.key());
            }
        }
        self.set_nox_value("keyframes", (!all.is_empty()).then_some(Value::Object(all)));
    }

    /// Whether `param` has keyframes.
    pub fn is_animated(&self, param: Param) -> bool {
        !self.keyframes(param).is_empty()
    }

    /// Sets `param` to `value` at media time `time`: as a keyframe there if
    /// the property is keyframed, else as its constant value.
    pub fn set_value_at(&mut self, param: Param, time: f64, value: f64) {
        let mut keys = self.keyframes(param);
        if keys.is_empty() {
            self.set_constant(param, value);
            return;
        }
        match keys.iter_mut().find(|k| (k.time - time).abs() < SAME_TIME) {
            Some(key) => key.value = value,
            None => keys.push(Keyframe {
                time,
                value,
                ease: Ease::Linear,
            }),
        }
        self.set_keyframes(param, keys);
    }

    /// Adds a keyframe for `param` at `time` with its current value there,
    /// or removes the keyframe at `time`.
    pub fn toggle_keyframe(&mut self, param: Param, time: f64) {
        let mut keys = self.keyframes(param);
        if let Some(i) = keys.iter().position(|k| (k.time - time).abs() < SAME_TIME) {
            let removed = keys.remove(i);
            if keys.is_empty() {
                // The property keeps the last keyframe's value.
                self.set_constant(param, removed.value);
            }
        } else {
            keys.push(Keyframe {
                time,
                value: self.value_at(param, time),
                ease: Ease::Linear,
            });
        }
        self.set_keyframes(param, keys);
    }

    /// Whether `param` has a keyframe at `time`.
    pub fn has_keyframe(&self, param: Param, time: f64) -> bool {
        self.keyframes(param)
            .iter()
            .any(|k| (k.time - time).abs() < SAME_TIME)
    }

    /// Sets the easing of every keyframe at `time` (effects' too).
    pub fn set_ease_at(&mut self, time: f64, ease: Ease) {
        for effect in &mut self.effects {
            effect.update_keyframes(|keys| {
                let mut changed = false;
                for key in keys
                    .iter_mut()
                    .filter(|k| (k.time - time).abs() < SAME_TIME)
                {
                    key.ease = ease;
                    changed = true;
                }
                changed
            });
        }
        for param in Param::ALL {
            let mut keys = self.keyframes(param);
            let mut changed = false;
            for key in keys
                .iter_mut()
                .filter(|k| (k.time - time).abs() < SAME_TIME)
            {
                key.ease = ease;
                changed = true;
            }
            if changed {
                self.set_keyframes(param, keys);
            }
        }
        let mut grades = self.grade_keyframes();
        if grades.iter().any(|k| (k.time - time).abs() < SAME_TIME) {
            for key in grades
                .iter_mut()
                .filter(|k| (k.time - time).abs() < SAME_TIME)
            {
                key.ease = ease;
            }
            self.set_grade_keyframes(grades);
        }
    }

    /// The grade keyframes, sorted by time (empty if the grade is
    /// constant).
    pub fn grade_keyframes(&self) -> Vec<GradeKeyframe> {
        let Some(list) = self
            .nox_value("keyframes")
            .and_then(Value::as_object)
            .and_then(|k| k.get("grade"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut keys: Vec<GradeKeyframe> = list
            .iter()
            .filter_map(|k| {
                let k = k.as_object()?;
                Some(GradeKeyframe {
                    time: k.get("t")?.as_f64()?,
                    grade: grade_from_value(k.get("grade")?),
                    ease: match k.get("ease").and_then(Value::as_str) {
                        Some("smooth") => Ease::Smooth,
                        _ => Ease::Linear,
                    },
                })
            })
            .collect();
        keys.sort_by(|a, b| a.time.total_cmp(&b.time));
        keys
    }

    pub fn set_grade_keyframes(&mut self, mut keys: Vec<GradeKeyframe>) {
        keys.sort_by(|a, b| a.time.total_cmp(&b.time));
        keys.dedup_by(|b, a| (a.time - b.time).abs() < SAME_TIME);
        let mut all = match self.nox_value("keyframes") {
            Some(Value::Object(all)) => all.clone(),
            _ => Object::new(),
        };
        if keys.is_empty() {
            all.remove("grade");
        } else {
            let list = keys
                .iter()
                .map(|k| {
                    let mut object = Object::new();
                    object.insert("t", Value::Float(k.time));
                    object.insert("grade", grade_to_value(&k.grade));
                    if k.ease == Ease::Smooth {
                        object.insert("ease", "smooth");
                    }
                    Value::Object(object)
                })
                .collect();
            all.insert("grade", Value::Array(list));
        }
        self.set_nox_value("keyframes", (!all.is_empty()).then_some(Value::Object(all)));
    }

    /// The grade at media time `time`.
    pub fn grade_at(&self, time: f64) -> Grade {
        let keys = self.grade_keyframes();
        if keys.is_empty() {
            self.grade()
        } else {
            interpolate_grade(&keys, time)
        }
    }

    /// Sets the grade at `time`: as a keyframe there if the grade is
    /// keyframed, else as the clip's constant grade.
    pub fn set_grade_at(&mut self, time: f64, grade: Grade) {
        let mut keys = self.grade_keyframes();
        if keys.is_empty() {
            self.set_grade(grade);
            return;
        }
        match keys.iter_mut().find(|k| (k.time - time).abs() < SAME_TIME) {
            Some(key) => key.grade = grade,
            None => keys.push(GradeKeyframe {
                time,
                grade,
                ease: Ease::Linear,
            }),
        }
        self.set_grade_keyframes(keys);
    }

    /// Adds a grade keyframe at `time` with the grade there, or removes
    /// the one at `time` (the last one's grade stays as the constant grade).
    pub fn toggle_grade_keyframe(&mut self, time: f64) {
        let mut keys = self.grade_keyframes();
        if let Some(i) = keys.iter().position(|k| (k.time - time).abs() < SAME_TIME) {
            let removed = keys.remove(i);
            if keys.is_empty() {
                self.set_grade(removed.grade);
            }
        } else {
            keys.push(GradeKeyframe {
                time,
                grade: self.grade_at(time),
                ease: Ease::Linear,
            });
        }
        self.set_grade_keyframes(keys);
    }

    pub fn has_grade_keyframe(&self, time: f64) -> bool {
        self.grade_keyframes()
            .iter()
            .any(|k| (k.time - time).abs() < SAME_TIME)
    }

    /// Moves the keyframes at `from` (of every property, effects' too) to
    /// `to`.
    pub fn move_keyframes(&mut self, from: f64, to: f64) {
        for effect in &mut self.effects {
            effect.update_keyframes(|keys| {
                let mut moved = false;
                for key in keys
                    .iter_mut()
                    .filter(|k| (k.time - from).abs() < SAME_TIME)
                {
                    key.time = to;
                    moved = true;
                }
                moved
            });
        }
        let mut grades = self.grade_keyframes();
        if grades.iter().any(|k| (k.time - from).abs() < SAME_TIME) {
            for key in grades
                .iter_mut()
                .filter(|k| (k.time - from).abs() < SAME_TIME)
            {
                key.time = to;
            }
            self.set_grade_keyframes(grades);
        }
        for param in Param::ALL {
            let mut keys = self.keyframes(param);
            let mut moved = false;
            for key in keys
                .iter_mut()
                .filter(|k| (k.time - from).abs() < SAME_TIME)
            {
                key.time = to;
                moved = true;
            }
            if moved {
                self.set_keyframes(param, keys);
            }
        }
    }

    /// The media times with a keyframe of any property (effects' too),
    /// sorted.
    pub fn keyframe_times(&self) -> Vec<f64> {
        let effects = self.effects.iter().flat_map(|e| {
            e.animated_params()
                .into_iter()
                .flat_map(|name| e.keyframes(&name).into_iter().map(|k| k.time))
        });
        let mut times: Vec<f64> = Param::ALL
            .into_iter()
            .flat_map(|p| self.keyframes(p).into_iter().map(|k| k.time))
            .chain(self.grade_keyframes().into_iter().map(|k| k.time))
            .chain(effects)
            .collect();
        times.sort_by(f64::total_cmp);
        times.dedup_by(|b, a| (*a - *b).abs() < SAME_TIME);
        times
    }

    /// `param` at media time `time`.
    pub fn value_at(&self, param: Param, time: f64) -> f64 {
        let keys = self.keyframes(param);
        if !keys.is_empty() {
            return interpolate(&keys, time);
        }
        match param {
            Param::Opacity => self.opacity(),
            Param::Volume => self.volume(),
            _ => param.of(&self.transform()),
        }
    }

    /// The transform at media time `time`.
    pub fn transform_at(&self, time: f64) -> Transform {
        let mut transform = self.transform();
        for param in Param::TRANSFORM {
            let keys = self.keyframes(param);
            if !keys.is_empty() {
                param.set(&mut transform, interpolate(&keys, time));
            }
        }
        transform
    }

    fn set_constant(&mut self, param: Param, value: f64) {
        match param {
            Param::Opacity => self.set_opacity(value),
            Param::Volume => self.set_volume(value),
            _ => {
                let mut transform = self.transform();
                param.set(&mut transform, value);
                self.set_transform(transform);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(time: f64, value: f64, ease: Ease) -> Keyframe {
        Keyframe { time, value, ease }
    }

    #[test]
    fn values_hold_and_interpolate() {
        let keys = [
            key(1.0, 0.0, Ease::Linear),
            key(3.0, 1.0, Ease::Smooth),
            key(5.0, 0.0, Ease::Linear),
        ];
        assert_eq!(interpolate(&keys, 0.0), 0.0);
        assert_eq!(interpolate(&keys, 2.0), 0.5);
        assert_eq!(interpolate(&keys, 3.0), 1.0);
        // Eased: the same halfway, slower near the keyframes.
        assert_eq!(interpolate(&keys, 4.0), 0.5);
        assert!(interpolate(&keys, 3.2) > 0.9, "slower than linear (0.9)");
        assert_eq!(interpolate(&keys, 9.0), 0.0);
    }

    #[test]
    fn editing_keyframes() {
        let mut item = Item::new("c");
        item.set_opacity(0.8);
        // Constant until keyframed.
        item.set_value_at(Param::Opacity, 1.0, 0.6);
        assert_eq!(
            (item.opacity(), item.is_animated(Param::Opacity)),
            (0.6, false)
        );
        // A keyframe takes the current value; then edits elsewhere add more.
        item.toggle_keyframe(Param::Opacity, 1.0);
        item.set_value_at(Param::Opacity, 3.0, 0.0);
        assert_eq!(item.keyframes(Param::Opacity).len(), 2);
        assert_eq!(item.value_at(Param::Opacity, 2.0), 0.3);
        assert!(item.has_keyframe(Param::Opacity, 3.0));
        // Transform parameters animate one by one.
        item.toggle_keyframe(Param::Zoom, 1.0);
        item.set_value_at(Param::Zoom, 2.0, 2.0);
        assert_eq!(item.transform_at(1.5).zoom, 1.5);
        assert_eq!(item.transform_at(1.5).x, 0.0);
        assert_eq!(item.keyframe_times(), [1.0, 2.0, 3.0]);
        item.set_ease_at(1.0, Ease::Smooth);
        assert_eq!(item.keyframes(Param::Zoom)[0].ease, Ease::Smooth);
        // Removing the last keyframe keeps its value.
        item.toggle_keyframe(Param::Opacity, 1.0);
        item.toggle_keyframe(Param::Opacity, 3.0);
        assert!(!item.is_animated(Param::Opacity));
        assert_eq!(item.opacity(), 0.0);
        item.toggle_keyframe(Param::Zoom, 1.0);
        item.toggle_keyframe(Param::Zoom, 2.0);
        assert_eq!(item.transform().zoom, 2.0);
        // Moving keyframes moves those of every property at that time.
        item.toggle_keyframe(Param::X, 2.0);
        item.toggle_keyframe(Param::Y, 2.0);
        item.move_keyframes(2.0, 2.5);
        assert!(item.has_keyframe(Param::X, 2.5) && item.has_keyframe(Param::Y, 2.5));
        assert!(!item.has_keyframe(Param::X, 2.0));
        item.toggle_keyframe(Param::X, 2.5);
        item.toggle_keyframe(Param::Y, 2.5);
        // The grade as a whole: halfway between two keyframes, halfway
        // between their values.
        let warm = Grade {
            gain: Wheel {
                rgb: [1.2, 1.0, 1.0],
                master: 1.0,
            },
            ..Grade::NEUTRAL
        };
        item.toggle_grade_keyframe(1.0);
        item.set_grade_at(3.0, warm);
        assert_eq!(item.grade_at(2.0).gain.rgb[0], 1.1);
        assert_eq!(item.grade_at(0.0), Grade::NEUTRAL);
        assert_eq!(item.keyframe_times(), [1.0, 3.0]);
        item.move_keyframes(3.0, 4.0);
        assert!(item.has_grade_keyframe(4.0));
        item.toggle_grade_keyframe(1.0);
        item.toggle_grade_keyframe(4.0);
        assert_eq!(item.grade(), warm);
        item.set_grade(Grade::NEUTRAL);
        // Everything gone: no keyframes metadata left.
        assert!(item.nox_value("keyframes").is_none());
    }
}
