//! RGB color spaces by their primaries and white point, and the linear
//! conversions between them (via CIE XYZ, with Bradford chromatic
//! adaptation when the white points differ, as in ACES 1).

use crate::Mat3;

/// RGB primaries and white point; the discriminant indexes the shaders'
/// `to_working` function (generated from this enum by `nox-gpu`'s build
/// script).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Primaries {
    #[default]
    Bt709 = 0,
    /// BT.601 625-line (PAL), H.273 code point 5.
    Bt601Pal = 1,
    /// BT.601 525-line (NTSC) and SMPTE 240M, code points 6 and 7.
    Bt601Ntsc = 2,
    Bt2020 = 3,
    /// DCI-P3 with the DCI white (code point 11).
    DciP3 = 4,
    /// P3 primaries with a D65 white (code point 12).
    DisplayP3 = 5,
    /// EBU Tech 3213 (code point 22).
    Ebu3213 = 6,
    /// ACES AP0 (ACES2065-1).
    Ap0 = 7,
    /// ACES AP1 (ACEScg, ACEScct).
    Ap1 = 8,
}

/// CIE 1931 xy chromaticities of red, green, blue and white.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticities {
    pub red: [f64; 2],
    pub green: [f64; 2],
    pub blue: [f64; 2],
    pub white: [f64; 2],
}

const D65: [f64; 2] = [0.3127, 0.3290];
/// The ACES white, near D60.
const ACES_WHITE: [f64; 2] = [0.32168, 0.33767];

impl Primaries {
    /// Every variant, in discriminant order.
    pub const ALL: [Self; 9] = [
        Self::Bt709,
        Self::Bt601Pal,
        Self::Bt601Ntsc,
        Self::Bt2020,
        Self::DciP3,
        Self::DisplayP3,
        Self::Ebu3213,
        Self::Ap0,
        Self::Ap1,
    ];

    /// From an H.273 `ColourPrimaries` code point; unspecified and
    /// unsupported ones are taken as BT.709, the HD default.
    pub fn from_cicp(code: u8) -> Self {
        match code {
            5 => Self::Bt601Pal,
            6 | 7 => Self::Bt601Ntsc,
            9 => Self::Bt2020,
            11 => Self::DciP3,
            12 => Self::DisplayP3,
            22 => Self::Ebu3213,
            _ => Self::Bt709,
        }
    }

    pub fn chromaticities(self) -> Chromaticities {
        let c = |red, green, blue, white| Chromaticities {
            red,
            green,
            blue,
            white,
        };
        match self {
            Self::Bt709 => c([0.64, 0.33], [0.30, 0.60], [0.15, 0.06], D65),
            Self::Bt601Pal => c([0.64, 0.33], [0.29, 0.60], [0.15, 0.06], D65),
            Self::Bt601Ntsc => c([0.630, 0.340], [0.310, 0.595], [0.155, 0.070], D65),
            Self::Bt2020 => c([0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65),
            Self::DciP3 => c(
                [0.680, 0.320],
                [0.265, 0.690],
                [0.150, 0.060],
                [0.314, 0.351],
            ),
            Self::DisplayP3 => c([0.680, 0.320], [0.265, 0.690], [0.150, 0.060], D65),
            Self::Ebu3213 => c([0.630, 0.340], [0.295, 0.605], [0.155, 0.077], D65),
            Self::Ap0 => c([0.7347, 0.2653], [0.0, 1.0], [0.0001, -0.0770], ACES_WHITE),
            Self::Ap1 => c([0.713, 0.293], [0.165, 0.830], [0.128, 0.044], ACES_WHITE),
        }
    }

    /// Linear RGB in these primaries to CIE XYZ (Y of white = 1).
    pub fn to_xyz(self) -> [[f64; 3]; 3] {
        let c = self.chromaticities();
        let xyz = |[x, y]: [f64; 2]| [x / y, 1.0, (1.0 - x - y) / y];
        let (r, g, b) = (xyz(c.red), xyz(c.green), xyz(c.blue));
        let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        // Scale each primary so that RGB (1, 1, 1) is the white point.
        let s = mul_vec(&invert(&m), xyz(c.white));
        m.map(|row| [row[0] * s[0], row[1] * s[1], row[2] * s[2]])
    }

    /// Linear RGB in `self` to linear RGB in `target`, adapting the white
    /// point with Bradford if they differ.
    pub fn conversion_to(self, target: Self) -> Mat3 {
        let from_white = self.chromaticities().white;
        let to_white = target.chromaticities().white;
        let mut m = mul(&invert(&target.to_xyz()), &self.to_xyz());
        if from_white != to_white {
            m = mul(
                &invert(&target.to_xyz()),
                &mul(&bradford(from_white, to_white), &self.to_xyz()),
            );
        }
        m.map(|row| row.map(|v| v as f32))
    }
}

/// Bradford chromatic adaptation from white `from` to white `to`, in XYZ.
fn bradford(from: [f64; 2], to: [f64; 2]) -> [[f64; 3]; 3] {
    const B: [[f64; 3]; 3] = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let xyz = |[x, y]: [f64; 2]| [x / y, 1.0, (1.0 - x - y) / y];
    let (src, dst) = (mul_vec(&B, xyz(from)), mul_vec(&B, xyz(to)));
    let scale = [
        [dst[0] / src[0], 0.0, 0.0],
        [0.0, dst[1] / src[1], 0.0],
        [0.0, 0.0, dst[2] / src[2]],
    ];
    mul(&invert(&B), &mul(&scale, &B))
}

fn mul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [0, 1, 2].map(|r| [0, 1, 2].map(|c| (0..3).map(|k| a[r][k] * b[k][c]).sum()))
}

fn mul_vec(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

fn invert(m: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let cofactor = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
    };
    let det: f64 = (0..3).map(|c| m[0][c] * cofactor(0, c)).sum();
    // The adjugate is the transposed cofactor matrix.
    [0, 1, 2].map(|r| [0, 1, 2].map(|c| cofactor(c, r) / det))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matrix(actual: Mat3, expected: Mat3, tolerance: f32) {
        for r in 0..3 {
            for c in 0..3 {
                assert!(
                    (actual[r][c] - expected[r][c]).abs() <= tolerance,
                    "[{r}][{c}]: {actual:?} vs {expected:?}"
                );
            }
        }
    }

    #[test]
    fn bt709_to_xyz_matches_the_standard() {
        // sRGB/BT.709 matrix (IEC 61966-2-1, unrounded).
        let m = Primaries::Bt709.to_xyz().map(|r| r.map(|v| v as f32));
        let expected = [
            [0.412_391, 0.357_584, 0.180_481],
            [0.212_639, 0.715_169, 0.072_192],
            [0.019_331, 0.119_195, 0.950_532],
        ];
        assert_matrix(m, expected, 1e-5);
    }

    #[test]
    fn conversions_match_published_matrices() {
        // BT.2087 (same white, no adaptation).
        assert_matrix(
            Primaries::Bt2020.conversion_to(Primaries::Bt709),
            crate::hdr::Gamut::Bt2020.to_bt709(),
            1e-4,
        );
        assert_matrix(
            Primaries::DisplayP3.conversion_to(Primaries::Bt709),
            crate::hdr::Gamut::DisplayP3.to_bt709(),
            1e-4,
        );
        // ACEScg to BT.709 with Bradford D60 → D65, as in OCIO's ACES
        // configs ("ACEScg to Linear Rec.709 (sRGB)").
        assert_matrix(
            Primaries::Ap1.conversion_to(Primaries::Bt709),
            [
                [1.705_051, -0.621_792, -0.083_259],
                [-0.130_256, 1.140_805, -0.010_548],
                [-0.024_003, -0.128_969, 1.152_972],
            ],
            1e-4,
        );
        // AP0 to AP1 (ACES S-2014-004).
        assert_matrix(
            Primaries::Ap0.conversion_to(Primaries::Ap1),
            [
                [1.451_439, -0.236_511, -0.214_929],
                [-0.076_554, 1.176_23, -0.099_676],
                [0.008_316, -0.006_032, 0.997_716],
            ],
            1e-4,
        );
    }

    #[test]
    fn round_trips_are_identity() {
        for p in Primaries::ALL {
            let there = p.conversion_to(Primaries::Ap1);
            let back = Primaries::Ap1.conversion_to(p);
            let product: Mat3 = [0, 1, 2]
                .map(|r| [0, 1, 2].map(|c| (0..3).map(|k| back[r][k] * there[k][c]).sum()));
            assert_matrix(
                product,
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                1e-5,
            );
            // White stays white.
            let white = there.map(|row| row.iter().sum::<f32>());
            assert_matrix([white, white, white], [[1.0; 3]; 3], 1e-4);
        }
    }

    #[test]
    fn unknown_code_points_are_bt709() {
        assert_eq!(Primaries::from_cicp(2), Primaries::Bt709);
        assert_eq!(Primaries::from_cicp(1), Primaries::Bt709);
        assert_eq!(Primaries::from_cicp(9), Primaries::Bt2020);
        assert_eq!(Primaries::from_cicp(7), Primaries::Bt601Ntsc);
        for (i, p) in Primaries::ALL.into_iter().enumerate() {
            assert_eq!(p as usize, i);
        }
    }
}
