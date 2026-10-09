//! Turning decoded frames into something displayable: a `CpuFrame` for the
//! GPU converter, or RGBA via swscale on the CPU (fallback and reference).

use ffmpeg::{ffi, format, frame, software::scaling};
use nox_color::{Cicp, Matrix3x4, Quantization, ycbcr};
use nox_gpu::{CpuFrame, Image, Layout, Plane, PlaneFormat};

use crate::Result;

/// Size at which the frame should be displayed, honoring the sample aspect
/// ratio (e.g. 720×576 with 16:15 pixels → 768×576).
pub fn display_size(frame: &frame::Video) -> (u32, u32) {
    let sar = frame.aspect_ratio();
    let (w, h) = (frame.width(), frame.height());
    if sar.numerator() > 0 && sar.denominator() > 0 && sar.numerator() != sar.denominator() {
        let width = (f64::from(w) * f64::from(sar)).round() as u32;
        (width.max(1), h)
    } else {
        (w, h)
    }
}

/// The frame's color tags.
pub fn color_tags(frame: &frame::Video) -> Cicp {
    Cicp {
        primaries: ffi::AVColorPrimaries::from(frame.color_primaries()) as u8,
        transfer: ffi::AVColorTransferCharacteristic::from(frame.color_transfer_characteristic())
            as u8,
        matrix: ffi::AVColorSpace::from(frame.color_space()) as u8,
        full_range: frame.color_range() == ffmpeg::color::Range::JPEG,
    }
}

/// Calls `f` with a GPU-uploadable view of `frame`. Hardware frames are
/// downloaded first; pixel formats the shader doesn't handle are converted
/// to RGBA by swscale.
pub fn with_cpu_frame<R>(frame: &frame::Video, f: impl FnOnce(&CpuFrame<'_>) -> R) -> Result<R> {
    if frame.format() == format::Pixel::VULKAN || is_hardware(frame.format()) {
        let software = to_software(frame)?;
        return with_cpu_frame(&software, f);
    }
    if let Some(cpu) = describe(frame) {
        return Ok(f(&cpu));
    }
    // Generic path: let swscale produce packed RGBA (16 bit for deep formats).
    let deep = frame.format().descriptor().is_some_and(|d| {
        // SAFETY: descriptors are static.
        unsafe { (*d.as_ptr()).comp[0].depth > 8 }
    });
    let target = if deep {
        format::Pixel::RGBA64LE
    } else {
        format::Pixel::RGBA
    };
    let rgba = convert(frame, target, frame.width(), frame.height())?;
    let quantization = if deep {
        Quantization {
            bit_depth: 16,
            container_bits: 16,
            msb_aligned: false,
            full_range: true,
        }
    } else {
        Quantization::eight_bit(true)
    };
    let cpu = CpuFrame {
        width: rgba.width(),
        height: rgba.height(),
        layout: Layout::Packed,
        swap_chroma: false,
        matrix: ycbcr::rgb_to_rgb(quantization),
        chroma_offset: [0.0, 0.0],
        planes: vec![plane(
            &rgba,
            0,
            if deep {
                PlaneFormat::Rgba16
            } else {
                PlaneFormat::Rgba8
            },
        )],
    };
    Ok(f(&cpu))
}

/// Converts to 8-bit RGBA at `width` × `height` on the CPU.
pub fn rgba_cpu(frame: &frame::Video, width: u32, height: u32) -> Result<Image> {
    let software;
    let frame = if is_hardware(frame.format()) {
        software = to_software(frame)?;
        &software
    } else {
        frame
    };
    let rgba = convert(frame, format::Pixel::RGBA, width, height)?;
    let stride = width as usize * 4;
    let mut data = Vec::with_capacity(stride * height as usize);
    for row in rgba.data(0).chunks(rgba.stride(0)).take(height as usize) {
        data.extend_from_slice(&row[..stride]);
    }
    Ok(Image {
        width,
        height,
        stride,
        data,
    })
}

fn is_hardware(format: format::Pixel) -> bool {
    format.descriptor().is_some_and(|d| {
        // SAFETY: descriptors are static.
        unsafe { (*d.as_ptr()).flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0 }
    })
}

/// Copies a hardware frame (e.g. Vulkan) to system memory.
pub fn to_software(frame: &frame::Video) -> Result<frame::Video> {
    let mut software = frame::Video::empty();
    // SAFETY: valid frames; FFmpeg picks the download format.
    let ret = unsafe { ffi::av_hwframe_transfer_data(software.as_mut_ptr(), frame.as_ptr(), 0) };
    if ret < 0 {
        return Err(ffmpeg::Error::from(ret).into());
    }
    // SAFETY: valid frames; copies timestamps and color properties.
    unsafe { ffi::av_frame_copy_props(software.as_mut_ptr(), frame.as_ptr()) };
    Ok(software)
}

/// How a pixel format maps onto the shader.
pub(crate) struct Classified {
    pub layout: Layout,
    pub swap_chroma: bool,
    pub matrix: Matrix3x4,
    /// In luma pixels; see `CpuFrame::chroma_offset`.
    pub chroma_offset: [f32; 2],
    /// Texel format and log2 subsampling (w, h) of FFmpeg planes 0, 1, ...
    pub planes: Vec<(PlaneFormat, u8, u8)>,
}

/// Classifies `format` (the frame's own format, or the software format of
/// a hardware frame) using the color properties of `frame`. `None` means
/// the shader can't read it directly.
pub(crate) fn classify(format: format::Pixel, frame: &frame::Video) -> Option<Classified> {
    let descriptor = format.descriptor()?;
    // SAFETY: descriptors are static.
    let d = unsafe { &*descriptor.as_ptr() };
    let unsupported = ffi::AV_PIX_FMT_FLAG_BE
        | ffi::AV_PIX_FMT_FLAG_PAL
        | ffi::AV_PIX_FMT_FLAG_BITSTREAM
        | ffi::AV_PIX_FMT_FLAG_HWACCEL
        | ffi::AV_PIX_FMT_FLAG_BAYER
        | ffi::AV_PIX_FMT_FLAG_FLOAT
        | ffi::AV_PIX_FMT_FLAG_XYZ;
    if d.flags & unsupported as u64 != 0 {
        return None;
    }
    let has_alpha = d.flags & ffi::AV_PIX_FMT_FLAG_ALPHA as u64 != 0;
    let color_components = usize::from(d.nb_components) - usize::from(has_alpha);
    let comps = &d.comp[..color_components];
    let depth = comps[0].depth as u32;
    if comps.iter().any(|c| c.depth as u32 != depth) || depth > 16 {
        return None;
    }
    let bytes = if depth <= 8 { 1 } else { 2 };
    let quantization = Quantization {
        bit_depth: depth,
        container_bits: bytes * 8,
        msb_aligned: comps[0].shift > 0,
        full_range: frame.color_range() == ffmpeg::color::Range::JPEG,
    };
    let (narrow, wide) = if bytes == 1 {
        (PlaneFormat::R8, PlaneFormat::Rg8)
    } else {
        (PlaneFormat::R16, PlaneFormat::Rg16)
    };
    let step = |c: &ffi::AVComponentDescriptor| c.step as u32;
    let is_rgb = d.flags & ffi::AV_PIX_FMT_FLAG_RGB as u64 != 0;
    let (cw, ch) = (d.log2_chroma_w, d.log2_chroma_h);

    let (layout, swap_chroma, matrix, planes) = match color_components {
        1 if step(&comps[0]) == bytes => (
            Layout::OnePlane,
            false,
            ycbcr::gray_to_rgb(quantization),
            vec![(narrow, 0, 0)],
        ),
        // Planar GBR: R is component 0 in plane 2, G in plane 0, B in plane 1.
        3 if is_rgb
            && comps.iter().all(|c| step(c) == bytes)
            && (comps[0].plane, comps[1].plane, comps[2].plane) == (2, 0, 1) =>
        {
            (
                Layout::ThreePlane,
                false,
                ycbcr::gbr_to_rgb(quantization),
                vec![(narrow, 0, 0); 3],
            )
        }
        3 if !is_rgb => {
            let coefficients = color_tags(frame).ycbcr_coefficients(frame.height())?;
            let matrix = ycbcr::ycbcr_to_rgb(coefficients, quantization);
            if comps.iter().all(|c| step(c) == bytes) && (comps[1].plane, comps[2].plane) == (1, 2)
            {
                (
                    Layout::ThreePlane,
                    false,
                    matrix,
                    vec![(narrow, 0, 0), (narrow, cw, ch), (narrow, cw, ch)],
                )
            } else if step(&comps[1]) == 2 * bytes && comps[1].plane == 1 && comps[2].plane == 1 {
                let swap = comps[1].offset > comps[2].offset;
                (
                    Layout::TwoPlane,
                    swap,
                    matrix,
                    vec![(narrow, 0, 0), (wide, cw, ch)],
                )
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let chroma_offset = if is_rgb {
        [0.0, 0.0]
    } else {
        chroma_offset(frame, (cw, ch))
    };
    Some(Classified {
        layout,
        swap_chroma,
        matrix,
        chroma_offset,
        planes,
    })
}

/// Describes software frames the shader reads directly; `None` means
/// convert first.
fn describe(frame: &frame::Video) -> Option<CpuFrame<'_>> {
    let classified = classify(frame.format(), frame)?;
    let planes = classified
        .planes
        .iter()
        .enumerate()
        .map(|(index, &(format, log2_w, log2_h))| Plane {
            format,
            width: frame.width().div_ceil(1 << log2_w),
            height: frame.height().div_ceil(1 << log2_h),
            data: frame.data(index),
            stride: frame.stride(index),
        })
        .collect();
    Some(CpuFrame {
        width: frame.width(),
        height: frame.height(),
        layout: classified.layout,
        swap_chroma: classified.swap_chroma,
        matrix: classified.matrix,
        chroma_offset: classified.chroma_offset,
        planes,
    })
}

/// Offset of the chroma samples from center-siting, in luma pixels, per
/// H.273 `chroma_sample_loc_type`. Unspecified is treated as left-sited
/// (the MPEG-2/H.264 default) for horizontally subsampled formats.
fn chroma_offset(frame: &frame::Video, (log2_w, log2_h): (u8, u8)) -> [f32; 2] {
    use ffmpeg::util::chroma::Location;
    // Left- or top-sited samples sit (factor - 1) / 2 luma pixels before
    // the center of their block.
    let shift = |log2: u8| ((1u32 << log2) - 1) as f32 / 2.0;
    let (horizontal, vertical) = match frame.chroma_location() {
        Location::Center => (0.0, 0.0),
        Location::TopLeft => (shift(log2_w), shift(log2_h)),
        Location::Top => (0.0, shift(log2_h)),
        Location::BottomLeft => (shift(log2_w), -shift(log2_h)),
        Location::Bottom => (0.0, -shift(log2_h)),
        Location::Left | Location::Unspecified => (shift(log2_w), 0.0),
    };
    [horizontal, vertical]
}

/// Full-size plane `index` (no subsampling).
fn plane(frame: &frame::Video, index: usize, format: PlaneFormat) -> Plane<'_> {
    Plane {
        format,
        width: frame.width(),
        height: frame.height(),
        data: frame.data(index),
        stride: frame.stride(index),
    }
}

fn convert(
    frame: &frame::Video,
    target: format::Pixel,
    width: u32,
    height: u32,
) -> Result<frame::Video> {
    let mut scaler = scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        target,
        width,
        height,
        // Interpolate chroma properly; the default fast paths replicate it.
        scaling::Flags::AREA
            | scaling::Flags::FULL_CHR_H_INT
            | scaling::Flags::FULL_CHR_H_INP
            | scaling::Flags::ACCURATE_RND,
    )?;
    set_colorspace(&mut scaler, frame);
    let mut out = frame::Video::empty();
    scaler.run(frame, &mut out)?;
    Ok(out)
}

/// swscale assumes BT.601 for YUV input; use the frame's matrix and range
/// instead (untagged HD is assumed BT.709). Ignored for RGB input.
fn set_colorspace(scaler: &mut scaling::Context, frame: &frame::Video) {
    let matrix = match ffi::AVColorSpace::from(frame.color_space()) as i32 {
        2 if frame.height() >= 720 => ffi::SWS_CS_ITU709,
        2 => ffi::SWS_CS_ITU601,
        matrix => matrix,
    };
    let full_range = i32::from(frame.color_range() == ffmpeg::color::Range::JPEG);
    // SAFETY: the scaler is valid; sws_getCoefficients returns static
    // tables (falling back to BT.601 for unknown values).
    unsafe {
        ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            ffi::sws_getCoefficients(matrix),
            full_range,
            ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
            1,
            0,
            1 << 16,
            1 << 16,
        );
    }
}
