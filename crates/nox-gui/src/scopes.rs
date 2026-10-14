//! The Color page's scopes (`resources/ui/scopes-panel.ui`): waveform, RGB
//! parade, vectorscope and histogram of the shown frame. The engine
//! measures them on the GPU (`nox_gpu::ScopeImages`); these widgets draw
//! the images with a graticule.

use gtk::{CompositeTemplate, gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};
use nox_engine::{Image, ScopeImages};

use crate::i18n::gettext;

/// Which scope a `ScopeView` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[enum_type(name = "NoxScopeKind")]
pub enum ScopeKind {
    #[default]
    Waveform,
    Parade,
    Vectorscope,
    Histogram,
}

impl ScopeKind {
    fn name(self) -> String {
        match self {
            Self::Waveform => gettext("Waveform"),
            Self::Parade => gettext("RGB Parade"),
            Self::Vectorscope => gettext("Vectorscope"),
            Self::Histogram => gettext("Histogram"),
        }
    }
}

const BACKGROUND: gdk::RGBA = gdk::RGBA::new(0.07, 0.075, 0.085, 1.0);
const GRATICULE: gdk::RGBA = gdk::RGBA::new(1.0, 1.0, 1.0, 0.16);
const LABEL: gdk::RGBA = gdk::RGBA::new(1.0, 1.0, 1.0, 0.45);
const SKIN_TONE: gdk::RGBA = gdk::RGBA::new(1.0, 0.75, 0.55, 0.45);
/// Space around the trace for the scale labels.
const MARGIN: f32 = 6.0;
const SCALE_WIDTH: f32 = 22.0;

mod view {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::ScopeView)]
    pub struct ScopeView {
        #[property(get, set = Self::set_kind, builder(ScopeKind::default()))]
        pub kind: Cell<ScopeKind>,
        pub texture: RefCell<Option<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScopeView {
        const NAME: &'static str = "NoxScopeView";
        type Type = super::ScopeView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("scope");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for ScopeView {
        fn constructed(&self) {
            self.parent_constructed();
            self.set_kind(ScopeKind::default());
        }
    }

    impl ScopeView {
        fn set_kind(&self, kind: ScopeKind) {
            self.kind.set(kind);
            self.obj()
                .update_property(&[gtk::accessible::Property::Label(&kind.name())]);
            self.obj().queue_draw();
        }
    }

    impl WidgetImpl for ScopeView {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (80, 160, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            snapshot.append_color(&BACKGROUND, &graphene::Rect::new(0.0, 0.0, w, h));
            let kind = self.kind.get();
            let area = trace_area(kind, w, h);
            if let Some(texture) = self.texture.borrow().as_ref() {
                snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Linear, &area);
            }
            draw_graticule(obj.upcast_ref(), snapshot, kind, &area);
            // The scope's name in the top left corner.
            let layout = obj.create_pango_layout(Some(&kind.name()));
            snapshot.save();
            snapshot.translate(&graphene::Point::new(MARGIN, MARGIN - 2.0));
            snapshot.append_layout(&layout, &LABEL);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    /// One scope with its graticule.
    pub struct ScopeView(ObjectSubclass<view::ScopeView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ScopeView {
    fn show(&self, image: Option<&Image>) {
        let texture = image.map(|image| {
            gdk::MemoryTextureBuilder::new()
                .set_width(image.width as i32)
                .set_height(image.height as i32)
                .set_format(gdk::MemoryFormat::R8g8b8a8)
                .set_bytes(Some(&glib::Bytes::from(&image.data[..])))
                .set_stride(image.stride)
                .build()
        });
        self.imp().texture.replace(texture);
        self.queue_draw();
    }
}

/// Where the trace goes: the waveform, parade and histogram fill the view
/// right of their scale; the vectorscope is a centred square.
fn trace_area(kind: ScopeKind, w: f32, h: f32) -> graphene::Rect {
    // Below the scope's name.
    let top = MARGIN + 22.0;
    let height = (h - top - MARGIN).max(1.0);
    match kind {
        ScopeKind::Vectorscope => {
            let side = (w - 2.0 * MARGIN).min(height).max(1.0);
            graphene::Rect::new((w - side) / 2.0, top + (height - side) / 2.0, side, side)
        }
        _ => graphene::Rect::new(
            SCALE_WIDTH,
            top,
            (w - SCALE_WIDTH - MARGIN).max(1.0),
            height,
        ),
    }
}

/// A 1 px line.
fn line(snapshot: &gtk::Snapshot, from: (f32, f32), to: (f32, f32), color: &gdk::RGBA) {
    let builder = gsk::PathBuilder::new();
    builder.move_to(from.0, from.1);
    builder.line_to(to.0, to.1);
    snapshot.append_stroke(&builder.to_path(), &gsk::Stroke::new(1.0), color);
}

/// Small text, vertically centred on `y`.
fn label(widget: &gtk::Widget, snapshot: &gtk::Snapshot, text: &str, x: f32, y: f32) {
    let layout = widget.create_pango_layout(Some(text));
    layout.set_font_description(Some(&gtk::pango::FontDescription::from_string("7")));
    let (_, height) = layout.pixel_size();
    snapshot.save();
    snapshot.translate(&graphene::Point::new(x, y - height as f32 / 2.0));
    snapshot.append_layout(&layout, &LABEL);
    snapshot.restore();
}

fn draw_graticule(
    widget: &gtk::Widget,
    snapshot: &gtk::Snapshot,
    kind: ScopeKind,
    area: &graphene::Rect,
) {
    let (x, y, w, h) = (area.x(), area.y(), area.width(), area.height());
    match kind {
        ScopeKind::Waveform | ScopeKind::Parade => {
            // Signal levels in percent; 100% at the top.
            for percent in [0, 25, 50, 75, 100] {
                let ly = y + h * (1.0 - percent as f32 / 100.0);
                line(snapshot, (x, ly), (x + w, ly), &GRATICULE);
                label(widget, snapshot, &percent.to_string(), 2.0, ly);
            }
            if kind == ScopeKind::Parade {
                for third in [1.0, 2.0] {
                    let lx = x + w * third / 3.0;
                    line(snapshot, (lx, y), (lx, y + h), &GRATICULE);
                }
            }
        }
        ScopeKind::Histogram => {
            for quarter in 1..4 {
                let lx = x + w * quarter as f32 / 4.0;
                line(snapshot, (lx, y), (lx, y + h), &GRATICULE);
            }
            line(snapshot, (x, y + h), (x + w, y + h), &GRATICULE);
        }
        ScopeKind::Vectorscope => draw_vectorscope_graticule(widget, snapshot, area),
    }
}

/// Crosshair, the edge of the signal range, 75% color bar targets and the
/// skin tone line, in BT.709 Cb/Cr like the trace.
fn draw_vectorscope_graticule(
    widget: &gtk::Widget,
    snapshot: &gtk::Snapshot,
    area: &graphene::Rect,
) {
    let (x, y, w, h) = (area.x(), area.y(), area.width(), area.height());
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let scale = w; // Cb/Cr of ±0.5 spans the square
    let at = |cb: f32, cr: f32| (cx + cb * scale, cy - cr * scale);
    line(snapshot, (x, cy), (x + w, cy), &GRATICULE);
    line(snapshot, (cx, y), (cx, y + h), &GRATICULE);
    let circle = gsk::PathBuilder::new();
    circle.add_circle(&graphene::Point::new(cx, cy), scale * 0.5);
    snapshot.append_stroke(&circle.to_path(), &gsk::Stroke::new(1.0), &GRATICULE);

    let chroma = |r: f32, g: f32, b: f32| {
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        ((b - y) / 1.8556, (r - y) / 1.5748)
    };
    for (name, rgb) in [
        ("R", (0.75, 0.0, 0.0)),
        ("Mg", (0.75, 0.0, 0.75)),
        ("B", (0.0, 0.0, 0.75)),
        ("Cy", (0.0, 0.75, 0.75)),
        ("G", (0.0, 0.75, 0.0)),
        ("Yl", (0.75, 0.75, 0.0)),
    ] {
        let (cb, cr) = chroma(rgb.0, rgb.1, rgb.2);
        let (tx, ty) = at(cb, cr);
        let builder = gsk::PathBuilder::new();
        builder.add_rect(&graphene::Rect::new(tx - 4.0, ty - 4.0, 8.0, 8.0));
        snapshot.append_stroke(&builder.to_path(), &gsk::Stroke::new(1.0), &LABEL);
        // Labels outward from the centre.
        let (dx, dy) = (tx - cx, ty - cy);
        let length = (dx * dx + dy * dy).sqrt().max(1.0);
        let (lx, ly) = (tx + dx / length * 12.0 - 5.0, ty + dy / length * 12.0);
        label(widget, snapshot, name, lx, ly);
    }
    // Skin tones of any complexion fall near one line, about 123° from the
    // +Cb axis.
    let angle = 123f32.to_radians();
    let edge = at(0.5 * angle.cos(), 0.5 * angle.sin());
    line(snapshot, (cx, cy), edge, &SKIN_TONE);
}

mod panel {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/feliwir/Nox/ui/scopes-panel.ui")]
    pub struct ScopesPanel {
        #[template_child]
        pub waveform: TemplateChild<ScopeView>,
        #[template_child]
        pub parade: TemplateChild<ScopeView>,
        #[template_child]
        pub vectorscope: TemplateChild<ScopeView>,
        #[template_child]
        pub histogram: TemplateChild<ScopeView>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScopesPanel {
        const NAME: &'static str = "NoxScopesPanel";
        type Type = super::ScopesPanel;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            ScopeView::ensure_type();
            ScopeKind::static_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ScopesPanel {}
    impl WidgetImpl for ScopesPanel {}
    impl BoxImpl for ScopesPanel {}
}

glib::wrapper! {
    /// The four scopes, two by two.
    pub struct ScopesPanel(ObjectSubclass<panel::ScopesPanel>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl ScopesPanel {
    /// Shows the scopes of a frame, or clears them.
    pub fn show(&self, scopes: Option<&ScopeImages>) {
        let imp = self.imp();
        imp.waveform.show(scopes.map(|s| &s.waveform));
        imp.parade.show(scopes.map(|s| &s.parade));
        imp.vectorscope.show(scopes.map(|s| &s.vectorscope));
        imp.histogram.show(scopes.map(|s| &s.histogram));
    }
}
