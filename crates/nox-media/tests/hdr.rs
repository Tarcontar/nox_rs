//! HDR (PQ, HLG) frames shown on SDR: the GPU matches the reference in
//! `nox_color::hdr`, and the content peak comes from the stream metadata.

mod common;

use common::{fixture, gpu};
use nox_color::hdr::{Gamut, HdrTransfer};
use nox_gpu::Converter;
use nox_media::{VideoDecoder, ffmpeg, tone_mapping, with_cpu_frame};

/// A solid 10-bit 4:4:4 frame tagged BT.2020 with `transfer`.
fn solid(name: &str, color: &str, transfer: &str) -> Option<ffmpeg::frame::Video> {
    // FFmpeg takes the tags from the frames, so set them there.
    let input = format!(
        "color=c={color}:size=64x36:rate=25:duration=0.2,format=yuv444p10le,\
         setparams=color_primaries=bt2020:color_trc={transfer}:colorspace=bt2020nc"
    );
    let args = ["-f", "lavfi", "-i", &input, "-c:v", "ffv1"];
    let path = fixture(name, &args)?;
    Some(
        VideoDecoder::open(&path, None)
            .unwrap()
            .frame_at(0)
            .unwrap(),
    )
}

/// The reference result for the center pixel: the frame's Y'CbCr matrix,
/// then `ToneMapping::apply`.
fn reference(frame: &ffmpeg::frame::Video) -> [f32; 3] {
    let sample = |plane: usize| {
        let row = &frame.data(plane)[frame.stride(plane) * 18..];
        f32::from(u16::from_le_bytes([row[64], row[65]])) / 65535.0
    };
    let texels = [sample(0), sample(1), sample(2), 1.0];
    let rows = with_cpu_frame(frame, |cpu| cpu.matrix).unwrap();
    let rgb = rows.map(|r| {
        (0..4)
            .map(|i| r[i] * texels[i])
            .sum::<f32>()
            .clamp(0.0, 1.0)
    });
    tone_mapping(frame).apply(rgb)
}

#[test]
fn gpu_tone_mapping_matches_the_reference() {
    let Some((_, gpu)) = gpu() else { return };
    let mut converter = Converter::new(gpu).unwrap();
    // Dark, mid, reference white, bright highlight, saturated colors.
    let colors = [
        "0x202020", "0x606060", "0x9a9a9a", "0xe0e0e0", "0xd04010", "0x20c040", "0x3050f0",
    ];
    for transfer in ["smpte2084", "arib-std-b67"] {
        for color in colors {
            let name = format!("hdr-{transfer}-{color}.mkv");
            let Some(frame) = solid(&name, color, transfer) else {
                return;
            };
            let tone = tone_mapping(&frame);
            let expected_transfer = if transfer == "smpte2084" {
                HdrTransfer::Pq
            } else {
                HdrTransfer::Hlg
            };
            assert_eq!(
                (tone.transfer, tone.gamut),
                (expected_transfer, Gamut::Bt2020)
            );
            let image = with_cpu_frame(&frame, |cpu| converter.convert(cpu, 64, 36))
                .unwrap()
                .unwrap();
            let i = 18 * image.stride + 32 * 4;
            let expected = reference(&frame);
            for c in 0..3 {
                let e = (expected[c] * 255.0).round();
                let got = f32::from(image.data[i + c]);
                assert!(
                    (got - e).abs() <= 2.0,
                    "{transfer} {color}: got {:?}, expected {expected:?}",
                    &image.data[i..i + 3]
                );
            }
        }
    }
}

/// MaxCLL from HEVC SEI metadata sets the source peak; the CPU path
/// (thumbnails) tone-maps too.
#[test]
fn content_peak_from_metadata() {
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i",
        "color=c=0xe0e0e0:size=128x72:rate=25:duration=0.2,format=yuv420p10le,\
         setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
        "-c:v", "libx265", "-x265-params",
        "log-level=error:hdr10=1:max-cll=4000,400:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)",
    ];
    let Some(path) = fixture("hdr10-meta.mp4", &args) else {
        return;
    };
    let frame = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(0)
        .unwrap();
    let tone = tone_mapping(&frame);
    assert_eq!((tone.transfer, tone.source_peak), (HdrTransfer::Pq, 4000.0));

    let image = nox_media::rgba_cpu(&frame, 16, 9).unwrap();
    let rgb = &image.data[(4 * 16 + 8) * 4..][..3];
    // Light gray PQ (about 1500 nits) maps to a bright, but not clipped,
    // SDR gray rather than the raw signal.
    let raw = (0.82f32 * 255.0) as u8;
    assert!(rgb.iter().all(|&c| c > 200 && c != raw), "{rgb:?}");
}
