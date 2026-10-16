//! Tensors on the CPU: model weights, constants, and the shape arithmetic
//! that is worked out while planning. Floats for data, 64-bit integers
//! for shapes and indices (and every other integer type ONNX has).

use std::borrow::Cow;
use std::sync::Arc;

use crate::onnx::{TensorProto, data_type};
use crate::{Error, Result};

/// The values, shared: copies of a tensor (planning hands constants
/// around, e.g. as the inputs of every node using them) share them.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    F32(Arc<Vec<f32>>),
    I64(Arc<Vec<i64>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Data,
}

impl Tensor {
    pub fn f32(shape: Vec<usize>, data: Vec<f32>) -> Self {
        debug_assert_eq!(shape.iter().product::<usize>(), data.len());
        Self {
            shape,
            data: Data::F32(Arc::new(data)),
        }
    }

    pub fn i64(shape: Vec<usize>, data: Vec<i64>) -> Self {
        debug_assert_eq!(shape.iter().product::<usize>(), data.len());
        Self {
            shape,
            data: Data::I64(Arc::new(data)),
        }
    }

    /// Elements in all.
    pub fn len(&self) -> usize {
        self.shape.iter().product()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_f32(&self) -> Result<&[f32]> {
        match &self.data {
            Data::F32(data) => Ok(data.as_slice()),
            Data::I64(_) => Err(Error::Model("expected a float tensor".into())),
        }
    }

    /// The values as floats, whatever the type.
    pub fn to_f32(&self) -> Vec<f32> {
        match &self.data {
            Data::F32(data) => data.to_vec(),
            Data::I64(data) => data.iter().map(|&v| v as f32).collect(),
        }
    }

    /// The values as floats, borrowed where they are.
    pub fn f32_values(&self) -> Cow<'_, [f32]> {
        match &self.data {
            Data::F32(data) => Cow::Borrowed(data),
            Data::I64(data) => Cow::Owned(data.iter().map(|&v| v as f32).collect()),
        }
    }

    /// The values as integers, borrowed where they are.
    pub fn i64_values(&self) -> Cow<'_, [i64]> {
        match &self.data {
            Data::F32(data) => Cow::Owned(data.iter().map(|&v| v as i64).collect()),
            Data::I64(data) => Cow::Borrowed(data),
        }
    }

    /// The values as floats, shared with the tensor where they are.
    pub fn shared_f32(&self) -> Arc<Vec<f32>> {
        match &self.data {
            Data::F32(data) => Arc::clone(data),
            Data::I64(data) => Arc::new(data.iter().map(|&v| v as f32).collect()),
        }
    }

    /// The values as integers, whatever the type (floats truncate).
    pub fn to_i64(&self) -> Vec<i64> {
        match &self.data {
            Data::F32(data) => data.iter().map(|&v| v as i64).collect(),
            Data::I64(data) => data.to_vec(),
        }
    }

    pub(crate) fn from_proto(proto: &TensorProto) -> Result<Self> {
        if proto.data_location == 1 {
            return Err(Error::Model(format!(
                "tensor {} keeps its data in another file, which isn't supported",
                proto.name
            )));
        }
        let shape: Vec<usize> = proto.dims.iter().map(|&d| d.max(0) as usize).collect();
        let len: usize = shape.iter().product();
        let raw = &proto.raw_data;
        let f32s = |values: Vec<f32>| Data::F32(Arc::new(values));
        let i64s = |values: Vec<i64>| Data::I64(Arc::new(values));
        let data = match proto.data_type {
            data_type::FLOAT if !raw.is_empty() => f32s(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect(),
            ),
            data_type::FLOAT => f32s(proto.float_data.clone()),
            data_type::DOUBLE if !raw.is_empty() => f32s(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| f64::from_le_bytes(*b) as f32)
                    .collect(),
            ),
            data_type::DOUBLE => f32s(proto.double_data.iter().map(|&v| v as f32).collect()),
            data_type::FLOAT16 if !raw.is_empty() => f32s(
                raw.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f16_to_f32(u16::from_le_bytes(*b)))
                    .collect(),
            ),
            data_type::FLOAT16 => f32s(
                proto
                    .int32_data
                    .iter()
                    .map(|&v| f16_to_f32(v as u16))
                    .collect(),
            ),
            data_type::INT64 if !raw.is_empty() => i64s(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| i64::from_le_bytes(*b))
                    .collect(),
            ),
            data_type::INT64 => i64s(proto.int64_data.clone()),
            data_type::INT32 if !raw.is_empty() => i64s(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| i64::from(i32::from_le_bytes(*b)))
                    .collect(),
            ),
            data_type::INT32 | data_type::INT8 | data_type::BOOL if raw.is_empty() => {
                i64s(proto.int32_data.iter().map(|&v| i64::from(v)).collect())
            }
            data_type::INT8 => i64s(raw.iter().map(|&b| i64::from(b as i8)).collect()),
            data_type::UINT8 | data_type::BOOL => i64s(raw.iter().map(|&b| i64::from(b)).collect()),
            other => {
                return Err(Error::Model(format!(
                    "tensor {} has data type {other}, which isn't supported",
                    proto.name
                )));
            }
        };
        let actual = match &data {
            Data::F32(d) => d.len(),
            Data::I64(d) => d.len(),
        };
        if actual != len {
            return Err(Error::Model(format!(
                "tensor {} has {actual} values for shape {shape:?}",
                proto.name
            )));
        }
        Ok(Self { shape, data })
    }
}

/// Row-major strides of `shape`, in elements.
pub fn strides(shape: &[usize]) -> Vec<usize> {
    let mut strides = vec![1; shape.len()];
    for i in (0..shape.len().saturating_sub(1)).rev() {
        strides[i] = strides[i + 1] * shape[i + 1];
    }
    strides
}

/// IEEE half to single precision.
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exponent = u32::from((h >> 10) & 0x1f);
    let mantissa = u32::from(h & 0x3ff);
    let bits = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalize.
            let shift = m.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | ((m << shift) & 0x3ff) << 13
        }
        (0x1f, m) => sign | 0x7f80_0000 | (m << 13),
        (e, m) => sign | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// IEEE single to half precision, rounding to nearest even (as GPUs
/// convert); too large becomes infinite.
pub(crate) fn f32_to_f16(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;
    if exponent == 0xff {
        // Infinite, or NaN (kept a NaN).
        return sign | 0x7c00 | if mantissa != 0 { 0x200 } else { 0 };
    }
    // Rounds `value >> shift` to nearest, ties to even.
    let round = |value: u32, shift: u32| {
        let kept = value >> shift;
        let rest = value & ((1 << shift) - 1);
        let half = 1 << (shift - 1);
        kept + u32::from(rest > half || (rest == half && kept & 1 == 1))
    };
    let e = exponent - 112;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        // Subnormal (or zero): units of 2^-24.
        if e < -10 {
            return sign;
        }
        return sign | round(mantissa | 0x80_0000, (14 - e) as u32) as u16;
    }
    // A carry out of the mantissa raises the exponent, up to infinity.
    sign | round(((e as u32) << 23) | mantissa, 13) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_half_floats() {
        for h in [
            0x3c00u16, 0xc000, 0x3555, 0x0001, 0x03ff, 0x0400, 0x7bff, 0x0000, 0x8000,
        ] {
            assert_eq!(f32_to_f16(f16_to_f32(h)), h, "{h:#06x}");
        }
        assert_eq!(f32_to_f16(0.1), 0x2e66);
        // Halfway between 1 and the next half rounds to even (1).
        assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        assert_eq!(f32_to_f16(65520.0), 0x7c00);
        assert_eq!(f32_to_f16(-1e9), 0xfc00);
        assert_eq!(f32_to_f16(1e-9), 0);
        assert_eq!(f32_to_f16(f32::NAN) & 0x7c00, 0x7c00);
    }

    #[test]
    fn half_floats() {
        for (h, f) in [
            (0x3c00, 1.0),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),
            (0x0001, 5.960_464_5e-8),
            (0x0003, 1.788_139_3e-7),
            (0x03ff, 6.097_555e-5),
            (0x7bff, 65504.0),
            (0x0000, 0.0),
        ] {
            assert_eq!(f16_to_f32(h), f, "{h:#06x}");
        }
        assert!(f16_to_f32(0x7c00).is_infinite());
    }

    #[test]
    fn row_major_strides() {
        assert_eq!(strides(&[2, 3, 4]), vec![12, 4, 1]);
        assert_eq!(strides(&[]), Vec::<usize>::new());
    }
}
