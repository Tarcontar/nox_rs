//! The working space pictures are composited in, and the transforms into
//! and out of it.
//!
//! The working space is linear ACEScg (AP1 primaries, ACES white), scaled
//! so that SDR reference white (203 nits, BT.2408) is 1.0. Sources enter
//! through their transfer function and primaries (`InputTransform`). For
//! SDR output, HDR sources are tone-mapped to SDR on the way in (`hdr`);
//! for HDR output they keep their light (`InputTransform::apply_hdr`), up
//! to about 49 for 10000 nits. Pictures leave through `OutputEncoding`: SDR
//! is BT.709 primaries and BT.1886 encoding, the transfer SDR sources are
//! decoded with, so SDR BT.709 video comes out with the code values it went
//! in with; HDR is BT.2020 with PQ (HDR10) or HLG.
//!
//! `shaders/ycbcr_to_rgb.comp`, `output.comp` and `output_yuv.comp` in
//! `nox-gpu` do the same on the GPU; tests compare them with this.

use crate::hdr::{SDR_WHITE_NITS, ToneMapping, hlg_encode, pq_inverse_eotf};
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

    /// R'G'B' (0–1) to linear working-space RGB for an HDR output: HDR
    /// sources keep their light (relative to SDR white, so above 1 for
    /// highlights) instead of being tone-mapped.
    pub fn apply_hdr(&self, rgb: [f32; 3]) -> [f32; 3] {
        if !self.tone.is_hdr() {
            return self.apply(rgb);
        }
        let light = self.tone.light(rgb).map(|v| v / SDR_WHITE_NITS);
        mul(
            &self.tone.gamut.primaries().conversion_to(WORKING_PRIMARIES),
            light,
        )
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

/// How pictures leave the working space; matches the shaders' output
/// `encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputEncoding {
    /// BT.709, BT.1886 (`OutputTransform`).
    #[default]
    Sdr = 0,
    /// BT.2020, SMPTE ST 2084 (HDR10).
    Pq = 1,
    /// BT.2020, hybrid log-gamma for a 1000-nit display.
    Hlg = 2,
}

impl OutputEncoding {
    pub fn is_hdr(self) -> bool {
        self != Self::Sdr
    }

    pub fn primaries(self) -> Primaries {
        match self {
            Self::Sdr => OutputTransform::PRIMARIES,
            Self::Pq | Self::Hlg => Primaries::Bt2020,
        }
    }

    /// The H.273 (CICP) tags of the output: primaries, transfer, matrix.
    pub fn cicp(self) -> Cicp {
        match self {
            Self::Sdr => Cicp {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false,
            },
            Self::Pq => Cicp {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            },
            Self::Hlg => Cicp {
                primaries: 9,
                transfer: 18,
                matrix: 9,
                full_range: false,
            },
        }
    }

    /// Linear working-space RGB to output R'G'B' (0–1).
    pub fn encode(self, rgb: [f32; 3]) -> [f32; 3] {
        if self == Self::Sdr {
            return OutputTransform.apply(rgb);
        }
        let linear = mul(&WORKING_PRIMARIES.conversion_to(Primaries::Bt2020), rgb);
        let nits = linear.map(|v| v.max(0.0) * SDR_WHITE_NITS);
        match self {
            Self::Pq => nits.map(|v| pq_inverse_eotf(v / 10_000.0)),
            _ => hlg_encode(nits),
        }
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

#[cfg(test)]
mod hdr_output_tests {
    use super::*;
    use crate::hdr::{Gamut, HdrTransfer};

    #[test]
    fn hdr_outputs_put_sdr_white_at_the_reference_levels() {
        // SDR white: 203 nits, PQ 0.58 (BT.2408), HLG 0.75.
        let pq = OutputEncoding::Pq.encode([1.0; 3]);
        assert!(pq.iter().all(|v| (v - 0.5807).abs() < 1e-3), "{pq:?}");
        let hlg = OutputEncoding::Hlg.encode([1.0; 3]);
        assert!(hlg.iter().all(|v| (v - 0.75).abs() < 2e-3), "{hlg:?}");
        // Black stays black; SDR is unchanged.
        assert_eq!(
            OutputEncoding::Pq.encode([0.0; 3]),
            [pq_inverse_eotf(0.0); 3]
        );
        assert_eq!(
            OutputEncoding::Sdr.encode([0.5; 3]),
            OutputTransform.apply([0.5; 3])
        );
    }

    #[test]
    fn hdr_sources_keep_their_light_for_hdr_output() {
        let tone = |transfer| ToneMapping {
            transfer,
            gamut: Gamut::Bt2020,
            source_peak: 1000.0,
        };
        let input = InputTransform {
            tone: tone(HdrTransfer::Pq),
            ..InputTransform::default()
        };
        // A 1000-nit PQ grey is about 4.93 × SDR white, and comes back out.
        let signal = pq_inverse_eotf(0.1);
        let working = input.apply_hdr([signal; 3]);
        assert!(
            working.iter().all(|v| (v - 1000.0 / 203.0).abs() < 1e-2),
            "{working:?}"
        );
        let out = OutputEncoding::Pq.encode(working);
        assert!(out.iter().all(|v| (v - signal).abs() < 1e-3), "{out:?}");
        // HLG round trips through its OOTF too.
        let hlg = InputTransform {
            tone: tone(HdrTransfer::Hlg),
            ..InputTransform::default()
        };
        let out = OutputEncoding::Hlg.encode(hlg.apply_hdr([0.6; 3]));
        assert!(out.iter().all(|v| (v - 0.6).abs() < 2e-3), "{out:?}");
        // For SDR output the same source is tone-mapped to at most white.
        assert!(input.apply([signal; 3]).iter().all(|&v| v <= 1.01));
    }
}
