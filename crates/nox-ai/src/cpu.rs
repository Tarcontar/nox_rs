//! Operators on the CPU: what planning works out ahead of time (shapes,
//! constant subgraphs), and the reference the GPU kernels are tested
//! against. Straightforward loops, written for clarity.
//!
//! The `*_params` functions resolve an operator's attributes and constant
//! inputs into what both this and the GPU kernels compute with, so the
//! two agree on shapes and coordinates.

use std::sync::Arc;

use crate::model::Node;
use crate::tensor::{Data, Tensor, strides};
use crate::{Error, Result};

/// Runs `node` on `inputs` (`None` for left-out optional inputs).
pub(crate) fn run(node: &Node, inputs: &[Option<&Tensor>]) -> Result<Vec<Tensor>> {
    let input = |i: usize| -> Result<&Tensor> {
        inputs
            .get(i)
            .copied()
            .flatten()
            .ok_or_else(|| Error::Model(format!("{} lacks input {i}", node.describe())))
    };
    let one = |t: Tensor| Ok(vec![t]);
    match node.op.as_str() {
        "Add" | "Sub" | "Mul" | "Div" | "Pow" => one(binary(&node.op, input(0)?, input(1)?)?),
        "Relu" | "Sigmoid" | "Tanh" | "Exp" | "Sqrt" | "Neg" | "Abs" | "Erf" | "HardSigmoid"
        | "HardSwish" | "Clip" | "Identity" | "Floor" => {
            let (op, alpha, beta) = unary_params(node, inputs)?;
            let x = input(0)?;
            match (&x.data, op) {
                // As it is (shared; integers stay integers).
                (_, UnaryOp::Identity) => one(x.clone()),
                _ => one(Tensor::f32(
                    x.shape.clone(),
                    x.to_f32()
                        .iter()
                        .map(|&v| unary(op, v, alpha, beta))
                        .collect(),
                )),
            }
        }
        "Cast" => {
            let x = input(0)?;
            // 1: float, 10: half, 11: double; 9: bool; the rest integers.
            match node.int("to", 1) {
                1 | 10 | 11 if matches!(x.data, Data::F32(_)) => one(x.clone()),
                1 | 10 | 11 => one(Tensor::f32(x.shape.clone(), x.to_f32())),
                9 => one(Tensor::i64(
                    x.shape.clone(),
                    x.to_f32().iter().map(|&v| i64::from(v != 0.0)).collect(),
                )),
                _ if matches!(x.data, Data::I64(_)) => one(x.clone()),
                _ => one(Tensor::i64(x.shape.clone(), x.to_i64())),
            }
        }
        "MatMul" => one(matmul(input(0)?, input(1)?)?),
        "LayerNormalization" => one(layer_norm(
            node,
            input(0)?,
            input(1)?,
            inputs.get(2).copied().flatten(),
        )?),
        "Softmax" => one(softmax(node, input(0)?)?),
        "GatherND" => one(gather_nd(node, input(0)?, input(1)?)?),
        "Pad" => {
            let x = input(0)?;
            let p = pad_params(node, &x.shape, inputs)?;
            one(pad(x, &p))
        }
        "Sum" => {
            let mut parts = inputs.iter().flatten();
            let mut sum = (*parts
                .next()
                .ok_or_else(|| Error::Model("Sum of nothing".into()))?)
            .clone();
            for part in parts {
                sum = binary("Add", &sum, part)?;
            }
            one(sum)
        }
        "BatchNormalization" => {
            let x = input(0)?;
            let (scale, shift) = batch_norm_params(node, x.shape.len(), inputs)?;
            one(binary("Add", &binary("Mul", x, &scale)?, &shift)?)
        }
        "ConstantOfShape" => {
            let shape: Vec<usize> = input(0)?
                .to_i64()
                .iter()
                .map(|&d| d.max(0) as usize)
                .collect();
            let n = shape.iter().product();
            one(match node.attrs.get("value") {
                Some(crate::model::Attr::Tensor(t)) => match &t.data {
                    Data::I64(v) => Tensor::i64(shape, vec![v.first().copied().unwrap_or(0); n]),
                    Data::F32(v) => Tensor::f32(shape, vec![v.first().copied().unwrap_or(0.0); n]),
                },
                _ => Tensor::f32(shape, vec![0.0; n]),
            })
        }
        "Equal" | "Less" | "Greater" | "And" | "Or" | "Mod" => {
            one(compare(node, input(0)?, input(1)?)?)
        }
        "Not" => {
            let x = input(0)?;
            one(Tensor::i64(
                x.shape.clone(),
                x.i64_values().iter().map(|&v| i64::from(v == 0)).collect(),
            ))
        }
        "Where" => one(where_(input(0)?, input(1)?, input(2)?)?),
        "ScatterND" => one(scatter_nd(input(0)?, input(1)?, input(2)?)?),
        "Range" => {
            let (start, limit, delta) = (input(0)?, input(1)?, input(2)?);
            match (&start.data, &limit.data, &delta.data) {
                (Data::I64(a), Data::I64(b), Data::I64(c)) => {
                    let (a, b, c) = (a[0], b[0], c[0]);
                    let v: Vec<i64> = (0..)
                        .map(|i| a + i * c)
                        .take_while(|&v| if c > 0 { v < b } else { v > b })
                        .collect();
                    one(Tensor::i64(vec![v.len()], v))
                }
                _ => {
                    let (a, b, c) = (start.to_f32()[0], limit.to_f32()[0], delta.to_f32()[0]);
                    let n = ((b - a) / c).ceil().max(0.0) as usize;
                    one(Tensor::f32(
                        vec![n],
                        (0..n).map(|i| a + i as f32 * c).collect(),
                    ))
                }
            }
        }
        "Shape" => {
            let x = input(0)?;
            one(Tensor::i64(
                vec![x.shape.len()],
                x.shape.iter().map(|&d| d as i64).collect(),
            ))
        }
        "Reshape" | "Flatten" | "Squeeze" | "Unsqueeze" => {
            let x = input(0)?;
            let shape = reshape_params(node, &x.shape, inputs)?;
            one(Tensor {
                shape,
                data: x.data.clone(),
            })
        }
        "Transpose" => {
            let x = input(0)?;
            let perm = transpose_perm(node, x.shape.len());
            let shape: Vec<usize> = perm.iter().map(|&p| x.shape[p]).collect();
            let in_strides = strides(&x.shape);
            let src = Strided {
                shape: shape.clone(),
                strides: perm.iter().map(|&p| in_strides[p] as isize).collect(),
                offset: 0,
            };
            one(gather_strided(x, &src))
        }
        "Concat" => {
            let parts: Vec<&Tensor> = inputs.iter().flatten().copied().collect();
            let (axis, shape) = concat_params(
                node,
                &parts.iter().map(|t| t.shape.clone()).collect::<Vec<_>>(),
            )?;
            let mut out = Tensor {
                shape: shape.clone(),
                data: match parts[0].data {
                    Data::F32(_) => Data::F32(Arc::new(vec![0.0; shape.iter().product()])),
                    Data::I64(_) => Data::I64(Arc::new(vec![0; shape.iter().product()])),
                },
            };
            let mut at = 0;
            for part in parts {
                scatter_strided(&mut out, part, axis, at);
                at += part.shape[axis];
            }
            one(out)
        }
        "Split" => {
            let x = input(0)?;
            let (axis, sizes) = split_params(node, &x.shape, inputs, node.outputs.len())?;
            let mut out = Vec::new();
            let mut at = 0;
            for size in sizes {
                let mut shape = x.shape.clone();
                shape[axis] = size;
                let in_strides = strides(&x.shape);
                let src = Strided {
                    shape,
                    strides: in_strides.iter().map(|&s| s as isize).collect(),
                    offset: at * in_strides[axis],
                };
                out.push(gather_strided(x, &src));
                at += size;
            }
            Ok(out)
        }
        "Slice" => {
            let x = input(0)?;
            let src = slice_params(node, &x.shape, inputs)?;
            one(gather_strided(x, &src))
        }
        "Expand" => {
            let x = input(0)?;
            let src = expand_params(&x.shape, &input(1)?.to_i64())?;
            one(gather_strided(x, &src))
        }
        "Gather" => {
            let x = input(0)?;
            let indices = input(1)?.to_i64();
            let rank = x.shape.len() as i64;
            let axis = normalize_axis(node.int("axis", 0), rank)?;
            let mut shape = x.shape[..axis].to_vec();
            shape.extend(&input(1)?.shape);
            shape.extend(&x.shape[axis + 1..]);
            let outer: usize = x.shape[..axis].iter().product();
            let inner: usize = x.shape[axis + 1..].iter().product();
            let n = x.shape[axis] as i64;
            let pick = |o: usize, i: i64, k: usize| {
                let i = if i < 0 { i + n } else { i } as usize;
                (o * x.shape[axis] + i) * inner + k
            };
            let order = (0..outer).flat_map(|o| {
                indices
                    .iter()
                    .flat_map(move |&i| (0..inner).map(move |k| (o, i, k)))
            });
            one(match &x.data {
                Data::F32(d) => {
                    Tensor::f32(shape, order.map(|(o, i, k)| d[pick(o, i, k)]).collect())
                }
                Data::I64(d) => {
                    Tensor::i64(shape, order.map(|(o, i, k)| d[pick(o, i, k)]).collect())
                }
            })
        }
        "Conv" => one(conv(
            node,
            input(0)?,
            input(1)?,
            inputs.get(2).copied().flatten(),
        )?),
        "Resize" | "Upsample" => {
            let x = input(0)?;
            let p = resize_params(node, &x.shape, inputs)?;
            one(resize(x, &p))
        }
        "AveragePool" | "MaxPool" => {
            let x = input(0)?;
            let p = pool_params(node, &x.shape)?;
            one(pool(x, &p))
        }
        "GlobalAveragePool" | "ReduceMean" | "ReduceSum" => {
            let x = input(0)?;
            let p = reduce_params(node, &x.shape, inputs)?;
            one(reduce(x, &p))
        }
        other => Err(Error::Unsupported(format!("operator {other} on the CPU"))),
    }
}

/// `axis` counted from the front (`-1` is the last).
pub(crate) fn normalize_axis(axis: i64, rank: i64) -> Result<usize> {
    let a = if axis < 0 { axis + rank } else { axis };
    if (0..rank.max(1)).contains(&a) {
        Ok(a as usize)
    } else {
        Err(Error::Model(format!("axis {axis} of a rank-{rank} tensor")))
    }
}

// ---------------------------------------------------------------- binary

/// NumPy-style broadcast of two shapes.
pub(crate) fn broadcast_shape(a: &[usize], b: &[usize]) -> Result<Vec<usize>> {
    let rank = a.len().max(b.len());
    let at = |s: &[usize], i: usize| {
        let pad = rank - s.len();
        if i < pad { 1 } else { s[i - pad] }
    };
    (0..rank)
        .map(|i| match (at(a, i), at(b, i)) {
            (x, y) if x == y => Ok(x),
            (1, y) => Ok(y),
            (x, 1) => Ok(x),
            (x, y) => Err(Error::Model(format!(
                "can't broadcast {a:?} with {b:?} ({x} vs {y})"
            ))),
        })
        .collect()
}

/// Strides of `shape` read as broadcast to `out` (0 where repeated).
pub(crate) fn broadcast_strides(shape: &[usize], out: &[usize]) -> Vec<usize> {
    let pad = out.len() - shape.len();
    let own = strides(shape);
    (0..out.len())
        .map(|i| {
            if i < pad || shape[i - pad] == 1 {
                0
            } else {
                own[i - pad]
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum BinaryOp {
    Add = 0,
    Sub = 1,
    Mul = 2,
    Div = 3,
    Pow = 4,
}

impl BinaryOp {
    pub fn of(op: &str) -> Option<Self> {
        Some(match op {
            "Add" => Self::Add,
            "Sub" => Self::Sub,
            "Mul" => Self::Mul,
            "Div" => Self::Div,
            "Pow" => Self::Pow,
            _ => return None,
        })
    }
}

fn binary(op: &str, a: &Tensor, b: &Tensor) -> Result<Tensor> {
    let op = BinaryOp::of(op).expect("binary op");
    if let (Data::I64(x), Data::I64(y)) = (&a.data, &b.data) {
        let (shape, data) = broadcast_map(&[a, b], &[x, y], |v| {
            let (x, y) = (v[0], v[1]);
            match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                BinaryOp::Mul => x * y,
                BinaryOp::Div => x.checked_div(y).unwrap_or(0),
                BinaryOp::Pow => x.pow(y.max(0) as u32),
            }
        })?;
        return Ok(Tensor::i64(shape, data));
    }
    let (shape, data) = broadcast_map(&[a, b], &[&a.f32_values(), &b.f32_values()], |v| {
        binary_f32(op, v[0], v[1])
    })?;
    Ok(Tensor::f32(shape, data))
}

pub(crate) fn binary_f32(op: BinaryOp, x: f32, y: f32) -> f32 {
    match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div => x / y,
        BinaryOp::Pow => x.powf(y),
    }
}

// ----------------------------------------------------------------- unary

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum UnaryOp {
    Identity = 0,
    Relu = 1,
    Sigmoid = 2,
    Tanh = 3,
    Exp = 4,
    Sqrt = 5,
    Neg = 6,
    Abs = 7,
    Erf = 8,
    /// `max(0, min(1, alpha x + beta))`
    HardSigmoid = 9,
    /// `x · HardSigmoid(x)`
    HardSwish = 10,
    /// `min(max(x, alpha), beta)`
    Clip = 11,
    Floor = 12,
    /// Toward zero (casting floats to integers).
    Trunc = 13,
    /// `alpha`, whatever `x` is.
    Fill = 14,
}

/// The operation and its two parameters.
pub(crate) fn unary_params(node: &Node, inputs: &[Option<&Tensor>]) -> Result<(UnaryOp, f32, f32)> {
    let scalar = |i: usize, default: f32| -> f32 {
        inputs
            .get(i)
            .copied()
            .flatten()
            .and_then(|t| t.to_f32().first().copied())
            .unwrap_or(default)
    };
    Ok(match node.op.as_str() {
        "Identity" => (UnaryOp::Identity, 0.0, 0.0),
        "Relu" => (UnaryOp::Relu, 0.0, 0.0),
        "Sigmoid" => (UnaryOp::Sigmoid, 0.0, 0.0),
        "Tanh" => (UnaryOp::Tanh, 0.0, 0.0),
        "Exp" => (UnaryOp::Exp, 0.0, 0.0),
        "Sqrt" => (UnaryOp::Sqrt, 0.0, 0.0),
        "Neg" => (UnaryOp::Neg, 0.0, 0.0),
        "Abs" => (UnaryOp::Abs, 0.0, 0.0),
        "Erf" => (UnaryOp::Erf, 0.0, 0.0),
        "Floor" => (UnaryOp::Floor, 0.0, 0.0),
        "HardSigmoid" => (
            UnaryOp::HardSigmoid,
            node.float("alpha", 0.2),
            node.float("beta", 0.5),
        ),
        "HardSwish" => (UnaryOp::HardSwish, 1.0 / 6.0, 0.5),
        // Before opset 11 the bounds are attributes, after inputs.
        "Clip" => (
            UnaryOp::Clip,
            scalar(1, node.float("min", f32::MIN)),
            scalar(2, node.float("max", f32::MAX)),
        ),
        other => return Err(Error::Unsupported(format!("unary operator {other}"))),
    })
}

pub(crate) fn unary(op: UnaryOp, x: f32, alpha: f32, beta: f32) -> f32 {
    let hard_sigmoid = |x: f32| (alpha * x + beta).clamp(0.0, 1.0);
    match op {
        UnaryOp::Identity => x,
        UnaryOp::Relu => x.max(0.0),
        UnaryOp::Sigmoid => 1.0 / (1.0 + (-x).exp()),
        UnaryOp::Tanh => x.tanh(),
        UnaryOp::Exp => x.exp(),
        UnaryOp::Sqrt => x.sqrt(),
        UnaryOp::Neg => -x,
        UnaryOp::Abs => x.abs(),
        UnaryOp::Erf => erf(x),
        UnaryOp::HardSigmoid => hard_sigmoid(x),
        UnaryOp::HardSwish => x * hard_sigmoid(x),
        UnaryOp::Clip => x.max(alpha).min(beta),
        UnaryOp::Floor => x.floor(),
        UnaryOp::Trunc => x.trunc(),
        UnaryOp::Fill => alpha,
    }
}

/// Abramowitz–Stegun 7.1.26 (|error| < 1.5e-7), as the GPU computes it.
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_8) * t - 0.284_496_74) * t
            + 0.254_829_6)
            * t
            * (-x * x).exp();
    y.copysign(x)
}

// ---------------------------------------------------------- reshaping etc.

/// The output shape of Reshape, Flatten, Squeeze and Unsqueeze.
pub(crate) fn reshape_params(
    node: &Node,
    shape: &[usize],
    inputs: &[Option<&Tensor>],
) -> Result<Vec<usize>> {
    let rank = shape.len() as i64;
    let axes = |out_rank: i64| -> Result<Vec<usize>> {
        let list = match inputs.get(1).copied().flatten() {
            Some(t) => t.to_i64(),
            None => node.ints("axes").map(<[i64]>::to_vec).unwrap_or_default(),
        };
        list.iter().map(|&a| normalize_axis(a, out_rank)).collect()
    };
    match node.op.as_str() {
        "Reshape" => {
            let target = inputs
                .get(1)
                .copied()
                .flatten()
                .ok_or_else(|| Error::Model("Reshape without a shape".into()))?
                .to_i64();
            let allow_zero = node.int("allowzero", 0) != 0;
            let total: usize = shape.iter().product();
            let mut out: Vec<usize> = Vec::new();
            let mut unknown = None;
            for (i, &d) in target.iter().enumerate() {
                match d {
                    -1 => {
                        unknown = Some(i);
                        out.push(1);
                    }
                    0 if !allow_zero => out.push(*shape.get(i).unwrap_or(&0)),
                    d => out.push(d as usize),
                }
            }
            if let Some(i) = unknown {
                let known: usize = out.iter().product();
                out[i] = total.checked_div(known).unwrap_or(0);
            }
            if out.iter().product::<usize>() != total {
                return Err(Error::Model(format!(
                    "can't reshape {shape:?} to {target:?}"
                )));
            }
            Ok(out)
        }
        "Flatten" => {
            let axis = normalize_axis(node.int("axis", 1), rank + 1)?;
            Ok(vec![
                shape[..axis].iter().product(),
                shape[axis..].iter().product(),
            ])
        }
        "Squeeze" => {
            let axes = axes(rank)?;
            Ok(shape
                .iter()
                .enumerate()
                .filter(|&(i, &d)| {
                    if axes.is_empty() {
                        d != 1
                    } else {
                        !axes.contains(&i)
                    }
                })
                .map(|(_, &d)| d)
                .collect())
        }
        _ => {
            let n = match inputs.get(1).copied().flatten() {
                Some(t) => t.len(),
                None => node.ints("axes").map_or(0, <[i64]>::len),
            } as i64;
            let axes = axes(rank + n)?;
            let mut rest = shape.iter();
            Ok((0..(rank + n) as usize)
                .map(|i| {
                    if axes.contains(&i) {
                        1
                    } else {
                        *rest.next().expect("dims")
                    }
                })
                .collect())
        }
    }
}

pub(crate) fn transpose_perm(node: &Node, rank: usize) -> Vec<usize> {
    node.ints("perm")
        .map(|p| p.iter().map(|&a| a as usize).collect())
        .unwrap_or_else(|| (0..rank).rev().collect())
}

/// A strided view: element `index` of `shape` is at `offset + Σ index·stride`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Strided {
    pub shape: Vec<usize>,
    pub strides: Vec<isize>,
    pub offset: usize,
}

/// Copies the view `src` of `x` into a new contiguous tensor.
fn gather_strided(x: &Tensor, src: &Strided) -> Tensor {
    let n: usize = src.shape.iter().product();
    let at = |i: usize| {
        let mut rest = i;
        let mut at = src.offset as isize;
        for d in (0..src.shape.len()).rev() {
            at += (rest % src.shape[d]) as isize * src.strides[d];
            rest /= src.shape[d];
        }
        at as usize
    };
    match &x.data {
        Data::F32(d) => Tensor::f32(src.shape.clone(), (0..n).map(|i| d[at(i)]).collect()),
        Data::I64(d) => Tensor::i64(src.shape.clone(), (0..n).map(|i| d[at(i)]).collect()),
    }
}

/// Writes `part` into `out` at position `at` along `axis`.
fn scatter_strided(out: &mut Tensor, part: &Tensor, axis: usize, at: usize) {
    let out_strides = strides(&out.shape);
    let n = part.len();
    let to = |i: usize| {
        let mut rest = i;
        let mut idx = at * out_strides[axis];
        for d in (0..part.shape.len()).rev() {
            idx += (rest % part.shape[d]) * out_strides[d];
            rest /= part.shape[d];
        }
        idx
    };
    match (&mut out.data, &part.data) {
        (Data::F32(o), _) => {
            let o = Arc::make_mut(o);
            let p = part.to_f32();
            for i in 0..n {
                o[to(i)] = p[i];
            }
        }
        (Data::I64(o), _) => {
            let o = Arc::make_mut(o);
            let p = part.to_i64();
            for i in 0..n {
                o[to(i)] = p[i];
            }
        }
    }
}

/// The axis and output shape of Concat.
pub(crate) fn concat_params(node: &Node, shapes: &[Vec<usize>]) -> Result<(usize, Vec<usize>)> {
    let first = shapes
        .first()
        .ok_or_else(|| Error::Model("Concat without inputs".into()))?;
    let axis = normalize_axis(node.int("axis", 0), first.len() as i64)?;
    let mut shape = first.clone();
    shape[axis] = shapes.iter().map(|s| s[axis]).sum();
    for s in shapes {
        let same = s.len() == shape.len() && (0..s.len()).all(|d| d == axis || s[d] == shape[d]);
        if !same {
            return Err(Error::Model(format!(
                "can't concatenate {shapes:?} on {axis}"
            )));
        }
    }
    Ok((axis, shape))
}

/// The axis and part sizes of Split.
pub(crate) fn split_params(
    node: &Node,
    shape: &[usize],
    inputs: &[Option<&Tensor>],
    outputs: usize,
) -> Result<(usize, Vec<usize>)> {
    let axis = normalize_axis(node.int("axis", 0), shape.len() as i64)?;
    let sizes: Vec<usize> = match inputs.get(1).copied().flatten() {
        Some(t) => t.to_i64().iter().map(|&s| s as usize).collect(),
        None => match node.ints("split") {
            Some(s) => s.iter().map(|&s| s as usize).collect(),
            None => {
                let each = shape[axis].div_ceil(outputs.max(1));
                (0..outputs)
                    .map(|i| each.min(shape[axis].saturating_sub(i * each)))
                    .collect()
            }
        },
    };
    if sizes.iter().sum::<usize>() != shape[axis] {
        return Err(Error::Model(format!(
            "can't split {shape:?} into {sizes:?}"
        )));
    }
    Ok((axis, sizes))
}

/// The view Slice takes of a tensor of `shape`.
pub(crate) fn slice_params(
    node: &Node,
    shape: &[usize],
    inputs: &[Option<&Tensor>],
) -> Result<Strided> {
    let rank = shape.len();
    let list = |i: usize, attr: &str| -> Option<Vec<i64>> {
        match inputs.get(i).copied().flatten() {
            Some(t) => Some(t.to_i64()),
            None => node.ints(attr).map(<[i64]>::to_vec),
        }
    };
    let starts = list(1, "starts").ok_or_else(|| Error::Model("Slice without starts".into()))?;
    let ends = list(2, "ends").ok_or_else(|| Error::Model("Slice without ends".into()))?;
    let axes = list(3, "axes").unwrap_or_else(|| (0..starts.len() as i64).collect());
    let steps = list(4, "steps").unwrap_or_else(|| vec![1; starts.len()]);
    let own = strides(shape);
    let mut out = Strided {
        shape: shape.to_vec(),
        strides: own.iter().map(|&s| s as isize).collect(),
        offset: 0,
    };
    for k in 0..starts.len() {
        let axis = normalize_axis(axes[k], rank as i64)?;
        let dim = shape[axis] as i64;
        let step = steps[k];
        if step == 0 {
            return Err(Error::Model("Slice with step 0".into()));
        }
        let clamp = |v: i64, lo: i64, hi: i64| v.clamp(lo, hi);
        let fix = |v: i64| if v < 0 { v + dim } else { v };
        let (start, end) = if step > 0 {
            (clamp(fix(starts[k]), 0, dim), clamp(fix(ends[k]), 0, dim))
        } else {
            (
                clamp(fix(starts[k]), -1, dim - 1),
                clamp(fix(ends[k]), -1, dim - 1),
            )
        };
        let len = if step > 0 {
            (end - start + step - 1).max(0) / step
        } else {
            (start - end - step - 1).max(0) / -step
        };
        out.shape[axis] = len as usize;
        out.strides[axis] = own[axis] as isize * step as isize;
        out.offset += start.max(0) as usize * own[axis];
    }
    Ok(out)
}

/// The view Expand takes of a tensor of `shape` (broadcast to `target`).
pub(crate) fn expand_params(shape: &[usize], target: &[i64]) -> Result<Strided> {
    let target: Vec<usize> = target.iter().map(|&d| d.max(0) as usize).collect();
    let out = broadcast_shape(shape, &target)?;
    Ok(Strided {
        strides: broadcast_strides(shape, &out)
            .iter()
            .map(|&s| s as isize)
            .collect(),
        shape: out,
        offset: 0,
    })
}

// ------------------------------------------------------------------ conv

/// A 2D convolution of NCHW tensors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConvParams {
    pub batch: usize,
    pub in_channels: usize,
    pub in_h: usize,
    pub in_w: usize,
    pub out_channels: usize,
    pub out_h: usize,
    pub out_w: usize,
    pub kernel_h: usize,
    pub kernel_w: usize,
    pub stride: [usize; 2],
    pub pad: [usize; 2],
    pub dilation: [usize; 2],
    pub group: usize,
}

pub(crate) fn conv_params(node: &Node, x: &[usize], w: &[usize]) -> Result<ConvParams> {
    if x.len() != 4 || w.len() != 4 {
        return Err(Error::Unsupported(format!(
            "{} on {x:?} (only 2D convolutions)",
            node.describe()
        )));
    }
    let two = |name: &str, default: usize| -> [usize; 2] {
        node.ints(name)
            .map_or([default; 2], |v| [v[0] as usize, v[1] as usize])
    };
    let stride = two("strides", 1);
    let dilation = two("dilations", 1);
    let group = node.int("group", 1).max(1) as usize;
    let (kernel_h, kernel_w) = (w[2], w[3]);
    let span = |k: usize, d: usize| (k - 1) * d + 1;
    let pads = match node.string("auto_pad", "NOTSET").as_str() {
        "NOTSET" | "VALID" => {
            let p = node.ints("pads").map(<[i64]>::to_vec).unwrap_or(vec![0; 4]);
            if p[0] != p[2] || p[1] != p[3] {
                return Err(Error::Unsupported(format!(
                    "{} with uneven padding {p:?}",
                    node.describe()
                )));
            }
            [p[0] as usize, p[1] as usize]
        }
        mode => {
            // SAME_*: as much padding as keeps size / stride; even only.
            let pad = |size: usize, s: usize, k: usize, d: usize| {
                let out = size.div_ceil(s);
                ((out - 1) * s + span(k, d)).saturating_sub(size)
            };
            let total = [
                pad(x[2], stride[0], kernel_h, dilation[0]),
                pad(x[3], stride[1], kernel_w, dilation[1]),
            ];
            if total.iter().any(|t| t % 2 != 0) {
                return Err(Error::Unsupported(format!(
                    "{} with {mode} and uneven padding",
                    node.describe()
                )));
            }
            [total[0] / 2, total[1] / 2]
        }
    };
    let out = |size: usize, p: usize, s: usize, k: usize, d: usize| {
        (size + 2 * p).saturating_sub(span(k, d)) / s + 1
    };
    if x[1] != w[1] * group || !w[0].is_multiple_of(group) {
        return Err(Error::Model(format!(
            "{}: {x:?} doesn't fit weights {w:?} in {group} groups",
            node.describe()
        )));
    }
    Ok(ConvParams {
        batch: x[0],
        in_channels: x[1],
        in_h: x[2],
        in_w: x[3],
        out_channels: w[0],
        out_h: out(x[2], pads[0], stride[0], kernel_h, dilation[0]),
        out_w: out(x[3], pads[1], stride[1], kernel_w, dilation[1]),
        kernel_h,
        kernel_w,
        stride,
        pad: pads,
        dilation,
        group,
    })
}

fn conv(node: &Node, x: &Tensor, w: &Tensor, bias: Option<&Tensor>) -> Result<Tensor> {
    let p = conv_params(node, &x.shape, &w.shape)?;
    let (xd, wd) = (x.to_f32(), w.to_f32());
    let bias = bias.map(Tensor::to_f32);
    let per_group_in = p.in_channels / p.group;
    let per_group_out = p.out_channels / p.group;
    let mut out = vec![0f32; p.batch * p.out_channels * p.out_h * p.out_w];
    for n in 0..p.batch {
        for m in 0..p.out_channels {
            let g = m / per_group_out;
            for oy in 0..p.out_h {
                for ox in 0..p.out_w {
                    let mut sum = bias.as_ref().map_or(0.0, |b| b[m]);
                    for c in 0..per_group_in {
                        let ic = g * per_group_in + c;
                        for ky in 0..p.kernel_h {
                            let iy = (oy * p.stride[0] + ky * p.dilation[0]) as isize
                                - p.pad[0] as isize;
                            if iy < 0 || iy >= p.in_h as isize {
                                continue;
                            }
                            for kx in 0..p.kernel_w {
                                let ix = (ox * p.stride[1] + kx * p.dilation[1]) as isize
                                    - p.pad[1] as isize;
                                if ix < 0 || ix >= p.in_w as isize {
                                    continue;
                                }
                                let xi = ((n * p.in_channels + ic) * p.in_h + iy as usize) * p.in_w
                                    + ix as usize;
                                let wi =
                                    ((m * per_group_in + c) * p.kernel_h + ky) * p.kernel_w + kx;
                                sum += xd[xi] * wd[wi];
                            }
                        }
                    }
                    out[((n * p.out_channels + m) * p.out_h + oy) * p.out_w + ox] = sum;
                }
            }
        }
    }
    Ok(Tensor::f32(
        vec![p.batch, p.out_channels, p.out_h, p.out_w],
        out,
    ))
}

// ---------------------------------------------------------------- resize

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum CoordinateMode {
    HalfPixel = 0,
    PytorchHalfPixel = 1,
    AlignCorners = 2,
    Asymmetric = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum NearestMode {
    RoundPreferFloor = 0,
    RoundPreferCeil = 1,
    Floor = 2,
    Ceil = 3,
}

/// Resizing the last two axes of an NCHW tensor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResizeParams {
    /// N × C.
    pub planes: usize,
    pub in_h: usize,
    pub in_w: usize,
    pub out_h: usize,
    pub out_w: usize,
    pub scale_h: f32,
    pub scale_w: f32,
    pub linear: bool,
    pub coordinates: CoordinateMode,
    pub nearest: NearestMode,
}

pub(crate) fn resize_params(
    node: &Node,
    x: &[usize],
    inputs: &[Option<&Tensor>],
) -> Result<ResizeParams> {
    if x.len() != 4 {
        return Err(Error::Unsupported(format!(
            "{} of a rank-{} tensor",
            node.describe(),
            x.len()
        )));
    }
    let given = |i: usize| inputs.get(i).copied().flatten().filter(|t| !t.is_empty());
    // Opset 10 (and Upsample): X, scales. Later: X, roi, scales, sizes.
    let (scales, sizes) = if node.op == "Upsample" || inputs.len() == 2 {
        (given(1).map(Tensor::to_f32), None)
    } else {
        (given(2).map(Tensor::to_f32), given(3).map(Tensor::to_i64))
    };
    let (out, scale): (Vec<usize>, Vec<f32>) = match (sizes, scales) {
        (Some(sizes), _) => {
            let out: Vec<usize> = sizes.iter().map(|&s| s as usize).collect();
            let scale = out
                .iter()
                .zip(x)
                .map(|(&o, &i)| o as f32 / i as f32)
                .collect();
            (out, scale)
        }
        (None, Some(scales)) => (
            x.iter()
                .zip(&scales)
                .map(|(&i, &s)| (i as f32 * s).floor() as usize)
                .collect(),
            scales,
        ),
        (None, None) => {
            return Err(Error::Model(format!(
                "{} without scales or sizes",
                node.describe()
            )));
        }
    };
    if out.len() != 4 || out[0] != x[0] || out[1] != x[1] {
        return Err(Error::Unsupported(format!(
            "{} resizing {x:?} to {out:?} (only height and width)",
            node.describe()
        )));
    }
    let linear = match node.string("mode", "nearest").as_str() {
        "nearest" => false,
        "linear" | "bilinear" => true,
        mode => {
            return Err(Error::Unsupported(format!(
                "{} in {mode} mode",
                node.describe()
            )));
        }
    };
    let coordinates = match node
        .string("coordinate_transformation_mode", "half_pixel")
        .as_str()
    {
        "half_pixel" => CoordinateMode::HalfPixel,
        "pytorch_half_pixel" => CoordinateMode::PytorchHalfPixel,
        "align_corners" => CoordinateMode::AlignCorners,
        "asymmetric" => CoordinateMode::Asymmetric,
        mode => {
            return Err(Error::Unsupported(format!(
                "{} with {mode} coordinates",
                node.describe()
            )));
        }
    };
    // Upsample and Resize before opset 11 map coordinates asymmetrically.
    let coordinates = if node.op == "Upsample" || inputs.len() == 2 {
        CoordinateMode::Asymmetric
    } else {
        coordinates
    };
    let nearest = match node.string("nearest_mode", "round_prefer_floor").as_str() {
        "round_prefer_ceil" => NearestMode::RoundPreferCeil,
        "floor" => NearestMode::Floor,
        "ceil" => NearestMode::Ceil,
        _ => NearestMode::RoundPreferFloor,
    };
    Ok(ResizeParams {
        planes: x[0] * x[1],
        in_h: x[2],
        in_w: x[3],
        out_h: out[2],
        out_w: out[3],
        scale_h: scale[2],
        scale_w: scale[3],
        linear,
        coordinates,
        nearest,
    })
}

/// Where output coordinate `o` comes from in the input (ONNX Resize).
pub(crate) fn source_coordinate(
    mode: CoordinateMode,
    o: usize,
    scale: f32,
    in_len: usize,
    out_len: usize,
) -> f32 {
    let o = o as f32;
    match mode {
        CoordinateMode::HalfPixel => (o + 0.5) / scale - 0.5,
        CoordinateMode::PytorchHalfPixel => {
            if out_len > 1 {
                (o + 0.5) / scale - 0.5
            } else {
                0.0
            }
        }
        CoordinateMode::AlignCorners => {
            if out_len > 1 {
                o * (in_len as f32 - 1.0) / (out_len as f32 - 1.0)
            } else {
                0.0
            }
        }
        CoordinateMode::Asymmetric => o / scale,
    }
}

/// Rounding to the nearest pixel allows this much: coordinates that are
/// exactly whole or halfway must round the same on the CPU and the GPU,
/// whose division may be an ulp or two off (resize.comp's SLACK).
const NEAREST_SLACK: f32 = 1e-4;

fn resize(x: &Tensor, p: &ResizeParams) -> Tensor {
    let data = x.to_f32();
    let mut out = vec![0f32; p.planes * p.out_h * p.out_w];
    let nearest = |v: f32, len: usize| -> usize {
        let i = match p.nearest {
            NearestMode::Floor => (v + NEAREST_SLACK).floor(),
            NearestMode::Ceil => (v - NEAREST_SLACK).ceil(),
            NearestMode::RoundPreferFloor => (v - 0.5 - NEAREST_SLACK).ceil(),
            NearestMode::RoundPreferCeil => (v + 0.5 + NEAREST_SLACK).floor(),
        };
        (i.max(0.0) as usize).min(len - 1)
    };
    for plane in 0..p.planes {
        let src = &data[plane * p.in_h * p.in_w..][..p.in_h * p.in_w];
        for oy in 0..p.out_h {
            let sy = source_coordinate(p.coordinates, oy, p.scale_h, p.in_h, p.out_h);
            for ox in 0..p.out_w {
                let sx = source_coordinate(p.coordinates, ox, p.scale_w, p.in_w, p.out_w);
                let v = if p.linear {
                    let sy = sy.clamp(0.0, (p.in_h - 1) as f32);
                    let sx = sx.clamp(0.0, (p.in_w - 1) as f32);
                    let (y0, x0) = (sy.floor() as usize, sx.floor() as usize);
                    let (y1, x1) = ((y0 + 1).min(p.in_h - 1), (x0 + 1).min(p.in_w - 1));
                    let (fy, fx) = (sy - y0 as f32, sx - x0 as f32);
                    let at = |y: usize, x: usize| src[y * p.in_w + x];
                    let top = at(y0, x0) * (1.0 - fx) + at(y0, x1) * fx;
                    let bottom = at(y1, x0) * (1.0 - fx) + at(y1, x1) * fx;
                    top * (1.0 - fy) + bottom * fy
                } else {
                    src[nearest(sy, p.in_h) * p.in_w + nearest(sx, p.in_w)]
                };
                out[(plane * p.out_h + oy) * p.out_w + ox] = v;
            }
        }
    }
    Tensor::f32(vec![x.shape[0], x.shape[1], p.out_h, p.out_w], out)
}

// ------------------------------------------------------------------ pool

/// A 2D pooling window over NCHW.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PoolParams {
    pub planes: usize,
    pub in_h: usize,
    pub in_w: usize,
    pub out_h: usize,
    pub out_w: usize,
    pub kernel: [usize; 2],
    pub stride: [usize; 2],
    pub pad: [usize; 2],
    pub max: bool,
    pub count_include_pad: bool,
}

pub(crate) fn pool_params(node: &Node, x: &[usize]) -> Result<PoolParams> {
    if x.len() != 4 {
        return Err(Error::Unsupported(format!(
            "{} of a rank-{} tensor",
            node.describe(),
            x.len()
        )));
    }
    let k = node
        .ints("kernel_shape")
        .ok_or_else(|| Error::Model(format!("{} without kernel_shape", node.describe())))?;
    let kernel = [k[0] as usize, k[1] as usize];
    let stride = node
        .ints("strides")
        .map_or([1, 1], |s| [s[0] as usize, s[1] as usize]);
    let p = node.ints("pads").map(<[i64]>::to_vec).unwrap_or(vec![0; 4]);
    if p[0] != p[2] || p[1] != p[3] || node.string("auto_pad", "NOTSET") != "NOTSET" {
        return Err(Error::Unsupported(format!(
            "{} with uneven or automatic padding",
            node.describe()
        )));
    }
    let pad = [p[0] as usize, p[1] as usize];
    let ceil = node.int("ceil_mode", 0) != 0;
    let out = |size: usize, i: usize| {
        let span = size + 2 * pad[i] - kernel[i];
        let mut n = if ceil {
            span.div_ceil(stride[i])
        } else {
            span / stride[i]
        } + 1;
        // A window must start inside the input (or its left padding).
        if ceil && (n - 1) * stride[i] >= size + pad[i] {
            n -= 1;
        }
        n
    };
    Ok(PoolParams {
        planes: x[0] * x[1],
        in_h: x[2],
        in_w: x[3],
        out_h: out(x[2], 0),
        out_w: out(x[3], 1),
        kernel,
        stride,
        pad,
        max: node.op == "MaxPool",
        count_include_pad: node.int("count_include_pad", 0) != 0,
    })
}

fn pool(x: &Tensor, p: &PoolParams) -> Tensor {
    let data = x.to_f32();
    let mut out = vec![0f32; p.planes * p.out_h * p.out_w];
    for plane in 0..p.planes {
        let src = &data[plane * p.in_h * p.in_w..][..p.in_h * p.in_w];
        for oy in 0..p.out_h {
            for ox in 0..p.out_w {
                let (y0, x0) = (
                    (oy * p.stride[0]) as isize - p.pad[0] as isize,
                    (ox * p.stride[1]) as isize - p.pad[1] as isize,
                );
                let mut acc = if p.max { f32::MIN } else { 0.0 };
                let mut count = 0usize;
                let mut padded = 0usize;
                for ky in 0..p.kernel[0] as isize {
                    for kx in 0..p.kernel[1] as isize {
                        let (y, x) = (y0 + ky, x0 + kx);
                        let inside =
                            y >= 0 && x >= 0 && (y as usize) < p.in_h && (x as usize) < p.in_w;
                        if inside {
                            let v = src[y as usize * p.in_w + x as usize];
                            acc = if p.max { acc.max(v) } else { acc + v };
                            count += 1;
                        } else if y < (p.in_h + p.pad[0]) as isize
                            && x < (p.in_w + p.pad[1]) as isize
                        {
                            padded += 1;
                        }
                    }
                }
                let n = if p.count_include_pad {
                    count + padded
                } else {
                    count
                };
                out[(plane * p.out_h + oy) * p.out_w + ox] =
                    if p.max { acc } else { acc / n.max(1) as f32 };
            }
        }
    }
    Tensor::f32(vec![x.shape[0], x.shape[1], p.out_h, p.out_w], out)
}

// ---------------------------------------------------------------- reduce

/// Reducing some axes of a tensor (sum or mean).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReduceParams {
    pub shape: Vec<usize>,
    pub axes: Vec<usize>,
    pub keep_dims: bool,
    pub mean: bool,
}

impl ReduceParams {
    /// The output shape.
    pub fn out_shape(&self) -> Vec<usize> {
        (0..self.shape.len())
            .filter_map(|d| match self.axes.contains(&d) {
                true if self.keep_dims => Some(1),
                true => None,
                false => Some(self.shape[d]),
            })
            .collect()
    }
}

pub(crate) fn reduce_params(
    node: &Node,
    x: &[usize],
    inputs: &[Option<&Tensor>],
) -> Result<ReduceParams> {
    let rank = x.len();
    let axes: Vec<usize> = if node.op == "GlobalAveragePool" {
        (2..rank).collect()
    } else {
        let list = match inputs.get(1).copied().flatten() {
            Some(t) => t.to_i64(),
            None => node.ints("axes").map(<[i64]>::to_vec).unwrap_or_default(),
        };
        if list.is_empty() {
            (0..rank).collect()
        } else {
            list.iter()
                .map(|&a| normalize_axis(a, rank as i64))
                .collect::<Result<_>>()?
        }
    };
    Ok(ReduceParams {
        shape: x.to_vec(),
        axes,
        keep_dims: node.op == "GlobalAveragePool" || node.int("keepdims", 1) != 0,
        mean: node.op != "ReduceSum",
    })
}

fn reduce(x: &Tensor, p: &ReduceParams) -> Tensor {
    let data = x.to_f32();
    let in_strides = strides(&p.shape);
    let kept: Vec<usize> = (0..p.shape.len()).filter(|d| !p.axes.contains(d)).collect();
    let reduced: usize = p.axes.iter().map(|&a| p.shape[a]).product();
    let outputs: usize = kept.iter().map(|&d| p.shape[d]).product();
    let mut out = vec![0f32; outputs];
    for (o, value) in out.iter_mut().enumerate() {
        let mut base = 0;
        let mut rest = o;
        for &d in kept.iter().rev() {
            base += (rest % p.shape[d]) * in_strides[d];
            rest /= p.shape[d];
        }
        let mut sum = 0.0;
        for r in 0..reduced {
            let mut at = base;
            let mut rest = r;
            for &d in p.axes.iter().rev() {
                at += (rest % p.shape[d]) * in_strides[d];
                rest /= p.shape[d];
            }
            sum += data[at];
        }
        *value = if p.mean {
            sum / reduced.max(1) as f32
        } else {
            sum
        };
    }
    Tensor::f32(p.out_shape(), out)
}

// ------------------------------------------------- transformers and more

/// Element `i` of `shape`'s index, read through `strides`.
fn at_strides(mut i: usize, shape: &[usize], strides: &[usize]) -> usize {
    let mut at = 0;
    for d in (0..shape.len()).rev() {
        at += (i % shape[d]) * strides[d];
        i /= shape[d];
    }
    at
}

/// Matrix multiplication with NumPy's rules: the last two axes multiply,
/// the ones before broadcast; 1-D operands are a row or a column.
pub(crate) struct MatMulParams {
    pub m: usize,
    pub k: usize,
    pub n: usize,
    /// The broadcast batch shape, and each operand's strides over it (in
    /// elements; 0 where repeated).
    pub batch: Vec<usize>,
    pub a_batch_strides: Vec<usize>,
    pub b_batch_strides: Vec<usize>,
    pub out_shape: Vec<usize>,
}

pub(crate) fn matmul_params(a: &[usize], b: &[usize]) -> Result<MatMulParams> {
    let a2: Vec<usize> = if a.len() == 1 {
        vec![1, a[0]]
    } else {
        a.to_vec()
    };
    let b2: Vec<usize> = if b.len() == 1 {
        vec![b[0], 1]
    } else {
        b.to_vec()
    };
    let (m, k) = (a2[a2.len() - 2], a2[a2.len() - 1]);
    let (k2, n) = (b2[b2.len() - 2], b2[b2.len() - 1]);
    if k != k2 {
        return Err(Error::Model(format!("can't multiply {a:?} by {b:?}")));
    }
    let (ab, bb) = (&a2[..a2.len() - 2], &b2[..b2.len() - 2]);
    let batch = broadcast_shape(ab, bb)?;
    let scale = |s: Vec<usize>, by: usize| s.into_iter().map(|v| v * by).collect::<Vec<_>>();
    let a_batch_strides = scale(broadcast_strides(ab, &batch), 1)
        .iter()
        .map(|&s| s * m * k)
        .collect();
    let b_batch_strides = scale(broadcast_strides(bb, &batch), 1)
        .iter()
        .map(|&s| s * k * n)
        .collect();
    let mut out_shape = batch.clone();
    if a.len() > 1 {
        out_shape.push(m);
    }
    if b.len() > 1 {
        out_shape.push(n);
    }
    Ok(MatMulParams {
        m,
        k,
        n,
        batch,
        a_batch_strides,
        b_batch_strides,
        out_shape,
    })
}

fn matmul(a: &Tensor, b: &Tensor) -> Result<Tensor> {
    let p = matmul_params(&a.shape, &b.shape)?;
    let (x, y) = (a.to_f32(), b.to_f32());
    let batches: usize = p.batch.iter().product();
    let mut out = vec![0f32; batches * p.m * p.n];
    for bi in 0..batches {
        let (ao, bo) = (
            at_strides(bi, &p.batch, &p.a_batch_strides),
            at_strides(bi, &p.batch, &p.b_batch_strides),
        );
        for i in 0..p.m {
            for j in 0..p.n {
                let mut sum = 0.0;
                for l in 0..p.k {
                    sum += x[ao + i * p.k + l] * y[bo + l * p.n + j];
                }
                out[(bi * p.m + i) * p.n + j] = sum;
            }
        }
    }
    Ok(Tensor::f32(p.out_shape, out))
}

/// Rows of the last axis (`axis` must be the last one).
fn last_axis(node: &Node, shape: &[usize]) -> Result<(usize, usize)> {
    let axis = normalize_axis(node.int("axis", -1), shape.len() as i64)?;
    if axis + 1 != shape.len() {
        return Err(Error::Unsupported(format!(
            "{} over axis {axis} (only the last)",
            node.describe()
        )));
    }
    let d = shape[axis];
    Ok((shape.iter().product::<usize>() / d.max(1), d))
}

fn layer_norm(node: &Node, x: &Tensor, scale: &Tensor, bias: Option<&Tensor>) -> Result<Tensor> {
    let (rows, d) = last_axis(node, &x.shape)?;
    let eps = node.float("epsilon", 1e-5);
    let (data, g) = (x.to_f32(), scale.to_f32());
    let b = bias.map(Tensor::to_f32);
    let mut out = vec![0f32; data.len()];
    for r in 0..rows {
        let row = &data[r * d..][..d];
        let mean = row.iter().sum::<f32>() / d as f32;
        let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / d as f32;
        let inv = 1.0 / (var + eps).sqrt();
        for c in 0..d {
            out[r * d + c] =
                (row[c] - mean) * inv * g[c % g.len()] + b.as_ref().map_or(0.0, |b| b[c % b.len()]);
        }
    }
    Ok(Tensor::f32(x.shape.clone(), out))
}

fn softmax(node: &Node, x: &Tensor) -> Result<Tensor> {
    let (rows, d) = last_axis(node, &x.shape)?;
    let data = x.to_f32();
    let mut out = vec![0f32; data.len()];
    for r in 0..rows {
        let row = &data[r * d..][..d];
        let max = row.iter().copied().fold(f32::MIN, f32::max);
        let sum: f32 = row.iter().map(|v| (v - max).exp()).sum();
        for c in 0..d {
            out[r * d + c] = (row[c] - max).exp() / sum;
        }
    }
    Ok(Tensor::f32(x.shape.clone(), out))
}

/// GatherND: for each index tuple (the last axis of `indices`, after the
/// batch axes), the slice of `data` it points at.
pub(crate) struct GatherNdParams {
    pub batch_dims: usize,
    /// Index tuples per batch and their length.
    pub tuples: usize,
    pub depth: usize,
    /// Elements per gathered slice.
    pub slice: usize,
    /// Elements of `data` per batch, and the strides of its indexed axes.
    pub data_batch: usize,
    pub index_strides: Vec<usize>,
    pub index_dims: Vec<usize>,
    pub out_shape: Vec<usize>,
}

pub(crate) fn gather_nd_params(
    node: &Node,
    data: &[usize],
    indices: &[usize],
) -> Result<GatherNdParams> {
    let b = node.int("batch_dims", 0).max(0) as usize;
    let depth = *indices
        .last()
        .ok_or_else(|| Error::Model("GatherND without indices".into()))?;
    if indices.len() < b + 1 || data.len() < b + depth || data[..b] != indices[..b] {
        return Err(Error::Model(format!(
            "{}: {data:?} by {indices:?}",
            node.describe()
        )));
    }
    let own = strides(data);
    let mut out_shape = indices[..indices.len() - 1].to_vec();
    out_shape.extend(&data[b + depth..]);
    Ok(GatherNdParams {
        batch_dims: b,
        tuples: indices[b..indices.len() - 1].iter().product(),
        depth,
        slice: data[b + depth..].iter().product(),
        data_batch: data[b..].iter().product(),
        index_strides: own[b..b + depth].to_vec(),
        index_dims: data[b..b + depth].to_vec(),
        out_shape,
    })
}

fn gather_nd(node: &Node, data: &Tensor, indices: &Tensor) -> Result<Tensor> {
    let p = gather_nd_params(node, &data.shape, &indices.shape)?;
    let (d, idx) = (data.to_f32(), indices.to_i64());
    let batches: usize = data.shape[..p.batch_dims].iter().product();
    let mut out = Vec::with_capacity(batches * p.tuples * p.slice);
    for bi in 0..batches {
        for t in 0..p.tuples {
            let tuple = &idx[(bi * p.tuples + t) * p.depth..][..p.depth];
            let mut at = bi * p.data_batch;
            for (k, &i) in tuple.iter().enumerate() {
                let dim = p.index_dims[k] as i64;
                at += (if i < 0 { i + dim } else { i }).clamp(0, dim - 1) as usize
                    * p.index_strides[k];
            }
            out.extend_from_slice(&d[at..at + p.slice]);
        }
    }
    Ok(Tensor::f32(p.out_shape, out))
}

/// Constant padding: the padded shape and where the input goes in it.
pub(crate) struct PadParams {
    pub out_shape: Vec<usize>,
    /// Padding before each axis (negative crops).
    pub before: Vec<i64>,
    pub value: f32,
}

pub(crate) fn pad_params(
    node: &Node,
    shape: &[usize],
    inputs: &[Option<&Tensor>],
) -> Result<PadParams> {
    if node.string("mode", "constant") != "constant" {
        return Err(Error::Unsupported(format!(
            "{} other than constant",
            node.describe()
        )));
    }
    let rank = shape.len();
    let pads = match inputs.get(1).copied().flatten() {
        Some(t) => t.to_i64(),
        None => node.ints("pads").map(<[i64]>::to_vec).unwrap_or_default(),
    };
    let value = inputs
        .get(2)
        .copied()
        .flatten()
        .and_then(|t| t.to_f32().first().copied())
        .unwrap_or(node.float("value", 0.0));
    let axes: Vec<usize> = match inputs.get(3).copied().flatten() {
        Some(t) => t
            .to_i64()
            .iter()
            .map(|&a| normalize_axis(a, rank as i64))
            .collect::<Result<_>>()?,
        None => (0..rank).collect(),
    };
    let n = axes.len();
    let mut before = vec![0i64; rank];
    let mut out_shape = shape.to_vec();
    for (k, &a) in axes.iter().enumerate() {
        let (b, e) = (pads[k], pads[n + k]);
        before[a] = b;
        out_shape[a] = (shape[a] as i64 + b + e).max(0) as usize;
    }
    Ok(PadParams {
        out_shape,
        before,
        value,
    })
}

fn pad(x: &Tensor, p: &PadParams) -> Tensor {
    let data = x.to_f32();
    let n: usize = p.out_shape.iter().product();
    let in_strides = strides(&x.shape);
    let mut out = vec![p.value; n];
    for (o, v) in out.iter_mut().enumerate() {
        let mut rest = o;
        let mut at = 0usize;
        let mut inside = true;
        for d in (0..p.out_shape.len()).rev() {
            let i = (rest % p.out_shape[d]) as i64 - p.before[d];
            rest /= p.out_shape[d];
            if i < 0 || i >= x.shape[d] as i64 {
                inside = false;
                break;
            }
            at += i as usize * in_strides[d];
        }
        if inside {
            *v = data[at];
        }
    }
    Tensor::f32(p.out_shape.clone(), out)
}

/// BatchNormalization at inference as a scale and a shift per channel
/// (axis 1), shaped to broadcast over a rank-`rank` input.
pub(crate) fn batch_norm_params(
    node: &Node,
    rank: usize,
    inputs: &[Option<&Tensor>],
) -> Result<(Tensor, Tensor)> {
    let get = |i: usize| -> Result<Vec<f32>> {
        inputs
            .get(i)
            .copied()
            .flatten()
            .map(Tensor::to_f32)
            .ok_or_else(|| {
                Error::Unsupported(format!("{} with computed statistics", node.describe()))
            })
    };
    let (gamma, beta, mean, var) = (get(1)?, get(2)?, get(3)?, get(4)?);
    let eps = node.float("epsilon", 1e-5);
    let scale: Vec<f32> = gamma
        .iter()
        .zip(&var)
        .map(|(g, v)| g / (v + eps).sqrt())
        .collect();
    let shift: Vec<f32> = beta
        .iter()
        .zip(&mean)
        .zip(&scale)
        .map(|((b, m), s)| b - m * s)
        .collect();
    let mut shape = vec![gamma.len()];
    shape.extend(std::iter::repeat_n(1, rank.saturating_sub(2)));
    Ok((Tensor::f32(shape.clone(), scale), Tensor::f32(shape, shift)))
}

/// Elementwise over broadcast operands (`values`, each tensor's in some
/// type).
fn broadcast_map<T: Copy, U>(
    tensors: &[&Tensor],
    values: &[&[T]],
    f: impl Fn(&[T]) -> U,
) -> Result<(Vec<usize>, Vec<U>)> {
    let mut shape = tensors[0].shape.clone();
    for t in &tensors[1..] {
        shape = broadcast_shape(&shape, &t.shape)?;
    }
    let st: Vec<Vec<usize>> = tensors
        .iter()
        .map(|t| broadcast_strides(&t.shape, &shape))
        .collect();
    let n: usize = shape.iter().product();
    let mut out = Vec::with_capacity(n);
    if n == 0 {
        return Ok((shape, out));
    }
    // Row by row along the innermost dimension, each operand's offset
    // stepped along with the outer ones (no divisions per element:
    // BiRefNet's attention masks took half a second so).
    let (inner, outer) = match shape.split_last() {
        Some((&inner, outer)) => (inner, outer),
        None => (1, &[][..]),
    };
    let inner_st: Vec<usize> = st.iter().map(|s| s.last().copied().unwrap_or(0)).collect();
    let mut index = vec![0; outer.len()];
    let mut offsets = vec![0; tensors.len()];
    let mut args = vec![values[0][0]; tensors.len()];
    for _ in 0..n / inner {
        for j in 0..inner {
            for k in 0..values.len() {
                args[k] = values[k][offsets[k] + j * inner_st[k]];
            }
            out.push(f(&args));
        }
        for d in (0..outer.len()).rev() {
            index[d] += 1;
            for (at, st) in offsets.iter_mut().zip(&st) {
                *at += st[d];
            }
            if index[d] < outer[d] {
                break;
            }
            for (at, st) in offsets.iter_mut().zip(&st) {
                *at -= st[d] * outer[d];
            }
            index[d] = 0;
        }
    }
    Ok((shape, out))
}

fn compare(node: &Node, a: &Tensor, b: &Tensor) -> Result<Tensor> {
    let op = node.op.as_str();
    if op == "Mod" && node.int("fmod", 0) != 0 || op == "Mod" && matches!(a.data, Data::F32(_)) {
        let (shape, v) = broadcast_map(&[a, b], &[&a.f32_values(), &b.f32_values()], |x| {
            x[0] % x[1]
        })?;
        return Ok(Tensor::f32(shape, v));
    }
    if op == "Mod" {
        let (shape, v) = broadcast_map(&[a, b], &[&a.i64_values(), &b.i64_values()], |x| {
            if x[1] == 0 {
                0
            } else {
                ((x[0] % x[1]) + x[1]) % x[1]
            }
        })?;
        return Ok(Tensor::i64(shape, v));
    }
    let (shape, v) = broadcast_map(&[a, b], &[&a.f32_values(), &b.f32_values()], |x| {
        let r = match op {
            "Equal" => x[0] == x[1],
            "Less" => x[0] < x[1],
            "Greater" => x[0] > x[1],
            "And" => x[0] != 0.0 && x[1] != 0.0,
            _ => x[0] != 0.0 || x[1] != 0.0,
        };
        i64::from(r)
    })?;
    Ok(Tensor::i64(shape, v))
}

fn where_(cond: &Tensor, x: &Tensor, y: &Tensor) -> Result<Tensor> {
    if let (Data::I64(xi), Data::I64(yi)) = (&x.data, &y.data) {
        let (shape, v) = broadcast_map(&[cond, x, y], &[&cond.i64_values(), xi, yi], |a| {
            if a[0] != 0 { a[1] } else { a[2] }
        })?;
        return Ok(Tensor::i64(shape, v));
    }
    let (c, x_values, y_values) = (cond.f32_values(), x.f32_values(), y.f32_values());
    let (shape, v) = broadcast_map(&[cond, x, y], &[&c, &x_values, &y_values], |a| {
        if a[0] != 0.0 { a[1] } else { a[2] }
    })?;
    Ok(Tensor::f32(shape, v))
}

fn scatter_nd(data: &Tensor, indices: &Tensor, updates: &Tensor) -> Result<Tensor> {
    let depth = *indices.shape.last().unwrap_or(&0);
    let own = strides(&data.shape);
    let slice: usize = data.shape[depth..].iter().product();
    let idx = indices.to_i64();
    let tuples = idx.len() / depth.max(1);
    let mut out = data.clone();
    // The updates in the data's type, once.
    let (updates_i64, updates_f32) = match out.data {
        Data::I64(_) => (updates.to_i64(), Vec::new()),
        Data::F32(_) => (Vec::new(), updates.to_f32()),
    };
    for t in 0..tuples {
        let mut at = 0;
        for k in 0..depth {
            let dim = data.shape[k] as i64;
            let i = idx[t * depth + k];
            at += (if i < 0 { i + dim } else { i }) as usize * own[k];
        }
        match &mut out.data {
            Data::I64(o) => {
                Arc::make_mut(o)[at..at + slice].copy_from_slice(&updates_i64[t * slice..][..slice])
            }
            Data::F32(o) => {
                Arc::make_mut(o)[at..at + slice].copy_from_slice(&updates_f32[t * slice..][..slice])
            }
        }
    }
    Ok(out)
}
