//! The Effects page's node editor: the shown clip's effects as a graph
//! (nox-fx's `wires`), between a Source node (the clip's picture) and an
//! Output node (what's composited). Nodes are boxes with their inputs on
//! the left and outputs on the right, colored by what flows through them
//! (pictures, mattes); wires connect them, dashed where implicit (the
//! picture before, as in a stack).
//!
//! Drag a node to move it, from an output to an input to connect them,
//! and a connected input away to reconnect it (or, for a matte, off to
//! disconnect it). Right-click adds a node, Delete removes the selected
//! one. Scroll pans, Ctrl+scroll zooms, Home fits the graph. Every change
//! is one undo step, on the shown clip only.
//!
//! While the page shows, each node shows a thumbnail of what it outputs at
//! the playhead (its picture, else its matte), made by the engine with the
//! frame (`Playback::set_thumbnail_clip`). Nodes the output doesn't need
//! aren't run and show none.

use gtk::{gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};
use nox_fx::{EffectDef, PortKind};
use nox_project::{Composable, Connection, Effect, Item, SOURCE};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::effect_controls::effect_list_popover;
use crate::i18n::{fill, gettext};
use crate::inspector::Inspector;
use crate::playback::Playback;
use crate::timeline::TimelineView;

/// The Output node's key.
pub const OUTPUT: &str = "output";

const NODE_WIDTH: f64 = 168.0;
const HEADER: f64 = 26.0;
const ROW: f64 = 22.0;
/// The thumbnail's room below the ports (16:9, inset by `THUMB_MARGIN`).
const THUMB_MARGIN: f64 = 6.0;
const THUMB_HEIGHT: f64 = (NODE_WIDTH - 2.0 * THUMB_MARGIN) * 9.0 / 16.0;
/// Room between auto-placed nodes.
const SPACING: f64 = 230.0;
/// How far from a port (pixels) it can be grabbed.
const GRAB: f64 = 10.0;

const PICTURE: gdk::RGBA = gdk::RGBA::new(0.85, 0.65, 0.25, 1.0);
const MATTE: gdk::RGBA = gdk::RGBA::new(0.62, 0.66, 0.70, 1.0);
const NODE: gdk::RGBA = gdk::RGBA::new(0.22, 0.23, 0.25, 1.0);
const NODE_HEADER: gdk::RGBA = gdk::RGBA::new(0.30, 0.31, 0.34, 1.0);
const SELECTED: gdk::RGBA = gdk::RGBA::new(0.21, 0.52, 0.89, 1.0);
const TEXT: gdk::RGBA = gdk::RGBA::new(0.92, 0.92, 0.92, 1.0);
const DIM_TEXT: gdk::RGBA = gdk::RGBA::new(0.6, 0.6, 0.6, 1.0);
const THUMB_BACKGROUND: gdk::RGBA = gdk::RGBA::new(0.15, 0.15, 0.16, 1.0);

fn color(kind: PortKind) -> gdk::RGBA {
    match kind {
        PortKind::Picture => PICTURE,
        PortKind::Matte => MATTE,
    }
}

/// A port of a node, as drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: &'static str,
    pub label: String,
    pub kind: PortKind,
}

/// A node, as drawn, in canvas units.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeBox {
    /// The effect's key, `SOURCE` or `OUTPUT`.
    pub key: String,
    pub title: String,
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
    pub position: (f64, f64),
    pub enabled: bool,
}

impl NodeBox {
    pub fn height(&self) -> f64 {
        self.thumbnail_top() + THUMB_HEIGHT + THUMB_MARGIN
    }

    /// Where the thumbnail starts, below the ports (from the node's top).
    fn thumbnail_top(&self) -> f64 {
        HEADER + ROW * self.inputs.len().max(self.outputs.len()).max(1) as f64 + 4.0
    }

    pub fn contains(&self, (x, y): (f64, f64)) -> bool {
        let (left, top) = self.position;
        x >= left && x <= left + NODE_WIDTH && y >= top && y <= top + self.height()
    }

    /// Where input (or output) `index` is.
    pub fn port(&self, output: bool, index: usize) -> (f64, f64) {
        let (left, top) = self.position;
        let x = if output { left + NODE_WIDTH } else { left };
        (x, top + HEADER + ROW * index as f64 + ROW / 2.0)
    }
}

/// A wire, as drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct WireBox {
    pub from: (usize, usize),
    pub to: (usize, usize),
    pub kind: PortKind,
    pub implicit: bool,
}

/// The shown clip's graph, in canvas units.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub nodes: Vec<NodeBox>,
    pub wires: Vec<WireBox>,
}

/// What's under a point.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    /// Output `port` of node `node`.
    Output {
        node: usize,
        port: usize,
    },
    Input {
        node: usize,
        port: usize,
    },
    Node(usize),
}

impl Layout {
    /// `item`'s graph: Source, its effects (where placed, else in a row in
    /// order), Output.
    pub fn of(item: &Item) -> Self {
        let mut nodes = vec![NodeBox {
            key: SOURCE.into(),
            title: gettext("Source"),
            inputs: Vec::new(),
            outputs: vec![Port {
                name: "picture",
                label: gettext("Picture"),
                kind: PortKind::Picture,
            }],
            position: (0.0, 0.0),
            enabled: true,
        }];
        let ports = |ports: &[nox_fx::PortDef]| {
            ports
                .iter()
                .map(|p| Port {
                    name: p.name,
                    label: gettext(p.label),
                    kind: p.kind,
                })
                .collect::<Vec<_>>()
        };
        let mut right = 0.0f64;
        for (i, (key, effect)) in item.rendering_effects().enumerate() {
            let def = nox_fx::find(&effect.effect_name);
            let (title, inputs, outputs) = match def {
                Some(def) => (gettext(def.label), ports(def.inputs), ports(def.outputs)),
                // Kept, passing its picture through.
                None => (
                    fill(
                        &gettext("{name} (not supported)"),
                        &[("name", &effect.name)],
                    ),
                    ports(&[nox_fx::registry::PICTURE_IN]),
                    ports(&[nox_fx::registry::PICTURE_OUT]),
                ),
            };
            let position = effect
                .node_position()
                .unwrap_or((SPACING * (i + 1) as f64, 0.0));
            right = right.max(position.0);
            nodes.push(NodeBox {
                key,
                title,
                inputs,
                outputs,
                position,
                enabled: effect.enabled,
            });
        }
        // The Source left of everything, the Output right of it.
        let left = nodes[1..]
            .iter()
            .map(|n| n.position.0)
            .fold(SPACING, f64::min);
        nodes[0].position = (left - SPACING, 0.0);
        nodes.push(NodeBox {
            key: OUTPUT.into(),
            title: gettext("Output"),
            inputs: vec![Port {
                name: "picture",
                label: gettext("Picture"),
                kind: PortKind::Picture,
            }],
            outputs: Vec::new(),
            position: (right + SPACING, 0.0),
            enabled: true,
        });
        let index = |key: &str| nodes.iter().position(|n| n.key == key);
        let port_index = |node: usize, name: &str, output: bool| {
            let ports = if output {
                &nodes[node].outputs
            } else {
                &nodes[node].inputs
            };
            ports.iter().position(|p| p.name == name)
        };
        let mut wires = Vec::new();
        let mut add = |from: &Connection, to: &str, input: &str, implicit: bool| {
            let (Some(source), Some(target)) = (index(&from.from), index(to)) else {
                return;
            };
            let (Some(out), Some(in_)) = (
                port_index(source, &from.port, true),
                port_index(target, input, false),
            ) else {
                return;
            };
            wires.push(WireBox {
                from: (source, out),
                to: (target, in_),
                kind: nodes[source].outputs[out].kind,
                implicit,
            });
        };
        for wire in nox_fx::wires(item) {
            add(&wire.from, &wire.to, wire.input, wire.implicit);
        }
        let (output, implicit) = nox_fx::output_wire(item);
        add(&output, OUTPUT, "picture", implicit);
        Self { nodes, wires }
    }

    /// What's at `point` (canvas units; ports within `grab`).
    pub fn hit(&self, point: (f64, f64), grab: f64) -> Option<Hit> {
        let near = |(x, y): (f64, f64)| (x - point.0).hypot(y - point.1) <= grab;
        // The last drawn is on top.
        for (node, n) in self.nodes.iter().enumerate().rev() {
            if let Some(port) = (0..n.outputs.len()).find(|&p| near(n.port(true, p))) {
                return Some(Hit::Output { node, port });
            }
            if let Some(port) = (0..n.inputs.len()).find(|&p| near(n.port(false, p))) {
                return Some(Hit::Input { node, port });
            }
            if n.contains(point) {
                return Some(Hit::Node(node));
            }
        }
        None
    }

    /// The canvas rectangle around every node.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        self.nodes.iter().fold(
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            |(l, t, r, b), n| {
                (
                    l.min(n.position.0),
                    t.min(n.position.1),
                    r.max(n.position.0 + NODE_WIDTH),
                    b.max(n.position.1 + n.height()),
                )
            },
        )
    }
}

/// What a drag does.
#[derive(Debug, Clone)]
enum Drag {
    /// Moves node `key` by the pointer's movement (`offset` so far).
    Node {
        key: String,
        offset: (f64, f64),
    },
    /// A wire from output `port` of `from` to the pointer; `detached` is
    /// the input it was taken off (reconnecting).
    Wire {
        from: Connection,
        kind: PortKind,
        start: (f64, f64),
        pointer: (f64, f64),
        detached: Option<(String, &'static str)>,
    },
    Pan {
        start: (f64, f64),
    },
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct NodeEditor {
        pub timeline: OnceCell<TimelineView>,
        pub inspector: OnceCell<glib::WeakRef<Inspector>>,
        pub layout: RefCell<Layout>,
        /// Canvas to widget: `widget = canvas × zoom + pan`.
        pub pan: Cell<(f64, f64)>,
        pub zoom: Cell<f64>,
        /// The clip the view was fitted to.
        pub fitted: RefCell<Option<nox_project::ItemRef>>,
        pub selected: RefCell<Option<String>>,
        pub(super) drag: RefCell<Option<Drag>>,
        pub playback: OnceCell<Weak<Playback>>,
        /// Whether the Effects page shows (thumbnails are made then).
        pub active: Cell<bool>,
        /// The clip thumbnails are made of (`Timeline::clip_key`).
        pub thumbnail_clip: Cell<Option<u64>>,
        /// Its nodes' thumbnails, by node key.
        pub thumbnails: RefCell<HashMap<String, gdk::Texture>>,
        #[allow(clippy::type_complexity)]
        pub selection_changed: RefCell<Vec<Box<dyn Fn(Option<String>)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NodeEditor {
        const NAME: &'static str = "NoxNodeEditor";
        type Type = super::NodeEditor;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("nox-node-editor");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for NodeEditor {
        fn constructed(&self) {
            self.parent_constructed();
            self.zoom.set(1.0);
            let obj = self.obj();
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.set_focusable(true);
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.update_property(&[gtk::accessible::Property::Label(&gettext("Effect graph"))]);
            obj.set_up_input();
        }
    }

    impl WidgetImpl for NodeEditor {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().draw(snapshot);
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            // Laid out before it had a size (a page not shown yet): fit now.
            self.obj().fit_if_new();
        }

        fn measure(&self, orientation: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (200, 800, -1, -1),
                _ => (160, 320, -1, -1),
            }
        }
    }
}

glib::wrapper! {
    pub struct NodeEditor(ObjectSubclass<imp::NodeEditor>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for NodeEditor {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl NodeEditor {
    /// Shows `timeline`'s shown clip; AI nodes are added through
    /// `inspector`, which offers their models. Called once.
    pub fn set_up(&self, timeline: &TimelineView, inspector: &Inspector) {
        let imp = self.imp();
        let _ = imp.timeline.set(timeline.clone());
        let _ = imp.inspector.set(inspector.downgrade());
        timeline.connect_selection_changed(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |_| editor.refresh()
        ));
        timeline.project().connect_changed(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |_| editor.refresh()
        ));
        self.refresh();
    }

    /// Shows thumbnails `playback` makes. Called once.
    pub fn set_playback(&self, playback: &Rc<Playback>) {
        let _ = self.imp().playback.set(Rc::downgrade(playback));
        let editor = self.downgrade();
        playback.connect_thumbnails(move |clip, thumbnails| {
            if let Some(editor) = editor.upgrade() {
                editor.show_thumbnails(clip, thumbnails);
            }
        });
    }

    /// Makes thumbnails while `active` (the Effects page shows).
    pub fn set_active(&self, active: bool) {
        if self.imp().active.replace(active) != active {
            self.update_thumbnail_clip();
        }
    }

    /// Asks for thumbnails of the shown clip, if active (else none).
    fn update_thumbnail_clip(&self) {
        let imp = self.imp();
        let clip = imp.active.get().then(|| self.shown_clip_key()).flatten();
        if imp.thumbnail_clip.replace(clip) == clip {
            return;
        }
        imp.thumbnails.borrow_mut().clear();
        if let Some(playback) = imp.playback.get().and_then(Weak::upgrade) {
            playback.set_thumbnail_clip(clip);
        }
        self.queue_draw();
    }

    /// The shown clip's `Timeline::clip_key`.
    fn shown_clip_key(&self) -> Option<u64> {
        let timeline = self.imp().timeline.get()?;
        let project = timeline.project();
        let model = project.timeline();
        match model.item(timeline.selection()?).ok()? {
            Composable::Clip(clip) => model.clip_key(clip),
            _ => None,
        }
    }

    /// Shows clip `clip`'s new thumbnails (if still shown); nodes without
    /// one (not run) show none.
    fn show_thumbnails(&self, clip: u64, thumbnails: &[nox_fx::Thumbnail]) {
        let imp = self.imp();
        if imp.thumbnail_clip.get() != Some(clip) {
            return;
        }
        *imp.thumbnails.borrow_mut() = thumbnails
            .iter()
            .map(|t| {
                let texture = gdk::MemoryTexture::new(
                    t.width as i32,
                    t.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from(&t.rgba),
                    t.width as usize * 4,
                );
                (t.node.clone(), texture.upcast())
            })
            .collect();
        self.queue_draw();
    }

    /// Calls `f` with the selected node's key when the selection changes.
    pub fn connect_selection_changed(&self, f: impl Fn(Option<String>) + 'static) {
        self.imp().selection_changed.borrow_mut().push(Box::new(f));
    }

    /// The selected node's key (an effect's, or `SOURCE`/`OUTPUT`).
    pub fn selected(&self) -> Option<String> {
        self.imp().selected.borrow().clone()
    }

    fn select(&self, key: Option<String>) {
        if *self.imp().selected.borrow() == key {
            return;
        }
        self.imp().selected.replace(key.clone());
        for f in self.imp().selection_changed.borrow().iter() {
            f(key.clone());
        }
        self.queue_draw();
    }

    /// The shown clip, if it's a video clip.
    fn item(&self) -> Option<(nox_project::ItemRef, Item)> {
        let timeline = self.imp().timeline.get()?;
        let selection = timeline.selection()?;
        let project = timeline.project();
        let model = project.timeline();
        let video = model
            .track(selection.track)
            .is_some_and(|t| t.kind == nox_project::TrackKind::Video);
        match model.item(selection).ok()? {
            Composable::Clip(clip) if video => Some((selection, clip.item.clone())),
            _ => None,
        }
    }

    /// Lays out the shown clip again (keeping an ongoing node drag's
    /// position).
    pub fn refresh(&self) {
        let imp = self.imp();
        let item = self.item();
        let mut layout = item
            .as_ref()
            .map(|(_, i)| Layout::of(i))
            .unwrap_or_default();
        if let Some(Drag::Node { key, offset, .. }) = &*imp.drag.borrow()
            && let Some(node) = layout.nodes.iter_mut().find(|n| &n.key == key)
        {
            node.position.0 += offset.0;
            node.position.1 += offset.1;
        }
        let selected = imp.selected.borrow().clone();
        if selected.is_some_and(|key| !layout.nodes.iter().any(|n| n.key == key)) {
            self.select(None);
        }
        imp.layout.replace(layout);
        self.fit_if_new();
        self.update_thumbnail_clip();
        self.queue_draw();
    }

    /// Fits a newly shown clip's graph (once the editor has a size).
    fn fit_if_new(&self) {
        let imp = self.imp();
        let shown = self.imp().timeline.get().and_then(TimelineView::selection);
        if *imp.fitted.borrow() != shown
            && self.width() > 0
            && !imp.layout.borrow().nodes.is_empty()
        {
            imp.fitted.replace(shown);
            self.fit();
        }
    }

    /// Zooms and pans so the whole graph shows (at most at 100 %).
    pub fn fit(&self) {
        let layout = self.imp().layout.borrow();
        if layout.nodes.is_empty() {
            return;
        }
        let (l, t, r, b) = layout.bounds();
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        let margin = 40.0;
        let zoom = ((w - 2.0 * margin) / (r - l))
            .min((h - 2.0 * margin) / (b - t))
            .clamp(0.25, 1.0);
        self.imp().zoom.set(zoom);
        self.imp().pan.set((
            (w - (r - l) * zoom) / 2.0 - l * zoom,
            (h - (b - t) * zoom) / 2.0 - t * zoom,
        ));
        self.queue_draw();
    }

    fn to_canvas(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let (px, py) = self.imp().pan.get();
        let zoom = self.imp().zoom.get();
        ((x - px) / zoom, (y - py) / zoom)
    }

    /// Changes the shown clip's effects (one undo step).
    fn edit(&self, label: &str, change: impl Fn(&mut Item)) {
        if let Some(timeline) = self.imp().timeline.get() {
            timeline.edit_shown_clip(label, None, |item| {
                // Written out, so reordering keeps every wire.
                nox_fx::materialize(item);
                change(item);
            });
        }
    }

    fn set_up_input(&self) {
        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |_, x, y| {
                editor.grab_focus();
                editor.begin(x, y);
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |gesture, dx, dy| {
                if let Some((x, y)) = gesture.start_point() {
                    editor.update((x, y), (dx, dy));
                }
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |gesture, dx, dy| {
                if let Some((x, y)) = gesture.start_point() {
                    editor.end((x + dx, y + dy));
                }
            }
        ));
        self.add_controller(drag);

        let menu = gtk::GestureClick::builder().button(3).build();
        menu.connect_pressed(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |_, _, x, y| editor.add_menu(x, y)
        ));
        self.add_controller(menu);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, dx, dy| {
                let imp = editor.imp();
                let ctrl = controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK);
                let (px, py) = imp.pan.get();
                if ctrl {
                    // Around the pointer.
                    let pointer = controller
                        .current_event()
                        .and_then(|e| e.position())
                        .unwrap_or((
                            f64::from(editor.width()) / 2.0,
                            f64::from(editor.height()) / 2.0,
                        ));
                    let zoom = imp.zoom.get();
                    let new = (zoom * 1.15f64.powf(-dy)).clamp(0.2, 3.0);
                    imp.zoom.set(new);
                    imp.pan.set((
                        pointer.0 - (pointer.0 - px) * new / zoom,
                        pointer.1 - (pointer.1 - py) * new / zoom,
                    ));
                } else {
                    imp.pan.set((px - dx * 30.0, py - dy * 30.0));
                }
                editor.queue_draw();
                glib::Propagation::Stop
            }
        ));
        self.add_controller(scroll);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| match key {
                gdk::Key::Delete | gdk::Key::BackSpace => {
                    editor.remove_selected();
                    glib::Propagation::Stop
                }
                gdk::Key::Home => {
                    editor.fit();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        ));
        self.add_controller(keys);
    }

    fn begin(&self, x: f64, y: f64) {
        let imp = self.imp();
        let point = self.to_canvas((x, y));
        let grab = GRAB / imp.zoom.get();
        let layout = imp.layout.borrow().clone();
        let drag = match layout.hit(point, grab) {
            Some(Hit::Output { node, port }) => {
                let n = &layout.nodes[node];
                Drag::Wire {
                    from: Connection::new(&n.key, n.outputs[port].name),
                    kind: n.outputs[port].kind,
                    start: n.port(true, port),
                    pointer: point,
                    detached: None,
                }
            }
            Some(Hit::Input { node, port }) => {
                // Take the wire off this input, to reconnect it.
                let Some(wire) = layout.wires.iter().find(|w| w.to == (node, port)) else {
                    return;
                };
                let source = &layout.nodes[wire.from.0];
                let n = &layout.nodes[node];
                Drag::Wire {
                    from: Connection::new(&source.key, source.outputs[wire.from.1].name),
                    kind: wire.kind,
                    start: source.port(true, wire.from.1),
                    pointer: point,
                    detached: Some((n.key.clone(), n.inputs[port].name)),
                }
            }
            Some(Hit::Node(node)) => {
                let key = layout.nodes[node].key.clone();
                self.select(Some(key.clone()));
                Drag::Node {
                    key,
                    offset: (0.0, 0.0),
                }
            }
            None => {
                self.select(None);
                Drag::Pan {
                    start: imp.pan.get(),
                }
            }
        };
        imp.drag.replace(Some(drag));
    }

    fn update(&self, start: (f64, f64), (dx, dy): (f64, f64)) {
        let imp = self.imp();
        let zoom = imp.zoom.get();
        let point = self.to_canvas((start.0 + dx, start.1 + dy));
        let mut drag = imp.drag.borrow_mut();
        match &mut *drag {
            Some(Drag::Node { key, offset, .. }) => {
                let delta = (dx / zoom - offset.0, dy / zoom - offset.1);
                *offset = (dx / zoom, dy / zoom);
                let mut layout = imp.layout.borrow_mut();
                if let Some(node) = layout.nodes.iter_mut().find(|n| &n.key == key) {
                    node.position.0 += delta.0;
                    node.position.1 += delta.1;
                }
            }
            Some(Drag::Wire { pointer, .. }) => *pointer = point,
            Some(Drag::Pan { start: pan }) => imp.pan.set((pan.0 + dx, pan.1 + dy)),
            None => {}
        }
        drop(drag);
        self.queue_draw();
    }

    fn end(&self, at: (f64, f64)) {
        let imp = self.imp();
        let Some(drag) = imp.drag.take() else {
            return;
        };
        match drag {
            Drag::Node { key, offset, .. } => {
                if offset == (0.0, 0.0) || key == SOURCE || key == OUTPUT {
                    // A click, or the Source/Output (placed by the others).
                    self.refresh();
                    return;
                }
                let layout = imp.layout.borrow().clone();
                let Some(position) = layout
                    .nodes
                    .iter()
                    .find(|n| n.key == key)
                    .map(|n| n.position)
                else {
                    return;
                };
                self.edit(&gettext("Move Node"), |item| {
                    if let Some(effect) = item.effect_mut(&key) {
                        effect.set_node_position(Some(position));
                    }
                });
            }
            Drag::Wire {
                from,
                kind,
                detached,
                ..
            } => {
                let point = self.to_canvas(at);
                let grab = GRAB * 1.5 / imp.zoom.get();
                let layout = imp.layout.borrow().clone();
                let target = match layout.hit(point, grab) {
                    Some(Hit::Input { node, port }) => Some((node, port)),
                    // Dropped on a node: its first free input of the kind.
                    Some(Hit::Node(node)) => layout.nodes[node]
                        .inputs
                        .iter()
                        .position(|p| p.kind == kind)
                        .map(|port| (node, port)),
                    _ => None,
                };
                match target {
                    Some((node, port)) => {
                        let n = &layout.nodes[node];
                        let input = &n.inputs[port];
                        if input.kind != kind || n.key == from.from {
                            self.refresh();
                            return;
                        }
                        let (to, name) = (n.key.clone(), input.name);
                        self.edit(&gettext("Connect"), |item| {
                            if let Some((old, old_input)) = &detached
                                && (old != &to || *old_input != name)
                                && let Some(effect) = item.effect_mut(old)
                                && kind == PortKind::Matte
                            {
                                effect.set_input(old_input, None);
                            }
                            if to == OUTPUT {
                                item.set_effects_output(Some(from.clone()));
                            } else {
                                // A cycle is refused: nothing changes.
                                let _ = item.connect(&from.from, &from.port, &to, name);
                            }
                        });
                    }
                    None => match detached {
                        // A matte input taken off: no mask (a picture input
                        // always has one).
                        Some((old, old_input)) if kind == PortKind::Matte => {
                            self.edit(&gettext("Disconnect"), |item| {
                                if let Some(effect) = item.effect_mut(&old) {
                                    effect.set_input(old_input, None);
                                }
                            });
                        }
                        _ => self.refresh(),
                    },
                }
            }
            Drag::Pan { .. } => {}
        }
        self.queue_draw();
    }

    /// Offers the effects to add at widget point (`x`, `y`).
    fn add_menu(&self, x: f64, y: f64) {
        if self.item().is_none() {
            return;
        }
        let at = self.to_canvas((x, y));
        let popover = effect_list_popover(
            |_| true,
            glib::clone!(
                #[weak(rename_to = editor)]
                self,
                move |def| editor.add_node(def, at)
            ),
        );
        popover.set_parent(self);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            // After its row's handler (unparenting at once would drop it).
            glib::idle_add_local_once(glib::clone!(
                #[weak]
                popover,
                move || popover.unparent()
            ));
        });
        popover.popup();
    }

    /// Adds `def` at canvas point `at` (offering its models first).
    fn add_node(&self, def: &'static EffectDef, at: (f64, f64)) {
        let add = std::rc::Rc::new(glib::clone!(
            #[weak(rename_to = editor)]
            self,
            move |_: &Inspector| {
                let label = fill(&gettext("Add {effect}"), &[("effect", &gettext(def.label))]);
                let added = RefCell::new(None);
                editor.edit(&label, |item| {
                    let key = item.add_effect(Effect::rendering(def.id, def.label));
                    if let Some(effect) = item.effect_mut(&key) {
                        effect.set_node_position(Some(at));
                    }
                    added.replace(Some(key));
                });
                editor.select(added.take());
            }
        ));
        if let Some(inspector) = self.imp().inspector.get().and_then(glib::WeakRef::upgrade) {
            inspector.with_models(def, add);
        }
    }

    fn remove_selected(&self) {
        let Some(key) = self.selected() else {
            return;
        };
        if key == SOURCE || key == OUTPUT {
            return;
        }
        self.edit(&gettext("Remove Node"), |item| {
            item.remove_effect(&key);
        });
    }

    fn draw(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let layout = imp.layout.borrow();
        if layout.nodes.is_empty() {
            let text = gettext("Select a video clip to see its effects.");
            let pango = self.create_pango_layout(Some(&text));
            let (w, h) = pango.pixel_size();
            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                (self.width() - w) as f32 / 2.0,
                (self.height() - h) as f32 / 2.0,
            ));
            snapshot.append_layout(&pango, &DIM_TEXT);
            snapshot.restore();
            return;
        }
        let (px, py) = imp.pan.get();
        let zoom = imp.zoom.get();
        snapshot.save();
        snapshot.translate(&graphene::Point::new(px as f32, py as f32));
        snapshot.scale(zoom as f32, zoom as f32);
        for wire in &layout.wires {
            let from = layout.nodes[wire.from.0].port(true, wire.from.1);
            let to = layout.nodes[wire.to.0].port(false, wire.to.1);
            draw_wire(snapshot, from, to, wire.kind, wire.implicit);
        }
        if let Some(Drag::Wire {
            start,
            pointer,
            kind,
            ..
        }) = &*imp.drag.borrow()
        {
            draw_wire(snapshot, *start, *pointer, *kind, false);
        }
        let selected = imp.selected.borrow();
        let thumbnails = imp.thumbnails.borrow();
        for node in &layout.nodes {
            self.draw_node(
                snapshot,
                node,
                selected.as_deref() == Some(node.key.as_str()),
                thumbnails.get(&node.key),
            );
        }
        snapshot.restore();
    }

    fn draw_node(
        &self,
        snapshot: &gtk::Snapshot,
        node: &NodeBox,
        selected: bool,
        thumbnail: Option<&gdk::Texture>,
    ) {
        let (x, y) = (node.position.0 as f32, node.position.1 as f32);
        let (w, h) = (NODE_WIDTH as f32, node.height() as f32);
        let bounds = self.snap(graphene::Rect::new(x, y, w, h));
        let rect = gsk::RoundedRect::from_rect(bounds, 6.0);
        snapshot.push_rounded_clip(&rect);
        snapshot.append_color(&NODE, &bounds);
        let header = self.snap(graphene::Rect::new(x, y, w, HEADER as f32));
        snapshot.append_color(&NODE_HEADER, &header);
        snapshot.pop();
        // The thumbnail, fitted into its room.
        let room = self.snap(graphene::Rect::new(
            x + THUMB_MARGIN as f32,
            y + node.thumbnail_top() as f32,
            w - 2.0 * THUMB_MARGIN as f32,
            THUMB_HEIGHT as f32,
        ));
        snapshot.append_color(&THUMB_BACKGROUND, &room);
        if let Some(texture) = thumbnail {
            let (tw, th) = (texture.width() as f32, texture.height() as f32);
            let scale = (room.width() / tw).min(room.height() / th);
            let (sw, sh) = (tw * scale, th * scale);
            let area = self.snap(graphene::Rect::new(
                room.x() + (room.width() - sw) / 2.0,
                room.y() + (room.height() - sh) / 2.0,
                sw,
                sh,
            ));
            snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Linear, &area);
        }
        let border = if selected {
            SELECTED
        } else {
            gdk::RGBA::new(0.0, 0.0, 0.0, 0.5)
        };
        // In whole pixels, whatever the zoom.
        let width = if selected { 2.0 } else { 1.0 } / self.device_scale();
        snapshot.append_border(&rect, &[width; 4], &[border; 4]);
        let text = if node.enabled { TEXT } else { DIM_TEXT };
        self.draw_text(
            snapshot,
            &node.title,
            x + 8.0,
            y + 4.0,
            w - 16.0,
            &text,
            false,
        );
        for (i, port) in node.inputs.iter().enumerate() {
            let (px, py) = node.port(false, i);
            draw_port(snapshot, (px, py), port.kind);
            self.draw_text(
                snapshot,
                &port.label,
                px as f32 + 10.0,
                py as f32 - 9.0,
                w / 2.0 - 12.0,
                &DIM_TEXT,
                false,
            );
        }
        for (i, port) in node.outputs.iter().enumerate() {
            let (px, py) = node.port(true, i);
            draw_port(snapshot, (px, py), port.kind);
            self.draw_text(
                snapshot,
                &port.label,
                x + w / 2.0,
                py as f32 - 9.0,
                w / 2.0 - 10.0,
                &DIM_TEXT,
                true,
            );
        }
    }

    /// Device pixels per canvas unit.
    fn device_scale(&self) -> f32 {
        let surface = self.native().and_then(|n| n.surface());
        let scale = surface.map_or(f64::from(self.scale_factor()), |s| s.scale());
        (scale * self.imp().zoom.get()) as f32
    }

    /// `rect` (canvas units) with its edges on whole device pixels, so
    /// edges are crisp at any zoom and nothing bleeds into pixels next to
    /// it (left over when it moves).
    fn snap(&self, rect: graphene::Rect) -> graphene::Rect {
        let (px, py) = self.imp().pan.get();
        let device = self.device_scale();
        let surface = device / self.imp().zoom.get() as f32;
        // Canvas to device pixels and back.
        let to = |v: f32, pan: f64| (v * device + pan as f32 * surface).round();
        let back = |d: f32, pan: f64| (d - pan as f32 * surface) / device;
        let (l, t) = (to(rect.x(), px), to(rect.y(), py));
        let (r, b) = (
            to(rect.x() + rect.width(), px),
            to(rect.y() + rect.height(), py),
        );
        graphene::Rect::new(
            back(l, px),
            back(t, py),
            back(r, px) - back(l, px),
            back(b, py) - back(t, py),
        )
    }

    /// `text` at (`x`, `y`), at most `width` wide (ellipsized),
    /// right-aligned in it if `right`.
    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &self,
        snapshot: &gtk::Snapshot,
        text: &str,
        x: f32,
        y: f32,
        width: f32,
        color: &gdk::RGBA,
        right: bool,
    ) {
        let layout = self.create_pango_layout(Some(text));
        layout.set_width((width * gtk::pango::SCALE as f32) as i32);
        layout.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if right {
            layout.set_alignment(gtk::pango::Alignment::Right);
        }
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x, y));
        snapshot.append_layout(&layout, color);
        snapshot.restore();
    }
}

fn draw_port(snapshot: &gtk::Snapshot, (x, y): (f64, f64), kind: PortKind) {
    let circle = gsk::PathBuilder::new();
    circle.add_circle(&graphene::Point::new(x as f32, y as f32), 5.0);
    let circle = circle.to_path();
    snapshot.append_fill(&circle, gsk::FillRule::Winding, &color(kind));
    snapshot.append_stroke(
        &circle,
        &gsk::Stroke::new(1.0),
        &gdk::RGBA::new(0.0, 0.0, 0.0, 0.6),
    );
}

fn draw_wire(
    snapshot: &gtk::Snapshot,
    from: (f64, f64),
    to: (f64, f64),
    kind: PortKind,
    implicit: bool,
) {
    let bend = ((to.0 - from.0).abs() / 2.0).max(40.0) as f32;
    let path = gsk::PathBuilder::new();
    path.move_to(from.0 as f32, from.1 as f32);
    path.cubic_to(
        from.0 as f32 + bend,
        from.1 as f32,
        to.0 as f32 - bend,
        to.1 as f32,
        to.0 as f32,
        to.1 as f32,
    );
    let stroke = gsk::Stroke::new(2.0);
    if implicit {
        stroke.set_dash(&[6.0, 4.0]);
    }
    snapshot.append_stroke(&path.to_path(), &stroke, &color(kind));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        let mut item = Item::new("clip");
        let segment = item.add_effect(Effect::rendering(nox_fx::registry::SEGMENT_SUBJECT, "S"));
        let blur = item.add_effect(Effect::rendering(nox_fx::registry::GAUSSIAN_BLUR, "B"));
        nox_fx::materialize(&mut item);
        item.connect(&segment, "matte", &blur, "mask").unwrap();
        item
    }

    #[test]
    fn layout_has_source_effects_output_and_wires() {
        let layout = Layout::of(&item());
        let keys: Vec<&str> = layout.nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys, [SOURCE, "1", "2", OUTPUT]);
        // Source → segment, segment's matte → blur's mask, segment's
        // (missing) picture: the blur takes the source; blur → output.
        let kinds: Vec<(usize, usize, PortKind)> = layout
            .wires
            .iter()
            .map(|w| (w.from.0, w.to.0, w.kind))
            .collect();
        assert!(kinds.contains(&(1, 2, PortKind::Matte)));
        assert!(kinds.contains(&(0, 1, PortKind::Picture)));
        assert!(kinds.contains(&(2, 3, PortKind::Picture)));
        // In a row, left to right.
        let xs: Vec<f64> = layout.nodes.iter().map(|n| n.position.0).collect();
        assert!(xs.windows(2).all(|w| w[0] < w[1]), "{xs:?}");
    }

    #[test]
    fn hits_ports_before_nodes() {
        let layout = Layout::of(&item());
        let blur = &layout.nodes[2];
        assert_eq!(
            layout.hit(blur.port(false, 1), GRAB),
            Some(Hit::Input { node: 2, port: 1 })
        );
        assert_eq!(
            layout.hit(blur.port(true, 0), GRAB),
            Some(Hit::Output { node: 2, port: 0 })
        );
        let middle = (blur.position.0 + 80.0, blur.position.1 + 10.0);
        assert_eq!(layout.hit(middle, GRAB), Some(Hit::Node(2)));
        assert_eq!(layout.hit((-10_000.0, 0.0), GRAB), None);
    }

    #[test]
    fn stored_positions_win() {
        let mut item = item();
        item.effect_mut("2")
            .unwrap()
            .set_node_position(Some((900.0, 300.0)));
        let layout = Layout::of(&item);
        assert_eq!(layout.nodes[2].position, (900.0, 300.0));
        assert!(layout.nodes[3].position.0 > 900.0, "the output after it");
    }
}
