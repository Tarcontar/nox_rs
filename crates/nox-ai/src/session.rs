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

use crate::cpu::{self, BinaryOp, Strided, UnaryOp};
use crate::kernels::{
    BinaryPush, CM2_COLUMNS, CM2_TILE_H, CM2_TILE_W, ConvPush, ConvTile, CopyPush, GEMM_TILE,
    GatherNdPush, KernelKind, Kernels, MatMulPush, MatrixKernels, PoolPush, Precision, ReducePush,
    ResizePush, RowsPush, TransposePush, UnaryPush, groups,
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
    groups: [u32; 3],
    /// An input the output may overwrite element by element (same size,
    /// read at the same index), if this is its last use.
    in_place: Option<usize>,
    /// The node it's for (`Node::describe`), for profilers' labels.
    node: Arc<str>,
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
    precision: Precision,
    // Dropped before the kernels its recording uses.
    plan: Option<Plan>,
    kernels: Kernels,
}

impl Session {
    /// A session for `model` in the precision it's made for, as far as
    /// `gpu` can (`Precision::for_model`).
    pub fn new(gpu: Arc<GpuDevice>, model: Model) -> Self {
        let precision = Precision::for_model(&model, &gpu);
        Self::with_precision(gpu, model, precision)
    }

    /// A session keeping tensors in `precision` (F16 needs the device's
    /// 16-bit storage).
    pub fn with_precision(gpu: Arc<GpuDevice>, model: Model, precision: Precision) -> Self {
        let kernels = Kernels::new(precision, &gpu);
        Self {
            gpu,
            model,
            precision,
            plan: None,
            kernels,
        }
    }

    /// Whether matrix multiplications (convolutions, MatMul) run on
    /// tensor cores: F16 on a GPU that has them.
    pub fn tensor_cores(&self) -> bool {
        self.kernels.matrices() != MatrixKernels::Scalar
    }

    /// What the matrix multiplications run on.
    pub fn matrix_kernels(&self) -> MatrixKernels {
        self.kernels.matrices()
    }

    /// How tensors are kept on the GPU: what input and output buffers
    /// hold (`buffer`).
    pub fn precision(&self) -> Precision {
        self.precision
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
            node: "".into(),
            matrices: self.kernels.matrices(),
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
            planner.node = node.describe().into();
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
            steps,
            ..
        } = planner;
        let mut steps = schedule(steps);
        let (physical, physical_sizes) = assign(&sizes, &pinned, &steps);
        let buffers: Vec<Buffer> = physical_sizes
            .iter()
            .map(|&n| {
                let bytes = (n * self.precision.bytes()).next_multiple_of(4);
                Buffer::new(&self.gpu, bytes as u64, MemoryUse::Device)
            })
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
        let uploads: Vec<(usize, Arc<Vec<f32>>)> =
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
        upload(&self.gpu, &buffers, &uploads, self.precision)?;
        let max_buffers = steps.iter().map(|s| s.kind.buffers()).max().unwrap_or(1);
        let p = self.precision;
        let mut recording = Recording::new(&self.gpu, steps.len() as u32, max_buffers)?;
        for step in &steps {
            let kernel = self.kernels.get(&self.gpu, step.kind)?;
            let slices: Vec<Binding<'_>> =
                step.buffers.iter().map(|&b| (&buffers[b]).into()).collect();
            // Named for profilers (NOX_GPU_LABELS=1): the node, the kernel.
            recording.begin_label(&format!("{} [{}]", step.node, step.kind.label()));
            recording.dispatch(kernel, &slices, &step.push, step.groups)?;
            recording.end_label();
        }
        for (from, to) in &copies {
            recording.copy(
                slice(&buffers[from.buffer], from.offset, p),
                slice(&buffers[to.buffer], to.offset, p),
                (from.len() * p.bytes()) as u64,
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
        let bytes = self.precision.encode(data);
        let mut staging = Buffer::new(&self.gpu, bytes.len() as u64, MemoryUse::Upload)?;
        staging.write(&bytes)?;
        let mut copy = Recording::new(&self.gpu, 0, 0)?;
        copy.copy(
            (&staging).into(),
            slice(&plan.buffers[value.buffer], value.offset, self.precision),
            bytes.len() as u64,
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
        let bytes = (value.len() * self.precision.bytes()) as u64;
        let staging = Buffer::new(&self.gpu, bytes, MemoryUse::Download)?;
        let mut copy = Recording::new(&self.gpu, 0, 0)?;
        copy.copy(
            slice(&plan.buffers[value.buffer], value.offset, self.precision),
            (&staging).into(),
            bytes,
        );
        copy.run()?;
        let mut data = self.precision.decode(&staging.read::<u8>()?);
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

/// `buffer` from element `offset` on.
fn slice(buffer: &Buffer, offset: usize, precision: Precision) -> BufferSlice<'_> {
    BufferSlice {
        buffer,
        offset: (offset * precision.bytes()) as u64,
        range: nox_gpu::vk::WHOLE_SIZE,
    }
}

/// Copies the planned constants into their buffers.
fn upload(
    gpu: &Arc<GpuDevice>,
    buffers: &[Buffer],
    uploads: &[(usize, Arc<Vec<f32>>)],
    precision: Precision,
) -> Result<()> {
    if uploads.is_empty() {
        return Ok(());
    }
    // Encoded straight into the staging buffer, one after another, in
    // pieces spread over a few threads (176 MB for BiRefNet in 16 bits:
    // 30 ms, 190 by copying everything first, then encoding on one thread).
    let total: usize = uploads
        .iter()
        .map(|(_, d)| d.len() * precision.bytes())
        .sum();
    let mut staging = Buffer::new(gpu, total as u64, MemoryUse::Upload)?;
    let mut rest = staging.mapped_mut()?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    // Dealt out in turn: the pieces of one tensor go to every thread.
    let mut shares: Vec<Vec<(&[f32], &mut [u8])>> = (0..threads).map(|_| Vec::new()).collect();
    let mut i = 0;
    for (_, data) in uploads {
        for chunk in data.chunks(1 << 20) {
            let (piece, after) = rest.split_at_mut(chunk.len() * precision.bytes());
            shares[i % threads].push((chunk, piece));
            rest = after;
            i += 1;
        }
    }
    std::thread::scope(|scope| {
        for share in shares {
            scope.spawn(move || {
                for (data, out) in share {
                    precision.encode_into(data, out);
                }
            });
        }
    });
    let mut copy = Recording::new(gpu, 0, 0)?;
    let mut at = 0u64;
    for (buffer, data) in uploads {
        let bytes = (data.len() * precision.bytes()) as u64;
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

/// Orders the steps so each value is computed just before it's needed: a
/// depth-first walk from the last steps, taking each step's inputs in
/// their original order. Models list nodes breadth-first (e.g. all four
/// corners of a deformable convolution's sampling before any is used),
/// which keeps far more alive at once.
fn schedule(steps: Vec<Step>) -> Vec<Step> {
    let n = steps.len();
    // The steps writing each (virtual) buffer so far, in order.
    let mut writers: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(n);
    for (i, step) in steps.iter().enumerate() {
        let output = *step.buffers.last().expect("output");
        let mut d: Vec<usize> = Vec::new();
        for &b in &step.buffers {
            // Inputs, and earlier writes to the output (a Concat's copies,
            // a Pad's fill) come first.
            if let Some(w) = writers.get(&b) {
                d.extend(w.iter().copied());
            }
        }
        // Newest first: a chain like ((a + b) + c) + d then adds each
        // term as it's made instead of making all of them first.
        d.sort_unstable_by(|a, b| b.cmp(a));
        d.dedup();
        deps.push(d);
        writers.entry(output).or_default().push(i);
    }
    let mut order = Vec::with_capacity(n);
    // 0: not reached, 1: waiting for its inputs, 2: placed.
    let mut state = vec![0u8; n];
    for root in (0..n).rev() {
        if state[root] != 0 {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        state[root] = 1;
        while let Some((i, next)) = stack.pop() {
            if let Some(&dep) = deps[i].get(next) {
                stack.push((i, next + 1));
                if state[dep] == 0 {
                    state[dep] = 1;
                    stack.push((dep, 0));
                }
            } else {
                state[i] = 2;
                order.push(i);
            }
        }
    }
    let mut slots: Vec<Option<Step>> = steps.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("placed once"))
        .collect()
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
        // Elementwise over an input that dies here: write over it.
        if physical[output] == NONE
            && let Some(input) = step.in_place
            && last[input] == i
            && !pinned[input]
            && first[input] != NONE
            && first[output] == i
            && sizes[input] >= sizes[output]
            && !free.contains(&physical[input])
        {
            physical[output] = physical[input];
            // It lives on as the output now; not freed with the input.
            last[input] = usize::MAX;
        }
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
    uploads: Vec<(usize, Arc<Vec<f32>>)>,
    /// Constants already on the GPU, by name.
    uploaded: HashMap<String, GpuValue>,
    steps: Vec<Step>,
    /// The node being planned (`Node::describe`), for its steps.
    node: Arc<str>,
    /// What the matrix multiplications run on.
    matrices: MatrixKernels,
}

/// `shape` with fewer dimensions, walked the same way by every operand
/// (`strides`, one list per operand): extents of 1 dropped, and a
/// dimension merged into the one inside it where each operand's stride
/// over it is the inner stride times the inner extent. Kernels then work
/// out fewer coordinates per element (a same-shaped elementwise step has
/// one dimension: no divisions). The order of elements stays the same.
/// At least one dimension.
fn collapse(shape: &[usize], strides: &[Vec<isize>]) -> (Vec<usize>, Vec<Vec<isize>>) {
    // Innermost first while building.
    let mut out_shape: Vec<usize> = Vec::new();
    let mut out_strides: Vec<Vec<isize>> = vec![Vec::new(); strides.len()];
    for d in (0..shape.len()).rev() {
        if shape[d] == 1 {
            continue;
        }
        let merges = out_shape.last().is_some_and(|&inner| {
            strides
                .iter()
                .zip(&out_strides)
                .all(|(s, o)| s[d] == o.last().expect("inner") * inner as isize)
        });
        if merges {
            *out_shape.last_mut().expect("inner") *= shape[d];
        } else {
            out_shape.push(shape[d]);
            for (o, s) in out_strides.iter_mut().zip(strides) {
                o.push(s[d]);
            }
        }
    }
    if out_shape.is_empty() {
        out_shape.push(1);
        for o in &mut out_strides {
            o.push(0);
        }
    }
    out_shape.reverse();
    for o in &mut out_strides {
        o.reverse();
    }
    (out_shape, out_strides)
}

/// Up to `N` dimensions, padded in front with size 1 (stride 0).
fn pad<const N: usize, T: Copy + Default>(
    shape: &[usize],
    strides: &[T],
) -> Result<([u32; N], [T; N])> {
    if shape.len() > N {
        return Err(Error::Unsupported(format!("rank-{} tensors", shape.len())));
    }
    let mut s = [1u32; N];
    let mut st = [T::default(); N];
    let pad = N - shape.len();
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
                self.uploads.push((value.buffer, tensor.shared_f32()));
                self.uploaded.insert(name.clone(), value.clone());
                Ok(value)
            }
            None => Err(Error::Model(format!("missing input {i}"))),
        }
    }

    /// Constant convolution weights `tensor` (out channels × in channels ×
    /// kernel height × width) on the GPU with the kernel's taps before the
    /// input channels (out channels × taps × in channels), as
    /// `conv_gemm_cm2.glsl` reads them; uploaded once per name.
    fn upload_taps_major(&mut self, name: &str, tensor: &Tensor) -> GpuValue {
        let key = format!("{name} (taps first)");
        if let Some(value) = self.uploaded.get(&key) {
            return value.clone();
        }
        let [out, inputs, h, w] = tensor.shape[..] else {
            unreachable!("Conv weights have 4 dimensions");
        };
        let taps = h * w;
        let data = tensor.shared_f32();
        let mut reordered = vec![0.0; data.len()];
        for o in 0..out {
            for i in 0..inputs {
                for t in 0..taps {
                    reordered[(o * taps + t) * inputs + i] = data[(o * inputs + i) * taps + t];
                }
            }
        }
        let value = self.alloc(tensor.shape.clone());
        self.pinned[value.buffer] = true;
        self.uploads.push((value.buffer, Arc::new(reordered)));
        self.uploaded.insert(key, value.clone());
        value
    }

    /// A dispatch with one invocation per element of `n`.
    fn step(&mut self, kind: KernelKind, buffers: Vec<usize>, push: &impl bytemuck::Pod, n: usize) {
        self.step_groups(kind, buffers, push, groups(n as u32));
    }

    fn step_groups(
        &mut self,
        kind: KernelKind,
        buffers: Vec<usize>,
        push: &impl bytemuck::Pod,
        groups: [u32; 3],
    ) {
        self.steps.push(Step {
            kind,
            buffers,
            push: bytemuck::bytes_of(push).to_vec(),
            groups,
            in_place: None,
            node: Arc::clone(&self.node),
        });
    }

    /// Lets the last step write over input `buffer` when it's done with it.
    fn last_in_place(&mut self, buffer: usize) {
        self.steps.last_mut().expect("step").in_place = Some(buffer);
    }

    /// `tensor`, computed while planning, on the GPU.
    fn upload_tensor(&mut self, tensor: &Tensor) -> GpuValue {
        let value = self.alloc(tensor.shape.clone());
        self.pinned[value.buffer] = true;
        self.uploads.push((value.buffer, tensor.shared_f32()));
        value
    }

    /// `out = a (op) b` with broadcasting.
    fn binary(&mut self, op: BinaryOp, a: &GpuValue, b: &GpuValue) -> Result<GpuValue> {
        let shape = cpu::broadcast_shape(&a.shape, &b.shape)?;
        let signed = |s: Vec<usize>| s.into_iter().map(|v| v as isize).collect::<Vec<_>>();
        let (dims, operands) = collapse(
            &shape,
            &[
                signed(cpu::broadcast_strides(&a.shape, &shape)),
                signed(cpu::broadcast_strides(&b.shape, &shape)),
            ],
        );
        let (padded, sa) = pad::<6, isize>(&dims, &operands[0])?;
        let (_, sb) = pad::<6, isize>(&dims, &operands[1])?;
        let o = self.alloc(shape);
        let push = BinaryPush {
            n: o.len() as u32,
            op: op as u32,
            a_offset: a.offset as u32,
            b_offset: b.offset as u32,
            rank: dims.len() as u32,
            shape: padded,
            a_strides: sa.map(|s| s as u32),
            b_strides: sb.map(|s| s as u32),
        };
        self.step(
            KernelKind::Binary,
            vec![a.buffer, b.buffer, o.buffer],
            &push,
            o.len(),
        );
        // An operand of the output's size, read where it's written.
        if a.offset == 0 && a.len() == o.len() {
            self.last_in_place(a.buffer);
        } else if b.offset == 0 && b.len() == o.len() {
            self.last_in_place(b.buffer);
        }
        Ok(o)
    }

    /// `out = f(x)` elementwise.
    fn unary(&mut self, op: UnaryOp, x: &GpuValue, alpha: f32, beta: f32) -> GpuValue {
        let o = self.alloc(x.shape.clone());
        self.unary_into(op, x, &o, alpha, beta);
        o
    }

    fn unary_into(&mut self, op: UnaryOp, x: &GpuValue, o: &GpuValue, alpha: f32, beta: f32) {
        let push = UnaryPush {
            n: o.len() as u32,
            op: op as u32,
            x_offset: x.offset as u32,
            alpha,
            beta,
        };
        self.step(KernelKind::Unary, vec![x.buffer, o.buffer], &push, o.len());
        if x.offset == 0 && x.len() == o.len() && op != UnaryOp::Fill {
            self.last_in_place(x.buffer);
        }
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
        let dst_strides: Vec<isize> = dst_strides.iter().map(|&s| s as isize).collect();
        let (dims, operands) = collapse(&view.shape, &[view.strides.clone(), dst_strides]);
        let rank = dims.len() as u32;
        let (shape, src_strides) = pad::<6, isize>(&dims, &operands[0])?;
        let (_, dst_strides) = pad::<6, isize>(&dims, &operands[1])?;
        // Reading along one dimension and writing along another, both
        // long enough to fill tiles: a transpose through shared memory.
        let fast = |strides: &[isize; 6]| (0..6).find(|&k| strides[k] == 1 && shape[k] >= 8);
        let innermost = (0..6).rev().find(|&k| shape[k] > 1);
        if let (Some(a), Some(b), Some(inner)) = (fast(&dst_strides), fast(&src_strides), innermost)
            && a != b
            && src_strides[inner] != 1
        {
            let tiles_a = shape[a].div_ceil(32);
            let tiles_b = shape[b].div_ceil(32);
            let outer = n as u32 / (shape[a] * shape[b]);
            let push = TransposePush {
                tiles_a,
                tiles_b,
                outer,
                dst_fast: a as u32,
                src_fast: b as u32,
                src_offset: (src.offset + view.offset) as i32,
                dst_offset: (dst.offset + dst_offset) as i32,
                rank,
                shape,
                src_strides: src_strides.map(|s| s as i32),
                dst_strides: dst_strides.map(|s| s as i32),
            };
            let total = tiles_a * tiles_b * outer;
            let x = total.min(65535);
            self.step_groups(
                KernelKind::Transpose,
                vec![src.buffer, dst.buffer],
                &push,
                [x, total.div_ceil(x), 1],
            );
            return Ok(());
        }
        let push = CopyPush {
            n: n as u32,
            src_offset: (src.offset + view.offset) as i32,
            dst_offset: (dst.offset + dst_offset) as i32,
            rank,
            shape,
            src_strides: src_strides.map(|s| s as i32),
            dst_strides: dst_strides.map(|s| s as i32),
        };
        self.step(KernelKind::Copy, vec![src.buffer, dst.buffer], &push, n);
        Ok(())
    }

    /// `x` from the start of a buffer of its own, for the kernels that
    /// read from there: copied if it's a view further in.
    fn contiguous(&mut self, x: GpuValue) -> Result<GpuValue> {
        if x.offset == 0 {
            return Ok(x);
        }
        let o = self.alloc(x.shape.clone());
        let view = Strided {
            shape: x.shape.clone(),
            strides: strides(&x.shape).iter().map(|&s| s as isize).collect(),
            offset: 0,
        };
        self.copy(&x, &view, &o, 0, &strides(&o.shape))?;
        Ok(o)
    }

    /// The view `view` of `x`: in place if its elements are one
    /// contiguous run (a slice along the outermost axis that isn't 1),
    /// else copied out.
    fn view_or_copy(&mut self, x: &GpuValue, view: Strided) -> Result<GpuValue> {
        let dense = strides(&view.shape);
        let contiguous = view
            .shape
            .iter()
            .zip(&view.strides)
            .zip(&dense)
            .all(|((&d, &s), &c)| d == 1 || s == c as isize);
        if contiguous {
            return Ok(GpuValue {
                buffer: x.buffer,
                offset: x.offset + view.offset,
                shape: view.shape,
            });
        }
        let o = self.alloc(view.shape.clone());
        self.copy(x, &view, &o, 0, &strides(&o.shape))?;
        Ok(o)
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
                let o = self.binary(BinaryOp::of(&node.op).expect("binary"), &a, &b)?;
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Sum" => {
                let mut sum = self.gpu_arg(node, &args, 0)?;
                for i in 1..args.len() {
                    if args[i].is_some() {
                        let part = self.gpu_arg(node, &args, i)?;
                        sum = self.binary(BinaryOp::Add, &sum, &part)?;
                    }
                }
                out(&mut self.values, 0, Value::Gpu(sum));
            }
            "BatchNormalization" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let (scale, shift) = cpu::batch_norm_params(node, x.shape.len(), &host)?;
                let (scale, shift) = (self.upload_tensor(&scale), self.upload_tensor(&shift));
                let scaled = self.binary(BinaryOp::Mul, &x, &scale)?;
                let o = self.binary(BinaryOp::Add, &scaled, &shift)?;
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Relu" | "Sigmoid" | "Tanh" | "Exp" | "Sqrt" | "Neg" | "Abs" | "Erf"
            | "HardSigmoid" | "HardSwish" | "Clip" | "Floor" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let (op, alpha, beta) = cpu::unary_params(node, &host)?;
                let o = self.unary(op, &x, alpha, beta);
                out(&mut self.values, 0, Value::Gpu(o));
            }
            // Integers on the GPU are whole floats.
            "Cast" if matches!(node.int("to", 1), 6 | 7) => {
                let x = self.gpu_arg(node, &args, 0)?;
                let o = self.unary(UnaryOp::Trunc, &x, 0.0, 0.0);
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "MatMul" => {
                let a = self.gpu_arg(node, &args, 0)?;
                let b = self.gpu_arg(node, &args, 1)?;
                let p = cpu::matmul_params(&a.shape, &b.shape)?;
                let (batch, a_strides) = pad::<4, usize>(&p.batch, &p.a_batch_strides)?;
                let (_, b_strides) = pad::<4, usize>(&p.batch, &p.b_batch_strides)?;
                let o = self.alloc(p.out_shape.clone());
                let push = MatMulPush {
                    m: p.m as u32,
                    k: p.k as u32,
                    n: p.n as u32,
                    a_offset: a.offset as u32,
                    b_offset: b.offset as u32,
                    pad: [0; 3],
                    batch,
                    a_batch_strides: a_strides.map(|s| s as u32),
                    b_batch_strides: b_strides.map(|s| s as u32),
                };
                let batches: u32 = batch.iter().product();
                let groups = [
                    (p.n as u32).div_ceil(GEMM_TILE),
                    (p.m as u32).div_ceil(GEMM_TILE),
                    batches,
                ];
                if groups[2] > 65535 {
                    return Err(Error::Unsupported(
                        "MatMul of more than 65535 matrices".into(),
                    ));
                }
                self.step_groups(
                    KernelKind::MatMul,
                    vec![a.buffer, b.buffer, o.buffer],
                    &push,
                    groups,
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "LayerNormalization" | "Softmax" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let rank = x.shape.len() as i64;
                let axis = cpu::normalize_axis(node.int("axis", -1), rank)?;
                if axis + 1 != x.shape.len() {
                    return Err(Error::Unsupported(format!(
                        "{} over axis {axis} (only the last)",
                        node.op
                    )));
                }
                let d = x.shape[axis];
                let layer_norm = node.op == "LayerNormalization";
                let (scale, bias) = if layer_norm {
                    let scale = self.gpu_arg(node, &args, 1)?;
                    let bias = match args.get(2).cloned().flatten() {
                        Some(_) => Some(self.gpu_arg(node, &args, 2)?),
                        None => None,
                    };
                    (Some(scale), bias)
                } else {
                    (None, None)
                };
                let o = self.alloc(x.shape.clone());
                let push = RowsPush {
                    rows: (x.len() / d.max(1)) as u32,
                    d: d as u32,
                    op: u32::from(!layer_norm),
                    x_offset: x.offset as u32,
                    epsilon: node.float("epsilon", 1e-5),
                    has_bias: u32::from(bias.is_some()),
                    scale_len: scale.as_ref().map_or(1, |s| s.len() as u32),
                    bias_len: bias.as_ref().map_or(1, |b| b.len() as u32),
                };
                let scale = scale.map(|s| self.contiguous(s)).transpose()?;
                let bias = bias.map(|b| self.contiguous(b)).transpose()?;
                let scale = scale.map_or(x.buffer, |s| s.buffer);
                let bias = bias.map_or(x.buffer, |b| b.buffer);
                // A workgroup per row (rows.comp).
                let x_groups = push.rows.clamp(1, 65535);
                self.step_groups(
                    KernelKind::Rows,
                    vec![x.buffer, scale, bias, o.buffer],
                    &push,
                    [x_groups, push.rows.div_ceil(x_groups).max(1), 1],
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "GatherND" => {
                let data = self.gpu_arg(node, &args, 0)?;
                let indices = self.gpu_arg(node, &args, 1)?;
                let p = cpu::gather_nd_params(node, &data.shape, &indices.shape)?;
                if p.depth > 4 {
                    return Err(Error::Unsupported("GatherND with tuples over 4".into()));
                }
                let (dims, strides) = pad::<4, usize>(&p.index_dims, &p.index_strides)?;
                // Padding goes in front; the kernel reads the first `depth`.
                let shift = |a: [u32; 4]| -> [u32; 4] {
                    let mut out = [0; 4];
                    out[..p.depth].copy_from_slice(&a[4 - p.depth..]);
                    out
                };
                let o = self.alloc(p.out_shape.clone());
                let push = GatherNdPush {
                    n: o.len() as u32,
                    tuples: p.tuples as u32,
                    depth: p.depth as u32,
                    slice: p.slice as u32,
                    data_batch: p.data_batch as u32,
                    data_offset: data.offset as u32,
                    indices_offset: indices.offset as u32,
                    pad0: 0,
                    index_dims: shift(dims),
                    index_strides: shift(strides.map(|s| s as u32)),
                };
                self.step(
                    KernelKind::GatherNd,
                    vec![data.buffer, indices.buffer, o.buffer],
                    &push,
                    o.len(),
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Gather" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let indices = host
                    .get(1)
                    .copied()
                    .flatten()
                    .ok_or_else(|| Error::Unsupported("Gather with computed indices".into()))?;
                let rank = x.shape.len();
                let axis = cpu::normalize_axis(node.int("axis", 0), rank as i64)?;
                let mut shape = x.shape[..axis].to_vec();
                shape.extend(&indices.shape);
                shape.extend(&x.shape[axis + 1..]);
                let own = strides(&x.shape);
                // One index: a slice of `x`, maybe in place.
                if indices.shape.is_empty() {
                    let dim = x.shape[axis] as i64;
                    let i = indices.to_i64()[0];
                    let i = if i < 0 { i + dim } else { i }.clamp(0, dim - 1) as usize;
                    let mut view_strides: Vec<isize> = own.iter().map(|&s| s as isize).collect();
                    view_strides.remove(axis);
                    let view = Strided {
                        shape,
                        strides: view_strides,
                        offset: i * own[axis],
                    };
                    let o = self.view_or_copy(&x, view)?;
                    out(&mut self.values, 0, Value::Gpu(o));
                    return Ok(());
                }
                let o = self.alloc(shape);
                let out_strides = strides(&o.shape);
                // Each index copies one slice (the axis gone) into place.
                let mut slice_shape = x.shape.clone();
                slice_shape.remove(axis);
                let mut slice_strides: Vec<isize> = own.iter().map(|&s| s as isize).collect();
                slice_strides.remove(axis);
                let outer = &out_strides[..axis];
                let inner = &out_strides[axis + indices.shape.len()..];
                let mut dst_strides: Vec<usize> = outer.to_vec();
                dst_strides.extend(inner);
                let index_stride = if indices.shape.is_empty() {
                    0
                } else {
                    out_strides[axis + indices.shape.len() - 1]
                };
                let dim = x.shape[axis] as i64;
                for (j, &i) in indices.to_i64().iter().enumerate() {
                    let i = if i < 0 { i + dim } else { i }.clamp(0, dim - 1) as usize;
                    let view = Strided {
                        shape: slice_shape.clone(),
                        strides: slice_strides.clone(),
                        offset: i * own[axis],
                    };
                    self.copy(&x, &view, &o, j * index_stride, &dst_strides)?;
                }
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Pad" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let p = cpu::pad_params(node, &x.shape, &host)?;
                if p.before.iter().any(|&b| b < 0)
                    || p.out_shape.iter().zip(&x.shape).any(|(o, i)| o < i)
                {
                    return Err(Error::Unsupported("Pad that crops".into()));
                }
                let o = self.alloc(p.out_shape.clone());
                self.unary_into(UnaryOp::Fill, &x, &o, p.value, 0.0);
                let out_strides = strides(&o.shape);
                let at: usize = p
                    .before
                    .iter()
                    .zip(&out_strides)
                    .map(|(&b, &s)| b as usize * s)
                    .sum();
                let view = Strided {
                    shape: x.shape.clone(),
                    strides: strides(&x.shape).iter().map(|&s| s as isize).collect(),
                    offset: 0,
                };
                self.copy(&x, &view, &o, at, &out_strides)?;
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
                let o = self.view_or_copy(&x, view)?;
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
                    let o = self.view_or_copy(&x, view)?;
                    out(&mut self.values, i, Value::Gpu(o));
                    at += size;
                }
            }
            "Conv" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let weights = args
                    .get(1)
                    .cloned()
                    .flatten()
                    .ok_or_else(|| Error::Model("missing input 1".into()))?;
                let p = cpu::conv_params(node, &x.shape, weights.shape())?;
                let per_group_out = p.out_channels / p.group;
                let pointwise =
                    p.kernel_h == 1 && p.kernel_w == 1 && p.stride == [1, 1] && p.pad == [0, 0];
                // On cooperative matrices 2 with many input channels per
                // group: with few, most of each step of 32 is padding, and
                // they were slower than on subgroup ones (Nsight GPU Trace
                // of BiRefNet and RVM on an RTX 5060, per convolution: the
                // least GPU time in total from 64 on, of 16 to 512).
                // Other than pointwise ones take their weights tap by tap,
                // reordered while planning (constant weights only).
                let tensor2 = self.matrices == MatrixKernels::Tensor2
                    && p.stride == [1, 1]
                    && p.in_channels / p.group >= 64
                    && (pointwise || matches!(weights, Value::Host(_)));
                let geometry = [
                    p.kernel_h,
                    p.kernel_w,
                    p.stride[0],
                    p.stride[1],
                    p.pad[0],
                    p.pad[1],
                    p.dilation[0],
                    p.dilation[1],
                ]
                .map(|v| v as u32);
                let (kind, tiles) = if tensor2 {
                    // Over pixels (pointwise) or tiles of the output.
                    let rows = if per_group_out > 64 { 128 } else { 64 };
                    let columns = if pointwise {
                        ((p.out_h * p.out_w) as u32).div_ceil(CM2_COLUMNS)
                    } else {
                        (p.out_h as u32).div_ceil(CM2_TILE_H)
                            * (p.out_w as u32).div_ceil(CM2_TILE_W)
                    };
                    (
                        KernelKind::ConvGemm2(geometry, rows),
                        [columns, (per_group_out as u32).div_ceil(rows)],
                    )
                } else {
                    // Wide tiles for few of them on tensor cores.
                    let tile = match self.matrices {
                        MatrixKernels::Scalar => ConvTile::Square,
                        _ => ConvTile::for_rows(per_group_out),
                    };
                    let (rows, columns) = tile.size();
                    (
                        KernelKind::ConvGemm(geometry, tile),
                        [
                            ((p.out_h * p.out_w) as u32).div_ceil(columns),
                            (per_group_out as u32).div_ceil(rows),
                        ],
                    )
                };
                let tiles = [tiles[0], tiles[1], (p.batch * p.group) as u32];
                // Many outputs per group: a matrix multiplication. Few
                // (depthwise) would leave most of its tiles empty; the
                // direct kernel takes those, in the weights' own order.
                let gemm = per_group_out >= 16 && tiles.iter().all(|&t| t <= 65535);
                let w = match &weights {
                    Value::Host(tensor) if gemm && tensor2 && !pointwise => {
                        self.upload_taps_major(&node.inputs[1], tensor)
                    }
                    _ => self.gpu_arg(node, &args, 1)?,
                };
                let bias = match args.get(2).cloned().flatten() {
                    Some(_) => Some(self.gpu_arg(node, &args, 2)?),
                    None => None,
                };
                let (x, w) = (self.contiguous(x)?, self.contiguous(w)?);
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
                let buffers = vec![x.buffer, w.buffer, bias, o.buffer];
                if gemm {
                    self.step_groups(kind, buffers, &push, tiles);
                } else {
                    self.step(KernelKind::Conv, buffers, &push, o.len());
                }
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "Resize" | "Upsample" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let x = self.contiguous(x)?;
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
                self.step(KernelKind::Resize, vec![x.buffer, o.buffer], &push, o.len());
                out(&mut self.values, 0, Value::Gpu(o));
            }
            "AveragePool" | "MaxPool" => {
                let x = self.gpu_arg(node, &args, 0)?;
                let x = self.contiguous(x)?;
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
                // Each part collapsed (an image's pixels are one run).
                let pick = |dims: &[usize]| -> Result<(u32, [u32; 4], [u32; 4])> {
                    let shape: Vec<usize> = dims.iter().map(|&d| x.shape[d]).collect();
                    let walk: Vec<isize> = dims.iter().map(|&d| own[d] as isize).collect();
                    let (shape, walk) = collapse(&shape, &[walk]);
                    let (padded, walk) = pad::<4, isize>(&shape, &walk[0])?;
                    Ok((shape.len() as u32, padded, walk.map(|s| s as u32)))
                };
                let (kept_rank, kept_shape, kept_strides) = pick(&kept)?;
                let (reduced_rank, reduced_shape, reduced_strides) = pick(&p.axes)?;
                let o = self.alloc(p.out_shape());
                let push = ReducePush {
                    n: o.len() as u32,
                    count: reduced_shape.iter().product(),
                    mean: u32::from(p.mean),
                    x_offset: x.offset as u32,
                    kept_rank,
                    reduced_rank,
                    pad: [0; 2],
                    kept: kept_shape,
                    kept_strides,
                    reduced: reduced_shape,
                    reduced_strides,
                };
                // A workgroup per output (reduce.comp).
                let x_groups = push.n.clamp(1, 65535);
                self.step_groups(
                    KernelKind::Reduce,
                    vec![x.buffer, o.buffer],
                    &push,
                    [x_groups, push.n.div_ceil(x_groups).max(1), 1],
                );
                out(&mut self.values, 0, Value::Gpu(o));
            }
            other => {
                return Err(Error::Unsupported(format!("operator {other} on the GPU")));
            }
        }
        Ok(())
    }
}
