//! Writing files: what goes in comes back out when decoded.

mod common;

use nox_media::{
    AudioDecoder, EncodeQuality, Image, MediaWriter, OutputSettings, VideoCodec, VideoDecoder,
    probe,
};
use nox_project::Rational;
use std::path::PathBuf;

const RATE: u32 = 48_000;

/// A solid frame whose color encodes its index.
fn frame(index: i64, width: u32, height: u32) -> Image {
    let shade = (40 + index * 6) as u8;
    let pixel = [shade, 255 - shade, 128, 255];
    Image {
        width,
        height,
        stride: width as usize * 4,
        data: pixel.repeat(width as usize * height as usize),
    }
}

fn out(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Writes 30 frames at 25 fps with 1.2 s of a 0.25 constant in both
/// channels, then checks the result.
fn round_trip(
    name: &str,
    codec: VideoCodec,
    hardware: bool,
    tolerance: i32,
) -> Option<&'static str> {
    let path = out(name);
    let settings = OutputSettings {
        path: path.clone(),
        width: 320,
        height: 180,
        frame_rate: Rational::new(25, 1),
        codec,
        quality: EncodeQuality::High,
        hardware,
        encoder: None,
        accel: None,
        audio_rate: Some(RATE),
        encoding: nox_media::OutputEncoding::Sdr,
        peak_nits: 1000.0,
    };
    let mut writer = match MediaWriter::create(&settings) {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("skipping {name}: {err}");
            return None;
        }
    };
    let encoder = writer.video_encoder();
    let audio_per_frame = RATE as usize / 25 * 2;
    for index in 0..30 {
        writer.write_video(&frame(index, 320, 180)).unwrap();
        writer.write_audio(&vec![0.25; audio_per_frame]).unwrap();
    }
    writer.finish().unwrap();

    let info = probe(&path).unwrap();
    let video = info.video.as_ref().unwrap();
    assert_eq!((video.width, video.height), (320, 180), "{name}");
    assert_eq!(video.frame_rate, Some(Rational::new(25, 1)), "{name}");
    assert_eq!(video.color.primaries, 1, "{name}: BT.709 tags");
    let duration = info.duration.unwrap().as_seconds();
    assert!((duration - 1.2).abs() < 0.05, "{name}: {duration} s");

    // Colors survive the round trip (RGB → Y'CbCr → RGB).
    let mut decoder = VideoDecoder::open(&path, None).unwrap();
    for index in [0, 7, 29] {
        let decoded = decoder.frame_at(index).unwrap();
        let rgba = nox_media::rgba_cpu(&decoded, 320, 180).unwrap();
        let center = &rgba.data[(90 * 320 + 160) * 4..][..3];
        let expected = &frame(index, 1, 1).data[..3];
        for c in 0..3 {
            let diff = (i32::from(center[c]) - i32::from(expected[c])).abs();
            assert!(
                diff <= tolerance,
                "{name} frame {index}: {center:?} vs {expected:?}"
            );
        }
    }

    let mut audio = AudioDecoder::open(&path, RATE).unwrap();
    let mut samples = vec![0f32; 2 * 4800];
    audio.read(24_000, &mut samples).unwrap();
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    assert!((mean - 0.25).abs() < 0.01, "{name}: audio level {mean}");
    Some(encoder)
}

#[test]
fn h264_software() {
    assert_eq!(
        round_trip("x264.mp4", VideoCodec::H264, false, 4),
        Some("libx264")
    );
}

#[test]
fn h264_hardware_or_fallback() {
    let encoder = round_trip("h264-hw.mp4", VideoCodec::H264, true, 4);
    eprintln!("hardware H.264 encoder: {encoder:?}");
    assert!(matches!(encoder, Some("h264_nvenc" | "libx264")));
}

#[test]
fn hevc() {
    round_trip("hevc.mp4", VideoCodec::Hevc, true, 4);
}

#[test]
fn prores_with_pcm_audio() {
    assert_eq!(
        round_trip("prores.mov", VideoCodec::ProRes, true, 3),
        Some("prores_ks")
    );
}

#[test]
fn odd_sizes_are_rejected() {
    let settings = OutputSettings {
        path: out("odd.mp4"),
        width: 321,
        height: 180,
        frame_rate: Rational::new(25, 1),
        codec: VideoCodec::H264,
        quality: EncodeQuality::Medium,
        hardware: false,
        encoder: None,
        accel: None,
        audio_rate: None,
        encoding: nox_media::OutputEncoding::Sdr,
        peak_nits: 1000.0,
    };
    assert!(MediaWriter::create(&settings).is_err());
}

/// Vulkan Video encoding with the compositor writing into the encoder's own
/// input images: what goes in comes out.
#[test]
fn vulkan_encode_without_copies() {
    gpu_encode("h264_vulkan", "vulkan.mp4");
}

#[test]
fn nvenc_encode_from_gpu_memory() {
    gpu_encode("h264_nvenc", "nvenc.mp4");
}

/// Encodes test frames with `encoder`, which takes them on the GPU, and
/// checks what comes out.
fn gpu_encode(encoder: &str, name: &str) {
    use nox_gpu::{Compositor, CpuFrame, LayerStyle, Layout, Plane, PlaneFormat, Rect};
    let Ok(accel) = nox_media::Accel::new() else {
        eprintln!("skipping: no Vulkan device");
        return;
    };
    let path = out(name);
    let settings = OutputSettings {
        path: path.clone(),
        width: 320,
        height: 180,
        frame_rate: Rational::new(25, 1),
        codec: VideoCodec::H264,
        quality: EncodeQuality::High,
        hardware: true,
        encoder: Some(encoder.into()),
        accel: Some(accel.clone()),
        audio_rate: None,
        encoding: nox_media::OutputEncoding::Sdr,
        peak_nits: 1000.0,
    };
    let mut writer = match MediaWriter::create(&settings) {
        Ok(writer) if writer.takes_gpu_frames() => writer,
        Ok(_) => {
            eprintln!("skipping: {encoder} takes frames from memory");
            return;
        }
        Err(err) => {
            eprintln!("skipping: {err}");
            return;
        }
    };
    assert_eq!(writer.video_encoder(), encoder);
    let mut compositor = Compositor::new(accel.gpu().clone()).unwrap();
    for index in 0..30 {
        let image = frame(index, 320, 180);
        let cpu = CpuFrame {
            width: 320,
            height: 180,
            layout: Layout::Packed,
            swap_chroma: false,
            matrix: nox_color::ycbcr::rgb_to_rgb(nox_color::Quantization::eight_bit(true)),
            chroma_offset: [0.0, 0.0],
            color: Default::default(),
            planes: vec![Plane {
                format: PlaneFormat::Rgba8,
                width: 320,
                height: 180,
                data: &image.data,
                stride: image.stride,
            }],
        };
        compositor.begin(320, 180).unwrap();
        compositor
            .add_layer(&cpu, Rect::fit(16.0 / 9.0, 320, 180), LayerStyle::default())
            .unwrap();
        writer
            .write_gpu(|target, measure| compositor.finish_to_gpu(target, measure))
            .unwrap();
    }
    writer.finish().unwrap();

    let info = probe(&path).unwrap();
    let video = info.video.as_ref().unwrap();
    assert_eq!((video.width, video.height), (320, 180));
    assert_eq!(video.frame_rate, Some(Rational::new(25, 1)));
    assert_eq!(video.color.primaries, 1, "BT.709 tags");
    let mut decoder = VideoDecoder::open(&path, None).unwrap();
    for index in [0, 7, 29] {
        let decoded = decoder.frame_at(index).unwrap();
        let rgba = nox_media::rgba_cpu(&decoded, 320, 180).unwrap();
        let center = &rgba.data[(90 * 320 + 160) * 4..][..3];
        let expected = &frame(index, 1, 1).data[..3];
        for c in 0..3 {
            let diff = (i32::from(center[c]) - i32::from(expected[c])).abs();
            assert!(diff <= 4, "frame {index}: {center:?} vs {expected:?}");
        }
    }
}
