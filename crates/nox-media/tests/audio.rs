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
