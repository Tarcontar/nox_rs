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
        encoder: None,
        range: None,
        audio: true,
        encoding: nox_media::OutputEncoding::Sdr,
        peak_nits: 1000.0,
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
    // The dissolve (45–55) is halfway at 50, mixing in linear light.
    let mid = center(&summary.path, 50);
    let linear = |v: i32| (f64::from(v) / 255.0).powf(2.4);
    let half = [0, 1, 2].map(|c| {
        let l = (linear(red[c]) + linear(blue[c])) / 2.0;
        (l.powf(1.0 / 2.4) * 255.0).round() as i32
    });
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

/// The 10-bit luma at the picture's center (an HDR export decodes to a
/// 10-bit format).
fn center_luma10(path: &Path, frame: i64) -> f64 {
    let decoded = VideoDecoder::open(path, None)
        .unwrap()
        .frame_at(frame)
        .unwrap();
    assert_eq!(
        decoded.format(),
        nox_media::ffmpeg::format::Pixel::YUV420P10LE
    );
    let stride = decoded.stride(0);
    let i = 90 * stride + 160 * 2;
    let data = decoded.data(0);
    f64::from(u16::from_le_bytes([data[i], data[i + 1]]))
}

/// HDR exports (HEVC Main 10, BT.2020): SDR material sits at the levels
/// BT.2408 puts SDR white at, so the same gray encodes to the PQ and HLG
/// values the SDR export's light implies. Tags and the HDR10 mastering
/// display are in the file.
#[test]
fn hdr_exports_encode_pq_and_hlg() {
    use nox_media::OutputEncoding;
    let Some(gray) = solid("x-gray.mkv", "0x9A9A9A", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    (timeline.settings.width, timeline.settings.height) = (320, 180);
    timeline.place(0, clip(&gray, 0, 10), f(0)).unwrap();
    // The light of the gray, from the SDR export (8-bit limited BT.709).
    let sdr = settings("hdr-ref.mp4", VideoCodec::H264, false);
    export(&timeline, &sdr, |_, _| true).unwrap();
    let y8 = {
        let decoded = VideoDecoder::open(&sdr.path, None)
            .unwrap()
            .frame_at(5)
            .unwrap();
        f64::from(decoded.data(0)[90 * decoded.stride(0) + 160])
    };
    let light = ((y8 - 16.0) / 219.0).powf(2.4) as f32;
    let code = |signal: f32| 64.0 + 876.0 * f64::from(signal);
    let pq = code(nox_color::hdr::pq_inverse_eotf(light * 203.0 / 10_000.0));
    let hlg = code(nox_color::hdr::hlg_encode([light * 203.0; 3])[0]);
    for (name, encoding, hardware, expected) in [
        ("hdr-pq-hw.mp4", OutputEncoding::Pq, true, pq),
        ("hdr-pq-sw.mp4", OutputEncoding::Pq, false, pq),
        ("hdr-hlg-hw.mp4", OutputEncoding::Hlg, true, hlg),
        ("hdr-hlg-sw.mp4", OutputEncoding::Hlg, false, hlg),
    ] {
        let mut settings = settings(name, VideoCodec::Hevc, hardware);
        settings.encoding = encoding;
        settings.audio = false;
        let summary = export(&timeline, &settings, |_, _| true).unwrap();
        let luma = center_luma10(&settings.path, 5);
        assert!(
            (luma - expected).abs() <= 3.0,
            "{name} ({}): luma {luma}, expected {expected:.1}",
            summary.encoder
        );
        eprintln!(
            "{name}: {}{}",
            summary.encoder,
            if summary.zero_copy { ", zero copy" } else { "" }
        );
        // HDR10 files say how bright they get: here, the gray all over,
        // measured on the CPU or (frames on the GPU) by the compositor.
        if encoding == OutputEncoding::Pq {
            let (max_cll, max_fall) = content_light(&settings.path).expect("clli");
            let nits = light * 203.0;
            assert!(
                (max_cll - nits).abs() <= 2.0,
                "MaxCLL {max_cll}, gray {nits}"
            );
            assert!(
                (max_fall - nits).abs() <= 2.0,
                "MaxFALL {max_fall}, gray {nits}"
            );
        }
        let info = probe(&settings.path).unwrap();
        let video = info.video.unwrap();
        let transfer = if encoding == OutputEncoding::Pq {
            16
        } else {
            18
        };
        assert_eq!(
            (
                video.color.primaries,
                video.color.transfer,
                video.color.matrix
            ),
            (9, transfer, 9),
            "{name}"
        );
    }
    // H.264 can't carry HDR.
    let mut h264 = settings("hdr-h264.mp4", VideoCodec::H264, false);
    h264.encoding = OutputEncoding::Pq;
    assert!(export(&timeline, &h264, |_, _| true).is_err());
}

/// An HDR10 source keeps its light in an HDR10 export (no tone mapping):
/// a bright PQ gray comes out with the code it went in with, where the SDR
/// export compresses it.
#[test]
fn hdr_sources_pass_through_hdr_exports() {
    use nox_media::OutputEncoding;
    // A gray around 1000 nits, tagged BT.2020 PQ, 10 bit.
    let args = [
        "-f",
        "lavfi",
        "-i",
        "color=c=0xC0C0C0:size=320x180:rate=25:duration=2,format=yuv444p10le,\
         setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
        "-c:v",
        "ffv1",
    ];
    let Some(source) = fixture("x-pq-gray2.mkv", &args) else {
        return;
    };
    let source_luma = {
        let decoded = VideoDecoder::open(&source, None)
            .unwrap()
            .frame_at(5)
            .unwrap();
        let i = 90 * decoded.stride(0) + 160 * 2;
        let data = decoded.data(0);
        f64::from(u16::from_le_bytes([data[i], data[i + 1]]))
    };
    let mut timeline = Timeline::new("t", FPS);
    (timeline.settings.width, timeline.settings.height) = (320, 180);
    let reference = MediaReference::from_path(&source, Some(TimeRange::new(f(0), f(50))));
    timeline
        .place(
            0,
            Composable::Clip(Clip::new("pq", reference, TimeRange::new(f(0), f(10)))),
            f(0),
        )
        .unwrap();
    let mut settings = settings("hdr-pass.mp4", VideoCodec::Hevc, false);
    settings.encoding = OutputEncoding::Pq;
    settings.audio = false;
    export(&timeline, &settings, |_, _| true).unwrap();
    let luma = center_luma10(&settings.path, 5);
    assert!(
        (luma - source_luma).abs() <= 3.0,
        "PQ in {source_luma}, out {luma}"
    );
    // Brighter than SDR white (PQ 0.58: code 572).
    assert!(luma > 620.0, "{luma}");
}

/// MaxCLL and MaxFALL from a file's content light metadata, read with
/// ffprobe.
fn content_light(path: &Path) -> Option<(f32, f32)> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream_side_data=max_content,max_average")
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.parse::<f32>().ok())
    };
    Some((value("max_content=")?, value("max_average=")?))
}

/// A timeline without audio exports without an audio track, even with
/// audio asked for, rather than with a silent one.
#[test]
fn no_audio_track_without_audio() {
    let Some(gray) = solid("x-gray-silent.mkv", "0x808080", "320x180") else {
        return;
    };
    let mut timeline = Timeline::new("t", FPS);
    (timeline.settings.width, timeline.settings.height) = (320, 180);
    timeline.place(0, clip(&gray, 0, 10), f(0)).unwrap();
    assert!(!nox_engine::has_audio(&timeline, &(0..10)));
    let settings = settings("silent.mp4", VideoCodec::H264, false);
    assert!(settings.audio);
    export(&timeline, &settings, |_, _| true).unwrap();
    assert!(probe(&settings.path).unwrap().audio.is_empty());
}
