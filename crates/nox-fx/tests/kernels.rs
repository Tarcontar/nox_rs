//! The effects' kernels and graphs on the GPU, against CPU references
//! (blur, sharpen) or what they must do (keying, painterly, mattes,
//! masks). Skipped without a Vulkan device unless `NOX_REQUIRE_GPU` is
//! set.

use nox_fx::{Context, Effects, ParamValue, registry};
use nox_gpu::compute::{Buffer, Image, MemoryUse, Recording};
use nox_gpu::{LayerHook, LayerPicture};
use nox_project::{Effect, Item};
use std::sync::{Arc, OnceLock};

fn gpu() -> Option<Arc<nox_gpu::GpuDevice>> {
    static GPU: OnceLock<Option<Arc<nox_gpu::GpuDevice>>> = OnceLock::new();
    GPU.get_or_init(|| match nox_media::Accel::new() {
        Ok(accel) => Some(accel.gpu().clone()),
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

type Pixel = [f32; 4];

/// `pixels` (`width` × `height`) through the effect `id` with `params`.
fn run(
    id: &str,
    params: &[(&str, ParamValue)],
    pixels: &[Pixel],
    width: u32,
) -> Option<Vec<Pixel>> {
    let mut item = Item::new("clip");
    let key = item.add_effect(Effect::rendering(id, id));
    set(&mut item, &key, params);
    run_item(&mut Effects::new(gpu()?).unwrap(), &item, pixels, width)
}

/// Sets `params` of effect `key`.
fn set(item: &mut Item, key: &str, params: &[(&str, ParamValue)]) {
    for (name, value) in params {
        item.effect_mut(key)
            .unwrap()
            .set_param(name, Some(value.to_value()));
    }
}

/// `pixels` (`width` × `height`) through `item`'s effects with `effects`.
fn run_item(
    effects: &mut Effects,
    item: &Item,
    pixels: &[Pixel],
    width: u32,
) -> Option<Vec<Pixel>> {
    run_item_with(effects, item, pixels, width, None).map(|(out, _)| out)
}

/// `run_item`, also making thumbnails at most `thumbnails` large.
fn run_item_with(
    effects: &mut Effects,
    item: &Item,
    pixels: &[Pixel],
    width: u32,
    thumbnails: Option<(u32, u32)>,
) -> Option<(Vec<Pixel>, Vec<nox_fx::Thumbnail>)> {
    let gpu = gpu()?;
    let height = pixels.len() as u32 / width;
    let chain = nox_fx::resolve(item, 0.0);
    let image = Image::new(&gpu, width, height).unwrap();
    let bytes = (pixels.len() * 16) as u64;
    let mut upload = Buffer::new(&gpu, bytes, MemoryUse::Upload).unwrap();
    upload.write(pixels.as_flattened()).unwrap();
    let mut copy = Recording::new(&gpu, 0, 0).unwrap();
    copy.copy_pixels(&upload, image.handle(), width, height, false);
    copy.run().unwrap();
    let picture = LayerPicture {
        image: image.handle(),
        view: image.view(),
        width,
        height,
    };
    let mut prepared = effects.prepare(
        chain,
        Context {
            clip: 1,
            frame: None,
        },
    );
    if let Some(max) = thumbnails {
        prepared = prepared.with_thumbnails(max);
    }
    prepared.process(&picture).unwrap();
    let thumbs = prepared.take_thumbnails();
    let download = Buffer::new(&gpu, bytes, MemoryUse::Download).unwrap();
    let mut copy = Recording::new(&gpu, 0, 0).unwrap();
    copy.copy_pixels(&download, image.handle(), width, height, true);
    copy.run().unwrap();
    let floats: Vec<f32> = download.read().unwrap();
    Some((
        floats[..pixels.len() * 4].as_chunks::<4>().0.to_vec(),
        thumbs,
    ))
}

/// Deterministic pixels in [0, 1), opaque.
fn noise(n: usize, seed: u64) -> Vec<Pixel> {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 40) as f32 / (1u64 << 24) as f32
    };
    (0..n).map(|_| [next(), next(), next(), 1.0]).collect()
}

/// The blur kernel's Gaussian of `radius` pixels, on the CPU: the same
/// taps, premultiplied, edges clamped.
fn blur_reference(pixels: &[Pixel], width: usize, radius: f64) -> Vec<Pixel> {
    let height = pixels.len() / width;
    let sigma = (radius / 2.0).max(0.3);
    let step = (sigma / 3.0).floor().max(1.0) as i64;
    let taps = ((3.0 * sigma) / step as f64).ceil() as i64;
    let pass = |src: &[Pixel], dx: i64, dy: i64, pre: bool, unpre: bool| -> Vec<Pixel> {
        let mut out = vec![[0.0; 4]; src.len()];
        for y in 0..height as i64 {
            for x in 0..width as i64 {
                let (mut sum, mut weights) = ([0.0f64; 4], 0.0);
                for i in -taps..=taps {
                    let d = i * step;
                    let w = (-((d * d) as f64) / (2.0 * sigma * sigma)).exp();
                    let sx = (x + dx * d).clamp(0, width as i64 - 1) as usize;
                    let sy = (y + dy * d).clamp(0, height as i64 - 1) as usize;
                    let mut c = src[sy * width + sx].map(f64::from);
                    if pre {
                        (0..3).for_each(|k| c[k] *= c[3]);
                    }
                    (0..4).for_each(|k| sum[k] += c[k] * w);
                    weights += w;
                }
                let mut c = sum.map(|v| v / weights);
                if unpre {
                    let a = c[3];
                    (0..3).for_each(|k| c[k] = if a > 1e-6 { c[k] / a } else { 0.0 });
                }
                out[y as usize * width + x as usize] = c.map(|v| v as f32);
            }
        }
        out
    };
    let horizontal = pass(pixels, 1, 0, true, false);
    pass(&horizontal, 0, 1, false, true)
}

fn max_difference(a: &[Pixel], b: &[Pixel]) -> f32 {
    a.iter()
        .zip(b)
        .flat_map(|(x, y)| (0..4).map(move |k| (x[k] - y[k]).abs()))
        .fold(0.0, f32::max)
}

#[test]
fn gaussian_blur_matches_the_reference() {
    let (width, height) = (37, 29);
    let pixels = noise(width * height, 1);
    // 20 % of 29 rows: a radius of 5.8 pixels.
    let params = [("radius", ParamValue::Number(20.0 * 0.5))];
    let Some(out) = run(registry::GAUSSIAN_BLUR, &params, &pixels, width as u32) else {
        return;
    };
    let reference = blur_reference(&pixels, width, 10.0 / 100.0 * height as f64);
    let diff = max_difference(&out, &reference);
    assert!(diff < 1e-4, "{diff}");
    assert!(max_difference(&out, &pixels) > 0.1, "it blurs");
}

#[test]
fn large_blurs_skip_pixels_like_the_reference() {
    // The largest radius on a tall picture: 30 pixels, σ 15, so every
    // fifth pixel.
    let (width, height) = (8, 300);
    let pixels = &noise(width * height, 3)[..];
    let params = [("radius", ParamValue::Number(10.0))];
    let Some(out) = run(registry::GAUSSIAN_BLUR, &params, pixels, width as u32) else {
        return;
    };
    let reference = blur_reference(pixels, width, 0.1 * height as f64);
    let diff = max_difference(&out, &reference);
    assert!(diff < 1e-4, "{diff}");
}

#[test]
fn transparent_pixels_do_not_bleed() {
    let (width, height) = (20, 10);
    // Opaque blue on the left, transparent red on the right.
    let pixels: Vec<Pixel> = (0..width * height)
        .map(|i| {
            if i % width < width / 2 {
                [0.0, 0.0, 1.0, 1.0]
            } else {
                [1.0, 0.0, 0.0, 0.0]
            }
        })
        .collect();
    let params = [("radius", ParamValue::Number(30.0))];
    let Some(out) = run(registry::GAUSSIAN_BLUR, &params, &pixels, width as u32) else {
        return;
    };
    for p in &out {
        if p[3] > 1e-3 {
            assert!(p[0] < 1e-3 && (p[2] - 1.0).abs() < 1e-3, "{p:?}");
        }
    }
    // Alpha itself spreads.
    assert!((0.1..0.9).contains(&out[width / 2][3]));
}

#[test]
fn sharpen_matches_the_reference() {
    let (width, height) = (31, 23);
    let pixels = noise(width * height, 4);
    let params = [
        ("amount", ParamValue::Number(150.0)),
        ("radius", ParamValue::Number(1.0)),
        ("threshold", ParamValue::Number(0.0)),
    ];
    let Some(out) = run(registry::SHARPEN, &params, &pixels, width as u32) else {
        return;
    };
    let radius = (0.01 * height as f64).max(0.5);
    let blurred = blur_reference(&pixels, width, radius);
    let reference: Vec<Pixel> = pixels
        .iter()
        .zip(&blurred)
        .map(|(c, b)| {
            let detail = [0, 1, 2].map(|k| c[k] - b[k]);
            let size = detail.iter().fold(0.0f32, |m, d| m.max(d.abs()));
            // smoothstep(0, 1e-6, size): 1 for any detail.
            let gate = if size >= 1e-6 { 1.0 } else { 0.0 };
            [
                (c[0] + 1.5 * gate * detail[0]).max(0.0),
                (c[1] + 1.5 * gate * detail[1]).max(0.0),
                (c[2] + 1.5 * gate * detail[2]).max(0.0),
                c[3],
            ]
        })
        .collect();
    let diff = max_difference(&out, &reference);
    assert!(diff < 1e-3, "{diff}");
}

#[test]
fn chroma_key_removes_the_key_and_keeps_the_rest() {
    // The key picked in sRGB, as the working space has it.
    let m = nox_color::Primaries::Bt709.conversion_to(nox_color::WORKING_PRIMARIES);
    let linear = [0.0f32, 0.69, 0.25].map(|c| nox_color::Transfer::Srgb.eotf(c));
    let green = [0, 1, 2].map(|r| (0..3).map(|c| m[r][c] * linear[c]).sum::<f32>());
    let colors: [Pixel; 5] = [
        [green[0], green[1], green[2], 1.0],
        // The same green, brighter.
        [green[0] * 2.0, green[1] * 2.0, green[2] * 2.0, 1.0],
        [0.5, 0.5, 0.5, 1.0],
        [0.8, 0.3, 0.2, 1.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let params = [
        ("color", ParamValue::Color([0.0, 0.69, 0.25])),
        ("tolerance", ParamValue::Number(30.0)),
        ("softness", ParamValue::Number(10.0)),
        ("spill", ParamValue::Number(100.0)),
    ];
    // The key color is sRGB: compare against the working space's green.
    let Some(out) = run(registry::CHROMA_KEY, &params, &colors, 5) else {
        return;
    };
    assert!(out[0][3] < 0.05, "key: {:?}", out[0]);
    assert!(out[1][3] < 0.05, "brighter key: {:?}", out[1]);
    for p in &out[2..] {
        assert!(p[3] > 0.99, "{p:?}");
    }
    // Spill: green held down to the larger of red and blue.
    assert!(out[0][1] <= out[0][0].max(out[0][2]) + 1e-6, "{:?}", out[0]);
    assert_eq!(out[2][..3], [0.5, 0.5, 0.5]);
    let params = [
        ("color", ParamValue::Color([0.0, 0.69, 0.25])),
        ("tolerance", ParamValue::Number(30.0)),
        ("show_matte", ParamValue::Bool(true)),
    ];
    let out = run(registry::CHROMA_KEY, &params, &colors, 5).unwrap();
    assert!(out[0][0] < 0.05 && out[2][0] > 0.99 && out[0][3] == 1.0);
}

#[test]
fn painterly_keeps_flat_areas_and_edges() {
    // The largest radius, 3 % of 200 rows: 6 pixels.
    let (width, height) = (40, 200);
    // Dark left, bright right, a little noise on both.
    let noise = noise(width * height, 5);
    let pixels: Vec<Pixel> = (0..width * height)
        .map(|i| {
            let base = if i % width < width / 2 { 0.1 } else { 0.8 };
            let n = noise[i][0] * 0.05;
            [base + n, base + n, base + n, 1.0]
        })
        .collect();
    let params = [("radius", ParamValue::Number(3.0))];
    let Some(out) = run(registry::PAINTERLY, &params, &pixels, width as u32) else {
        return;
    };
    let row = height / 2 * width;
    // Next to the edge, each side keeps its own level (no blur across).
    assert!(
        (out[row + width / 2 - 1][0] - 0.125).abs() < 0.04,
        "{:?}",
        out[row + width / 2 - 1]
    );
    assert!(
        (out[row + width / 2][0] - 0.825).abs() < 0.04,
        "{:?}",
        out[row + width / 2]
    );
    // Noise is smoothed.
    let spread = |p: &[Pixel]| {
        let left: Vec<f32> = (0..height)
            .flat_map(|y| (8..width / 2 - 8).map(move |x| (y, x)))
            .map(|(y, x)| p[y * width + x][0])
            .collect();
        left.iter().cloned().fold(f32::MIN, f32::max)
            - left.iter().cloned().fold(f32::MAX, f32::min)
    };
    assert!(
        spread(&out) < spread(&pixels) * 0.6,
        "{} vs {}",
        spread(&out),
        spread(&pixels)
    );
    assert!(out.iter().all(|p| p[3] == 1.0));
}

#[test]
fn no_op_values_leave_the_picture() {
    let pixels = noise(12 * 8, 6);
    let Some(out) = run(
        registry::GAUSSIAN_BLUR,
        &[("radius", ParamValue::Number(0.0))],
        &pixels,
        12,
    ) else {
        return;
    };
    assert_eq!(out, pixels);
}

/// Green on the left (keyed out), gray noise on the right: a key's matte
/// is 0 on the left and 1 on the right.
fn green_and_noise(width: usize, height: usize) -> Vec<Pixel> {
    let m = nox_color::Primaries::Bt709.conversion_to(nox_color::WORKING_PRIMARIES);
    let linear = [0.0f32, 0.69, 0.25].map(|c| nox_color::Transfer::Srgb.eotf(c));
    let green = [0, 1, 2].map(|r| (0..3).map(|c| m[r][c] * linear[c]).sum::<f32>());
    let noise = noise(width * height, 9);
    (0..width * height)
        .map(|i| {
            if i % width < width / 2 {
                [green[0], green[1], green[2], 1.0]
            } else {
                let v = 0.2 + noise[i][0] * 0.6;
                [v, v, v, 1.0]
            }
        })
        .collect()
}

/// A key of the clip's picture whose matte (0 left, 1 right) feeds
/// effect `target`'s `input`. (Added last, the key would take `target`'s
/// picture, and its matte back into `target` would be a cycle.)
fn keyed(item: &mut Item, target: &str, input: &str) -> String {
    let key = item.add_effect(Effect::rendering(registry::CHROMA_KEY, "Key"));
    nox_fx::materialize(item);
    item.connect(nox_project::SOURCE, "picture", &key, "picture")
        .unwrap();
    item.connect(&key, "matte", target, input).unwrap();
    key
}

#[test]
fn a_mask_limits_an_effect() {
    let (width, height) = (40, 30);
    let pixels = green_and_noise(width, height);
    let mut item = Item::new("clip");
    let blur = item.add_effect(Effect::rendering(registry::GAUSSIAN_BLUR, "Blur"));
    set(&mut item, &blur, &[("radius", ParamValue::Number(10.0))]);
    keyed(&mut item, &blur, "mask");
    // The blur takes the clip's picture, not the key's.
    item.connect(nox_project::SOURCE, "picture", &blur, "picture")
        .unwrap();
    item.set_effects_output(Some(nox_project::Connection::new(&blur, "picture")));
    let Some(gpu) = gpu() else { return };
    let mut effects = Effects::new(gpu).unwrap();
    let out = run_item(&mut effects, &item, &pixels, width as u32).unwrap();
    let blurred = blur_reference(&pixels, width, 0.1 * height as f64);
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if x < width / 2 - 6 {
                // Unmasked: the clip's own picture (not keyed, not blurred).
                assert_eq!(out[i], pixels[i], "({x}, {y})");
            } else if x >= width / 2 + 6 {
                let diff = max_difference(&out[i..=i], &blurred[i..=i]);
                assert!(diff < 1e-3, "({x}, {y}): {:?} vs {:?}", out[i], blurred[i]);
            }
        }
    }
}

#[test]
fn mattes_into_alpha() {
    let (width, height) = (32, 8);
    let pixels = green_and_noise(width, height);
    let alpha = |out: &[Pixel], x: usize| out[4 * width + x][3];
    for (mode, invert, left, right) in [
        ("multiply", false, 0.0, 1.0),
        ("replace", false, 0.0, 1.0),
        ("multiply", true, 1.0, 0.0),
    ] {
        let mut item = Item::new("clip");
        let apply = item.add_effect(Effect::rendering(registry::APPLY_MATTE, "Apply"));
        set(
            &mut item,
            &apply,
            &[
                ("mode", ParamValue::Choice(mode)),
                ("invert", ParamValue::Bool(invert)),
            ],
        );
        keyed(&mut item, &apply, "matte");
        item.connect(nox_project::SOURCE, "picture", &apply, "picture")
            .unwrap();
        // (Materializing fixed the output on the key, the last picture
        // then.)
        item.set_effects_output(Some(nox_project::Connection::new(&apply, "picture")));
        let Some(gpu) = gpu() else { return };
        let out = run_item(
            &mut Effects::new(gpu).unwrap(),
            &item,
            &pixels,
            width as u32,
        )
        .unwrap();
        assert!(
            (alpha(&out, 2) - left).abs() < 1e-3,
            "{mode} {invert}: {}",
            alpha(&out, 2)
        );
        assert!(
            (alpha(&out, 29) - right).abs() < 1e-3,
            "{mode} {invert}: {}",
            alpha(&out, 29)
        );
        // Colors untouched (the key's spill suppression isn't applied:
        // the picture comes from the clip).
        assert_eq!(out[4 * width + 2][..3], pixels[4 * width + 2][..3]);
    }
}

#[test]
fn inverted_and_feathered_mattes() {
    let (width, height) = (64, 16);
    let pixels = green_and_noise(width, height);
    let mut item = Item::new("clip");
    let apply = item.add_effect(Effect::rendering(registry::APPLY_MATTE, "Apply"));
    set(
        &mut item,
        &apply,
        &[("mode", ParamValue::Choice("replace"))],
    );
    let invert = item.add_effect(Effect::rendering(registry::INVERT_MATTE, "Invert"));
    let feather = item.add_effect(Effect::rendering(registry::FEATHER_MATTE, "Feather"));
    set(&mut item, &feather, &[("radius", ParamValue::Number(10.0))]);
    keyed(&mut item, &invert, "matte");
    item.connect(&invert, "matte", &feather, "matte").unwrap();
    item.connect(&feather, "matte", &apply, "matte").unwrap();
    item.connect(nox_project::SOURCE, "picture", &apply, "picture")
        .unwrap();
    item.set_effects_output(Some(nox_project::Connection::new(&apply, "picture")));
    let Some(gpu) = gpu() else { return };
    let out = run_item(
        &mut Effects::new(gpu).unwrap(),
        &item,
        &pixels,
        width as u32,
    )
    .unwrap();
    let alpha = |x: usize| out[8 * width + x][3];
    assert!(
        alpha(0) > 0.99 && alpha(63) < 0.01,
        "inverted: {} {}",
        alpha(0),
        alpha(63)
    );
    // Soft around the edge (σ 0.8 px of a 16-row matte... 10 % of it).
    let edge = alpha(width / 2 - 1);
    assert!(edge > 0.1 && edge < 0.9, "feathered: {edge}");
    for x in 1..width {
        assert!(alpha(x) <= alpha(x - 1) + 1e-5, "falls off: {x}");
    }
}

#[test]
fn the_output_can_be_an_earlier_picture() {
    let (width, height) = (24, 18);
    let pixels = noise(width * height, 11);
    let mut item = Item::new("clip");
    let blur = item.add_effect(Effect::rendering(registry::GAUSSIAN_BLUR, "Blur"));
    set(&mut item, &blur, &[("radius", ParamValue::Number(10.0))]);
    item.add_effect(Effect::rendering(registry::SHARPEN, "Sharpen"));
    item.set_effects_output(Some(nox_project::Connection::new(&blur, "picture")));
    let Some(gpu) = gpu() else { return };
    let out = run_item(
        &mut Effects::new(gpu).unwrap(),
        &item,
        &pixels,
        width as u32,
    )
    .unwrap();
    let reference = blur_reference(&pixels, width, 0.1 * height as f64);
    assert!(max_difference(&out, &reference) < 1e-4);
}

/// Thumbnails show the source, each node's output (a matte for a
/// matte-only node) and the output, in sRGB, keeping the aspect.
#[test]
fn thumbnails_of_every_node() {
    let (width, height) = (40, 30);
    let pixels = green_and_noise(width, height);
    let mut item = Item::new("clip");
    let blur = item.add_effect(Effect::rendering(registry::GAUSSIAN_BLUR, "Blur"));
    let key = keyed(&mut item, &blur, "mask");
    let invert = item.add_effect(Effect::rendering(registry::INVERT_MATTE, "Invert"));
    item.connect(&key, "matte", &invert, "matte").unwrap();
    item.connect(&invert, "matte", &blur, "mask").unwrap();
    item.connect(nox_project::SOURCE, "picture", &blur, "picture")
        .unwrap();
    item.set_effects_output(Some(nox_project::Connection::new(&blur, "picture")));
    let Some(gpu) = gpu() else { return };
    let mut effects = Effects::new(gpu).unwrap();
    let (_, thumbs) =
        run_item_with(&mut effects, &item, &pixels, width as u32, Some((160, 90))).unwrap();
    let nodes: Vec<&str> = thumbs.iter().map(|t| t.node.as_str()).collect();
    assert_eq!(
        nodes,
        [
            nox_project::SOURCE,
            key.as_str(),
            invert.as_str(),
            blur.as_str(),
            nox_fx::OUTPUT
        ]
    );
    for thumb in &thumbs {
        // 40×30 into 160×90: 120×90.
        assert_eq!((thumb.width, thumb.height), (120, 90));
        assert_eq!(thumb.rgba.len(), 120 * 90 * 4);
    }
    let pixel = |node: usize, x: usize, y: usize| {
        let i = (y * 120 + x) * 4;
        <[u8; 4]>::try_from(&thumbs[node].rgba[i..i + 4]).unwrap()
    };
    // The source's green, back in sRGB (0, 0.69, 0.25).
    let green = pixel(0, 10, 45);
    assert!(
        green[0] <= 2 && green[1].abs_diff(176) <= 2 && green[2].abs_diff(64) <= 2,
        "{green:?}"
    );
    assert_eq!(green[3], 255);
    // The key is 0 on the green half; inverted, 1.
    assert_eq!(pixel(2, 10, 45), [255, 255, 255, 255]);
    assert_eq!(pixel(2, 110, 45), [0, 0, 0, 255]);
}

/// Background removal's model runs once for a picture: changing what
/// comes after it takes the matte from the cache. Needs the models
/// (skipped otherwise).
#[test]
fn ai_mattes_are_cached() {
    let dir = nox_ai::models::default_dir();
    if !nox_ai::models::registry()
        .models()
        .iter()
        .any(|m| m.is_available(&dir))
    {
        eprintln!("skipping: no background removal model");
        return;
    }
    let (width, height) = (64, 48);
    let pixels = noise(width * height, 12);
    let mut item = Item::new("clip");
    let segment = item.add_effect(Effect::rendering(registry::SEGMENT_SUBJECT, "Segment"));
    let apply = item.add_effect(Effect::rendering(registry::APPLY_MATTE, "Apply"));
    nox_fx::materialize(&mut item);
    item.connect(&segment, "matte", &apply, "matte").unwrap();
    item.connect(nox_project::SOURCE, "picture", &segment, "picture")
        .unwrap();
    let Some(gpu) = gpu() else { return };
    let mut effects = Effects::new(gpu).unwrap();
    let first = run_item(&mut effects, &item, &pixels, width as u32).unwrap();
    assert_eq!(effects.model_runs(), 1);
    set(&mut item, &apply, &[("invert", ParamValue::Bool(true))]);
    let second = run_item(&mut effects, &item, &pixels, width as u32).unwrap();
    assert_eq!(effects.model_runs(), 1, "the matte came from the cache");
    // Inverted: the alphas add up to one.
    for (a, b) in first.iter().zip(&second) {
        assert!((a[3] + b[3] - 1.0).abs() < 1e-3, "{} {}", a[3], b[3]);
    }
}
