//! Background removal with whichever model the registry picks
//! (`models::Registry::candidates`): the model's runner does the work.

use nox_gpu::{GpuDevice, LayerPicture};
use std::path::Path;
use std::sync::Arc;

use crate::models::{ModelInfo, Runner};
use crate::{Matte, Matting, Result, Segmentation};

pub enum BackgroundRemover {
    Matting(Matting),
    Segmentation(Segmentation),
}

impl BackgroundRemover {
    /// Loads `model` from `path` for its runner.
    pub fn new(gpu: Arc<GpuDevice>, model: &ModelInfo, path: &Path) -> Result<Self> {
        Ok(match model.runner {
            Runner::Matting => Self::Matting(Matting::new(gpu, path)?),
            Runner::Segmentation => Self::Segmentation(Segmentation::new(gpu, path)?),
        })
    }

    /// Writes the matte of clip `key` into `picture`'s alpha: of frame
    /// `frame` of the clip's media, or of a still (`None`).
    pub fn apply(&mut self, key: u64, frame: Option<i64>, picture: &LayerPicture) -> Result<()> {
        match self {
            Self::Matting(matting) => matting.apply(key, frame, picture),
            // Each picture on its own.
            Self::Segmentation(segmentation) => segmentation.apply(picture),
        }
    }

    /// The matte `apply` would use, as a `Matte` of its own.
    pub fn matte(&mut self, key: u64, frame: Option<i64>, picture: &LayerPicture) -> Result<Matte> {
        match self {
            Self::Matting(matting) => matting.matte(key, frame, picture),
            Self::Segmentation(segmentation) => segmentation.matte(picture),
        }
    }
}
