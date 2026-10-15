//! Background removal with Robust Video Matting (RVM; Lin et al. 2021,
//! GPL-3.0): a recurrent network for video, which keeps state from frame
//! to frame, so mattes are steady.
//!
//! Each clip gets a planned `Session` of its own (its state is the
//! clip's). The compositor's picture of the clip goes in as sRGB
//! (`layer_to_rgb.comp`), and the matte comes out into the picture's alpha
//! (`apply_matte.comp`), on the GPU throughout. State starts over when a
//! frame isn't the one after the last (seeking, scrubbing).

use nox_gpu::compute::{Binding, BindingKind, Kernel, Recording};
use nox_gpu::{GpuDevice, LayerPicture};
use std::path::Path;
use std::sync::Arc;

use crate::kernels::groups;
use crate::{Error, Model, Result, Session, Tensor};

/// Longest side the model sees; bigger pictures are scaled down for it
/// and the matte scaled up (the refiner at full 4K would take gigabytes).
const MAX_SIDE: u32 = 1920;
/// The internal (coarse) pass works at about this many pixels on the long
/// side, as RVM recommends (0.25 for HD, 0.125 for 4K).
const COARSE_SIDE: f32 = 512.0;
/// Clips whose sessions are kept.
const MAX_CLIPS: usize = 4;
/// Runs on a still, so the recurrent state settles.
const STILL_RUNS: usize = 3;
/// The recurrent state: inputs and the outputs fed back into them.
const STATE: [(&str, &str); 4] = [
    ("r1o", "r1i"),
    ("r2o", "r2i"),
    ("r3o", "r3i"),
    ("r4o", "r4i"),
];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ToRgbPush {
    rows: [[f32; 4]; 3],
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ApplyPush {
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
}

/// A clip's session and where it is.
struct Clip {
    key: u64,
    /// The layer picture's size.
    layer: (u32, u32),
    /// The frame expected next (state carries over), if any.
    next: Option<i64>,
    used: u64,
    session: Session,
}

pub struct Matting {
    gpu: Arc<GpuDevice>,
    model: Model,
    clips: Vec<Clip>,
    to_rgb: Kernel,
    apply: Kernel,
    /// The working space to linear sRGB primaries.
    rows: [[f32; 4]; 3],
    clock: u64,
}

impl Matting {
    /// Loads the RVM model at `path` (an `.onnx` file).
    pub fn new(gpu: Arc<GpuDevice>, path: &Path) -> Result<Self> {
        let model = Model::load(path)?;
        for name in ["src", "r1i", "r2i", "r3i", "r4i", "downsample_ratio"] {
            if !model.inputs.iter().any(|i| i.name == name) {
                return Err(Error::Model(format!(
                    "{}: not Robust Video Matting (no input {name})",
                    path.display()
                )));
            }
        }
        let image_and_buffer = [BindingKind::Image, BindingKind::Buffer];
        let to_rgb = Kernel::new(
            &gpu,
            include_bytes!(concat!(env!("OUT_DIR"), "/layer_to_rgb.comp.spv")),
            &image_and_buffer,
            size_of::<ToRgbPush>(),
        )?;
        let apply = Kernel::new(
            &gpu,
            include_bytes!(concat!(env!("OUT_DIR"), "/apply_matte.comp.spv")),
            &image_and_buffer,
            size_of::<ApplyPush>(),
        )?;
        let m = nox_color::WORKING_PRIMARIES.conversion_to(nox_color::Primaries::Bt709);
        let rows = [0, 1, 2].map(|r| [m[r][0], m[r][1], m[r][2], 0.0]);
        Ok(Self {
            gpu,
            model,
            clips: Vec::new(),
            to_rgb,
            apply,
            rows,
            clock: 0,
        })
    }

    /// The model's input size for a picture of `width` × `height`.
    fn model_size(width: u32, height: u32) -> (u32, u32) {
        let scale = (MAX_SIDE as f32 / width.max(height) as f32).min(1.0);
        let fit = |v: u32| ((v as f32 * scale).round() as u32).max(1);
        (fit(width), fit(height))
    }

    /// Plans `model` for pictures of `width` × `height` with its state fed
    /// back, all state zero.
    fn plan(&self, width: u32, height: u32) -> Result<Session> {
        let mut session = Session::new(self.gpu.clone(), self.model.clone());
        let (w, h) = (width as usize, height as usize);
        let ratio = (COARSE_SIDE / width.max(height) as f32).min(1.0);
        let constants = [("downsample_ratio", Tensor::f32(vec![1], vec![ratio]))];
        // A first plan with empty state tells the state's shapes...
        let src = [1, 3, h, w];
        let empty = [1usize, 1, 1, 1];
        let mut shapes: Vec<(&str, &[usize])> = vec![("src", &src)];
        shapes.extend(STATE.iter().map(|&(_, input)| (input, &empty[..])));
        session.plan(&shapes, &constants, &[])?;
        let state: Vec<(&str, Vec<usize>)> = STATE
            .iter()
            .map(|&(output, input)| {
                session
                    .shape(output)
                    .map(|s| (input, s))
                    .ok_or_else(|| Error::Model(format!("no output {output}")))
            })
            .collect::<Result<_>>()?;
        // ...for the real one.
        let mut shapes: Vec<(&str, &[usize])> = vec![("src", &src)];
        shapes.extend(state.iter().map(|(n, s)| (*n, s.as_slice())));
        session.plan(&shapes, &constants, &STATE)?;
        tracing::info!(
            "background removal planned for {width}×{height} ({:.0} MB)",
            session.memory() as f64 / 1e6
        );
        Ok(session)
    }

    /// Zeroes `session`'s recurrent state.
    fn reset(session: &mut Session) -> Result<()> {
        for (_, input) in STATE {
            let n = session.shape(input).map_or(0, |s| s.iter().product());
            session.write(input, &vec![0.0; n])?;
        }
        Ok(())
    }

    /// Writes the matte of clip `key` into `picture`'s alpha: of frame
    /// `frame` (in the clip's media), or of a still (`None`).
    pub fn apply(&mut self, key: u64, frame: Option<i64>, picture: &LayerPicture) -> Result<()> {
        self.clock += 1;
        let layer = (picture.width, picture.height);
        let index = match self
            .clips
            .iter()
            .position(|c| c.key == key && c.layer == layer)
        {
            Some(index) => index,
            None => {
                let (w, h) = Self::model_size(layer.0, layer.1);
                let session = self.plan(w, h)?;
                if self.clips.len() >= MAX_CLIPS {
                    let oldest = (0..self.clips.len())
                        .min_by_key(|&i| self.clips[i].used)
                        .expect("clips");
                    self.clips.swap_remove(oldest);
                }
                self.clips.push(Clip {
                    key,
                    layer,
                    next: None,
                    used: 0,
                    session,
                });
                self.clips.len() - 1
            }
        };
        let clip = &mut self.clips[index];
        clip.used = self.clock;
        let runs = match frame {
            Some(frame) if clip.next == Some(frame) => 1,
            Some(_) => {
                Self::reset(&mut clip.session)?;
                1
            }
            None => {
                Self::reset(&mut clip.session)?;
                STILL_RUNS
            }
        };
        clip.next = frame.map(|f| f + 1);

        let src = clip.session.shape("src").expect("planned");
        let (w, h) = (src[3] as u32, src[2] as u32);
        // Recorded each time: the compositor's layer images come and go
        // (a recording kept would point at a freed image).
        let mut to_rgb = Recording::new(&self.gpu, 1, 2)?;
        let push = ToRgbPush {
            rows: self.rows,
            width: w,
            height: h,
            layer_width: layer.0,
            layer_height: layer.1,
        };
        to_rgb.dispatch(
            &self.to_rgb,
            &[
                Binding::Image(picture.view),
                clip.session.buffer("src")?.into(),
            ],
            bytemuck::bytes_of(&push),
            groups(w * h),
        )?;
        to_rgb.run()?;
        for _ in 0..runs {
            clip.session.run()?;
        }
        let mut apply = Recording::new(&self.gpu, 1, 2)?;
        let push = ApplyPush {
            width: w,
            height: h,
            layer_width: layer.0,
            layer_height: layer.1,
        };
        apply.dispatch(
            &self.apply,
            &[
                Binding::Image(picture.view),
                clip.session.buffer("pha")?.into(),
            ],
            bytemuck::bytes_of(&push),
            groups(layer.0 * layer.1),
        )?;
        apply.run()?;
        Ok(())
    }
}
