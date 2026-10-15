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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KernelKind {
    Binary,
    Unary,
    Copy,
    Conv,
    Resize,
    Pool,
    Reduce,
}

impl KernelKind {
    fn spirv(self) -> &'static [u8] {
        macro_rules! spv {
            ($name:literal) => {
                include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".comp.spv"))
            };
        }
        match self {
            Self::Binary => spv!("binary"),
            Self::Unary => spv!("unary"),
            Self::Copy => spv!("copy"),
            Self::Conv => spv!("conv"),
            Self::Resize => spv!("resize"),
            Self::Pool => spv!("pool"),
            Self::Reduce => spv!("reduce"),
        }
    }

    /// Storage buffers the kernel binds (inputs, then the output).
    pub fn buffers(self) -> u32 {
        match self {
            Self::Binary => 3,
            Self::Conv => 4,
            _ => 2,
        }
    }

    fn push_size(self) -> usize {
        match self {
            Self::Binary => size_of::<BinaryPush>(),
            Self::Unary => size_of::<UnaryPush>(),
            Self::Copy => size_of::<CopyPush>(),
            Self::Conv => size_of::<ConvPush>(),
            Self::Resize => size_of::<ResizePush>(),
            Self::Pool => size_of::<PoolPush>(),
            Self::Reduce => size_of::<ReducePush>(),
        }
    }
}

/// The kernels, created the first time they're needed.
#[derive(Default)]
pub(crate) struct Kernels(HashMap<KernelKind, Kernel>);

impl Kernels {
    pub fn get(&mut self, gpu: &Arc<GpuDevice>, kind: KernelKind) -> Result<&Kernel> {
        Ok(match self.0.entry(kind) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let bindings = vec![BindingKind::Buffer; kind.buffers() as usize];
                e.insert(Kernel::new(gpu, kind.spirv(), &bindings, kind.push_size())?)
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
    pub shape: [u32; 4],
    pub a_strides: [u32; 4],
    pub b_strides: [u32; 4],
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
    pub pad0: u32,
    pub shape: [u32; 4],
    pub src_strides: [i32; 4],
    pub dst_strides: [i32; 4],
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
    pub kept: [u32; 4],
    pub kept_strides: [u32; 4],
    pub reduced: [u32; 4],
    pub reduced_strides: [u32; 4],
}
