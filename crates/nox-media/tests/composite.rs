//! GPU compositing: layers placed, blended and output like a CPU reference.

mod common;

use common::fixture;
use nox_color::{InputTransform, OutputTransform};
use nox_gpu::{BlendMode, Compositor, Converter, Rect};
use nox_media::{HwDevice, VideoDecoder, ffmpeg, with_cpu_frame};
use std::sync::{Arc, OnceLock};

fn gpu() -> Option<(HwDevice, Arc<nox_gpu::GpuDevice>)> {
    static GPU: OnceLock<Option<(HwDevice, Arc<nox_gpu::GpuDevice>)>> = OnceLock::new();
    GPU.get_or_init(|| match HwDevice::vulkan() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            eprintln!("skipping GPU tests: {err}");
            None
        }
    })
    .clone()
}

/// A frame of a solid-color clip (4:4:4, so there are no chroma edges).
fn solid(name: &str, color: &str, size: &str) -> Option<ffmpeg::frame::Video> {
    let input = format!("color=c={color}:size={size}:rate=25:duration=0.2,format=yuv444p");
    let args = ["-f", "lavfi", "-i", &input, "-c:v", "rawvideo", "-f", "nut"];
    let path = fixture(name, &args)?;
    Some(
        VideoDecoder::open(&path, None)
            .unwrap()
            .frame_at(0)
            .unwrap(),
    )
}

/// The color the converter makes of a solid frame, as floats.
fn color_of(converter: &mut Converter, frame: &ffmpeg::frame::Video) -> [f64; 3] {
    let image = with_cpu_frame(frame, |cpu| converter.convert(cpu, 4, 4))
        .unwrap()
        .unwrap();
    [0, 1, 2].map(|c| f64::from(image.data[c]) / 255.0)
}

fn pixel(image: &nox_gpu::Image, x: u32, y: u32) -> [u8; 3] {
    let i = y as usize * image.stride + x as usize * 4;
    [image.data[i], image.data[i + 1], image.data[i + 2]]
}

fn assert_close(actual: [u8; 3], expected: [f64; 3], what: &str) {
    for c in 0..3 {
        let e = (expected[c].clamp(0.0, 1.0) * 255.0).round();
        assert!(
            (f64::from(actual[c]) - e).abs() <= 2.0,
            "{what}: got {actual:?}, expected {:?}",
            expected.map(|v| (v.clamp(0.0, 1.0) * 255.0).round())
        );
    }
}

#[test]
fn layers_blend_like_the_cpu_reference() {
    let Some((_, gpu)) = gpu() else { return };
    let (Some(red), Some(blue), Some(gray)) = (
        solid("red.nut", "0xC03020", "64x36"),
        solid("blue.nut", "0x2060E0", "64x36"),
        solid("gray.nut", "0x808080", "64x36"),
    ) else {
        return;
    };
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let (r, b, g) = (
        color_of(&mut converter, &red),
        color_of(&mut converter, &blue),
        color_of(&mut converter, &gray),
    );
    let mut compositor = Compositor::new(gpu).unwrap();
    let full = Rect::fit(16.0 / 9.0, 128, 72);
    let center = Rect {
        x: 32,
        y: 18,
        width: 64,
        height: 36,
    };
    // Layers blend in the linear working space (ACEScg): BT.709 sources
    // enter through the input transform, and the output transform encodes.
    let linear = |v: [f64; 3]| {
        InputTransform::default()
            .apply(v.map(|c| c as f32))
            .map(f64::from)
    };
    let encode = |v: [f64; 3]| OutputTransform.apply(v.map(|c| c as f32)).map(f64::from);
    let mix = |a: [f64; 3], b: [f64; 3], t: f64| {
        let (a, b) = (linear(a), linear(b));
        encode([0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * t))
    };
    let (lr, lb) = (linear(r), linear(b));

    for (mode, opacity, expected) in [
        (BlendMode::Normal, 0.5, mix(r, b, 0.5)),
        (BlendMode::Normal, 1.0, b),
        (
            BlendMode::Add,
            1.0,
            encode([0, 1, 2].map(|c| lr[c] + lb[c])),
        ),
        (
            BlendMode::Multiply,
            1.0,
            encode([0, 1, 2].map(|c| lr[c] * lb[c])),
        ),
        (
            BlendMode::Screen,
            0.75,
            encode([0, 1, 2].map(|c| {
                let screen = 1.0 - (1.0 - lr[c]) * (1.0 - lb[c]);
                lr[c] + (screen - lr[c]) * 0.75
            })),
        ),
    ] {
        compositor.begin(128, 72).unwrap();
        with_cpu_frame(&red, |cpu| {
            compositor.add_layer(cpu, full, 1.0, BlendMode::Normal)
        })
        .unwrap()
        .unwrap();
        with_cpu_frame(&blue, |cpu| {
            compositor.add_layer(cpu, center, opacity as f32, mode)
        })
        .unwrap()
        .unwrap();
        let image = compositor.finish().unwrap();
        assert_close(
            pixel(&image, 64, 36),
            expected,
            &format!("{mode:?} {opacity}"),
        );
        // Outside the top layer only the bottom one shows.
        assert_close(pixel(&image, 5, 5), r, &format!("{mode:?} outside"));
    }

    // A third layer on top, and a 4:3 picture pillarboxed over black.
    compositor.begin(128, 72).unwrap();
    with_cpu_frame(&red, |cpu| {
        compositor.add_layer(cpu, full, 1.0, BlendMode::Normal)
    })
    .unwrap()
    .unwrap();
    with_cpu_frame(&blue, |cpu| {
        compositor.add_layer(cpu, center, 1.0, BlendMode::Normal)
    })
    .unwrap()
    .unwrap();
    with_cpu_frame(&gray, |cpu| {
        compositor.add_layer(cpu, center, 0.5, BlendMode::Normal)
    })
    .unwrap()
    .unwrap();
    let image = compositor.finish().unwrap();
    assert_close(pixel(&image, 64, 36), mix(b, g, 0.5), "three layers");

    compositor.begin(128, 72).unwrap();
    let boxed = Rect::fit(4.0 / 3.0, 128, 72);
    assert_eq!(
        boxed,
        Rect {
            x: 16,
            y: 0,
            width: 96,
            height: 72
        }
    );
    with_cpu_frame(&gray, |cpu| {
        compositor.add_layer(cpu, boxed, 1.0, BlendMode::Normal)
    })
    .unwrap()
    .unwrap();
    let image = compositor.finish().unwrap();
    assert_close(pixel(&image, 5, 36), [0.0; 3], "pillarbox");
    assert_close(pixel(&image, 64, 36), g, "picture");

    // No layers: black.
    compositor.begin(32, 18).unwrap();
    let image = compositor.finish().unwrap();
    assert_eq!((image.width, image.height), (32, 18));
    assert_close(pixel(&image, 3, 3), [0.0; 3], "empty");
}

/// A single full-frame layer through the compositor matches a plain
/// conversion, both from Vulkan-decoded frames (zero copy) and into a
/// DMA-BUF.
#[test]
fn single_layer_matches_plain_conversion() {
    let Some((hw, gpu)) = gpu() else { return };
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=1",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "2", "-g", "8",
    ];
    let Some(path) = fixture("composite-h264.mp4", &args) else {
        return;
    };
    let mut decoder = VideoDecoder::open(&path, Some(&hw)).unwrap();
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let mut compositor = Compositor::new(gpu).unwrap();
    let (w, h) = (320, 180);
    for index in [0, 7, 3] {
        let frame = decoder.frame_at(index).unwrap();
        let expected = nox_media::with_gpu_input(&frame, |input| converter.convert(input, w, h))
            .unwrap()
            .unwrap();
        compositor.begin(w, h).unwrap();
        nox_media::with_gpu_input(&frame, |input| {
            compositor.add_layer(input, Rect::fit(16.0 / 9.0, w, h), 1.0, BlendMode::Normal)
        })
        .unwrap()
        .unwrap();
        let image = compositor.finish().unwrap();
        let max = image
            .data
            .iter()
            .zip(&expected.data)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(max <= 1, "frame {index}: max difference {max}");
    }

    if let Err(err) = compositor.enable_dmabuf(&[
        (nox_gpu::FOURCC_ABGR16161616F, 0),
        (nox_gpu::FOURCC_ABGR8888, 0),
    ]) {
        eprintln!("skipping DMA-BUF part: {err}");
        return;
    }
    compositor.begin(w, h).unwrap();
    let frame = decoder.frame_at(2).unwrap();
    nox_media::with_gpu_input(&frame, |input| {
        compositor.add_layer(input, Rect::fit(16.0 / 9.0, w, h), 1.0, BlendMode::Normal)
    })
    .unwrap()
    .unwrap();
    let exported = compositor.finish_dmabuf().unwrap();
    assert_eq!((exported.width, exported.height), (w, h));
}
