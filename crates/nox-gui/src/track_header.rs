//! A timeline track header (`resources/ui/track-header.ui`): the track's
//! name and its toggles as real widgets, so they are reachable by keyboard
//! and assistive technology. The timeline owns the headers, keeps them in
//! sync with the tracks and applies the toggles as undoable edits.

use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*};
use nox_project::{Track, TrackKind};
use std::rc::Rc;

use crate::i18n::{fill, gettext};

/// A toggle in a track header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Mute for audio, hide for video: on means the track is off.
    Off,
    Solo,
    Lock,
    Sync,
}

type Handler = Rc<dyn Fn(usize, Toggle, bool)>;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/track-header.ui")]
    pub struct TrackHeader {
        #[template_child]
        pub name: TemplateChild<gtk::Label>,
        #[template_child]
        pub enabled: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub solo: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub lock: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub sync: TemplateChild<gtk::ToggleButton>,
        /// Stack index of the track shown.
        pub track: Cell<usize>,
        /// Set while showing the track's state, so it isn't sent back.
        pub updating: Cell<bool>,
        pub handler: RefCell<Option<Handler>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackHeader {
        const NAME: &'static str = "NoxTrackHeader";
        type Type = super::TrackHeader;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for TrackHeader {}
    impl WidgetImpl for TrackHeader {}
    impl BoxImpl for TrackHeader {}
}

glib::wrapper! {
    pub struct TrackHeader(ObjectSubclass<imp::TrackHeader>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl TrackHeader {
    /// A header that calls `handler` with the track, toggle and new state
    /// when a toggle is clicked.
    pub fn new(handler: impl Fn(usize, Toggle, bool) + 'static) -> Self {
        let header: Self = glib::Object::new();
        header.imp().handler.replace(Some(Rc::new(handler)));
        header
    }

    /// Shows `track` (stack index `index`), named `name`.
    pub fn show(&self, index: usize, track: &Track, name: &str) {
        let imp = self.imp();
        imp.track.set(index);
        imp.updating.set(true);
        imp.name.set_label(name);
        let audio = track.kind == TrackKind::Audio;
        let off = !track.item.enabled;
        imp.enabled.set_active(off);
        imp.enabled.set_icon_name(match (audio, off) {
            (true, false) => "audio-volume-high-symbolic",
            (true, true) => "audio-volume-muted-symbolic",
            (false, false) => "view-reveal-symbolic",
            (false, true) => "view-conceal-symbolic",
        });
        // Solo is for audio, as in Resolve.
        imp.solo.set_visible(audio);
        imp.solo.set_active(track.item.solo());
        imp.lock.set_active(track.item.locked());
        imp.sync.set_active(track.item.sync_locked());
        imp.updating.set(false);

        // Labels name the track, since screen readers announce the button
        // alone; the pressed state is announced by the toggle itself.
        let named = |template: String| fill(&template, &[("track", &name)]);
        let enabled_label = if audio {
            named(gettext("Mute {track}"))
        } else {
            named(gettext("Hide {track}"))
        };
        for (button, label, tooltip) in [
            (
                &*imp.enabled,
                enabled_label,
                if audio {
                    gettext("Mute")
                } else {
                    gettext("Hide")
                },
            ),
            (&*imp.solo, named(gettext("Solo {track}")), gettext("Solo")),
            (&*imp.lock, named(gettext("Lock {track}")), gettext("Lock")),
            (
                &*imp.sync,
                named(gettext("Sync lock {track}")),
                gettext("Sync Lock: follow ripple edits on other tracks"),
            ),
        ] {
            button.set_tooltip_text(Some(&tooltip));
            button.update_property(&[gtk::accessible::Property::Label(&label)]);
        }
    }

    fn toggled(&self, toggle: Toggle, button: &gtk::ToggleButton) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let handler = imp.handler.borrow().clone();
        if let Some(handler) = handler {
            handler(imp.track.get(), toggle, button.is_active());
        }
    }

    #[template_callback]
    fn on_enabled_toggled(&self, button: &gtk::ToggleButton) {
        self.toggled(Toggle::Off, button);
    }

    #[template_callback]
    fn on_solo_toggled(&self, button: &gtk::ToggleButton) {
        self.toggled(Toggle::Solo, button);
    }

    #[template_callback]
    fn on_lock_toggled(&self, button: &gtk::ToggleButton) {
        self.toggled(Toggle::Lock, button);
    }

    #[template_callback]
    fn on_sync_toggled(&self, button: &gtk::ToggleButton) {
        self.toggled(Toggle::Sync, button);
    }
}
