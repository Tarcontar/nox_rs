//! Turned pictures (phone videos with a container rotation, photos with an
//! EXIF orientation) come out upright, as the `ffmpeg` tool shows them.

mod common;

use common::fixture;
use nox_gpu::Converter;
use nox_media::{Accel, Orientation, VideoDecoder, display_size, probe, rgba_cpu, with_cpu_frame};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 64×32: red left half with a green corner at the top left, blue right
/// half; lossless, so every pixel is known.
fn source() -> Option<PathBuf> {
    fixture(
        "orient-source.mov",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x32:d=1,\
             drawbox=x=32:y=0:w=32:h=32:color=blue:t=fill,\
             drawbox=x=0:y=0:w=8:h=8:color=lime:t=fill,format=yuv444p",
            "-frames:v",
            "1",
            "-c:v",
            "ffv1",
        ],
    )
}

/// `source` with the display matrix the `ffmpeg` options in `args` write.
fn turned(name: &str, args: &[&str]) -> Option<PathBuf> {
    let source = source()?;
    let mut all = args.to_vec();
    all.extend_from_slice(&["-i", source.to_str()?, "-c", "copy"]);
    fixture(name, &all)
}

/// The first frame as the `ffmpeg` tool shows it (rotated automatically),
/// as RGBA.
fn reference(path: &Path) -> Vec<u8> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn mean_difference(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "sizes differ");
    let total: u64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(a, b)| (0..3).map(|c| u64::from(a[c].abs_diff(b[c]))).sum::<u64>())
        .sum();
    total as f64 / (a.len() / 4 * 3) as f64
}

/// Every turn and mirror: the probed size, the CPU path and the GPU
/// converter all match the `ffmpeg` tool's upright picture.
#[test]
fn turned_videos_show_upright() {
    let cases: &[(&str, &[&str], Orientation)] = &[
        // Phones shot upright: stored landscape, turned clockwise.
        (
            "orient-cw.mov",
            &["-display_rotation", "-90"],
            turns(1, false),
        ),
        (
            "orient-180.mov",
            &["-display_rotation", "180"],
            turns(2, false),
        ),
        (
            "orient-ccw.mov",
            &["-display_rotation", "90"],
            turns(3, false),
        ),
        ("orient-hflip.mov", &["-display_hflip"], turns(0, true)),
        ("orient-vflip.mov", &["-display_vflip"], turns(2, true)),
        (
            "orient-cw-hflip.mov",
            &["-display_rotation", "-90", "-display_hflip"],
            turns(1, true),
        ),
    ];
    let gpu = match Accel::new() {
        Ok(accel) => Some(accel.gpu().clone()),
        Err(err) if std::env::var_os("NOX_REQUIRE_GPU").is_some() => {
            panic!("no Vulkan GPU ({err}), but NOX_REQUIRE_GPU is set")
        }
        Err(err) => {
            eprintln!("skipping the GPU half: {err}");
            None
        }
    };
    for &(name, args, expected) in cases {
        let Some(path) = turned(name, args) else {
            return;
        };
        let video = probe(&path).unwrap().video.unwrap();
        assert_eq!(video.orientation, expected, "{name}");
        let (w, h) = expected.display_size(64, 32);
        assert_eq!((video.width, video.height), (w, h), "{name}: probed size");

        let frame = VideoDecoder::open(&path, None)
            .unwrap()
            .frame_at(0)
            .unwrap();
        assert_eq!(display_size(&frame), (w, h), "{name}: frame size");
        let reference = reference(&path);
        assert_eq!(
            reference.len(),
            (w * h * 4) as usize,
            "{name}: ffmpeg's size"
        );

        let cpu = rgba_cpu(&frame, w, h).unwrap();
        let cpu_diff = mean_difference(&cpu.data, &reference);
        assert!(cpu_diff < 2.0, "{name}: CPU differs by {cpu_diff:.2}");

        if let Some(gpu) = &gpu {
            let mut converter = Converter::new(gpu.clone()).unwrap();
            let image = with_cpu_frame(&frame, |input| converter.convert(input, w, h))
                .unwrap()
                .unwrap();
            let data: Vec<u8> = image
                .data
                .chunks(image.stride)
                .flat_map(|row| &row[..w as usize * 4])
                .copied()
                .collect();
            let gpu_diff = mean_difference(&data, &reference);
            assert!(gpu_diff < 3.0, "{name}: GPU differs by {gpu_diff:.2}");
        }
    }
}

fn turns(quarter_turns: u8, mirror: bool) -> Orientation {
    Orientation {
        quarter_turns,
        mirror,
    }
}

/// A JPEG whose EXIF says "turn 90° clockwise" (orientation 6, as phones
/// write for upright shots) is probed and decoded upright.
#[test]
fn exif_orientation_of_photos() {
    let Some(plain) = fixture(
        "orient-plain.jpg",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x32,drawbox=x=0:y=0:w=8:h=8:color=lime:t=fill",
            "-frames:v",
            "1",
        ],
    ) else {
        return;
    };
    // An APP1 segment with a big-endian TIFF header and one IFD entry:
    // Orientation (0x0112), SHORT, 1 value, 6.
    let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01".to_vec();
    exif.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
    let jpeg = std::fs::read(&plain).unwrap();
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(exif.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&jpeg[2..]);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("orient-exif6.jpg");
    std::fs::write(&path, out).unwrap();

    let video = probe(&path).unwrap().video.unwrap();
    assert_eq!(video.orientation, turns(1, false));
    assert_eq!((video.width, video.height), (32, 64));
    let frame = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(0)
        .unwrap();
    let image = rgba_cpu(&frame, 32, 64).unwrap();
    // The green corner moves from the top left to the top right.
    let at = |x: usize, y: usize| &image.data[y * image.stride + x * 4..][..3];
    assert!(
        at(30, 1)[1] > 200 && at(30, 1)[0] < 60,
        "top right {:?}",
        at(30, 1)
    );
    assert!(at(1, 1)[0] > 200, "top left {:?}", at(1, 1));
}
