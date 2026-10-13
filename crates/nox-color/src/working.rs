//! The working space pictures are composited in, and the transforms into
//! and out of it.
//!
//! The working space is linear ACEScg (AP1 primaries, ACES white), scaled
//! so that SDR reference white is 1.0. Sources enter through their
//! transfer function and primaries (`InputTransform`); HDR sources are
//! tone-mapped to SDR on the way in (`hdr`) until the output transforms
//! handle HDR. Pictures leave through `OutputTransform`: BT.709 primaries
//! and BT.1886 encoding, the transfer SDR sources are decoded with, so SDR
//! BT.709 video comes out with the code values it went in with.
//!
//! `shaders/ycbcr_to_rgb.comp`, `output.comp` and `output_yuv.comp` in
//! `nox-gpu` do the same on the GPU; tests compare them with this.

use crate::hdr::ToneMapping;
use crate::{Cicp, Mat3, Primaries, Transfer};

/// Primaries of the working space.
pub const WORKING_PRIMARIES: Primaries = Primaries::Ap1;

/// How a source's R'G'B' signal (after the Y'CbCr matrix) enters the
/// working space.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InputTransform {
    /// For SDR sources.
    pub transfer: Transfer,
    pub primaries: Primaries,
    /// For HDR sources, which ignore `transfer` and `primaries`.
    pub tone: ToneMapping,
}

impl InputTransform {
    /// For a frame tagged `cicp`, with the content peak from its HDR
    /// metadata if known.
    pub fn for_cicp(cicp: Cicp, content_peak: Option<f32>) -> Self {
        Self {
            transfer: Transfer::from_cicp(cicp.transfer),
            primaries: Primaries::from_cicp(cicp.primaries),
            tone: ToneMapping::for_cicp(cicp, content_peak),
        }
    }

    /// R'G'B' (0–1) to linear working-space RGB.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.tone.is_hdr() {
            let linear = self.tone.apply_linear(rgb);
            return mul(&Primaries::Bt709.conversion_to(WORKING_PRIMARIES), linear);
        }
        let linear = rgb.map(|v| self.transfer.eotf(v));
        mul(&self.primaries.conversion_to(WORKING_PRIMARIES), linear)
    }

    /// R'G'B' (0–1) to display R'G'B', without the working space: HDR is
    /// tone-mapped, SDR passes unchanged. For thumbnails and previews of
    /// single sources.
    pub fn display(&self, rgb: [f32; 3]) -> [f32; 3] {
        self.tone.apply(rgb)
    }
}

/// From the working space to the output: BT.709 primaries, clipped to the
/// gamut, BT.1886 encoding.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OutputTransform;

impl OutputTransform {
    pub const PRIMARIES: Primaries = Primaries::Bt709;
    pub const TRANSFER: Transfer = Transfer::Bt1886;

    /// Linear working-space RGB to output R'G'B' (0–1).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let linear = mul(&WORKING_PRIMARIES.conversion_to(Self::PRIMARIES), rgb);
        linear.map(|v| Self::TRANSFER.inverse_eotf(v))
    }
}

fn mul(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_bt709_round_trips() {
        let input = InputTransform::for_cicp(Cicp::BT709, None);
        for rgb in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.25, 0.5, 0.75],
            [0.9, 0.1, 0.4],
        ] {
            let out = OutputTransform.apply(input.apply(rgb));
            for c in 0..3 {
                assert!((out[c] - rgb[c]).abs() < 1e-4, "{rgb:?} → {out:?}");
            }
        }
    }

    #[test]
    fn white_is_one_in_the_working_space() {
        let input = InputTransform::for_cicp(Cicp::BT709, None);
        for v in input.apply([1.0; 3]) {
            assert!((v - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn wide_gamut_sources_are_converted() {
        // Pure BT.2020 green is outside BT.709: it clips, and red and blue
        // become 0 rather than staying the signal values.
        let input = InputTransform::for_cicp(
            Cicp {
                primaries: 9,
                ..Cicp::BT709
            },
            None,
        );
        let out = OutputTransform.apply(input.apply([0.0, 1.0, 0.0]));
        assert_eq!(out[0], 0.0);
        assert!(out[1] > 0.99);
        assert_eq!(out[2], 0.0);
    }

    #[test]
    fn hdr_matches_the_display_path() {
        // On an SDR output, HDR goes through the same tone mapping as the
        // display path.
        let input = InputTransform::for_cicp(
            Cicp {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            },
            Some(1000.0),
        );
        for rgb in [[0.5, 0.5, 0.5], [0.7, 0.3, 0.2], [0.58, 0.58, 0.58]] {
            let via_working = OutputTransform.apply(input.apply(rgb));
            let direct = input.display(rgb);
            for c in 0..3 {
                assert!(
                    (via_working[c] - direct[c]).abs() < 1e-3,
                    "{rgb:?}: {via_working:?} vs {direct:?}"
                );
            }
        }
    }
}
