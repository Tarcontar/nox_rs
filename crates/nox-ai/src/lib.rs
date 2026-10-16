//! AI effects for Nox: ONNX models run as Vulkan compute shaders on the
//! device Nox decodes, composites and encodes with, so frames never leave
//! the GPU (docs/AI_PLAN.md).
//!
//! - `Model` loads an `.onnx` file (`onnx`: the protobuf schema).
//! - `cpu` runs operators on the CPU: planning works out shapes and
//!   constant subgraphs with it, and it is the reference for the kernels.
//! - `Session` plans a model for given input sizes and runs it on the GPU
//!   (`kernels`: the compute shaders).
//! - `Model::run_cpu` runs a whole graph on the CPU (tests, and models too
//!   small to be worth the GPU).

mod bridge;
mod cpu;
mod kernels;
mod matting;
mod model;
pub mod models;
/// ONNX's protobuf schema, for building models in tests.
#[doc(hidden)]
pub mod onnx;
mod remover;
mod segmentation;
mod session;
mod tensor;

use std::collections::HashMap;
use std::path::PathBuf;

pub use kernels::{MatrixKernels, Precision};
pub use matting::Matting;
pub use model::{Input, Model};
pub use remover::BackgroundRemover;
pub use segmentation::Segmentation;
pub use session::Session;
pub use tensor::{Data, Tensor};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("model: {0}")]
    Model(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Gpu(#[from] nox_gpu::Error),
    #[error("download: {0}")]
    Download(String),
    #[error("model registry: {0}")]
    Registry(String),
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Model {
    /// Runs the graph on the CPU with `inputs` by name, returning every
    /// graph output by name.
    pub fn run_cpu(&self, inputs: &[(&str, Tensor)]) -> Result<HashMap<String, Tensor>> {
        let mut values: HashMap<&str, Tensor> = HashMap::new();
        for (name, tensor) in inputs {
            values.insert(name, tensor.clone());
        }
        for node in &self.nodes {
            let args: Vec<Option<&Tensor>> = node
                .inputs
                .iter()
                .map(|name| {
                    if name.is_empty() {
                        None
                    } else {
                        values
                            .get(name.as_str())
                            .or_else(|| self.constants.get(name))
                    }
                })
                .collect();
            if let Some(missing) = node
                .inputs
                .iter()
                .zip(&args)
                .find(|(name, arg)| !name.is_empty() && arg.is_none())
            {
                return Err(Error::Model(format!(
                    "{} needs {}, which nothing computes",
                    node.describe(),
                    missing.0
                )));
            }
            let outputs = cpu::run(node, &args)?;
            for (name, tensor) in node.outputs.iter().zip(outputs) {
                values.insert(name, tensor);
            }
        }
        self.outputs
            .iter()
            .map(|name| {
                values
                    .remove(name.as_str())
                    .or_else(|| self.constants.get(name).cloned())
                    .map(|t| (name.clone(), t))
                    .ok_or_else(|| Error::Model(format!("nothing computes output {name}")))
            })
            .collect()
    }
}
