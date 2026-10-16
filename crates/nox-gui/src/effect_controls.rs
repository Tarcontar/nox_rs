//! Controls for effects, shared by the inspector's stack and the Effects
//! page's node panel: a control per parameter of nox-fx's registry (sliders
//! with keyframe toggles, checks, drop-downs, color buttons), and the list
//! effects are added from. Edits go through an `EffectHost`.

use gtk::{gdk, glib, prelude::*};
use nox_fx::{EffectDef, ParamKind, ParamValue, Unit};
use nox_project::{Effect, Value};
use std::rc::Rc;

use crate::i18n::{fill, gettext};
use crate::ui_util::string_list;

/// What controls edit through.
pub(crate) trait EffectHost {
    /// Whether the controls are being set from the model (their changes
    /// aren't edits then).
    fn updating(&self) -> bool;
    /// The selected clip's media time at the playhead (for keyframes).
    fn media_time(&self) -> f64;
    /// Changes effect `key` of the selected clips.
    fn edit_effect(
        &self,
        label: &str,
        merge_key: Option<&str>,
        key: &str,
        change: &dyn Fn(&mut Effect),
    );
    /// Whether the models for `choice` (an AI effect's "model") are there;
    /// if not, the host may offer them and call `then` once they are.
    fn models_ready(&self, _choice: &'static str, _then: Rc<dyn Fn()>) -> bool {
        true
    }
}

/// A parameter's control.
pub(crate) enum Control {
    Number {
        adjustment: gtk::Adjustment,
        keyframe: gtk::ToggleButton,
    },
    Bool(gtk::CheckButton),
    Choice(gtk::DropDown),
    Color(gtk::ColorDialogButton),
}

/// Controls for every parameter of `def` on effect `key` (called
/// `title`), added to `grid` from row 0.
pub(crate) fn param_controls(
    host: &Rc<dyn EffectHost>,
    grid: &gtk::Grid,
    key: &str,
    title: &str,
    def: &'static EffectDef,
) -> Vec<Control> {
    let mut line = 0;
    def.params
        .iter()
        .map(|param| param_control(host, grid, &mut line, key, title, def, param))
        .collect()
}

/// Shows `effect`'s values at media time `time` in `controls` (made by
/// `param_controls` for `def`).
pub(crate) fn update_controls(
    controls: &[Control],
    def: &'static EffectDef,
    effect: &Effect,
    time: f64,
) {
    let values = nox_fx::values(effect, def, time);
    for ((param, value), control) in def.params.iter().zip(values).zip(controls) {
        match (control, value) {
            (
                Control::Number {
                    adjustment,
                    keyframe,
                },
                ParamValue::Number(v),
            ) => {
                adjustment.set_value(v);
                keyframe.set_active(effect.has_keyframe(param.name, time));
                if effect.keyframes(param.name).is_empty() {
                    keyframe.remove_css_class("animated");
                } else {
                    keyframe.add_css_class("animated");
                }
            }
            (Control::Bool(check), ParamValue::Bool(on)) => check.set_active(on),
            (Control::Choice(dropdown), ParamValue::Choice(choice)) => {
                if let ParamKind::Choice { options, .. } = param.kind
                    && let Some(i) = options.iter().position(|(c, _)| *c == choice)
                {
                    dropdown.set_selected(i as u32);
                }
            }
            (Control::Color(button), ParamValue::Color([r, g, b])) => {
                let rgba = gdk::RGBA::new(r as f32, g as f32, b as f32, 1.0);
                if button.rgba() != rgba {
                    button.set_rgba(&rgba);
                }
            }
            _ => {}
        }
    }
}

/// Adds `param`'s control of effect `key` (called `title`) to `grid` at
/// `line`, moving it on.
fn param_control(
    host: &Rc<dyn EffectHost>,
    grid: &gtk::Grid,
    line: &mut i32,
    key: &str,
    title: &str,
    def: &'static EffectDef,
    param: &'static nox_fx::ParamDef,
) -> Control {
    let label = gtk::Label::new(Some(&gettext(param.label)));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    // Long names wrap rather than widen the panel.
    label.set_wrap(true);
    label.set_max_width_chars(12);
    let labelled = [gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])];
    let name = param.name;
    let undo = fill(&gettext("Change {effect}"), &[("effect", &title)]);
    let merge = format!("effect:{key}:{name}");
    let key = key.to_owned();
    let host = host.clone();
    match param.kind {
        ParamKind::Number {
            min,
            max,
            default,
            digits,
            unit,
        } => {
            let step = 10f64.powi(-(digits as i32));
            let adjustment = gtk::Adjustment::new(default, min, max, step, step * 10.0, 0.0);
            let spin = gtk::SpinButton::new(Some(&adjustment), step, digits);
            spin.set_width_chars(4);
            let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adjustment));
            scale.set_draw_value(false);
            scale.set_hexpand(true);
            let unit = match unit {
                Unit::Plain => None,
                Unit::Percent => Some(gettext("In percent")),
                Unit::PictureHeight => Some(gettext("In percent of the picture's height")),
            };
            spin.set_tooltip_text(unit.as_deref());
            scale.set_tooltip_text(unit.as_deref());
            spin.update_relation(&labelled);
            scale.update_relation(&labelled);
            let keyframe = gtk::ToggleButton::new();
            keyframe.set_icon_name("nox-keyframe-symbolic");
            keyframe.set_tooltip_text(Some(&gettext("Keyframe at the playhead")));
            keyframe.add_css_class("keyframe");
            keyframe.add_css_class("flat");
            keyframe.update_relation(&labelled);
            grid.attach(&label, 0, *line, 2, 1);
            grid.attach(&spin, 2, *line, 1, 1);
            grid.attach(&keyframe, 3, *line, 1, 1);
            grid.attach(&scale, 0, *line + 1, 3, 1);
            *line += 2;
            let (value_host, value_key) = (host.clone(), key.clone());
            adjustment.connect_value_changed(move |adjustment| {
                if value_host.updating() {
                    return;
                }
                let value = adjustment.value();
                let time = value_host.media_time();
                value_host.edit_effect(&undo, Some(&merge), &value_key, &|e| {
                    e.set_number_at(name, time, value)
                });
            });
            keyframe.connect_toggled(move |_| {
                if host.updating() {
                    return;
                }
                let time = host.media_time();
                host.edit_effect(&gettext("Toggle Keyframe"), None, &key, &|e| {
                    e.toggle_keyframe(name, time, default)
                });
            });
            Control::Number {
                adjustment,
                keyframe,
            }
        }
        ParamKind::Bool { .. } => {
            let check = gtk::CheckButton::with_label(&gettext(param.label));
            grid.attach(&check, 0, *line, 4, 1);
            *line += 1;
            check.connect_toggled(move |check| {
                if host.updating() {
                    return;
                }
                let on = check.is_active();
                host.edit_effect(&undo, None, &key, &|e| {
                    e.set_param(name, Some(Value::Bool(on)))
                });
            });
            Control::Bool(check)
        }
        ParamKind::Choice { options, .. } => {
            let dropdown = gtk::DropDown::new(
                Some(string_list(
                    options.iter().map(|(_, label)| gettext(*label)),
                )),
                gtk::Expression::NONE,
            );
            dropdown.update_relation(&labelled);
            grid.attach(&label, 0, *line, 2, 1);
            grid.attach(&dropdown, 2, *line, 2, 1);
            *line += 1;
            let ai = def.task.is_some();
            dropdown.connect_selected_notify(move |dropdown| {
                if host.updating() {
                    return;
                }
                let Some(&(choice, _)) = options.get(dropdown.selected() as usize) else {
                    return;
                };
                let set: Rc<dyn Fn()> = {
                    let (host, undo, key) = (host.clone(), undo.clone(), key.clone());
                    Rc::new(move || {
                        host.edit_effect(&undo, None, &key, &|e| {
                            e.set_param(name, Some(Value::from(choice)))
                        });
                    })
                };
                // A model choice may need another model downloaded.
                if !ai || host.models_ready(choice, set.clone()) {
                    set();
                }
            });
            Control::Choice(dropdown)
        }
        ParamKind::Color { .. } => {
            let button = gtk::ColorDialogButton::new(Some(
                gtk::ColorDialog::builder()
                    .with_alpha(false)
                    .title(gettext(param.label))
                    .build(),
            ));
            button.update_relation(&labelled);
            grid.attach(&label, 0, *line, 2, 1);
            grid.attach(&button, 2, *line, 2, 1);
            *line += 1;
            button.connect_rgba_notify(move |button| {
                if host.updating() {
                    return;
                }
                let rgba = button.rgba();
                let rgb = [rgba.red(), rgba.green(), rgba.blue()].map(f64::from);
                host.edit_effect(&undo, None, &key, &|e| {
                    e.set_param(name, Some(ParamValue::Color(rgb).to_value()))
                });
            });
            Control::Color(button)
        }
    }
}

/// A popover listing the effects `show` lets through, by category, with a
/// search field; `pick` gets the one chosen.
pub(crate) fn effect_list_popover(
    show: fn(&EffectDef) -> bool,
    pick: impl Fn(&'static EffectDef) + 'static,
) -> gtk::Popover {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    let defs: Vec<&'static EffectDef> = nox_fx::effects().iter().filter(|d| show(d)).collect();
    for def in &defs {
        let label = gtk::Label::new(Some(&gettext(def.label)));
        label.set_xalign(0.0);
        label.set_margin_start(6);
        label.set_margin_end(6);
        list.append(&label);
    }
    let defs = Rc::new(defs);
    let def_of = {
        let defs = defs.clone();
        move |row: &gtk::ListBoxRow| defs.get(usize::try_from(row.index()).ok()?).copied()
    };
    // A heading where the category changes.
    let header_def = def_of.clone();
    list.set_header_func(move |row, before| {
        let category = header_def(row).map(|d| d.category);
        if before.and_then(&header_def).map(|d| d.category) == category {
            row.set_header(gtk::Widget::NONE);
            return;
        }
        let heading = gtk::Label::new(category.map(|c| gettext(c.label())).as_deref());
        heading.set_xalign(0.0);
        heading.set_margin_top(6);
        heading.set_margin_start(6);
        heading.add_css_class("heading");
        row.set_header(Some(&heading));
    });
    let search = gtk::SearchEntry::new();
    let text = search.clone();
    let filter_def = def_of.clone();
    list.set_filter_func(move |row| {
        let wanted = text.text().to_lowercase();
        filter_def(row).is_some_and(|def| gettext(def.label).to_lowercase().contains(&wanted))
    });
    search.connect_search_changed(glib::clone!(
        #[weak]
        list,
        move |_| list.invalidate_filter()
    ));
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_child(Some(&list));
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(320);
    scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&search);
    content.append(&scroller);
    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));
    popover.connect_show(glib::clone!(
        #[weak]
        search,
        move |_| {
            search.set_text("");
            search.grab_focus();
        }
    ));
    list.connect_row_activated(glib::clone!(
        #[weak]
        popover,
        move |_, row| {
            popover.popdown();
            if let Some(def) = def_of(row) {
                pick(def);
            }
        }
    ));
    // Enter takes the first effect left.
    search.connect_activate(glib::clone!(
        #[weak]
        list,
        move |_| {
            let mut child = list.first_child();
            while let Some(widget) = child {
                if let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>()
                    && row.is_child_visible()
                {
                    row.activate();
                    return;
                }
                child = widget.next_sibling();
            }
        }
    ));
    popover
}
