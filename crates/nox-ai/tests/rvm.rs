//! Robust Video Matting on the GPU against the CPU reference, over two
//! frames so the recurrent state fed back between runs is checked too.
//! Needs the model: `NOX_AI_MODELS` names the folder it's in (else the
//! test skips). `downloads_and_verifies` (ignored: network) fetches it.

use nox_ai::models::ROBUST_VIDEO_MATTING;
use nox_ai::{Model, Session, Tensor};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn model_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("NOX_AI_MODELS")?);
    let path = ROBUST_VIDEO_MATTING.path(&dir);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: no {}", path.display());
        None
    }
}

fn random(n: usize, seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32
        })
        .collect()
}

#[test]
fn gpu_matches_cpu_over_frames() {
    let Some(path) = model_path() else { return };
    let Ok(accel) = nox_media::Accel::new() else {
        eprintln!("skipping: no GPU");
        return;
    };
    let model = Model::load(&path).unwrap();
    let (h, w) = (96, 160);
    let ratio = Tensor::f32(vec![1], vec![0.5]);
    let state = ["r1i", "r2i", "r3i", "r4i"];
    // The state's shapes, from a CPU run with empty state.
    let empty = Tensor::f32(vec![1, 1, 1, 1], vec![0.0]);
    let frames: Vec<Tensor> = (0..2)
        .map(|f| Tensor::f32(vec![1, 3, h, w], random(3 * h * w, f + 1)))
        .collect();
    let mut feeds: Vec<(&str, Tensor)> = vec![
        ("src", frames[0].clone()),
        ("downsample_ratio", ratio.clone()),
    ];
    feeds.extend(state.iter().map(|&n| (n, empty.clone())));
    let first = model.run_cpu(&feeds).unwrap();
    let shapes: Vec<Vec<usize>> = (1..=4)
        .map(|i| first[&format!("r{i}o")].shape.clone())
        .collect();

    // The CPU with zero state of those shapes, then frame 2 with frame 1's.
    let zeros = |s: &Vec<usize>| Tensor::f32(s.clone(), vec![0.0; s.iter().product()]);
    let mut expected = Vec::new();
    let mut carried: Vec<Tensor> = shapes.iter().map(zeros).collect();
    for frame in &frames {
        let mut feeds: Vec<(&str, Tensor)> =
            vec![("src", frame.clone()), ("downsample_ratio", ratio.clone())];
        feeds.extend(state.iter().zip(&carried).map(|(&n, t)| (n, t.clone())));
        let out = model.run_cpu(&feeds).unwrap();
        carried = (1..=4).map(|i| out[&format!("r{i}o")].clone()).collect();
        expected.push(out["pha"].to_f32());
    }

    let mut session = Session::new(accel.gpu().clone(), model);
    let src_shape = [1, 3, h, w];
    let mut plan: Vec<(&str, &[usize])> = vec![("src", &src_shape)];
    plan.extend(state.iter().zip(&shapes).map(|(&n, s)| (n, s.as_slice())));
    let feedback = [
        ("r1o", "r1i"),
        ("r2o", "r2i"),
        ("r3o", "r3i"),
        ("r4o", "r4i"),
    ];
    session
        .plan(&plan, &[("downsample_ratio", ratio)], &feedback)
        .unwrap();
    for (n, s) in state.iter().zip(&shapes) {
        session.write(n, &vec![0.0; s.iter().product()]).unwrap();
    }
    for (frame, want) in frames.iter().zip(&expected) {
        session.write("src", &frame.to_f32()).unwrap();
        session.run().unwrap();
        let got = session.read("pha").unwrap();
        let worst = got
            .iter()
            .zip(want)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-4, "matte off by {worst}");
    }
}

#[test]
#[ignore = "downloads 15 MB"]
fn downloads_and_verifies() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models");
    let _ = std::fs::remove_file(ROBUST_VIDEO_MATTING.path(&dir));
    let mut last = 0;
    let path = ROBUST_VIDEO_MATTING
        .download(&dir, |done, _| last = done, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(last, ROBUST_VIDEO_MATTING.size);
    assert!(ROBUST_VIDEO_MATTING.is_available(&dir));
    Model::load(&path).unwrap();
}
