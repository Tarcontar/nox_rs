use gtk::{gdk, gio, glib, prelude::*};
use nox_engine::Preview;
use nox_project::{Time, timecode};
use std::cell::RefCell;
use std::rc::Rc;

use crate::media_object::MediaObject;
use crate::{media_pool, timeline::TimelineView, viewer::Viewer};

pub fn build(app: &gtk::Application, files: &[gio::File]) -> gtk::ApplicationWindow {
    let header = gtk::HeaderBar::new();

    let pool = media_pool::MediaPool::new();
    for file in files {
        pool.import(file);
    }
    let viewer = Rc::new(Viewer::new());
    let timeline = TimelineView::new();
    connect_preview(&pool, &viewer, &timeline);
    pool.select_first();

    let top = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&pool.widget)
        .end_child(&viewer.widget)
        .resize_start_child(false)
        .shrink_start_child(false)
        .position(340)
        .build();

    let main = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&top)
        .end_child(&timeline)
        .shrink_end_child(false)
        .position(500)
        .build();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Nox")
        .default_width(1400)
        .default_height(860)
        .titlebar(&header)
        .child(&main)
        .build();
    add_transport_actions(&window, &timeline);
    window
}

/// `win.seek(frame)`, `win.next-frame`, `win.previous-frame` and
/// `win.go-to-start`, used by the transport buttons and shortcuts (and
/// reachable over D-Bus via org.gtk.Actions, which tests use).
fn add_transport_actions(window: &gtk::ApplicationWindow, timeline: &TimelineView) {
    let seek = gio::ActionEntry::builder("seek")
        .parameter_type(Some(&i64::static_variant_type()))
        .activate(glib::clone!(
            #[weak]
            timeline,
            move |_: &gtk::ApplicationWindow, _, parameter| {
                if let Some(frame) = parameter.and_then(|p| p.get::<i64>()) {
                    timeline.set_playhead(frame.max(0));
                }
            }
        ))
        .build();
    let step = |name: &str, delta: i64| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                timeline,
                move |_: &gtk::ApplicationWindow, _, _| {
                    timeline.set_playhead((timeline.playhead() + delta).max(0));
                }
            ))
            .build()
    };
    let go_to_start = gio::ActionEntry::builder("go-to-start")
        .activate(glib::clone!(
            #[weak]
            timeline,
            move |_: &gtk::ApplicationWindow, _, _| timeline.set_playhead(0)
        ))
        .build();
    window.add_action_entries([
        seek,
        step("previous-frame", -1),
        step("next-frame", 1),
        go_to_start,
    ]);
}

/// DRM modifiers GTK can import for RGBA8 frames, i.e. whether the engine
/// may hand over frames as DMA-BUF. `NOX_PRESENT=readback` disables it.
fn importable_modifiers() -> Vec<u64> {
    if std::env::var("NOX_PRESENT").is_ok_and(|v| v == "readback") {
        tracing::info!("NOX_PRESENT=readback: not using DMA-BUF");
        return Vec::new();
    }
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let formats = display.dmabuf_formats();
    (0..formats.n_formats())
        .map(|i| formats.format(i))
        .filter(|&(fourcc, _)| fourcc == nox_engine::FOURCC_ABGR8888)
        .map(|(_, modifier)| modifier)
        .collect()
}

/// Until clips live on the timeline (M3), the viewer shows the clip selected
/// in the media pool at the playhead position.
fn connect_preview(pool: &media_pool::MediaPool, viewer: &Rc<Viewer>, timeline: &TimelineView) {
    let (sender, receiver) = async_channel::unbounded();
    let preview = Rc::new(Preview::new(importable_modifiers(), move |frame| {
        let _ = sender.send_blocking(frame);
    }));
    let source: Rc<RefCell<Option<MediaObject>>> = Rc::default();

    glib::spawn_future_local(glib::clone!(
        #[strong]
        viewer,
        #[strong]
        preview,
        #[strong]
        source,
        async move {
            while let Ok(result) = receiver.recv().await {
                match result {
                    // Drop frames that arrive after the selection changed.
                    Ok(frame)
                        if source
                            .borrow()
                            .as_ref()
                            .is_some_and(|s| s.path() == *frame.path.to_string_lossy()) =>
                    {
                        let (path, index) = (frame.path.clone(), frame.index);
                        if let Err(err) = viewer.show_frame(frame.image) {
                            tracing::warn!(
                                "GTK cannot import DMA-BUF frames ({err}), falling back to readback"
                            );
                            preview.disable_dmabuf();
                            preview.request(&path, index);
                        }
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!("preview: {err}"),
                }
            }
        }
    ));

    let request = Rc::new(glib::clone!(
        #[strong]
        preview,
        #[strong]
        source,
        #[strong]
        viewer,
        #[weak]
        timeline,
        move || {
            let Some(media) = source.borrow().clone() else {
                viewer.clear();
                return;
            };
            let Some(info) = media.info() else { return }; // still loading
            let Some(video) = &info.video else {
                viewer.clear();
                return;
            };
            let index = match video.frame_rate {
                Some(fps) => {
                    Time::new(timeline.playhead(), timeline.fps().inverse())
                        .rescale(fps.inverse())
                        .ticks
                }
                None => 0, // still image
            };
            preview.request(&info.path, index);
        }
    ));

    timeline.connect_playhead_changed(glib::clone!(
        #[strong]
        viewer,
        #[strong]
        request,
        move |timeline, frame| {
            viewer.timecode.set_text(&timecode(frame, timeline.fps()));
            request();
        }
    ));

    pool.connect_selected(move |media| {
        if let Some(media) = &media {
            // Items still loading request their frame once probed.
            media.connect_notify_local(
                Some("summary"),
                glib::clone!(
                    #[strong]
                    request,
                    #[strong]
                    source,
                    move |media, _| {
                        if source.borrow().as_ref() == Some(media) {
                            request();
                        }
                    }
                ),
            );
        }
        source.replace(media);
        request();
    });
}
