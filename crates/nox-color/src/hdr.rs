//! Showing HDR video (PQ and HLG, Rec. ITU-R BT.2100) on SDR displays.
//!
//! The signal is decoded to display light in nits, converted from the
//! source primaries to BT.709, tone-mapped with the BT.2390 EETF (applied
//! to the largest component, which preserves hue) so that the content's
//! peak lands on SDR reference white (203 nits, BT.2408), and encoded with
//! the BT.1886 display gamma, like SDR video. This mirrors
//! `shaders/ycbcr_to_rgb.comp` in `nox-gpu`, which tests compare against.
//! In the working space (`working`) the same mapping is used without the
//! final encoding. Proper HDR output is M6.

use crate::Cicp;

/// Luminance of SDR reference white (BT.2408) and of the tone-mapped peak.
pub const SDR_WHITE_NITS: f32 = 203.0;
/// Assumed content peak when the stream doesn't say (HDR10 and HLG's
/// nominal display).
pub const DEFAULT_PEAK_NITS: f32 = 1000.0;

/// Encoding of an HDR signal; matches the shader's `hdr_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HdrTransfer {
    /// SDR: shown as is.
    #[default]
    None = 0,
    /// SMPTE ST 2084 (HDR10).
    Pq = 1,
    /// Hybrid log-gamma.
    Hlg = 2,
}

/// Source primaries, converted to BT.709 for HDR; matches the shader's
/// `hdr_gamut`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gamut {
    #[default]
    Bt709 = 0,
    Bt2020 = 1,
    DisplayP3 = 2,
}

impl Gamut {
    /// The primaries these stand for.
    pub fn primaries(self) -> crate::Primaries {
        match self {
            Self::Bt709 => crate::Primaries::Bt709,
            Self::Bt2020 => crate::Primaries::Bt2020,
            Self::DisplayP3 => crate::Primaries::DisplayP3,
        }
    }

    /// Linear RGB in these primaries to linear BT.709 RGB (same white).
    pub fn to_bt709(self) -> [[f32; 3]; 3] {
        match self {
            Self::Bt709 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            // BT.2087.
            Self::Bt2020 => [
                [1.660_491, -0.587_641, -0.072_850],
                [-0.124_550, 1.132_9, -0.008_349],
                [-0.018_151, -0.100_579, 1.118_73],
            ],
            Self::DisplayP3 => [
                [1.224_94, -0.224_940, 0.0],
                [-0.042_057, 1.042_057, 0.0],
                [-0.019_638, -0.078_636, 1.098_274],
            ],
        }
    }
}

/// How to show a frame on an SDR display.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ToneMapping {
    pub transfer: HdrTransfer,
    pub gamut: Gamut,
    /// Brightest content, in nits (MaxCLL or the mastering display's peak).
    pub source_peak: f32,
}

impl ToneMapping {
    /// For a frame tagged `cicp`, with the content peak from its HDR
    /// metadata if known. SDR frames get `HdrTransfer::None`.
    pub fn for_cicp(cicp: Cicp, content_peak: Option<f32>) -> Self {
        let transfer = match cicp.transfer {
            16 => HdrTransfer::Pq,
            18 => HdrTransfer::Hlg,
            _ => return Self::default(),
        };
        let gamut = match cicp.primaries {
            9 => Gamut::Bt2020,
            11 | 12 => Gamut::DisplayP3,
            _ => Gamut::Bt709,
        };
        let source_peak = match transfer {
            // HLG is relative; its nominal display is 1000 nits.
            HdrTransfer::Hlg => DEFAULT_PEAK_NITS,
            _ => content_peak
                .filter(|p| p.is_finite() && *p > SDR_WHITE_NITS)
                .unwrap_or(DEFAULT_PEAK_NITS),
        };
        Self {
            transfer,
            gamut,
            source_peak,
        }
    }

    pub fn is_hdr(&self) -> bool {
        self.transfer != HdrTransfer::None
    }

    /// Maps a non-linear R'G'B' value (0–1, after the Y'CbCr matrix) to
    /// display R'G'B' for SDR.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if !self.is_hdr() {
            return rgb;
        }
        self.apply_linear(rgb).map(|v| v.powf(1.0 / 2.4))
    }

    /// Display light in nits, in the source's primaries, without tone
    /// mapping: what an HDR output keeps. HDR frames only.
    pub fn light(&self, rgb: [f32; 3]) -> [f32; 3] {
        match self.transfer {
            HdrTransfer::None => panic!("light on an SDR frame"),
            HdrTransfer::Pq => rgb.map(|v| pq_eotf(v) * 10_000.0),
            HdrTransfer::Hlg => hlg_display_light(rgb, self.gamut),
        }
    }

    /// Like `apply`, but returns linear BT.709 light relative to SDR white
    /// (0–1), before the BT.1886 encoding. HDR frames only.
    pub fn apply_linear(&self, rgb: [f32; 3]) -> [f32; 3] {
        let light = match self.transfer {
            HdrTransfer::None => panic!("apply_linear on an SDR frame"),
            HdrTransfer::Pq => rgb.map(|v| pq_eotf(v) * 10_000.0),
            HdrTransfer::Hlg => hlg_display_light(rgb, self.gamut),
        };
        let m = self.gamut.to_bt709();
        let linear: [f32; 3] = [0, 1, 2]
            .map(|r| (m[r][0] * light[0] + m[r][1] * light[1] + m[r][2] * light[2]).max(0.0));
        let peak = linear[0].max(linear[1]).max(linear[2]);
        let scale = if peak > 0.0 {
            eetf(peak, self.source_peak, SDR_WHITE_NITS) / peak
        } else {
            0.0
        };
        linear.map(|v| (v * scale / SDR_WHITE_NITS).clamp(0.0, 1.0))
    }
}

const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// PQ signal (0–1) to linear light relative to 10000 nits.
pub fn pq_eotf(signal: f32) -> f32 {
    let p = signal.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// Linear light relative to 10000 nits to PQ signal.
pub fn pq_inverse_eotf(light: f32) -> f32 {
    let y = light.clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

/// HLG signal to display light in nits on a 1000-nit display (inverse
/// OETF, then the BT.2100 OOTF with system gamma 1.2).
fn hlg_display_light(rgb: [f32; 3], gamut: Gamut) -> [f32; 3] {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let scene = rgb.map(|e| {
        let e = e.clamp(0.0, 1.0);
        if e <= 0.5 {
            e * e / 3.0
        } else {
            (((e - C) / A).exp() + B) / 12.0
        }
    });
    let weights = match gamut {
        Gamut::Bt2020 => [0.2627, 0.6780, 0.0593],
        _ => [0.2126, 0.7152, 0.0722],
    };
    let y = weights[0] * scene[0] + weights[1] * scene[1] + weights[2] * scene[2];
    let gain = DEFAULT_PEAK_NITS * y.max(1e-6).powf(0.2);
    scene.map(|v| v * gain)
}

/// HLG OETF (BT.2100): scene light (0–1) to signal.
pub fn hlg_oetf(scene: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let e = scene.max(0.0);
    if e <= 1.0 / 12.0 {
        (3.0 * e).sqrt()
    } else {
        A * (12.0 * e - B).ln() + C
    }
}

/// BT.2020 display light in nits to an HLG signal for the nominal
/// 1000-nit display: the inverse of the BT.2100 OOTF (system gamma 1.2),
/// then the OETF. Light above the display's peak is clipped.
pub fn hlg_encode(light: [f32; 3]) -> [f32; 3] {
    let display = light.map(|v| (v / DEFAULT_PEAK_NITS).clamp(0.0, 1.0));
    let y = 0.2627 * display[0] + 0.6780 * display[1] + 0.0593 * display[2];
    // Fd = Ys^(γ-1) · E with Yd = Ys^γ, so E = Fd · Yd^((1-γ)/γ).
    let gain = if y > 0.0 {
        y.powf((1.0 - 1.2) / 1.2)
    } else {
        0.0
    };
    display.map(|v| hlg_oetf((v * gain).min(1.0)))
}

/// BT.2390 EETF: maps `nits` from content with peak `source_peak` onto a
/// display with peak `target_peak`, compressing the highlights smoothly
/// above a knee and leaving darker values (almost) unchanged. Works in PQ
/// space, as the standard specifies.
pub fn eetf(nits: f32, source_peak: f32, target_peak: f32) -> f32 {
    if source_peak <= target_peak {
        return nits.min(target_peak);
    }
    let source = pq_inverse_eotf(source_peak / 10_000.0);
    let e1 = pq_inverse_eotf(nits / 10_000.0) / source;
    let max_lum = pq_inverse_eotf(target_peak / 10_000.0) / source;
    let knee = 1.5 * max_lum - 0.5;
    let e2 = if e1 < knee {
        e1
    } else {
        // Hermite spline from the knee to max_lum.
        let t = (e1.min(1.0) - knee) / (1.0 - knee);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    pq_eotf(e2 * source) * 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDR10: Cicp = Cicp {
        primaries: 9,
        transfer: 16,
        matrix: 9,
        full_range: false,
    };

    #[test]
    fn pq_round_trip_and_reference_points() {
        // 100 nits is PQ 0.508, 1000 nits 0.752 (BT.2100 tables).
        assert!((pq_inverse_eotf(0.01) - 0.508).abs() < 0.001);
        assert!((pq_inverse_eotf(0.1) - 0.752).abs() < 0.001);
        for v in [0.0, 0.1, 0.5, 0.75, 1.0] {
            assert!((pq_inverse_eotf(pq_eotf(v)) - v).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn eetf_keeps_shadows_and_compresses_highlights() {
        let map = |n| eetf(n, 1000.0, SDR_WHITE_NITS);
        assert!((map(10.0) - 10.0).abs() < 0.1);
        assert!((map(1000.0) - SDR_WHITE_NITS).abs() < 0.5);
        assert!(map(2000.0) <= SDR_WHITE_NITS + 0.5);
        // Monotonic.
        let mut last = 0.0;
        for n in (0..=1000).step_by(10) {
            let m = map(n as f32);
            assert!(m >= last, "{n}");
            last = m;
        }
        // Content that fits is left alone.
        assert_eq!(eetf(150.0, 180.0, SDR_WHITE_NITS), 150.0);
    }

    #[test]
    fn tone_mapping_for_tags() {
        let sdr = ToneMapping::for_cicp(Cicp::BT709, None);
        assert!(!sdr.is_hdr());
        assert_eq!(sdr.apply([0.2, 0.5, 0.9]), [0.2, 0.5, 0.9]);

        let hdr = ToneMapping::for_cicp(HDR10, Some(4000.0));
        assert_eq!(
            (hdr.transfer, hdr.gamut, hdr.source_peak),
            (HdrTransfer::Pq, Gamut::Bt2020, 4000.0)
        );
        assert_eq!(
            ToneMapping::for_cicp(HDR10, None).source_peak,
            DEFAULT_PEAK_NITS
        );

        // HDR reference white (203 nits, PQ 0.58) becomes nearly full SDR
        // white; black stays black; the peak is full white.
        let hdr = ToneMapping::for_cicp(HDR10, Some(1000.0));
        let white = pq_inverse_eotf(SDR_WHITE_NITS / 10_000.0);
        let w = hdr.apply([white; 3]);
        assert!(w.iter().all(|&c| (0.9..=1.0).contains(&c)), "{w:?}");
        assert_eq!(hdr.apply([0.0; 3]), [0.0; 3]);
        let p = hdr.apply([0.752; 3]);
        assert!(p.iter().all(|&c| c > 0.99), "{p:?}");
        // A saturated BT.2020 green is out of BT.709 gamut: clipped, but
        // still green.
        let g = hdr.apply([0.0, 0.6, 0.0]);
        assert!(g[1] > 0.5 && g[0] == 0.0 && g[2] == 0.0, "{g:?}");

        let hlg = ToneMapping::for_cicp(
            Cicp {
                transfer: 18,
                ..HDR10
            },
            None,
        );
        let mid = hlg.apply([0.75; 3]); // HLG reference white
        assert!(mid.iter().all(|&c| (0.85..=1.0).contains(&c)), "{mid:?}");
    }
}
