//! The models Nox can use, which one a task uses, and fetching them.
//!
//! The registry is a JSON file (`models.json` in this crate, built in),
//! read on startup: `NOX_MODEL_REGISTRY` names another, else
//! `nox/models.json` in the user's data folder is used if it's there. It
//! lists the models (each with a pinned URL and SHA-256, and the runner
//! that knows its inputs and outputs) and, per task, rules in order of
//! preference: a model and the conditions it's for. Which model a task
//! uses is worked out at run time (`Registry::candidates`), from what's
//! being processed and what's downloaded.
//!
//! Models are downloaded the first time an effect needs them (after the
//! user agrees), into a folder the caller chooses (the user's data
//! folder). Downloads go to a temporary file, renamed when complete and
//! verified.

use nox_project::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Result};

/// The registry built into Nox.
const BUILT_IN: &str = include_str!("../models.json");

/// A model Nox knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// For people.
    pub name: String,
    /// What runs it.
    pub runner: Runner,
    pub file: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    /// In bytes.
    pub size: u64,
    pub license: String,
}

/// The code that runs a model: what it feeds it and reads from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// Video matting with recurrent state (`Matting`, Robust Video
    /// Matting's inputs and outputs).
    Matting,
    /// One picture's subject at 1024 × 1024 (`Segmentation`, BiRefNet's).
    Segmentation,
}

impl Runner {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "matting" => Self::Matting,
            "segmentation" => Self::Segmentation,
            _ => return None,
        })
    }
}

/// What a model is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Task {
    BackgroundRemoval,
}

impl Task {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "background_removal" => Self::BackgroundRemoval,
            _ => return None,
        })
    }
}

/// What's being processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Still,
    Video,
}

impl Media {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "still" => Self::Still,
            "video" => Self::Video,
            _ => return None,
        })
    }
}

/// The facts rules are checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conditions {
    pub media: Media,
    /// Whether the GPU can run fp16 models in 16 bits (`Precision::F16`:
    /// it keeps 16-bit floats).
    pub f16: bool,
}

/// A model for a task, if the conditions match (`None`: any).
#[derive(Debug, Clone)]
struct Rule {
    model: usize,
    media: Option<Vec<Media>>,
    f16: Option<bool>,
}

impl Rule {
    fn matches(&self, conditions: &Conditions) -> bool {
        self.media
            .as_ref()
            .is_none_or(|media| media.contains(&conditions.media))
            && self.f16.is_none_or(|f16| f16 == conditions.f16)
    }
}

/// The models and the rules for picking them.
#[derive(Debug, Clone)]
pub struct Registry {
    models: Vec<ModelInfo>,
    tasks: HashMap<Task, Vec<Rule>>,
}

impl Registry {
    /// The registry in `text` (JSON), checked: known runners, tasks and
    /// conditions, unique ids, and rules naming listed models.
    pub fn parse(text: &str) -> Result<Self> {
        let bad = |what: String| Error::Registry(what);
        let root = nox_otio::json::parse(text).map_err(|e| bad(e.to_string()))?;
        let root = root
            .as_object()
            .ok_or_else(|| bad("not an object".into()))?;
        let field = |object: &nox_project::Object, key: &str, at: &str| -> Result<Value> {
            object
                .get(key)
                .cloned()
                .ok_or_else(|| bad(format!("{at}: no \"{key}\"")))
        };
        let string = |object: &nox_project::Object, key: &str, at: &str| -> Result<String> {
            field(object, key, at)?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("{at}: \"{key}\" isn't a string")))
        };
        let mut models: Vec<ModelInfo> = Vec::new();
        let list = field(root, "models", "registry")?;
        for (i, entry) in list
            .as_array()
            .ok_or_else(|| bad("\"models\" isn't a list".into()))?
            .iter()
            .enumerate()
        {
            let at = format!("model {i}");
            let entry = entry
                .as_object()
                .ok_or_else(|| bad(format!("{at} isn't an object")))?;
            let id = string(entry, "id", &at)?;
            let at = format!("model {id}");
            if models.iter().any(|m| m.id == id) {
                return Err(bad(format!("{at} is listed twice")));
            }
            let runner = string(entry, "runner", &at)?;
            let sha256 = string(entry, "sha256", &at)?;
            if sha256.len() != 64
                || !sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(bad(format!(
                    "{at}: \"sha256\" isn't 64 lowercase hex digits"
                )));
            }
            let file = string(entry, "file", &at)?;
            if file.is_empty() || file.contains(['/', '\\']) || file.starts_with('.') {
                return Err(bad(format!("{at}: \"file\" must be a plain file name")));
            }
            models.push(ModelInfo {
                name: string(entry, "name", &at)?,
                runner: Runner::parse(&runner)
                    .ok_or_else(|| bad(format!("{at}: unknown runner \"{runner}\"")))?,
                file,
                url: string(entry, "url", &at)?,
                sha256,
                size: field(entry, "size", &at)?
                    .as_i64()
                    .and_then(|s| u64::try_from(s).ok())
                    .ok_or_else(|| bad(format!("{at}: \"size\" isn't a byte count")))?,
                license: string(entry, "license", &at)?,
                id,
            });
        }
        let mut tasks = HashMap::new();
        let table = field(root, "tasks", "registry")?;
        for (name, rules) in table
            .as_object()
            .ok_or_else(|| bad("\"tasks\" isn't an object".into()))?
            .iter()
        {
            let task = Task::parse(name).ok_or_else(|| bad(format!("unknown task \"{name}\"")))?;
            let mut parsed = Vec::new();
            for (i, rule) in rules
                .as_array()
                .ok_or_else(|| bad(format!("task {name} isn't a list")))?
                .iter()
                .enumerate()
            {
                let at = format!("task {name}, rule {i}");
                let rule = rule
                    .as_object()
                    .ok_or_else(|| bad(format!("{at} isn't an object")))?;
                let id = string(rule, "model", &at)?;
                let model = models
                    .iter()
                    .position(|m| m.id == id)
                    .ok_or_else(|| bad(format!("{at}: no model \"{id}\"")))?;
                let mut media = None;
                let mut f16 = None;
                if let Some(when) = rule.get("when") {
                    let when = when
                        .as_object()
                        .ok_or_else(|| bad(format!("{at}: \"when\" isn't an object")))?;
                    for (key, value) in when.iter() {
                        match key {
                            "media" => {
                                // One, or a list.
                                let names: Vec<&Value> = match value.as_array() {
                                    Some(list) => list.iter().collect(),
                                    None => vec![value],
                                };
                                media = Some(
                                    names
                                        .into_iter()
                                        .map(|v| v.as_str().and_then(Media::parse))
                                        .collect::<Option<Vec<_>>>()
                                        .ok_or_else(|| {
                                            bad(format!(
                                                "{at}: \"media\" is \"still\" or \"video\""
                                            ))
                                        })?,
                                );
                            }
                            "f16" => {
                                f16 = Some(value.as_bool().ok_or_else(|| {
                                    bad(format!("{at}: \"f16\" is true or false"))
                                })?);
                            }
                            other => {
                                return Err(bad(format!("{at}: unknown condition \"{other}\"")));
                            }
                        }
                    }
                }
                parsed.push(Rule { model, media, f16 });
            }
            tasks.insert(task, parsed);
        }
        Ok(Self { models, tasks })
    }

    /// The registry built into Nox.
    pub fn built_in() -> Self {
        Self::parse(BUILT_IN).expect("the built-in registry is valid")
    }

    /// Every model listed.
    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The models for `task` under `conditions`, most preferred first:
    /// use the first that's downloaded and loads, the next if it fails.
    pub fn candidates(&self, task: Task, conditions: &Conditions) -> Vec<&ModelInfo> {
        let mut found: Vec<&ModelInfo> = Vec::new();
        for rule in self.tasks.get(&task).into_iter().flatten() {
            let model = &self.models[rule.model];
            if rule.matches(conditions) && !found.contains(&model) {
                found.push(model);
            }
        }
        found
    }

    /// What to download for `task` under `conditions`: the most preferred
    /// candidate of each runner (e.g. a still's segmentation model, and
    /// the matting model as its fallback), not every variant.
    pub fn preferred(&self, task: Task, conditions: &Conditions) -> Vec<&ModelInfo> {
        let mut found: Vec<&ModelInfo> = Vec::new();
        for model in self.candidates(task, conditions) {
            if !found.iter().any(|m| m.runner == model.runner) {
                found.push(model);
            }
        }
        found
    }
}

/// The registry, read the first time it's asked for (Nox asks on
/// startup). A registry file that doesn't read or parse is reported and
/// the built-in one used.
pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let path = std::env::var_os("NOX_MODEL_REGISTRY")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(data_dir().join("models.json")).filter(|p| p.is_file()));
        let Some(path) = path else {
            return Registry::built_in();
        };
        let read = std::fs::read_to_string(&path)
            .map_err(|e| Error::Registry(e.to_string()))
            .and_then(|text| Registry::parse(&text));
        match read {
            Ok(registry) => {
                tracing::info!("model registry from {}", path.display());
                registry
            }
            Err(err) => {
                tracing::warn!(
                    "{}: {err}; using the built-in model registry",
                    path.display()
                );
                Registry::built_in()
            }
        }
    })
}

/// Nox's folder in the user's data folder (`$XDG_DATA_HOME` or
/// `~/.local/share` on Linux and in the Flatpak, `%LOCALAPPDATA%` on
/// Windows, `~/Library/Application Support` on macOS).
fn data_dir() -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let data = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/share"))
    };
    data.join("nox")
}

/// Where Nox keeps downloaded models: `NOX_MODELS_DIR` if set, else
/// `models` in Nox's data folder.
pub fn default_dir() -> PathBuf {
    match std::env::var_os("NOX_MODELS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => data_dir().join("models"),
    }
}

impl ModelInfo {
    /// Where the model goes in `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.file)
    }

    /// Whether it's in `dir` (of the right size; the hash is checked when
    /// downloading).
    pub fn is_available(&self, dir: &Path) -> bool {
        std::fs::metadata(self.path(dir)).is_ok_and(|m| m.len() == self.size)
    }

    /// Downloads the model into `dir` unless it's there, calling
    /// `progress(done, total)` in bytes as it goes, and stopping with
    /// `Error::Cancelled` once `cancel` is set. Returns its path.
    pub fn download(
        &self,
        dir: &Path,
        mut progress: impl FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let path = self.path(dir);
        if self.is_available(dir) {
            return Ok(path);
        }
        let io = |source: std::io::Error, path: &Path| Error::Io {
            path: path.to_owned(),
            source,
        };
        std::fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
        let partial = dir.join(format!("{}.part", self.file));
        let mut response = ureq::get(&self.url)
            .call()
            .map_err(|e| Error::Download(format!("{}: {e}", self.url)))?;
        let mut body = response.body_mut().as_reader();
        let mut file = std::fs::File::create(&partial).map_err(|e| io(e, &partial))?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut chunk = vec![0u8; 1 << 16];
        let result = loop {
            if cancel.load(Ordering::Relaxed) {
                break Err(Error::Cancelled);
            }
            let n = match body.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) => break Err(Error::Download(format!("{}: {e}", self.url))),
            };
            hasher.update(&chunk[..n]);
            if let Err(e) = file.write_all(&chunk[..n]) {
                break Err(io(e, &partial));
            }
            done += n as u64;
            progress(done, self.size);
        };
        drop(file);
        let checked = result.and_then(|()| {
            let hash: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if hash == self.sha256 && done == self.size {
                Ok(())
            } else {
                Err(Error::Download(format!(
                    "{} isn't the expected file ({done} bytes, SHA-256 {hash})",
                    self.url
                )))
            }
        });
        match checked {
            Ok(()) => {
                std::fs::rename(&partial, &path).map_err(|e| io(e, &path))?;
                tracing::info!("downloaded {} to {}", self.name, path.display());
                Ok(path)
            }
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STILL: Conditions = Conditions {
        media: Media::Still,
        f16: false,
    };
    const VIDEO: Conditions = Conditions {
        media: Media::Video,
        f16: false,
    };
    const STILL_F16: Conditions = Conditions {
        media: Media::Still,
        f16: true,
    };
    const VIDEO_F16: Conditions = Conditions {
        media: Media::Video,
        f16: true,
    };

    fn ids(models: Vec<&ModelInfo>) -> Vec<&str> {
        models.into_iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn built_in_rules() {
        let registry = Registry::built_in();
        let task = Task::BackgroundRemoval;
        assert_eq!(
            ids(registry.candidates(task, &STILL)),
            ["birefnet_lite", "rvm_mobilenetv3"]
        );
        assert_eq!(ids(registry.candidates(task, &VIDEO)), ["rvm_mobilenetv3"]);
        // fp16 first where the GPU runs it, the fp32 files as fallbacks.
        assert_eq!(
            ids(registry.candidates(task, &STILL_F16)),
            [
                "birefnet_lite_fp16",
                "birefnet_lite",
                "rvm_mobilenetv3_fp16",
                "rvm_mobilenetv3"
            ]
        );
        assert_eq!(
            ids(registry.candidates(task, &VIDEO_F16)),
            ["rvm_mobilenetv3_fp16", "rvm_mobilenetv3"]
        );
        // One of each runner to download.
        assert_eq!(
            ids(registry.preferred(task, &STILL_F16)),
            ["birefnet_lite_fp16", "rvm_mobilenetv3_fp16"]
        );
        assert_eq!(ids(registry.preferred(task, &VIDEO)), ["rvm_mobilenetv3"]);
        let rvm = registry.get("rvm_mobilenetv3").unwrap();
        assert_eq!(rvm.runner, Runner::Matting);
        assert_eq!(rvm.size, 14_975_696);
    }

    fn model(id: &str) -> String {
        format!(
            r#"{{"id": "{id}", "name": "N", "runner": "matting", "file": "{id}.onnx",
                "url": "https://example.org/{id}", "sha256": "{}", "size": 1, "license": "MIT"}}"#,
            "0".repeat(64)
        )
    }

    #[test]
    fn rules_in_order_with_lists() {
        let text = format!(
            r#"{{"models": [{}, {}], "tasks": {{"background_removal": [
                {{"model": "a", "when": {{"media": ["video"]}}}},
                {{"model": "b"}},
                {{"model": "a"}}]}}}}"#,
            model("a"),
            model("b")
        );
        let registry = Registry::parse(&text).unwrap();
        assert_eq!(
            ids(registry.candidates(Task::BackgroundRemoval, &VIDEO)),
            ["a", "b"]
        );
        assert_eq!(
            ids(registry.candidates(Task::BackgroundRemoval, &STILL)),
            ["b", "a"]
        );
    }

    #[test]
    fn mistakes_are_reported() {
        let with = |models: &str, rules: &str| {
            let text = format!(
                r#"{{"models": [{models}], "tasks": {{"background_removal": [{rules}]}}}}"#
            );
            Registry::parse(&text).unwrap_err().to_string()
        };
        let a = model("a");
        assert!(with(&a, r#"{"model": "x"}"#).contains("no model \"x\""));
        assert!(with(&format!("{a}, {a}"), "").contains("listed twice"));
        assert!(with(&a, r#"{"model": "a", "when": {"gpu": 1}}"#).contains("unknown condition"));
        assert!(with(&a, r#"{"model": "a", "when": {"media": "audio"}}"#).contains("\"still\""));
        assert!(with(&a, r#"{"model": "a", "when": {"f16": "yes"}}"#).contains("true or false"));
        assert!(with(&a.replace("matting", "magic"), "").contains("unknown runner"));
        assert!(with(&a.replace("a.onnx", "../a.onnx"), "").contains("plain file name"));
        assert!(
            Registry::parse(r#"{"models": [], "tasks": {"upscale": []}}"#)
                .unwrap_err()
                .to_string()
                .contains("unknown task")
        );
    }
}
