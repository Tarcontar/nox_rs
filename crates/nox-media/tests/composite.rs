//! GPU compositing: layers placed, blended and output like a CPU reference.

mod common;

use common::fixture;
use nox_color::{InputTransform, OutputTransform};
use nox_gpu::{BlendMode, Compositor, Converter, LayerStyle, Rect, Transform};
use nox_media::{Accel, VideoDecoder, ffmpeg, with_cpu_frame};
use std::sync::{Arc, OnceLock};

fn gpu() -> Option<(Accel, Arc<nox_gpu::GpuDevice>)> {
    static GPU: OnceLock<Option<(Accel, Arc<nox_gpu::GpuDevice>)>> = OnceLock::new();
    GPU.get_or_init(|| match Accel::new() {
        Ok(accel) => Some((accel.clone(), accel.gpu().clone())),
        Err(err) if std::env::var_os("NOX_REQUIRE_GPU").is_some() => {
            panic!("no Vulkan GPU ({err}), but NOX_REQUIRE_GPU is set")
        }
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
            compositor.add_layer(cpu, full, LayerStyle::default())
        })
        .unwrap()
        .unwrap();
        with_cpu_frame(&blue, |cpu| {
            compositor.add_layer(cpu, center, LayerStyle::blend(opacity as f32, mode))
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
        compositor.add_layer(cpu, full, LayerStyle::default())
    })
    .unwrap()
    .unwrap();
    with_cpu_frame(&blue, |cpu| {
        compositor.add_layer(cpu, center, LayerStyle::default())
    })
    .unwrap()
    .unwrap();
    with_cpu_frame(&gray, |cpu| {
        compositor.add_layer(cpu, center, LayerStyle::blend(0.5, BlendMode::Normal))
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
        compositor.add_layer(cpu, boxed, LayerStyle::default())
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
        let expected =
            nox_media::with_gpu_input(&frame, None, |input| converter.convert(input, w, h))
                .unwrap()
                .unwrap();
        compositor.begin(w, h).unwrap();
        nox_media::with_gpu_input(&frame, None, |input| {
            compositor.add_layer(input, Rect::fit(16.0 / 9.0, w, h), LayerStyle::default())
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

    // DMA-BUF is Linux only.
    #[cfg(target_os = "linux")]
    {
        if let Err(err) = compositor.enable_dmabuf(&[
            (nox_gpu::FOURCC_ABGR16161616F, 0),
            (nox_gpu::FOURCC_ABGR8888, 0),
        ]) {
            eprintln!("skipping DMA-BUF part: {err}");
            return;
        }
        compositor.begin(w, h).unwrap();
        let frame = decoder.frame_at(2).unwrap();
        nox_media::with_gpu_input(&frame, None, |input| {
            compositor.add_layer(input, Rect::fit(16.0 / 9.0, w, h), LayerStyle::default())
        })
        .unwrap()
        .unwrap();
        let exported = compositor.finish_dmabuf().unwrap();
        assert_eq!((exported.width, exported.height), (w, h));
    }
}

/// Scopes of solid colours land where the BT.709 signal says: gray on one
/// waveform row and the vectorscope's centre, red off-centre towards Cr.
#[test]
fn scopes_measure_the_output_signal() {
    let Some((_, gpu)) = gpu() else { return };
    let (Some(gray), Some(red)) = (
        solid("scope-gray.nut", "0x808080", "64x36"),
        solid("scope-red.nut", "0xC03020", "64x36"),
    ) else {
        return;
    };
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let g = color_of(&mut converter, &gray);
    let r = color_of(&mut converter, &red);
    let mut compositor = Compositor::new(gpu).unwrap();
    compositor.set_scopes(true).unwrap();
    let alpha = |image: &nox_gpu::Image, x: u32, y: u32| {
        image.data[y as usize * image.stride + x as usize * 4 + 3]
    };
    let level = |v: f64| (255 - (v * 255.0).round() as i64) as u32;

    compositor.begin(128, 72).unwrap();
    with_cpu_frame(&gray, |cpu| {
        compositor.add_layer(cpu, Rect::fit(16.0 / 9.0, 128, 72), LayerStyle::default())
    })
    .unwrap()
    .unwrap();
    compositor.finish().unwrap();
    let scopes = compositor.take_scopes().expect("scopes");
    let row = level(g[1]);
    for x in [0, 200, 511] {
        assert!(
            alpha(&scopes.waveform, x, row) > 200,
            "waveform at {x},{row}"
        );
        assert_eq!(alpha(&scopes.waveform, x, row.saturating_sub(10)), 0);
    }
    for x in [10, 300, 700] {
        assert!(alpha(&scopes.parade, x, row) > 200, "parade at {x},{row}");
    }
    // Neutral: the centre of the vectorscope.
    let centre = (127..=128).flat_map(|x| (127..=128).map(move |y| (x, y)));
    assert!(
        centre
            .clone()
            .any(|(x, y)| alpha(&scopes.vectorscope, x, y) > 200)
    );
    assert_eq!(alpha(&scopes.vectorscope, 200, 40), 0);
    // One histogram bin per channel at the gray level, full height.
    let bin = (g[0] * 255.0).round() as u32;
    assert!(
        alpha(&scopes.histogram, bin, 0) > 0,
        "histogram bar at {bin}"
    );
    assert_eq!(alpha(&scopes.histogram, (bin + 40) % 256, 64), 0);

    compositor.begin(128, 72).unwrap();
    with_cpu_frame(&red, |cpu| {
        compositor.add_layer(cpu, Rect::fit(16.0 / 9.0, 128, 72), LayerStyle::default())
    })
    .unwrap()
    .unwrap();
    compositor.finish().unwrap();
    let scopes = compositor.take_scopes().expect("scopes");
    let y = 0.2126 * r[0] + 0.7152 * r[1] + 0.0722 * r[2];
    let (cb, cr) = ((r[2] - y) / 1.8556, (r[0] - y) / 1.5748);
    let vx = ((cb + 0.5) * 255.0).round() as u32;
    let vy = 255 - ((cr + 0.5) * 255.0).round() as u32;
    assert!(cr > 0.1 && cb < 0.0, "red is Cr+, Cb-: {cb} {cr}");
    let near = (vx - 1..=vx + 1).flat_map(|x| (vy - 1..=vy + 1).map(move |y| (x, y)));
    assert!(
        near.clone()
            .any(|(x, y)| alpha(&scopes.vectorscope, x, y) > 200),
        "vectorscope near {vx},{vy}"
    );
    // Parade: red's R column high, its B column low.
    assert!(alpha(&scopes.parade, 10, level(r[0])) > 200);
    assert!(alpha(&scopes.parade, 2 * 256 + 10, level(r[2])) > 200);

    // Disabled: no scopes.
    compositor.set_scopes(false).unwrap();
    compositor.begin(32, 18).unwrap();
    compositor.finish().unwrap();
    assert!(compositor.take_scopes().is_none());
}

/// Graded layers match the CPU reference (`nox_color::Grade`): the grade
/// runs in ACEScct inside the blend pass.
#[test]
fn grades_match_the_cpu_reference() {
    use nox_color::{Grade, Wheel};
    let Some((_, gpu)) = gpu() else { return };
    let Some(frame) = solid("grade-teal.nut", "0x406080", "64x36") else {
        return;
    };
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let signal = color_of(&mut converter, &frame);
    let mut compositor = Compositor::new(gpu).unwrap();
    let grades = [
        Grade {
            lift: Wheel {
                rgb: [0.02, 0.0, -0.01],
                master: 0.01,
            },
            ..Grade::NEUTRAL
        },
        Grade {
            gamma: Wheel {
                rgb: [1.0, 1.1, 0.9],
                master: 1.2,
            },
            ..Grade::NEUTRAL
        },
        Grade {
            gain: Wheel {
                rgb: [1.2, 1.0, 0.8],
                master: 1.1,
            },
            offset: Wheel {
                rgb: [0.0, 0.03, 0.0],
                master: -0.02,
            },
            ..Grade::NEUTRAL
        },
    ];
    for grade in grades {
        compositor.begin(64, 36).unwrap();
        let style = LayerStyle {
            grade,
            ..LayerStyle::default()
        };
        with_cpu_frame(&frame, |cpu| {
            compositor.add_layer(cpu, Rect::fit(16.0 / 9.0, 64, 36), style)
        })
        .unwrap()
        .unwrap();
        let image = compositor.finish().unwrap();
        let linear = InputTransform::default().apply(signal.map(|c| c as f32));
        let expected = OutputTransform.apply(grade.apply(linear)).map(f64::from);
        assert_close(pixel(&image, 32, 18), expected, &format!("{grade:?}"));
        // The grade changed something.
        assert!(
            (0..3).any(|c| (f64::from(pixel(&image, 32, 18)[c]) - signal[c] * 255.0).abs() > 3.0),
            "{grade:?} had no effect"
        );
    }
}

/// Sources with straight alpha (an RGBA PNG title over video) blend by
/// their alpha: transparent pixels show the layer below.
#[test]
fn alpha_channels_blend() {
    let Some((_, gpu)) = gpu() else { return };
    // Blue at alpha 0 on the left half, at alpha 128/255 on the right.
    let input = "color=c=0x2060E0:size=64x36:rate=25:duration=0.2,format=rgba,\
                 geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,32),0,128)'";
    let (Some(red), Some(path)) = (
        solid("red-under-alpha.nut", "0xC03020", "64x36"),
        fixture("alpha.png", &["-f", "lavfi", "-i", input, "-frames:v", "1"]),
    ) else {
        return;
    };
    let title = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(0)
        .unwrap();
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let r = color_of(&mut converter, &red);
    let b = [0x20, 0x60, 0xE0].map(|v| f64::from(v) / 255.0);
    let linear = |v: [f64; 3]| {
        InputTransform::default()
            .apply(v.map(|c| c as f32))
            .map(f64::from)
    };
    let (lr, lb) = (linear(r), linear(b));
    let t = 128.0 / 255.0;
    let half = OutputTransform
        .apply([0, 1, 2].map(|c| (lr[c] + (lb[c] - lr[c]) * t) as f32))
        .map(f64::from);

    let mut compositor = Compositor::new(gpu).unwrap();
    let full = Rect::fit(16.0 / 9.0, 64, 36);
    compositor.begin(64, 36).unwrap();
    for frame in [&red, &title] {
        with_cpu_frame(frame, |cpu| {
            compositor.add_layer(cpu, full, LayerStyle::default())
        })
        .unwrap()
        .unwrap();
    }
    let image = compositor.finish().unwrap();
    assert_close(pixel(&image, 10, 18), r, "transparent");
    assert_close(pixel(&image, 50, 18), half, "half transparent");
}

/// Transformed layers: moved and scaled, rotated, and cropped, sampled
/// back from the layer (a solid color stays exact under bilinear sampling).
#[test]
fn transforms_move_scale_rotate_and_crop() {
    let Some((_, gpu)) = gpu() else { return };
    let (Some(red), Some(blue)) = (
        solid("red-under-transform.nut", "0xC03020", "64x36"),
        solid("blue-transformed.nut", "0x2060E0", "64x36"),
    ) else {
        return;
    };
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let (r, b) = (
        color_of(&mut converter, &red),
        color_of(&mut converter, &blue),
    );
    let mut compositor = Compositor::new(gpu).unwrap();
    let full = Rect::fit(16.0 / 9.0, 128, 72);
    let mut draw = |transform: Transform| {
        compositor.begin(128, 72).unwrap();
        with_cpu_frame(&red, |cpu| {
            compositor.add_layer(cpu, full, LayerStyle::default())
        })
        .unwrap()
        .unwrap();
        let style = LayerStyle {
            transform,
            ..LayerStyle::default()
        };
        with_cpu_frame(&blue, |cpu| compositor.add_layer(cpu, full, style))
            .unwrap()
            .unwrap();
        compositor.finish().unwrap()
    };

    // Half size, moved right by a quarter of the width: x 64–128, y 18–54.
    let image = draw(Transform {
        offset: [0.25, 0.0],
        scale: 0.5,
        ..Transform::IDENTITY
    });
    assert_close(pixel(&image, 100, 36), b, "moved and scaled");
    assert_close(pixel(&image, 40, 36), r, "left of it");
    assert_close(pixel(&image, 100, 10), r, "above it");

    // Half size and turned a quarter: 36 wide and 64 tall, centered.
    let image = draw(Transform {
        scale: 0.5,
        rotation: 90.0,
        ..Transform::IDENTITY
    });
    assert_close(pixel(&image, 64, 6), b, "rotated, top");
    assert_close(pixel(&image, 40, 36), r, "rotated, beside it");

    // The left half cut off; the right half stays where it was.
    let image = draw(Transform {
        crop: [0.5, 0.0, 0.0, 0.0],
        ..Transform::IDENTITY
    });
    assert_close(pixel(&image, 20, 36), r, "cropped away");
    assert_close(pixel(&image, 100, 36), b, "kept");

    // Moved entirely out of the picture: nothing drawn.
    let image = draw(Transform {
        offset: [1.5, 0.0],
        ..Transform::IDENTITY
    });
    assert_close(pixel(&image, 64, 36), r, "moved out");
}

/// Linear 4:2:0 buffers, the frames shared with other APIs (NVENC, NVDEC),
/// both ways: the compositor writes a picture into a buffer as NV12 or P010
/// (as for an encoder), and the converter reads it back (as from a decoder)
/// to the color it came in with.
#[test]
fn linear_buffers_round_trip() {
    use nox_color::ycbcr::{self, Coefficients, Quantization};
    use nox_gpu::{
        BufferFrame, BufferTarget, LayerStyle, LinearFormat, LinearLayout, MemoryHandle,
        SharedBuffer,
    };
    let Some((_, gpu)) = gpu() else { return };
    let Some(handle) = MemoryHandle::native() else {
        eprintln!("skipping: no memory sharing on this platform");
        return;
    };
    let Some(red) = solid("linear-red.nut", "0xC03020", "64x36") else {
        return;
    };
    let mut converter = Converter::new(gpu.clone()).unwrap();
    let expected = color_of(&mut converter, &red);
    let mut compositor = Compositor::new(gpu.clone()).unwrap();
    for (format, quantization) in [
        (LinearFormat::Nv12, Quantization::eight_bit(false)),
        (
            LinearFormat::P010,
            Quantization {
                bit_depth: 10,
                container_bits: 16,
                msb_aligned: true,
                full_range: false,
            },
        ),
    ] {
        let layout = LinearLayout::packed(format, 64, 36, 256);
        let buffer = match SharedBuffer::new(&gpu, layout.size(), handle) {
            Ok(buffer) => buffer,
            Err(err) => {
                eprintln!("skipping: {err}");
                return;
            }
        };
        compositor.begin(64, 36).unwrap();
        with_cpu_frame(&red, |cpu| {
            compositor.add_layer(cpu, Rect::fit(16.0 / 9.0, 64, 36), LayerStyle::default())
        })
        .unwrap()
        .unwrap();
        compositor
            .finish_to_buffer(
                &BufferTarget {
                    buffer: &buffer,
                    layout,
                    matrix: ycbcr::rgb_to_ycbcr(Coefficients::BT709, quantization),
                },
                false,
            )
            .unwrap();
        let image = converter
            .convert(
                &BufferFrame {
                    buffer: &buffer,
                    layout,
                    width: 64,
                    height: 36,
                    matrix: ycbcr::ycbcr_to_rgb(Coefficients::BT709, quantization),
                    // The compositor writes left-sited chroma.
                    chroma_offset: [0.5, 0.0],
                    color: InputTransform::default(),
                    orientation: Default::default(),
                },
                64,
                36,
            )
            .unwrap();
        assert_close(pixel(&image, 32, 18), expected, &format!("{format:?}"));
    }
}
