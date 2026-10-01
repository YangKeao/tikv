// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. (relocated native vector policies).

//! Native aligned VECTOR policy, sharing numerical leaves with packed vectors.
//! Raw mutation and decoding deliberately preserve arbitrary float32 bits.

use std::{cmp::Ordering, fmt};

use serde_json::value::RawValue;

use super::vector::{VectorFloat32, VectorFloat32Ref};

/// Maximum dimension accepted by native vector text parsing.
pub const NATIVE_MAX_VECTOR_DIMENSION: usize = 16_383;

/// Native vector error, retaining the original diagnostic payload.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeVectorError(String);

impl NativeVectorError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Debug for NativeVectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("VectorError").field(&self.0).finish()
    }
}

impl fmt::Display for NativeVectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NativeVectorError {}

/// Native aligned vector; validated construction and raw mutation are distinct.
#[derive(Clone, Default, PartialEq)]
pub struct NativeVectorFloat32 {
    elements: Vec<f32>,
}

impl fmt::Debug for NativeVectorFloat32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("VectorFloat32")
            .field(&self.elements)
            .finish()
    }
}

impl fmt::Display for NativeVectorFloat32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.raw_ref().fmt_native(formatter)
    }
}

impl NativeVectorFloat32 {
    /// Creates a vector after rejecting NaN and infinity, without a dimension
    /// cap.
    pub fn create(elements: impl Into<Vec<f32>>) -> Result<Self, NativeVectorError> {
        let elements = elements.into();
        VectorFloat32Ref::from_raw_f32(&elements).validate_native_elements()?;
        Ok(Self { elements })
    }

    /// Creates a vector and panics on an invalid value.
    pub fn must_create(elements: impl Into<Vec<f32>>) -> Self {
        Self::create(elements).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Creates a zero-filled vector of the given dimension.
    pub fn init(dimensions: usize) -> Self {
        Self {
            elements: vec![0.0; dimensions],
        }
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
    pub fn elements(&self) -> &[f32] {
        &self.elements
    }
    /// Like the source, values written here are not revalidated.
    pub fn elements_mut(&mut self) -> &mut [f32] {
        &mut self.elements
    }

    fn raw_ref(&self) -> VectorFloat32Ref<'_> {
        VectorFloat32Ref::from_raw_f32(&self.elements)
    }

    /// Transfers the actual packed native-endian float bits, without
    /// validation.
    pub fn into_wire_raw(self) -> VectorFloat32 {
        VectorFloat32 {
            value: bytemuck::cast_slice(&self.elements).to_vec(),
        }
    }

    /// Actual retained element capacity, for checked worker storage accounting.
    pub fn elements_capacity(&self) -> usize {
        self.elements.capacity()
    }

    /// Strict UTF-8 input followed by the unchanged native vector text parser.
    pub fn parse_bytes(input: &[u8]) -> Result<Self, NativeVectorError> {
        let text = std::str::from_utf8(input)
            .map_err(|error| NativeVectorError::new(error.to_string()))?;
        Self::parse(text)
    }

    /// Checks the dimension declared by a column. `None` is unspecified.
    pub fn check_dims_fit_column(&self, expected: Option<usize>) -> Result<(), NativeVectorError> {
        if let Some(expected) = expected {
            if self.len() != expected {
                return Err(NativeVectorError::new(format!(
                    "vector has {} dimensions, does not fit VECTOR({expected})",
                    self.len()
                )));
            }
        }
        Ok(())
    }

    /// Returns the source log/EXPLAIN representation.
    pub fn truncated_string(&self) -> String {
        const DISPLAY: usize = 5;
        let displayed = self.elements.len().min(DISPLAY);
        let mut output = String::from("[");
        for (index, value) in self.elements[..displayed].iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(&format_f32_general_two_digits(*value));
        }
        if self.elements.len() > DISPLAY {
            output.push_str(&format!(",({} more)...", self.elements.len() - DISPLAY));
        }
        output.push(']');
        output
    }

    /// Appends the exact little-endian source wire representation.
    pub fn serialize_to(&self, destination: &mut Vec<u8>) {
        destination.extend_from_slice(&(self.len() as u32).to_le_bytes());
        for value in &self.elements {
            destination.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.serialized_size());
        self.serialize_to(&mut result);
        result
    }

    pub const fn serialized_size(&self) -> usize {
        4 + self.elements.len() * 4
    }
    pub const fn estimated_mem_usage(&self) -> usize {
        std::mem::size_of::<Self>() + self.serialized_size()
    }

    /// Parses the JSON-array text accepted by the native source.
    pub fn parse(text: &str) -> Result<Self, NativeVectorError> {
        if text.trim() == "null" {
            return Err(NativeVectorError::new(format!(
                "Invalid vector text: {text}"
            )));
        }
        let values: Vec<&RawValue> = serde_json::from_str(text)
            .map_err(|_| NativeVectorError::new(format!("Invalid vector text: {text}")))?;
        let mut elements = Vec::with_capacity(values.len());
        for value in values {
            let value = value
                .get()
                .parse::<f64>()
                .map_err(|_| NativeVectorError::new(format!("Invalid vector text: {text}")))?;
            if value.is_nan() {
                return Err(NativeVectorError::new("NaN not allowed in vector"));
            }
            if value.is_infinite() {
                return Err(NativeVectorError::new(format!(
                    "Invalid vector text: {text}"
                )));
            }
            if !(-f64::from(f32::MAX)..=f64::from(f32::MAX)).contains(&value) {
                return Err(NativeVectorError::new(format!(
                    "value {} out of range for float32",
                    format_go_exponent(value)
                )));
            }
            elements.push(value as f32);
        }
        check_native_vector_dim_valid(elements.len() as isize)?;
        Ok(Self { elements })
    }

    pub fn is_zero_value(&self) -> bool {
        self.is_empty()
    }
    pub fn compare(&self, other: &Self) -> Ordering {
        self.raw_ref().cmp(&other.raw_ref())
    }

    pub fn l2_squared_distance(&self, other: &Self) -> Result<f64, NativeVectorError> {
        self.raw_ref().native_l2_squared_distance(other.raw_ref())
    }
    pub fn l2_distance(&self, other: &Self) -> Result<f64, NativeVectorError> {
        self.raw_ref().native_l2_distance(other.raw_ref())
    }
    pub fn inner_product(&self, other: &Self) -> Result<f64, NativeVectorError> {
        self.raw_ref().native_inner_product(other.raw_ref())
    }
    pub fn negative_inner_product(&self, other: &Self) -> Result<f64, NativeVectorError> {
        Ok(-self.inner_product(other)?)
    }
    pub fn cosine_distance(&self, other: &Self) -> Result<f64, NativeVectorError> {
        self.raw_ref().native_cosine_distance(other.raw_ref())
    }
    pub fn l1_distance(&self, other: &Self) -> Result<f64, NativeVectorError> {
        self.raw_ref().native_l1_distance(other.raw_ref())
    }
    pub fn l2_norm(&self) -> f64 {
        self.raw_ref().native_l2_norm()
    }

    pub fn add(&self, other: &Self) -> Result<Self, NativeVectorError> {
        self.elementwise(other, |left, right| left + right)
    }
    pub fn sub(&self, other: &Self) -> Result<Self, NativeVectorError> {
        self.elementwise(other, |left, right| left - right)
    }
    pub fn mul(&self, other: &Self) -> Result<Self, NativeVectorError> {
        self.elementwise(other, |left, right| left * right)
    }
    fn elementwise(
        &self,
        other: &Self,
        operation: impl Fn(f32, f32) -> f32,
    ) -> Result<Self, NativeVectorError> {
        self.raw_ref().check_native_dims(other.raw_ref())?;
        let elements: Vec<_> = self
            .elements
            .iter()
            .copied()
            .zip(other.elements.iter().copied())
            .map(|(left, right)| operation(left, right))
            .collect();
        for value in &elements {
            if value.is_infinite() {
                return Err(NativeVectorError::new("value out of range: overflow"));
            }
            if value.is_nan() {
                return Err(NativeVectorError::new("value out of range: NaN"));
            }
        }
        Ok(Self { elements })
    }
}

/// Checks native dimension bounds.
pub fn check_native_vector_dim_valid(dimensions: isize) -> Result<(), NativeVectorError> {
    if dimensions < 0 {
        return Err(NativeVectorError::new(
            "dimensions for type vector must be at least 0",
        ));
    }
    if dimensions as usize > NATIVE_MAX_VECTOR_DIMENSION {
        return Err(NativeVectorError::new(format!(
            "vector cannot have more than {NATIVE_MAX_VECTOR_DIMENSION} dimensions"
        )));
    }
    Ok(())
}

fn format_go_exponent(value: f64) -> String {
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i32 = exponent.parse().expect("Rust exponent is numeric");
    format!("{mantissa}e{exponent:+}")
}

pub(crate) fn format_f32_fixed_shortest(value: f32) -> String {
    let shortest = value.to_string();
    let Some((mantissa, exponent)) = shortest
        .split_once('e')
        .or_else(|| shortest.split_once('E'))
    else {
        return shortest;
    };
    let exponent: i32 = exponent.parse().expect("Rust exponent is numeric");
    let unsigned = mantissa.trim_start_matches('-');
    let negative = mantissa.starts_with('-');
    let digits: String = unsigned
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let decimal = unsigned.find('.').map_or(1_i32, |index| index as i32);
    let point = decimal + exponent;
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if point <= 0 {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', (-point) as usize));
        output.push_str(&digits);
    } else if point as usize >= digits.len() {
        output.push_str(&digits);
        output.extend(std::iter::repeat_n('0', point as usize - digits.len()));
    } else {
        output.push_str(&digits[..point as usize]);
        output.push('.');
        output.push_str(&digits[point as usize..]);
    }
    output
}

fn format_f32_general_two_digits(value: f32) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0".to_owned()
        } else {
            "0".to_owned()
        };
    }
    let exponent = value.abs().log10().floor() as i32;
    if !(-4..2).contains(&exponent) {
        let scientific = format!("{value:.1e}");
        let (mantissa, exponent) = scientific
            .split_once('e')
            .expect("scientific format has exponent");
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let exponent: i32 = exponent.parse().expect("Rust exponent is numeric");
        format!("{mantissa}e{exponent:+03}")
    } else {
        let decimals = (1 - exponent).max(0) as usize;
        let fixed = format!("{value:.decimals$}");
        fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// Returns the number of bytes occupied by the first native serialized vector.
pub fn peek_native_vector_float32(bytes: &[u8]) -> Result<usize, NativeVectorError> {
    let header = bytes.get(..4).ok_or_else(|| {
        NativeVectorError::new(format!(
            "bad VectorFloat32 value header (len={})",
            bytes.len()
        ))
    })?;
    let dimensions = u32::from_le_bytes(header.try_into().expect("fixed vector header"));
    let expected = dimensions
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| NativeVectorError::new("bad VectorFloat32 value size overflow"))?;
    if bytes.len() < expected as usize {
        return Err(NativeVectorError::new(format!(
            "bad VectorFloat32 value (len={}, expected={expected})",
            bytes.len()
        )));
    }
    Ok(expected as usize)
}

/// Decodes raw little-endian elements without finite validation, preserving
/// suffix.
pub fn deserialize_native_vector_float32(
    bytes: &[u8],
) -> Result<(NativeVectorFloat32, &[u8]), NativeVectorError> {
    let length = peek_native_vector_float32(bytes)?;
    let mut elements = Vec::with_capacity((length - 4) / 4);
    for chunk in bytes[4..length].chunks_exact(4) {
        elements.push(f32::from_bits(u32::from_le_bytes(
            chunk.try_into().expect("fixed vector element"),
        )));
    }
    Ok((NativeVectorFloat32 { elements }, &bytes[length..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_vector_original_codec_and_parser_vectors() {
        // Original native vector fixtures, not newly recorded provider output.
        let mut vector = NativeVectorFloat32::init(2);
        vector.elements_mut().copy_from_slice(&[1.1, 2.2]);
        assert_eq!(
            vector.serialize(),
            [2, 0, 0, 0, 0xcd, 0xcc, 0x8c, 0x3f, 0xcd, 0xcc, 0x0c, 0x40]
        );
        assert_eq!(
            NativeVectorFloat32::parse("[-1e39, 1e39]")
                .unwrap_err()
                .to_string(),
            "value -1e+39 out of range for float32"
        );
        assert!(peek_native_vector_float32(&[0, 0, 0, 0x40]).is_err());
        let parsed = NativeVectorFloat32::parse_bytes(b"[1.1, 2.2, 3.3]").unwrap();
        assert_eq!(parsed.to_string(), "[1.1,2.2,3.3]");
        assert_eq!(parsed.raw_ref().to_native_string(), "[1.1,2.2,3.3]");
        assert!(NativeVectorFloat32::parse_bytes(b"\xff").is_err());
        assert!(parsed.elements_capacity() >= parsed.len());
    }

    #[test]
    fn native_vector_raw_bits_are_not_validated_or_canonicalized() {
        // Hand-derived policy boundary: mutation/codec accept raw bits while
        // validated construction rejects nonfinite values, and +/-0 compare equal.
        let bits = [0x8000_0000, 0x7fc0_0042, 0x7f80_0000];
        let mut raw = NativeVectorFloat32::init(3);
        for (value, bits) in raw.elements_mut().iter_mut().zip(bits) {
            *value = f32::from_bits(bits);
        }
        assert!(NativeVectorFloat32::create(raw.elements().to_vec()).is_err());
        let wire = raw.clone().into_wire_raw();
        for (chunk, bits) in wire.value.chunks_exact(4).zip(bits) {
            assert_eq!(chunk, bits.to_ne_bytes());
        }
        let mut encoded = raw.serialize();
        encoded.push(99);
        let (decoded, suffix) = deserialize_native_vector_float32(&encoded).unwrap();
        assert_eq!(suffix, &[99]);
        for (value, bits) in decoded.elements().iter().zip(bits) {
            assert_eq!(value.to_bits(), bits);
        }
        let negative = NativeVectorFloat32::must_create(vec![-0.0]);
        let positive = NativeVectorFloat32::must_create(vec![0.0]);
        assert_eq!(negative, positive);
        assert_ne!(negative.into_wire_raw(), positive.into_wire_raw());
        assert!(NativeVectorFloat32::create(vec![0.0; NATIVE_MAX_VECTOR_DIMENSION + 1]).is_ok());
        assert!(check_native_vector_dim_valid((NATIVE_MAX_VECTOR_DIMENSION + 1) as isize).is_err());
    }

    #[test]
    fn native_vector_shared_metrics_keep_source_precision() {
        // Original native arithmetic vectors.
        let left = NativeVectorFloat32::must_create(vec![1.0, 2.0, 3.0]);
        let right = NativeVectorFloat32::must_create(vec![4.0, 5.0, 6.0]);
        assert_eq!(left.l2_squared_distance(&right).unwrap(), 27.0);
        assert_eq!(left.inner_product(&right).unwrap(), 32.0);
        assert_eq!(left.negative_inner_product(&right).unwrap(), -32.0);
        assert_eq!(left.l1_distance(&right).unwrap(), 9.0);
        assert_eq!(left.add(&right).unwrap().elements(), &[5.0, 7.0, 9.0]);
        assert!(
            NativeVectorFloat32::default()
                .cosine_distance(&NativeVectorFloat32::default())
                .unwrap()
                .is_nan()
        );
        assert_eq!(
            left.inner_product(&NativeVectorFloat32::default())
                .unwrap_err()
                .to_string(),
            "vectors have different dimensions: 3 and 0"
        );
        // Hand-derived f32 accumulation sentinel: the middle +1 rounds away.
        let rounding = NativeVectorFloat32::must_create(vec![16777216.0, 1.0, -16777216.0]);
        let ones = NativeVectorFloat32::must_create(vec![1.0; 3]);
        assert_eq!(rounding.inner_product(&ones).unwrap(), 0.0);
        assert_eq!(
            rounding.raw_ref().inner_product(ones.raw_ref()).unwrap(),
            0.0
        );
    }
}
