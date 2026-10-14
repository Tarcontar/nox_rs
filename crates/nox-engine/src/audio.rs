//! Audio playback: mixing the timeline's audio tracks and playing them on
//! the default output device, which also serves as the playback clock.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use nox_media::{AudioDecoder, CHANNELS};
use nox_project::{Rational, Time, TimeRange, Timeline};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Samples per mixing block.
const BLOCK: usize = 1024;
/// How far ahead the mixer fills the output buffer.
const AHEAD: Duration = Duration::from_millis(250);
/// Decoders unused for this many blocks are closed.
const IDLE_BLOCKS: u64 = 200;

/// Mixes the audio tracks of a timeline to stereo.
pub struct Mixer {
    rate: u32,
    decoders: HashMap<(PathBuf, usize), (AudioDecoder, u64)>,
    failed: HashSet<PathBuf>,
    scratch: Vec<f32>,
    /// Source samples for clips at another speed.
    varispeed: Vec<f32>,
    blocks: u64,
}

impl Mixer {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            decoders: HashMap::new(),
            failed: HashSet::new(),
            scratch: Vec::new(),
            varispeed: Vec::new(),
            blocks: 0,
        }
    }

    /// Mixes the samples from timeline sample `start` on into `out`
    /// (interleaved stereo).
    pub fn mix(&mut self, timeline: &Timeline, start: i64, out: &mut [f32]) {
        out.fill(0.0);
        self.blocks += 1;
        let frames = (out.len() / CHANNELS) as i64;
        let timebase = Rational::new(1, i64::from(self.rate));
        let range = TimeRange::new(Time::new(start, timebase), Time::new(frames, timebase));
        let mut slots: HashMap<PathBuf, usize> = HashMap::new();
        for segment in timeline.audio_segments(range) {
            // Freeze frames are silent.
            if self.failed.contains(&segment.path) || segment.speed == 0.0 {
                continue;
            }
            // Two segments of one file (e.g. in a crossfade) need two decoders.
            let slot = slots.entry(segment.path.clone()).or_default();
            let key = (segment.path.clone(), *slot);
            *slot += 1;
            if !self.decoders.contains_key(&key) {
                match AudioDecoder::open(&segment.path, self.rate) {
                    Ok(decoder) => {
                        self.decoders.insert(key.clone(), (decoder, 0));
                    }
                    Err(nox_media::Error::NoAudioStream) => {
                        self.failed.insert(segment.path.clone());
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!("{}: no audio: {err}", segment.path.display());
                        self.failed.insert(segment.path.clone());
                        continue;
                    }
                }
            }
            let (decoder, used) = self.decoders.get_mut(&key).expect("decoder");
            *used = self.blocks;
            let place = segment.place.start.rescale(timebase).ticks;
            let source_start = segment.source_start.rescale(timebase).ticks;
            let frames = out.len() / CHANNELS;
            let speed = segment.speed;
            if speed == 1.0 {
                let source = source_start + (start - place);
                self.scratch.resize(out.len(), 0.0);
                if let Err(err) = decoder.read(source, &mut self.scratch) {
                    tracing::warn!("{}: {err}", segment.path.display());
                    continue;
                }
            } else {
                // Varispeed: read the stretch of source this block covers
                // and resample it linearly (the pitch changes with speed).
                let first = source_start as f64 + (start - place) as f64 * speed;
                let base = first.floor();
                let span = (frames as f64 * speed).ceil() as usize + 2;
                self.varispeed.resize(span * CHANNELS, 0.0);
                if let Err(err) = decoder.read(base as i64, &mut self.varispeed) {
                    tracing::warn!("{}: {err}", segment.path.display());
                    continue;
                }
                self.scratch.resize(out.len(), 0.0);
                for (i, frame) in self
                    .scratch
                    .as_chunks_mut::<CHANNELS>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    let pos = first - base + i as f64 * speed;
                    let index = (pos.floor() as usize).min(span - 2);
                    let f = (pos - index as f64) as f32;
                    for (c, sample) in frame.iter_mut().enumerate() {
                        let a = self.varispeed[index * CHANNELS + c];
                        let b = self.varispeed[(index + 1) * CHANNELS + c];
                        *sample = a + (b - a) * f;
                    }
                }
            }
            for (i, (dst, src)) in out
                .chunks_exact_mut(CHANNELS)
                .zip(self.scratch.chunks_exact(CHANNELS))
                .enumerate()
            {
                let t = (start + i as i64) as f64 / f64::from(self.rate);
                let gain = segment.gain_at(t) as f32;
                if gain != 0.0 {
                    for c in 0..CHANNELS {
                        dst[c] += src[c] * gain;
                    }
                }
            }
        }
        let blocks = self.blocks;
        self.decoders
            .retain(|_, (_, used)| blocks - *used < IDLE_BLOCKS);
    }
}

/// Where the output device is, for the playback clock.
#[derive(Default)]
pub(crate) struct AudioClock {
    /// Frames handed to the device so far.
    consumed: AtomicU64,
    /// At the last callback: frames consumed before it, when it ran, and
    /// how long until its first frame is heard.
    last: Mutex<Option<(u64, Instant, Duration)>>,
    /// Frames the device played as silence because the mixer was late.
    pub underrun: AtomicU64,
}

impl AudioClock {
    /// Records a device callback, as the stream does.
    #[cfg(test)]
    pub(crate) fn set(&self, frames: u64, at: Instant, latency: Duration) {
        *self.last.lock().unwrap() = Some((frames, at, latency));
    }

    /// Seconds of audio heard so far.
    #[cfg(test)]
    pub fn seconds(&self, rate: u32) -> f64 {
        self.seconds_at(rate, Instant::now())
    }

    /// Seconds of audio heard by `now`.
    pub fn seconds_at(&self, rate: u32, now: Instant) -> f64 {
        let last = *self.last.lock().unwrap_or_else(|e| e.into_inner());
        match last {
            Some((frames, at, latency)) => {
                let played = frames as f64 / f64::from(rate)
                    + now.saturating_duration_since(at).as_secs_f64()
                    - latency.as_secs_f64();
                played.max(0.0)
            }
            None => 0.0,
        }
    }
}

/// An open output stream and the thread feeding it.
pub(crate) struct AudioOutput {
    _stream: cpal::Stream,
    pub rate: u32,
    pub clock: Arc<AudioClock>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
}

impl AudioOutput {
    /// Starts playing the timeline from `start` (seconds). `timeline`
    /// returns the current timeline, so edits during playback are heard.
    pub fn start(
        start: f64,
        timeline: impl Fn() -> Option<Arc<Timeline>> + Send + 'static,
    ) -> Result<Self, String> {
        let wanted = std::env::var("NOX_AUDIO_DEVICE").ok();
        if wanted.as_deref() == Some("none") {
            return Err("disabled by NOX_AUDIO_DEVICE=none".into());
        }
        let host = cpal::default_host();
        let device = match &wanted {
            Some(name) => host
                .output_devices()
                .map_err(|e| e.to_string())?
                .find(|d| {
                    d.description()
                        .is_ok_and(|desc| desc.name().contains(name.as_str()))
                })
                .ok_or_else(|| format!("no output device matching {name:?}"))?,
            None => host
                .default_output_device()
                .ok_or("no default output device")?,
        };
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let ring = HeapRb::<f32>::new(rate as usize * CHANNELS);
        let (mut producer, mut consumer) = ring.split();
        let clock = Arc::new(AudioClock::default());

        let callback_clock = clock.clone();
        let mut stereo = Vec::new();
        let data_callback = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            let timestamp = info.timestamp();
            let latency = timestamp.playback.duration_since(timestamp.callback);
            let before = callback_clock
                .consumed
                .fetch_add(frames as u64, Ordering::Relaxed);
            if let Ok(mut last) = callback_clock.last.try_lock() {
                *last = Some((before, Instant::now(), latency));
            }
            stereo.resize(frames * CHANNELS, 0.0);
            let got = consumer.pop_slice(&mut stereo) / CHANNELS;
            if got < frames {
                stereo[got * CHANNELS..].fill(0.0);
                callback_clock
                    .underrun
                    .fetch_add((frames - got) as u64, Ordering::Relaxed);
            }
            for (out, frame) in data
                .chunks_exact_mut(channels)
                .zip(stereo.chunks_exact(CHANNELS))
            {
                match channels {
                    1 => out[0] = 0.5 * (frame[0] + frame[1]),
                    _ => {
                        out[0] = frame[0];
                        out[1] = frame[1];
                        out[2..].fill(0.0);
                    }
                }
            }
        };
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
                config.config(),
                data_callback,
                |err| tracing::warn!("audio output: {err}"),
                None,
            ),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;

        // Fill some audio before the device starts asking for it.
        let mut mixer = Mixer::new(rate);
        let mut position = (start * f64::from(rate)).round() as i64;
        let mut block = vec![0.0; BLOCK * CHANNELS];
        let ahead = (AHEAD.as_secs_f64() * f64::from(rate)) as usize * CHANNELS;
        let fill = move |mixer: &mut Mixer,
                         position: &mut i64,
                         producer: &mut ringbuf::HeapProd<f32>,
                         block: &mut Vec<f32>| {
            while producer.occupied_len() < ahead && producer.vacant_len() >= block.len() {
                match timeline() {
                    Some(timeline) => mixer.mix(&timeline, *position, block),
                    None => block.fill(0.0),
                }
                producer.push_slice(block);
                *position += BLOCK as i64;
            }
        };
        fill(&mut mixer, &mut position, &mut producer, &mut block);
        stream.play().map_err(|e| e.to_string())?;

        let stop = Arc::new(AtomicBool::new(false));
        let feeder_stop = stop.clone();
        let feeder_clock = clock.clone();
        let feeder = std::thread::Builder::new()
            .name("nox-audio".into())
            .spawn(move || {
                while !feeder_stop.load(Ordering::Relaxed) {
                    // Frames the device played as silence: skip them, so
                    // audio stays in step with the clock.
                    let missed = feeder_clock.underrun.swap(0, Ordering::Relaxed);
                    if missed > 0 {
                        tracing::debug!("audio underrun: {missed} frames");
                        position += missed as i64;
                    }
                    fill(&mut mixer, &mut position, &mut producer, &mut block);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .expect("failed to spawn audio thread");
        tracing::info!(
            "audio output: {} Hz, {channels} channels ({})",
            rate,
            device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_default()
        );
        Ok(Self {
            _stream: stream,
            rate,
            clock,
            stop,
            feeder: Some(feeder),
        })
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_follows_consumed_frames_minus_latency() {
        let clock = AudioClock::default();
        assert_eq!(clock.seconds(48000), 0.0);
        *clock.last.lock().unwrap() = Some((96_000, Instant::now(), Duration::from_millis(500)));
        let seconds = clock.seconds(48000);
        assert!((1.5..1.6).contains(&seconds), "{seconds}");
    }
}
