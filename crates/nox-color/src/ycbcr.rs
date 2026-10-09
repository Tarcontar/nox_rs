//! Y'CbCr → R'G'B' conversion per ITU-T H.273 / BT.709 / BT.2020.
//!
//! Produces an affine 3×4 matrix that maps *normalized texel values* — what
//! a GPU returns when sampling a UNORM texture — straight to non-linear
//! R'G'B', so shaders only need one matrix multiply. Quantization (range,
//! bit depth, how samples sit in their container) is folded into the matrix.

use crate::Cicp;

/// Rows of an affine transform applied to `(c0, c1, c2, 1)`.
pub type Matrix3x4 = [[f32; 4]; 3];

/// Luma coefficients `K_R`, `K_B` of a Y'CbCr matrix (`K_G = 1 - K_R - K_B`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefficients {
    pub kr: f64,
    pub kb: f64,
}

impl Coefficients {
    pub const BT601: Self = Self {
        kr: 0.299,
        kb: 0.114,
    };
    pub const BT709: Self = Self {
        kr: 0.2126,
        kb: 0.0722,
    };
    pub const BT2020: Self = Self {
        kr: 0.2627,
        kb: 0.0593,
    };
}

impl Cicp {
    /// Y'CbCr coefficients for the matrix code point, or `None` if samples
    /// are RGB (identity matrix). Unspecified or unsupported matrices fall
    /// back to BT.709 for HD and BT.601 for SD, like most players.
    pub fn ycbcr_coefficients(&self, height: u32) -> Option<Coefficients> {
        match self.matrix {
            0 => None,
            1 => Some(Coefficients::BT709),
            4 => Some(Coefficients { kr: 0.30, kb: 0.11 }),
            5 | 6 => Some(Coefficients::BT601),
            7 => Some(Coefficients {
                kr: 0.212,
                kb: 0.087,
            }),
            // The constant-luminance variant (10) is approximated by NCL.
            9 | 10 => Some(Coefficients::BT2020),
            _ if height >= 720 => Some(Coefficients::BT709),
            _ => Some(Coefficients::BT601),
        }
    }
}

/// How code values are quantized and stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantization {
    /// Significant bits per sample (8, 10, 12, 16).
    pub bit_depth: u32,
    /// Bits of the storage unit (8 or 16).
    pub container_bits: u32,
    /// Samples are stored in the high bits of the container (e.g. P010).
    pub msb_aligned: bool,
    pub full_range: bool,
}

impl Quantization {
    pub const fn eight_bit(full_range: bool) -> Self {
        Self {
            bit_depth: 8,
            container_bits: 8,
            msb_aligned: false,
            full_range,
        }
    }

    /// Factor turning a normalized texel value into `code / (2^n - 1)`.
    fn texel_scale(&self) -> f64 {
        let container_max = ((1u64 << self.container_bits) - 1) as f64;
        let max = ((1u64 << self.bit_depth) - 1) as f64;
        let shift = if self.msb_aligned {
            (1u64 << (self.container_bits - self.bit_depth)) as f64
        } else {
            1.0
        };
        container_max / (max * shift)
    }

    /// `(scale, offset)` mapping a normalized texel value to Y' in [0, 1].
    fn luma(&self) -> (f64, f64) {
        let s = self.texel_scale();
        if self.full_range {
            (s, 0.0)
        } else {
            // D = (219 Y' + 16) 2^(n-8)  →  Y' = (D / 2^(n-8) - 16) / 219
            let max = ((1u64 << self.bit_depth) - 1) as f64;
            let unit = (1u64 << (self.bit_depth - 8)) as f64;
            (s * max / unit / 219.0, -16.0 / 219.0)
        }
    }

    /// `(scale, offset)` mapping a normalized texel value to Cb/Cr in [-0.5, 0.5].
    fn chroma(&self) -> (f64, f64) {
        let s = self.texel_scale();
        let max = ((1u64 << self.bit_depth) - 1) as f64;
        let half = (1u64 << (self.bit_depth - 1)) as f64;
        if self.full_range {
            // D = (2^n - 1) C + 2^(n-1)
            (s, -half / max)
        } else {
            // D = (224 C + 128) 2^(n-8)
            let unit = (1u64 << (self.bit_depth - 8)) as f64;
            (s * max / unit / 224.0, -128.0 / 224.0)
        }
    }
}

/// Matrix for planar Y'CbCr input `(Y, Cb, Cr)`.
pub fn ycbcr_to_rgb(k: Coefficients, q: Quantization) -> Matrix3x4 {
    let (ys, yo) = q.luma();
    let (cs, co) = q.chroma();
    let kg = 1.0 - k.kr - k.kb;
    // R' = Y' + 2(1-Kr) Cr,  B' = Y' + 2(1-Kb) Cb,  G' = (Y' - Kr R' - Kb B') / Kg
    let r_cr = 2.0 * (1.0 - k.kr);
    let b_cb = 2.0 * (1.0 - k.kb);
    let g_cb = -k.kb * b_cb / kg;
    let g_cr = -k.kr * r_cr / kg;
    let row = |cb: f64, cr: f64| {
        [
            ys as f32,
            (cb * cs) as f32,
            (cr * cs) as f32,
            (yo + (cb + cr) * co) as f32,
        ]
    };
    [row(0.0, r_cr), row(g_cb, g_cr), row(b_cb, 0.0)]
}

/// Matrix from R'G'B' (0–1) to normalized texel values `(Y, Cb, Cr)` for
/// storage as `q` describes: the exact inverse of `ycbcr_to_rgb`, for
/// encoding.
pub fn rgb_to_ycbcr(k: Coefficients, q: Quantization) -> Matrix3x4 {
    let forward = ycbcr_to_rgb(k, q);
    let m: [[f64; 3]; 3] =
        std::array::from_fn(|r| std::array::from_fn(|c| f64::from(forward[r][c])));
    let t: [f64; 3] = std::array::from_fn(|r| f64::from(forward[r][3]));
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let cofactor = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
    };
    // inverse[r][c] = cofactor(c, r) / det; offset = -inverse · t.
    let inverse: [[f64; 3]; 3] =
        std::array::from_fn(|r| std::array::from_fn(|c| cofactor(c, r) / det));
    std::array::from_fn(|r| {
        let offset = -(0..3).map(|c| inverse[r][c] * t[c]).sum::<f64>();
        [
            inverse[r][0] as f32,
            inverse[r][1] as f32,
            inverse[r][2] as f32,
            offset as f32,
        ]
    })
}

/// Matrix for RGB input in `(R, G, B)` order.
pub fn rgb_to_rgb(q: Quantization) -> Matrix3x4 {
    let (s, o) = q.luma();
    let (s, o) = (s as f32, o as f32);
    [[s, 0.0, 0.0, o], [0.0, s, 0.0, o], [0.0, 0.0, s, o]]
}

/// Matrix for planar RGB stored as `(G, B, R)`, FFmpeg's `gbrp*` formats.
pub fn gbr_to_rgb(q: Quantization) -> Matrix3x4 {
    let (s, o) = q.luma();
    let (s, o) = (s as f32, o as f32);
    [[0.0, 0.0, s, o], [s, 0.0, 0.0, o], [0.0, s, 0.0, o]]
}

/// Matrix for luma-only (gray) input: R' = G' = B' = Y'.
pub fn gray_to_rgb(q: Quantization) -> Matrix3x4 {
    let (s, o) = q.luma();
    let row = [s as f32, 0.0, 0.0, o as f32];
    [row, row, row]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &Matrix3x4, v: [f64; 3]) -> [f64; 3] {
        m.map(|r| r[0] as f64 * v[0] + r[1] as f64 * v[1] + r[2] as f64 * v[2] + r[3] as f64)
    }

    fn assert_close(actual: [f64; 3], expected: [f64; 3]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 2e-3, "{actual:?} != {expected:?}");
        }
    }

    /// Normalized texel values for 8-bit codes.
    fn t8(codes: [u32; 3]) -> [f64; 3] {
        codes.map(|c| c as f64 / 255.0)
    }

    #[test]
    fn limited_range_8bit_black_white_red() {
        let m = ycbcr_to_rgb(Coefficients::BT709, Quantization::eight_bit(false));
        assert_close(apply(&m, t8([16, 128, 128])), [0.0, 0.0, 0.0]);
        assert_close(apply(&m, t8([235, 128, 128])), [1.0, 1.0, 1.0]);
        // BT.709 100% red: Y' = 0.2126, Cb = -0.1146, Cr = 0.5
        let red = [
            16.0 + 219.0 * 0.2126,
            128.0 - 224.0 * 0.114572,
            128.0 + 224.0 * 0.5,
        ];
        assert_close(apply(&m, red.map(|c| c / 255.0)), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn full_range_8bit() {
        let m = ycbcr_to_rgb(Coefficients::BT601, Quantization::eight_bit(true));
        assert_close(apply(&m, t8([0, 128, 128])), [0.0, 0.0, 0.0]);
        assert_close(apply(&m, t8([255, 128, 128])), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn ten_bit_in_16bit_containers() {
        // P010: 10-bit codes in the high bits of 16-bit words.
        let msb = Quantization {
            bit_depth: 10,
            container_bits: 16,
            msb_aligned: true,
            full_range: false,
        };
        let m = ycbcr_to_rgb(Coefficients::BT2020, msb);
        let t = |c: u32| ((c << 6) as f64) / 65535.0;
        assert_close(apply(&m, [t(940), t(512), t(512)]), [1.0, 1.0, 1.0]);
        assert_close(apply(&m, [t(64), t(512), t(512)]), [0.0, 0.0, 0.0]);

        // yuv420p10le: the same codes in the low bits.
        let lsb = Quantization {
            msb_aligned: false,
            ..msb
        };
        let m = ycbcr_to_rgb(Coefficients::BT2020, lsb);
        let t = |c: u32| c as f64 / 65535.0;
        assert_close(apply(&m, [t(940), t(512), t(512)]), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn rgb_orders() {
        let q = Quantization::eight_bit(true);
        assert_close(apply(&gbr_to_rgb(q), [0.2, 0.3, 0.1]), [0.1, 0.2, 0.3]);
        assert_close(apply(&rgb_to_rgb(q), [0.1, 0.2, 0.3]), [0.1, 0.2, 0.3]);
        assert_close(apply(&gray_to_rgb(q), [0.4, 0.9, 0.9]), [0.4, 0.4, 0.4]);
    }

    #[test]
    fn unspecified_matrix_depends_on_height() {
        let untagged = Cicp {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            full_range: false,
        };
        assert_eq!(untagged.ycbcr_coefficients(1080), Some(Coefficients::BT709));
        assert_eq!(untagged.ycbcr_coefficients(576), Some(Coefficients::BT601));
        assert_eq!(
            Cicp {
                matrix: 0,
                ..untagged
            }
            .ycbcr_coefficients(1080),
            None
        );
    }

    #[test]
    fn rgb_to_ycbcr_inverts_ycbcr_to_rgb() {
        let q = Quantization::eight_bit(false);
        let to_ycbcr = rgb_to_ycbcr(Coefficients::BT709, q);
        let to_rgb = ycbcr_to_rgb(Coefficients::BT709, q);
        let apply =
            |m: &Matrix3x4, v: [f32; 3]| m.map(|r| r[0] * v[0] + r[1] * v[1] + r[2] * v[2] + r[3]);
        // White and black: Y' 235/16, chroma 128 (as 8-bit codes).
        let codes = |v: [f32; 3]| v.map(|c| (c * 255.0).round());
        assert_eq!(codes(apply(&to_ycbcr, [1.0; 3])), [235.0, 128.0, 128.0]);
        assert_eq!(codes(apply(&to_ycbcr, [0.0; 3])), [16.0, 128.0, 128.0]);
        for rgb in [[0.2, 0.5, 0.9], [1.0, 0.0, 0.0], [0.3, 0.3, 0.3]] {
            let back = apply(&to_rgb, apply(&to_ycbcr, rgb));
            for c in 0..3 {
                assert!((back[c] - rgb[c]).abs() < 1e-5, "{rgb:?} -> {back:?}");
            }
        }
    }
}
