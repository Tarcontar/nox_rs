//! ONNX models as Nox runs them: the nodes in order, constants (weights
//! and Constant nodes), and the graph's inputs and outputs.

use std::collections::HashMap;
use std::path::Path;

use prost::Message;

use crate::onnx::{self, ModelProto, attribute_type};
use crate::tensor::Tensor;
use crate::{Error, Result};

/// A graph input: its name and dimensions (`None` where the model leaves
/// one open, e.g. the picture size).
#[derive(Debug, Clone)]
pub struct Input {
    pub name: String,
    pub dims: Vec<Option<usize>>,
}

#[derive(Debug, Clone)]
pub(crate) enum Attr {
    Float(f32),
    Int(i64),
    Floats(Vec<f32>),
    Ints(Vec<i64>),
    String(String),
    Tensor(Tensor),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub op: String,
    pub name: String,
    /// Empty names are inputs left out (optional ones).
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub attrs: HashMap<String, Attr>,
}

impl Node {
    pub fn int(&self, name: &str, default: i64) -> i64 {
        match self.attrs.get(name) {
            Some(Attr::Int(v)) => *v,
            _ => default,
        }
    }

    pub fn float(&self, name: &str, default: f32) -> f32 {
        match self.attrs.get(name) {
            Some(Attr::Float(v)) => *v,
            _ => default,
        }
    }

    pub fn ints(&self, name: &str) -> Option<&[i64]> {
        match self.attrs.get(name) {
            Some(Attr::Ints(v)) => Some(v),
            _ => None,
        }
    }

    pub fn string(&self, name: &str, default: &'static str) -> String {
        match self.attrs.get(name) {
            Some(Attr::String(v)) => v.clone(),
            _ => default.to_owned(),
        }
    }

    /// Says which node failed, for errors.
    pub fn describe(&self) -> String {
        if self.name.is_empty() {
            format!(
                "{} ({})",
                self.op,
                self.outputs.first().map_or("", String::as_str)
            )
        } else {
            format!("{} {}", self.op, self.name)
        }
    }
}

/// A loaded model.
#[derive(Debug, Clone)]
pub struct Model {
    pub(crate) nodes: Vec<Node>,
    pub(crate) constants: HashMap<String, Tensor>,
    pub inputs: Vec<Input>,
    pub outputs: Vec<String>,
    /// The default domain's operator set version.
    pub opset: i64,
}

impl Model {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let model = ModelProto::decode(bytes)
            .map_err(|err| Error::Model(format!("not an ONNX model: {err}")))?;
        let opset = model
            .opset_import
            .iter()
            .find(|o| o.domain.is_empty() || o.domain == "ai.onnx")
            .map_or(1, |o| o.version);
        let graph = model
            .graph
            .ok_or_else(|| Error::Model("the model has no graph".into()))?;
        let mut constants = HashMap::new();
        for init in &graph.initializer {
            constants.insert(init.name.clone(), Tensor::from_proto(init)?);
        }
        let inputs = graph
            .input
            .iter()
            .filter(|i| !constants.contains_key(&i.name))
            .map(|i| Input {
                name: i.name.clone(),
                dims: i
                    .r#type
                    .as_ref()
                    .and_then(|t| t.tensor_type.as_ref())
                    .and_then(|t| t.shape.as_ref())
                    .map(|s| {
                        s.dim
                            .iter()
                            .map(|d| match d.value {
                                Some(onnx::DimensionValue::DimValue(v)) if v > 0 => {
                                    Some(v as usize)
                                }
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        let mut nodes = Vec::new();
        for node in graph.node {
            if !(node.domain.is_empty() || node.domain == "ai.onnx") {
                return Err(Error::Unsupported(format!(
                    "operator {} of domain {}",
                    node.op_type, node.domain
                )));
            }
            let mut attrs = HashMap::new();
            for a in &node.attribute {
                let value = match a.r#type {
                    attribute_type::FLOAT => Attr::Float(a.f),
                    attribute_type::INT => Attr::Int(a.i),
                    attribute_type::FLOATS => Attr::Floats(a.floats.clone()),
                    attribute_type::INTS => Attr::Ints(a.ints.clone()),
                    attribute_type::STRING => {
                        Attr::String(String::from_utf8_lossy(&a.s).into_owned())
                    }
                    attribute_type::TENSOR => match &a.t {
                        Some(t) => Attr::Tensor(Tensor::from_proto(t)?),
                        None => continue,
                    },
                    // Graphs and others: no operator Nox runs uses them.
                    _ => continue,
                };
                attrs.insert(a.name.clone(), value);
            }
            let node = Node {
                op: node.op_type,
                name: node.name,
                inputs: node.input,
                outputs: node.output,
                attrs,
            };
            // Constant nodes are constants.
            if node.op == "Constant" {
                let value = match node.attrs.get("value") {
                    Some(Attr::Tensor(t)) => t.clone(),
                    Some(Attr::Float(v)) => Tensor::f32(vec![], vec![*v]),
                    Some(Attr::Int(v)) => Tensor::i64(vec![], vec![*v]),
                    Some(Attr::Floats(v)) => Tensor::f32(vec![v.len()], v.clone()),
                    Some(Attr::Ints(v)) => Tensor::i64(vec![v.len()], v.clone()),
                    _ => {
                        return Err(Error::Unsupported(format!(
                            "{} without a value",
                            node.describe()
                        )));
                    }
                };
                for output in &node.outputs {
                    constants.insert(output.clone(), value.clone());
                }
                continue;
            }
            nodes.push(node);
        }
        Ok(Self {
            nodes,
            constants,
            inputs,
            outputs: graph.output.into_iter().map(|o| o.name).collect(),
            opset,
        })
    }

    /// How many nodes of each operator the model has, most first.
    pub fn operators(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for node in &self.nodes {
            *counts.entry(node.op.as_str()).or_default() += 1;
        }
        let mut counts: Vec<_> = counts.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }
}
