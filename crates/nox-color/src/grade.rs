//! Primary grading: lift, gamma, gain and offset, as on the color wheels
//! of DaVinci Resolve, applied per clip before compositing.
//!
//! The grade works on ACEScct values (S-2016-001), the log encoding ACES
//! defines for grading: in linear light, lift and gamma would act almost
//! only on the darkest shadows. Each wheel has an R, G, B balance plus a
//! master value; per channel they fold into one formula,
//!
//! ```text
//! v' = (v·(gain − lift) + lift + offset) ^ (1/gamma)
//! ```
//!
//! so lift moves the bottom of the range (pivoting at 1.0), gain the top
//! (pivoting at 0), gamma the middle, and offset everything. The neutral
//! grade changes nothing. `shaders/blend.comp` in `nox-gpu` does the same.

/// ACEScct value of linear 0 (the toe's offset).
const CCT_TOE: f32 = 0.072_905_53;
const CCT_SLOPE: f32 = 10.540_238;
/// Linear value where the toe meets the log curve.
const CCT_LINEAR_BREAK: f32 = 0.007_812_5;
/// ACEScct value at that point.
const CCT_BREAK: f32 = 0.155_251_15;

/// Linear (AP1) to ACEScct.
pub fn acescct_from_linear(lin: f32) -> f32 {
    if lin <= CCT_LINEAR_BREAK {
        CCT_SLOPE * lin + CCT_TOE
    } else {
        (lin.log2() + 9.72) / 17.52
    }
}

/// ACEScct to linear (AP1), up to the half-float maximum.
pub fn linear_from_acescct(cct: f32) -> f32 {
    if cct <= CCT_BREAK {
        (cct - CCT_TOE) / CCT_SLOPE
    } else {
        (cct * 17.52 - 9.72).exp2().min(65504.0)
    }
}

/// One wheel: R, G, B balance and a master value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wheel {
    pub rgb: [f32; 3],
    pub master: f32,
}

impl Wheel {
    /// Neutral for additive wheels (lift, offset).
    pub const ZERO: Self = Self {
        rgb: [0.0; 3],
        master: 0.0,
    };
    /// Neutral for multiplicative wheels (gamma, gain).
    pub const ONE: Self = Self {
        rgb: [1.0; 3],
        master: 1.0,
    };

    fn sum(&self, c: usize) -> f32 {
        self.rgb[c] + self.master
    }

    fn product(&self, c: usize) -> f32 {
        self.rgb[c] * self.master
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grade {
    /// Added, weighted towards the shadows (ACEScct units).
    pub lift: Wheel,
    /// Power for the midtones (> 0; larger is brighter).
    pub gamma: Wheel,
    /// Multiplier, weighted towards the highlights.
    pub gain: Wheel,
    /// Added everywhere (ACEScct units).
    pub offset: Wheel,
}

impl Default for Grade {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

/// The grade as per-channel coefficients: `v' = max(v·slope + add, 0)^power`
/// on ACEScct values. What the shader receives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeCoefficients {
    pub slope: [f32; 3],
    pub add: [f32; 3],
    pub power: [f32; 3],
}

impl Grade {
    pub const NEUTRAL: Self = Self {
        lift: Wheel::ZERO,
        gamma: Wheel::ONE,
        gain: Wheel::ONE,
        offset: Wheel::ZERO,
    };

    pub fn is_neutral(&self) -> bool {
        *self == Self::NEUTRAL
    }

    pub fn coefficients(&self) -> GradeCoefficients {
        let per_channel = |f: &dyn Fn(usize) -> f32| [0, 1, 2].map(f);
        GradeCoefficients {
            slope: per_channel(&|c| self.gain.product(c) - self.lift.sum(c)),
            add: per_channel(&|c| self.lift.sum(c) + self.offset.sum(c)),
            power: per_channel(&|c| 1.0 / self.gamma.product(c).max(0.01)),
        }
    }

    /// Grades linear working-space RGB.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.is_neutral() {
            return rgb;
        }
        let k = self.coefficients();
        [0, 1, 2].map(|c| {
            let v = acescct_from_linear(rgb[c]);
            let graded = (v * k.slope[c] + k.add[c]).max(0.0).powf(k.power[c]);
            linear_from_acescct(graded)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acescct_round_trips_and_matches_the_spec() {
        for lin in [0.0, 0.001, 0.0078125, 0.01, 0.18, 1.0, 16.0, 222.0] {
            let back = linear_from_acescct(acescct_from_linear(lin));
            assert!((back - lin).abs() <= lin * 1e-4 + 1e-6, "{lin}: {back}");
        }
        // S-2016-001: 18% grey is 0.4136; linear 0 is 0.0729.
        assert!((acescct_from_linear(0.18) - 0.413_588).abs() < 1e-4);
        assert!((acescct_from_linear(0.0) - 0.072_905).abs() < 1e-5);
        // The two pieces meet.
        let below = acescct_from_linear(CCT_LINEAR_BREAK - 1e-7);
        let above = acescct_from_linear(CCT_LINEAR_BREAK + 1e-7);
        assert!((below - above).abs() < 1e-4);
    }

    #[test]
    fn neutral_changes_nothing() {
        let rgb = [0.18, 0.5, 2.0];
        assert_eq!(Grade::NEUTRAL.apply(rgb), rgb);
        let k = Grade::NEUTRAL.coefficients();
        assert_eq!((k.slope, k.add, k.power), ([1.0; 3], [0.0; 3], [1.0; 3]));
    }

    #[test]
    fn wheels_move_their_part_of_the_range() {
        let cct = |lin: f32| acescct_from_linear(lin);
        let grey = 0.18;
        // Gain brightens highlights more than shadows (in ACEScct).
        let gain = Grade {
            gain: Wheel {
                master: 1.2,
                ..Wheel::ONE
            },
            ..Grade::NEUTRAL
        };
        let bright = cct(gain.apply([4.0; 3])[0]) - cct(4.0);
        let dark = cct(gain.apply([0.01; 3])[0]) - cct(0.01);
        assert!(bright > dark && dark > 0.0, "{bright} {dark}");
        // Lift raises shadows more than highlights.
        let lift = Grade {
            lift: Wheel {
                master: 0.05,
                ..Wheel::ZERO
            },
            ..Grade::NEUTRAL
        };
        let dark = cct(lift.apply([0.01; 3])[0]) - cct(0.01);
        let bright = cct(lift.apply([4.0; 3])[0]) - cct(4.0);
        assert!(dark > bright && bright > 0.0, "{dark} {bright}");
        // Gamma > 1 brightens the middle, keeps black and 1.0 (ACEScct).
        let gamma = Grade {
            gamma: Wheel {
                master: 1.3,
                ..Wheel::ONE
            },
            ..Grade::NEUTRAL
        };
        assert!(gamma.apply([grey; 3])[0] > grey);
        // Offset shifts every level by the same ACEScct amount.
        let offset = Grade {
            offset: Wheel {
                rgb: [0.02, 0.0, 0.0],
                master: 0.0,
            },
            ..Grade::NEUTRAL
        };
        for lin in [0.05f32, 1.0, 8.0] {
            let out = offset.apply([lin; 3]);
            assert!((cct(out[0]) - cct(lin) - 0.02).abs() < 1e-4);
            assert!((out[1] - lin).abs() < lin * 1e-5);
        }
    }
}
