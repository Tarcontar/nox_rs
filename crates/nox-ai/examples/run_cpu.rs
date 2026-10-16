//! Runs a model on the CPU with inputs from raw little-endian f32 files and
//! writes its outputs the same way (for comparing against other runtimes).
//!
//!     run_cpu [--gpu] model.onnx OUT_DIR name=shape:file ...   (shape like 1x3x64x64)
//!
//! With `--gpu`, runs it on the GPU (`Session`) instead, inputs of shape
//! [1] held fixed.

use nox_ai::{Model, Tensor};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let gpu = args.first().is_some_and(|a| a == "--gpu");
    if gpu {
        args.remove(0);
    }
    let model = Model::load(args[0].as_ref()).expect("model");
    let out_dir = std::path::PathBuf::from(&args[1]);
    let inputs: Vec<(String, Tensor)> = args[2..]
        .iter()
        .map(|a| {
            let (name, rest) = a.split_once('=').expect("name=shape:file");
            let (shape, file) = rest.split_once(':').expect("shape:file");
            let shape: Vec<usize> = shape.split('x').map(|d| d.parse().expect("dim")).collect();
            let bytes = std::fs::read(file).expect("input file");
            let data = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            (name.to_owned(), Tensor::f32(shape, data))
        })
        .collect();
    let refs: Vec<(&str, Tensor)> = inputs
        .iter()
        .map(|(n, t)| (n.as_str(), t.clone()))
        .collect();
    let outputs = if gpu {
        run_gpu(model, &refs)
    } else {
        let started = std::time::Instant::now();
        let outputs = model.run_cpu(&refs).expect("run");
        eprintln!("ran in {:?}", started.elapsed());
        outputs
    };
    for (name, tensor) in outputs {
        let bytes: Vec<u8> = tensor
            .to_f32()
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        std::fs::write(out_dir.join(format!("{name}.f32")), bytes).expect("write");
        println!("{name} {:?}", tensor.shape);
    }
}

fn run_gpu(model: Model, inputs: &[(&str, Tensor)]) -> std::collections::HashMap<String, Tensor> {
    let accel = nox_media::Accel::new().expect("GPU");
    let names = model.outputs.clone();
    let mut session = nox_ai::Session::new(accel.gpu().clone(), model);
    let (fixed, fed): (Vec<_>, Vec<_>) = inputs.iter().partition(|(_, t)| t.shape == [1]);
    let shapes: Vec<(&str, &[usize])> = fed.iter().map(|(n, t)| (*n, t.shape.as_slice())).collect();
    let constants: Vec<(&str, Tensor)> = fixed.iter().map(|(n, t)| (*n, t.clone())).collect();
    let started = std::time::Instant::now();
    session.plan(&shapes, &constants, &[]).expect("plan");
    eprintln!(
        "planned in {:?}, {:.0} MB, {:?}, matrices {:?}",
        started.elapsed(),
        session.memory() as f64 / 1e6,
        session.precision(),
        session.matrix_kernels()
    );
    for (name, tensor) in &fed {
        session.write(name, &tensor.to_f32()).expect("write");
    }
    for _ in 0..3 {
        let started = std::time::Instant::now();
        session.run().expect("run");
        eprintln!("ran in {:?}", started.elapsed());
    }
    names
        .iter()
        .map(|name| {
            let shape = session.shape(name).expect("shape");
            (
                name.clone(),
                Tensor::f32(shape, session.read(name).expect("read")),
            )
        })
        .collect()
}
