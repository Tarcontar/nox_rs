//! The engine: renders sequence frames on a worker thread into a cache,
//! and plays the sequence with audio as the clock.
//!
//! While paused, the worker renders the latest requested frame (newer
//! requests replace older ones, so scrubbing never queues up stale work).
//! While playing, it renders ahead of the clock; the GUI shows whatever
//! cached frame matches the clock on each display refresh.

use nox_media::{Decoding, HwDecode};
use nox_project::Timeline;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{AudioClock, AudioOutput};
use crate::cache::{CacheKey, FrameCache};
use crate::render::{PREVIEW_COST, Quality, Renderer};
use crate::{EngineEvent, RenderedFrame};

/// A requested frame renders at least this long before a newer request
/// can interrupt it.
const MIN_WORK_BEFORE_INTERRUPT: Duration = Duration::from_millis(50);
/// While a requested frame renders, a cached frame this close stands in.
const NEARBY_SECONDS: f64 = 0.5;
/// How far ahead of the clock playback renders.
const RENDER_AHEAD: Duration = Duration::from_millis(500);
/// Frame cache size unless `NOX_CACHE_MB` says otherwise.
const DEFAULT_CACHE_MB: usize = 768;

enum Clock {
    /// No audio output: wall time.
    Wall,
    Audio(Arc<AudioClock>, u32),
}

/// An audio clock further than this from wall time is not trusted (e.g.
/// a device that consumes samples faster than real time).
const MAX_CLOCK_DRIFT: f64 = 1.0;

/// The playback clock follows the audio clock this far before snapping
/// to it; smaller errors are corrected gradually.
const MAX_SLEW: f64 = 0.05;
/// Fraction of the error to the audio clock corrected per reading.
const SLEW_GAIN: f64 = 0.02;

struct Playback {
    /// Timeline seconds at the start.
    start: f64,
    started: Instant,
    clock: Clock,
    /// The audio clock drifted too far; wall time is used instead.
    audio_untrusted: AtomicBool,
    /// Audio clock minus wall time, smoothed. The audio clock advances in
    /// steps (per device callback) and its latency estimate jitters, which
    /// would make frames at 60 fps alternate between skipped and repeated;
    /// wall time with a slowly adjusted offset is smooth and stays in sync.
    offset: Mutex<Option<f64>>,
}

impl Playback {
    fn new(start: f64, clock: Clock) -> Self {
        Self {
            start,
            started: Instant::now(),
            clock,
            audio_untrusted: AtomicBool::new(false),
            offset: Mutex::new(None),
        }
    }

    /// Timeline seconds at `at` (now or soon, e.g. when the next display
    /// refresh will be shown).
    fn seconds_at(&self, at: Instant) -> f64 {
        self.seconds_at_with(at, Instant::now())
    }

    /// `seconds_at`, with the current time given (for tests).
    fn seconds_at_with(&self, at: Instant, now: Instant) -> f64 {
        let wall = now.saturating_duration_since(self.started).as_secs_f64();
        let ahead = if at >= now {
            (at - now).as_secs_f64()
        } else {
            -(now - at).as_secs_f64()
        };
        let played = match &self.clock {
            Clock::Wall => wall,
            Clock::Audio(_, _) if self.audio_untrusted.load(Ordering::Relaxed) => wall,
            Clock::Audio(clock, rate) => {
                let audio = clock.seconds_at(*rate, now);
                if (audio - wall).abs() > MAX_CLOCK_DRIFT {
                    tracing::warn!(
                        "audio clock at {audio:.2} s after {wall:.2} s of playback; using wall time"
                    );
                    self.audio_untrusted.store(true, Ordering::Relaxed);
                    wall
                } else {
                    let mut offset = self.offset.lock().unwrap_or_else(|e| e.into_inner());
                    let error = audio - wall - offset.unwrap_or(0.0);
                    *offset = Some(match *offset {
                        Some(o) if error.abs() <= MAX_SLEW => o + error * SLEW_GAIN,
                        _ => audio - wall,
                    });
                    wall + offset.expect("set")
                }
            }
        };
        self.start + played + ahead
    }
}

struct State {
    timeline: Option<Arc<Timeline>>,
    generation: u64,
    size: (u32, u32),
    /// Frame to render next while paused.
    request: Option<i64>,
    playback: Option<Playback>,
    /// Counts playback starts, so the worker knows when one begins.
    playback_id: u64,
    /// Measure scopes with every frame (`RenderedFrame::scopes`).
    scopes: bool,
    shutdown: bool,
}

impl State {
    fn key(&self) -> CacheKey {
        CacheKey {
            generation: self.generation,
            width: self.size.0,
            height: self.size.1,
        }
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// Locked after `state` when both are needed.
    cache: Mutex<FrameCache>,
    /// Bumped by everything that makes a requested frame in progress
    /// pointless (a newer request, a new timeline or size, playback,
    /// shutdown), so long decodes give up early.
    interrupts: AtomicU64,
    /// How decoders decode (`Engine::set_hw_decode`).
    hw_decode: Mutex<HwDecode>,
    /// Set when effects should be loaded again (`Engine::reload_effects`).
    reload_effects: AtomicBool,
    /// How each file decodes, as far as known.
    decodings: Mutex<HashMap<PathBuf, Decoding>>,
    /// Whether the GPU keeps 16-bit floats, once the renderer has it.
    storage_16bit: std::sync::OnceLock<bool>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cache(&self) -> MutexGuard<'_, FrameCache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn decodings(&self) -> MutexGuard<'_, HashMap<PathBuf, Decoding>> {
        self.decodings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hw_decode(&self) -> HwDecode {
        *self.hw_decode.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wakes the worker and stops the request it is rendering, if any.
    fn interrupt(&self) {
        self.interrupts.fetch_add(1, Ordering::Relaxed);
        self.wake.notify_all();
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    /// `None` once shut down.
    worker: RefCell<Option<JoinHandle<()>>>,
    dmabuf_allowed: Arc<AtomicBool>,
    /// The output stream lives on the thread that created it.
    audio: RefCell<Option<AudioOutput>>,
}

impl Engine {
    /// Starts the render worker. `dmabuf_formats` are the (DRM fourcc,
    /// modifier) pairs the consumer can import (empty: always read back);
    /// `AB4H` is used if offered, else `AB24`. `on_event` is called on the
    /// worker thread.
    pub fn new(
        dmabuf_formats: Vec<(u32, u64)>,
        on_event: impl Fn(EngineEvent) + Send + 'static,
    ) -> Self {
        let budget = std::env::var("NOX_CACHE_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CACHE_MB);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                timeline: None,
                generation: 0,
                size: (640, 360),
                request: None,
                playback: None,
                playback_id: 0,
                scopes: false,
                shutdown: false,
            }),
            wake: Condvar::new(),
            cache: Mutex::new(FrameCache::new(budget << 20)),
            interrupts: AtomicU64::new(0),
            hw_decode: Mutex::new(HwDecode::from_env()),
            reload_effects: AtomicBool::new(false),
            decodings: Mutex::default(),
            storage_16bit: std::sync::OnceLock::new(),
        });
        let dmabuf_allowed = Arc::new(AtomicBool::new(!dmabuf_formats.is_empty()));
        let allowed = dmabuf_allowed.clone();
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("nox-render".into())
            .spawn(move || {
                let renderer = Renderer::new(&dmabuf_formats, allowed);
                if let Some(storage_16bit) = renderer.storage_16bit() {
                    let _ = worker_shared.storage_16bit.set(storage_16bit);
                }
                run(&worker_shared, renderer, on_event);
            })
            .expect("failed to spawn render thread");
        Self {
            shared,
            worker: RefCell::new(Some(worker)),
            dmabuf_allowed,
            audio: RefCell::new(None),
        }
    }

    /// Uses `timeline` from now on; cached frames of older ones are stale.
    pub fn set_timeline(&self, timeline: Arc<Timeline>) {
        let mut state = self.shared.state();
        state.timeline = Some(timeline);
        state.generation += 1;
        self.shared.interrupt();
    }

    /// Sets the output size (stale cached frames are dropped).
    pub fn set_size(&self, width: u32, height: u32) {
        let mut state = self.shared.state();
        let size = (width.max(2), height.max(2));
        if state.size != size {
            state.size = size;
            self.shared.interrupt();
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.shared.state().size
    }

    /// Frame `frame` if it is cached; otherwise it is rendered next and
    /// delivered as `EngineEvent::Frame`, and the closest cached frame
    /// (within half a second) is returned to show meanwhile. Check
    /// `RenderedFrame::frame` to tell which one you got.
    pub fn request(&self, frame: i64) -> Option<Arc<RenderedFrame>> {
        if let Some(cached) = self.cached(frame) {
            return Some(cached);
        }
        let nearby = self.nearby(frame);
        let mut state = self.shared.state();
        state.request = Some(frame);
        self.shared.interrupt();
        nearby
    }

    /// The cached frame closest to `frame`, for showing while it renders.
    fn nearby(&self, frame: i64) -> Option<Arc<RenderedFrame>> {
        let state = self.shared.state();
        let key = state.key();
        let distance = state.timeline.as_ref().map_or(12, |t| {
            (t.frame_rate().as_f64() * NEARBY_SECONDS).ceil() as i64
        });
        drop(state);
        self.shared.cache().nearest(key, frame, distance)
    }

    /// Frame `frame` if it is cached.
    pub fn cached(&self, frame: i64) -> Option<Arc<RenderedFrame>> {
        let key = self.shared.state().key();
        self.shared.cache().get(key, frame)
    }

    /// Starts playing from sequence frame `frame`.
    pub fn play(&self, frame: i64) {
        let Some(timeline) = self.shared.state().timeline.clone() else {
            return;
        };
        let start = frame as f64 / timeline.frame_rate().as_f64();
        let shared = self.shared.clone();
        let current = move || shared.state().timeline.clone();
        let clock = match AudioOutput::start(start, current) {
            Ok(output) => {
                let clock = Clock::Audio(output.clock.clone(), output.rate);
                self.audio.replace(Some(output));
                clock
            }
            Err(err) => {
                tracing::info!("playing without audio: {err}");
                Clock::Wall
            }
        };
        let mut state = self.shared.state();
        state.playback = Some(Playback::new(start, clock));
        state.playback_id += 1;
        state.request = None;
        self.shared.interrupt();
    }

    pub fn pause(&self) {
        self.shared.state().playback = None;
        self.audio.replace(None);
    }

    /// Whether playback is clocked by an audio device (else wall time).
    pub fn has_audio(&self) -> bool {
        self.audio.borrow().is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.shared.state().playback.is_some()
    }

    /// The playback position in sequence frames (fractional), while playing.
    pub fn position(&self) -> Option<f64> {
        self.position_at(Instant::now())
    }

    /// The playback position at `at`, e.g. when the next display refresh
    /// will be presented.
    pub fn position_at(&self, at: Instant) -> Option<f64> {
        let state = self.shared.state();
        let fps = state.timeline.as_ref()?.frame_rate().as_f64();
        state.playback.as_ref().map(|p| p.seconds_at(at) * fps)
    }

    /// Measures scopes with every frame rendered from now on
    /// (`RenderedFrame::scopes`), e.g. while they are visible. Cached
    /// frames without them are rendered again.
    pub fn set_scopes(&self, enabled: bool) {
        let mut state = self.shared.state();
        if state.scopes != enabled {
            state.scopes = enabled;
            state.generation += 1;
            self.shared.interrupt();
        }
    }

    /// Decodes video as `hw` says from now on (initially as `NOX_DECODE`
    /// says). Frames are rendered again with the new decoders.
    pub fn set_hw_decode(&self, hw: HwDecode) {
        {
            let mut current = self
                .shared
                .hw_decode
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *current == hw {
                return;
            }
            *current = hw;
        }
        self.shared.decodings().clear();
        self.shared.state().generation += 1;
        self.shared.interrupt();
    }

    /// Loads effects that couldn't load before (e.g. background removal,
    /// once its model is downloaded) and renders frames again.
    pub fn reload_effects(&self) {
        self.shared.reload_effects.store(true, Ordering::Relaxed);
        self.shared.state().generation += 1;
        self.shared.interrupt();
    }

    /// Whether the GPU keeps 16-bit floats, so runs fp16 models in 16 bits
    /// (`None` until the renderer has started, or without a GPU).
    pub fn storage_16bit(&self) -> Option<bool> {
        self.shared.storage_16bit.get().copied()
    }

    /// How `path` decodes, once a frame of it has been (reported as
    /// `EngineEvent::Decoding`).
    pub fn decoding(&self, path: &Path) -> Option<Decoding> {
        self.shared.decodings().get(path).cloned()
    }

    /// Falls back to readback, e.g. after the consumer failed to import a
    /// DMA-BUF.
    pub fn disable_dmabuf(&self) {
        self.dmabuf_allowed.store(false, Ordering::Relaxed);
        // Cached DMA-BUF frames are no use now.
        self.shared.state().generation += 1;
    }
}

impl Engine {
    /// Stops playback and the render worker, interrupting a render in
    /// progress, and waits for it to finish. Call it before the process
    /// exits: a worker still decoding while the GPU driver's libraries are
    /// unloaded at exit crashes in the driver. Dropping the engine does the
    /// same; later calls do nothing.
    pub fn shutdown(&self) {
        self.audio.replace(None);
        self.shared.state().shutdown = true;
        self.shared.interrupt();
        let worker = self.worker.borrow_mut().take();
        if let Some(worker) = worker {
            tracing::debug!("stopping the render worker");
            let _ = worker.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Job {
    /// Requested while paused; reported back.
    Request(i64),
    /// Rendering ahead during playback; `protect` is the playback window.
    Ahead(i64, std::ops::Range<i64>),
}

fn run(shared: &Shared, mut renderer: Renderer, on_event: impl Fn(EngineEvent)) {
    let interrupts = || shared.interrupts.load(Ordering::Relaxed);
    // Frames that failed to render for a generation, not retried.
    let mut failed: (u64, HashSet<i64>) = (0, HashSet::new());
    // Average render time in seconds. When playback falls behind, rendering
    // aims this far ahead of the clock, so finished frames are still
    // useful (the ones in between are dropped).
    let mut render_time = 0.0f64;
    // The furthest frame rendered ahead in the current playback. Rendering
    // ahead only ever moves forward: going back behind it (when the lead
    // shrinks after a slow frame) would make decoders seek back to a
    // keyframe, which is slow and makes the lead grow again.
    let mut ahead: (u64, i64) = (0, i64::MIN);
    loop {
        let (job, timeline, key, scopes, interrupts_then) = {
            let mut state = shared.state();
            loop {
                if state.shutdown {
                    return;
                }
                let Some(timeline) = state.timeline.clone() else {
                    state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    continue;
                };
                let key = state.key();
                if failed.0 != key.generation {
                    failed = (key.generation, HashSet::new());
                }
                let mut cache = shared.cache();
                cache.validate(key);
                if let Some(frame) = state.request.take()
                    && !cache.contains(key, frame)
                {
                    break (
                        Job::Request(frame),
                        timeline,
                        key,
                        state.scopes,
                        interrupts(),
                    );
                }
                if let Some(playback) = &state.playback {
                    let fps = timeline.frame_rate();
                    let now = (playback.seconds_at(Instant::now()) * fps.as_f64()).floor() as i64;
                    let ahead_frames =
                        ((RENDER_AHEAD.as_secs_f64() * fps.as_f64()).ceil() as i64).max(4);
                    let end = timeline.duration().frame(fps) + 1;
                    let lead = (render_time * fps.as_f64()).ceil() as i64;
                    let from = if ahead.0 == state.playback_id {
                        (now + lead).max(ahead.1 + 1)
                    } else {
                        now + lead
                    };
                    // Empty once rendering is far enough ahead.
                    let window_end = now + lead + ahead_frames;
                    let next = (from.min(end)..window_end.min(end))
                        .find(|f| !cache.contains(key, *f) && !failed.1.contains(f));
                    if let Some(frame) = next {
                        ahead = (state.playback_id, frame);
                        break (
                            Job::Ahead(frame, now - 2..now + ahead_frames),
                            timeline,
                            key,
                            state.scopes,
                            interrupts(),
                        );
                    }
                }
                drop(cache);
                state = if state.playback.is_some() {
                    shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(4))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    shared.wake.wait(state).unwrap_or_else(|e| e.into_inner())
                };
            }
        };
        let frame = match job {
            Job::Request(frame) | Job::Ahead(frame, _) => frame,
        };
        let started = Instant::now();
        let (width, height) = (key.width, key.height);
        renderer.set_scopes(scopes);
        renderer.set_hw_decode(shared.hw_decode());
        if shared.reload_effects.swap(false, Ordering::Relaxed) {
            renderer.reload_matting();
        }
        let result = match job {
            // Scrubbing: if the frame is expensive to reach (far from a
            // keyframe), show a cheap stand-in at once, then work on the
            // exact frame until something newer is asked for.
            Job::Request(_) => {
                // Short renders finish (abandoning them while the mouse
                // keeps moving would never show anything); long decodes
                // give way. Their progress is kept by the decoder.
                let stop = || {
                    interrupts() != interrupts_then && started.elapsed() > MIN_WORK_BEFORE_INTERRUPT
                };
                // A nearby cached frame (shown by `request`) beats a
                // keyframe seconds away.
                let distance = (timeline.frame_rate().as_f64() * NEARBY_SECONDS).ceil() as i64;
                let nearby_cached = shared.cache().nearest(key, frame, distance).is_some();
                if !nearby_cached && renderer.decode_cost(&timeline, frame) > PREVIEW_COST {
                    match renderer.render(&timeline, frame, width, height, Quality::Preview, &stop)
                    {
                        Ok(preview) => {
                            tracing::trace!("preview of frame {frame} in {:?}", started.elapsed());
                            on_event(EngineEvent::Frame(Arc::new(preview)));
                        }
                        Err(err) => tracing::debug!("no preview of frame {frame}: {err}"),
                    }
                }
                if stop() {
                    Err(nox_media::Error::Interrupted)
                } else {
                    renderer.render(&timeline, frame, width, height, Quality::Exact, &stop)
                }
            }
            Job::Ahead(..) => {
                renderer.render(&timeline, frame, width, height, Quality::Exact, &|| false)
            }
        };
        let elapsed = started.elapsed();
        for (path, decoding) in renderer.decodings_changed() {
            shared.decodings().insert(path.clone(), decoding);
            on_event(EngineEvent::Decoding(path));
        }
        if let Err(nox_media::Error::Interrupted) = result {
            tracing::trace!("frame {frame} interrupted after {elapsed:?}");
            continue;
        }
        tracing::trace!("rendered frame {frame} in {elapsed:?}");
        if let Job::Ahead(..) = job {
            render_time = 0.8 * render_time + 0.2 * elapsed.as_secs_f64();
        }
        match result {
            Ok(rendered) => {
                let rendered = Arc::new(rendered);
                let protect = match &job {
                    Job::Ahead(_, protect) => protect.clone(),
                    Job::Request(frame) => *frame..*frame + 1,
                };
                shared.cache().insert(key, rendered.clone(), protect);
                if let Job::Request(_) = job {
                    on_event(EngineEvent::Frame(rendered));
                }
            }
            Err(err) => {
                failed.1.insert(frame);
                let message = format!("frame {frame}: {err}");
                if let Job::Request(_) = job {
                    on_event(EngineEvent::Error { frame, message });
                } else {
                    tracing::warn!("{message}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A device clock that advances in callback-sized steps with jittery
    /// latency gives a smooth, monotonic playback clock in step with it.
    #[test]
    fn audio_clock_is_smoothed() {
        let audio = Arc::new(AudioClock::default());
        let playback = Playback::new(10.0, Clock::Audio(audio.clone(), 48_000));
        let base = playback.started;
        let mut last = 0.0;
        let mut steps = Vec::new();
        // 2 s of readings every 4 ms; callbacks every 20 ms with ±3 ms of
        // latency jitter.
        for i in 0..500u64 {
            let t = base + Duration::from_millis(i * 4);
            let callback = i * 4 / 20 * 20;
            let jitter = [0, 3, 1, 2, 0][((i * 4 / 20) % 5) as usize];
            audio.set(
                callback * 48,
                base + Duration::from_millis(callback),
                Duration::from_millis(30 + jitter),
            );
            let seconds = playback.seconds_at_with(t, t);
            if i > 0 {
                steps.push(seconds - last);
                assert!(seconds >= last, "clock went back at {i}");
            }
            last = seconds;
        }
        // Even steps (4 ms ± 0.5 ms) after settling, not 0/20 ms jumps.
        for (i, step) in steps.iter().enumerate().skip(50) {
            assert!((step - 0.004).abs() < 0.0005, "step {i}: {step}");
        }
        // And in step with the audio clock (30–33 ms latency).
        let expected = 10.0 + 1.996 - 0.0315;
        assert!((last - expected).abs() < 0.01, "{last} vs {expected}");

        // A jump beyond the slew limit (e.g. the device skipping) is
        // followed at once. (Beyond a second, the drift guard takes over.)
        let t = base + Duration::from_secs(2);
        audio.set(2_300 * 48, t, Duration::ZERO);
        let jumped = playback.seconds_at_with(t, t);
        assert!((jumped - 12.3).abs() < 0.001, "{jumped}");
    }
}
