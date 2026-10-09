//! Background decoding of single frames for the viewer.
//!
//! Requests go to a worker thread that owns the decoders and the GPU
//! converter. While it is busy, newer requests replace older ones, so
//! scrubbing never queues up stale work.
//!
//! Frames are exported as DMA-BUF when the consumer passed importable
//! modifiers (zero copy), and read back to system memory otherwise. Every
//! change of the presentation path is logged at info level.

use nox_gpu::{Converter, DmabufFrame, Image};
use nox_media::{HwDevice, VideoDecoder};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

/// Display-referred RGBA at display size (sample aspect ratio applied).
#[derive(Debug)]
pub enum FrameImage {
    /// In system memory.
    Pixels(Image),
    /// On the GPU, exported as DMA-BUF.
    Dmabuf(DmabufFrame),
}

/// A decoded and converted frame.
#[derive(Debug)]
pub struct PreviewFrame {
    pub path: PathBuf,
    pub index: i64,
    pub image: FrameImage,
}

struct Request {
    path: PathBuf,
    index: i64,
}

pub struct Preview {
    requests: Option<Sender<Request>>,
    worker: Option<JoinHandle<()>>,
    dmabuf_allowed: Arc<AtomicBool>,
}

impl Preview {
    /// Starts the worker. `dmabuf_modifiers` are the DRM modifiers the
    /// consumer can import for `FOURCC_ABGR8888` (empty: always read back).
    /// `on_frame` is called on the worker thread for every completed request.
    pub fn new(
        dmabuf_modifiers: Vec<u64>,
        on_frame: impl Fn(Result<PreviewFrame, String>) + Send + 'static,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let dmabuf_allowed = Arc::new(AtomicBool::new(!dmabuf_modifiers.is_empty()));
        let allowed = dmabuf_allowed.clone();
        let worker = std::thread::Builder::new()
            .name("nox-preview".into())
            .spawn(move || Worker::new(&dmabuf_modifiers, allowed).run(receiver, on_frame))
            .expect("failed to spawn preview thread");
        Self {
            requests: Some(requests),
            worker: Some(worker),
            dmabuf_allowed,
        }
    }

    /// Requests frame `index` of `path` (counted at the clip's frame rate).
    pub fn request(&self, path: &Path, index: i64) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(Request {
                path: path.to_owned(),
                index,
            });
        }
    }

    /// Falls back to readback, e.g. after the consumer failed to import a
    /// DMA-BUF.
    pub fn disable_dmabuf(&self) {
        self.dmabuf_allowed.store(false, Ordering::Relaxed);
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // Closing the channel stops the worker.
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// How frames reach the consumer.
#[derive(Debug, Clone, PartialEq)]
enum PresentPath {
    /// GPU conversion, exported without copies.
    Dmabuf { modifier: u64 },
    /// GPU conversion, copied to system memory.
    Readback { reason: String },
    /// No Vulkan device: swscale on the CPU.
    Cpu,
}

struct Worker {
    gpu: Option<(HwDevice, Converter)>,
    decoder: Option<(PathBuf, VideoDecoder)>,
    dmabuf_allowed: Arc<AtomicBool>,
    /// Why DMA-BUF is not used, if it isn't.
    readback_reason: String,
    present_path: Option<PresentPath>,
}

impl Worker {
    fn new(dmabuf_modifiers: &[u64], dmabuf_allowed: Arc<AtomicBool>) -> Self {
        let mut readback_reason =
            "viewer offered no DMA-BUF formats (NOX_PRESENT=readback or unsupported)".to_owned();
        let gpu = match HwDevice::vulkan().and_then(|(hw, gpu)| Ok((hw, Converter::new(gpu)?))) {
            Ok((hw, mut converter)) => {
                if !dmabuf_modifiers.is_empty()
                    && let Err(err) = converter.enable_dmabuf(dmabuf_modifiers)
                {
                    readback_reason = format!("DMA-BUF export unavailable: {err}");
                }
                Some((hw, converter))
            }
            Err(err) => {
                tracing::warn!("no Vulkan device: {err}");
                None
            }
        };
        Self {
            gpu,
            decoder: None,
            dmabuf_allowed,
            readback_reason,
            present_path: None,
        }
    }

    fn run(mut self, requests: Receiver<Request>, on_frame: impl Fn(Result<PreviewFrame, String>)) {
        while let Ok(mut request) = requests.recv() {
            // Skip to the newest request.
            while let Ok(newer) = requests.try_recv() {
                request = newer;
            }
            let started = Instant::now();
            let result = self
                .render(&request)
                .map_err(|err| format!("{}: {err}", request.path.display()));
            tracing::trace!("preview frame {} in {:?}", request.index, started.elapsed());
            on_frame(result);
        }
    }

    fn render(&mut self, request: &Request) -> nox_media::Result<PreviewFrame> {
        if self
            .decoder
            .as_ref()
            .is_none_or(|(path, _)| *path != request.path)
        {
            let hw = self.gpu.as_ref().map(|(hw, _)| hw);
            self.decoder = Some((request.path.clone(), VideoDecoder::open(&request.path, hw)?));
        }
        let (_, decoder) = self.decoder.as_mut().expect("decoder");
        let frame = decoder.frame_at(request.index)?;
        let (width, height) = nox_media::display_size(&frame);

        let image = match &mut self.gpu {
            None => FrameImage::Pixels(nox_media::rgba_cpu(&frame, width, height)?),
            Some((_, converter)) => {
                let mut image = None;
                if converter.dmabuf_enabled() {
                    if !self.dmabuf_allowed.load(Ordering::Relaxed) {
                        self.readback_reason = "consumer failed to import DMA-BUF".into();
                    } else {
                        match nox_media::with_gpu_input(&frame, |input| {
                            converter.convert_to_dmabuf(input, width, height)
                        })? {
                            Ok(dmabuf) => image = Some(FrameImage::Dmabuf(dmabuf)),
                            Err(err) => {
                                self.dmabuf_allowed.store(false, Ordering::Relaxed);
                                self.readback_reason = format!("DMA-BUF export failed: {err}");
                            }
                        }
                    }
                }
                match image {
                    Some(image) => image,
                    None => FrameImage::Pixels(nox_media::with_gpu_input(&frame, |input| {
                        converter.convert(input, width, height)
                    })??),
                }
            }
        };

        let path = match (&image, &self.gpu) {
            (FrameImage::Dmabuf(dmabuf), _) => PresentPath::Dmabuf {
                modifier: dmabuf.modifier,
            },
            (FrameImage::Pixels(_), Some(_)) => PresentPath::Readback {
                reason: self.readback_reason.clone(),
            },
            (FrameImage::Pixels(_), None) => PresentPath::Cpu,
        };
        self.log_present_path(path);
        Ok(PreviewFrame {
            path: request.path.clone(),
            index: request.index,
            image,
        })
    }

    fn log_present_path(&mut self, path: PresentPath) {
        if self.present_path.as_ref() == Some(&path) {
            return;
        }
        match &path {
            PresentPath::Dmabuf { modifier } => {
                tracing::info!("presenting via DMA-BUF (zero copy, DRM modifier {modifier:#x})")
            }
            PresentPath::Readback { reason } => {
                tracing::info!("presenting via GPU readback ({reason})")
            }
            PresentPath::Cpu => tracing::info!("presenting via CPU conversion (no Vulkan device)"),
        }
        self.present_path = Some(path);
    }
}
