//! Running a model on the GPU.
//!
//! `Session::plan` works out the whole computation for given input shapes
//! (and inputs held fixed, such as RVM's downsample ratio):
//! - Values that only depend on constants (shape arithmetic, mostly) are
//!   computed on the CPU right away (`cpu`).
//! - The rest become kernel dispatches over GPU buffers. Reshapes are views
//!   of their input's buffer. Intermediate values share buffers: once
//!   the last step reading a value has run, its buffer holds a later one
//!   (`assign`), which cuts RVM at 1080p from 732 MB to a fraction.
//! - Weights are uploaded once.
//!
//! The dispatches are recorded once; `run` submits them again for every
//! frame. Outputs can be fed back into inputs after each run (recurrent
//! state).

use nox_gpu::GpuDevice;
use nox_gpu::compute::{Binding, Buffer, BufferSlice, MemoryUse, Recording};
use std::collections::HashMap;
use std::sync::Arc;

use crate::cpu::{self, BinaryOp, Strided};
use crate::kernels::{
    BinaryPush, ConvPush, CopyPush, KernelKind, Kernels, PoolPush, ReducePush, ResizePush,
    UnaryPush, groups,
};
use crate::model::{Model, Node};
use crate::tensor::{Tensor, strides};
use crate::{Error, Result};

/// A value on the GPU: `shape` floats from element `offset` of a buffer.
#[derive(Debug, Clone)]
struct GpuValue {
    buffer: usize,
    offset: usize,
    shape: Vec<usize>,
}

impl GpuValue {
    fn len(&self) -> usize {
        self.shape.iter().product()
    }
}

#[derive(Debug, Clone)]
enum Value {
    Host(Tensor),
    Gpu(GpuValue),
}

impl Value {
    fn shape(&self) -> &[usize] {
        match self {
            Value::Host(t) => &t.shape,
            Value::Gpu(g) => &g.shape,
        }
    }
}

/// One kernel dispatch over buffers (by index), with its push constants.
struct Step {
    kind: KernelKind,
    buffers: Vec<usize>,
    push: Vec<u8>,
    n: u32,
}

struct Plan {
    shapes: Vec<(String, Vec<usize>)>,
    values: HashMap<String, Value>,
    // Dropped before the buffers it records over.
    recording: Recording,
    buffers: Vec<Buffer>,
}

/// A model ready to run on a GPU.
pub struct Session {
    gpu: Arc<GpuDevice>,
    model: Model,
    // Dropped before the kernels its recording uses.
    plan: Option<Plan>,
    kernels: Kernels,
}

impl Session {
    pub fn new(gpu: Arc<GpuDevice>, model: Model) -> Self {
        Self {
            gpu,
            model,
            plan: None,
            kernels: Kernels::default(),
        }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// GPU memory the current plan takes, in bytes.
    pub fn memory(&self) -> u64 {
        self.plan
            .as_ref()
            .map_or(0, |p| p.buffers.iter().map(Buffer::size).sum())
    }

    /// The input shapes planned for, if any.
    pub fn planned_shapes(&self) -> Option<&[(String, Vec<usize>)]> {
        self.plan.as_ref().map(|p| p.shapes.as_slice())
    }

    /// Plans runs with inputs of `shapes`, inputs in `constants` held
    /// fixed, and after each run each output in `feedback` copied into its
    /// input (output, input), as recurrent models need.
    pub fn plan(
        &mut self,
        shapes: &[(&str, &[usize])],
        constants: &[(&str, Tensor)],
        feedback: &[(&str, &str)],
    ) -> Result<()> {
        // The old plan's buffers go first.
        self.plan = None;
        let mut planner = Planner {
            sizes: Vec::new(),
            pinned: Vec::new(),
            values: HashMap::new(),
            uploads: Vec::new(),
            uploaded: HashMap::new(),
            steps: Vec::new(),
        };
        for (name, tensor) in constants {
            planner
                .values
                .insert((*name).to_owned(), Value::Host(tensor.clone()));
        }
        for (name, shape) in shapes {
            let value = planner.alloc(shape.to_vec());
            planner.pinned[value.buffer] = true;
            planner.values.insert((*name).to_owned(), Value::Gpu(value));
        }
        for input in &self.model.inputs {
            if !planner.values.contains_key(&input.name) {
                return Err(Error::Model(format!(
                    "input {} needs a shape or a value",
                    input.name
                )));
            }
        }
        for node in &self.model.nodes {
            planner
                .plan_node(node, &self.model.constants)
                .map_err(|err| match err {
                    Error::Unsupported(what) => {
                        Error::Unsupported(format!("{what} ({})", node.describe()))
                    }
                    Error::Model(what) => Error::Model(format!("{what} ({})", node.describe())),
                    other => other,
                })?;
        }
        let mut copies = Vec::new();
        for (output, input) in feedback {
            let (Some(Value::Gpu(from)), Some(Value::Gpu(to))) =
                (planner.values.get(*output), planner.values.get(*input))
            else {
                return Err(Error::Model(format!(
                    "can't feed {output} back into {input}: both must be on the GPU"
                )));
            };
            if from.shape != to.shape {
                return Err(Error::Model(format!(
                    "can't feed {output} {:?} back into {input} {:?}",
                    from.shape, to.shape
                )));
            }
            copies.push((from.clone(), to.clone()));
        }

        // What can be read afterwards: the inputs and outputs, in buffers
        // of their own.
        let names: Vec<&str> = shapes
            .iter()
            .map(|(n, _)| *n)
            .chain(self.model.outputs.iter().map(String::as_str))
            .collect();
        let mut values: HashMap<String, Value> = names
            .iter()
            .filter_map(|n| Some(((*n).to_owned(), planner.values.get(*n)?.clone())))
            .collect();
        for value in values.values() {
            if let Value::Gpu(v) = value {
                planner.pinned[v.buffer] = true;
            }
        }
        for (from, to) in &copies {
            planner.pinned[from.buffer] = true;
            planner.pinned[to.buffer] = true;
        }
        let Planner {
            sizes,
            pinned,
            uploads,
            mut steps,
            ..
        } = planner;
        let (physical, physical_sizes) = assign(&sizes, &pinned, &steps);
        let buffers: Vec<Buffer> = physical_sizes
            .iter()
            .map(|&n| Buffer::new(&self.gpu, (n * 4) as u64, MemoryUse::Device))
            .collect::<std::result::Result<_, _>>()?;
        for step in &mut steps {
            for b in &mut step.buffers {
                *b = physical[*b];
            }
        }
        for value in values.values_mut() {
            if let Value::Gpu(v) = value {
                v.buffer = physical[v.buffer];
            }
        }
        let uploads: Vec<(usize, Vec<f32>)> =
            uploads.into_iter().map(|(b, d)| (physical[b], d)).collect();
        let copies: Vec<(GpuValue, GpuValue)> = copies
            .into_iter()
            .map(|(from, to)| {
                (
                    GpuValue {
                        buffer: physical[from.buffer],
                        ..from
                    },
                    GpuValue {
                        buffer: physical[to.buffer],
                        ..to
                    },
                )
            })
            .collect();
        upload(&self.gpu, &buffers, &uploads)?;
        let max_buffers = steps.iter().map(|s| s.kind.buffers()).max().unwrap_or(1);
        let mut recording = Recording::new(&self.gpu, steps.len() as u32, max_buffers)?;
        for step in &steps {
            let kernel = self.kernels.get(&self.gpu, step.kind)?;
            let slices: Vec<Binding<'_>> =
                step.buffers.iter().map(|&b| (&buffers[b]).into()).collect();
            recording.dispatch(kernel, &slices, &step.push, groups(step.n))?;
        }
        for (from, to) in &copies {
            recording.copy(
                slice(&buffers[from.buffer], from.offset),
                slice(&buffers[to.buffer], to.offset),
                (from.len() * 4) as u64,
            );
        }
        tracing::debug!(
            "planned {} dispatches over {} buffers for {} values ({:.1} MB)",
            steps.len(),
            buffers.len(),
            sizes.len(),
            buffers.iter().map(Buffer::size).sum::<u64>() as f64 / 1e6
        );
        self.plan = Some(Plan {
            shapes: shapes
                .iter()
                .map(|(n, s)| ((*n).to_owned(), s.to_vec()))
                .collect(),
            values,
            recording,
            buffers,
        });
        Ok(())
    }

    fn plan_ref(&self) -> Result<&Plan> {
        self.plan
            .as_ref()
            .ok_or_else(|| Error::Model("not planned yet".into()))
    }

    fn gpu_value(&self, name: &str) -> Result<(&Plan, &GpuValue)> {
        let plan = self.plan_ref()?;
        match plan.values.get(name) {
            Some(Value::Gpu(value)) => Ok((plan, value)),
            Some(Value::Host(_)) => Err(Error::Model(format!("{name} is computed on the CPU"))),
            None => Err(Error::Model(format!("no value {name}"))),
        }
    }

    /// The buffer holding input or output `name` (from element
    /// `buffer_offset(name)` on), for writing inputs and reading outputs
    /// on the GPU.
    pub fn buffer(&self, name: &str) -> Result<&Buffer> {
        let (plan, value) = self.gpu_value(name)?;
        Ok(&plan.buffers[value.buffer])
    }

    /// Where `name` starts in its buffer, in floats.
    pub fn buffer_offset(&self, name: &str) -> Result<usize> {
        Ok(self.gpu_value(name)?.1.offset)
    }

    /// The shape of value `name` in the current plan.
    pub fn shape(&self, name: &str) -> Option<Vec<usize>> {
        self.plan
            .as_ref()?
            .values
            .get(name)
            .map(|v| v.shape().to_vec())
    }

    /// Copies `data` into input `name` (through a staging buffer).
    pub fn write(&mut self, name: &str, data: &[f32]) -> Result<()> {
        let (plan, value) = self.gpu_value(name)?;
        if data.len() != value.len() {
            return Err(Error::Model(format!(
                "{name} takes {} values, not {}",
                value.len(),
                data.len()
            )));
        }
        let mut staging = Buffer::new(&self.gpu, (data.len() * 4) as u64, MemoryUse::Upload)?;
        staging.write(data)?;
        let mut copy = Recording::new(&self.gpu, 0, 0)?;
        copy.copy(
            (&staging).into(),
            slice(&plan.buffers[value.buffer], value.offset),
            (data.len() * 4) as u64,
        );
        copy.run()?;
        Ok(())
    }

    /// The values of `name` (an output, usually), read back from the GPU.
    pub fn read(&self, name: &str) -> Result<Vec<f32>> {
        let plan = self.plan_ref()?;
        let value = match plan.values.get(name) {
            Some(Value::Host(t)) => return Ok(t.to_f32()),
            Some(Value::Gpu(value)) => value,
            None => return Err(Error::Model(format!("no value {name}"))),
        };
        let bytes = (value.len() * 4) as u64;
        let staging = Buffer::new(&self.gpu, bytes, MemoryUse::Download)?;
        let mut copy = Recording::new(&self.gpu, 0, 0)?;
        copy.copy(
            slice(&plan.buffers[value.buffer], value.offset),
            (&staging).into(),
            bytes,
        );
        copy.run()?;
        let mut data = staging.read::<f32>()?;
        data.truncate(value.len());
        Ok(data)
    }

    /// Runs the planned computation and waits for it.
    pub fn run(&mut self) -> Result<()> {
        let plan = self
            .plan
            .as_mut()
            .ok_or_else(|| Error::Model("not planned yet".into()))?;
        plan.recording.run()?;
        Ok(())
    }
}

fn slice(buffer: &Buffer, offset: usize) -> BufferSlice<'_> {
    BufferSlice {
        buffer,
        offset: (offset * 4) as u64,
        range: nox_gpu::vk::WHOLE_SIZE,
    }
}

/// Copies the planned constants into their buffers.
fn upload(gpu: &Arc<GpuDevice>, buffers: &[Buffer], uploads: &[(usize, Vec<f32>)]) -> Result<()> {
    if uploads.is_empty() {
        return Ok(());
    }
    let total: usize = uploads.iter().map(|(_, d)| d.len()).sum();
    let mut staging = Buffer::new(gpu, (total * 4) as u64, MemoryUse::Upload)?;
    let all: Vec<f32> = uploads
        .iter()
        .flat_map(|(_, d)| d.iter().copied())
        .collect();
    staging.write(&all)?;
    let mut copy = Recording::new(gpu, 0, 0)?;
    let mut at = 0u64;
    for (buffer, data) in uploads {
        let bytes = (data.len() * 4) as u64;
        copy.copy(
            BufferSlice {
                buffer: &staging,
                offset: at,
                range: bytes,
            },
            (&buffers[*buffer]).into(),
            bytes,
        );
        at += bytes;
    }
    copy.run()?;
    Ok(())
}

/// Physical buffers for the virtual ones a plan uses (by index), and their
/// sizes in floats. Each value lives from the first step writing it (its
/// output is the last binding) to the last step reading it; afterwards
/// its buffer is free for later values, the smallest big enough first.
/// Pinned values (inputs, outputs, weights) keep buffers of their own.
fn assign(sizes: &[usize], pinned: &[bool], steps: &[Step]) -> (Vec<usize>, Vec<usize>) {
    const NONE: usize = usize::MAX;
    let mut first = vec![NONE; sizes.len()];
    let mut last = vec![0; sizes.len()];
    for (i, step) in steps.iter().enumerate() {
        for &b in &step.buffers {
            last[b] = i;
        }
        let output = *step.buffers.last().expect("output");
        first[output] = first[output].min(i);
    }
    let mut physical = vec![NONE; sizes.len()];
    let mut physical_sizes: Vec<usize> = Vec::new();
    let mut free: Vec<usize> = Vec::new();
    let own = |physical_sizes: &mut Vec<usize>, n: usize| {
        physical_sizes.push(n.max(1));
        physical_sizes.len() - 1
    };
    for b in 0..sizes.len() {
        // Pinned, or never written by a step (e.g. an input nothing reads).
        if pinned[b] || first[b] == NONE {
            physical[b] = own(&mut physical_sizes, sizes[b]);
        }
    }
    for (i, step) in steps.iter().enumerate() {
        let output = *step.buffers.last().expect("output");
        if physical[output] == NONE {
            let fit = free
                .iter()
                .enumerate()
                .filter(|&(_, &p)| physical_sizes[p] >= sizes[output])
                .min_by_key(|&(_, &p)| physical_sizes[p])
                .map(|(at, _)| at);
            physical[output] = match fit {
                Some(at) => free.swap_remove(at),
                None => own(&mut physical_sizes, sizes[output]),
            };
        }
        for &b in &step.buffers {
            if last[b] == i && !pinned[b] && first[b] != NONE && !free.contains(&physical[b]) {
                free.push(physical[b]);
            }
        }
    }
    (physical, physical_sizes)
}

struct Planner {
    /// Virtual buffers: sizes in floats, and whether they're pinned.
    sizes: Vec<usize>,
    pinned: Vec<bool>,
    values: HashMap<String, Value>,
    /// Constants to copy into buffers: (buffer, data).
    uploads: Vec<(usize, Vec<f32>)>,
    /// Constants already on the GPU, by name.
    uploaded: HashMap<String, GpuValue>,
    steps: Vec<Step>,
}

/// Up to 4 dimensions, padded in front with size 1 (stride 0).
fn pad4<T: Copy + Default>(shape: &[usize], strides: &[T]) -> Result<([u32; 4], [T; 4])> {
    if shape.len() > 4 {
        return Err(Error::Unsupported(format!("rank-{} tensors", shape.len())));
    }
    let mut s = [1u32; 4];
    let mut st = [T::default(); 4];
    let pad = 4 - shape.len();
    for i in 0..shape.len() {
        s[pad + i] = shape[i] as u32;
        st[pad + i] = strides[i];
    }
    Ok((s, st))
}

impl Planner {
    /// A new (virtual) buffer for a value of `shape`.
    fn alloc(&mut self, shape: Vec<usize>) -> GpuValue {
        self.sizes.push(shape.iter().product());
        self.pinned.push(false);
        GpuValue {
            buffer: self.sizes.len() - 1,
            offset: 0,
            shape,
        }
    }

    /// Input `i` of `node` on the GPU (constants are uploaded, once).
    fn gpu_arg(&mut self, node: &Node, args: &[Option<Value>], i: usize) -> Result<GpuValue> {
        match args.get(i).cloned().flatten() {
            Some(Value::Gpu(value)) => Ok(value),
            Some(Value::Host(tensor)) => {
                let name = &node.inputs[i];
                if let Some(value) = self.uploaded.get(name) {
                    return Ok(value.clone());
                }
                let value = self.alloc(tensor.shape.clone());
                self.pinned[value.buffer] = true;
                self.uploads.push((value.buffer, tensor.to_f32()));
                self.uploaded.insert(name.clone(), value.clone());
                Ok(value)
            }
            None => Err(Error::Model(format!("missing input {i}"))),
        }
    }

    fn step(&mut self, kind: KernelKind, buffers: Vec<usize>, push: &impl bytemuck::Pod, n: usize) {
        self.steps.push(Step {
            kind,
            buffers,
            push: bytemuck::bytes_of(push).to_vec(),
            n: n as u32,
        });
    }

    /// Copies the view `view` (offsets relative to `src`'s) into `dst`
    /// at `dst_offset` with `dst_strides`.
    fn copy(
        &mut self,
        src: &GpuValue,
        view: &Strided,
        dst: &GpuValue,
        dst_offset: usize,
        dst_strides: &[usize],
    ) -> Result<()> {
        let n: usize = view.shape.iter().product();
        if n == 0 {
            return Ok(());
        }
        let (shape, src_strides) = pad4(&view.shape, &view.strides)?;
        let dst_strides: Vec<isize> = dst_strides.iter().map(|&s| s as isize).collect();
        let (_, dst_strides) = pad4(&view.shape, &dst_strides)?;
        let push = CopyPush {
            n: n as u32,
            src_offset: (src.offset + view.offset) as i32,
            dst_offset: (dst.offset + dst_offset) as i32,
            pad0: 0,
            shape,
            src_strides: src_strides.map(|s| s as i32),
            dst_strides: dst_strides.map(|s| s as i32),
        };
        self.step(KernelKind::Copy, vec![src.buffer, dst.buffer], &push, n);
        Ok(())
    }

    fn plan_node(&mut self, node: &Node, constants: &HashMap<String, Tensor>) -> Result<()> {
        let args: Vec<Option<Value>> = node
            .inputs
            .iter()
            .map(|name| {
                if name.is_empty() {
                    return Ok(None);
                }
                self.values
                    .get(name)
                    .cloned()
                    .or_else(|| constants.get(name).cloned().map(Value::Host))
                    .map(Some)
                    .ok_or_else(|| Error::Model(format!("nothing computes {name}")))
            })
            .collect::<Result<_>>()?;
        // The constant inputs, for working out parameters.
        let host: Vec<Option<&Tensor>> = args
            .iter()
            .map(|a| match a {
                Some(Value::Host(t)) => Some(t),
                _ => None,
            })
            .collect();
        let on_gpu = args.iter().any(|a| matches!(a, Some(Value::Gpu(_))));
        let out = |values: &mut HashMap<String, Value>, i: usize, value: Value| {
            if let Some(name) = node.outputs.get(i).filter(|n| !n.is_empty()) {
                values.insert(name.clone(), value);
            }
        };
        if !on_gpu {
            for (i, tensor) in cpu::run(node, &host)?.into_iter().enumerate() {
                out(&mut self.values, i, Value::Host(tensor));
            }
            return Ok(());
        }
        let shape = |i: usize| -> Result<Vec<usize>> {
            args.get(i)
                .cloned()
                .flatten()
                .map(|a| a.shape().to_vec())
                .ok_or_else(|| Error::Model(format!("missing input {i}")))
        };
        match node.op.as_str() {
            "Shape" => {
                let s = shape(0)?;
                let value = Tensor::i64(vec![s.len()], s.iter().map(|&d| d as i64).collect());
                out(&mut self.values, 0, Value::Host(value));
            }
            // Same values, other shape: a view of the input.
            "Reshape" | "Flatten" | "Squeeze" | "Unsqueeze" | "Identity" | "Dropout" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let new_shape = if matches!(node.op.as_str(), "Identity" | "Dropout") {
                    x.shape.clone()
                } else {
                    cpu::reshape_params(node, &x.shape, &host)?
                };
                out(
                    &mut self.values,
                    0,
                    Value::Gpu(GpuValue {
                        shape: new_shape,
                        ..x
                    }),
                );
            }
            "Cast" if matches!(node.int("to", 1), 1 | 10 | 11) => {
                let x = self.gpu_arg(node, &args, 0)?;
                out(&mut self.values, 0, Value::Gpu(x));
            }
            "Add" | "Sub" | "Mul" | "Div" | "Pow" => {
                let a = self.gpu_arg(node, &args, 0)?;
                let b = self.gpu_arg(node, &args, 1)?;
                let shape = cpu::broadcast_shape(&a.shape, &b.shape)?;
                let (dims, sa) = pad4(&shape, &cpu::broadcast_strides(&a.shape, &shape))?;
                let (_, sb) = pad4(&shape, &cpu::broadcast_strides(&b.shape, &shape))?;
                let o = self.alloc(shape);
                let push = BinaryPush {
                    n: o.len() as u32,
                    op: BinaryOp::of(&node.op).expect("binary") as u32,
                    a_offset: a.offset as u32,
                    b_offset: b.offset as u32,
                    shape: dims,
                    a_strides: sa.map(|s| s as u32),
                    b_strides: sb.map(|s| s as u32),
                };
                self.step(
                    KernelKind::Binary,
                    vec![a.buffer, b.buffer, o.buffer],
                    &push,
                    o.len(),
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Relu" | "Sigmoid" | "Tanh" | "Exp" | "Sqrt" | "Neg" | "Abs" | "Erf"
            | "HardSigmoid" | "HardSwish" | "Clip" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let (op, alpha, beta) = cpu::unary_params(node, &host)?;
                let o = self.alloc(x.shape.clone());
                let push = UnaryPush {
                    n: o.len() as u32,
                    op: op as u32,
                    x_offset: x.offset as u32,
                    alpha,
                    beta,
                };
                self.step(KernelKind::Unary, vec![x.buffer, o.buffer], &push, o.len());
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Transpose" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let perm = cpu::transpose_perm(node, x.shape.len());
                let own = strides(&x.shape);
                let view = Strided {
                    shape: perm.iter().map(|&p| x.shape[p]).collect(),
                    strides: perm.iter().map(|&p| own[p] as isize).collect(),
                    offset: 0,
                };
                let o = self.alloc(view.shape.clone());
                self.copy(&x, &view, &o, 0, &strides(&o.shape))?;
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Slice" | "Expand" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let view = if node.op == "Slice" {
                    cpu::slice_params(node, &x.shape, &host)?
                } else {
                    let target =
                        host.get(1).copied().flatten().ok_or_else(|| {
                            Error::Unsupported("Expand to a computed shape".into())
                        })?;
                    cpu::expand_params(&x.shape, &target.to_i64())?
                };
                let o = self.alloc(view.shape.clone());
                self.copy(&x, &view, &o, 0, &strides(&o.shape))?;
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Concat" => {
                let parts: Vec<GpuValue> = (0..args.len())
                    .filter(|&i| args[i].is_some())
                    .map(|i| self.gpu_arg(node, &args, i))
                    .collect::<Result<_>>()?;
                let shapes: Vec<Vec<usize>> = parts.iter().map(|p| p.shape.clone()).collect();
                let (axis, shape) = cpu::concat_params(node, &shapes)?;
                let o = self.alloc(shape);
                let out_strides = strides(&o.shape);
                let mut at = 0;
                for part in &parts {
                    let view = Strided {
                        shape: part.shape.clone(),
                        strides: strides(&part.shape).iter().map(|&s| s as isize).collect(),
                        offset: 0,
                    };
                    self.copy(part, &view, &o, at * out_strides[axis], &out_strides)?;
                    at += part.shape[axis];
                }
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Split" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let (axis, sizes) = cpu::split_params(node, &x.shape, &host, node.outputs.len())?;
                let own = strides(&x.shape);
                let mut at = 0;
                for (i, size) in sizes.into_iter().enumerate() {
                    let mut part = x.shape.clone();
                    part[axis] = size;
                    let view = Strided {
                        shape: part.clone(),
                        strides: own.iter().map(|&s| s as isize).collect(),
                        offset: at * own[axis],
                    };
                    let o = self.alloc(part);
                    self.copy(&x, &view, &o, 0, &strides(&o.shape))?;
                    out(&mut self.values, i, Value::Gpu(o));
                    at += size;
                }
            }
            "Conv" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let w = self.gpu_arg(node, &args, 1)?;
                let bias = match args.get(2).cloned().flatten() {
                    Some(_) => Some(self.gpu_arg(node, &args, 2)?),
                    None => None,
                };
                let p = cpu::conv_params(node, &x.shape, &w.shape)?;
                if x.offset != 0 || w.offset != 0 {
                    return Err(Error::Unsupported("Conv of a view".into()));
                }
                let o = self.alloc(vec![p.batch, p.out_channels, p.out_h, p.out_w]);
                let push = ConvPush {
                    n: o.len() as u32,
                    in_channels: p.in_channels as u32,
                    in_h: p.in_h as u32,
                    in_w: p.in_w as u32,
                    out_channels: p.out_channels as u32,
                    out_h: p.out_h as u32,
                    out_w: p.out_w as u32,
                    kernel_h: p.kernel_h as u32,
                    kernel_w: p.kernel_w as u32,
                    stride_h: p.stride[0] as u32,
                    stride_w: p.stride[1] as u32,
                    pad_h: p.pad[0] as u32,
                    pad_w: p.pad[1] as u32,
                    dilation_h: p.dilation[0] as u32,
                    dilation_w: p.dilation[1] as u32,
                    group: p.group as u32,
                    has_bias: u32::from(bias.is_some()),
                };
                // Without a bias, any buffer will do for the binding.
                let bias = bias.map_or(w.buffer, |b| b.buffer);
                self.step(
                    KernelKind::Conv,
                    vec![x.buffer, w.buffer, bias, o.buffer],
                    &push,
                    o.len(),
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Resize" | "Upsample" => {
                let x = self.gpu_arg(node, &args, 0)?;
                if args
                    .iter()
                    .skip(1)
                    .any(|a| matches!(a, Some(Value::Gpu(_))))
                {
                    return Err(Error::Unsupported("Resize to computed sizes".into()));
                }
                let p = cpu::resize_params(node, &x.shape, &host)?;
                let o = self.alloc(vec![x.shape[0], x.shape[1], p.out_h, p.out_w]);
                let push = ResizePush {
                    n: o.len() as u32,
                    in_h: p.in_h as u32,
                    in_w: p.in_w as u32,
                    out_h: p.out_h as u32,
                    out_w: p.out_w as u32,
                    scale_h: p.scale_h,
                    scale_w: p.scale_w,
                    linear: u32::from(p.linear),
                    coordinates: p.coordinates as u32,
                    nearest: p.nearest as u32,
                };
                if x.offset != 0 {
                    return Err(Error::Unsupported("Resize of a view".into()));
                }
                self.step(KernelKind::Resize, vec![x.buffer, o.buffer], &push, o.len());
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "AveragePool" | "MaxPool" => {
                let x = self.gpu_arg(node, &args, 0)?;
                if x.offset != 0 {
                    return Err(Error::Unsupported("pooling a view".into()));
                }
                let p = cpu::pool_params(node, &x.shape)?;
                let o = self.alloc(vec![x.shape[0], x.shape[1], p.out_h, p.out_w]);
                let push = PoolPush {
                    n: o.len() as u32,
                    in_h: p.in_h as u32,
                    in_w: p.in_w as u32,
                    out_h: p.out_h as u32,
                    out_w: p.out_w as u32,
                    kernel_h: p.kernel[0] as u32,
                    kernel_w: p.kernel[1] as u32,
                    stride_h: p.stride[0] as u32,
                    stride_w: p.stride[1] as u32,
                    pad_h: p.pad[0] as u32,
                    pad_w: p.pad[1] as u32,
                    is_max: u32::from(p.max),
                    count_include_pad: u32::from(p.count_include_pad),
                };
                self.step(KernelKind::Pool, vec![x.buffer, o.buffer], &push, o.len());
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "GlobalAveragePool" | "ReduceMean" | "ReduceSum" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let p = cpu::reduce_params(node, &x.shape, &host)?;
                let own = strides(&x.shape);
                let kept: Vec<usize> = (0..x.shape.len()).filter(|d| !p.axes.contains(d)).collect();
                let pick = |dims: &[usize]| -> (Vec<usize>, Vec<u32>) {
                    (
                        dims.iter().map(|&d| x.shape[d]).collect(),
                        dims.iter().map(|&d| own[d] as u32).collect(),
                    )
                };
                let (kept_shape, kept_strides) = pick(&kept);
                let (reduced_shape, reduced_strides) = pick(&p.axes);
                let (kept_shape, kept_strides) = pad4(&kept_shape, &kept_strides)?;
                let (reduced_shape, reduced_strides) = pad4(&reduced_shape, &reduced_strides)?;
                let o = self.alloc(p.out_shape());
                let push = ReducePush {
                    n: o.len() as u32,
                    count: reduced_shape.iter().product(),
                    mean: u32::from(p.mean),
                    x_offset: x.offset as u32,
                    kept: kept_shape,
                    kept_strides,
                    reduced: reduced_shape,
                    reduced_strides,
                };
                self.step(KernelKind::Reduce, vec![x.buffer, o.buffer], &push, o.len());
                out(&mut self.values, 0, Value::Gpu(o));
            }
            other => {
                return Err(Error::Unsupported(format!("operator {other} on the GPU")));
            }
        }
        Ok(())
    }
}
