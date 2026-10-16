//! The compute kernels (`shaders/*.comp`) and their push constants, which
//! mirror the shaders' `Params` blocks field by field.

use bytemuck::{Pod, Zeroable};
use nox_gpu::GpuDevice;
use nox_gpu::compute::{BindingKind, Kernel};
use std::collections::HashMap;
use std::sync::Arc;

use crate::Result;

/// Invocations per workgroup (`common.glsl`).
const WORKGROUP: u32 = 256;
/// The output tile of a workgroup of the matrix multiplications
/// (`gemm.glsl`, `gemm_tensor.glsl`), in rows and columns.
pub(crate) const GEMM_TILE: u32 = 64;
/// The columns of a workgroup of convolutions on cooperative matrices 2
/// (`conv_gemm_cm2.glsl`): pixels of pointwise ones, a 4×16 tile of the
/// output of others.
pub(crate) const CM2_COLUMNS: u32 = 128;
pub(crate) const CM2_TILE_H: u32 = 4;
pub(crate) const CM2_TILE_W: u32 = 16;

/// The output tile (rows × columns) of a ConvGemm workgroup on tensor
/// cores (`gemm_tensor.glsl`): wide ones for few output channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConvTile {
    Square,
    Wide32,
    Wide16,
}

impl ConvTile {
    /// For `rows` output channels (per group).
    pub fn for_rows(rows: usize) -> Self {
        match rows {
            0..=16 => Self::Wide16,
            17..=32 => Self::Wide32,
            _ => Self::Square,
        }
    }

    /// Rows and columns.
    pub fn size(self) -> (u32, u32) {
        match self {
            Self::Square => (GEMM_TILE, GEMM_TILE),
            Self::Wide32 => (32, 128),
            Self::Wide16 => (16, 128),
        }
    }
}

/// What the matrix multiplications (convolutions, MatMul) run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixKernels {
    /// Shader arithmetic (`gemm.glsl`).
    Scalar,
    /// Tensor cores, through subgroup cooperative matrices
    /// (`gemm_tensor.glsl`).
    Tensor,
    /// The same, and NVIDIA's workgroup cooperative matrices 2 for the
    /// pointwise convolutions with many output channels
    /// (`conv_gemm_cm2.glsl`), where they're faster.
    Tensor2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KernelKind {
    Binary,
    Unary,
    Copy,
    /// A copy that transposes, through shared memory (`transpose.comp`).
    Transpose,
    Conv,
    /// Conv as a matrix multiplication (`conv_gemm.comp`), same push, a
    /// kernel per geometry: kernel height and width, strides, padding and
    /// dilations (its specialization constants); and per tile on tensor
    /// cores.
    ConvGemm([u32; 8], ConvTile),
    /// A ConvGemm with stride 1 on cooperative matrices 2
    /// (`conv_gemm_cm2.glsl`, the same specialization constants), with
    /// 128 or 64 rows (output channels) per workgroup.
    ConvGemm2([u32; 8], u32),
    Resize,
    Pool,
    Reduce,
    MatMul,
    Rows,
    GatherNd,
}

impl KernelKind {
    fn spirv(self, precision: Precision, matrices: MatrixKernels) -> &'static [u8] {
        // The matrix multiplications on tensor cores (F16 only).
        let f16 = precision == Precision::F16;
        let tensor = f16 && matrices != MatrixKernels::Scalar;
        match self {
            Self::ConvGemm2(_, rows) => return conv_gemm2_spirv(rows),
            Self::ConvGemm(_, tile) if tensor => {
                return match tile {
                    ConvTile::Square => {
                        include_bytes!(concat!(env!("OUT_DIR"), "/conv_gemm_tensor.comp.spv"))
                    }
                    ConvTile::Wide32 => include_bytes!(concat!(
                        env!("OUT_DIR"),
                        "/conv_gemm_tensor_32x128.comp.spv"
                    )),
                    ConvTile::Wide16 => include_bytes!(concat!(
                        env!("OUT_DIR"),
                        "/conv_gemm_tensor_16x128.comp.spv"
                    )),
                };
            }
            Self::MatMul if tensor => {
                return include_bytes!(concat!(env!("OUT_DIR"), "/matmul_tensor.comp.spv"));
            }
            _ => {}
        }
        macro_rules! spv {
            ($name:literal) => {
                match precision {
                    Precision::F32 => {
                        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".comp.spv"))
                    }
                    Precision::F16 => {
                        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, "_f16.comp.spv"))
                    }
                }
            };
        }
        match self {
            Self::Binary => spv!("binary"),
            Self::Unary => spv!("unary"),
            Self::Copy => spv!("copy"),
            Self::Transpose => spv!("transpose"),
            Self::Conv => spv!("conv"),
            Self::ConvGemm(..) => spv!("conv_gemm"),
            Self::ConvGemm2(..) => unreachable!(),
            Self::Resize => spv!("resize"),
            Self::Pool => spv!("pool"),
            Self::Reduce => spv!("reduce"),
            Self::MatMul => spv!("matmul"),
            Self::Rows => spv!("rows"),
            Self::GatherNd => spv!("gather_nd"),
        }
    }

    /// Storage buffers the kernel binds (inputs, then the output).
    pub fn buffers(self) -> u32 {
        match self {
            Self::Binary | Self::MatMul | Self::GatherNd => 3,
            Self::Conv | Self::ConvGemm(..) | Self::ConvGemm2(..) | Self::Rows => 4,
            _ => 2,
        }
    }

    /// Its specialization constants.
    fn constants(self) -> Vec<u32> {
        match self {
            Self::ConvGemm(geometry, _) | Self::ConvGemm2(geometry, _) => geometry.to_vec(),
            _ => Vec::new(),
        }
    }

    /// Its name, for profilers' labels.
    pub fn label(self) -> String {
        match self {
            Self::ConvGemm(_, tile) => {
                let (rows, columns) = tile.size();
                format!("ConvGemm{rows}x{columns}")
            }
            Self::ConvGemm2(_, rows) => format!("ConvGemm2x{rows}"),
            _ => format!("{self:?}"),
        }
    }

    fn push_size(self) -> usize {
        match self {
            Self::Binary => size_of::<BinaryPush>(),
            Self::Unary => size_of::<UnaryPush>(),
            Self::Copy => size_of::<CopyPush>(),
            Self::Transpose => size_of::<TransposePush>(),
            Self::Conv | Self::ConvGemm(..) | Self::ConvGemm2(..) => size_of::<ConvPush>(),
            Self::Resize => size_of::<ResizePush>(),
            Self::Pool => size_of::<PoolPush>(),
            Self::Reduce => size_of::<ReducePush>(),
            Self::MatMul => size_of::<MatMulPush>(),
            Self::Rows => size_of::<RowsPush>(),
            Self::GatherNd => size_of::<GatherNdPush>(),
        }
    }
}

/// How tensors are kept on the GPU. The arithmetic is 32-bit either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Precision {
    F32,
    /// 16-bit floats: half the memory and bandwidth, for models whose
    /// weights are 16-bit (`Model::half`). Needs the device's 16-bit
    /// storage.
    F16,
}

impl Precision {
    /// What `model` runs in on `gpu`: F16 if its weights are 16-bit and
    /// the device can keep 16-bit floats.
    pub fn for_model(model: &crate::Model, gpu: &GpuDevice) -> Self {
        if model.half() && gpu.storage_16bit() {
            Self::F16
        } else {
            Self::F32
        }
    }

    /// Bytes per element.
    pub fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F16 => 2,
        }
    }

    /// `values` as stored on the GPU.
    pub(crate) fn encode(self, values: &[f32]) -> Vec<u8> {
        let mut bytes = vec![0; values.len() * self.bytes()];
        self.encode_into(values, &mut bytes);
        bytes
    }

    /// `values` as stored on the GPU, into `out` (exactly their size).
    pub(crate) fn encode_into(self, values: &[f32], out: &mut [u8]) {
        match self {
            Self::F32 => out.copy_from_slice(bytemuck::cast_slice(values)),
            Self::F16 => {
                for (v, o) in values.iter().zip(out.as_chunks_mut::<2>().0) {
                    *o = crate::tensor::f32_to_f16(*v).to_le_bytes();
                }
            }
        }
    }

    /// Stored values back as `f32`s.
    pub(crate) fn decode(self, bytes: &[u8]) -> Vec<f32> {
        match self {
            Self::F32 => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
            Self::F16 => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| crate::tensor::f16_to_f32(u16::from_le_bytes(*b)))
                .collect(),
        }
    }
}

/// Whether the cooperative matrices 2 kernels were built (`build.rs`: the
/// GLSL compiler needs `GL_NV_cooperative_matrix2`).
const CM2_BUILT: bool = cfg!(nox_cm2);

/// The SPIR-V of a ConvGemm2 with `rows` rows per workgroup.
#[cfg(nox_cm2)]
fn conv_gemm2_spirv(rows: u32) -> &'static [u8] {
    if rows == 64 {
        include_bytes!(concat!(env!("OUT_DIR"), "/conv_gemm_cm2_64.comp.spv"))
    } else {
        include_bytes!(concat!(env!("OUT_DIR"), "/conv_gemm_cm2.comp.spv"))
    }
}

/// Never called: without these kernels `Kernels::new` doesn't pick
/// `MatrixKernels::Tensor2`.
#[cfg(not(nox_cm2))]
fn conv_gemm2_spirv(_rows: u32) -> &'static [u8] {
    unreachable!("built without the cooperative matrices 2 kernels")
}

/// The kernels for one precision, created the first time they're needed.
pub(crate) struct Kernels {
    precision: Precision,
    matrices: MatrixKernels,
    kernels: HashMap<KernelKind, Kernel>,
}

impl Kernels {
    /// For `precision` on `gpu`. F16 multiplies on tensor cores where the
    /// GPU has them (`GpuDevice::tensor_f16`), also with cooperative
    /// matrices 2 if it has those (and they were built: `CM2_BUILT`).
    /// `NOX_AI_TENSOR` picks for comparisons:
    /// `0` (no tensor cores), `1` (no cooperative matrices 2), `2`.
    pub fn new(precision: Precision, gpu: &GpuDevice) -> Self {
        let wanted = std::env::var("NOX_AI_TENSOR").ok();
        let tensor2 = CM2_BUILT && gpu.tensor_f16() && gpu.features().cooperative_matrix2;
        let matrices = match (precision, wanted.as_deref()) {
            (Precision::F32, _) | (_, Some("0")) => MatrixKernels::Scalar,
            (_, Some("1")) if gpu.tensor_f16() => MatrixKernels::Tensor,
            _ if tensor2 => MatrixKernels::Tensor2,
            _ if gpu.tensor_f16() => MatrixKernels::Tensor,
            _ => MatrixKernels::Scalar,
        };
        Self {
            precision,
            matrices,
            kernels: HashMap::new(),
        }
    }

    pub fn matrices(&self) -> MatrixKernels {
        self.matrices
    }

    pub fn get(&mut self, gpu: &Arc<GpuDevice>, kind: KernelKind) -> Result<&Kernel> {
        Ok(match self.kernels.entry(kind) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let bindings = vec![BindingKind::Buffer; kind.buffers() as usize];
                let spirv = kind.spirv(self.precision, self.matrices);
                e.insert(Kernel::with_constants(
                    gpu,
                    spirv,
                    &bindings,
                    kind.push_size(),
                    &kind.constants(),
                )?)
            }
        })
    }
}

/// Workgroups for one invocation per element of `n`, on a 2D grid.
pub(crate) fn groups(n: u32) -> [u32; 3] {
    let groups = n.div_ceil(WORKGROUP).max(1);
    let x = groups.min(65535);
    [x, groups.div_ceil(x), 1]
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct BinaryPush {
    pub n: u32,
    pub op: u32,
    pub a_offset: u32,
    pub b_offset: u32,
    /// Dimensions in use, the innermost last (`collapse`).
    pub rank: u32,
    pub shape: [u32; 6],
    pub a_strides: [u32; 6],
    pub b_strides: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct UnaryPush {
    pub n: u32,
    pub op: u32,
    pub x_offset: u32,
    pub alpha: f32,
    pub beta: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct CopyPush {
    pub n: u32,
    pub src_offset: i32,
    pub dst_offset: i32,
    /// Dimensions in use, the innermost last (`collapse`).
    pub rank: u32,
    pub shape: [u32; 6],
    pub src_strides: [i32; 6],
    pub dst_strides: [i32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct TransposePush {
    pub tiles_a: u32,
    pub tiles_b: u32,
    pub outer: u32,
    pub dst_fast: u32,
    pub src_fast: u32,
    pub src_offset: i32,
    pub dst_offset: i32,
    /// Dimensions in use, the innermost last (`collapse`).
    pub rank: u32,
    pub shape: [u32; 6],
    pub src_strides: [i32; 6],
    pub dst_strides: [i32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct ConvPush {
    pub n: u32,
    pub in_channels: u32,
    pub in_h: u32,
    pub in_w: u32,
    pub out_channels: u32,
    pub out_h: u32,
    pub out_w: u32,
    pub kernel_h: u32,
    pub kernel_w: u32,
    pub stride_h: u32,
    pub stride_w: u32,
    pub pad_h: u32,
    pub pad_w: u32,
    pub dilation_h: u32,
    pub dilation_w: u32,
    pub group: u32,
    pub has_bias: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct ResizePush {
    pub n: u32,
    pub in_h: u32,
    pub in_w: u32,
    pub out_h: u32,
    pub out_w: u32,
    pub scale_h: f32,
    pub scale_w: f32,
    pub linear: u32,
    pub coordinates: u32,
    pub nearest: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct PoolPush {
    pub n: u32,
    pub in_h: u32,
    pub in_w: u32,
    pub out_h: u32,
    pub out_w: u32,
    pub kernel_h: u32,
    pub kernel_w: u32,
    pub stride_h: u32,
    pub stride_w: u32,
    pub pad_h: u32,
    pub pad_w: u32,
    pub is_max: u32,
    pub count_include_pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct ReducePush {
    pub n: u32,
    pub count: u32,
    pub mean: u32,
    pub x_offset: u32,
    /// Kept and reduced dimensions in use, the innermost last.
    pub kept_rank: u32,
    pub reduced_rank: u32,
    pub pad: [u32; 2],
    pub kept: [u32; 4],
    pub kept_strides: [u32; 4],
    pub reduced: [u32; 4],
    pub reduced_strides: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct MatMulPush {
    pub m: u32,
    pub k: u32,
    pub n: u32,
    pub a_offset: u32,
    pub b_offset: u32,
    pub pad: [u32; 3],
    pub batch: [u32; 4],
    pub a_batch_strides: [u32; 4],
    pub b_batch_strides: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct RowsPush {
    pub rows: u32,
    pub d: u32,
    /// 0: LayerNormalization, 1: Softmax.
    pub op: u32,
    pub x_offset: u32,
    pub epsilon: f32,
    pub has_bias: u32,
    pub scale_len: u32,
    pub bias_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub(crate) struct GatherNdPush {
    pub n: u32,
    pub tuples: u32,
    pub depth: u32,
    pub slice: u32,
    pub data_batch: u32,
    pub data_offset: u32,
    pub indices_offset: u32,
    pub pad0: u32,
    pub index_dims: [u32; 4],
    pub index_strides: [u32; 4],
}
