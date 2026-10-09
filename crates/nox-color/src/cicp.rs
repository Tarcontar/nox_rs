//! ITU-T H.273 coding-independent code points (CICP): the color tags carried
//! by video streams. FFmpeg's `AVColorPrimaries`, `AVColorTransferCharacteristic`
//! and `AVColorSpace` use the same numeric values.

use std::fmt;

/// Color description of a video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cicp {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// `true` for full range (0–255 for 8 bit), `false` for limited/video range.
    pub full_range: bool,
}

/// Code point meaning "unspecified" for all three tables.
pub const UNSPECIFIED: u8 = 2;

impl Cicp {
    /// Rec. ITU-R BT.709, limited range: the usual HD video tagging.
    pub const BT709: Self = Self {
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
    };

    pub fn is_specified(&self) -> bool {
        self.primaries != UNSPECIFIED || self.transfer != UNSPECIFIED || self.matrix != UNSPECIFIED
    }

    pub fn primaries_name(&self) -> &'static str {
        match self.primaries {
            1 => "BT.709",
            4 => "BT.470M",
            5 => "BT.601 625",
            6 => "BT.601 525",
            7 => "SMPTE 240M",
            8 => "Generic film",
            9 => "BT.2020",
            10 => "XYZ",
            11 => "DCI-P3",
            12 => "Display P3",
            22 => "EBU 3213",
            UNSPECIFIED => "unspecified",
            _ => "reserved",
        }
    }

    pub fn transfer_name(&self) -> &'static str {
        match self.transfer {
            1 | 6 | 14 | 15 => "BT.709",
            4 => "Gamma 2.2",
            5 => "Gamma 2.8",
            7 => "SMPTE 240M",
            8 => "Linear",
            9 => "Log 100:1",
            10 => "Log 316:1",
            11 => "IEC 61966-2-4",
            12 => "BT.1361",
            13 => "sRGB",
            16 => "PQ",
            17 => "SMPTE 428",
            18 => "HLG",
            UNSPECIFIED => "unspecified",
            _ => "reserved",
        }
    }

    pub fn matrix_name(&self) -> &'static str {
        match self.matrix {
            0 => "Identity (RGB)",
            1 => "BT.709",
            4 => "FCC",
            5 | 6 => "BT.601",
            7 => "SMPTE 240M",
            8 => "YCgCo",
            9 => "BT.2020 NCL",
            10 => "BT.2020 CL",
            11 => "SMPTE 2085",
            12 => "Chroma-derived NCL",
            13 => "Chroma-derived CL",
            14 => "ICtCp",
            UNSPECIFIED => "unspecified",
            _ => "reserved",
        }
    }
}

impl fmt::Display for Cicp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} primaries, {} transfer, {} matrix, {} range",
            self.primaries_name(),
            self.transfer_name(),
            self.matrix_name(),
            if self.full_range { "full" } else { "limited" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(
            Cicp::BT709.to_string(),
            "BT.709 primaries, BT.709 transfer, BT.709 matrix, limited range"
        );
        let hdr = Cicp {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: false,
        };
        assert_eq!(hdr.transfer_name(), "PQ");
        assert!(
            !Cicp {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false
            }
            .is_specified()
        );
    }
}
