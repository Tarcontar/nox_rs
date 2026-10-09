//! Connects the engine to the viewer and timeline: shows the sequence at
//! the playhead, and plays it in sync with the display.
//!
//! While paused, playhead moves request a frame (instant if cached). While
//! playing, a tick callback runs on every display refresh: it reads the
//! engine's clock (the audio device, if there is one), moves the playhead
//! and shows the matching cached frame. Frames that aren't ready in time
//! are skipped and counted.

use gtk::{gdk, glib, prelude::*};
use nox_engine::{Engine, EngineEvent, RenderedFrame};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::project::Project;
use crate::timeline::TimelineView;
use crate::viewer::Viewer;

pub struct Playback {
    engine: Engine,
    project: Rc<Project>,
    viewer: Viewer,
    timeline: TimelineView,
    /// Frame on screen.
    shown: Cell<Option<i64>>,
    /// Set while playback itself moves the playhead.
    ticking: Cell<bool>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    stats: Cell<Stats>,
}

impl Playback {
    pub fn new(project: &Rc<Project>, viewer: &Viewer, timeline: &TimelineView) -> Rc<Self> {
        let (sender, receiver) = async_channel::unbounded();
        let engine = Engine::new(importable_modifiers(), move |event| {
            let _ = sender.send_blocking(event);
        });
        let playback = Rc::new(Self {
            engine,
            project: project.clone(),
            viewer: viewer.clone(),
            timeline: timeline.clone(),
            shown: Cell::new(None),
            ticking: Cell::new(false),
            tick: RefCell::default(),
            stats: Cell::new(Stats::default()),
        });
        playback.sync_timeline();

        let weak = Rc::downgrade(&playback);
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                let Some(playback) = weak.upgrade() else {
                    break;
                };
                match event {
                    EngineEvent::Frame(rendered) => {
                        // Late results of earlier requests are dropped.
                        if !playback.engine.is_playing()
                            && rendered.frame == playback.timeline.playhead()
                        {
                            playback.show(&rendered);
                        }
                    }
                    EngineEvent::Error { frame, message } => {
                        tracing::warn!("cannot render {message}");
                        if frame == playback.timeline.playhead() {
                            playback.viewer.clear();
                            playback.shown.set(None);
                        }
                    }
                }
            }
        });

        timeline.connect_playhead_changed(glib::clone!(
            #[weak]
            playback,
            move |_, frame| {
                playback
                    .viewer
                    .set_timecode(&playback.project.timecode(frame));
                if playback.ticking.get() {
                    return;
                }
                if playback.engine.is_playing() {
                    // Scrubbing while playing: carry on from there.
                    playback.engine.play(frame);
                } else {
                    playback.show_playhead();
                }
            }
        ));
        project.connect_changed(glib::clone!(
            #[weak]
            playback,
            move |_| {
                playback.sync_timeline();
                if !playback.engine.is_playing() {
                    playback.show_playhead();
                }
            }
        ));
        // Render at the size the viewer shows (GTK has no resize signal for
        // pictures, so poll).
        glib::timeout_add_local(
            Duration::from_millis(200),
            glib::clone!(
                #[weak]
                playback,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    playback.update_size();
                    glib::ControlFlow::Continue
                }
            ),
        );
        playback.update_size();
        playback.show_playhead();
        playback
    }

    fn sync_timeline(&self) {
        self.engine
            .set_timeline(Arc::new(self.project.timeline().clone()));
        self.update_size();
    }

    /// Output size: the sequence fitted into the viewer (in device pixels),
    /// never larger than the sequence.
    fn update_size(&self) {
        let picture = self.viewer.picture();
        if picture.width() == 0 {
            return; // not shown yet
        }
        let scale = f64::from(picture.scale_factor());
        let (vw, vh) = (
            f64::from(picture.width().max(64)) * scale,
            f64::from(picture.height().max(36)) * scale,
        );
        let settings = self.project.timeline().settings.clone();
        let (sw, sh) = (f64::from(settings.width), f64::from(settings.height));
        let fit = (vw / sw).min(vh / sh).min(1.0);
        // Even sizes, which every consumer handles.
        let even = |v: f64| ((v * fit / 2.0).round() as u32 * 2).max(16);
        let size = (even(sw), even(sh));
        if self.engine.size() != size {
            self.engine.set_size(size.0, size.1);
            tracing::debug!("rendering at {}×{}", size.0, size.1);
            if !self.engine.is_playing() {
                self.show_playhead();
            }
        }
    }

    /// Shows the frame at the playhead (rendering it if needed).
    fn show_playhead(&self) {
        let frame = self.timeline.playhead();
        if let Some(rendered) = self.engine.request(frame) {
            self.show(&rendered);
        }
    }

    fn show(&self, rendered: &Arc<RenderedFrame>) {
        tracing::trace!(
            target: "nox::viewer",
            "showing frame {}{} at playhead {}",
            rendered.frame,
            if rendered.exact { "" } else { " (preview)" },
            self.timeline.playhead()
        );
        if let Err(err) = self.viewer.show_frame(rendered) {
            tracing::warn!("GTK cannot import DMA-BUF frames ({err}), falling back to readback");
            self.engine.disable_dmabuf();
            self.shown.set(None);
            self.show_playhead();
            return;
        }
        self.shown.set(Some(rendered.frame));
    }

    pub fn play(self: &Rc<Self>) {
        if self.engine.is_playing() {
            return;
        }
        let end = self.project.duration_frames();
        if end == 0 {
            return;
        }
        // At the end, start over.
        if self.timeline.playhead() >= end - 1 {
            self.ticking.set(true);
            self.timeline.set_playhead(0);
            self.ticking.set(false);
        }
        self.stats.set(Stats {
            started: Some(std::time::Instant::now()),
            ..Stats::default()
        });
        self.engine.play(self.timeline.playhead());
        self.viewer.set_playing(true);
        let weak = Rc::downgrade(self);
        let id = self
            .viewer
            .picture()
            .add_tick_callback(move |_, clock| match weak.upgrade() {
                Some(playback) if playback.on_tick(clock) => glib::ControlFlow::Continue,
                _ => glib::ControlFlow::Break,
            });
        self.tick.replace(Some(id));
        tracing::info!(
            "playing from frame {}{}",
            self.timeline.playhead(),
            if self.engine.has_audio() {
                ""
            } else {
                " (no audio)"
            }
        );
    }

    pub fn pause(&self) {
        if !self.engine.is_playing() {
            return;
        }
        self.engine.pause();
        if let Some(id) = self.tick.take() {
            id.remove();
        }
        self.viewer.set_playing(false);
        tracing::info!(
            "paused at frame {}: {}",
            self.timeline.playhead(),
            self.stats.get()
        );
        // Make sure the paused frame is the playhead's.
        if self.shown.get() != Some(self.timeline.playhead()) {
            self.show_playhead();
        }
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.engine.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Advances to the clock's frame. Returns whether to keep ticking.
    fn on_tick(&self, clock: &gdk::FrameClock) -> bool {
        // Pick the frame for when this refresh will be on screen: about one
        // refresh interval after the frame clock's time. All ticks of one
        // refresh then agree on the frame, and the steps are even.
        let (interval, _) = clock.refresh_info(clock.frame_time());
        let interval = if interval > 0 { interval } else { 16_667 };
        let lead_us = clock.frame_time() + interval - glib::monotonic_time();
        let now = std::time::Instant::now();
        let at = if lead_us >= 0 {
            now + Duration::from_micros(lead_us as u64)
        } else {
            now.checked_sub(Duration::from_micros(lead_us.unsigned_abs()))
                .unwrap_or(now)
        };
        let Some(position) = self.engine.position_at(at) else {
            return false;
        };
        let end = self.project.duration_frames();
        let frame = position.floor() as i64;
        if frame >= end {
            self.ticking.set(true);
            self.timeline.set_playhead((end - 1).max(0));
            self.ticking.set(false);
            self.tick.take();
            self.pause();
            return false;
        }
        let mut stats = self.stats.get();
        stats.ticks += 1;
        if self.shown.get() != Some(frame) {
            if let Some(rendered) = self.engine.cached(frame) {
                let skipped = self
                    .shown
                    .get()
                    .map_or(0, |last| (frame - last - 1).max(0) as u64);
                stats.shown += 1;
                tracing::trace!(target: "nox::pacing", "frame {frame} at {} µs", clock.frame_time());
                stats.skipped += skipped;
                if skipped > 0 {
                    tracing::trace!("skipped {skipped} frames before {frame}");
                }
                self.show(&rendered);
            } else {
                stats.late += 1;
            }
            self.ticking.set(true);
            self.timeline.set_playhead(frame);
            self.ticking.set(false);
        }
        self.stats.set(stats);
        true
    }
}

/// What playback looked like, logged on pause.
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    started: Option<std::time::Instant>,
    /// Display refreshes.
    ticks: u64,
    shown: u64,
    /// Frames never shown.
    skipped: u64,
    /// Refreshes where the frame due wasn't rendered yet.
    late: u64,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.started.map_or(0.0, |s| s.elapsed().as_secs_f64());
        write!(
            f,
            "{} frames shown, {} skipped, {} refreshes waiting for a frame; \
             {} refreshes in {seconds:.1} s ({:.0} Hz)",
            self.shown,
            self.skipped,
            self.late,
            self.ticks,
            self.ticks as f64 / seconds.max(1e-3)
        )
    }
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
