//! Rendering effects on items (clips, tracks, the timeline's stack): an
//! ordered stack of OTIO `Effect.1`s, each `effect_name` naming what
//! renders it (nox-fx's registry) and its parameters in
//! `metadata.nox.params`, keyframed ones in `metadata.nox.keyframes`
//! (media seconds, like the clip's own keyframes). This crate stores
//! values only; what parameters an effect has, and their defaults, are
//! the registry's.
//!
//! Each effect has a `uid` (`metadata.nox.uid`), unique within its item,
//! that stays when effects are reordered: the identity of its controls
//! and of whatever the renderer keeps for it. Effects without one (older
//! projects, other tools) are known by their position (`#2`) until an
//! edit gives them one.
//!
//! The effects form a graph: an effect's inputs may be connected to
//! outputs of effects before it (`metadata.nox.inputs`), or to the clip's
//! own picture (`SOURCE`). The list order is the order they're evaluated
//! in, so connections only point backwards and there are no cycles;
//! `Item::connect` reorders the list when a connection needs it. Inputs
//! without a connection are implicit: nox-fx's registry knows which
//! (a picture input takes the picture before it, as in a plain stack).

use crate::keyframes::{Ease, Keyframe, SAME_TIME, interpolate, keyframes_from, keyframes_value};
use crate::metadata::{Object, Value};
use crate::timeline::{Effect, EffectKind, Item};

/// Where an input comes from: output `port` of the effect with uid `from`,
/// or the clip's picture (`from` is `SOURCE`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Connection {
    pub from: String,
    pub port: String,
}

/// The `from` of connections to the clip's own picture.
pub const SOURCE: &str = "source";

impl Connection {
    pub fn new(from: &str, port: &str) -> Self {
        Self {
            from: from.into(),
            port: port.into(),
        }
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self::new(
            object.get("from")?.as_str()?,
            object.get("port")?.as_str()?,
        ))
    }

    fn to_value(&self) -> Value {
        let mut object = Object::new();
        object.insert("from", self.from.as_str());
        object.insert("port", self.port.as_str());
        Value::Object(object)
    }
}

/// `Item::connect` refused: the connection would make a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle;

impl Effect {
    /// A rendering effect named `effect_name` (e.g. `nox.gaussian_blur`)
    /// with default parameters; `name` is what other tools show.
    pub fn rendering(effect_name: &str, name: &str) -> Self {
        Self {
            name: name.into(),
            metadata: Object::new(),
            effect_name: effect_name.into(),
            enabled: true,
            kind: EffectKind::Generic,
            extra: Object::new(),
        }
    }

    /// Whether it renders (an `Effect.1`), rather than changing time.
    pub fn is_rendering(&self) -> bool {
        self.kind == EffectKind::Generic
    }

    /// `metadata.nox.<key>`.
    pub fn nox_value(&self, key: &str) -> Option<&Value> {
        crate::metadata::nox_value(&self.metadata, key)
    }

    /// Sets or (with `None`) removes `metadata.nox.<key>`.
    pub fn set_nox_value(&mut self, key: &str, value: Option<Value>) {
        crate::metadata::set_nox_value(&mut self.metadata, key, value);
    }

    pub fn uid(&self) -> Option<&str> {
        self.nox_value("uid").and_then(Value::as_str)
    }

    /// The stored value of parameter `name` (none: the default).
    pub fn param(&self, name: &str) -> Option<&Value> {
        self.nox_value("params")?.as_object()?.get(name)
    }

    /// Stores parameter `name`; `None` goes back to the default.
    pub fn set_param(&mut self, name: &str, value: Option<Value>) {
        let mut params = match self.nox_value("params") {
            Some(Value::Object(params)) => params.clone(),
            _ => Object::new(),
        };
        match value {
            Some(value) => params.insert(name, value),
            None => {
                params.remove(name);
            }
        }
        self.set_nox_value(
            "params",
            (!params.is_empty()).then_some(Value::Object(params)),
        );
    }

    /// The keyframes of number parameter `name`, sorted (empty if it is
    /// constant).
    pub fn keyframes(&self, name: &str) -> Vec<Keyframe> {
        self.nox_value("keyframes")
            .and_then(Value::as_object)
            .and_then(|k| k.get(name))
            .map(keyframes_from)
            .unwrap_or_default()
    }

    /// Replaces the keyframes of `name` (none: constant again).
    pub fn set_keyframes(&mut self, name: &str, keys: Vec<Keyframe>) {
        let mut all = match self.nox_value("keyframes") {
            Some(Value::Object(all)) => all.clone(),
            _ => Object::new(),
        };
        match keyframes_value(keys) {
            Some(list) => all.insert(name, list),
            None => {
                all.remove(name);
            }
        }
        self.set_nox_value("keyframes", (!all.is_empty()).then_some(Value::Object(all)));
    }

    /// Names of the parameters with keyframes.
    pub fn animated_params(&self) -> Vec<String> {
        self.nox_value("keyframes")
            .and_then(Value::as_object)
            .map(|k| k.iter().map(|(name, _)| name.to_owned()).collect())
            .unwrap_or_default()
    }

    /// Number parameter `name` at media time `time`: interpolated if it
    /// is keyframed, else its stored value, else `default`.
    pub fn number_at(&self, name: &str, time: f64, default: f64) -> f64 {
        let keys = self.keyframes(name);
        if !keys.is_empty() {
            return interpolate(&keys, time);
        }
        self.param(name).and_then(Value::as_f64).unwrap_or(default)
    }

    /// Sets number parameter `name` at `time`: as a keyframe there if it
    /// is keyframed, else as its constant value.
    pub fn set_number_at(&mut self, name: &str, time: f64, value: f64) {
        let mut keys = self.keyframes(name);
        if keys.is_empty() {
            self.set_param(name, Some(Value::Float(value)));
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
        self.set_keyframes(name, keys);
    }

    /// Adds a keyframe for `name` at `time` with its value there, or
    /// removes the one at `time` (the last one's value stays as the
    /// constant value).
    pub fn toggle_keyframe(&mut self, name: &str, time: f64, default: f64) {
        let mut keys = self.keyframes(name);
        if let Some(i) = keys.iter().position(|k| (k.time - time).abs() < SAME_TIME) {
            let removed = keys.remove(i);
            if keys.is_empty() {
                self.set_param(name, Some(Value::Float(removed.value)));
            }
        } else {
            keys.push(Keyframe {
                time,
                value: self.number_at(name, time, default),
                ease: Ease::Linear,
            });
        }
        self.set_keyframes(name, keys);
    }

    pub fn has_keyframe(&self, name: &str, time: f64) -> bool {
        self.keyframes(name)
            .iter()
            .any(|k| (k.time - time).abs() < SAME_TIME)
    }

    /// Where input `name` is connected to (none: implicit).
    pub fn input(&self, name: &str) -> Option<Connection> {
        Connection::from_value(self.nox_value("inputs")?.as_object()?.get(name)?)
    }

    /// The connected inputs, by name.
    pub fn inputs(&self) -> Vec<(String, Connection)> {
        self.nox_value("inputs")
            .and_then(Value::as_object)
            .map(|inputs| {
                inputs
                    .iter()
                    .filter_map(|(name, v)| Some((name.to_owned(), Connection::from_value(v)?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Connects input `name` (`None`: implicit again). Prefer
    /// `Item::connect`, which keeps the evaluation order valid.
    pub fn set_input(&mut self, name: &str, connection: Option<Connection>) {
        let mut inputs = match self.nox_value("inputs") {
            Some(Value::Object(inputs)) => inputs.clone(),
            _ => Object::new(),
        };
        match connection {
            Some(connection) => inputs.insert(name, connection.to_value()),
            None => {
                inputs.remove(name);
            }
        }
        self.set_nox_value(
            "inputs",
            (!inputs.is_empty()).then_some(Value::Object(inputs)),
        );
    }

    /// Where the node editor shows it (canvas units), if placed.
    pub fn node_position(&self) -> Option<(f64, f64)> {
        let node = self.nox_value("node")?.as_object()?;
        Some((node.get("x")?.as_f64()?, node.get("y")?.as_f64()?))
    }

    pub fn set_node_position(&mut self, position: Option<(f64, f64)>) {
        let value = position.map(|(x, y)| {
            let mut node = Object::new();
            node.insert("x", Value::Float(x));
            node.insert("y", Value::Float(y));
            Value::Object(node)
        });
        self.set_nox_value("node", value);
    }

    /// Applies `f` to each parameter's keyframes, storing those it
    /// changed (`f` returns whether it did).
    pub(crate) fn update_keyframes(&mut self, mut f: impl FnMut(&mut Vec<Keyframe>) -> bool) {
        for name in self.animated_params() {
            let mut keys = self.keyframes(&name);
            if f(&mut keys) {
                self.set_keyframes(&name, keys);
            }
        }
    }
}

impl Item {
    /// The rendering effects, in order, with their keys (`uid`, or `#n`
    /// by position for those without one).
    pub fn rendering_effects(&self) -> impl Iterator<Item = (String, &Effect)> {
        self.effects
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_rendering())
            .map(|(i, e)| (effect_key(i, e), e))
    }

    /// The effect with key `key` (`rendering_effects`).
    pub fn effect(&self, key: &str) -> Option<&Effect> {
        self.effect_index(key).map(|i| &self.effects[i])
    }

    pub fn effect_mut(&mut self, key: &str) -> Option<&mut Effect> {
        self.effect_index(key).map(|i| &mut self.effects[i])
    }

    fn effect_index(&self, key: &str) -> Option<usize> {
        self.effects
            .iter()
            .enumerate()
            .position(|(i, e)| effect_key(i, e) == key)
    }

    /// Adds `effect` at the end of the stack with a new uid, which it
    /// returns.
    pub fn add_effect(&mut self, mut effect: Effect) -> String {
        let uid = self.next_uid();
        effect.set_nox_value("uid", Some(Value::String(uid.clone())));
        self.effects.push(effect);
        uid
    }

    /// A uid no effect of the item has: one more than the largest.
    fn next_uid(&self) -> String {
        let largest = self
            .effects
            .iter()
            .filter_map(|e| e.uid()?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        (largest + 1).to_string()
    }

    /// A copy of effect `key` after it, with a new uid; returns that.
    pub fn duplicate_effect(&mut self, key: &str) -> Option<String> {
        let at = self.effect_index(key)?;
        let mut copy = self.effects[at].clone();
        let uid = self.next_uid();
        copy.set_nox_value("uid", Some(Value::String(uid.clone())));
        self.effects.insert(at + 1, copy);
        Some(uid)
    }

    /// Removes effect `key`; what was connected to it is implicit again.
    pub fn remove_effect(&mut self, key: &str) -> Option<Effect> {
        let at = self.effect_index(key)?;
        let removed = self.effects.remove(at);
        if let Some(uid) = removed.uid() {
            for effect in &mut self.effects {
                for (name, connection) in effect.inputs() {
                    if connection.from == uid {
                        effect.set_input(&name, None);
                    }
                }
            }
            if self.effects_output().is_some_and(|c| c.from == uid) {
                self.set_effects_output(None);
            }
        }
        Some(removed)
    }

    /// Gives every rendering effect a uid (the node editor connects
    /// effects by uid; positional keys would change).
    pub fn ensure_effect_uids(&mut self) {
        for i in 0..self.effects.len() {
            if self.effects[i].is_rendering() && self.effects[i].uid().is_none() {
                let uid = self.next_uid();
                self.effects[i].set_nox_value("uid", Some(Value::String(uid)));
            }
        }
    }

    /// What the effects output to the compositor (none: the last
    /// picture).
    pub fn effects_output(&self) -> Option<Connection> {
        Connection::from_value(self.nox_value("effects_output")?)
    }

    pub fn set_effects_output(&mut self, connection: Option<Connection>) {
        self.set_nox_value("effects_output", connection.map(|c| c.to_value()));
    }

    /// Connects output `port` of effect `from` (or `SOURCE`) to input
    /// `input` of effect `to`, giving effects uids and moving `from` (with
    /// what it depends on) before `to` if needed. Refused if `from`
    /// depends on `to`, as that would make a cycle.
    pub fn connect(&mut self, from: &str, port: &str, to: &str, input: &str) -> Result<(), Cycle> {
        self.ensure_effect_uids();
        if from == to || (from != SOURCE && self.depends_on(from, to)) {
            return Err(Cycle);
        }
        let Some(target) = self.effect_mut(to) else {
            return Ok(());
        };
        target.set_input(input, Some(Connection::new(from, port)));
        self.order_effects();
        Ok(())
    }

    /// Whether effect `uid` takes, directly or not, from effect `other`.
    pub fn depends_on(&self, uid: &str, other: &str) -> bool {
        let mut pending = vec![uid.to_owned()];
        let mut seen = std::collections::HashSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            let Some(effect) = self.effect(&current) else {
                continue;
            };
            for (_, connection) in effect.inputs() {
                if connection.from == other {
                    return true;
                }
                pending.push(connection.from);
            }
        }
        false
    }

    /// Reorders the rendering effects so every effect comes after those
    /// it's connected to, moving only what must move: each effect's
    /// sources are pulled forward to just before it (time effects keep
    /// their places).
    fn order_effects(&mut self) {
        let places: Vec<usize> = (0..self.effects.len())
            .filter(|&i| self.effects[i].is_rendering())
            .collect();
        let effects: Vec<Effect> = places.iter().map(|&i| self.effects[i].clone()).collect();
        let mut placed = vec![false; effects.len()];
        let mut ordered: Vec<Effect> = Vec::with_capacity(effects.len());
        // Places effect `i` after its sources (a stored cycle stops at
        // effects being placed).
        fn place(i: usize, effects: &[Effect], placed: &mut [bool], ordered: &mut Vec<Effect>) {
            if placed[i] {
                return;
            }
            placed[i] = true;
            for (_, connection) in effects[i].inputs() {
                if let Some(source) = effects
                    .iter()
                    .position(|e| e.uid() == Some(connection.from.as_str()))
                {
                    place(source, effects, placed, ordered);
                }
            }
            ordered.push(effects[i].clone());
        }
        for i in 0..effects.len() {
            place(i, &effects, &mut placed, &mut ordered);
        }
        for (place, effect) in places.into_iter().zip(ordered) {
            self.effects[place] = effect;
        }
    }

    /// Moves effect `key` one place up (earlier, `-1`) or down (`1`)
    /// among the rendering effects; time effects keep their places.
    /// Returns whether it moved.
    pub fn move_effect(&mut self, key: &str, by: isize) -> bool {
        let Some(at) = self.effect_index(key) else {
            return false;
        };
        // Effects without a uid are known by their place: give them one
        // first, so keys stay valid after the move.
        self.ensure_effect_uids();
        let rendering: Vec<usize> = (0..self.effects.len())
            .filter(|&i| self.effects[i].is_rendering())
            .collect();
        let Some(place) = rendering.iter().position(|&i| i == at) else {
            return false;
        };
        let Some(&other) = place.checked_add_signed(by).and_then(|p| rendering.get(p)) else {
            return false;
        };
        self.effects.swap(at, other);
        true
    }
}

fn effect_key(index: usize, effect: &Effect) -> String {
    match effect.uid() {
        Some(uid) => uid.to_owned(),
        None => format!("#{index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        let mut item = Item::new("clip");
        item.add_effect(Effect::rendering("nox.gaussian_blur", "Blur"));
        item.add_effect(Effect::rendering("nox.chroma_key", "Key"));
        item
    }

    #[test]
    fn stack_order_and_uids() {
        let mut item = item();
        let keys: Vec<String> = item.rendering_effects().map(|(k, _)| k).collect();
        assert_eq!(keys, ["1", "2"]);
        assert!(item.move_effect("2", -1));
        let names: Vec<&str> = item
            .rendering_effects()
            .map(|(_, e)| e.effect_name.as_str())
            .collect();
        assert_eq!(names, ["nox.chroma_key", "nox.gaussian_blur"]);
        assert!(!item.move_effect("2", -1), "already first");
        assert_eq!(item.duplicate_effect("1").as_deref(), Some("3"));
        assert!(item.remove_effect("2").is_some());
        let keys: Vec<String> = item.rendering_effects().map(|(k, _)| k).collect();
        assert_eq!(keys, ["1", "3"]);
    }

    #[test]
    fn time_effects_keep_their_place() {
        let mut item = item();
        item.set_time_effects(2.0, false);
        assert!(item.move_effect("1", 1));
        assert_eq!(item.speed(), 2.0);
        assert_eq!(item.rendering_effects().count(), 2);
        assert_eq!(item.effects.len(), 3);
    }

    #[test]
    fn effects_without_uid_by_position() {
        let mut item = Item::new("clip");
        // As other tools (or older Nox) write them: without a uid.
        item.effects
            .push(Effect::rendering("nox.sharpen", "Sharpen"));
        item.add_effect(Effect::rendering("nox.gaussian_blur", "Blur"));
        let keys: Vec<String> = item.rendering_effects().map(|(k, _)| k).collect();
        assert_eq!(keys, ["#0", "1"]);
        assert!(item.move_effect("1", -1));
        let keys: Vec<String> = item.rendering_effects().map(|(k, _)| k).collect();
        assert_eq!(keys, ["1", "2"]);
        assert_eq!(item.effects[1].effect_name, "nox.sharpen");
    }

    #[test]
    fn params_and_keyframes() {
        let mut item = item();
        let blur = item.effect_mut("1").expect("blur");
        assert_eq!(blur.number_at("radius", 0.0, 5.0), 5.0, "default");
        blur.set_number_at("radius", 0.0, 8.0);
        assert_eq!(blur.number_at("radius", 3.0, 5.0), 8.0, "constant");
        blur.toggle_keyframe("radius", 1.0, 5.0);
        blur.set_number_at("radius", 3.0, 20.0);
        assert_eq!(blur.keyframes("radius").len(), 2);
        assert_eq!(blur.number_at("radius", 2.0, 5.0), 14.0, "interpolated");
        blur.toggle_keyframe("radius", 1.0, 5.0);
        blur.toggle_keyframe("radius", 3.0, 5.0);
        assert!(blur.keyframes("radius").is_empty());
        assert_eq!(blur.number_at("radius", 0.0, 5.0), 20.0, "last value stays");
        blur.set_param("radius", None);
        assert_eq!(blur.number_at("radius", 0.0, 5.0), 5.0);
        assert!(blur.metadata.is_empty() || blur.param("radius").is_none());
    }

    fn order(item: &Item) -> Vec<String> {
        item.rendering_effects().map(|(k, _)| k).collect()
    }

    #[test]
    fn connections_order_the_effects() {
        let mut item = item(); // 1 blur, 2 key
        let segment = item.add_effect(Effect::rendering("nox.segment_subject", "Segment"));
        assert_eq!(segment, "3");
        // The blur's mask from the segmentation: it moves before the blur.
        item.connect("3", "matte", "1", "mask").unwrap();
        assert_eq!(order(&item), ["3", "1", "2"]);
        assert_eq!(
            item.effect("1").unwrap().input("mask"),
            Some(Connection::new("3", "matte"))
        );
        // The segmentation of the source picture: no move needed.
        item.connect(SOURCE, "picture", "3", "picture").unwrap();
        assert_eq!(order(&item), ["3", "1", "2"]);
        // The key from the blur; then the blur can't take from the key.
        item.connect("1", "picture", "2", "picture").unwrap();
        assert_eq!(item.connect("2", "matte", "1", "mask"), Err(Cycle));
        assert_eq!(
            item.connect("2", "picture", "3", "picture"),
            Err(Cycle),
            "via the blur"
        );
        assert_eq!(item.connect("1", "picture", "1", "picture"), Err(Cycle));
        assert!(item.depends_on("2", "3"));
        assert!(!item.depends_on("3", "2"));
    }

    #[test]
    fn removing_an_effect_drops_its_connections() {
        let mut item = item();
        item.connect("1", "picture", "2", "picture").unwrap();
        item.set_effects_output(Some(Connection::new("1", "picture")));
        item.remove_effect("1").unwrap();
        assert_eq!(item.effect("2").unwrap().input("picture"), None);
        assert_eq!(item.effects_output(), None);
    }

    #[test]
    fn time_effects_stay_in_place_when_ordering() {
        let mut item = item();
        item.set_time_effects(2.0, false); // after 1 and 2
        item.connect("2", "matte", "1", "mask").unwrap();
        assert_eq!(order(&item), ["2", "1"]);
        assert_eq!(
            item.effects[2].kind,
            EffectKind::LinearTimeWarp { time_scalar: 2.0 }
        );
    }

    #[test]
    fn positions_and_uids() {
        let mut item = Item::new("clip");
        // As other tools (or older Nox) write them: without a uid.
        item.effects
            .push(Effect::rendering("nox.sharpen", "Sharpen"));
        item.ensure_effect_uids();
        assert_eq!(order(&item), ["1"]);
        let effect = item.effect_mut("1").unwrap();
        assert_eq!(effect.node_position(), None);
        effect.set_node_position(Some((120.0, -40.0)));
        assert_eq!(effect.node_position(), Some((120.0, -40.0)));
    }

    #[test]
    fn item_keyframe_helpers_include_effects() {
        let mut item = item();
        let blur = item.effect_mut("1").expect("blur");
        blur.toggle_keyframe("radius", 2.0, 5.0);
        assert_eq!(item.keyframe_times(), [2.0]);
        item.move_keyframes(2.0, 4.0);
        assert!(item.effect("1").expect("blur").has_keyframe("radius", 4.0));
        item.set_ease_at(4.0, Ease::Smooth);
        assert_eq!(
            item.effect("1").expect("blur").keyframes("radius")[0].ease,
            Ease::Smooth
        );
    }
}
