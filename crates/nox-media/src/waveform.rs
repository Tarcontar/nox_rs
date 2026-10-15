//! Audio waveforms for the timeline: the peak level of a file's audio in
//! short windows, computed once per file and cached on disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{AudioDecoder, CHANNELS, Error, Result};

/// Peaks per second of audio: enough for a frame-level zoom at 50 fps.
pub const PEAKS_PER_SECOND: u32 = 100;
/// Decoding rate; the waveform needs no more than this.
const RATE: u32 = 16_000;
const MAGIC: &[u8; 8] = b"NOXWAV1\0";

/// The loudest sample (of either channel) in each window of
/// 1/`PEAKS_PER_SECOND` s, from 0 to 255 for full scale.
#[derive(Debug, Clone, PartialEq)]
pub struct Waveform {
    pub peaks: Vec<u8>,
}

impl Waveform {
    /// The loudest peak from `start` to `end` seconds (file time), 0–1.
    pub fn peak(&self, start: f64, end: f64) -> f32 {
        let at = |t: f64| (t * f64::from(PEAKS_PER_SECOND)).max(0.0) as usize;
        let first = at(start).min(self.peaks.len());
        let last = at(end).max(first + 1).min(self.peaks.len());
        self.peaks[first..last]
            .iter()
            .max()
            .map_or(0.0, |&p| f32::from(p) / 255.0)
    }

    /// Computes the waveform of `path`'s first audio stream. Returns
    /// `Error::Interrupted` once `cancel` is set.
    pub fn compute(path: &Path, cancel: &AtomicBool) -> Result<Self> {
        let mut decoder = AudioDecoder::open(path, RATE)?;
        let seconds = decoder.duration_seconds().unwrap_or(0.0);
        let count = (seconds * f64::from(PEAKS_PER_SECOND)).ceil() as usize;
        let window = (RATE / PEAKS_PER_SECOND) as usize;
        // About a second per read.
        let per_read = PEAKS_PER_SECOND as usize;
        let mut buffer = vec![0f32; window * per_read * CHANNELS];
        let mut peaks = Vec::with_capacity(count);
        while peaks.len() < count {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Interrupted);
            }
            let start = (peaks.len() * window) as i64;
            decoder.read(start, &mut buffer)?;
            for chunk in buffer.chunks(window * CHANNELS).take(count - peaks.len()) {
                let loudest = chunk.iter().fold(0f32, |m, s| m.max(s.abs()));
                peaks.push((loudest.min(1.0) * 255.0).round() as u8);
            }
        }
        Ok(Self { peaks })
    }

    /// The waveform of `path` from the cache in `cache_dir`, or computed
    /// and stored there. The cache entry is keyed by the file's path, size
    /// and modification time, so changed files are computed again.
    pub fn load_or_compute(path: &Path, cache_dir: &Path, cancel: &AtomicBool) -> Result<Self> {
        let entry = cache_entry(path, cache_dir)?;
        if let Some(waveform) = std::fs::read(&entry).ok().and_then(|b| Self::decode(&b)) {
            return Ok(waveform);
        }
        let waveform = Self::compute(path, cancel)?;
        // A cache that can't be written only costs time.
        let written = std::fs::create_dir_all(cache_dir)
            .and_then(|()| std::fs::write(&entry, waveform.encode()));
        if let Err(err) = written {
            tracing::debug!("cannot cache the waveform of {}: {err}", path.display());
        }
        Ok(waveform)
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&PEAKS_PER_SECOND.to_le_bytes());
        bytes.extend_from_slice(&self.peaks);
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(MAGIC)?;
        let (rate, peaks) = rest.split_first_chunk::<4>()?;
        (u32::from_le_bytes(*rate) == PEAKS_PER_SECOND).then(|| Self {
            peaks: peaks.to_vec(),
        })
    }
}

fn cache_entry(path: &Path, cache_dir: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    // FNV-1a over the path, size and modification time: unlike std's
    // hasher, the same in every Rust release, so caches stay valid.
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    let hash = [
        path.as_os_str().as_encoded_bytes(),
        &metadata.len().to_le_bytes(),
        &modified.as_secs().to_le_bytes(),
        &modified.subsec_nanos().to_le_bytes(),
    ]
    .concat()
    .iter()
    .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok(cache_dir.join(format!("{hash:016x}.wave")))
}
