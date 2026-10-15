//! Project settings (`resources/ui/project-settings.ui`), laid out like
//! Resolve's: categories in a sidebar, the settings of one category on the
//! right, Cancel and Save. Saving applies everything as one undoable edit.

use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*};
use nox_project::{Rational, ScalingMode, supports_drop_frame};
use std::rc::Rc;

use crate::i18n::{fill, gettext, n_, scaling_mode_name};
use crate::project::Project;
use crate::ui_util::string_list;

/// Timeline resolutions offered (name translated when shown); anything
/// else is "Custom".
const RESOLUTIONS: &[(u32, u32, &str)] = &[
    (1280, 720, n_("HD")),
    (1920, 1080, n_("HD")),
    (2560, 1440, n_("QHD")),
    (3840, 2160, n_("Ultra HD")),
    (4096, 2160, n_("DCI 4K")),
    (1080, 1920, n_("Vertical")),
    (1080, 1080, n_("Square")),
];

/// Timeline frame rates offered.
const FRAME_RATES: &[(&str, i64, i64)] = &[
    ("23.976", 24000, 1001),
    ("24", 24, 1),
    ("25", 25, 1),
    ("29.97", 30000, 1001),
    ("30", 30, 1),
    ("48", 48, 1),
    ("50", 50, 1),
    ("59.94", 60000, 1001),
    ("60", 60, 1),
];

mod imp {
    use super::*;
    use std::cell::OnceCell;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/feliwir/Nox/ui/project-settings.ui")]
    pub struct ProjectSettings {
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub name: TemplateChild<gtk::Entry>,
        #[template_child]
        pub resolution: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub width: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub height: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub frame_rate: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub drop_frame: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub scaling: TemplateChild<gtk::DropDown>,
        pub project: OnceCell<Rc<Project>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProjectSettings {
        const NAME: &'static str = "NoxProjectSettings";
        type Type = super::ProjectSettings;
        type ParentType = gtk::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            // Escape closes, as for GTK's own dialogs.
            klass.add_binding_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                "window.close",
            );
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ProjectSettings {}
    impl WidgetImpl for ProjectSettings {}
    impl WindowImpl for ProjectSettings {}
}

glib::wrapper! {
    pub struct ProjectSettings(ObjectSubclass<imp::ProjectSettings>)
        @extends gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
            gtk::Root, gtk::ShortcutManager;
}

/// Opens the dialog for `project` over `parent`.
pub fn show(parent: &gtk::Window, project: &Rc<Project>) {
    let dialog: ProjectSettings = glib::Object::builder()
        .property("transient-for", parent)
        .build();
    dialog.load(project);
    dialog.present();
}

#[gtk::template_callbacks]
impl ProjectSettings {
    /// Fills the controls from `project`.
    pub(crate) fn load(&self, project: &Rc<Project>) {
        let imp = self.imp();
        assert!(imp.project.set(project.clone()).is_ok(), "loaded once");
        let timeline = project.timeline().clone();
        let settings = &timeline.settings;
        self.set_title(Some(&fill(
            &gettext("Project Settings: {name}"),
            &[("name", &timeline.name)],
        )));
        imp.name.set_text(&timeline.name);

        let mut resolutions: Vec<String> = RESOLUTIONS
            .iter()
            .map(|&(w, h, name)| format!("{w} × {h} {}", gettext(name)))
            .collect();
        resolutions.push(gettext("Custom"));
        imp.resolution.set_model(Some(&string_list(&resolutions)));
        let preset = RESOLUTIONS
            .iter()
            .position(|&(w, h, _)| (w, h) == (settings.width, settings.height));
        imp.width.set_value(f64::from(settings.width));
        imp.height.set_value(f64::from(settings.height));
        imp.resolution
            .set_selected(preset.unwrap_or(RESOLUTIONS.len()) as u32);
        self.sync_size();
        imp.resolution.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.sync_size()
        ));

        imp.frame_rate
            .set_model(Some(&string_list(FRAME_RATES.iter().map(|(n, _, _)| *n))));
        let fps = timeline.frame_rate();
        // An imported timeline at another rate keeps it unless changed.
        let rate = FRAME_RATES
            .iter()
            .position(|&(_, n, d)| Rational::new(n, d) == fps);
        imp.frame_rate
            .set_selected(rate.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32));
        imp.drop_frame.set_active(settings.drop_frame);
        self.sync_drop_frame();
        imp.frame_rate.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.sync_drop_frame()
        ));

        imp.scaling.set_model(Some(&string_list(
            ScalingMode::ALL.iter().map(|&m| scaling_mode_name(m)),
        )));
        let mode = ScalingMode::ALL.iter().position(|&m| m == settings.scaling);
        imp.scaling.set_selected(mode.unwrap_or(0) as u32);

        imp.stack.set_visible_child_name("timeline");
    }

    fn selected_rate(&self) -> Option<Rational> {
        FRAME_RATES
            .get(self.imp().frame_rate.selected() as usize)
            .map(|&(_, n, d)| Rational::new(n, d))
    }

    /// Presets fill in the size; only "Custom" can be edited.
    fn sync_size(&self) {
        let imp = self.imp();
        let selected = imp.resolution.selected() as usize;
        let custom = selected >= RESOLUTIONS.len();
        imp.width.set_sensitive(custom);
        imp.height.set_sensitive(custom);
        if let Some(&(w, h, _)) = RESOLUTIONS.get(selected) {
            imp.width.set_value(f64::from(w));
            imp.height.set_value(f64::from(h));
        }
    }

    /// Drop-frame timecode exists only for 29.97 and 59.94 fps.
    fn sync_drop_frame(&self) {
        let rate = self.selected_rate();
        self.imp()
            .drop_frame
            .set_sensitive(rate.is_some_and(supports_drop_frame));
    }

    #[template_callback]
    fn on_save_clicked(&self) {
        let imp = self.imp();
        let Some(project) = imp.project.get() else {
            return;
        };
        let rate = self.selected_rate();
        let size = (
            imp.width.value() as u32 & !1,
            imp.height.value() as u32 & !1,
        );
        let mode = ScalingMode::ALL[imp.scaling.selected() as usize % ScalingMode::ALL.len()];
        let drop = imp.drop_frame.is_active() && rate.is_some_and(supports_drop_frame);
        let name = imp.name.text().trim().to_owned();
        project.change(&gettext("Change Project Settings"), |t| {
            if !name.is_empty() {
                t.name = name;
            }
            if let Some(rate) = rate
                && rate != t.frame_rate()
            {
                t.set_frame_rate(rate);
            }
            (t.settings.width, t.settings.height) = size;
            t.settings.scaling = mode;
            t.settings.drop_frame = drop;
        });
        self.close();
    }
}
