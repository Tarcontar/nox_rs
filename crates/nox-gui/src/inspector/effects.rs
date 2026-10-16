//! The inspector's effect stack: a row per effect of the selected clip
//! (an expander with an on/off check, a menu, and the parameters'
//! controls from `effect_controls`), and the "Add effect" list.
//!
//! Only plain stacks are shown here (`nox_fx::is_linear`): clips whose
//! effects are wired as a graph (mattes, masks, other outputs) are edited
//! on the Effects page, and the section says so. Stack edits keep stacks
//! plain (`nox_fx::simplify`).
//!
//! Rows are rebuilt only when the stack's shape changes (effects added,
//! removed or reordered) and otherwise updated in place, so a slider being
//! dragged isn't replaced under the pointer.

use gtk::{gio, glib, prelude::*, subclass::prelude::*};
use nox_fx::EffectDef;
use nox_project::{Effect, Item};
use std::rc::Rc;

use super::Inspector;
use crate::effect_controls::{
    Control, EffectHost, effect_list_popover, param_controls, update_controls,
};
use crate::i18n::{fill, gettext};

/// An effect's row.
pub struct EffectRow {
    key: String,
    effect_name: String,
    def: Option<&'static EffectDef>,
    enabled: gtk::CheckButton,
    controls: Vec<Control>,
}

/// The shape shown for clips whose effects are a graph.
const GRAPH: &str = "graph";

/// The inspector, as controls edit through it.
impl EffectHost for glib::WeakRef<Inspector> {
    fn updating(&self) -> bool {
        self.upgrade().is_none_or(|i| i.imp().updating.get())
    }

    fn media_time(&self) -> f64 {
        self.upgrade()
            .and_then(|i| i.media_time())
            .unwrap_or_default()
    }

    fn edit_effect(
        &self,
        label: &str,
        merge_key: Option<&str>,
        key: &str,
        change: &dyn Fn(&mut Effect),
    ) {
        if let Some(inspector) = self.upgrade() {
            inspector.edit_effect(label, merge_key, key, change);
        }
    }

    fn models_ready(&self, choice: &'static str, then: Rc<dyn Fn()>) -> bool {
        let Some(inspector) = self.upgrade() else {
            return true;
        };
        let missing = inspector.missing_models(choice);
        if missing.is_empty() {
            return true;
        }
        // The control goes back to what's stored until the models are there.
        inspector.refresh();
        inspector.offer_model_download(missing, Rc::new(move |_: &Inspector| then()));
        false
    }
}

impl Inspector {
    /// Builds the "Add effect" list (effects that fit a stack) into its
    /// button.
    pub(super) fn setup_effects(&self) {
        let popover = effect_list_popover(
            EffectDef::in_stack,
            glib::clone!(
                #[weak(rename_to = inspector)]
                self,
                move |def| inspector.add_effect(def)
            ),
        );
        self.imp().add_effect.set_popover(Some(&popover));
    }

    /// Adds `def` to the selected clips, or offers to download its models
    /// first and adds it then.
    pub(crate) fn add_effect(&self, def: &'static EffectDef) {
        self.with_models(
            def,
            Rc::new(move |inspector: &Inspector| inspector.push_effect(def)),
        );
    }

    /// Calls `then` once `def`'s models (if it has any) are downloaded:
    /// at once, or after offering them.
    pub(crate) fn with_models(&self, def: &'static EffectDef, then: Rc<dyn Fn(&Inspector)>) {
        if def.task.is_some() {
            let missing = self.missing_models("auto");
            if !missing.is_empty() {
                self.offer_model_download(missing, then);
                return;
            }
        }
        then(self);
    }

    fn push_effect(&self, def: &'static EffectDef) {
        self.imp().expand_added_effect.set(true);
        let label = fill(&gettext("Add {effect}"), &[("effect", &gettext(def.label))]);
        self.edit(&label, None, move |item| {
            item.add_effect(Effect::rendering(def.id, def.label));
            nox_fx::simplify(item);
        });
    }

    /// Changes effect `key` of the selected clips.
    fn edit_effect(
        &self,
        label: &str,
        merge_key: Option<&str>,
        key: &str,
        change: &dyn Fn(&mut Effect),
    ) {
        let key = key.to_owned();
        self.edit(label, merge_key, move |item| {
            if let Some(effect) = item.effect_mut(&key) {
                change(effect);
            }
        });
    }

    /// Shows `item`'s effects at media time `time` (while `updating`).
    pub(super) fn show_effects(&self, item: &Item, time: f64) {
        let imp = self.imp();
        let linear = nox_fx::is_linear(item);
        let shape: Vec<(String, String)> = if linear {
            item.rendering_effects()
                .map(|(key, effect)| (key, effect.effect_name.clone()))
                .collect()
        } else {
            vec![(GRAPH.into(), GRAPH.into())]
        };
        let shown: Vec<(String, String)> = imp
            .effect_rows
            .borrow()
            .iter()
            .map(|row| (row.key.clone(), row.effect_name.clone()))
            .collect();
        imp.add_effect.set_visible(linear);
        if shape != shown {
            let list = imp.effects_list.get();
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            if !linear {
                list.append(&graph_note());
                *imp.effect_rows.borrow_mut() = vec![EffectRow {
                    key: GRAPH.into(),
                    effect_name: GRAPH.into(),
                    def: None,
                    enabled: gtk::CheckButton::new(),
                    controls: Vec::new(),
                }];
                return;
            }
            let added = imp.expand_added_effect.replace(false);
            let count = shape.len();
            let rows = item
                .rendering_effects()
                .enumerate()
                .map(|(i, (key, effect))| {
                    if added && i + 1 == count {
                        imp.expanded_effects.borrow_mut().insert(key.clone());
                    }
                    self.effect_row(&key, effect, i, count)
                })
                .collect();
            *imp.effect_rows.borrow_mut() = rows;
        }
        if !linear {
            return;
        }
        for row in imp.effect_rows.borrow().iter() {
            if let Some(effect) = item.effect(&row.key) {
                row.enabled.set_active(effect.enabled);
                if let Some(def) = row.def {
                    update_controls(&row.controls, def, effect, time);
                }
            }
        }
    }

    /// The row of effect `key` (`index` of `count`), added to the list.
    fn effect_row(&self, key: &str, effect: &Effect, index: usize, count: usize) -> EffectRow {
        let imp = self.imp();
        let def = nox_fx::find(&effect.effect_name);
        let title = match def {
            Some(def) => gettext(def.label),
            None => fill(
                &gettext("{name} (not supported)"),
                &[("name", &effect.name)],
            ),
        };
        let enabled = gtk::CheckButton::new();
        enabled.set_tooltip_text(Some(&gettext("Turns the effect on or off")));
        let name = gtk::Label::new(Some(&title));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        enabled.update_relation(&[gtk::accessible::Relation::LabelledBy(&[name.upcast_ref()])]);
        let menu = gio::Menu::new();
        if index > 0 {
            menu.append(Some(&gettext("Move Up")), Some("effect.up"));
        }
        if index + 1 < count {
            menu.append(Some(&gettext("Move Down")), Some("effect.down"));
        }
        menu.append(Some(&gettext("Duplicate")), Some("effect.duplicate"));
        if def.is_some_and(|d| !d.params.is_empty()) {
            menu.append(Some(&gettext("Reset")), Some("effect.reset"));
        }
        menu.append(Some(&gettext("Remove")), Some("effect.remove"));
        let menu_button = gtk::MenuButton::new();
        menu_button.set_icon_name("view-more-symbolic");
        menu_button.set_menu_model(Some(&menu));
        menu_button.add_css_class("flat");
        menu_button.set_tooltip_text(Some(&gettext("Effect actions")));
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&enabled);
        header.append(&name);
        header.append(&menu_button);
        let expander = gtk::Expander::new(None);
        expander.set_label_widget(Some(&header));
        expander.set_expanded(imp.expanded_effects.borrow().contains(key));
        let row_key = key.to_owned();
        expander.connect_expanded_notify(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |expander| {
                let mut expanded = inspector.imp().expanded_effects.borrow_mut();
                if expander.is_expanded() {
                    expanded.insert(row_key.clone());
                } else {
                    expanded.remove(&row_key);
                }
            }
        ));
        expander.insert_action_group("effect", Some(&self.effect_actions(key, &title)));
        let key_owned = key.to_owned();
        enabled.connect_toggled(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |check| {
                if inspector.imp().updating.get() {
                    return;
                }
                let on = check.is_active();
                let label = if on {
                    gettext("Turn Effect On")
                } else {
                    gettext("Turn Effect Off")
                };
                inspector.edit_effect(&label, None, &key_owned, &|e| e.enabled = on);
            }
        ));
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(8);
        grid.set_margin_top(6);
        grid.set_margin_start(6);
        let host: Rc<dyn EffectHost> = Rc::new(self.downgrade());
        let controls = match def {
            Some(def) => param_controls(&host, &grid, key, &title, def),
            None => {
                let note = gtk::Label::new(Some(&gettext(
                    "Nox can't render this effect; it stays in the project.",
                )));
                note.set_wrap(true);
                note.set_xalign(0.0);
                note.add_css_class("dim-label");
                grid.attach(&note, 0, 0, 4, 1);
                Vec::new()
            }
        };
        expander.set_child(Some(&grid));
        imp.effects_list.append(&expander);
        EffectRow {
            key: key.to_owned(),
            effect_name: effect.effect_name.clone(),
            def,
            enabled,
            controls,
        }
    }

    /// The menu's actions on effect `key` (called `title`).
    fn effect_actions(&self, key: &str, title: &str) -> gio::SimpleActionGroup {
        /// An action's change to the item holding effect `key`.
        type Change = fn(&mut Item, &str);
        let group = gio::SimpleActionGroup::new();
        let actions: [(&str, String, Change); 5] = [
            ("up", gettext("Move Effect"), |item, key| {
                item.move_effect(key, -1);
            }),
            ("down", gettext("Move Effect"), |item, key| {
                item.move_effect(key, 1);
            }),
            (
                "duplicate",
                fill(&gettext("Duplicate {effect}"), &[("effect", &title)]),
                |item, key| {
                    item.duplicate_effect(key);
                },
            ),
            (
                "reset",
                fill(&gettext("Reset {effect}"), &[("effect", &title)]),
                |item, key| {
                    if let Some(effect) = item.effect_mut(key) {
                        effect.set_nox_value("params", None);
                        effect.set_nox_value("keyframes", None);
                    }
                },
            ),
            (
                "remove",
                fill(&gettext("Remove {effect}"), &[("effect", &title)]),
                |item, key| {
                    item.remove_effect(key);
                },
            ),
        ];
        for (name, label, change) in actions {
            let action = gio::SimpleAction::new(name, None);
            let key = key.to_owned();
            action.connect_activate(glib::clone!(
                #[weak(rename_to = inspector)]
                self,
                move |_, _| {
                    inspector.edit(&label, None, |item| {
                        change(item, &key);
                        // Inputs written out (by the Effects page) but
                        // the same as a stack's go again.
                        nox_fx::simplify(item);
                    })
                }
            ));
            group.add_action(&action);
        }
        group
    }
}

/// Says the clip's effects are a graph, with a way to the Effects page.
fn graph_note() -> gtk::Box {
    let note = gtk::Label::new(Some(&gettext(
        "These effects are wired as a graph and edited on the Effects page.",
    )));
    note.set_wrap(true);
    note.set_xalign(0.0);
    note.add_css_class("dim-label");
    let button = gtk::Button::with_label(&gettext("Open Effects Page"));
    button.set_halign(gtk::Align::Start);
    button.set_action_name(Some("win.page"));
    button.set_action_target_value(Some(&"effects".to_variant()));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&note);
    content.append(&button);
    content
}
