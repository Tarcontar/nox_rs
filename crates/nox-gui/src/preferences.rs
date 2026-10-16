//! The user's preferences, the same for every window: how the timeline
//! draws clips and how video is decoded. Kept in `nox/preferences.ini` in
//! the user's config folder, saved on every change, and applied at once
//! (whoever cares connects to the property's notify signal).
//!
//! The preferences window (`resources/ui/preferences.ui`) edits them.

use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*};
use nox_engine::HwDecode;
use std::path::PathBuf;

use crate::i18n::{fill, gettext, n_};

mod model {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::Preferences)]
    pub struct Preferences {
        /// Filmstrips on video clips.
        #[property(get, set)]
        pub timeline_thumbnails: Cell<bool>,
        /// Waveforms on audio clips.
        #[property(get, set)]
        pub timeline_waveforms: Cell<bool>,
        /// How video is decoded: an `HwDecode` key.
        #[property(get, set)]
        pub video_decoding: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Preferences {
        const NAME: &'static str = "NoxPreferences";
        type Type = super::Preferences;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Preferences {}
}

glib::wrapper! {
    pub struct Preferences(ObjectSubclass<model::Preferences>);
}

thread_local! {
    static PREFERENCES: Preferences = Preferences::load();
}

const TIMELINE: &str = "timeline";
const PLAYBACK: &str = "playback";

impl Preferences {
    /// The preferences (loaded on first use).
    pub fn get() -> Self {
        PREFERENCES.with(Clone::clone)
    }

    fn file() -> PathBuf {
        glib::user_config_dir().join("nox").join("preferences.ini")
    }

    /// Reads the file (defaults for whatever it lacks) and saves on every
    /// change from then on.
    fn load() -> Self {
        let file = glib::KeyFile::new();
        if let Err(err) = file.load_from_file(Self::file(), glib::KeyFileFlags::KEEP_COMMENTS)
            && !err.matches(glib::FileError::Noent)
        {
            tracing::warn!("cannot read {}: {err}", Self::file().display());
        }
        let flag = |group, key| file.boolean(group, key).unwrap_or(true);
        let decoding = file
            .string(PLAYBACK, "video-decoding")
            .ok()
            .filter(|key| HwDecode::from_key(key).is_some())
            .map_or_else(|| HwDecode::Auto.key().to_owned(), String::from);
        let preferences: Self = glib::Object::builder()
            .property("timeline-thumbnails", flag(TIMELINE, "thumbnails"))
            .property("timeline-waveforms", flag(TIMELINE, "waveforms"))
            .property("video-decoding", decoding)
            .build();
        let saved = std::cell::RefCell::new(file.to_data().to_string());
        preferences.connect_notify_local(None, move |preferences, _| {
            preferences.save(&file, &mut saved.borrow_mut());
        });
        preferences
    }

    /// Writes the file if its contents changed from `saved`.
    fn save(&self, file: &glib::KeyFile, saved: &mut String) {
        file.set_boolean(TIMELINE, "thumbnails", self.timeline_thumbnails());
        file.set_boolean(TIMELINE, "waveforms", self.timeline_waveforms());
        file.set_string(PLAYBACK, "video-decoding", &self.video_decoding());
        let data = file.to_data().to_string();
        if data == *saved {
            return;
        }
        *saved = data;
        let path = Self::file();
        let saved = std::fs::create_dir_all(path.parent().expect("config folder"))
            .map_err(|err| err.to_string())
            .and_then(|()| file.save_to_file(&path).map_err(|err| err.to_string()));
        if let Err(err) = saved {
            tracing::warn!("cannot save {}: {err}", path.display());
        }
    }

    /// How to decode video: `NOX_DECODE` if set, else the preference.
    pub fn hw_decode(&self) -> HwDecode {
        HwDecode::env_override()
            .or_else(|| HwDecode::from_key(&self.video_decoding()))
            .unwrap_or_default()
    }
}

/// The ways to decode offered here, with what each does. Only APIs this
/// platform has.
fn decoding_choices() -> Vec<(HwDecode, &'static str, &'static str)> {
    let mut choices = vec![
        (
            HwDecode::Auto,
            n_("Automatic"),
            n_(
                "Decodes on the graphics card where it can (Vulkan Video, then NVDEC, then VAAPI), else on the processor.",
            ),
        ),
        (
            HwDecode::Software,
            n_("Processor only"),
            n_(
                "Decodes on the processor: works with every file, but is much slower, especially at 4K. Try it if hardware decoding shows wrong frames.",
            ),
        ),
        (
            HwDecode::Vulkan,
            "Vulkan Video",
            n_(
                "The graphics card's video engine through Vulkan, for the codecs it handles; others are decoded on the processor.",
            ),
        ),
    ];
    if !cfg!(target_os = "macos") {
        choices.push((
            HwDecode::Cuda,
            "NVDEC",
            n_(
                "NVIDIA's video engine, for the codecs it handles; others are decoded on the processor. NVIDIA cards only.",
            ),
        ));
    }
    if cfg!(target_os = "linux") {
        choices.push((
            HwDecode::Vaapi,
            "VAAPI",
            n_(
                "The graphics card's video engine through VAAPI (AMD and Intel cards), for the codecs it handles; others are decoded on the processor.",
            ),
        ));
    }
    choices
}

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/feliwir/Nox/ui/preferences.ui")]
    pub struct PreferencesWindow {
        #[template_child]
        pub thumbnails: TemplateChild<gtk::Switch>,
        #[template_child]
        pub waveforms: TemplateChild<gtk::Switch>,
        #[template_child]
        pub decoding: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub decoding_info: TemplateChild<gtk::Label>,
        #[template_child]
        pub models_info: TemplateChild<gtk::Label>,
        #[template_child]
        pub delete_models: TemplateChild<gtk::Button>,
        pub choices: RefCell<Vec<(HwDecode, &'static str, &'static str)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PreferencesWindow {
        const NAME: &'static str = "NoxPreferencesWindow";
        type Type = super::PreferencesWindow;
        type ParentType = gtk::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
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

    impl ObjectImpl for PreferencesWindow {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup();
        }
    }

    impl WidgetImpl for PreferencesWindow {}
    impl WindowImpl for PreferencesWindow {}
}

glib::wrapper! {
    pub struct PreferencesWindow(ObjectSubclass<imp::PreferencesWindow>)
        @extends gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
            gtk::Root, gtk::ShortcutManager;
}

/// Shows the preferences over `parent`.
pub fn show(parent: Option<&gtk::Window>) {
    let window: PreferencesWindow = glib::Object::builder()
        .property("transient-for", parent)
        .build();
    window.present();
}

#[gtk::template_callbacks]
impl PreferencesWindow {
    #[template_callback]
    fn on_delete_models_clicked(&self) {
        let dir = nox_ai::models::default_dir();
        for model in nox_ai::models::registry().models() {
            let path = model.path(&dir);
            if path.exists()
                && let Err(err) = std::fs::remove_file(&path)
            {
                tracing::warn!("cannot delete {}: {err}", path.display());
            }
        }
        self.show_models();
    }

    fn setup(&self) {
        let imp = self.imp();
        let preferences = Preferences::get();
        for (property, switch) in [
            ("timeline-thumbnails", &imp.thumbnails),
            ("timeline-waveforms", &imp.waveforms),
        ] {
            preferences
                .bind_property(property, &switch.get(), "active")
                .bidirectional()
                .sync_create()
                .build();
        }

        let choices = decoding_choices();
        let names: Vec<String> = choices
            .iter()
            .map(|&(hw, name, _)| match hw {
                // Product names stay as they are.
                HwDecode::Vulkan | HwDecode::Cuda | HwDecode::Vaapi => name.to_owned(),
                _ => gettext(name),
            })
            .collect();
        imp.decoding
            .set_model(Some(&crate::ui_util::string_list(&names)));
        let current = HwDecode::from_key(&preferences.video_decoding()).unwrap_or_default();
        let index = choices.iter().position(|c| c.0 == current).unwrap_or(0);
        imp.choices.replace(choices);
        imp.decoding.set_selected(index as u32);
        imp.decoding.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |dropdown| {
                let choices = window.imp().choices.borrow();
                if let Some(&(hw, ..)) = choices.get(dropdown.selected() as usize) {
                    Preferences::get().set_video_decoding(hw.key());
                }
                drop(choices);
                window.explain_decoding();
            }
        ));
        self.explain_decoding();
        self.show_models();
    }

    /// Which AI models are downloaded, and how big they are.
    fn show_models(&self) {
        let imp = self.imp();
        let dir = nox_ai::models::default_dir();
        let present: Vec<&nox_ai::models::ModelInfo> = nox_ai::models::registry()
            .models()
            .iter()
            .filter(|m| m.is_available(&dir))
            .collect();
        let text = if present.is_empty() {
            gettext("No models downloaded. Effects that need one offer to download it.")
        } else {
            let names: Vec<&str> = present.iter().map(|m| m.name.as_str()).collect();
            let size: u64 = present.iter().map(|m| m.size).sum();
            fill(
                &gettext("{models} ({size} MB) in {folder}"),
                &[
                    ("models", &names.join(", ")),
                    ("size", &(size / 1_000_000)),
                    ("folder", &dir.display()),
                ],
            )
        };
        imp.models_info.set_text(&text);
        imp.delete_models.set_sensitive(!present.is_empty());
    }

    /// Says what the chosen way to decode does, or that `NOX_DECODE`
    /// overrides it.
    fn explain_decoding(&self) {
        let imp = self.imp();
        if let Some(hw) = HwDecode::env_override() {
            imp.decoding.set_sensitive(false);
            imp.decoding_info.set_text(&fill(
                &gettext("Set by the NOX_DECODE environment variable ({value}) for this session."),
                &[("value", &hw.key())],
            ));
            return;
        }
        let choices = imp.choices.borrow();
        let text = choices
            .get(imp.decoding.selected() as usize)
            .map(|&(.., description)| gettext(description))
            .unwrap_or_default();
        imp.decoding_info.set_text(&text);
    }
}
