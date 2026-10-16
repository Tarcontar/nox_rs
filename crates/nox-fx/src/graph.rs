//! A clip's effects as a graph (`nox_project::effects`): which effect's
//! output each input takes, resolved at a time into nodes the renderer
//! runs in order (`resolve`), and between the two ways of editing them:
//! the inspector's stack (only for `is_linear` graphs) and the Effects
//! page's node editor (`materialize` before its first edit, `simplify`
//! after stack edits).
//!
//! An unconnected picture input takes the picture of the effect before it
//! (or the clip's), as in a plain stack. A disabled effect, or one Nox
//! doesn't know, passes its picture input through and outputs no matte.

use nox_project::{Connection, Item, SOURCE};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::registry::{EffectDef, ParamValue, PortKind, find, values};

/// Where a node's input comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// The clip's own picture.
    Clip,
    /// Output `port` of node `index` (an earlier one).
    Node { index: usize, port: &'static str },
}

/// An effect at one time, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub def: &'static EffectDef,
    /// Its key in the item (`Item::rendering_effects`).
    pub key: String,
    /// Every parameter of `def`, in its order.
    pub values: Vec<ParamValue>,
    /// Where each input of `def` comes from (`None`: nothing, e.g. no mask).
    pub inputs: Vec<Option<Source>>,
}

impl Resolved {
    pub fn get(&self, name: &str) -> ParamValue {
        let i = self
            .def
            .params
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| panic!("{} has no parameter {name}", self.def.id));
        self.values[i]
    }

    pub fn number(&self, name: &str) -> f64 {
        match self.get(name) {
            ParamValue::Number(v) => v,
            other => panic!("{name} is {other:?}"),
        }
    }

    pub fn bool(&self, name: &str) -> bool {
        matches!(self.get(name), ParamValue::Bool(true))
    }

    pub fn choice(&self, name: &str) -> &'static str {
        match self.get(name) {
            ParamValue::Choice(v) => v,
            other => panic!("{name} is {other:?}"),
        }
    }

    pub fn color(&self, name: &str) -> [f64; 3] {
        match self.get(name) {
            ParamValue::Color(v) => v,
            other => panic!("{name} is {other:?}"),
        }
    }

    /// Where input `name` comes from.
    pub fn input(&self, name: &str) -> Option<Source> {
        let i = self.def.inputs.iter().position(|p| p.name == name)?;
        self.inputs[i]
    }

    /// Feeds what decides its output into `hasher` (its effect, values
    /// and inputs; not where its inputs' pictures come from).
    pub fn hash_into(&self, hasher: &mut impl Hasher) {
        self.def.id.hash(hasher);
        for value in &self.values {
            value.hash_into(hasher);
        }
        self.inputs.hash(hasher);
    }
}

/// A clip's effects at one time: nodes in the order they run (each takes
/// only from earlier ones), and where the picture that's composited comes
/// from. Nodes the output doesn't need are left out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Graph {
    pub nodes: Vec<Resolved>,
    pub output: Option<Source>,
}

impl Graph {
    /// Whether it changes nothing: the clip's picture as it is.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Where the composited picture comes from.
    pub fn output(&self) -> Source {
        self.output.unwrap_or(Source::Clip)
    }

    /// Feeds what decides the output into `hasher`.
    pub fn hash_into(&self, hasher: &mut impl Hasher) {
        for node in &self.nodes {
            node.hash_into(hasher);
        }
        self.output.hash(hasher);
    }

    /// What `source` carries.
    pub fn kind(&self, source: Source) -> PortKind {
        match source {
            Source::Clip => PortKind::Picture,
            Source::Node { index, port } => self.nodes[index]
                .def
                .output(port)
                .map_or(PortKind::Picture, |p| p.kind),
        }
    }
}

/// What an effect key stands for while resolving.
enum Entry {
    Node(usize),
    /// Disabled or unknown: its picture output is this.
    Through(Source),
}

/// `item`'s enabled effects that Nox knows, with their values at media
/// time `time` (seconds) and their inputs resolved. Others (another
/// tool's, a newer Nox's) are kept in the project and pass their picture
/// through.
pub fn resolve(item: &Item, time: f64) -> Graph {
    let mut graph = Graph::default();
    let mut entries: HashMap<String, Entry> = HashMap::new();
    let mut previous = Source::Clip;
    let lookup = |graph: &Graph, entries: &HashMap<String, Entry>, c: &Connection| {
        if c.from == SOURCE {
            return (c.port == "picture").then_some(Source::Clip);
        }
        match entries.get(&c.from)? {
            Entry::Node(index) => {
                let port = graph.nodes[*index].def.output(&c.port)?;
                Some(Source::Node {
                    index: *index,
                    port: port.name,
                })
            }
            Entry::Through(source) => (c.port == "picture").then_some(*source),
        }
    };
    for (key, effect) in item.rendering_effects() {
        let picture = match effect.input("picture") {
            Some(c) => lookup(&graph, &entries, &c).unwrap_or(Source::Clip),
            None => previous,
        };
        let def = find(&effect.effect_name).filter(|_| effect.enabled);
        let Some(def) = def else {
            // Passes its picture through (unknown effects are taken to
            // have one; a disabled one without, e.g. a segmentation, adds
            // nothing).
            let known = find(&effect.effect_name);
            if known.is_none_or(|d| d.outputs_picture()) {
                previous = picture;
            }
            entries.insert(key, Entry::Through(picture));
            continue;
        };
        let inputs = def
            .inputs
            .iter()
            .map(|port| {
                let source = match (effect.input(port.name), port.kind) {
                    (None, PortKind::Picture) => Some(picture),
                    (None, PortKind::Matte) => None,
                    (Some(_), PortKind::Picture) => Some(picture),
                    (Some(c), PortKind::Matte) => lookup(&graph, &entries, &c),
                };
                // A connection to the wrong kind counts as none.
                source.filter(|&s| graph.kind(s) == port.kind)
            })
            .collect();
        let index = graph.nodes.len();
        graph.nodes.push(Resolved {
            def,
            key: key.clone(),
            values: values(effect, def, time),
            inputs,
        });
        if def.outputs_picture() {
            previous = Source::Node {
                index,
                port: "picture",
            };
        }
        entries.insert(key, Entry::Node(index));
    }
    let output = item
        .effects_output()
        .and_then(|c| lookup(&graph, &entries, &c))
        .filter(|&s| graph.kind(s) == PortKind::Picture)
        .unwrap_or(previous);
    graph.output = Some(output);
    prune(graph)
}

/// `graph` without the nodes its output doesn't need (e.g. a segmentation
/// nothing uses).
fn prune(graph: Graph) -> Graph {
    let mut needed = vec![false; graph.nodes.len()];
    let mut pending: Vec<Source> = graph.output.into_iter().collect();
    while let Some(source) = pending.pop() {
        if let Source::Node { index, .. } = source
            && !needed[index]
        {
            needed[index] = true;
            pending.extend(graph.nodes[index].inputs.iter().flatten());
        }
    }
    let mut new_index = vec![usize::MAX; graph.nodes.len()];
    let mut kept = 0;
    for (index, keep) in needed.iter().enumerate() {
        if *keep {
            new_index[index] = kept;
            kept += 1;
        }
    }
    let remap = |source: Source| match source {
        Source::Clip => Source::Clip,
        Source::Node { index, port } => Source::Node {
            index: new_index[index],
            port,
        },
    };
    let nodes = graph
        .nodes
        .into_iter()
        .zip(&needed)
        .filter(|(_, keep)| **keep)
        .map(|(mut node, _)| {
            node.inputs = node.inputs.into_iter().map(|s| s.map(remap)).collect();
            node
        })
        .collect();
    let output = match graph.output.map(remap) {
        Some(Source::Clip) | None => None,
        output => output,
    };
    Graph { nodes, output }
}

/// The implicit picture input of each rendering effect (by key, in
/// order), and the last picture (the default output).
fn implicit_pictures(item: &Item) -> (Vec<(String, Connection)>, Connection) {
    let mut previous = Connection::new(SOURCE, "picture");
    let mut implicit = Vec::new();
    for (key, effect) in item.rendering_effects() {
        implicit.push((key.clone(), previous.clone()));
        // Disabled ones too: they pass their picture on, and the next
        // effect still connects to them.
        if find(&effect.effect_name).is_none_or(|d| d.outputs_picture()) {
            previous = Connection::new(&key, "picture");
        }
    }
    (implicit, previous)
}

/// A wire for showing the graph: into input `input` of effect `to` (by
/// key) from `from`, implicit (not stored) or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wire {
    pub to: String,
    pub input: &'static str,
    pub from: Connection,
    pub implicit: bool,
}

/// Every wire into `item`'s effects, as the node editor shows them:
/// stored connections, and the picture each unconnected picture input
/// takes. Unlike `resolve`, it keeps disabled and unused effects.
pub fn wires(item: &Item) -> Vec<Wire> {
    let (implicit, _) = implicit_pictures(item);
    let mut wires = Vec::new();
    for (key, previous) in implicit {
        let Some(effect) = item.effect(&key) else {
            continue;
        };
        let Some(def) = find(&effect.effect_name) else {
            continue;
        };
        for port in def.inputs {
            match effect.input(port.name) {
                Some(from) => wires.push(Wire {
                    to: key.clone(),
                    input: port.name,
                    from,
                    implicit: false,
                }),
                None if port.kind == PortKind::Picture => wires.push(Wire {
                    to: key.clone(),
                    input: port.name,
                    from: previous.clone(),
                    implicit: true,
                }),
                None => {}
            }
        }
    }
    wires
}

/// What the output takes, and whether that's implicit (the last picture).
pub fn output_wire(item: &Item) -> (Connection, bool) {
    match item.effects_output() {
        Some(connection) => (connection, false),
        None => (implicit_pictures(item).1, true),
    }
}

/// Whether `item`'s effects are a plain stack the inspector can show:
/// only effects that fit one (`EffectDef::in_stack`), each taking the
/// picture before it, no mattes wired, and the last picture as the output.
pub fn is_linear(item: &Item) -> bool {
    let (implicit, last) = implicit_pictures(item);
    if item.effects_output().is_some_and(|c| c != last) {
        return false;
    }
    implicit.iter().all(|(key, previous)| {
        let Some(effect) = item.effect(key) else {
            return true;
        };
        let fits = find(&effect.effect_name).is_none_or(|d| d.in_stack());
        fits && effect
            .inputs()
            .iter()
            .all(|(name, c)| name == "picture" && c == previous)
    })
}

/// Writes out every implicit picture input and the output, so the graph
/// stays the same when the node editor reorders effects. Gives effects
/// uids first.
pub fn materialize(item: &mut Item) {
    item.ensure_effect_uids();
    let (implicit, last) = implicit_pictures(item);
    for (key, previous) in implicit {
        let Some(effect) = item.effect_mut(&key) else {
            continue;
        };
        let takes_picture = find(&effect.effect_name).is_none_or(|d| d.input("picture").is_some());
        if takes_picture && effect.input("picture").is_none() {
            effect.set_input("picture", Some(previous));
        }
    }
    if item.effects_output().is_none() {
        item.set_effects_output(Some(last));
    }
}

/// Clears connections that are the implicit ones anyway (after the stack
/// was edited), so stacks stay plain.
pub fn simplify(item: &mut Item) {
    let (implicit, last) = implicit_pictures(item);
    for (key, previous) in implicit {
        if let Some(effect) = item.effect_mut(&key)
            && effect.input("picture").as_ref() == Some(&previous)
        {
            effect.set_input("picture", None);
        }
    }
    if item.effects_output().as_ref() == Some(&last) {
        item.set_effects_output(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::*;
    use nox_project::{Effect, Value};

    fn add(item: &mut Item, id: &str) -> String {
        item.add_effect(Effect::rendering(id, id))
    }

    #[test]
    fn stacks_resolve_in_order() {
        let mut item = Item::new("clip");
        let blur = add(&mut item, GAUSSIAN_BLUR);
        let key = add(&mut item, CHROMA_KEY);
        add(&mut item, "com.other.glow");
        let graph = resolve(&item, 0.0);
        assert_eq!(graph.nodes.len(), 2, "unknown effects aren't rendered");
        assert_eq!(graph.nodes[0].input("picture"), Some(Source::Clip));
        assert_eq!(graph.nodes[0].input("mask"), None);
        assert_eq!(
            graph.nodes[1].input("picture"),
            Some(Source::Node {
                index: 0,
                port: "picture"
            })
        );
        assert_eq!(
            graph.output(),
            Source::Node {
                index: 1,
                port: "picture"
            }
        );
        assert_eq!(graph.nodes[0].number("radius"), 1.0);
        assert_eq!(graph.nodes[1].color("color"), [0.0, 0.69, 0.25]);
        let effect = item.effect_mut(&blur).unwrap();
        effect.set_number_at("radius", 0.0, 99.0);
        let effect = item.effect_mut(&key).unwrap();
        effect.set_param("color", Some(ParamValue::Color([0.1, 0.2, 0.9]).to_value()));
        effect.set_param("show_matte", Some(Value::Bool(true)));
        effect.toggle_keyframe("tolerance", 0.0, 20.0);
        effect.set_number_at("tolerance", 2.0, 40.0);
        let graph = resolve(&item, 1.0);
        assert_eq!(graph.nodes[0].number("radius"), 10.0, "clamped");
        assert_eq!(graph.nodes[1].color("color"), [0.1, 0.2, 0.9]);
        assert!(graph.nodes[1].bool("show_matte"));
        assert_eq!(graph.nodes[1].number("tolerance"), 30.0);
        // Disabled: the key takes the clip's picture.
        item.effect_mut(&blur).unwrap().enabled = false;
        let graph = resolve(&item, 1.0);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].input("picture"), Some(Source::Clip));
        assert!(is_linear(&item));
    }

    /// The background blurred: the blur masked by the inverted subject.
    fn background_blur() -> (Item, [String; 3]) {
        let mut item = Item::new("clip");
        let segment = add(&mut item, SEGMENT_SUBJECT);
        let invert = add(&mut item, INVERT_MATTE);
        let blur = add(&mut item, GAUSSIAN_BLUR);
        materialize(&mut item);
        item.connect(&segment, "matte", &invert, "matte").unwrap();
        item.connect(&invert, "matte", &blur, "mask").unwrap();
        (item, [segment, invert, blur])
    }

    #[test]
    fn mattes_wire_masks() {
        let (item, _) = background_blur();
        assert!(!is_linear(&item));
        let graph = resolve(&item, 0.0);
        let ids: Vec<&str> = graph.nodes.iter().map(|n| n.def.id).collect();
        assert_eq!(ids, [SEGMENT_SUBJECT, INVERT_MATTE, GAUSSIAN_BLUR]);
        assert_eq!(graph.nodes[0].input("picture"), Some(Source::Clip));
        assert_eq!(
            graph.nodes[1].input("matte"),
            Some(Source::Node {
                index: 0,
                port: "matte"
            })
        );
        // The segmentation outputs no picture: the blur takes the clip's.
        assert_eq!(graph.nodes[2].input("picture"), Some(Source::Clip));
        assert_eq!(
            graph.nodes[2].input("mask"),
            Some(Source::Node {
                index: 1,
                port: "matte"
            })
        );
        assert_eq!(
            graph.output(),
            Source::Node {
                index: 2,
                port: "picture"
            }
        );
    }

    #[test]
    fn unused_nodes_are_left_out() {
        let (mut item, [_, invert, blur]) = background_blur();
        // The blur's mask gone: segmentation and inversion feed nothing.
        item.effect_mut(&blur).unwrap().set_input("mask", None);
        let graph = resolve(&item, 0.0);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].def.id, GAUSSIAN_BLUR);
        // A disabled inversion outputs no matte: no mask.
        let (mut item, _) = background_blur();
        item.effect_mut(&invert).unwrap().enabled = false;
        let graph = resolve(&item, 0.0);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].input("mask"), None);
    }

    #[test]
    fn the_output_can_be_any_picture() {
        let mut item = Item::new("clip");
        let blur = add(&mut item, GAUSSIAN_BLUR);
        add(&mut item, SHARPEN);
        item.set_effects_output(Some(Connection::new(&blur, "picture")));
        let graph = resolve(&item, 0.0);
        assert_eq!(graph.nodes.len(), 1, "the sharpen isn't shown");
        assert_eq!(
            graph.output(),
            Source::Node {
                index: 0,
                port: "picture"
            }
        );
        assert!(!is_linear(&item));
        // Straight from the clip: nothing to run.
        item.set_effects_output(Some(Connection::new(SOURCE, "picture")));
        assert!(resolve(&item, 0.0).is_empty());
    }

    #[test]
    fn materialized_stacks_survive_reordering_and_simplify() {
        let mut item = Item::new("clip");
        let blur = add(&mut item, GAUSSIAN_BLUR);
        let key = add(&mut item, CHROMA_KEY);
        let before = resolve(&item, 0.0);
        materialize(&mut item);
        assert_eq!(
            item.effect(&key).unwrap().input("picture"),
            Some(Connection::new(&blur, "picture"))
        );
        assert!(is_linear(&item), "explicit but the same");
        assert_eq!(resolve(&item, 0.0), before);
        simplify(&mut item);
        assert!(item.effect(&key).unwrap().inputs().is_empty());
        assert_eq!(item.effects_output(), None);
        assert_eq!(resolve(&item, 0.0), before);
    }

    #[test]
    fn wires_show_implicit_and_stored_inputs() {
        let (item, [segment, invert, blur]) = background_blur();
        let wires = wires(&item);
        assert!(wires.iter().all(|w| !w.implicit), "materialized");
        assert!(wires.contains(&Wire {
            to: blur.clone(),
            input: "mask",
            from: Connection::new(&invert, "matte"),
            implicit: false,
        }));
        assert!(
            wires
                .iter()
                .any(|w| w.to == segment && w.input == "picture")
        );
        let mut plain = Item::new("clip");
        let first = add(&mut plain, GAUSSIAN_BLUR);
        let second = add(&mut plain, SHARPEN);
        assert_eq!(
            super::wires(&plain),
            [
                Wire {
                    to: first.clone(),
                    input: "picture",
                    from: Connection::new(SOURCE, "picture"),
                    implicit: true
                },
                Wire {
                    to: second.clone(),
                    input: "picture",
                    from: Connection::new(&first, "picture"),
                    implicit: true
                },
            ]
        );
        assert_eq!(
            output_wire(&plain),
            (Connection::new(&second, "picture"), true)
        );
    }

    #[test]
    fn graph_only_effects_are_not_linear() {
        let mut item = Item::new("clip");
        add(&mut item, SEGMENT_SUBJECT);
        assert!(!is_linear(&item));
    }
}
