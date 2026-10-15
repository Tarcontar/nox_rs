//! Tensors on the CPU: model weights, constants, and the shape arithmetic
//! that is worked out while planning. Floats for data, 64-bit integers
//! for shapes and indices (and every other integer type ONNX has).

use crate::onnx::{TensorProto, data_type};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    F32(Vec<f32>),
    I64(Vec<i64>),
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
            data: Data::F32(data),
        }
    }

    pub fn i64(shape: Vec<usize>, data: Vec<i64>) -> Self {
        debug_assert_eq!(shape.iter().product::<usize>(), data.len());
        Self {
            shape,
            data: Data::I64(data),
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
            Data::F32(data) => Ok(data),
            Data::I64(_) => Err(Error::Model("expected a float tensor".into())),
        }
    }

    /// The values as floats, whatever the type.
    pub fn to_f32(&self) -> Vec<f32> {
        match &self.data {
            Data::F32(data) => data.clone(),
            Data::I64(data) => data.iter().map(|&v| v as f32).collect(),
        }
    }

    /// The values as integers, whatever the type (floats truncate).
    pub fn to_i64(&self) -> Vec<i64> {
        match &self.data {
            Data::F32(data) => data.iter().map(|&v| v as i64).collect(),
            Data::I64(data) => data.clone(),
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
        let data = match proto.data_type {
            data_type::FLOAT if !raw.is_empty() => Data::F32(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect(),
            ),
            data_type::FLOAT => Data::F32(proto.float_data.clone()),
            data_type::DOUBLE if !raw.is_empty() => Data::F32(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| f64::from_le_bytes(*b) as f32)
                    .collect(),
            ),
            data_type::DOUBLE => Data::F32(proto.double_data.iter().map(|&v| v as f32).collect()),
            data_type::FLOAT16 if !raw.is_empty() => Data::F32(
                raw.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f16_to_f32(u16::from_le_bytes(*b)))
                    .collect(),
            ),
            data_type::FLOAT16 => Data::F32(
                proto
                    .int32_data
                    .iter()
                    .map(|&v| f16_to_f32(v as u16))
                    .collect(),
            ),
            data_type::INT64 if !raw.is_empty() => Data::I64(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| i64::from_le_bytes(*b))
                    .collect(),
            ),
            data_type::INT64 => Data::I64(proto.int64_data.clone()),
            data_type::INT32 if !raw.is_empty() => Data::I64(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| i64::from(i32::from_le_bytes(*b)))
                    .collect(),
            ),
            data_type::INT32 | data_type::INT8 | data_type::BOOL if raw.is_empty() => {
                Data::I64(proto.int32_data.iter().map(|&v| i64::from(v)).collect())
            }
            data_type::INT8 => Data::I64(raw.iter().map(|&b| i64::from(b as i8)).collect()),
            data_type::UINT8 | data_type::BOOL => {
                Data::I64(raw.iter().map(|&b| i64::from(b)).collect())
            }
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
fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exponent = u32::from((h >> 10) & 0x1f);
    let mantissa = u32::from(h & 0x3ff);
    let bits = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalize.
            let shift = m.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | ((m << (shift + 1)) & 0x3ff) << 13
        }
        (0x1f, m) => sign | 0x7f80_0000 | (m << 13),
        (e, m) => sign | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        for (h, f) in [
            (0x3c00, 1.0),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),
            (0x0001, 5.960_464_5e-8),
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
