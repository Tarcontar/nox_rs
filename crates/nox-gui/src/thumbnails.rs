//! Filmstrips for the timeline's video clips: thumbnails along each clip,
//! decoded on a background thread (`nox_media::ThumbnailReader`, a few
//! files kept open) and kept in memory as textures.
//!
//! Thumbnails are asked for by file and time in milliseconds; the timeline
//! rounds times to a step that grows with the zoom, so zooming reuses them.
//! The newest requests are decoded first (what's on screen now), and old
//! ones are dropped when many pile up.

use gtk::{gdk, glib, prelude::*};
use nox_media::{Image, ThumbnailReader};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};

/// Height thumbnails are decoded at: twice the filmstrip's, for HiDPI.
const DECODE_HEIGHT: u32 = 64;
/// Files kept open on the worker.
const OPEN_FILES: usize = 4;
/// Requests waiting beyond this are dropped, oldest first.
const MAX_QUEUED: usize = 256;
/// Thumbnails kept; beyond, all are dropped and decoded again as needed.
const MAX_KEPT: usize = 4000;

type Key = (PathBuf, i64);

#[derive(Default)]
struct Queue {
    requests: VecDeque<Key>,
    pending: HashSet<Key>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct File {
    pictures: BTreeMap<i64, gdk::Texture>,
    /// Width over height, once a picture is in.
    aspect: Option<f32>,
    /// Doesn't open or decode; not asked for again.
    failed: bool,
}

/// A decoded thumbnail, or why there's none for the whole file.
enum Decoded {
    Picture(Key, Image),
    Failed(PathBuf),
}

pub struct Thumbnails {
    shared: Arc<Shared>,
    files: RefCell<HashMap<PathBuf, File>>,
    kept: std::cell::Cell<usize>,
}

impl Thumbnails {
    /// Starts the worker; `on_ready` is called when thumbnails arrive.
    pub fn new(on_ready: impl Fn() + 'static) -> Rc<Self> {
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = async_channel::unbounded();
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("nox-thumbnails".into())
            .spawn(move || work(&worker, &sender))
            .expect("failed to spawn the thumbnail thread");
        let thumbnails = Rc::new(Self {
            shared,
            files: RefCell::default(),
            kept: std::cell::Cell::new(0),
        });
        let weak = Rc::downgrade(&thumbnails);
        glib::spawn_future_local(async move {
            while let Ok(decoded) = receiver.recv().await {
                let Some(thumbnails) = weak.upgrade() else {
                    break;
                };
                thumbnails.arrived(decoded);
                // Draw once per batch, not once per thumbnail.
                if receiver.is_empty() {
                    on_ready();
                }
            }
        });
        thumbnails
    }

    fn arrived(&self, decoded: Decoded) {
        let mut files = self.files.borrow_mut();
        match decoded {
            Decoded::Picture((path, ms), image) => {
                if self.kept.get() >= MAX_KEPT {
                    for file in files.values_mut() {
                        file.pictures.clear();
                    }
                    self.kept.set(0);
                }
                let file = files.entry(path).or_default();
                file.aspect = Some(image.width as f32 / image.height.max(1) as f32);
                let bytes = glib::Bytes::from_owned(image.data);
                let texture = gdk::MemoryTexture::new(
                    image.width as i32,
                    image.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    image.stride,
                );
                if file.pictures.insert(ms, texture.upcast()).is_none() {
                    self.kept.set(self.kept.get() + 1);
                }
            }
            Decoded::Failed(path) => files.entry(path).or_default().failed = true,
        }
    }

    /// Width over height of `path`'s pictures (16:9 until one is in).
    pub fn aspect(&self, path: &Path) -> f32 {
        self.files
            .borrow()
            .get(path)
            .and_then(|f| f.aspect)
            .unwrap_or(16.0 / 9.0)
    }

    /// Whether `path` can't give thumbnails.
    pub fn failed(&self, path: &Path) -> bool {
        self.files.borrow().get(path).is_some_and(|f| f.failed)
    }

    /// The thumbnail of `path` at `ms`; if it's not decoded yet, asks for
    /// it and returns the nearest one there is, if any.
    pub fn get(&self, path: &Path, ms: i64) -> Option<gdk::Texture> {
        let files = self.files.borrow();
        let file = files.get(path);
        if let Some(texture) = file.and_then(|f| f.pictures.get(&ms)) {
            return Some(texture.clone());
        }
        if file.is_some_and(|f| f.failed) {
            return None;
        }
        self.request((path.to_owned(), ms));
        let pictures = &file?.pictures;
        let before = pictures.range(..ms).next_back();
        let after = pictures.range(ms..).next();
        match (before, after) {
            (Some(b), Some(a)) => Some(if ms - b.0 <= a.0 - ms { b.1 } else { a.1 }),
            (b, a) => b.or(a).map(|(_, t)| t),
        }
        .cloned()
    }

    fn request(&self, key: Key) {
        let mut queue = self.shared.queue();
        if !queue.pending.insert(key.clone()) {
            // Already waiting: move it to the front.
            if let Some(i) = queue.requests.iter().position(|k| *k == key) {
                queue.requests.remove(i);
            }
        }
        queue.requests.push_front(key);
        while queue.requests.len() > MAX_QUEUED {
            if let Some(old) = queue.requests.pop_back() {
                queue.pending.remove(&old);
            }
        }
        self.shared.wake.notify_one();
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        self.shared.queue().stop = true;
        self.shared.wake.notify_one();
    }
}

/// The worker: decodes requests newest first until stopped.
fn work(shared: &Shared, sender: &async_channel::Sender<Decoded>) {
    let mut open: Vec<(PathBuf, ThumbnailReader)> = Vec::new();
    let mut failed: HashSet<PathBuf> = HashSet::new();
    loop {
        let key = {
            let mut queue = shared.queue();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(key) = queue.requests.pop_front() {
                    break key;
                }
                queue = shared.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        let (path, ms) = &key;
        let message = if failed.contains(path) {
            None
        } else {
            let index = match open.iter().position(|(p, _)| p == path) {
                Some(index) => Ok(index),
                None => {
                    ThumbnailReader::open(path, DECODE_HEIGHT * 4, DECODE_HEIGHT).map(|reader| {
                        if open.len() >= OPEN_FILES {
                            open.remove(0);
                        }
                        open.push((path.clone(), reader));
                        open.len() - 1
                    })
                }
            };
            match index {
                Ok(index) => {
                    // Most recently used last.
                    let entry = open.remove(index);
                    open.push(entry);
                    let reader = &mut open.last_mut().expect("reader").1;
                    match reader.at(*ms as f64 / 1000.0) {
                        Ok(image) => Some(Decoded::Picture(key.clone(), image)),
                        Err(err) => {
                            tracing::debug!("no thumbnail of {} at {ms} ms: {err}", path.display());
                            None
                        }
                    }
                }
                Err(err) => {
                    tracing::info!("no thumbnails for {}: {err}", path.display());
                    failed.insert(path.clone());
                    Some(Decoded::Failed(path.clone()))
                }
            }
        };
        shared.queue().pending.remove(&key);
        let Some(message) = message else {
            continue;
        };
        if sender.send_blocking(message).is_err() {
            return;
        }
    }
}
