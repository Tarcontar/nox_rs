//! The timeline's input: pointer gestures (drag, click, context menu),
//! scrolling, keys, drops from the media pool, and tooltips.

use super::*;

impl imp::TimelineView {
    /// Connects the gestures and controllers. Called once, on construction.
    pub(super) fn set_up_input(&self) {
        let obj = self.obj();
        obj.set_focusable(true);
        obj.set_overflow(gtk::Overflow::Hidden);
        obj.update_property(&[
            gtk::accessible::Property::Label(&crate::i18n::gettext("Timeline")),
            gtk::accessible::Property::Description(&crate::i18n::gettext(
                "Left and Right step one frame, Up and Down jump to edit points, \
                 Shift+V selects the clip at the playhead, Ctrl+A all clips and \
                 Escape none, Shift+F10 opens the clip menu (in a track header, the \
                 track menu), Delete removes the \
                 selected clips.",
            )),
        ]);

        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak]
            obj,
            move |gesture, x, y| {
                let state = gesture.current_event_state();
                obj.imp().drag_begin(
                    x,
                    y,
                    state.contains(gdk::ModifierType::CONTROL_MASK),
                    state.contains(gdk::ModifierType::SHIFT_MASK),
                    state.contains(gdk::ModifierType::ALT_MASK),
                )
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak]
            obj,
            move |gesture, dx, dy| {
                if let Some((x, y)) = gesture.start_point() {
                    obj.imp().drag_update(x, y, dx, dy);
                }
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak]
            obj,
            move |_, _, _| obj.imp().drag_end()
        ));
        obj.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak]
            obj,
            move |_, x, y| obj.imp().hover(x, y)
        ));
        obj.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll(glib::clone!(
            #[weak]
            obj,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, dx, dy| {
                let zoom = controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK);
                obj.imp().on_scroll(zoom, dx, dy);
                glib::Propagation::Stop
            }
        ));
        obj.add_controller(scroll);

        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Shift>Delete|<Shift>BackSpace"),
            Some(gtk::NamedAction::new("win.ripple-delete")),
        ));
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Delete|BackSpace"),
            Some(gtk::CallbackAction::new(|widget, _| {
                let view = widget
                    .downcast_ref::<super::TimelineView>()
                    .expect("timeline");
                if view.selected_marker().is_some() {
                    view.delete_selected_marker();
                } else {
                    view.delete_selection();
                }
                glib::Propagation::Stop
            })),
        ));
        // Through the window action, so the menu shows the state.
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("n"),
            Some(gtk::NamedAction::new("win.snapping")),
        ));
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>a"),
            Some(gtk::CallbackAction::new(|widget, _| {
                let view = widget
                    .downcast_ref::<super::TimelineView>()
                    .expect("timeline");
                view.select_all_clips();
                glib::Propagation::Stop
            })),
        ));
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(|widget, _| {
                let view = widget
                    .downcast_ref::<super::TimelineView>()
                    .expect("timeline");
                view.set_selection(None);
                glib::Propagation::Stop
            })),
        ));
        // The clip menu without a mouse.
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Shift>F10|Menu"),
            Some(gtk::CallbackAction::new(|widget, _| {
                let view = widget
                    .downcast_ref::<super::TimelineView>()
                    .expect("timeline");
                view.imp().selection_menu();
                glib::Propagation::Stop
            })),
        ));
        obj.add_controller(shortcuts);

        let drop = gtk::DropTarget::new(MediaObject::static_type(), gdk::DragAction::COPY);
        drop.connect_motion(glib::clone!(
            #[weak]
            obj,
            #[upgrade_or]
            gdk::DragAction::empty(),
            move |_, x, y| {
                let imp = obj.imp();
                imp.drop_hint
                    .set(Some((imp.track_at_y(y).unwrap_or(0), imp.drop_frame(x))));
                obj.queue_draw();
                gdk::DragAction::COPY
            }
        ));
        drop.connect_leave(glib::clone!(
            #[weak]
            obj,
            move |_| {
                obj.imp().drop_hint.set(None);
                obj.queue_draw();
            }
        ));
        drop.connect_drop(glib::clone!(
            #[weak]
            obj,
            #[upgrade_or]
            false,
            move |target, value, x, y| {
                let imp = obj.imp();
                imp.drop_hint.set(None);
                // Ctrl inserts (pushing what follows), as in Premiere.
                let insert = target
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK);
                obj.queue_draw();
                let Ok(media) = value.get::<MediaObject>() else {
                    return false;
                };
                let Some(info) = media.info() else {
                    tracing::info!("{} is still loading", media.name());
                    return false;
                };
                let frame = imp.drop_frame(x);
                crate::framing::add_media(
                    &obj,
                    imp.project(),
                    info,
                    (imp.track_at_y(y), frame, insert),
                    None,
                    |_, _| {},
                );
                true
            }
        ));
        obj.add_controller(drop);

        let menu = gtk::PopoverMenu::from_model(Some(&clip_menu()));
        menu.set_parent(&*obj);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        self.menu.set(menu).expect("menu set once");
        let ruler = gtk::PopoverMenu::from_model(Some(&ruler_menu()));
        ruler.set_parent(&*obj);
        ruler.set_has_arrow(false);
        ruler.set_halign(gtk::Align::Start);
        self.ruler_menu.set(ruler).expect("ruler menu set once");
        let context = gtk::GestureClick::builder().button(3).build();
        context.connect_pressed(glib::clone!(
            #[weak]
            obj,
            move |_, _, x, y| obj.imp().context_menu(x, y)
        ));
        obj.add_controller(context);

        let (video, audio) = track_menus();
        let track_menu = |model: &gio::MenuModel| {
            let menu = gtk::PopoverMenu::from_model(Some(model));
            menu.set_parent(&*obj);
            menu.set_has_arrow(false);
            menu.set_halign(gtk::Align::Start);
            menu
        };
        self.track_menus
            .set((track_menu(&video), track_menu(&audio)))
            .expect("menus set once");
        self.add_track_actions();

        obj.set_has_tooltip(true);
        obj.connect_query_tooltip(|obj, x, y, _, tooltip| {
            let imp = obj.imp();
            let hit = imp.hit(x.into(), y.into());
            if let Hit::Marker(index) = hit {
                let project = imp.project();
                let timeline = project.timeline();
                let Some(marker) = timeline.markers().get(index) else {
                    return false;
                };
                let mut text = if marker.name.is_empty() {
                    crate::i18n::gettext("Marker")
                } else {
                    marker.name.clone()
                };
                if !marker.comment.is_empty() {
                    text = format!("{text}\n{}", marker.comment); // no-i18n
                }
                tooltip.set_text(Some(&text));
                return true;
            }
            false
        });
    }
}
