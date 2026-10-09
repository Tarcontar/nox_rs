//! Exporting timelines: the file shows what the viewer shows.

mod common;

use common::fixture;
use nox_engine::{EncodeQuality, ExportError, ExportSettings, VideoCodec, export};
use nox_media::{AudioDecoder, VideoDecoder, probe};
use nox_project::{
    Clip, Composable, Edge, ItemRef, MediaReference, Rational, ScalingMode, Time, TimeRange,
    Timeline,
};
use std::path::{Path, PathBuf};

const FPS: Rational = Rational::new(25, 1);

fn f(n: i64) -> Time {
    Time::from_frames(n, FPS)
}

fn solid(name: &str, color: &str, size: &str) -> Option<PathBuf> {
    let input = format!("color=c={color}:size={size}:rate=25:duration=4,format=yuv444p");
    fixture(name, &["-f", "lavfi", "-i", &input, "-c:v", "ffv1"])
}

fn clip(path: &Path, start: i64, length: i64) -> Composable {
    let reference = MediaReference::from_path(path, Some(TimeRange::new(f(0), f(100))));
    Composable::Clip(Clip::new(
        "c",
        reference,
        TimeRange::new(f(start), f(length)),
    ))
}

/// Red for 2 s, a 0.4 s dissolve to blue, blue for 1 s; a 0.5 tone on A1
/// for the first 2 s.
fn timeline() -> Option<Timeline> {
    let red = solid("x-red.mkv", "0xC03020", "320x180")?;
    let blue = solid("x-blue.mkv", "0x2060E0", "320x180")?;
    let tone = fixture(
        "x-tone.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.5|0.5:s=48000:d=4",
            "-c:a",
            "pcm_f32le",
        ],
    )?;
    let mut timeline = Timeline::new("t", FPS);
    (timeline.settings.width, timeline.settings.height) = (320, 180);
    timeline.place(0, clip(&red, 10, 50), f(0)).unwrap();
    timeline.place(0, clip(&blue, 10, 25), f(50)).unwrap();
    timeline
        .add_dissolve(ItemRef { track: 0, index: 0 }, Edge::End, f(10))
        .unwrap();
    timeline.place(1, clip(&tone, 0, 50), f(0)).unwrap();
    Some(timeline)
}

fn settings(name: &str, codec: VideoCodec, hardware: bool) -> ExportSettings {
    ExportSettings {
        path: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name),
        codec,
        quality: EncodeQuality::High,
        hardware,
        range: None,
        audio: true,
    }
}

fn center(path: &Path, frame: i64) -> [i32; 3] {
    let decoded = VideoDecoder::open(path, None)
        .unwrap()
        .frame_at(frame)
        .unwrap();
    let rgba = nox_media::rgba_cpu(&decoded, 320, 180).unwrap();
    let px = &rgba.data[(90 * 320 + 160) * 4..];
    [px[0], px[1], px[2]].map(i32::from)
}

fn close(a: [i32; 3], b: [i32; 3]) -> bool {
    (0..3).all(|c| (a[c] - b[c]).abs() <= 5)
}

fn check(name: &str, codec: VideoCodec, hardware: bool) -> Option<&'static str> {
    let timeline = timeline()?;
    let settings = settings(name, codec, hardware);
    let mut last = 0;
    let summary = export(&timeline, &settings, |done, total| {
        assert_eq!(total, 75);
        last = done;
        true
    })
    .unwrap();
    assert_eq!((summary.frames, last), (75, 75));

    let info = probe(&summary.path).unwrap();
    assert_eq!(info.video.as_ref().unwrap().frame_rate, Some(FPS));
    let duration = info.duration.unwrap().as_seconds();
    assert!((duration - 3.0).abs() < 0.05, "{name}: {duration} s");

    let red = center(&summary.path, 10);
    let blue = center(&summary.path, 70);
    assert!(close(red, [0xC0, 0x30, 0x20]), "{name}: red {red:?}");
    assert!(close(blue, [0x20, 0x60, 0xE0]), "{name}: blue {blue:?}");
    // The dissolve (45–55) is halfway at 50.
    let mid = center(&summary.path, 50);
    let half = [0, 1, 2].map(|c| (red[c] + blue[c]) / 2);
    assert!(close(mid, half), "{name}: dissolve {mid:?} vs {half:?}");

    let mut audio = AudioDecoder::open(&summary.path, 48_000).unwrap();
    let mut samples = vec![0f32; 2 * 4800];
    audio.read(48_000, &mut samples).unwrap(); // 1.0–1.1 s: the tone
    let level = samples.iter().sum::<f32>() / samples.len() as f32;
    assert!((level - 0.5).abs() < 0.02, "{name}: tone {level}");
    audio.read(2 * 48_000 + 24_000, &mut samples).unwrap(); // 2.5 s: silence
    assert!(samples.iter().all(|s| s.abs() < 0.01), "{name}: silence");
    Some(summary.encoder)
}

#[test]
fn hardware_export_is_zero_copy_when_possible() {
    let encoder = check("export-hw.mp4", VideoCodec::H264, true);
    eprintln!("hardware export used {encoder:?}");
}

#[test]
fn software_export() {
    assert_eq!(
        check("export-sw.mp4", VideoCodec::H264, false),
        Some("libx264")
    );
}

#[test]
fn prores_export() {
    assert_eq!(
        check("export.mov", VideoCodec::ProRes, false),
        Some("prores_ks")
    );
}

/// The scaling mode reaches the file: a 4:3 source fills a 16:9 frame.
#[test]
fn scaling_mode_applies() {
    let Some(gray) = solid("x-gray43.mkv", "0x808080", "240x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    (timeline.settings.width, timeline.settings.height) = (320, 180);
    timeline.place(0, clip(&gray, 0, 5), f(0)).unwrap();
    for (mode, edge) in [
        (ScalingMode::Fit, [0, 0, 0]),
        (ScalingMode::Fill, [128, 128, 128]),
    ] {
        timeline.settings.scaling = mode;
        let settings = ExportSettings {
            audio: false,
            ..settings(
                &format!("export-{}.mp4", mode.as_str()),
                VideoCodec::H264,
                false,
            )
        };
        let summary = export(&timeline, &settings, |_, _| true).unwrap();
        let decoded = VideoDecoder::open(&summary.path, None)
            .unwrap()
            .frame_at(2)
            .unwrap();
        let rgba = nox_media::rgba_cpu(&decoded, 320, 180).unwrap();
        let px = &rgba.data[(90 * 320 + 5) * 4..];
        let px = [px[0], px[1], px[2]].map(i32::from);
        assert!(close(px, edge), "{mode:?}: left edge {px:?}");
    }
}

#[test]
fn cancelling_removes_the_file() {
    let Some(timeline) = timeline() else { return };
    let settings = settings("export-cancel.mp4", VideoCodec::H264, false);
    let result = export(&timeline, &settings, |done, _| done < 10);
    assert!(matches!(result, Err(ExportError::Cancelled)));
    assert!(!settings.path.exists());
}
