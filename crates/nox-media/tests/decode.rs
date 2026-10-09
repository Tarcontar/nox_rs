//! Decoding accuracy and GPU color conversion, against FFmpeg's CPU path.

mod common;

use common::fixture;
use nox_gpu::Converter;
use nox_media::{HwDevice, VideoDecoder, ffmpeg, rgba_cpu, to_software, with_cpu_frame};
use std::sync::{Arc, OnceLock};

/// One shared GPU for all tests (device creation is slow); `None` without Vulkan.
fn gpu() -> Option<(HwDevice, Arc<nox_gpu::GpuDevice>)> {
    static GPU: OnceLock<Option<(HwDevice, Arc<nox_gpu::GpuDevice>)>> = OnceLock::new();
    GPU.get_or_init(|| match HwDevice::vulkan() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            eprintln!("skipping GPU tests: {err}");
            None
        }
    })
    .clone()
}

/// A clip whose luma is `16 + 3 * frame index`, so decoded frames identify
/// themselves. MPEG-4 with B-frames and short GOPs exercises reordering and
/// seeking.
fn counting_clip() -> Option<std::path::PathBuf> {
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "color=c=black:size=64x64:rate=25:duration=2.4,format=yuv420p",
        "-vf", "geq=lum='16+N*3':cb=128:cr=128",
        "-c:v", "mpeg4", "-q:v", "1", "-g", "12", "-bf", "2",
    ];
    fixture("counting.mp4", &args)
}

fn luma_of(frame: &ffmpeg::frame::Video) -> u8 {
    frame.data(0)[frame.stride(0) * 32 + 32]
}

#[test]
fn frame_accurate_random_access() {
    let Some(path) = counting_clip() else { return };
    let mut decoder = VideoDecoder::open(&path, None).unwrap();
    // Forward steps, backward seeks, jumps across GOPs and the end clamp.
    for index in [0, 1, 2, 30, 5, 6, 7, 13, 12, 11, 59, 45, 46, 200, 3] {
        let frame = decoder.frame_at(index).unwrap();
        let expected = (16 + index.min(59) * 3) as i32;
        let actual = i32::from(luma_of(&frame));
        assert!(
            (actual - expected).abs() <= 1,
            "frame {index}: luma {actual}, expected {expected}"
        );
    }
}

/// swscale ignores the chroma location and assumes center-sited chroma,
/// while we treat untagged 4:2:0/4:2:2 as left-sited (the MPEG-2/H.264
/// convention). Tag frames as center-sited to compare against swscale;
/// `chroma_siting` tests the siting itself.
fn set_center_sited(frame: &mut ffmpeg::frame::Video) {
    // SAFETY: plain field write on a valid frame.
    unsafe {
        (*frame.as_mut_ptr()).chroma_location = ffmpeg::ffi::AVChromaLocation::AVCHROMA_LOC_CENTER
    };
}

/// Mean and maximum absolute difference of the RGB channels.
fn diff(a: &nox_gpu::Image, b: &nox_gpu::Image) -> (f64, u8) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut sum = 0u64;
    let mut max = 0u8;
    let mut count = 0u64;
    for (pa, pb) in a.data.chunks(4).zip(b.data.chunks(4)) {
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            sum += u64::from(d);
            max = max.max(d);
            count += 1;
        }
    }
    (sum as f64 / count as f64, max)
}

/// Converts frame 10 of a clip in `pix_fmt` on the GPU and on the CPU.
fn compare_gpu_cpu(name: &str, pix_fmt: &str, codec: &[&str]) {
    let Some((_, gpu)) = gpu() else { return };
    // Soften edges so chroma interpolation differences stay small.
    let filter = format!("gblur=sigma=2,format={pix_fmt}");
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=256x144:rate=25:duration=1",
    ];
    args.extend_from_slice(&["-vf", &filter]);
    args.extend_from_slice(codec);
    let Some(path) = fixture(name, &args) else {
        return;
    };

    let mut decoder = VideoDecoder::open(&path, None).unwrap();
    let mut frame = decoder.frame_at(10).unwrap();
    assert_eq!(frame.format().descriptor().unwrap().name(), pix_fmt);
    set_center_sited(&mut frame);
    let mut converter = Converter::new(gpu).unwrap();
    let (w, h) = (frame.width(), frame.height());
    let gpu_image = with_cpu_frame(&frame, |cpu| converter.convert(cpu, w, h))
        .unwrap()
        .unwrap();
    let cpu_image = rgba_cpu(&frame, w, h).unwrap();
    let (mean, max) = diff(&gpu_image, &cpu_image);
    eprintln!("{pix_fmt}: mean {mean:.2}, max {max}");
    assert!(mean < 1.5, "{pix_fmt}: mean difference {mean:.2}");
    assert!(max < 40, "{pix_fmt}: max difference {max}");
}

const FFV1: &[&str] = &["-c:v", "ffv1"];
const RAW_NUT: &[&str] = &["-c:v", "rawvideo", "-f", "nut"];

#[test]
fn gpu_yuv420p() {
    compare_gpu_cpu("yuv420p.nut", "yuv420p", RAW_NUT);
}

#[test]
fn gpu_yuv422p10le() {
    compare_gpu_cpu("yuv422p10le.mkv", "yuv422p10le", FFV1);
}

#[test]
fn gpu_yuv444p() {
    compare_gpu_cpu("yuv444p.mkv", "yuv444p", FFV1);
}

#[test]
fn gpu_nv12() {
    compare_gpu_cpu("nv12.nut", "nv12", RAW_NUT);
}

#[test]
fn gpu_nv21() {
    compare_gpu_cpu("nv21.nut", "nv21", RAW_NUT);
}

/// P010 (10 bits in the high bits of 16-bit words) has no lossless file
/// container that round-trips it, so convert a decoded frame in memory.
#[test]
fn gpu_p010le() {
    let Some((_, gpu)) = gpu() else { return };
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=256x144:rate=25:duration=1",
        "-vf", "gblur=sigma=2,format=yuv420p10le", "-c:v", "ffv1",
    ];
    let Some(path) = fixture("p010-source.mkv", &args) else {
        return;
    };
    let source = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(10)
        .unwrap();
    let (w, h) = (source.width(), source.height());
    let mut scaler = ffmpeg::software::scaling::Context::get(
        source.format(),
        w,
        h,
        ffmpeg::format::Pixel::P010LE,
        w,
        h,
        ffmpeg::software::scaling::Flags::POINT,
    )
    .unwrap();
    let mut frame = ffmpeg::frame::Video::empty();
    scaler.run(&source, &mut frame).unwrap();
    frame.set_color_range(source.color_range());
    set_center_sited(&mut frame);

    let mut converter = Converter::new(gpu).unwrap();
    let gpu_image = with_cpu_frame(&frame, |cpu| converter.convert(cpu, w, h))
        .unwrap()
        .unwrap();
    let (mean, max) = diff(&gpu_image, &rgba_cpu(&source, w, h).unwrap());
    eprintln!("p010le: mean {mean:.2}, max {max}");
    assert!(mean < 1.5 && max < 40, "p010le: mean {mean:.2}, max {max}");
}

#[test]
fn gpu_gbrp10le() {
    compare_gpu_cpu("gbrp10le.mkv", "gbrp10le", FFV1);
}

#[test]
fn gpu_gray() {
    compare_gpu_cpu("gray.mkv", "gray", FFV1);
}

#[test]
fn gpu_rgb24_fallback() {
    compare_gpu_cpu("rgb24.nut", "rgb24", RAW_NUT);
}

#[test]
fn gpu_scaled_output() {
    let Some((_, gpu)) = gpu() else { return };
    let Some(path) = fixture(
        "scaled.nut",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=1",
            "-c:v",
            "rawvideo",
            "-f",
            "nut",
        ],
    ) else {
        return;
    };
    let frame = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(0)
        .unwrap();
    let mut converter = Converter::new(gpu).unwrap();
    let image = with_cpu_frame(&frame, |cpu| converter.convert(cpu, 160, 90))
        .unwrap()
        .unwrap();
    assert_eq!(
        (image.width, image.height, image.data.len()),
        (160, 90, 160 * 90 * 4)
    );
}

/// Vulkan Video output must be bit-exact with software decoding (H.264
/// decoding is exactly specified).
#[test]
fn vulkan_video_decode_matches_software() {
    let Some((hw, _)) = gpu() else { return };
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=2",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "2", "-g", "10",
    ];
    let Some(path) = fixture("h264.mp4", &args) else {
        return;
    };
    let mut hardware = VideoDecoder::open(&path, Some(&hw)).unwrap();
    let mut software = VideoDecoder::open(&path, None).unwrap();
    for index in [0, 7, 23, 4, 49] {
        let hw_frame = hardware.frame_at(index).unwrap();
        if hw_frame.format() != ffmpeg::format::Pixel::VULKAN {
            eprintln!("skipping: no Vulkan Video H.264 decode on this GPU");
            return;
        }
        let nv12 = to_software(&hw_frame).unwrap();
        let yuv = software.frame_at(index).unwrap();
        assert_eq!(nv12.format(), ffmpeg::format::Pixel::NV12);
        let (w, h) = (yuv.width() as usize, yuv.height() as usize);
        for y in 0..h {
            assert_eq!(
                &nv12.data(0)[y * nv12.stride(0)..][..w],
                &yuv.data(0)[y * yuv.stride(0)..][..w],
                "frame {index}: luma row {y}"
            );
        }
        for y in 0..h / 2 {
            let uv = &nv12.data(1)[y * nv12.stride(1)..][..w];
            let u = &yuv.data(1)[y * yuv.stride(1)..][..w / 2];
            let v = &yuv.data(2)[y * yuv.stride(2)..][..w / 2];
            let interleaved: Vec<u8> = u.iter().zip(v).flat_map(|(&u, &v)| [u, v]).collect();
            assert_eq!(uv, &interleaved[..], "frame {index}: chroma row {y}");
        }
    }
}

/// A sharp chroma edge in a 16×2 4:2:0 frame: chroma texels 0–3 neutral,
/// 4–7 reddish. With left-sited chroma, chroma texel 4 sits exactly on
/// luma pixel 8, so pixel 8 gets the full red; center-sited, it sits between
/// pixels 8 and 9 and pixel 8 only gets 3/4 of the step.
#[test]
fn chroma_siting() {
    let Some((_, gpu)) = gpu() else { return };
    let mut converter = Converter::new(gpu).unwrap();
    let red_at = |converter: &mut Converter, location| {
        let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, 16, 2);
        let (y_stride, c_stride) = (frame.stride(0), frame.stride(1));
        frame.data_mut(0)[..y_stride * 2].fill(126);
        frame.data_mut(1)[..c_stride].fill(128);
        let cr = &mut frame.data_mut(2)[..c_stride];
        cr.fill(128);
        cr[4..8].fill(170); // small enough that red does not clip
        // SAFETY: plain field write on a valid frame.
        unsafe { (*frame.as_mut_ptr()).chroma_location = location };
        let image = with_cpu_frame(&frame, |cpu| converter.convert(cpu, 16, 2))
            .unwrap()
            .unwrap();
        (0..16)
            .map(|x| f64::from(image.data[x * 4]))
            .collect::<Vec<_>>()
    };
    let left = red_at(
        &mut converter,
        ffmpeg::ffi::AVChromaLocation::AVCHROMA_LOC_LEFT,
    );
    let center = red_at(
        &mut converter,
        ffmpeg::ffi::AVChromaLocation::AVCHROMA_LOC_CENTER,
    );
    let (low, high) = (left[0], left[15]);
    let fraction = |v: f64| (v - low) / (high - low);
    // Left-sited: pixel 7 halfway, pixel 8 full. Center-sited: 1/4 and 3/4.
    for (x, expected_left, expected_center) in
        [(6, 0.0, 0.0), (7, 0.5, 0.25), (8, 1.0, 0.75), (9, 1.0, 1.0)]
    {
        assert!(
            (fraction(left[x]) - expected_left).abs() < 0.03,
            "left-sited pixel {x}: {:.3}",
            fraction(left[x])
        );
        assert!(
            (fraction(center[x]) - expected_center).abs() < 0.03,
            "center-sited pixel {x}: {:.3}",
            fraction(center[x])
        );
    }
}

/// The image2 demuxer (JPEG) can't seek; the decoder must still return the
/// image for any index instead of failing with EPERM or NoFrame.
#[test]
fn still_image_any_index() {
    let args = ["-f", "lavfi", "-i", "testsrc2=size=64x48", "-frames:v", "1"];
    let Some(path) = fixture("decoder-still.jpg", &args) else {
        return;
    };
    let mut decoder = VideoDecoder::open(&path, None).unwrap();
    for index in [0, 30, 5, 0, 1000] {
        let frame = decoder
            .frame_at(index)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert_eq!((frame.width(), frame.height()), (64, 48));
    }
}

/// DMA-BUF export: with the linear modifier the buffer can be mapped, so
/// the exported pixels can be compared with the readback path.
#[test]
fn dmabuf_export_matches_readback() {
    use std::os::fd::AsRawFd;

    let Some((_, gpu)) = gpu() else { return };
    let args = [
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=25:duration=1",
        "-c:v",
        "rawvideo",
        "-f",
        "nut",
    ];
    let Some(path) = fixture("dmabuf.nut", &args) else {
        return;
    };
    let frame = VideoDecoder::open(&path, None)
        .unwrap()
        .frame_at(3)
        .unwrap();
    let mut converter = Converter::new(gpu).unwrap();
    if let Err(err) = converter.enable_dmabuf(&[0]) {
        eprintln!("skipping: {err}");
        return;
    }
    let (w, h) = (320, 240);
    let expected = with_cpu_frame(&frame, |cpu| converter.convert(cpu, w, h))
        .unwrap()
        .unwrap();
    let exported = with_cpu_frame(&frame, |cpu| converter.convert_to_dmabuf(cpu, w, h))
        .unwrap()
        .unwrap();
    assert_eq!(
        (exported.width, exported.height, exported.modifier),
        (w, h, 0)
    );
    assert_eq!(exported.fourcc, nox_gpu::FOURCC_ABGR8888);

    let size = exported.offset as usize + exported.stride as usize * h as usize;
    // SAFETY: mapping a dma-buf read-only; unmapped below.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            exported.fd.as_raw_fd(),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        eprintln!("skipping: dma-buf not mappable on this driver");
        return;
    }
    // SAFETY: the mapping is `size` bytes long.
    let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), size) };
    for y in 0..h as usize {
        let row =
            &bytes[exported.offset as usize + y * exported.stride as usize..][..w as usize * 4];
        assert_eq!(
            row,
            &expected.data[y * expected.stride..][..w as usize * 4],
            "row {y}"
        );
    }
    // SAFETY: matches the mmap above.
    unsafe { libc::munmap(mapped, size) };

    // Frames return their image to the pool; repeated exports must work.
    drop(exported);
    for _ in 0..5 {
        with_cpu_frame(&frame, |cpu| converter.convert_to_dmabuf(cpu, w, h))
            .unwrap()
            .unwrap();
    }
}

/// Vulkan-decoded frames converted in place (no download) must match the
/// download + upload path. 1080p decodes into padded (1088-row) images,
/// which exercises the UV scaling.
#[test]
fn vulkan_zero_copy_matches_download() {
    let Some((hw, gpu)) = gpu() else { return };
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=25:duration=1",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "2", "-g", "8",
    ];
    let Some(path) = fixture("h264-1080p.mp4", &args) else {
        return;
    };
    let mut decoder = VideoDecoder::open(&path, Some(&hw)).unwrap();
    let mut converter = Converter::new(gpu).unwrap();
    for index in [0, 5, 3, 20, 21] {
        let frame = decoder.frame_at(index).unwrap();
        if frame.format() != ffmpeg::format::Pixel::VULKAN {
            eprintln!("skipping: no Vulkan Video H.264 decode on this GPU");
            return;
        }
        let (w, h) = (frame.width(), frame.height());
        let zero_copy = nox_media::with_gpu_input(&frame, |input| {
            assert!(
                matches!(input, nox_gpu::Input::Vulkan(_)),
                "expected zero-copy input"
            );
            converter.convert(input, w, h)
        })
        .unwrap()
        .unwrap();
        let downloaded = with_cpu_frame(&frame, |cpu| converter.convert(cpu, w, h))
            .unwrap()
            .unwrap();
        let (mean, max) = diff(&zero_copy, &downloaded);
        assert!(max <= 1, "frame {index}: mean {mean:.3}, max {max}");
    }
}

/// Stress: random access through Vulkan Video, converted in place into
/// DMA-BUF frames, with the consumer holding the last few frames (as GTK
/// does). Exercises decoder seeks, frame locking, timeline semaphores and
/// export-pool reuse.
#[test]
fn vulkan_zero_copy_dmabuf_stress() {
    let Some((hw, gpu)) = gpu() else { return };
    #[rustfmt::skip]
    let args = [
        "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=4",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", "2", "-g", "12",
    ];
    let Some(path) = fixture("h264-stress.mp4", &args) else {
        return;
    };
    let mut decoder = VideoDecoder::open(&path, Some(&hw)).unwrap();
    let mut converter = Converter::new(gpu).unwrap();
    if let Err(err) = converter.enable_dmabuf(&[0]) {
        eprintln!("skipping: {err}");
        return;
    }
    let mut held = std::collections::VecDeque::new();
    let mut state = 0x2545_f491_u64; // xorshift: deterministic "random" access
    for step in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // Mostly short forward steps, sometimes jumps.
        let index = if step % 10 == 0 {
            (state % 100) as i64
        } else {
            (step / 2) as i64
        };
        let frame = decoder.frame_at(index).unwrap();
        let (w, h) = (frame.width(), frame.height());
        let exported =
            nox_media::with_gpu_input(&frame, |input| converter.convert_to_dmabuf(input, w, h))
                .unwrap()
                .unwrap_or_else(|e| panic!("step {step} (frame {index}): {e}"));
        held.push_back(exported);
        if held.len() > 3 {
            held.pop_front();
        }
    }
}
