//! Rendering timelines: compositing, dissolves, caching and audio mixing.

mod common;

use common::{FPS, clip, f, fixture, solid, wait_for};
use nox_engine::{DecodeApi, Engine, EngineEvent, FrameImage, HwDecode, Mixer, RenderedFrame};
use nox_project::{
    Clip, Composable, Edge, ItemRef, MediaReference, TimeRange, Timeline, TrackKind,
};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

fn pixel(frame: &RenderedFrame, x: u32, y: u32) -> [u8; 3] {
    let FrameImage::Pixels(image) = &frame.image else {
        panic!("expected a readback frame")
    };
    let i = y as usize * image.stride + x as usize * 4;
    [image.data[i], image.data[i + 1], image.data[i + 2]]
}

fn close(a: [u8; 3], b: [u8; 3]) -> bool {
    (0..3).all(|c| a[c].abs_diff(b[c]) <= 3)
}

/// Requests a frame and waits for it.
fn render(engine: &Engine, events: &mpsc::Receiver<EngineEvent>, frame: i64) -> Arc<RenderedFrame> {
    // `request` may also return a nearby cached frame to show meanwhile.
    if let Some(cached) = engine.request(frame).filter(|f| f.frame == frame) {
        return cached;
    }
    loop {
        match events
            .recv_timeout(Duration::from_secs(20))
            .expect("frame in time")
        {
            EngineEvent::Frame(rendered) if rendered.frame == frame && rendered.exact => {
                return rendered;
            }
            EngineEvent::Frame(_) | EngineEvent::Decoding(_) => {}
            EngineEvent::Error { message, .. } => panic!("{message}"),
        }
    }
}

#[test]
fn composites_tracks_and_dissolves() {
    // Composited on the GPU (the CPU fallback shows the top layer only).
    if !common::have_gpu() {
        return;
    }
    let (Some(red), Some(blue)) = (
        solid("red.mkv", "0xC03020", "320x180"),
        solid("blue.mkv", "0x2060E0", "320x180"),
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    // V1: red 0–50, then blue 50–80 with a dissolve over the cut.
    timeline.place(0, clip(&red, 10, 50), f(0)).unwrap();
    timeline.place(0, clip(&blue, 10, 30), f(50)).unwrap();
    timeline
        .add_dissolve(ItemRef { track: 0, index: 0 }, Edge::End, f(10))
        .unwrap();
    // V2: half-transparent blue over frames 10–20.
    let v2 = timeline.add_track(TrackKind::Video);
    let mut top = clip(&blue, 0, 10);
    top.item_mut().unwrap().set_opacity(0.5);
    timeline.place(v2, top, f(10)).unwrap();

    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline.clone()));

    let red_px = pixel(&render(&engine, &events, 5), 80, 45);
    let blue_px = pixel(&render(&engine, &events, 70), 80, 45);
    assert!(close(red_px, [0xC0, 0x30, 0x20]), "red: {red_px:?}");
    assert!(close(blue_px, [0x20, 0x60, 0xE0]), "blue: {blue_px:?}");
    // Opacity and dissolves mix in linear light (BT.1886 decoding).
    let mix = |t: f64| {
        let linear = |v: u8| (f64::from(v) / 255.0).powf(2.4);
        [0, 1, 2].map(|c| {
            let l = linear(red_px[c]) * (1.0 - t) + linear(blue_px[c]) * t;
            (l.powf(1.0 / 2.4) * 255.0).round() as u8
        })
    };

    let half = pixel(&render(&engine, &events, 15), 80, 45);
    assert!(close(half, mix(0.5)), "opacity: {half:?} vs {:?}", mix(0.5));
    // The dissolve runs 45–55; at 50 it is halfway.
    let dissolve = pixel(&render(&engine, &events, 50), 80, 45);
    assert!(
        close(dissolve, mix(0.5)),
        "dissolve: {dissolve:?} vs {:?}",
        mix(0.5)
    );
    let early = pixel(&render(&engine, &events, 47), 80, 45);
    assert!(
        close(early, mix(0.2)),
        "dissolve at 47: {early:?} vs {:?}",
        mix(0.2)
    );
    // Past the end: black.
    assert!(close(
        pixel(&render(&engine, &events, 90), 80, 45),
        [0, 0, 0]
    ));

    // Rendered frames are cached; a new size makes them stale.
    assert!(engine.request(15).is_some());
    engine.set_size(64, 36);
    assert!(engine.cached(15).is_none());
    assert_eq!(render(&engine, &events, 15).size(), (64, 36));

    // A disabled (hidden) track isn't composited.
    timeline.track_mut(v2).unwrap().item.enabled = false;
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    let hidden = pixel(&render(&engine, &events, 15), 80, 45);
    assert!(close(hidden, red_px), "hidden V2: {hidden:?}");
}

#[test]
fn mixer_places_and_fades_audio() {
    const RATE: u32 = 48_000;
    // One second of a constant 0.5 in both channels.
    let Some(path) = fixture(
        "half.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.5|0.5:s=48000:d=1",
            "-c:a",
            "pcm_f32le",
        ],
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    let reference = MediaReference::from_path(&path, Some(TimeRange::new(f(0), f(25))));
    let mut audio = Composable::Clip(Clip::new("a", reference, TimeRange::new(f(0), f(10))));
    audio.item_mut().unwrap().set_volume(0.5);
    // Frames 5–15 on A1: 0.2 s to 0.6 s.
    timeline.place(1, audio, f(5)).unwrap();

    let mut mixer = Mixer::new(RATE);
    let mut block = vec![0.0f32; 2 * 4800]; // 0.1 s
    let level = |mixer: &mut Mixer, block: &mut Vec<f32>, start: i64| {
        mixer.mix(&timeline, start, block);
        (block[0], block[block.len() - 1])
    };
    assert_eq!(level(&mut mixer, &mut block, 0), (0.0, 0.0));
    // 0.15–0.25 s: silence until 0.2 s, then 0.5 * 0.5.
    mixer.mix(&timeline, 7200, &mut block);
    assert_eq!(block[0], 0.0);
    assert!(
        (block[2 * 2400 + 10] - 0.25).abs() < 1e-3,
        "{}",
        block[2 * 2400 + 10]
    );
    let (first, last) = level(&mut mixer, &mut block, 24_000);
    assert!((first - 0.25).abs() < 1e-3 && (last - 0.25).abs() < 1e-3);
    assert_eq!(level(&mut mixer, &mut block, 30_000), (0.0, 0.0));

    // A muted (disabled) track is left out of the mix.
    let mut muted = timeline.clone();
    muted.track_mut(1).unwrap().item.enabled = false;
    mixer.mix(&muted, 24_000, &mut block);
    assert!(block.iter().all(|&s| s == 0.0));
}

#[test]
fn playback_follows_the_clock_and_renders_ahead() {
    let input = "color=c=0xC03020:size=320x180:rate=25:duration=4,format=yuv420p";
    let args = ["-f", "lavfi", "-i", input, "-c:v", "rawvideo", "-f", "nut"];
    let Some(red) = fixture("red-play.nut", &args) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 0, 100), f(0)).unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    // As in the app, the viewer shows the start frame before playing (which
    // also gets the GPU and decoder going).
    render(&engine, &events, 10);

    // Without an audio device the clock is wall time.
    // SAFETY: this test binary's other tests don't read the variable.
    unsafe { std::env::set_var("NOX_AUDIO_DEVICE", "none") };
    let started = std::time::Instant::now();
    engine.play(10);
    assert!(engine.is_playing() && !engine.has_audio());
    assert!(wait_for(Duration::from_secs(5), || engine
        .position()
        .unwrap()
        >= 20.0));
    let elapsed = started.elapsed().as_secs_f64() * 25.0;
    let position = engine.position().unwrap() - 10.0;
    assert!(
        (position - elapsed).abs() < 3.0,
        "{position} frames after {elapsed}"
    );
    // Frames ahead of the clock get rendered.
    assert!(wait_for(Duration::from_secs(5), || {
        let now = engine.position().unwrap() as i64;
        engine.cached(now + 3).is_some()
    }));
    engine.pause();
    assert!(engine.position().is_none());

    // ALSA's null device takes samples as fast as they come, so its clock
    // runs ahead of real time: playback must notice and use wall time.
    // SAFETY: as above.
    unsafe { std::env::set_var("NOX_AUDIO_DEVICE", "Discard all samples") };
    let started = std::time::Instant::now();
    engine.play(0);
    if !engine.has_audio() {
        eprintln!("skipping audio clock check: no ALSA null device");
        return;
    }
    assert!(wait_for(Duration::from_secs(5), || {
        engine.position().unwrap() >= 10.0
    }));
    let elapsed = started.elapsed().as_secs_f64() * 25.0;
    let position = engine.position().unwrap();
    // Within the allowed drift (a second) of wall time.
    assert!(
        position < elapsed + 28.0,
        "{position} frames after {elapsed}"
    );
    engine.pause();
}

/// More layers of one file than the decoder pool opens per file.
#[test]
fn one_file_on_many_layers() {
    let Some(red) = solid("red-layers.mkv", "0xC03020", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 0, 20), f(0)).unwrap();
    for start in [5, 10, 15] {
        let track = timeline.add_track(TrackKind::Video);
        let mut layer = clip(&red, start, 20);
        layer.item_mut().unwrap().set_opacity(0.5);
        timeline.place(track, layer, f(0)).unwrap();
    }
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(64, 36);
    engine.set_timeline(Arc::new(timeline));
    for frame in [0, 3, 1] {
        let px = pixel(&render(&engine, &events, frame), 32, 18);
        assert!(close(px, [0xC0, 0x30, 0x20]), "{px:?}");
    }
}

/// Scrubbing in long-GOP footage: frames far from a keyframe get a quick
/// stand-in first, and newer requests abandon older ones.
#[test]
fn scrubbing_previews_and_interrupts() {
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=12",
        "-c:v", "libx264", "-g", "250", "-bf", "2", "-preset", "ultrafast",
    ];
    let Some(path) = fixture("scrub-long-gop.mp4", &args) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    let reference = MediaReference::from_path(&path, Some(TimeRange::new(f(0), f(300))));
    timeline
        .place(
            0,
            Composable::Clip(Clip::new("c", reference, TimeRange::new(f(0), f(300)))),
            f(0),
        )
        .unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    let next = || loop {
        match events.recv_timeout(Duration::from_secs(20)).expect("event") {
            EngineEvent::Frame(rendered) => return rendered,
            EngineEvent::Decoding(_) => {}
            EngineEvent::Error { message, .. } => panic!("{message}"),
        }
    };

    // Near the keyframe: exact straight away.
    assert!(engine.request(2).is_none());
    let first = next();
    assert_eq!((first.frame, first.exact), (2, true));

    // 200 frames past the keyframe: a preview, then the exact frame.
    assert!(engine.request(200).is_none());
    let preview = next();
    assert_eq!((preview.frame, preview.exact), (200, false));
    let exact = next();
    assert_eq!((exact.frame, exact.exact), (200, true));
    // Only the exact one is cached.
    assert!(engine.cached(200).is_some_and(|f| f.exact));

    // A newer request abandons the one in progress: going back to 150
    // (a long decode from the keyframe), then at once to 3.
    assert!(engine.request(150).is_none());
    // Frame 2 is cached, so it stands in for 3 at once.
    assert_eq!(engine.request(3).map(|f| f.frame), Some(2));
    let mut delivered = Vec::new();
    loop {
        let rendered = next();
        delivered.push((rendered.frame, rendered.exact));
        if rendered.frame == 3 && rendered.exact {
            break;
        }
    }
    assert!(!delivered.contains(&(150, true)), "{delivered:?}");
}

/// Stills are converted once and reused: the picture stays right across
/// frames, alongside other layers, and for the same file on two tracks.
#[test]
fn stills_from_the_cache() {
    let Some(photo) = fixture(
        "still.png",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=0x30A050:size=1600x1200,format=rgb24",
            "-frames:v",
            "1",
        ],
    ) else {
        return;
    };
    let Some(red) = solid("red-still-test.mkv", "0xC03020", "320x180") else {
        return;
    };
    let still = |start: i64, length: i64| {
        let reference = MediaReference::from_path(&photo, None);
        Composable::Clip(Clip::new(
            "p",
            reference,
            TimeRange::new(f(start), f(length)),
        ))
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, still(0, 20), f(0)).unwrap();
    timeline.place(0, clip(&red, 0, 10), f(20)).unwrap();
    timeline.place(0, still(0, 20), f(30)).unwrap();
    // The same photo, half transparent, on V2 over the second still.
    let v2 = timeline.add_track(TrackKind::Video);
    let mut top = still(0, 5);
    top.item_mut().unwrap().set_opacity(0.5);
    timeline.place(v2, top, f(35)).unwrap();

    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    let green = [0x30, 0xA0, 0x50];
    // 4:3 in 16:9: pillarboxed, so the edges are black.
    for frame in [0, 5, 19, 25, 30, 36, 45] {
        let rendered = render(&engine, &events, frame);
        let center = pixel(&rendered, 80, 45);
        let expected = if (20..30).contains(&frame) {
            [0xC0, 0x30, 0x20]
        } else {
            green
        };
        assert!(close(center, expected), "frame {frame}: {center:?}");
        if !(20..30).contains(&frame) {
            assert!(
                close(pixel(&rendered, 3, 45), [0, 0, 0]),
                "frame {frame}: edge"
            );
        }
    }
}

/// Frames carry scopes only while they are enabled; switching re-renders
/// cached frames rather than handing out ones without them.
#[test]
fn scopes_come_with_frames_when_enabled() {
    let Some(red) = solid("red-scopes.mkv", "0xC03020", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 0, 20), f(0)).unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));

    let plain = render(&engine, &events, 5);
    if plain.scopes.is_some() {
        panic!("scopes while disabled");
    }
    engine.set_scopes(true);
    let measured = render(&engine, &events, 5);
    let Some(scopes) = &measured.scopes else {
        // Without a GPU there are no scopes.
        return;
    };
    assert_eq!(
        (scopes.waveform.width, scopes.waveform.height),
        (
            nox_engine::scope_sizes::WAVEFORM_WIDTH,
            nox_engine::scope_sizes::LEVELS
        )
    );
    assert!(scopes.waveform.data.chunks(4).any(|p| p[3] > 0), "a trace");
    engine.set_scopes(false);
    assert!(render(&engine, &events, 5).scopes.is_none());
}

/// A clip's grade (`metadata.nox.grade`) reaches the rendered picture.
#[test]
fn clip_grades_are_rendered() {
    // Composited on the GPU (the CPU fallback shows the top layer only).
    if !common::have_gpu() {
        return;
    }
    let Some(gray) = solid("gray-grade.mkv", "0x808080", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    let grade = nox_project::Grade {
        gain: nox_project::Wheel {
            rgb: [1.1, 1.0, 1.0],
            master: 1.0,
        },
        ..nox_project::Grade::NEUTRAL
    };
    let mut graded = clip(&gray, 0, 20);
    graded.item_mut().unwrap().set_grade(grade);
    timeline.place(0, clip(&gray, 0, 20), f(0)).unwrap();
    timeline.place(0, graded, f(20)).unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    let plain = pixel(&render(&engine, &events, 5), 80, 45);
    let warm = pixel(&render(&engine, &events, 25), 80, 45);
    // As the CPU reference grades it: in ACEScct on AP1 channels, so more
    // AP1 red also lowers BT.709 green and blue.
    let linear = nox_color::InputTransform::default().apply(plain.map(|c| f32::from(c) / 255.0));
    let expected = nox_color::OutputTransform
        .apply(grade.apply(linear))
        .map(|c| (c * 255.0).round() as u8);
    assert!(warm[0] > plain[0] + 10, "{plain:?} → {warm:?}");
    assert!(close(warm, expected), "{warm:?} vs {expected:?}");
}

/// A clip's transform reaches the compositor: zoomed out and moved right,
/// it leaves the left of the picture to the track below.
#[test]
fn clip_transforms_are_rendered() {
    // Composited on the GPU (the CPU fallback shows the top layer only).
    if !common::have_gpu() {
        return;
    }
    let (Some(red), Some(blue)) = (
        solid("red-under.mkv", "0xC03020", "320x180"),
        solid("blue-moved.mkv", "0x2060E0", "320x180"),
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    let v2 = timeline.add_track(TrackKind::Video);
    timeline.place(0, clip(&red, 0, 20), f(0)).unwrap();
    let mut moved = clip(&blue, 0, 20);
    moved
        .item_mut()
        .unwrap()
        .set_transform(nox_project::Transform {
            x: 0.25,
            zoom: 0.5,
            ..nox_project::Transform::IDENTITY
        });
    timeline.place(v2, moved, f(0)).unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    let frame = render(&engine, &events, 5);
    // Blue covers x 80–160 and y 22–68 of 160 × 90.
    let (left, right) = (pixel(&frame, 30, 45), pixel(&frame, 120, 45));
    assert!(left[0] > left[2], "red on the left: {left:?}");
    assert!(right[2] > right[0], "blue on the right: {right:?}");
    let above = pixel(&frame, 120, 10);
    assert!(above[0] > above[2], "red above the blue: {above:?}");
}

/// Clips at another speed play their audio that much faster (varispeed);
/// freeze frames are silent.
#[test]
fn mixer_follows_clip_speed() {
    const RATE: u32 = 48_000;
    // A ramp: every sample tells its source time (t / 4).
    let Some(path) = fixture(
        "speed-ramp.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=t/4|t/4:s=48000:d=4",
            "-c:a",
            "pcm_f32le",
        ],
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    let reference = MediaReference::from_path(&path, Some(TimeRange::new(f(0), f(100))));
    let clip = Composable::Clip(Clip::new("a", reference, TimeRange::new(f(0), f(50))));
    timeline.place(1, clip, f(0)).unwrap();
    let a = ItemRef { track: 1, index: 0 };
    let fast = timeline.set_speed(a, 2.0, true).unwrap();

    let mut mixer = Mixer::new(RATE);
    let mut block = vec![0.0f32; 2 * 480]; // 10 ms
    // 0.5 s into the clip is 1 s into the source: 0.25.
    mixer.mix(&timeline, 24_000, &mut block);
    assert!((block[0] - 0.25).abs() < 1e-3, "{}", block[0]);
    // Within the block the source advances twice as fast.
    let step = block[2 * 479] - block[0];
    assert!((step - 2.0 * 479.0 / 48_000.0 / 4.0).abs() < 1e-3, "{step}");

    // Without the time warp it plays at normal speed again.
    let mut normal = timeline.clone();
    normal
        .track_mut(fast.track)
        .unwrap()
        .children
        .get_mut(fast.index)
        .and_then(Composable::as_clip_mut)
        .unwrap()
        .item
        .effects
        .clear();
    mixer.mix(&normal, 24_000, &mut block);
    assert!(
        (block[0] - 0.125).abs() < 1e-3,
        "normal speed again: {}",
        block[0]
    );
}

/// A dip to white covers the outgoing picture with white, then the
/// incoming one fades in over it, whatever is on the tracks below.
#[test]
fn dips_go_through_a_color() {
    // Composited on the GPU (the CPU fallback shows the top layer only).
    if !common::have_gpu() {
        return;
    }
    let (Some(red), Some(blue)) = (
        solid("red-dip.mkv", "0xC03020", "320x180"),
        solid("blue-dip.mkv", "0x2060E0", "320x180"),
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 20, 20), f(0)).unwrap();
    timeline.place(0, clip(&blue, 20, 20), f(20)).unwrap();
    timeline
        .add_transition(
            ItemRef { track: 0, index: 0 },
            Edge::End,
            f(20),
            nox_project::TransitionKind::Dip(nox_project::DipColor::White),
        )
        .unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    // The dip runs from frame 10 to 30; halfway it is all white.
    let middle = pixel(&render(&engine, &events, 20), 80, 45);
    assert!(middle.iter().all(|&c| c >= 250), "white: {middle:?}");
    // A quarter through: red, half covered by white.
    let quarter = pixel(&render(&engine, &events, 15), 80, 45);
    assert!(
        quarter[0] > 200 && quarter[1] > 100 && quarter[2] > 100,
        "{quarter:?}"
    );
    assert!(quarter[1] < 250, "not white yet: {quarter:?}");
}

/// The engine tells how each file decodes, and decodes as it's told.
#[test]
fn reports_how_files_decode() {
    let Some(red) = solid("red.mkv", "0xC03020", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 0, 50), f(0)).unwrap();
    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    assert!(engine.decoding(&red).is_none());
    render(&engine, &events, 5);
    // FFmpeg 9 decodes FFV1 with Vulkan (compute shaders) on GPUs.
    let decoding = engine.decoding(&red).expect("decoding reported");
    assert_eq!(decoding.decoder, "ffv1");

    // A new way to decode: decoders open again and report again.
    engine.set_hw_decode(HwDecode::Software);
    assert!(engine.decoding(&red).is_none());
    render(&engine, &events, 6);
    assert_eq!(
        engine.decoding(&red).map(|d| d.api),
        Some(DecodeApi::Software)
    );
}

/// Background removal (nox-ai's matting) runs on a clip that has it: a
/// plain blue picture has no person in it, so it's matted out and the red
/// clip below shows through. Needs the model in `NOX_MODELS_DIR` (skips
/// otherwise).
#[test]
fn background_removal_shows_what_is_below() {
    let model = nox_ai::models::ROBUST_VIDEO_MATTING;
    if !common::have_gpu() || !model.is_available(&nox_ai::models::default_dir()) {
        eprintln!("skipping: needs a GPU and the matting model");
        return;
    }
    let (Some(red), Some(blue)) = (
        solid("red-matte.mkv", "0xC03020", "320x180"),
        solid("blue-matte.mkv", "0x2060E0", "320x180"),
    ) else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    timeline.place(0, clip(&red, 0, 50), f(0)).unwrap();
    let v2 = timeline.add_track(TrackKind::Video);
    let mut top = clip(&blue, 0, 50);
    top.item_mut().unwrap().set_background_removal(true);
    timeline.place(v2, top, f(0)).unwrap();

    let (sender, events) = mpsc::channel();
    let engine = Engine::new(Vec::new(), move |event| {
        let _ = sender.send(event);
    });
    engine.set_size(160, 90);
    engine.set_timeline(Arc::new(timeline));
    for frame in [5, 6, 30] {
        let px = pixel(&render(&engine, &events, frame), 80, 45);
        assert!(close(px, [0xC0, 0x30, 0x20]), "frame {frame}: {px:?}");
    }
}
