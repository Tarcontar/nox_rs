//! The Effects page's node panel: the selected node's controls (as in
//! the inspector, `effect_controls`), editing the shown clip only.

use gtk::{glib, prelude::*, subclass::prelude::*};
use nox_fx::EffectDef;
use nox_project::{Composable, Effect, Item, SOURCE};
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use crate::effect_controls::{Control, EffectHost, param_controls, update_controls};
use crate::i18n::{fill, gettext};
use crate::node_editor::OUTPUT;
use crate::timeline::TimelineView;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct NodePanel {
        pub timeline: OnceCell<TimelineView>,
        pub title: OnceCell<gtk::Label>,
        pub enabled: OnceCell<gtk::CheckButton>,
        pub grid: OnceCell<gtk::Grid>,
        /// The node shown (an effect's key, or the Source/Output).
        pub key: RefCell<Option<String>>,
        /// What the controls were built for: key and effect name.
        pub shown: RefCell<Option<(String, String)>>,
        pub def: Cell<Option<&'static EffectDef>>,
        pub(super) controls: RefCell<Vec<Control>>,
        pub updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NodePanel {
        const NAME: &'static str = "NoxNodePanel";
        type Type = super::NodePanel;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for NodePanel {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            obj.set_spacing(8);
            obj.set_margin_top(12);
            obj.set_margin_bottom(12);
            obj.set_margin_start(12);
            obj.set_margin_end(12);
            let title = gtk::Label::new(None);
            title.set_xalign(0.0);
            title.set_wrap(true);
            title.add_css_class("heading");
            let enabled = gtk::CheckButton::with_label(&gettext("On"));
            enabled.set_tooltip_text(Some(&gettext("Turns the effect on or off")));
            enabled.connect_toggled(glib::clone!(
                #[weak]
                obj,
                move |check| {
                    let on = check.is_active();
                    let label = if on {
                        gettext("Turn Effect On")
                    } else {
                        gettext("Turn Effect Off")
                    };
                    obj.edit_shown(&label, None, &move |e: &mut Effect| e.enabled = on);
                }
            ));
            let grid = gtk::Grid::new();
            grid.set_row_spacing(6);
            grid.set_column_spacing(8);
            obj.append(&title);
            obj.append(&enabled);
            obj.append(&grid);
            let _ = self.title.set(title);
            let _ = self.enabled.set(enabled);
            let _ = self.grid.set(grid);
        }
    }

    impl WidgetImpl for NodePanel {}
    impl BoxImpl for NodePanel {}
}

glib::wrapper! {
    pub struct NodePanel(ObjectSubclass<imp::NodePanel>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for NodePanel {
    fn default() -> Self {
        glib::Object::new()
    }
}

/// The panel, as controls edit through it.
impl EffectHost for glib::WeakRef<NodePanel> {
    fn updating(&self) -> bool {
        self.upgrade().is_none_or(|p| p.imp().updating.get())
    }

    fn media_time(&self) -> f64 {
        self.upgrade()
            .and_then(|p| p.imp().timeline.get()?.selected_media_time())
            .unwrap_or_default()
    }

    fn edit_effect(
        &self,
        label: &str,
        merge_key: Option<&str>,
        key: &str,
        change: &dyn Fn(&mut Effect),
    ) {
        if let Some(panel) = self.upgrade()
            && let Some(timeline) = panel.imp().timeline.get()
        {
            let key = key.to_owned();
            timeline.edit_shown_clip(label, merge_key, |item| {
                if let Some(effect) = item.effect_mut(&key) {
                    change(effect);
                }
            });
        }
    }
}

impl NodePanel {
    /// Follows `timeline`'s shown clip. Called once.
    pub fn set_up(&self, timeline: &TimelineView) {
        let _ = self.imp().timeline.set(timeline.clone());
        timeline.connect_playhead_changed(glib::clone!(
            #[weak(rename_to = panel)]
            self,
            move |_, _| panel.refresh()
        ));
        timeline.project().connect_changed(glib::clone!(
            #[weak(rename_to = panel)]
            self,
            move |_| panel.refresh()
        ));
        self.refresh();
    }

    /// Shows node `key` (none: nothing selected).
    pub fn show(&self, key: Option<String>) {
        self.imp().key.replace(key);
        self.refresh();
    }

    /// Changes the shown effect of the shown clip.
    fn edit_shown(&self, label: &str, merge_key: Option<&str>, change: &dyn Fn(&mut Effect)) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        if let Some(key) = imp.key.borrow().clone() {
            EffectHost::edit_effect(&self.downgrade(), label, merge_key, &key, change);
        }
    }

    fn item(&self) -> Option<Item> {
        let timeline = self.imp().timeline.get()?;
        let project = timeline.project();
        let model = project.timeline();
        match model.item(timeline.selection()?).ok()? {
            Composable::Clip(clip) => Some(clip.item.clone()),
            _ => None,
        }
    }

    fn refresh(&self) {
        let imp = self.imp();
        let (title, enabled, grid) = (
            imp.title.get().expect("built"),
            imp.enabled.get().expect("built"),
            imp.grid.get().expect("built"),
        );
        let key = imp.key.borrow().clone();
        let item = self.item();
        let effect = key
            .as_deref()
            .zip(item.as_ref())
            .and_then(|(key, item)| item.effect(key));
        imp.updating.set(true);
        let Some(effect) = effect else {
            title.set_text(&match key.as_deref() {
                Some(SOURCE) => gettext("Source: the clip's picture"),
                Some(OUTPUT) => gettext("Output: what's composited"),
                _ => gettext("Select a node to change it."),
            });
            enabled.set_visible(false);
            while let Some(child) = grid.first_child() {
                grid.remove(&child);
            }
            imp.shown.replace(None);
            imp.controls.borrow_mut().clear();
            imp.updating.set(false);
            return;
        };
        let key = key.expect("an effect");
        let def = nox_fx::find(&effect.effect_name);
        let shape = Some((key.clone(), effect.effect_name.clone()));
        if *imp.shown.borrow() != shape {
            while let Some(child) = grid.first_child() {
                grid.remove(&child);
            }
            let name = match def {
                Some(def) => gettext(def.label),
                None => fill(
                    &gettext("{name} (not supported)"),
                    &[("name", &effect.name)],
                ),
            };
            title.set_text(&name);
            let host: Rc<dyn EffectHost> = Rc::new(self.downgrade());
            let controls = match def {
                Some(def) => param_controls(&host, grid, &key, &name, def),
                None => Vec::new(),
            };
            imp.controls.replace(controls);
            imp.def.set(def);
            imp.shown.replace(shape);
        }
        enabled.set_visible(true);
        enabled.set_active(effect.enabled);
        if let Some(def) = imp.def.get() {
            let time = imp
                .timeline
                .get()
                .and_then(TimelineView::selected_media_time)
                .unwrap_or_default();
            update_controls(&imp.controls.borrow(), def, effect, time);
        }
        imp.updating.set(false);
    }
}
