//! Between compositor layers and models: a layer picture into a model's
//! input tensor (`layer_to_rgb.comp`), and a matte back into the
//! picture's alpha (`apply_matte.comp`), on the GPU.

use nox_gpu::compute::{Binding, BindingKind, Buffer, Kernel, Recording};
use nox_gpu::{GpuDevice, LayerPicture};
use std::sync::Arc;

use crate::kernels::groups;
use crate::{Precision, Result};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ToRgbPush {
    rows: [[f32; 4]; 3],
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
    mean: [f32; 4],
    inv_std: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ApplyPush {
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
    logits: u32,
}

/// How a model wants its input: sRGB in [0, 1], then `(v - mean) / std`
/// per channel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Normalize {
    pub mean: [f32; 3],
    pub std: [f32; 3],
}

impl Normalize {
    /// sRGB in [0, 1] as it is.
    pub const NONE: Self = Self {
        mean: [0.0; 3],
        std: [1.0; 3],
    };
    /// ImageNet's statistics, which models trained on it expect.
    pub const IMAGENET: Self = Self {
        mean: [0.485, 0.456, 0.406],
        std: [0.229, 0.224, 0.225],
    };
}

pub(crate) struct Bridge {
    gpu: Arc<GpuDevice>,
    to_rgb: Kernel,
    apply: Kernel,
    /// The working space to linear sRGB primaries.
    rows: [[f32; 4]; 3],
}

impl Bridge {
    /// For tensors kept in `precision`.
    pub fn new(gpu: Arc<GpuDevice>, precision: Precision) -> Result<Self> {
        macro_rules! spv {
            ($name:literal) => {
                match precision {
                    Precision::F32 => {
                        &include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".comp.spv"))[..]
                    }
                    Precision::F16 => {
                        &include_bytes!(concat!(env!("OUT_DIR"), "/", $name, "_f16.comp.spv"))[..]
                    }
                }
            };
        }
        let image_and_buffer = [BindingKind::Image, BindingKind::Buffer];
        let to_rgb = Kernel::new(
            &gpu,
            spv!("layer_to_rgb"),
            &image_and_buffer,
            size_of::<ToRgbPush>(),
        )?;
        let apply = Kernel::new(
            &gpu,
            spv!("apply_matte"),
            &image_and_buffer,
            size_of::<ApplyPush>(),
        )?;
        let m = nox_color::WORKING_PRIMARIES.conversion_to(nox_color::Primaries::Bt709);
        let rows = [0, 1, 2].map(|r| [m[r][0], m[r][1], m[r][2], 0.0]);
        Ok(Self {
            gpu,
            to_rgb,
            apply,
            rows,
        })
    }

    /// Writes `picture` into `tensor` as planar RGB of `width` × `height`.
    //
    // Recorded each time: the compositor's layer images come and go (a
    // recording kept would point at a freed image).
    pub fn to_tensor(
        &self,
        picture: &LayerPicture,
        tensor: &Buffer,
        (width, height): (u32, u32),
        normalize: Normalize,
    ) -> Result<()> {
        let mut recording = Recording::new(&self.gpu, 1, 2)?;
        let [m0, m1, m2] = normalize.mean;
        let [s0, s1, s2] = normalize.std;
        let push = ToRgbPush {
            rows: self.rows,
            width,
            height,
            layer_width: picture.width,
            layer_height: picture.height,
            mean: [m0, m1, m2, 0.0],
            inv_std: [1.0 / s0, 1.0 / s1, 1.0 / s2, 0.0],
        };
        recording.dispatch(
            &self.to_rgb,
            &[Binding::Image(picture.view), tensor.into()],
            bytemuck::bytes_of(&push),
            groups(width * height),
        )?;
        recording.run()?;
        Ok(())
    }

    /// Multiplies `picture`'s alpha by `matte`, one plane of `width` ×
    /// `height` (of logits if `logits`).
    pub fn apply_matte(
        &self,
        picture: &LayerPicture,
        matte: &Buffer,
        (width, height): (u32, u32),
        logits: bool,
    ) -> Result<()> {
        let mut recording = Recording::new(&self.gpu, 1, 2)?;
        let push = ApplyPush {
            width,
            height,
            layer_width: picture.width,
            layer_height: picture.height,
            logits: logits.into(),
        };
        recording.dispatch(
            &self.apply,
            &[Binding::Image(picture.view), matte.into()],
            bytemuck::bytes_of(&push),
            groups(picture.width * picture.height),
        )?;
        recording.run()?;
        Ok(())
    }
}
