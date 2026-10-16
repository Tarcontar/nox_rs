//! Background removal for stills with BiRefNet (Zheng et al. 2024, MIT):
//! a segmentation network at 1024 × 1024 that finds the picture's main
//! subject, with much finer edges than video matting and not only people.
//!
//! Its plan takes about 4.6 GB of GPU memory, so each picture plans,
//! runs once and lets it go; the compositor keeps stills it has matted.

use nox_gpu::{GpuDevice, LayerPicture};
use std::path::Path;
use std::sync::Arc;

use crate::bridge::{Bridge, Normalize};
use crate::{Error, Model, Precision, Result, Session};

/// The model's input size.
const SIDE: usize = 1024;
const INPUT: &str = "input_image";
const OUTPUT: &str = "output_image";

pub struct Segmentation {
    gpu: Arc<GpuDevice>,
    model: Model,
    bridge: Bridge,
}

impl Segmentation {
    /// Loads the BiRefNet model at `path` (an `.onnx` file).
    pub fn new(gpu: Arc<GpuDevice>, path: &Path) -> Result<Self> {
        let model = Model::load(path)?;
        if !model.inputs.iter().any(|i| i.name == INPUT)
            || !model.outputs.iter().any(|o| o == OUTPUT)
        {
            return Err(Error::Model(format!(
                "{}: not BiRefNet (no {INPUT} or {OUTPUT})",
                path.display()
            )));
        }
        Ok(Self {
            bridge: Bridge::new(gpu.clone(), Precision::for_model(&model, &gpu))?,
            gpu,
            model,
        })
    }

    /// Multiplies `picture`'s alpha by the matte of its subject.
    pub fn apply(&mut self, picture: &LayerPicture) -> Result<()> {
        let mut session = Session::new(self.gpu.clone(), self.model.clone());
        session.plan(&[(INPUT, &[1, 3, SIDE, SIDE])], &[], &[])?;
        tracing::info!(
            "still background removal planned ({:.0} MB, {:?}, {:?})",
            session.memory() as f64 / 1e6,
            session.precision(),
            if session.tensor_cores() {
                ", tensor cores"
            } else {
                ""
            }
        );
        let size = (SIDE as u32, SIDE as u32);
        self.bridge
            .to_tensor(picture, session.buffer(INPUT)?, size, Normalize::IMAGENET)?;
        session.run()?;
        if session.buffer_offset(OUTPUT)? != 0 {
            return Err(Error::Unsupported(format!(
                "{OUTPUT} inside another buffer"
            )));
        }
        self.bridge
            .apply_matte(picture, session.buffer(OUTPUT)?, size, true)
    }
}
