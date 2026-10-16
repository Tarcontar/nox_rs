//! Times background removal models on the GPU as Nox runs them, and
//! writes their mattes (raw little-endian f32) for accuracy comparisons.
//!
//!     bench birefnet MODEL INPUT OUT [RUNS]
//!         INPUT: [1, 3, 1024, 1024], ImageNet-normalized. Runs once to warm
//!         up, then RUNS times (default 5); OUT gets the logits.
//!     bench rvm MODEL FRAMES WIDTH HEIGHT OUT
//!         FRAMES: [n, 3, HEIGHT, WIDTH] in [0, 1], run in order with the
//!         recurrent state carried over (downsample ratio 512 / long side);
//!         OUT gets every frame's matte.
//!
//! The precision follows the model (an fp16 model runs in 16 bits);
//! `NOX_AI_TENSOR` picks the fp16 matrix multiplications: `0` (no tensor
//! cores), `1` (subgroup cooperative matrices), `2` (cooperative matrices 2).
//! `NOX_GPU_LABELS=1` labels dispatches for Nsight.

use nox_ai::{Model, Session, Tensor};
use std::time::Instant;

fn read_f32(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("input file");
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn write_f32(path: &str, data: &[f32]) {
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(path, bytes).expect("output file");
}

/// Milliseconds: mean, median, min.
fn summary(times: &[f64]) -> String {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    format!(
        "mean {mean:.2} ms, median {:.2} ms, min {:.2} ms over {}",
        sorted[sorted.len() / 2],
        sorted[0],
        times.len()
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let accel = nox_media::Accel::new().expect("GPU");
    let gpu = accel.gpu().clone();
    let model = Model::load(args[1].as_ref()).expect("model");
    let mut session = Session::new(gpu, model);
    let describe = |session: &Session, planned: f64| {
        println!(
            "precision {:?}, matrices {:?}, planned in {planned:.0} ms, {:.0} MB",
            session.precision(),
            session.matrix_kernels(),
            session.memory() as f64 / 1e6
        );
    };
    match args[0].as_str() {
        "birefnet" => {
            let input = read_f32(&args[2]);
            let runs: usize = args.get(4).map_or(5, |r| r.parse().expect("runs"));
            let started = Instant::now();
            session
                .plan(&[("input_image", &[1, 3, 1024, 1024])], &[], &[])
                .expect("plan");
            describe(&session, started.elapsed().as_secs_f64() * 1e3);
            session.write("input_image", &input).expect("write");
            session.run().expect("run");
            let mut times = Vec::new();
            for _ in 0..runs {
                let started = Instant::now();
                session.run().expect("run");
                times.push(started.elapsed().as_secs_f64() * 1e3);
            }
            println!("run: {}", summary(&times));
            write_f32(&args[3], &session.read("output_image").expect("read"));
        }
        "rvm" => {
            let frames = read_f32(&args[2]);
            let (w, h): (usize, usize) = (
                args[3].parse().expect("width"),
                args[4].parse().expect("height"),
            );
            let size = 3 * w * h;
            let ratio = 512.0 / w.max(h) as f32;
            let constants = [("downsample_ratio", Tensor::f32(vec![1], vec![ratio]))];
            let state = [
                ("r1o", "r1i"),
                ("r2o", "r2i"),
                ("r3o", "r3i"),
                ("r4o", "r4i"),
            ];
            let src = [1, 3, h, w];
            let started = Instant::now();
            // As Matting plans: once with empty state for its shapes, then
            // with the state fed back.
            let empty = [1usize, 1, 1, 1];
            let mut shapes: Vec<(&str, &[usize])> = vec![("src", &src)];
            shapes.extend(state.iter().map(|&(_, input)| (input, &empty[..])));
            session.plan(&shapes, &constants, &[]).expect("plan");
            let state_shapes: Vec<(&str, Vec<usize>)> = state
                .iter()
                .map(|&(output, input)| (input, session.shape(output).expect("state")))
                .collect();
            let mut shapes: Vec<(&str, &[usize])> = vec![("src", &src)];
            shapes.extend(state_shapes.iter().map(|(n, s)| (*n, s.as_slice())));
            session.plan(&shapes, &constants, &state).expect("plan");
            describe(&session, started.elapsed().as_secs_f64() * 1e3);
            for (input, shape) in &state_shapes {
                session
                    .write(input, &vec![0.0; shape.iter().product()])
                    .expect("state");
            }
            let mut mattes = Vec::new();
            let mut times = Vec::new();
            for frame in frames.chunks_exact(size) {
                session.write("src", frame).expect("write");
                let started = Instant::now();
                session.run().expect("run");
                times.push(started.elapsed().as_secs_f64() * 1e3);
                mattes.extend(session.read("pha").expect("read"));
            }
            // The first frame warms up.
            println!("frame: {}", summary(&times[1..]));
            write_f32(&args[5], &mattes);
        }
        other => panic!("unknown model kind {other}"),
    }
}
