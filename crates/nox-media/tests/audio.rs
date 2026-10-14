//! Audio decoding: sample-accurate random access with resampling.

mod common;

use common::fixture;
use nox_media::{AudioDecoder, CHANNELS};

/// A 4 second ramp: left = t/4, right = -t/4, so every sample tells where
/// it is.
fn ramp(name: &str, rate: u32, codec: &[&str]) -> Option<std::path::PathBuf> {
    let source = format!("aevalsrc=exprs=t/4|-t/4:s={rate}:d=4");
    let mut args = vec!["-f", "lavfi", "-i", &source];
    args.extend_from_slice(codec);
    fixture(name, &args)
}

fn check(path: &std::path::Path, tolerance: f32) {
    const RATE: u32 = 48000;
    let mut decoder = AudioDecoder::open(path, RATE).unwrap();
    let mut buffer = vec![0f32; 1024 * CHANNELS];
    // Forward, small jumps, backward seeks and a seek near the end.
    for start in [0, 1024, 2048, 48_000, 47_000, 150_000, 10, 180_000, 96_000] {
        decoder.read(start, &mut buffer).unwrap();
        for (i, frame) in buffer.chunks(CHANNELS).enumerate() {
            let t = (start + i as i64) as f32 / RATE as f32;
            // Skip the first few ms, where the resampler and codecs ramp up.
            if t < 0.01 {
                continue;
            }
            let expected = t / 4.0;
            assert!(
                (frame[0] - expected).abs() <= tolerance
                    && (frame[1] + expected).abs() <= tolerance,
                "{}: sample {} of read at {start}: {:?}, expected ±{expected}",
                path.display(),
                i,
                frame
            );
        }
    }
    // Past the end: silence, apart from a lossy codec's ringing after the
    // signal stops abruptly.
    decoder.read(4 * RATE as i64 + 100, &mut buffer).unwrap();
    let loudest = buffer.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(loudest < 0.01, "past the end: {loudest}");
    decoder.read(5 * RATE as i64, &mut buffer).unwrap();
    assert!(buffer.iter().all(|&s| s == 0.0));
}

#[test]
fn wav_at_another_rate() {
    let Some(path) = ramp("ramp.wav", 44_100, &["-c:a", "pcm_f32le"]) else {
        return;
    };
    check(&path, 1e-3);
}

#[test]
fn aac_in_mp4() {
    let Some(path) = ramp("ramp.m4a", 48_000, &["-c:a", "aac", "-b:a", "256k"]) else {
        return;
    };
    check(&path, 0.02);
}

#[test]
fn video_without_audio() {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=64x36:rate=25:duration=0.2",
        "-c:v",
        "mpeg4",
    ];
    let Some(path) = fixture("silent.mp4", &args) else {
        return;
    };
    assert!(matches!(
        AudioDecoder::open(&path, 48000),
        Err(nox_media::Error::NoAudioStream)
    ));
}

/// Waveform peaks follow the level, and the disk cache returns the same.
#[test]
fn waveform_peaks() {
    use nox_media::waveform::{PEAKS_PER_SECOND, Waveform};
    use std::sync::atomic::AtomicBool;
    // Silence for a second, then a full-scale and a half-scale tone.
    // Stereo: mono is mixed to both channels at -3 dB, as in playback.
    let tone = "if(lt(t,1),0,if(lt(t,2),1,0.5))*sin(2*PI*440*t)";
    let source = format!("aevalsrc=exprs='{tone}|{tone}':s=48000:d=3");
    let Some(path) = fixture("levels.wav", &["-f", "lavfi", "-i", &source]) else {
        return;
    };
    let cancel = AtomicBool::new(false);
    let waveform = Waveform::compute(&path, &cancel).unwrap();
    assert_eq!(waveform.peaks.len(), 3 * PEAKS_PER_SECOND as usize);
    assert!(waveform.peak(0.1, 0.9) < 0.01);
    assert!(
        (waveform.peak(1.1, 1.9) - 1.0).abs() < 0.05,
        "{}",
        waveform.peak(1.1, 1.9)
    );
    assert!(
        (waveform.peak(2.1, 2.9) - 0.5).abs() < 0.05,
        "{}",
        waveform.peak(2.1, 2.9)
    );
    // Past the end there is nothing.
    assert_eq!(waveform.peak(5.0, 6.0), 0.0);

    let cache = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("waveforms");
    let _ = std::fs::remove_dir_all(&cache);
    let first = Waveform::load_or_compute(&path, &cache, &cancel).unwrap();
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
    let cached = Waveform::load_or_compute(&path, &cache, &cancel).unwrap();
    assert_eq!((first.clone(), cached), (waveform.clone(), waveform));
    // Cancelled before it starts.
    let cancelled = AtomicBool::new(true);
    assert!(Waveform::compute(&path, &cancelled).is_err());
}
