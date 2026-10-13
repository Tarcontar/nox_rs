//! SDR transfer functions: signal (0–1) to display light relative to
//! reference white, and back. HDR signals (PQ, HLG) are handled by
//! `hdr`, which tone-maps them.

/// The discriminant is the shaders' `TRANSFER_*` value (generated from this
/// enum by `nox-gpu`'s build script).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// BT.1886: a pure 2.4 power (zero black level), the reference display
    /// for BT.709 and BT.2020 SDR video.
    #[default]
    Bt1886 = 0,
    /// IEC 61966-2-1 sRGB, piecewise.
    Srgb = 1,
    Gamma22 = 2,
    Gamma28 = 3,
    Linear = 4,
}

impl Transfer {
    /// Every variant, in discriminant order.
    pub const ALL: [Self; 5] = [
        Self::Bt1886,
        Self::Srgb,
        Self::Gamma22,
        Self::Gamma28,
        Self::Linear,
    ];

    /// From an H.273 `TransferCharacteristics` code point. Video curves
    /// (BT.709, BT.601, BT.2020, SMPTE 240M) and unspecified ones are shown
    /// on a BT.1886 display, as in other editors and players; so are HDR
    /// curves, which `hdr` tone-maps to BT.1886 first.
    pub fn from_cicp(code: u8) -> Self {
        match code {
            4 => Self::Gamma22,
            5 => Self::Gamma28,
            8 => Self::Linear,
            13 => Self::Srgb,
            _ => Self::Bt1886,
        }
    }

    /// Signal to relative display light.
    pub fn eotf(self, v: f32) -> f32 {
        let v = v.clamp(0.0, 1.0);
        match self {
            Self::Bt1886 => v.powf(2.4),
            Self::Srgb if v <= 0.04045 => v / 12.92,
            Self::Srgb => ((v + 0.055) / 1.055).powf(2.4),
            Self::Gamma22 => v.powf(2.2),
            Self::Gamma28 => v.powf(2.8),
            Self::Linear => v,
        }
    }

    /// Relative display light to signal.
    pub fn inverse_eotf(self, l: f32) -> f32 {
        let l = l.clamp(0.0, 1.0);
        match self {
            Self::Bt1886 => l.powf(1.0 / 2.4),
            Self::Srgb if l <= 0.003_130_8 => l * 12.92,
            Self::Srgb => 1.055 * l.powf(1.0 / 2.4) - 0.055,
            Self::Gamma22 => l.powf(1.0 / 2.2),
            Self::Gamma28 => l.powf(1.0 / 2.8),
            Self::Linear => l,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverses_round_trip() {
        for t in Transfer::ALL {
            for i in 0..=20 {
                let v = i as f32 / 20.0;
                let back = t.inverse_eotf(t.eotf(v));
                assert!((back - v).abs() < 1e-5, "{t:?} at {v}: {back}");
            }
        }
    }

    #[test]
    fn known_values() {
        assert!((Transfer::Srgb.eotf(0.5) - 0.214_041).abs() < 1e-5);
        assert!((Transfer::Bt1886.eotf(0.5) - 0.189_464_6).abs() < 1e-6);
        assert_eq!(Transfer::from_cicp(1), Transfer::Bt1886);
        assert_eq!(Transfer::from_cicp(2), Transfer::Bt1886);
        assert_eq!(Transfer::from_cicp(13), Transfer::Srgb);
        for (i, t) in Transfer::ALL.into_iter().enumerate() {
            assert_eq!(t as usize, i);
        }
    }
}
