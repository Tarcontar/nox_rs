//! Measures how fast a file plays through Nox's frame path:
//! `cargo run --release -p nox-engine --example bench -- clip.mp4 [frames]`.
//!
//! Times sequential decoding alone, then decoding plus compositing at a
//! viewer-sized output (960×540, read back), and reports whether frames
//! come out of the hardware decoder.

use nox_gpu::{Compositor, LayerStyle, Rect};
use nox_media::{HwDevice, VideoDecoder, ffmpeg};
use std::time::{Duration, Instant};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("NOX_LOG"))
        .init();
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: bench <file> [frames]");
    let frames: i64 = args.next().map_or(90, |n| n.parse().expect("frame count"));
    let (hw, gpu) = HwDevice::vulkan().expect("Vulkan device");

    let mut decoder = VideoDecoder::open(path.as_ref(), Some(&hw)).expect("open");
    let fps = decoder.frame_rate().as_f64();
    let first = decoder.frame_at(0).expect("frame 0");
    let hardware = first.format() == ffmpeg::format::Pixel::VULKAN;
    println!(
        "{path}: {}×{} at {fps:.3} fps, {} decoding",
        first.width(),
        first.height(),
        if hardware { "Vulkan Video" } else { "software" }
    );

    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for index in 1..frames {
        let t = Instant::now();
        decoder.frame_at(index).expect("frame");
        worst = worst.max(t.elapsed());
    }
    report("decode", start.elapsed(), frames - 1, worst, fps);

    // Scrubbing: jumps to spread-out frames (a seek and decoding from the
    // previous keyframe each).
    let mut worst = Duration::ZERO;
    let jumps: Vec<i64> = (0..10).map(|i| (i * 37 + 11) % frames.max(1)).collect();
    let start = Instant::now();
    for &index in &jumps {
        let t = Instant::now();
        decoder.frame_at(index).expect("frame");
        worst = worst.max(t.elapsed());
    }
    println!(
        "  random access: {:.0} ms per jump on average, worst {:.0} ms",
        start.elapsed().as_secs_f64() * 1000.0 / jumps.len() as f64,
        worst.as_secs_f64() * 1000.0
    );

    let mut compositor = Compositor::new(gpu).expect("compositor");
    let mut decoder = VideoDecoder::open(path.as_ref(), Some(&hw)).expect("open");
    let (mut decode, mut composite) = (Duration::ZERO, Duration::ZERO);
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for index in 0..frames {
        let t = Instant::now();
        let frame = decoder.frame_at(index).expect("frame");
        let decoded = t.elapsed();
        compositor.begin(960, 540).unwrap();
        let (w, h) = nox_media::display_size(&frame);
        let rect = Rect::fit(f64::from(w) / f64::from(h), 960, 540);
        nox_media::with_gpu_input(&frame, |input| {
            compositor.add_layer(input, rect, LayerStyle::default())
        })
        .unwrap()
        .unwrap();
        compositor.finish().unwrap();
        decode += decoded;
        composite += t.elapsed() - decoded;
        worst = worst.max(t.elapsed());
    }
    report("decode + composite", start.elapsed(), frames, worst, fps);
    println!(
        "  of which decode {:.1} ms/frame, convert + composite + readback {:.1} ms/frame",
        decode.as_secs_f64() * 1000.0 / frames as f64,
        composite.as_secs_f64() * 1000.0 / frames as f64
    );
}

fn report(what: &str, total: Duration, frames: i64, worst: Duration, fps: f64) {
    let per_frame = total.as_secs_f64() / frames as f64;
    println!(
        "  {what}: {:.1} ms/frame ({:.0} fps, {:.1}× real time), worst {:.1} ms",
        per_frame * 1000.0,
        1.0 / per_frame,
        1.0 / per_frame / fps,
        worst.as_secs_f64() * 1000.0
    );
}
