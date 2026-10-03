// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::BTreeSet,
    convert::{TryFrom, TryInto},
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use rand::Rng;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::codec::{data_type::*, mysql::RoundMode};
use uuid::Uuid;

/// The three grouping-id comparison algorithms used by TiDB's tipb payload.
/// The discriminants match `tipb.GroupingMode` without making mode an operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GroupingMode {
    /// A grouping mark is present when `grouping_id & mark == 0`.
    BitAnd = 1,
    /// A grouping mark is present when `grouping_id <= mark`.
    NumericCmp = 2,
    /// A grouping mark is present when `grouping_id` is absent from the set.
    NumericSet = 3,
}

impl TryFrom<u8> for GroupingMode {
    type Error = GroupingMetadataError;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::BitAnd),
            2 => Ok(Self::NumericCmp),
            3 => Ok(Self::NumericSet),
            other => Err(GroupingMetadataError::InvalidMode(other)),
        }
    }
}

/// Errors raised while constructing or using grouping metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupingMetadataError {
    /// `GROUPING` was evaluated before planner metadata was installed.
    Uninitialized,
    /// A wire mode did not map to one of TiDB's supported algorithms.
    InvalidMode(u8),
    /// Bit-and and numeric-compare require exactly one mark per argument.
    InvalidGroupingMarkCount {
        /// The mode whose mark cardinality was invalid.
        mode: GroupingMode,
        /// Zero-based argument position of the invalid mark.
        index: usize,
        /// Number of grouping ids supplied for that argument.
        count: usize,
    },
}

/// Validated metadata attached to one scalar grouping function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupingMetadata {
    mode: GroupingMode,
    grouping_marks: Vec<BTreeSet<u64>>,
}

impl GroupingMetadata {
    /// Validates and stores the source `GroupingMode` and grouping marks.
    pub fn new(
        mode: GroupingMode,
        grouping_marks: Vec<BTreeSet<u64>>,
    ) -> std::result::Result<Self, GroupingMetadataError> {
        if matches!(mode, GroupingMode::BitAnd | GroupingMode::NumericCmp) {
            for (index, mark) in grouping_marks.iter().enumerate() {
                if mark.len() != 1 {
                    return Err(GroupingMetadataError::InvalidGroupingMarkCount {
                        mode,
                        index,
                        count: mark.len(),
                    });
                }
            }
        }
        Ok(Self {
            mode,
            grouping_marks,
        })
    }

    /// Returns the selected source algorithm.
    pub fn mode(&self) -> GroupingMode {
        self.mode
    }

    /// Returns the validated mark sets in argument order.
    pub fn grouping_marks(&self) -> &[BTreeSet<u64>] {
        &self.grouping_marks
    }

    /// Evaluates one grouping id using this validated metadata.
    pub fn eval(&self, grouping_id: u64) -> u64 {
        let mut result = 0u64;
        match self.mode {
            GroupingMode::BitAnd => {
                for mark in &self.grouping_marks {
                    result <<= 1;
                    let key = *mark
                        .iter()
                        .next()
                        .expect("validated bit-and mark has one element");
                    if grouping_id & key == 0 {
                        result += 1;
                    }
                }
            }
            GroupingMode::NumericCmp => {
                for mark in &self.grouping_marks {
                    result <<= 1;
                    let key = *mark
                        .iter()
                        .next()
                        .expect("validated numeric-compare mark has one element");
                    if grouping_id <= key {
                        result += 1;
                    }
                }
            }
            GroupingMode::NumericSet => {
                for mark in &self.grouping_marks {
                    result <<= 1;
                    if !mark.contains(&grouping_id) {
                        result += 1;
                    }
                }
            }
        }
        result
    }
}

/// Pure scalar implementation of TiDB's rewritten `GROUPING` function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupingFunction {
    metadata: Option<GroupingMetadata>,
}

impl GroupingFunction {
    /// Constructs a function before planner metadata has been installed.
    pub fn uninitialized() -> Self {
        Self::default()
    }

    /// Constructs a function with validated planner metadata.
    pub fn with_metadata(
        mode: GroupingMode,
        grouping_marks: Vec<BTreeSet<u64>>,
    ) -> std::result::Result<Self, GroupingMetadataError> {
        let mut function = Self::uninitialized();
        function.set_metadata(mode, grouping_marks)?;
        Ok(function)
    }

    /// A failed replacement leaves the function uninitialized, matching Go.
    pub fn set_metadata(
        &mut self,
        mode: GroupingMode,
        grouping_marks: Vec<BTreeSet<u64>>,
    ) -> std::result::Result<(), GroupingMetadataError> {
        self.metadata = None;
        let metadata = GroupingMetadata::new(mode, grouping_marks)?;
        self.metadata = Some(metadata);
        Ok(())
    }

    /// Returns validated metadata, or the source uninitialized error.
    pub fn metadata(&self) -> std::result::Result<&GroupingMetadata, GroupingMetadataError> {
        self.metadata
            .as_ref()
            .ok_or(GroupingMetadataError::Uninitialized)
    }

    /// Returns the selected mode, if metadata has been installed.
    pub fn mode(&self) -> std::result::Result<GroupingMode, GroupingMetadataError> {
        Ok(self.metadata()?.mode())
    }

    /// Returns the same unsigned bits as the source's unsigned-flagged int64.
    pub fn eval(&self, grouping_id: u64) -> std::result::Result<u64, GroupingMetadataError> {
        Ok(self.metadata()?.eval(grouping_id))
    }
}

fn grouping_packet_word(input: &mut &[u8]) -> Result<u64> {
    let word = input
        .get(..8)
        .ok_or_else(|| other_err!("Truncated GROUPING marks envelope"))?;
    let word = u64::from_le_bytes(word.try_into().expect("eight-byte GROUPING word"));
    *input = &input[8..];
    Ok(word)
}

// One allocation-free structural validator is used by admission and decoding.
// Counts bound slices before iteration or allocation; sets must be canonical.
fn walk_grouping_marks(
    mut input: &[u8],
    mode: GroupingMode,
    mut visit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let groups = usize::try_from(grouping_packet_word(&mut input)?)
        .map_err(|_| other_err!("GROUPING group count exceeds indexing width"))?;
    if groups > input.len() / 8 {
        return Err(other_err!("GROUPING group count exceeds its envelope"));
    }
    for _ in 0..groups {
        let count = usize::try_from(grouping_packet_word(&mut input)?)
            .map_err(|_| other_err!("GROUPING mark count exceeds indexing width"))?;
        if matches!(mode, GroupingMode::BitAnd | GroupingMode::NumericCmp) && count != 1 {
            return Err(other_err!(
                "GROUPING single-mark recipe requires exactly one mark"
            ));
        }
        let bytes = count
            .checked_mul(8)
            .ok_or_else(|| other_err!("GROUPING mark byte count overflow"))?;
        let marks = input
            .get(..bytes)
            .ok_or_else(|| other_err!("GROUPING mark count exceeds its envelope"))?;
        input = &input[bytes..];
        let mut previous = None;
        for word in marks.chunks_exact(8) {
            let mark = u64::from_le_bytes(word.try_into().expect("eight-byte GROUPING mark"));
            if previous.is_some_and(|previous| previous >= mark) {
                return Err(other_err!("GROUPING marks must be strictly ascending"));
            }
            previous = Some(mark);
        }
        visit(marks)?;
    }
    if !input.is_empty() {
        return Err(other_err!("Trailing bytes in GROUPING marks envelope"));
    }
    Ok(())
}

/// Validates the actual gid and marks carrier for one fixed GROUPING recipe.
/// No mode byte or precomputed result belongs in either envelope.
pub fn grouping_native_args_valid(gid: &[u8], marks: &[u8], mode: GroupingMode) -> bool {
    gid.len() == 8 && walk_grouping_marks(marks, mode, |_| Ok(())).is_ok()
}

fn grouping_native(gid: BytesRef, marks: BytesRef, mode: GroupingMode) -> Result<Option<Int>> {
    let gid = u64::from_le_bytes(
        gid.try_into()
            .map_err(|_| other_err!("GROUPING gid transport requires exactly eight bytes"))?,
    );
    let mut groups = Vec::new();
    walk_grouping_marks(marks, mode, |marks| {
        groups
            .try_reserve(1)
            .map_err(|_| other_err!("Unable to allocate GROUPING mark groups"))?;
        groups.push(
            marks
                .chunks_exact(8)
                .map(|word| u64::from_le_bytes(word.try_into().expect("eight-byte GROUPING mark")))
                .collect(),
        );
        Ok(())
    })?;
    let metadata = GroupingMetadata::new(mode, groups)
        .map_err(|error| other_err!("Invalid GROUPING metadata: {:?}", error))?;
    Ok(Some(metadata.eval(gid) as Int))
}

#[rpn_fn]
fn grouping_bit_and_native(gid: BytesRef, marks: BytesRef) -> Result<Option<Int>> {
    grouping_native(gid, marks, GroupingMode::BitAnd)
}

#[rpn_fn]
fn grouping_numeric_cmp_native(gid: BytesRef, marks: BytesRef) -> Result<Option<Int>> {
    grouping_native(gid, marks, GroupingMode::NumericCmp)
}

#[rpn_fn]
fn grouping_numeric_set_native(gid: BytesRef, marks: BytesRef) -> Result<Option<Int>> {
    grouping_native(gid, marks, GroupingMode::NumericSet)
}

#[rpn_fn(nullable)]
fn grouping_null_native(witness: Option<&Int>) -> Result<Option<Int>> {
    match witness {
        None => Ok(None),
        Some(_) => Err(other_err!("GROUPING NULL witness must be an actual NULL")),
    }
}

/// UUID epoch offset used by the native UUID generator and timestamp kernels.
pub const NATIVE_UUID_EPOCH_100NS: i64 = 122_192_928_000_000_000;

/// Canonical lowercase UUID spelling, shared with native host generation.
pub fn format_uuid_native(bytes: &[u8; 16]) -> String {
    Uuid::from_bytes(*bytes).hyphenated().to_string()
}

// Native google/uuid.Parse accepts arbitrary enclosing bytes in its 38-byte
// form and case-insensitive URN prefixes. Wire uuidcrate parsing stays
// separate.
fn parse_uuid_native(value: &[u8]) -> Option<[u8; 16]> {
    let canonical = match value.len() {
        36 => value,
        45 if value[..9].eq_ignore_ascii_case(b"urn:uuid:") => &value[9..],
        38 => &value[1..37],
        32 => value,
        _ => return None,
    };
    // Only 32/36-byte interiors reach the existing wire decoder. Invalid
    // UTF-8 cannot be hexadecimal; the ignored wrapper bytes stay unchecked.
    let text = std::str::from_utf8(canonical).ok()?;
    Uuid::parse_str(text).ok().map(|uuid| *uuid.as_bytes())
}

// Both callers retain their original signed/unsigned microsecond arithmetic;
// this exact decimal scaling leaf has no floating-point conversion.
fn uuid_decimal_micros(micros: Decimal) -> Decimal {
    let shifted = micros.shift(-6);
    let rounded = (*shifted).clone().round(6, RoundMode::Truncate);
    (*rounded).clone()
}

#[rpn_fn(nullable)]
fn is_uuid_native(input: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(input.map(|input| {
        let trim_view = String::from_utf8_lossy(input);
        if trim_view.trim() != trim_view.as_ref() {
            0
        } else {
            i64::from(parse_uuid_native(input).is_some())
        }
    }))
}

#[rpn_fn(nullable)]
fn uuid_version_native(input: Option<BytesRef>) -> Result<Option<Int>> {
    let Some(input) = input else {
        return Ok(None);
    };
    let uuid = parse_uuid_native(input).ok_or(EvaluateError::UuidVersionInvalid)?;
    Ok(Some(i64::from(uuid[6] >> 4)))
}

#[rpn_fn(nullable)]
fn uuid_timestamp_native(input: Option<BytesRef>) -> Result<Option<Decimal>> {
    let Some(input) = input else {
        return Ok(None);
    };
    let uuid = parse_uuid_native(input).ok_or(EvaluateError::UuidTimestampInvalid)?;
    let timestamp_100ns = match uuid[6] >> 4 {
        1 => {
            i64::from(u32::from_be_bytes([uuid[0], uuid[1], uuid[2], uuid[3]]))
                | (i64::from(u16::from_be_bytes([uuid[4], uuid[5]])) << 32)
                | (i64::from(u16::from_be_bytes([uuid[6], uuid[7]]) & 0x0fff) << 48)
        }
        6 => {
            (i64::from(u32::from_be_bytes([uuid[0], uuid[1], uuid[2], uuid[3]])) << 28)
                | (i64::from(u16::from_be_bytes([uuid[4], uuid[5]])) << 12)
                | i64::from(u16::from_be_bytes([uuid[6], uuid[7]]) & 0x0fff)
        }
        7 => {
            let first_eight = u64::from_be_bytes([
                uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5], uuid[6], uuid[7],
            ]);
            ((first_eight >> 16) * 10_000) as i64 + NATIVE_UUID_EPOCH_100NS
        }
        _ => return Ok(None),
    };
    let unix_micros = (timestamp_100ns - NATIVE_UUID_EPOCH_100NS) / 10;
    Ok(Some(uuid_decimal_micros(Decimal::from(unix_micros))))
}

#[rpn_fn(nullable)]
fn uuid_to_bin_parse_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(input) = input else {
        return Ok(None);
    };
    if std::str::from_utf8(input)
        .map(|text| text.trim() != text)
        .unwrap_or(false)
    {
        return Err(EvaluateError::UuidToBinWhitespace.into());
    }
    let uuid = parse_uuid_native(input).ok_or(EvaluateError::UuidToBinInvalid)?;
    Ok(Some(uuid.to_vec()))
}

#[rpn_fn(nullable)]
fn uuid_to_bin_swap_native(input: Option<BytesRef>, flag: Option<&Int>) -> Result<Option<Bytes>> {
    let (Some(input), Some(flag)) = (input, flag) else {
        return Err(other_err!("invalid UUID_TO_BIN computed operand shape"));
    };
    let uuid: &[u8; 16] = input
        .try_into()
        .map_err(|_| other_err!("invalid UUID_TO_BIN computed operand width"))?;
    let output = if *flag != 0 {
        [
            uuid[6], uuid[7], uuid[4], uuid[5], uuid[0], uuid[1], uuid[2], uuid[3], uuid[8],
            uuid[9], uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
        ]
    } else {
        *uuid
    };
    Ok(Some(output.to_vec()))
}

#[rpn_fn(nullable)]
fn bin_to_uuid_native(input: Option<BytesRef>, flag: Option<&Int>) -> Result<Option<Bytes>> {
    let flag = flag.ok_or_else(|| other_err!("missing BIN_TO_UUID prepared flag"))?;
    let Some(input) = input else {
        return Ok(None);
    };
    let uuid: &[u8; 16] = input
        .try_into()
        .map_err(|_| EvaluateError::BinToUuidInvalidLength {
            input: input.to_vec(),
        })?;
    let output = if *flag != 0 {
        // Inverse field permutation, not UUID_TO_BIN's forward byte swap.
        let restored = [
            uuid[4], uuid[5], uuid[6], uuid[7], uuid[2], uuid[3], uuid[0], uuid[1], uuid[8],
            uuid[9], uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
        ];
        format_uuid_native(&restored)
    } else {
        format_uuid_native(uuid)
    };
    Ok(Some(output.into_bytes()))
}

pub(crate) fn get_native_is_uuid_fn_meta() -> crate::RpnFnMeta {
    is_uuid_native_fn_meta()
}
pub(crate) fn get_native_uuid_version_fn_meta() -> crate::RpnFnMeta {
    uuid_version_native_fn_meta()
}
pub(crate) fn get_native_uuid_timestamp_fn_meta() -> crate::RpnFnMeta {
    uuid_timestamp_native_fn_meta()
}
pub(crate) fn get_native_uuid_to_bin_parse_fn_meta() -> crate::RpnFnMeta {
    uuid_to_bin_parse_native_fn_meta()
}
pub(crate) fn get_native_uuid_to_bin_swap_fn_meta() -> crate::RpnFnMeta {
    uuid_to_bin_swap_native_fn_meta()
}
pub(crate) fn get_native_bin_to_uuid_fn_meta() -> crate::RpnFnMeta {
    bin_to_uuid_native_fn_meta()
}

#[rpn_fn(nullable)]
fn get_native_tidb_shard(input: Option<&Int>) -> Result<Option<Int>> {
    Ok(input.map(|key| (tidb_query_crypto::hash_uint64(*key as u64) % 256) as i64))
}

#[rpn_fn(nullable)]
fn get_native_vitess_hash(input: Option<&Int>) -> Result<Option<Int>> {
    Ok(input.map(|key| tidb_query_crypto::hash_uint64(*key as u64) as i64))
}

// Factory-only IEEE754 binary64 transport, not SQL bytes or a finite-only Real.
fn decode_format_real_native(input: Option<BytesRef>) -> Result<Option<f64>> {
    let bytes = match input {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let bits = <[u8; 8]>::try_from(bytes).map_err(|_| {
        other_err!(
            "Internal raw f64 transport requires exactly 8 bytes, received {}",
            bytes.len()
        )
    })?;
    Ok(Some(f64::from_bits(u64::from_le_bytes(bits))))
}

#[rpn_fn(nullable)]
fn get_native_format_bytes(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_format_real_native(input)?.map(|value| {
        format_scaled(
            value,
            &[
                (1_u64 << 60, "EiB"),
                (1_u64 << 50, "PiB"),
                (1_u64 << 40, "TiB"),
                (1_u64 << 30, "GiB"),
                (1_u64 << 20, "MiB"),
                (1_u64 << 10, "KiB"),
            ],
            "bytes",
        )
        .into_bytes()
    }))
}

#[rpn_fn(nullable)]
fn get_native_format_nano_time(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_format_real_native(input)?.map(|value| {
        format_scaled(
            value,
            &[
                (86_400_000_000_000, "d"),
                (3_600_000_000_000, "h"),
                (60_000_000_000, "min"),
                (1_000_000_000, "s"),
                (1_000_000, "ms"),
                (1_000, "us"),
            ],
            "ns",
        )
        .into_bytes()
    }))
}

/// Shared structural port of `GetFormatBytes` and `GetFormatNanoTime`.
fn format_scaled(value: f64, scales: &[(u64, &str)], base_unit: &str) -> String {
    let magnitude = value.abs();
    let Some(&(divisor, unit)) = scales
        .iter()
        .find(|(divisor, _)| magnitude >= *divisor as f64)
    else {
        return format!("{} {base_unit}", fixed(value, 0));
    };
    let scaled = value / divisor as f64;
    let number = if scaled.abs() >= 100_000.0 {
        scientific(scaled)
    } else {
        fixed(scaled, 2)
    };
    format!("{number} {unit}")
}

/// Go's `strconv.FormatFloat(value, 'f', precision, 64)` uses positive zero
/// for `-0`, as confirmed with `FORMAT_BYTES(-0.0)` and
/// `FORMAT_NANO_TIME(-0.0)` through `goeval`.
fn fixed(value: f64, precision: usize) -> String {
    let value = if value == 0.0 { 0.0 } else { value };
    format!("{value:.precision$}")
}

/// Go's `strconv.FormatFloat(value, 'e', 2, 64)` always emits an exponent
/// sign and pads its absolute exponent to at least two digits (`e+08`). Rust
/// supplies the correctly rounded mantissa, then this normalizes only that
/// spelling difference.
fn scientific(value: f64) -> String {
    let rendered = format!("{value:.2e}");
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("Rust scientific format always contains an exponent");
    let exponent = exponent
        .parse::<i32>()
        .expect("Rust scientific exponent is a signed integer");
    format!("{mantissa}e{exponent:+03}")
}

const IPV4_LENGTH: usize = 4;
const IPV6_LENGTH: usize = 16;
const PREFIX_COMPAT: [u8; 12] = [0x00; 12];
const PREFIX_MAPPED: [u8; 12] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
];

#[rpn_fn(nullable, varg)]
#[inline]
pub fn any_value<T: Evaluable + EvaluableRet>(args: &[Option<&T>]) -> Result<Option<T>> {
    if let Some(arg) = args.first() {
        Ok(arg.cloned())
    } else {
        Ok(None)
    }
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn any_value_json(args: &[Option<JsonRef>]) -> Result<Option<Json>> {
    if let Some(arg) = args.first() {
        Ok(arg.map(|x| x.to_owned()))
    } else {
        Ok(None)
    }
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn any_value_vector_float32(
    args: &[Option<VectorFloat32Ref>],
) -> Result<Option<VectorFloat32>> {
    if let Some(arg) = args.first() {
        Ok(arg.map(|x| x.to_owned()))
    } else {
        Ok(None)
    }
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn any_value_bytes(args: &[Option<BytesRef>]) -> Result<Option<Bytes>> {
    if let Some(arg) = args.first() {
        Ok(arg.map(|x| x.to_vec()))
    } else {
        Ok(None)
    }
}

#[rpn_fn]
#[inline]
pub fn inet_aton(addr: BytesRef) -> Result<Option<Int>> {
    let addr = String::from_utf8_lossy(addr);

    if addr.is_empty() || addr.ends_with('.') {
        return Ok(None);
    }
    let (mut byte_result, mut result, mut dot_count): (u64, u64, usize) = (0, 0, 0);
    for c in addr.chars() {
        if c.is_ascii_digit() {
            let digit = c as u64 - '0' as u64;
            byte_result = byte_result * 10 + digit;
            if byte_result > 255 {
                return Ok(None);
            }
        } else if c == '.' {
            dot_count += 1;
            if dot_count > 3 {
                return Ok(None);
            }
            result = (result << 8) + byte_result;
            byte_result = 0;
        } else {
            return Ok(None);
        }
    }
    if dot_count == 1 {
        result <<= 16;
    } else if dot_count == 2 {
        result <<= 8;
    }

    Ok(Some(((result << 8) + byte_result) as i64))
}

#[rpn_fn(nullable)]
#[inline]
pub fn inet_ntoa(arg: Option<&Int>) -> Result<Option<Bytes>> {
    Ok(arg
        .cloned()
        .and_then(|arg| u32::try_from(arg).ok())
        .map(|arg| format!("{}", Ipv4Addr::from(arg)).into_bytes()))
}

#[rpn_fn(nullable)]
#[inline]
pub fn inet6_aton(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    let input = match input {
        Some(input) => String::from_utf8_lossy(input),
        None => return Ok(None),
    };

    let ipv6_addr = Ipv6Addr::from_str(&input).map(|t| t.octets().to_vec());
    let ipv4_addr_eval = |_| Ipv4Addr::from_str(&input).map(|t| t.octets().to_vec());
    ipv6_addr
        .or_else(ipv4_addr_eval)
        .map(Option::Some)
        .or(Ok(None))
}

#[rpn_fn(nullable)]
#[inline]
pub fn inet6_ntoa(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(arg.and_then(|s| {
        if s.len() == IPV6_LENGTH {
            let v: &[u8; 16] = s.try_into().unwrap();
            Some(format!("{}", Ipv6Addr::from(*v)).into_bytes())
        } else if s.len() == IPV4_LENGTH {
            let v: &[u8; 4] = s.try_into().unwrap();
            Some(format!("{}", Ipv4Addr::from(*v)).into_bytes())
        } else {
            None
        }
    }))
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_ipv4(addr: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(match addr {
        Some(addr) => match std::str::from_utf8(addr) {
            Ok(addr) => {
                if Ipv4Addr::from_str(addr).is_ok() {
                    Some(1)
                } else {
                    Some(0)
                }
            }
            _ => Some(0),
        },
        None => Some(0),
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_ipv4_compat(addr: Option<BytesRef>) -> Result<Option<i64>> {
    Ok(addr.as_ref().map_or(Some(0), |addr| {
        if addr.len() != IPV6_LENGTH || !addr.starts_with(&PREFIX_COMPAT) {
            Some(0)
        } else {
            Some(1)
        }
    }))
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_ipv4_mapped(addr: Option<BytesRef>) -> Result<Option<i64>> {
    Ok(addr.as_ref().map_or(Some(0), |addr| {
        if addr.len() != IPV6_LENGTH || !addr.starts_with(&PREFIX_MAPPED) {
            Some(0)
        } else {
            Some(1)
        }
    }))
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_ipv6(addr: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(match addr {
        Some(addr) => match std::str::from_utf8(addr) {
            Ok(addr) => {
                if Ipv6Addr::from_str(addr).is_ok() {
                    Some(1)
                } else {
                    Some(0)
                }
            }
            _ => Some(0),
        },
        None => Some(0),
    })
}

#[rpn_fn]
#[inline]
fn is_ipv4_nullable(arg: BytesRef) -> Result<Option<Int>> {
    is_ipv4(Some(arg))
}

#[rpn_fn]
#[inline]
fn is_ipv6_nullable(arg: BytesRef) -> Result<Option<Int>> {
    is_ipv6(Some(arg))
}

#[rpn_fn]
#[inline]
fn is_ipv4_compat_nullable(arg: BytesRef) -> Result<Option<Int>> {
    is_ipv4_compat(Some(arg))
}

#[rpn_fn]
#[inline]
fn is_ipv4_mapped_nullable(arg: BytesRef) -> Result<Option<Int>> {
    is_ipv4_mapped(Some(arg))
}

#[rpn_fn(nullable)]
#[inline]
pub fn uuid() -> Result<Option<Bytes>> {
    let mut node_id = rand::thread_rng().gen::<[u8; 6]>();
    node_id[0] |= 0x01; // RFC 4122 multicast bit

    let result = Uuid::now_v1(&node_id);
    Ok(Some(format_uuid_native(result.as_bytes()).into_bytes()))
}

#[rpn_fn(nullable)]
#[inline]
pub fn uuid_version(input: Option<BytesRef>) -> Result<Option<Int>> {
    let input = match input {
        Some(input) => String::from_utf8_lossy(input),
        None => return Ok(None),
    };
    let uuid = Uuid::parse_str(&input);
    match uuid {
        Ok(u) => Ok(Some(u.get_version_num() as i64)),
        Err(_e) => Ok(None),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn uuid_timestamp(input: Option<BytesRef>) -> Result<Option<Decimal>> {
    let input = match input {
        Some(input) => String::from_utf8_lossy(input),
        None => return Ok(None),
    };
    let uuid = Uuid::parse_str(&input);
    if uuid.is_err() {
        return Ok(None);
    };
    let ts = uuid.unwrap().get_timestamp();
    let (s, ns) = match ts {
        None => return Ok(None),
        Some(t) => t.to_unix(),
    };
    // s * 1_000_000 to convert from seconds to microseconds
    // ns / 1_000 to convert from nanoseconds to microseconds
    // shift by -6 to get from microseconds to seconds
    // in the end we return a decimal of seconds since the UNIX epoch.
    Ok(Some(uuid_decimal_micros(Decimal::from(
        s * 1_000_000 + ((ns as u64) / 1_000),
    ))))
}

#[cfg(test)]
mod native_grouping_tests {
    use super::*;

    fn packet(groups: &[Vec<u64>]) -> Vec<u8> {
        let mut bytes = (groups.len() as u64).to_le_bytes().to_vec();
        for group in groups {
            bytes.extend_from_slice(&(group.len() as u64).to_le_bytes());
            for mark in group {
                bytes.extend_from_slice(&mark.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn grouping_fixed_profiles_full_width_and_wrapping() {
        let gid = u64::MAX.to_le_bytes();
        let marks = packet(&[vec![u64::MAX], vec![1 << 63], vec![0]]);
        for (mode, recipe, expected) in [
            (
                GroupingMode::BitAnd,
                grouping_bit_and_native as fn(BytesRef, BytesRef) -> Result<Option<Int>>,
                1,
            ),
            (GroupingMode::NumericCmp, grouping_numeric_cmp_native, 4),
            (GroupingMode::NumericSet, grouping_numeric_set_native, 3),
        ] {
            assert!(grouping_native_args_valid(&gid, &marks, mode));
            assert_eq!(recipe(&gid, &marks).unwrap(), Some(expected));
            assert_eq!(recipe(&gid, &packet(&[])).unwrap(), Some(0));
        }
        let multiple = packet(&[vec![0, u64::MAX], vec![], vec![u64::MAX]]);
        assert_eq!(
            grouping_numeric_set_native(&gid, &multiple).unwrap(),
            Some(2)
        );
        assert!(grouping_bit_and_native(&gid, &multiple).is_err());
        assert!(grouping_numeric_cmp_native(&gid, &multiple).is_err());
        let ones = packet(&vec![vec![]; 65]);
        assert_eq!(grouping_numeric_set_native(&gid, &ones).unwrap(), Some(-1));
        let mut shifted_out = vec![vec![u64::MAX]; 65];
        shifted_out[0] = vec![];
        assert_eq!(
            grouping_numeric_set_native(&gid, &packet(&shifted_out)).unwrap(),
            Some(0)
        );
        let mut function =
            GroupingFunction::with_metadata(GroupingMode::BitAnd, vec![BTreeSet::from([6])])
                .unwrap();
        assert_eq!(function.eval(1), Ok(1));
        assert!(
            function
                .set_metadata(GroupingMode::BitAnd, vec![BTreeSet::new()])
                .is_err()
        );
        assert_eq!(function.eval(1), Err(GroupingMetadataError::Uninitialized));
        assert_eq!(
            GroupingMode::try_from(0),
            Err(GroupingMetadataError::InvalidMode(0))
        );
    }

    #[test]
    fn grouping_envelopes_reject_counts_order_trailing_and_false_null() {
        let gid = 1_u64.to_le_bytes();
        let valid = packet(&[vec![1]]);
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut opcode_packet = vec![GroupingMode::NumericSet as u8];
        opcode_packet.extend_from_slice(&valid);
        let mut huge_marks = 1_u64.to_le_bytes().to_vec();
        huge_marks.extend_from_slice(&u64::MAX.to_le_bytes());
        for malformed in [
            vec![],
            vec![0; 7],
            u64::MAX.to_le_bytes().to_vec(),
            huge_marks,
            trailing,
            opcode_packet,
            packet(&[vec![2, 1]]),
            packet(&[vec![1, 1]]),
        ] {
            assert!(!grouping_native_args_valid(
                &gid,
                &malformed,
                GroupingMode::NumericSet
            ));
            assert!(grouping_numeric_set_native(&gid, &malformed).is_err());
        }
        for short in [&gid[..0], &gid[..7]] {
            assert!(!grouping_native_args_valid(
                short,
                &valid,
                GroupingMode::BitAnd
            ));
            assert!(grouping_bit_and_native(short, &valid).is_err());
        }
        let empty_set = packet(&[vec![]]);
        assert!(!grouping_native_args_valid(
            &gid,
            &empty_set,
            GroupingMode::BitAnd
        ));
        assert!(!grouping_native_args_valid(
            &gid,
            &empty_set,
            GroupingMode::NumericCmp
        ));
        assert!(grouping_native_args_valid(
            &gid,
            &empty_set,
            GroupingMode::NumericSet
        ));
        assert_eq!(grouping_null_native(None).unwrap(), None);
        assert!(grouping_null_native(Some(&0)).is_err());
    }
}

#[cfg(test)]
mod tests {
    use bstr::ByteVec;
    use tidb_query_datatype::expr::EvalContext;
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::test_util::RpnFnScalarEvaluator;

    #[test]
    fn existing_any_value_bytes_preserves_native_identity_frames_and_varg_contract() {
        use crate::{
            NativeIdentityRef as Identity, decode_native_identity, encode_native_identity,
        };

        assert_eq!(any_value_bytes_fn_meta().name, "any_value_bytes");
        // The framing module covers all eighteen non-NULL representations.
        // Here the existing worker must copy representative opaque frames,
        // preserving their representation rather than interpreting SQL values.
        let cases = [
            Identity::Real(0x7ff8_0000_0000_0042),
            Identity::Float32(0x7ff8_0000_0000_0042),
            Identity::Decimal {
                negative: true,
                scale: 2,
                storage_scale: 9,
                declared_shape: Some((-1, 7)),
                coefficient: &[255, 0, b'x'],
            },
            Identity::Time {
                core: u64::MAX,
                kind: 0,
                fsp: 7,
            },
            Identity::Json {
                type_code: 255,
                bytes: &[0, 255, b'['],
            },
            Identity::Vector(&[0x42, 0, 0xc0, 0x7f, 0, 0, 0, 0x80]),
        ];
        for value in cases {
            let frame = encode_native_identity(value).unwrap();
            let expected = frame.clone();
            let output = any_value_bytes(&[Some(frame.as_slice())]).unwrap().unwrap();
            assert_eq!(output, expected);
            assert_ne!(output.as_ptr(), frame.as_ptr());
            drop(frame);
            assert_eq!(output, expected);
            assert_eq!(decode_native_identity(&output), Ok(value));
        }
        assert_eq!(any_value_bytes(&[None]).unwrap(), None);
        // Ordinary varg behavior remains unchanged. These leaf calls do NOT
        // admit zero/multiple operands through either fixed-arity C4 profile;
        // their one-operand boundary is checked separately by the C4 tests.
        assert_eq!(any_value_bytes(&[]).unwrap(), None);
        let first = encode_native_identity(Identity::Bytes(&[255, 0])).unwrap();
        let second = encode_native_identity(Identity::Int(42)).unwrap();
        assert_eq!(
            any_value_bytes(&[Some(&first), Some(&second)]).unwrap(),
            Some(first.clone())
        );
        assert_eq!(any_value_bytes(&[None, Some(&second)]).unwrap(), None);
    }

    #[test]
    fn test_native_shard_vitess_source_literals() {
        // Original tidb-util vitess.rs five fixed hexadecimal digests.
        for (key, expected) in [
            (30_375_298_039_i64, "031265661E5F1133"),
            (1123, "031B565D41BDF8CA"),
            (30_573_721_600, "1EFD6439F2050FFD"),
            (116, "1E1788FF0FDE093C"),
            (-1, "355550B2150E2451"),
        ] {
            let digest = get_native_vitess_hash(Some(&key)).unwrap().unwrap() as u64;
            assert_eq!(format!("{digest:016X}"), expected);
        }
        // Original builtin_ext/misc.rs fixtures, including a high-bit output.
        assert_eq!(
            get_native_vitess_hash(Some(&0)).unwrap().unwrap() as u64,
            10_134_873_677_816_210_343_u64
        );
        for (key, expected) in [(-1, 81), (0, 167), (1, 214), (9_999_999_999_999_999, 63)] {
            assert_eq!(get_native_tidb_shard(Some(&key)).unwrap(), Some(expected));
        }
        assert_eq!(get_native_tidb_shard(None).unwrap(), None);
        assert_eq!(get_native_vitess_hash(None).unwrap(), None);
    }

    #[test]
    fn test_native_format_bytes_source_literals_and_transport() {
        // Original builtin_ext/info.rs finite vectors and negative-zero case.
        for (value, expected) in [
            (0.0_f64, "0 bytes"),
            (2048.0, "2.00 KiB"),
            (75_295_729.0, "71.81 MiB"),
            (5_287_242_702.0, "4.92 GiB"),
            (5_039_757_204_245.0, "4.58 TiB"),
            (890_250_274_520_475_525.0, "790.70 PiB"),
            (18_446_644_073_709_551_615.0, "16.00 EiB"),
            (287_952_852_482_075_252_752_429_875.0, "2.50e+08 EiB"),
            (-18_446_644_073_709_551_615.0, "-16.00 EiB"),
            (-0.0, "0 bytes"),
        ] {
            let input = value.to_bits().to_le_bytes();
            assert_eq!(
                get_native_format_bytes(Some(&input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        assert_eq!(get_native_format_bytes(None).unwrap(), None);
        assert!(get_native_format_bytes(Some(&[0; 7])).is_err());
        // Source-derived ready-value/transport checks, not old SQL rows:
        // overflow coercion supplies MAX; raw NaN survives; raw infinity still
        // reaches the original scientific().expect panic, not a finite filter.
        for (value, expected) in [(f64::MAX, "1.56e+290 EiB"), (f64::NAN, "NaN bytes")] {
            let input = value.to_bits().to_le_bytes();
            assert_eq!(
                get_native_format_bytes(Some(&input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            let input = value.to_bits().to_le_bytes();
            assert!(std::panic::catch_unwind(|| get_native_format_bytes(Some(&input))).is_err());
        }
    }

    #[test]
    fn test_native_format_nano_time_source_literals_and_transport() {
        // Original builtin_ext/info.rs finite vectors and negative-zero case.
        for (value, expected) in [
            (0.0_f64, "0 ns"),
            (2000.0, "2.00 us"),
            (898_787_877.0, "898.79 ms"),
            (9_999_999_991.0, "10.00 s"),
            (898_787_877_424.0, "14.98 min"),
            (5_827_527_520_021.0, "1.62 h"),
            (42_566_623_663_736_353.0, "492.67 d"),
            (4_827_524_825_702_572_425_242_552.0, "5.59e+10 d"),
            (-9_999_999_991.0, "-10.00 s"),
            (-0.0, "0 ns"),
        ] {
            let input = value.to_bits().to_le_bytes();
            assert_eq!(
                get_native_format_nano_time(Some(&input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        assert_eq!(get_native_format_nano_time(None).unwrap(), None);
        assert!(get_native_format_nano_time(Some(&[0; 9])).is_err());
        // Source-derived ready-value/transport checks, not old SQL rows.
        for (value, expected) in [(f64::MAX, "2.08e+294 d"), (f64::NAN, "NaN ns")] {
            let input = value.to_bits().to_le_bytes();
            assert_eq!(
                get_native_format_nano_time(Some(&input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            let input = value.to_bits().to_le_bytes();
            assert!(
                std::panic::catch_unwind(|| get_native_format_nano_time(Some(&input))).is_err()
            );
        }
    }

    #[test]
    fn test_uuid_native_parse_policy_and_null() {
        // Canonical/compact/braced source spellings, plus hand-derived ignored
        // wrapper and mixed-case URN policy literals (not provider recordings).
        let canonical = b"6ccd780c-baba-1026-9564-5b8c656024db";
        let expected = hex("6ccd780cbaba102695645b8c656024db");
        for input in [
            canonical.as_slice(),
            b"6CCD780CBABA102695645B8C656024DB",
            b"{6ccd780c-baba-1026-9564-5b8c656024db}",
            b"X6ccd780c-baba-1026-9564-5b8c656024dbY",
            b"UrN:UuId:6ccd780c-baba-1026-9564-5b8c656024db",
            b"\xff6ccd780c-baba-1026-9564-5b8c656024db\xfe",
        ] {
            assert_eq!(is_uuid_native(Some(input)).unwrap(), Some(1));
            assert_eq!(
                uuid_to_bin_parse_native(Some(input)).unwrap(),
                Some(expected.clone())
            );
            assert_eq!(uuid_version_native(Some(input)).unwrap(), Some(1));
        }
        // No common trim step: version accepts these ignored wrapper bytes.
        let spaces = b" 6ccd780c-baba-1026-9564-5b8c656024db ";
        assert_eq!(is_uuid_native(Some(spaces)).unwrap(), Some(0));
        assert_eq!(uuid_version_native(Some(spaces)).unwrap(), Some(1));
        assert_eq!(is_uuid_native(Some(b"abc")).unwrap(), Some(0));
        assert_eq!(is_uuid_native(None).unwrap(), None);
        assert_eq!(uuid_version_native(None).unwrap(), None);
        assert_eq!(uuid_to_bin_parse_native(None).unwrap(), None);
        // The wire parser must NOT inherit native's arbitrary-wrapper policy.
        assert_eq!(
            uuid_version(Some(b"X6ccd780c-baba-1026-9564-5b8c656024dbY")).unwrap(),
            None
        );
    }

    #[test]
    fn test_uuid_native_binary_source_vectors() {
        // Existing native UUID_TO_BIN/BIN_TO_UUID source literals.
        let normal = hex("6ccd780cbaba102695645b8c656024db");
        let swapped = hex("1026baba6ccd780c95645b8c656024db");
        assert_eq!(
            uuid_to_bin_swap_native(Some(&normal), Some(&0)).unwrap(),
            Some(normal.clone())
        );
        assert_eq!(
            uuid_to_bin_swap_native(Some(&normal), Some(&1)).unwrap(),
            Some(swapped.clone())
        );
        let canonical = b"6ccd780c-baba-1026-9564-5b8c656024db".to_vec();
        assert_eq!(
            bin_to_uuid_native(Some(&normal), Some(&0)).unwrap(),
            Some(canonical.clone())
        );
        assert_eq!(
            bin_to_uuid_native(Some(&swapped), Some(&1)).unwrap(),
            Some(canonical)
        );
        assert_eq!(bin_to_uuid_native(None, Some(&0)).unwrap(), None);
        assert!(uuid_to_bin_swap_native(None, Some(&0)).is_err());
        assert!(uuid_to_bin_swap_native(Some(&normal), None).is_err());
        let error = uuid_to_bin_swap_native(Some(b"short"), Some(&1)).unwrap_err();
        assert!(matches!(
            *error.0,
            tidb_query_common::error::ErrorInner::Evaluate(EvaluateError::Other(_))
        ));
    }

    #[test]
    fn test_uuid_native_timestamp_source_vectors() {
        // Exact native fixture literals, including signed pre-1970 output.
        for (text, expected) in [
            ("5f13f854-d74a-11f0-9b7a-0ae0156bd76b", "1765537487.118139"),
            ("1f0e48c1-7860-69cc-9b3f-35f89c103d4d", "1766995078.970004"),
            ("019b1440-87b7-7380-ab00-ce413e795004", "1765571332.023000"),
            ("6ccd780cbaba102695645b8c656024db", "-11129156903.290674"),
        ] {
            let actual = uuid_timestamp_native(Some(text.as_bytes()))
                .unwrap()
                .unwrap();
            assert_eq!(actual, Decimal::from_str(expected).unwrap());
            assert_eq!(actual.result_frac_cnt(), 6);
        }
        assert_eq!(uuid_timestamp_native(None).unwrap(), None);
        assert_eq!(
            uuid_timestamp_native(Some(b"a3e3b4a1-ea6d-471e-9860-8303a8b261f6")).unwrap(),
            None
        );
    }

    #[test]
    fn test_uuid_native_typed_error_causes() {
        use tidb_query_common::error::ErrorInner;
        let whitespace = uuid_to_bin_parse_native(Some(b" bad ")).unwrap_err();
        assert!(matches!(
            *whitespace.0,
            ErrorInner::Evaluate(EvaluateError::UuidToBinWhitespace)
        ));
        let invalid = uuid_to_bin_parse_native(Some(b"bad")).unwrap_err();
        assert!(matches!(
            *invalid.0,
            ErrorInner::Evaluate(EvaluateError::UuidToBinInvalid)
        ));
        let version = uuid_version_native(Some(b"bad")).unwrap_err();
        assert!(matches!(
            *version.0,
            ErrorInner::Evaluate(EvaluateError::UuidVersionInvalid)
        ));
        let timestamp = uuid_timestamp_native(Some(b"bad")).unwrap_err();
        assert!(matches!(
            *timestamp.0,
            ErrorInner::Evaluate(EvaluateError::UuidTimestampInvalid)
        ));
        // Hand-derived binary diagnostic payload; no lossy conversion in cause.
        let invalid_bytes = b"\xffx";
        let bin = bin_to_uuid_native(Some(invalid_bytes), Some(&0)).unwrap_err();
        let ErrorInner::Evaluate(cause @ EvaluateError::BinToUuidInvalidLength { .. }) = *bin.0
        else {
            panic!("expected typed length cause");
        };
        assert_eq!(cause.code(), 1411);
        let EvaluateError::BinToUuidInvalidLength { input } = cause else {
            unreachable!()
        };
        assert_eq!(input, invalid_bytes);
        for cause in [
            EvaluateError::UuidToBinWhitespace,
            EvaluateError::UuidToBinInvalid,
            EvaluateError::UuidVersionInvalid,
            EvaluateError::UuidTimestampInvalid,
        ] {
            assert_eq!(cause.code(), 10000);
        }
    }

    fn hex(data: impl AsRef<[u8]>) -> Vec<u8> {
        hex::decode(data).unwrap()
    }

    #[test]
    fn test_decimal_any_value() {
        let test_cases = vec![
            (vec![], None),
            (vec![Decimal::from(10)], Some(Decimal::from(10))),
            (
                vec![Decimal::from(10), Decimal::from(20)],
                Some(Decimal::from(10)),
            ),
            (
                vec![Decimal::from(10), Decimal::from(20), Decimal::from(30)],
                Some(Decimal::from(10)),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Decimal>(ScalarFuncSig::DecimalAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_duration_any_value() {
        let test_cases = vec![
            (vec![], None),
            (
                vec![Duration::from_millis(10, 0).unwrap()],
                Some(Duration::from_millis(10, 0).unwrap()),
            ),
            (
                vec![
                    Duration::from_millis(10, 0).unwrap(),
                    Duration::from_millis(11, 0).unwrap(),
                ],
                Some(Duration::from_millis(10, 0).unwrap()),
            ),
            (
                vec![
                    Duration::from_millis(10, 0).unwrap(),
                    Duration::from_millis(11, 0).unwrap(),
                    Duration::from_millis(12, 0).unwrap(),
                ],
                Some(Duration::from_millis(10, 0).unwrap()),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Duration>(ScalarFuncSig::DurationAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_int_any_value() {
        let test_cases = vec![
            (vec![], None),
            (vec![1i64], Some(1i64)),
            (vec![1i64, 2i64], Some(1i64)),
            (vec![1i64, 2i64, 3i64], Some(1i64)),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Int>(ScalarFuncSig::IntAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_json_any_value() {
        let test_cases = vec![
            (vec![], None),
            (
                vec![Json::from_u64(1).unwrap()],
                Some(Json::from_u64(1).unwrap()),
            ),
            (
                vec![Json::from_u64(1).unwrap(), Json::from_u64(2).unwrap()],
                Some(Json::from_u64(1).unwrap()),
            ),
            (
                vec![
                    Json::from_u64(1).unwrap(),
                    Json::from_u64(2).unwrap(),
                    Json::from_u64(3).unwrap(),
                ],
                Some(Json::from_u64(1).unwrap()),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Json>(ScalarFuncSig::JsonAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_real_any_value() {
        let test_cases = vec![
            (vec![], None),
            (
                vec![Real::new(1.2_f64).unwrap()],
                Some(Real::new(1.2_f64).unwrap()),
            ),
            (
                vec![Real::new(1.2_f64).unwrap(), Real::new(2.3_f64).unwrap()],
                Some(Real::new(1.2_f64).unwrap()),
            ),
            (
                vec![
                    Real::new(1.2_f64).unwrap(),
                    Real::new(2.3_f64).unwrap(),
                    Real::new(3_f64).unwrap(),
                ],
                Some(Real::new(1.2_f64).unwrap()),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Real>(ScalarFuncSig::RealAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_string_any_value() {
        let test_cases = vec![
            (vec![], None),
            (vec![Bytes::from("abc")], Some(Bytes::from("abc"))),
            (
                vec![Bytes::from("abc"), Bytes::from("def")],
                Some(Bytes::from("abc")),
            ),
            (
                vec![Bytes::from("abc"), Bytes::from("def"), Bytes::from("ojk")],
                Some(Bytes::from("abc")),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<Bytes>(ScalarFuncSig::StringAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_time_any_value() {
        let mut ctx = EvalContext::default();
        let test_cases = vec![
            (vec![], None),
            (
                vec![DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap()],
                Some(DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap()),
            ),
            (
                vec![
                    DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap(),
                    DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:01", 0, false).unwrap(),
                ],
                Some(DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap()),
            ),
            (
                vec![
                    DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap(),
                    DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:01", 0, false).unwrap(),
                    DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:02", 0, false).unwrap(),
                ],
                Some(DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).unwrap()),
            ),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate::<DateTime>(ScalarFuncSig::TimeAnyValue)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_inet_aton() {
        let test_cases = vec![
            (Some(b"0.0.0.0".to_vec()), Some(0)),
            (Some(b"255.255.255.255".to_vec()), Some(4294967295)),
            (Some(b"127.0.0.1".to_vec()), Some(2130706433)),
            (Some(b"113.14.22.3".to_vec()), Some(1896748547)),
            (Some(b"1".to_vec()), Some(1)),
            (Some(b"0.1.2".to_vec()), Some(65538)),
            (Some(b"0.1.2.3.4".to_vec()), None),
            (Some(b"0.1.2..3".to_vec()), None),
            (Some(b".0.1.2.3".to_vec()), None),
            (Some(b"0.1.2.3.".to_vec()), None),
            (Some(b"1.-2.3.4".to_vec()), None),
            (Some(b"".to_vec()), None),
            (Some(b"0.0.0.256".to_vec()), None),
            (Some(b"127.0.0,1".to_vec()), None),
            (None, None),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::InetAton)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_inet_ntoa() {
        let test_cases = vec![
            (Some(167773449), Some(Bytes::from("10.0.5.9"))),
            (Some(2063728641), Some(Bytes::from("123.2.0.1"))),
            (Some(0), Some(Bytes::from("0.0.0.0"))),
            (
                Some(i64::from(u32::MAX)),
                Some(Bytes::from("255.255.255.255")),
            ),
            (Some(545460846593), None),
            (Some(-1), None),
            (None, None),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Bytes>(ScalarFuncSig::InetNtoa)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_inet6_aton() {
        let test_cases = vec![
            (Some(b"0.0.0.0".to_vec()), Some(hex("00000000"))),
            (Some(b"10.0.5.9".to_vec()), Some(hex("0A000509"))),
            (
                Some(b"::1.2.3.4".to_vec()),
                Some(hex("00000000000000000000000001020304")),
            ),
            (
                Some(b"::FFFF:1.2.3.4".to_vec()),
                Some(hex("00000000000000000000FFFF01020304")),
            ),
            (
                Some(b"::fdfe:5a55:caff:fefa:9089".to_vec()),
                Some(hex("000000000000FDFE5A55CAFFFEFA9089")),
            ),
            (
                Some(b"fdfe::5a55:caff:fefa:9089".to_vec()),
                Some(hex("FDFE0000000000005A55CAFFFEFA9089")),
            ),
            (
                Some(b"2001:0db8:85a3:0000:0000:8a2e:0370:7334".to_vec()),
                Some(hex("20010db885a3000000008a2e03707334")),
            ),
            (Some(b"".to_vec()), None),
            (None, None),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Bytes>(ScalarFuncSig::Inet6Aton)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_inet6_ntoa() {
        let test_cases = vec![
            (Some(hex("00000000")), Some(b"0.0.0.0".to_vec())),
            (Some(hex("0A000509")), Some(b"10.0.5.9".to_vec())),
            (
                Some(hex("00000000000000000000000001020304")),
                // See https://github.com/rust-lang/libs-team/issues/239
                Some(b"::102:304".to_vec()),
            ),
            (
                Some(hex("00000000000000000000FFFF01020304")),
                Some(b"::ffff:1.2.3.4".to_vec()),
            ),
            (
                Some(hex("000000000000FDFE5A55CAFFFEFA9089")),
                Some(b"::fdfe:5a55:caff:fefa:9089".to_vec()),
            ),
            (
                Some(hex("FDFE0000000000005A55CAFFFEFA9089")),
                Some(b"fdfe::5a55:caff:fefa:9089".to_vec()),
            ),
            (
                Some(hex("20010db885a3123456788a2e03707334")),
                Some(b"2001:db8:85a3:1234:5678:8a2e:370:7334".to_vec()),
            ),
            // missing bytes
            (Some(b"".to_vec()), None),
            // missing a byte ipv4
            (Some(hex("20010d")), None),
            // missing a byte ipv6
            (Some(hex("00000000000000000000FFFFFFFFFF")), None),
            (None, None),
        ];

        for (i, (input, expect_output)) in test_cases.into_iter().enumerate() {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Bytes>(ScalarFuncSig::Inet6Ntoa)
                .unwrap();
            assert_eq!(output, expect_output, "case {}", i);
        }
    }

    #[test]
    fn test_is_ipv4() {
        let test_cases = vec![
            (Some(b"127.0.0.1".to_vec()), Some(1)),
            (Some(b"127.0.0.256".to_vec()), Some(0)),
            (None, Some(0)),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::IsIPv4)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_is_ipv4_compat() {
        let test_cases = vec![
            (Some(hex("00000000000000000001000001020304")), Some(0)),
            (Some(hex("00000000000000000000000001020304")), Some(1)),
            (Some(hex("10101010")), Some(0)),
            (Some(hex("00000000000000000001ffff01020304")), Some(0)),
            (Some(hex("00010203040506")), Some(0)),
            (None, Some(0)),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::IsIPv4Compat)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_is_ipv4_mapped() {
        let test_cases = vec![
            (Some(hex("00000000000000000001000001020304")), Some(0)),
            (Some(hex("00000000000000000000000001020304")), Some(0)),
            (Some(hex("10101010")), Some(0)),
            (Some(hex("00000000000000000000ffff01020304")), Some(1)),
            (Some(hex("00010203040506")), Some(0)),
            (None, Some(0)),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::IsIPv4Mapped)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_is_ipv6() {
        let test_cases = vec![
            (Some(b"::1".to_vec()), Some(1)),
            (Some(b"1:2:3:4:5:6:7:10000".to_vec()), Some(0)),
            (None, Some(0)),
        ];

        for (input, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::IsIPv6)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_uuid() {
        let got = RpnFnScalarEvaluator::new()
            .evaluate::<Bytes>(ScalarFuncSig::Uuid)
            .unwrap();
        let r = got.unwrap().into_string().unwrap_or_default();
        let v: Vec<&str> = r.split('-').collect();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].len(), 8);
        assert_eq!(v[1].len(), 4);
        assert_eq!(v[2].len(), 4);
        assert_eq!(v[3].len(), 4);
        assert_eq!(v[4].len(), 12);
        let u = Uuid::parse_str(&r).expect("Parsing UUID failed");
        assert_eq!(u.get_version_num(), 1);
    }

    #[test]
    fn test_uuid_version() {
        let test_cases = vec![
            ("5f13f854-d74a-11f0-9b7a-0ae0156bd76b", Some(1)),
            ("c6437ef1-5b86-3a4e-a071-c2d4ad414e65", Some(3)),
            ("a3e3b4a1-ea6d-471e-9860-8303a8b261f6", Some(4)),
            ("271a8175-dadd-5df9-b0bd-20a4a0b441e6", Some(5)),
            ("1f0e48c1-7860-69cc-9b3f-35f89c103d4d", Some(6)),
            ("019b1440-87b7-7380-ab00-ce413e795004", Some(7)),
        ];

        for (input, expected_ver) in test_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::UuidVersion)
                .unwrap();
            assert_eq!(got, expected_ver);
        }
    }

    #[test]
    fn test_uuid_timestamp() {
        let test_cases = vec![
            (
                "5f13f854-d74a-11f0-9b7a-0ae0156bd76b",
                Some(Decimal::from_str("1765537487.118139").unwrap()),
            ),
            ("c6437ef1-5b86-3a4e-a071-c2d4ad414e65", None),
            ("a3e3b4a1-ea6d-471e-9860-8303a8b261f6", None),
            ("271a8175-dadd-5df9-b0bd-20a4a0b441e6", None),
            (
                "1f0e48c1-7860-69cc-9b3f-35f89c103d4d",
                Some(Decimal::from_str("1766995078.970004").unwrap()),
            ),
            (
                "019b1440-87b7-7380-ab00-ce413e795004",
                Some(Decimal::from_str("1765571332.023000").unwrap()),
            ),
        ];

        for (input, expected_ts) in test_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Decimal>(ScalarFuncSig::UuidTimestamp)
                .unwrap();
            assert_eq!(got, expected_ts);
        }
    }
}
