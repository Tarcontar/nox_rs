//! AI effects: mattes from nox-ai's background removal models, loaded as
//! clips first need them; which models is the model registry's choice for
//! the clip (still or video, fp16 or not), or the effect's "model"
//! parameter. Mattes are kept in GPU memory (`MatteCache`), so changing
//! what comes after the model doesn't run it again.

use nox_ai::models::{Conditions, Media, ModelInfo, Task, registry};
use nox_ai::{BackgroundRemover, Matte};
use nox_gpu::{GpuDevice, LayerPicture};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use crate::Context;

/// A model, loaded when a clip first needs it.
enum Loaded<T> {
    Unloaded,
    Ready(Box<T>),
    /// The model isn't there or doesn't load; not tried again until
    /// `retry`.
    Unavailable,
}

impl<T> Loaded<T> {
    /// The model, loaded with `new` if it isn't yet and its file is there.
    fn load(
        &mut self,
        model: &ModelInfo,
        new: impl FnOnce(&Path) -> nox_ai::Result<T>,
    ) -> Option<&mut T> {
        if let Self::Unloaded = self {
            let dir = nox_ai::models::default_dir();
            *self = if !model.is_available(&dir) {
                tracing::warn!("{} ({}) isn't in {}", model.name, model.file, dir.display());
                Self::Unavailable
            } else {
                match new(&model.path(&dir)) {
                    Ok(loaded) => Self::Ready(Box::new(loaded)),
                    Err(err) => {
                        tracing::warn!("{} unavailable: {err}", model.name);
                        Self::Unavailable
                    }
                }
            };
        }
        match self {
            Self::Ready(loaded) => Some(loaded),
            _ => None,
        }
    }

    fn retry(&mut self) {
        if let Self::Unavailable = self {
            *self = Self::Unloaded;
        }
    }
}

/// Mattes made, by what they depend on, least recently used dropped
/// first beyond a budget of GPU memory.
struct MatteCache {
    entries: HashMap<u64, (Arc<Matte>, u64)>,
    clock: u64,
    bytes: u64,
    budget: u64,
}

impl MatteCache {
    fn new() -> Self {
        let mb = std::env::var("NOX_MATTE_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(256);
        Self {
            entries: HashMap::new(),
            clock: 0,
            bytes: 0,
            budget: mb * 1_000_000,
        }
    }

    fn get(&mut self, key: u64) -> Option<Arc<Matte>> {
        self.clock += 1;
        let (matte, used) = self.entries.get_mut(&key)?;
        *used = self.clock;
        Some(matte.clone())
    }

    fn insert(&mut self, key: u64, matte: Arc<Matte>) {
        self.clock += 1;
        self.bytes += size(&matte);
        if let Some((old, _)) = self.entries.insert(key, (matte, self.clock)) {
            self.bytes -= size(&old);
        }
        while self.bytes > self.budget && self.entries.len() > 1 {
            let oldest = *self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key)
                .expect("entries");
            if let Some((old, _)) = self.entries.remove(&oldest) {
                self.bytes -= size(&old);
            }
        }
    }
}

fn size(matte: &Matte) -> u64 {
    u64::from(matte.width) * u64::from(matte.height) * 4
}

pub(crate) struct Ai {
    gpu: Arc<GpuDevice>,
    removers: HashMap<String, Loaded<BackgroundRemover>>,
    cache: MatteCache,
    /// How often a model ran (tests check the cache with it).
    pub runs: usize,
}

impl Ai {
    pub fn new(gpu: Arc<GpuDevice>) -> Self {
        Self {
            gpu,
            removers: HashMap::new(),
            cache: MatteCache::new(),
            runs: 0,
        }
    }

    /// Tries loading models again (e.g. after a download).
    pub fn reload(&mut self) {
        self.removers.values_mut().for_each(Loaded::retry);
    }

    /// The background removal models for `model` ("auto", "video",
    /// "still") on `context`'s clip that load, most preferred first.
    pub fn removers(&mut self, model: &str, context: &Context) -> Vec<String> {
        let media = match model {
            "video" => Media::Video,
            "still" => Media::Still,
            _ if context.frame.is_none() => Media::Still,
            _ => Media::Video,
        };
        let conditions = Conditions {
            media,
            f16: self.gpu.storage_16bit(),
        };
        let gpu = &self.gpu;
        registry()
            .candidates(Task::BackgroundRemoval, &conditions)
            .into_iter()
            .filter(|model| {
                self.removers
                    .entry(model.id.clone())
                    .or_insert(Loaded::Unloaded)
                    .load(model, |path| {
                        BackgroundRemover::new(gpu.clone(), model, path)
                    })
                    .is_some()
            })
            .map(|model| model.id.clone())
            .collect()
    }

    /// The subject's matte in `picture` (the clip's, at its frame), with
    /// the first of `models` that works, or from the cache: `upstream`
    /// identifies what made the picture. A model failing (e.g. short of
    /// GPU memory: BiRefNet needs gigabytes) falls back to the next; with
    /// none, there is no matte (the clip stays as it is rather than the
    /// frame unrendered).
    pub fn matte(
        &mut self,
        models: &[String],
        context: &Context,
        picture: &LayerPicture,
        upstream: u64,
    ) -> Option<Arc<Matte>> {
        let key = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (context.clip, context.frame, models.first(), upstream).hash(&mut hasher);
            (picture.width, picture.height).hash(&mut hasher);
            hasher.finish()
        };
        if let Some(matte) = self.cache.get(key) {
            return Some(matte);
        }
        for id in models {
            if let Some(Loaded::Ready(remover)) = self.removers.get_mut(id) {
                match remover.matte(context.clip, context.frame, picture) {
                    Ok(matte) => {
                        self.runs += 1;
                        let matte = Arc::new(matte);
                        self.cache.insert(key, matte.clone());
                        return Some(matte);
                    }
                    Err(err) => tracing::warn!("background removal with {id}: {err}"),
                }
            }
        }
        None
    }
}
