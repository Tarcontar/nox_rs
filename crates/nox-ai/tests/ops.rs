//! Every operator the GPU runs, against the CPU reference: one-node models
//! with random inputs, in the shapes and attributes models use.

use nox_ai::onnx::*;
use nox_ai::{Model, Precision, Session, Tensor};
use prost::Message;
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

/// Deterministic values in [-1, 1).
fn random(n: usize, seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        })
        .collect()
}

enum Arg {
    /// Fed at run time, of this shape.
    Input(Vec<usize>),
    /// A float constant (weights).
    Weights(Vec<usize>),
    /// An integer constant (shapes, axes, indices).
    Ints(Vec<i64>),
    /// An integer constant of this shape.
    IntsShaped(Vec<usize>, Vec<i64>),
    /// A float constant with these values.
    Floats(Vec<f32>),
    /// Left out.
    None,
}

fn attr_ints(name: &str, v: &[i64]) -> AttributeProto {
    AttributeProto {
        name: name.into(),
        r#type: attribute_type::INTS,
        ints: v.to_vec(),
        ..Default::default()
    }
}

fn attr_int(name: &str, v: i64) -> AttributeProto {
    AttributeProto {
        name: name.into(),
        r#type: attribute_type::INT,
        i: v,
        ..Default::default()
    }
}

fn attr_float(name: &str, v: f32) -> AttributeProto {
    AttributeProto {
        name: name.into(),
        r#type: attribute_type::FLOAT,
        f: v,
        ..Default::default()
    }
}

fn attr_str(name: &str, v: &str) -> AttributeProto {
    AttributeProto {
        name: name.into(),
        r#type: attribute_type::STRING,
        s: v.as_bytes().to_vec(),
        ..Default::default()
    }
}

/// Runs `op` with `args` and `attrs` on the GPU and the CPU, and checks
/// every output agrees within `tolerance`.
fn check(op: &str, args: Vec<Arg>, attrs: Vec<AttributeProto>, outputs: usize, tolerance: f32) {
    let Some(gpu) = gpu() else { return };
    let mut graph = GraphProto::default();
    let mut feeds: Vec<(String, Tensor)> = Vec::new();
    let mut inputs = Vec::new();
    for (i, arg) in args.into_iter().enumerate() {
        let name = format!("in{i}");
        match arg {
            Arg::Input(shape) => {
                let n = shape.iter().product();
                feeds.push((name.clone(), Tensor::f32(shape, random(n, i as u64 + 1))));
                graph.input.push(ValueInfoProto {
                    name: name.clone(),
                    r#type: None,
                });
            }
            Arg::Weights(shape) => {
                let n: usize = shape.iter().product();
                graph.initializer.push(TensorProto {
                    name: name.clone(),
                    dims: shape.iter().map(|&d| d as i64).collect(),
                    data_type: data_type::FLOAT,
                    float_data: random(n, 100 + i as u64),
                    ..Default::default()
                });
            }
            Arg::Ints(values) => graph.initializer.push(TensorProto {
                name: name.clone(),
                dims: vec![values.len() as i64],
                data_type: data_type::INT64,
                int64_data: values,
                ..Default::default()
            }),
            Arg::IntsShaped(shape, values) => graph.initializer.push(TensorProto {
                name: name.clone(),
                dims: shape.iter().map(|&d| d as i64).collect(),
                data_type: data_type::INT64,
                int64_data: values,
                ..Default::default()
            }),
            Arg::Floats(values) => graph.initializer.push(TensorProto {
                name: name.clone(),
                dims: vec![values.len() as i64],
                data_type: data_type::FLOAT,
                float_data: values,
                ..Default::default()
            }),
            Arg::None => {
                inputs.push(String::new());
                continue;
            }
        }
        inputs.push(name);
    }
    let out_names: Vec<String> = (0..outputs).map(|i| format!("out{i}")).collect();
    graph.node.push(NodeProto {
        input: inputs,
        output: out_names.clone(),
        op_type: op.into(),
        attribute: attrs,
        ..Default::default()
    });
    graph.output = out_names
        .iter()
        .map(|n| ValueInfoProto {
            name: n.clone(),
            r#type: None,
        })
        .collect();
    let model = ModelProto {
        ir_version: 8,
        opset_import: vec![OperatorSetIdProto {
            domain: String::new(),
            version: 17,
        }],
        graph: Some(graph),
        ..Default::default()
    };
    let model = Model::from_bytes(&model.encode_to_vec()).unwrap();
    let feeds_ref: Vec<(&str, Tensor)> =
        feeds.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let expected = model.run_cpu(&feeds_ref).unwrap();

    // In both precisions, if the device keeps 16-bit floats.
    let mut precisions = vec![Precision::F32];
    if gpu.storage_16bit() {
        precisions.push(Precision::F16);
    }
    for precision in precisions {
        let mut session = Session::with_precision(gpu.clone(), model.clone(), precision);
        let shapes: Vec<(&str, &[usize])> = feeds
            .iter()
            .map(|(n, t)| (n.as_str(), t.shape.as_slice()))
            .collect();
        session.plan(&shapes, &[], &[]).unwrap();
        for (name, tensor) in &feeds {
            session.write(name, &tensor.to_f32()).unwrap();
        }
        session.run().unwrap();
        for name in &out_names {
            let want = &expected[name];
            assert_eq!(
                session.shape(name).unwrap(),
                want.shape,
                "{op} {name} shape"
            );
            let got = session.read(name).unwrap();
            let want = want.to_f32();
            // 16-bit floats keep 11 significant bits: an error relative to
            // the value, and to the values around it (sums cancel).
            let typical = want.iter().map(|v| v.abs()).sum::<f32>() / want.len().max(1) as f32;
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                let allowed = match precision {
                    Precision::F32 => tolerance,
                    Precision::F16 if b.abs() > 6e4 => continue,
                    Precision::F16 => tolerance + 1e-2 * (b.abs() + 3.0 * typical),
                };
                assert!(
                    (a - b).abs() <= allowed,
                    "{op} {name} ({precision:?}) at {i}: {a} for {b}"
                );
            }
        }
    }
}

#[test]
fn binary_broadcasting() {
    for op in ["Add", "Sub", "Mul", "Div"] {
        check(
            op,
            vec![Arg::Input(vec![2, 3, 4, 5]), Arg::Input(vec![3, 1, 1])],
            vec![],
            1,
            1e-5,
        );
        check(
            op,
            vec![Arg::Input(vec![1, 4, 1, 6]), Arg::Weights(vec![1])],
            vec![],
            1,
            1e-5,
        );
        check(
            op,
            vec![Arg::Weights(vec![5]), Arg::Input(vec![3, 5])],
            vec![],
            1,
            1e-5,
        );
    }
    // Shapes that collapse: the same shape (one dimension), a channel
    // broadcast (two), and a broadcast in the middle.
    for (a, b) in [
        (vec![2, 3, 16, 17], vec![2, 3, 16, 17]),
        (vec![1, 8, 16, 16], vec![1, 8, 1, 1]),
        (vec![4, 5, 6, 7], vec![4, 1, 1, 7]),
    ] {
        check("Mul", vec![Arg::Input(a), Arg::Input(b)], vec![], 1, 1e-6);
    }
}

#[test]
fn unary_operators() {
    for op in [
        "Relu",
        "Sigmoid",
        "Tanh",
        "Exp",
        "Neg",
        "Abs",
        "Erf",
        "HardSwish",
    ] {
        check(op, vec![Arg::Input(vec![2, 3, 7, 5])], vec![], 1, 1e-5);
    }
    check(
        "HardSigmoid",
        vec![Arg::Input(vec![4, 9])],
        vec![attr_float("alpha", 1.0 / 6.0), attr_float("beta", 0.5)],
        1,
        1e-6,
    );
    check(
        "Clip",
        vec![
            Arg::Input(vec![4, 9]),
            Arg::Floats(vec![-0.25]),
            Arg::Floats(vec![0.5]),
        ],
        vec![],
        1,
        0.0,
    );
}

#[test]
fn convolutions() {
    // 3×3, padded, strided; with bias.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 3, 17, 23]),
            Arg::Weights(vec![8, 3, 3, 3]),
            Arg::Weights(vec![8]),
        ],
        vec![
            attr_ints("pads", &[1, 1, 1, 1]),
            attr_ints("strides", &[2, 2]),
        ],
        1,
        1e-4,
    );
    // Depthwise and dilated.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 6, 15, 12]),
            Arg::Weights(vec![6, 1, 3, 3]),
        ],
        vec![
            attr_int("group", 6),
            attr_ints("pads", &[2, 2, 2, 2]),
            attr_ints("dilations", &[2, 2]),
        ],
        1,
        1e-4,
    );
    // 1×1 without bias, batch of two.
    check(
        "Conv",
        vec![
            Arg::Input(vec![2, 16, 9, 9]),
            Arg::Weights(vec![12, 16, 1, 1]),
        ],
        vec![],
        1,
        1e-4,
    );
    // Grouped.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 8, 10, 10]),
            Arg::Weights(vec![4, 4, 3, 3]),
            Arg::Weights(vec![4]),
        ],
        vec![attr_int("group", 2), attr_ints("pads", &[1, 1, 1, 1])],
        1,
        1e-4,
    );
}

/// Convolutions with many outputs per group run as matrix
/// multiplications: partial tiles, strides, dilation, groups and batches.
#[test]
fn convolutions_as_matrix_multiplication() {
    check(
        "Conv",
        vec![
            Arg::Input(vec![2, 20, 33, 47]),
            Arg::Weights(vec![70, 20, 3, 3]),
            Arg::Weights(vec![70]),
        ],
        vec![
            attr_ints("pads", &[1, 2, 1, 2]),
            attr_ints("strides", &[2, 1]),
            attr_ints("dilations", &[1, 2]),
        ],
        1,
        1e-4,
    );
    check(
        "Conv",
        vec![
            Arg::Input(vec![2, 8, 21, 19]),
            Arg::Weights(vec![64, 4, 3, 3]),
            Arg::Weights(vec![64]),
        ],
        vec![attr_int("group", 2), attr_ints("pads", &[1, 1, 1, 1])],
        1,
        1e-4,
    );
    // Patches that don't overlap (BiRefNet's deformable convolution).
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 5, 35, 70]),
            Arg::Weights(vec![24, 5, 7, 7]),
        ],
        vec![attr_ints("strides", &[7, 7])],
        1,
        1e-4,
    );
    // 1×1 with many input channels.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 300, 13, 11]),
            Arg::Weights(vec![17, 300, 1, 1]),
        ],
        vec![],
        1,
        1e-4,
    );
    // Few output channels (the tensor cores' widest tile), more than a
    // tile of pixels.
    check(
        "Conv",
        vec![
            Arg::Input(vec![2, 12, 9, 31]),
            Arg::Weights(vec![16, 12, 3, 3]),
            Arg::Weights(vec![16]),
        ],
        vec![attr_ints("pads", &[1, 1, 1, 1])],
        1,
        1e-4,
    );
    // Dilated with stride 1 and many input channels per group
    // (cooperative matrices 2 where there are), more than a tile of rows
    // and columns.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 520, 19, 37]),
            Arg::Weights(vec![130, 260, 3, 3]),
            Arg::Weights(vec![130]),
        ],
        vec![
            attr_int("group", 2),
            attr_ints("pads", &[2, 2, 2, 2]),
            attr_ints("dilations", &[2, 2]),
        ],
        1,
        1e-4,
    );
    // Many input channels, too few output channels for a matrix
    // multiplication: the direct kernel, with the weights in their order.
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 96, 13, 17]),
            Arg::Weights(vec![1, 96, 3, 3]),
            Arg::Weights(vec![1]),
        ],
        vec![attr_ints("pads", &[1, 1, 1, 1])],
        1,
        1e-4,
    );
    // 7×7 over 64 input channels (BiRefNet's deformable offsets).
    check(
        "Conv",
        vec![
            Arg::Input(vec![1, 64, 23, 41]),
            Arg::Weights(vec![98, 64, 7, 7]),
            Arg::Weights(vec![98]),
        ],
        vec![attr_ints("pads", &[3, 3, 3, 3])],
        1,
        1e-4,
    );
    // 1×1 with many output channels (cooperative matrices 2 where there
    // are), more than a tile of them and of pixels, grouped and batched.
    check(
        "Conv",
        vec![
            Arg::Input(vec![2, 80, 13, 11]),
            Arg::Weights(vec![260, 40, 1, 1]),
            Arg::Weights(vec![260]),
        ],
        vec![attr_int("group", 2)],
        1,
        1e-4,
    );
}

#[test]
fn resizing() {
    for (mode, coordinates) in [
        ("linear", "pytorch_half_pixel"),
        ("linear", "half_pixel"),
        ("linear", "align_corners"),
        ("linear", "asymmetric"),
        ("nearest", "asymmetric"),
        ("nearest", "half_pixel"),
    ] {
        for scales in [
            vec![1.0, 1.0, 2.0, 2.0],
            vec![1.0, 1.0, 0.5, 0.25],
            vec![1.0, 1.0, 1.7, 0.6],
        ] {
            eprintln!("resize {mode} {coordinates} {scales:?}");
            check(
                "Resize",
                vec![
                    Arg::Input(vec![1, 3, 12, 16]),
                    Arg::None,
                    Arg::Floats(scales.clone()),
                ],
                vec![
                    attr_str("mode", mode),
                    attr_str("coordinate_transformation_mode", coordinates),
                    attr_str("nearest_mode", "floor"),
                ],
                1,
                1e-5,
            );
        }
    }
    // To a size.
    check(
        "Resize",
        vec![
            Arg::Input(vec![1, 2, 9, 7]),
            Arg::None,
            Arg::None,
            Arg::Ints(vec![1, 2, 20, 13]),
        ],
        vec![
            attr_str("mode", "linear"),
            attr_str("coordinate_transformation_mode", "half_pixel"),
        ],
        1,
        1e-5,
    );
}

#[test]
fn pooling_and_reduction() {
    check(
        "AveragePool",
        vec![Arg::Input(vec![1, 4, 11, 9])],
        vec![
            attr_ints("kernel_shape", &[2, 2]),
            attr_ints("strides", &[2, 2]),
            attr_int("ceil_mode", 1),
        ],
        1,
        1e-5,
    );
    check(
        "MaxPool",
        vec![Arg::Input(vec![1, 4, 11, 9])],
        vec![
            attr_ints("kernel_shape", &[3, 3]),
            attr_ints("strides", &[2, 2]),
            attr_ints("pads", &[1, 1, 1, 1]),
        ],
        1,
        0.0,
    );
    check(
        "GlobalAveragePool",
        vec![Arg::Input(vec![2, 5, 7, 6])],
        vec![],
        1,
        1e-5,
    );
    check(
        "ReduceMean",
        vec![Arg::Input(vec![2, 5, 7, 6])],
        vec![attr_ints("axes", &[1]), attr_int("keepdims", 1)],
        1,
        1e-5,
    );
    check(
        "ReduceSum",
        vec![Arg::Input(vec![3, 4, 5]), Arg::Ints(vec![0, 2])],
        vec![attr_int("keepdims", 0)],
        1,
        1e-4,
    );
    // More per output than a workgroup has invocations; axes apart.
    check(
        "GlobalAveragePool",
        vec![Arg::Input(vec![2, 3, 40, 50])],
        vec![],
        1,
        1e-5,
    );
    check(
        "ReduceMean",
        vec![Arg::Input(vec![6, 5, 30, 7])],
        vec![attr_ints("axes", &[0, 2]), attr_int("keepdims", 0)],
        1,
        1e-5,
    );
}

#[test]
fn moving_data() {
    check(
        "Concat",
        vec![
            Arg::Input(vec![2, 3, 4]),
            Arg::Input(vec![2, 5, 4]),
            Arg::Weights(vec![2, 1, 4]),
        ],
        vec![attr_int("axis", 1)],
        1,
        0.0,
    );
    check(
        "Concat",
        vec![Arg::Input(vec![1, 2, 3, 4]), Arg::Input(vec![1, 2, 3, 4])],
        vec![attr_int("axis", -3)],
        1,
        0.0,
    );
    check(
        "Split",
        vec![Arg::Input(vec![2, 10, 3]), Arg::Ints(vec![3, 7])],
        vec![attr_int("axis", 1)],
        2,
        0.0,
    );
    // Two axes, the second backwards.
    check(
        "Slice",
        vec![
            Arg::Input(vec![4, 10, 6]),
            Arg::Ints(vec![1, 8]),
            Arg::Ints(vec![3, 1]),
            Arg::Ints(vec![0, 1]),
            Arg::Ints(vec![1, -2]),
        ],
        vec![],
        1,
        0.0,
    );
    check(
        "Expand",
        vec![Arg::Input(vec![1, 3, 1]), Arg::Ints(vec![2, 3, 5])],
        vec![],
        1,
        0.0,
    );
    check(
        "Transpose",
        vec![Arg::Input(vec![2, 3, 4, 5])],
        vec![attr_ints("perm", &[0, 2, 3, 1])],
        1,
        0.0,
    );
    // Long enough both ways for tiles (transpose.comp), partial ones.
    check(
        "Transpose",
        vec![Arg::Input(vec![3, 40, 50])],
        vec![attr_ints("perm", &[0, 2, 1])],
        1,
        0.0,
    );
    check(
        "Transpose",
        vec![Arg::Input(vec![2, 9, 33, 17])],
        vec![attr_ints("perm", &[0, 2, 3, 1])],
        1,
        0.0,
    );
    check(
        "Transpose",
        vec![Arg::Input(vec![2, 9, 3, 10, 4, 11])],
        vec![attr_ints("perm", &[0, 2, 4, 1, 3, 5])],
        1,
        0.0,
    );
    check(
        "Transpose",
        vec![Arg::Input(vec![1, 70, 2, 40])],
        vec![attr_ints("perm", &[0, 3, 2, 1])],
        1,
        0.0,
    );
    check(
        "Reshape",
        vec![Arg::Input(vec![2, 3, 4]), Arg::Ints(vec![0, -1])],
        vec![],
        1,
        0.0,
    );
}

#[test]
fn matrix_multiplication() {
    // Batched, broadcasting, odd sizes (partial tiles).
    check(
        "MatMul",
        vec![Arg::Input(vec![2, 3, 17, 33]), Arg::Input(vec![3, 33, 9])],
        vec![],
        1,
        1e-4,
    );
    check(
        "MatMul",
        vec![Arg::Input(vec![5, 40]), Arg::Weights(vec![40, 21])],
        vec![],
        1,
        1e-4,
    );
    check(
        "MatMul",
        vec![Arg::Input(vec![4, 7, 12]), Arg::Input(vec![12])],
        vec![],
        1,
        1e-4,
    );
    // More than a tile each way, K not a multiple of the step.
    check(
        "MatMul",
        vec![Arg::Input(vec![3, 130, 101]), Arg::Weights(vec![101, 70])],
        vec![],
        1,
        1e-4,
    );
}

#[test]
fn rows() {
    check(
        "LayerNormalization",
        vec![
            Arg::Input(vec![2, 9, 96]),
            Arg::Weights(vec![96]),
            Arg::Weights(vec![96]),
        ],
        vec![attr_int("axis", -1), attr_float("epsilon", 1e-5)],
        1,
        1e-4,
    );
    check(
        "Softmax",
        vec![Arg::Input(vec![3, 4, 49])],
        vec![attr_int("axis", -1)],
        1,
        1e-6,
    );
    // Rows longer than a workgroup, not a multiple of it.
    check(
        "LayerNormalization",
        vec![
            Arg::Input(vec![7, 200]),
            Arg::Weights(vec![200]),
            Arg::Weights(vec![200]),
        ],
        vec![attr_int("axis", -1), attr_float("epsilon", 1e-5)],
        1,
        1e-4,
    );
    check(
        "Softmax",
        vec![Arg::Input(vec![5, 130])],
        vec![attr_int("axis", -1)],
        1,
        1e-6,
    );
}

#[test]
fn gathering() {
    // Batch dims 2, pairs of coordinates, as deformable convolutions do.
    check(
        "GatherND",
        vec![
            Arg::Input(vec![2, 3, 5, 4]),
            Arg::IntsShaped(vec![2, 3, 1, 2], vec![0, 1, 4, 3, 2, 0, 1, 1, 3, 3, 0, 2]),
        ],
        vec![attr_int("batch_dims", 2)],
        1,
        0.0,
    );
    check(
        "Gather",
        vec![Arg::Input(vec![3, 4, 5]), Arg::Ints(vec![2, 0, 2])],
        vec![attr_int("axis", 1)],
        1,
        0.0,
    );
}

#[test]
fn more_elementwise() {
    check("Floor", vec![Arg::Input(vec![3, 7])], vec![], 1, 0.0);
    check(
        "Sum",
        vec![
            Arg::Input(vec![2, 3]),
            Arg::Input(vec![3]),
            Arg::Input(vec![2, 1]),
        ],
        vec![],
        1,
        1e-6,
    );
    check(
        "BatchNormalization",
        vec![
            Arg::Input(vec![1, 4, 5, 6]),
            Arg::Floats(vec![1.0, 2.0, 0.5, 1.5]),
            Arg::Floats(vec![0.1, -0.2, 0.3, 0.0]),
            Arg::Floats(vec![0.0, 0.5, -0.5, 0.2]),
            Arg::Floats(vec![1.0, 0.25, 2.0, 0.5]),
        ],
        vec![],
        1,
        1e-5,
    );
    check(
        "Pad",
        vec![
            Arg::Input(vec![1, 2, 5, 3]),
            Arg::Ints(vec![0, 0, 1, 2, 0, 1, 3, 0]),
        ],
        vec![],
        1,
        0.0,
    );
    // Swin's window shuffles are 6-D.
    check(
        "Transpose",
        vec![Arg::Input(vec![1, 2, 3, 2, 3, 4])],
        vec![attr_ints("perm", &[0, 1, 3, 2, 4, 5])],
        1,
        0.0,
    );
    check(
        "Add",
        vec![
            Arg::Input(vec![2, 3, 1, 4, 5]),
            Arg::Input(vec![3, 2, 4, 1]),
        ],
        vec![],
        1,
        1e-6,
    );
}
