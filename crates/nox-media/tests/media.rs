//! Probing and thumbnails against clips generated with the `ffmpeg` CLI.
//! Tests are skipped (with a message) when `ffmpeg` is not on PATH.

mod common;

use common::fixture;
use nox_media::{Error, probe, thumbnail};
use nox_project::Rational;
use std::path::{Path, PathBuf};

fn clip() -> Option<PathBuf> {
    fixture(
        "clip.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-movflags",
            "+write_colr",
        ],
    )
}

#[test]
fn probe_video_with_audio() {
    let Some(path) = clip() else { return };
    let info = probe(&path).unwrap();

    let video = info.video.as_ref().expect("video stream");
    assert_eq!(video.codec, "mpeg4");
    assert_eq!((video.width, video.height), (320, 240));
    assert_eq!(video.frame_rate, Some(Rational::new(25, 1)));
    assert_eq!(video.pixel_format, "yuv420p");
    assert_eq!(video.color.primaries, 1);
    assert_eq!(video.color.matrix, 1);

    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].sample_rate, 48000);
    assert_eq!(info.audio[0].channels, 2);

    let seconds = info.duration.expect("duration").as_seconds();
    assert!((seconds - 2.0).abs() < 0.05, "duration {seconds}");
    assert!(!info.is_still_image());
}

#[test]
fn probe_audio_only() {
    let Some(path) = fixture(
        "tone.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-c:a",
            "pcm_s16le",
        ],
    ) else {
        return;
    };
    let info = probe(&path).unwrap();
    assert!(info.video.is_none());
    assert_eq!(info.audio[0].sample_rate, 44100);
    assert!(matches!(
        thumbnail(&path, 160, 90),
        Err(Error::NoVideoStream)
    ));
}

#[test]
fn probe_still_image() {
    let Some(path) = fixture(
        "still.png",
        &["-f", "lavfi", "-i", "testsrc2=size=64x48", "-frames:v", "1"],
    ) else {
        return;
    };
    let info = probe(&path).unwrap();
    assert!(info.is_still_image());
    assert_eq!(info.video.unwrap().frame_rate, None);
    let image = thumbnail(&path, 160, 90).unwrap();
    assert_eq!((image.width, image.height), (64, 48), "never upscaled");
}

#[test]
fn thumbnail_fits_and_keeps_aspect() {
    let Some(path) = clip() else { return };
    let image = thumbnail(&path, 160, 90).unwrap();
    assert_eq!((image.width, image.height), (120, 90));
    assert_eq!(image.stride, 120 * 4);
    assert_eq!(image.data.len(), image.stride * 90);
    // testsrc2 is colorful; a black or blank frame would mean decoding failed.
    let distinct: std::collections::HashSet<_> =
        image.data.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
    assert!(
        distinct.len() > 16,
        "only {} distinct colors",
        distinct.len()
    );
}

#[test]
fn probe_rejects_non_media() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("not-media.txt");
    std::fs::write(&path, "this is not a video").unwrap();
    assert!(probe(&path).is_err());
}

/// Every common still format must give a thumbnail. JPEG goes through the
/// `image2` demuxer, which reports a one-frame duration; seeking into it used
/// to skip the only frame.
#[test]
fn image_thumbnails() {
    for name in [
        "still.jpg",
        "still.webp",
        "still.tif",
        "still.bmp",
        "alpha.png",
    ] {
        let pix_fmt = if name == "alpha.png" { "rgba" } else { "rgb24" };
        let args = [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48",
            "-frames:v",
            "1",
            "-pix_fmt",
            pix_fmt,
        ];
        let Some(path) = fixture(name, &args) else {
            return;
        };
        let info = probe(&path).unwrap();
        assert!(info.is_still_image(), "{name} should be a still");
        let image = thumbnail(&path, 160, 90).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((image.width, image.height), (64, 48), "{name}");
    }
}
