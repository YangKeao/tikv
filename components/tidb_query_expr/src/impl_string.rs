// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{borrow::Cow, cmp::Ordering, convert::TryFrom, iter, str, sync::Arc};

use bstr::ByteSlice;
use memchr::memmem;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    codec::{
        collation::{encoding::unicode_to_lower, native::NativeCollation, *},
        data_type::*,
    },
    *,
};

use crate::{
    impl_math::i64_to_usize,
    local::{LocalError, LocalResult, NativeSearchPolicy},
};

const SPACE: u8 = 0o40u8;
const MAX_BLOB_WIDTH: i32 = 16_777_216; // FIXME: Should be isize

// see https://dev.mysql.com/doc/refman/5.7/en/string-functions.html#function_to-base64
// mysql base64 doc: A newline is added after each 76 characters of encoded
// output
const BASE64_LINE_WRAP_LENGTH: usize = 76;

// mysql base64 doc: Each 3 bytes of the input data are encoded using 4
// characters.
const BASE64_INPUT_CHUNK_LENGTH: usize = 3;
const BASE64_ENCODED_CHUNK_LENGTH: usize = 4;
const BASE64_LINE_WRAP: u8 = b'\n';

/// Returns the byte index of the char at `char_idx` in `s`.
/// If `char_idx` is larger then the number of UTF-8 chars in `s`,
/// the length of `s` in bytes is returned.
#[inline]
fn get_utf8_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .map(|(i, _)| i)
        .nth(char_idx)
        .unwrap_or(s.len())
}

#[rpn_fn(writer)]
#[inline]
pub fn bin(num: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(Bytes::from(format!("{:b}", num)))))
}

#[rpn_fn(writer)]
#[inline]
pub fn oct_int(num: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    oct_bits(*num as u64, writer)
}

#[inline]
fn oct_bits(bits: u64, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(format!("{:o}", bits).into_bytes())))
}

#[rpn_fn(writer)]
#[inline]
pub fn oct_string(s: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    if s.is_empty() {
        return Ok(writer.write(None));
    }
    let bits = oct_decimal_prefix(s.iter().copied().skip_while(u8::is_ascii_whitespace));
    oct_bits(bits, writer)
}

#[rpn_fn(writer)]
#[inline]
fn oct_string_native(s: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    if s.is_empty() {
        return Ok(writer.write(None));
    }
    let trimmed = match str::from_utf8(s) {
        Ok(text) => text.trim().as_bytes(),
        Err(_) => {
            let mut start = 0;
            let mut end = s.len();
            while start < end && s[start].is_ascii_whitespace() {
                start += 1;
            }
            while end > start && s[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            &s[start..end]
        }
    };
    oct_bits(oct_decimal_prefix(trimmed.iter().copied()), writer)
}

#[inline]
fn oct_decimal_prefix(mut bytes: impl Iterator<Item = u8>) -> u64 {
    let mut r = Some(0u64);
    let mut negative = false;
    let mut overflow = false;
    if let Some(c) = bytes.next() {
        if c == b'-' {
            negative = true;
        } else if c.is_ascii_digit() {
            r = Some(u64::from(c) - u64::from(b'0'));
        } else if c != b'+' {
            return 0;
        }

        for c in bytes.take_while(u8::is_ascii_digit) {
            r = r
                .and_then(|r| r.checked_mul(10))
                .and_then(|r| r.checked_add(u64::from(c - b'0')));
            if r.is_none() {
                overflow = true;
                break;
            }
        }
    }
    let mut r = r.unwrap_or(u64::MAX);
    if negative && !overflow {
        r = r.wrapping_neg();
    }
    r
}

#[rpn_fn]
#[inline]
pub fn length(arg: BytesRef) -> Result<Option<i64>> {
    Ok(Some(arg.len() as i64))
}

#[rpn_fn(writer)]
#[inline]
pub fn unhex(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    // hex::decode will fail on odd-length content
    // but mysql won't
    // so do some padding
    let mut padded_content = Vec::with_capacity(arg.len() + arg.len() % 2);
    if arg.len() % 2 == 1 {
        padded_content.push(b'0')
    }
    padded_content.extend_from_slice(arg);
    Ok(writer.write(hex::decode(padded_content).ok()))
}

#[inline]
fn search_bytes(haystack: BytesRef, needle: BytesRef) -> Option<usize> {
    memmem::find(haystack, needle)
}

#[inline]
fn find_str(text: &str, pattern: &str) -> Option<usize> {
    search_bytes(text.as_bytes(), pattern.as_bytes()).map(|i| text[..i].chars().count())
}

#[rpn_fn]
#[inline]
pub fn locate_2_args_utf8<C: Collator>(substr: BytesRef, s: BytesRef) -> Result<Option<i64>> {
    let substr = str::from_utf8(substr)?;
    let s = str::from_utf8(s)?;
    let offset = if C::IS_CASE_INSENSITIVE {
        find_str(&s.to_lowercase(), &substr.to_lowercase())
    } else {
        find_str(s, substr)
    };
    Ok(Some(offset.map_or(0, |i| 1 + i as i64)))
}

#[rpn_fn]
#[inline]
pub fn locate_3_args_utf8<C: Collator>(
    substr: BytesRef,
    s: BytesRef,
    pos: &Int,
) -> Result<Option<i64>> {
    if *pos < 1 {
        return Ok(Some(0));
    }
    let substr = str::from_utf8(substr)?;
    let s = str::from_utf8(s)?;
    let start = match s
        .char_indices()
        .map(|(i, _)| i)
        .chain(iter::once(s.len()))
        .nth(*pos as usize - 1)
    {
        Some(start) => start,
        None => return Ok(Some(0)),
    };
    let offset = if C::IS_CASE_INSENSITIVE {
        find_str(&s[start..].to_lowercase(), &substr.to_lowercase())
    } else {
        find_str(&s[start..], substr)
    };
    Ok(Some(offset.map_or(0, |i| pos + i as i64)))
}

#[derive(Clone, Copy)]
enum NativeLocatePolicy {
    Bytes,
    Utf8(Option<NativeCollation>),
}

fn decode_native_collation(tag: &Int) -> Result<NativeCollation> {
    NativeCollation::from_tag(*tag)
        .ok_or_else(|| other_err!("Invalid native collation policy {}", tag))
}

fn decode_native_search_policy(tag: &Int) -> Result<NativeLocatePolicy> {
    match NativeSearchPolicy::from_tag(*tag) {
        Some(NativeSearchPolicy::Bytes) => Ok(NativeLocatePolicy::Bytes),
        Some(NativeSearchPolicy::Utf8(collation)) => Ok(NativeLocatePolicy::Utf8(Some(collation))),
        None => Err(other_err!("Invalid native search policy {}", tag)),
    }
}

#[rpn_fn]
#[inline]
fn locate_2_native(needle: BytesRef, haystack: BytesRef, policy: &Int) -> Result<Option<Int>> {
    Ok(Some(native_locate_impl(
        needle,
        haystack,
        0,
        decode_native_search_policy(policy)?,
        false,
    )?))
}

#[rpn_fn]
#[inline]
fn locate_3_native(
    needle: BytesRef,
    haystack: BytesRef,
    position: &Int,
    policy: &Int,
) -> Result<Option<Int>> {
    let policy = decode_native_search_policy(policy)?;
    // Preserve the native source's unchecked subtraction, including MIN.
    let start = *position - 1;
    Ok(Some(native_locate_impl(
        needle, haystack, start, policy, true,
    )?))
}

#[rpn_fn]
#[inline]
fn locate_3_bytes_ext_native(
    needle: BytesRef,
    haystack: BytesRef,
    position: &Int,
) -> Result<Option<Int>> {
    Ok(Some(native_locate_impl(
        needle,
        haystack,
        position.wrapping_sub(1),
        NativeLocatePolicy::Bytes,
        false,
    )?))
}

#[rpn_fn]
#[inline]
fn locate_3_utf8_ext_native(
    needle: BytesRef,
    haystack: BytesRef,
    position: &Int,
) -> Result<Option<Int>> {
    Ok(Some(native_locate_impl(
        needle,
        haystack,
        position.wrapping_sub(1),
        NativeLocatePolicy::Utf8(None),
        false,
    )?))
}

fn native_locate_impl(
    needle: BytesRef,
    haystack: BytesRef,
    start: Int,
    policy: NativeLocatePolicy,
    lower_ci: bool,
) -> Result<Int> {
    let collation = match policy {
        NativeLocatePolicy::Bytes => {
            if start < 0 || start > haystack.len() as Int - needle.len() as Int {
                return Ok(0);
            }
            return Ok(search_bytes(&haystack[start as usize..], needle)
                .map_or(0, |offset| start + offset as Int + 1));
        }
        NativeLocatePolicy::Utf8(collation) => collation,
    };
    let needle = str::from_utf8(needle)?;
    let haystack = str::from_utf8(haystack)?;
    let (needle, haystack) = if lower_ci && collation.is_some_and(|c| c.is_ci()) {
        let lower = |input: &str| {
            input
                .chars()
                .map(|ch| unicode_to_lower(ch).unwrap_or(ch))
                .collect::<String>()
        };
        (Cow::Owned(lower(needle)), Cow::Owned(lower(haystack)))
    } else {
        (Cow::Borrowed(needle), Cow::Borrowed(haystack))
    };
    let needle_len = needle.chars().count();
    let boundaries: Vec<usize> = haystack
        .char_indices()
        .map(|(index, _)| index)
        .chain(iter::once(haystack.len()))
        .collect();
    let haystack_len = boundaries.len() - 1;
    if start < 0 || start > haystack_len as Int - needle_len as Int {
        return Ok(0);
    }
    if needle_len == 0 {
        return Ok(start + 1);
    }
    for index in start as usize..=haystack_len - needle_len {
        let candidate = &haystack.as_bytes()[boundaries[index]..boundaries[index + needle_len]];
        let matches = match collation {
            Some(collation) => collation.compare(candidate, needle.as_bytes())? == Ordering::Equal,
            None => candidate == needle.as_bytes(),
        };
        if matches {
            return Ok(index as Int + 1);
        }
    }
    Ok(0)
}

#[rpn_fn]
#[inline]
pub fn bit_length(arg: BytesRef) -> Result<Option<i64>> {
    Ok(Some(arg.len() as i64 * 8))
}

#[rpn_fn(nullable)]
#[inline]
pub fn ord<C: Collator>(arg: Option<BytesRef>) -> Result<Option<i64>> {
    let mut result = 0;
    if let Some(content) = arg {
        let size = if let Some((_, size)) = C::Charset::decode_one(content) {
            size
        } else {
            0
        };
        let bytes = &content[..size];
        result = ord_impl(bytes);
    }
    Ok(Some(result))
}

#[rpn_fn]
#[inline]
fn ord_native(arg: BytesRef) -> Result<Option<Int>> {
    // The closed factory validates the prepared slice has at most four bytes.
    Ok(Some(ord_impl(arg)))
}

#[inline]
fn ord_impl(bytes: BytesRef) -> Int {
    let mut result = 0;
    let mut factor = 1;
    for b in bytes.iter().rev() {
        result += i64::from(*b) * factor;
        factor *= 256;
    }
    result
}

#[rpn_fn(varg, writer, min_args = 1)]
#[inline]
pub fn concat(args: &[BytesRef], writer: BytesWriter) -> Result<BytesGuard> {
    let mut writer = writer.begin();
    concat_join(args.iter().copied().map(Ok), None, |part| {
        writer.partial_write(part);
    })?;
    Ok(writer.finish())
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn concat_ws(args: &[Option<BytesRef>]) -> Result<Option<Bytes>> {
    if let Some(sep) = args[0] {
        let mut output = Vec::new();
        concat_join(
            args[1..].iter().filter_map(|arg| *arg).map(Ok),
            Some(sep),
            |part| {
                output.extend_from_slice(part);
            },
        )?;
        Ok(Some(output))
    } else {
        Ok(None)
    }
}

fn concat_join<'a>(
    parts: impl Iterator<Item = Result<&'a [u8]>>,
    separator: Option<&[u8]>,
    mut write: impl FnMut(&[u8]),
) -> Result<()> {
    let mut first = true;
    for part in parts {
        let part = part?;
        if !first {
            if let Some(separator) = separator {
                write(separator);
            }
        }
        write(part);
        first = false;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcatKind {
    Concat,
    ConcatWs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcatTerminal {
    Complete,
    InputNull,
    PacketExceeded { limit: u64 },
}

/// An owned, checked encoding of only the actually demanded SQL prefix.
#[derive(Clone, Debug)]
pub struct PreparedConcatArgs {
    kind: ConcatKind,
    encoded: Vec<u8>,
}

impl PreparedConcatArgs {
    pub(crate) fn kind(&self) -> ConcatKind {
        self.kind
    }

    pub(crate) fn into_encoded(self) -> Vec<u8> {
        self.encoded
    }
}

#[derive(Clone, Copy)]
struct ConcatHeader {
    kind: ConcatKind,
    total: usize,
    prefix_count: usize,
    terminal: ConcatTerminal,
}

struct ConcatValidation {
    header: ConcatHeader,
    observed: usize,
    separator_len: u64,
    concat_len: usize,
    ws_budget: u64,
}

fn invalid_concat(message: &str) -> LocalError {
    LocalError::InvalidBatch(message.into())
}

impl ConcatValidation {
    fn new(header: ConcatHeader) -> LocalResult<Self> {
        let minimum = match header.kind {
            ConcatKind::Concat => 1,
            ConcatKind::ConcatWs => 2,
        };
        if header.total < minimum || header.prefix_count == 0 || header.prefix_count > header.total
        {
            return Err(invalid_concat(
                "Invalid CONCAT SQL arity or demanded prefix length",
            ));
        }
        match header.terminal {
            ConcatTerminal::Complete if header.prefix_count != header.total => {
                return Err(invalid_concat(
                    "Complete CONCAT requires every SQL argument",
                ));
            }
            ConcatTerminal::InputNull
                if header.kind == ConcatKind::ConcatWs && header.prefix_count != 1 =>
            {
                return Err(invalid_concat(
                    "NULL CONCAT_WS requires only the NULL separator",
                ));
            }
            ConcatTerminal::PacketExceeded { .. }
                if header.kind == ConcatKind::ConcatWs && header.prefix_count < 2 =>
            {
                return Err(invalid_concat(
                    "CONCAT_WS packet prefix requires a data argument",
                ));
            }
            _ => {}
        }
        Ok(Self {
            header,
            observed: 0,
            separator_len: 0,
            concat_len: 0,
            ws_budget: 0,
        })
    }

    fn observe(&mut self, value: Option<&[u8]>) -> LocalResult<()> {
        let index = self.observed;
        if index >= self.header.prefix_count {
            return Err(invalid_concat("Too many prepared CONCAT arguments"));
        }
        let last = index == self.header.prefix_count - 1;
        match self.header.kind {
            ConcatKind::Concat => {
                let must_be_null = self.header.terminal == ConcatTerminal::InputNull && last;
                if value.is_none() != must_be_null {
                    return Err(invalid_concat("Invalid NULL position in CONCAT prefix"));
                }
            }
            ConcatKind::ConcatWs => {
                if index == 0 {
                    let must_be_null = self.header.terminal == ConcatTerminal::InputNull;
                    if value.is_none() != must_be_null {
                        return Err(invalid_concat("Invalid CONCAT_WS separator nullness"));
                    }
                } else if last
                    && matches!(self.header.terminal, ConcatTerminal::PacketExceeded { .. })
                    && value.is_none()
                {
                    return Err(invalid_concat(
                        "CONCAT_WS packet trigger must be a non-NULL data argument",
                    ));
                }
            }
        }
        if let Some(bytes) = value {
            match self.header.kind {
                ConcatKind::Concat => self.concat_len = self.concat_len.saturating_add(bytes.len()),
                ConcatKind::ConcatWs if index == 0 => self.separator_len = bytes.len() as u64,
                ConcatKind::ConcatWs => {
                    self.ws_budget = self.ws_budget.saturating_add(bytes.len() as u64);
                    // SQL data index, including NULL slots, not surviving-part index.
                    if index > 1 {
                        self.ws_budget = self.ws_budget.saturating_add(self.separator_len);
                    }
                }
            }
        }
        self.observed = index
            .checked_add(1)
            .ok_or_else(|| invalid_concat("Prepared CONCAT argument count overflow"))?;
        Ok(())
    }

    fn finish(self) -> LocalResult<()> {
        if self.observed != self.header.prefix_count {
            return Err(invalid_concat("Incomplete prepared CONCAT prefix"));
        }
        if let ConcatTerminal::PacketExceeded { limit } = self.header.terminal {
            let requested = match self.header.kind {
                ConcatKind::Concat => self.concat_len as u64,
                ConcatKind::ConcatWs => self.ws_budget,
            };
            // Authenticate the recorded terminal, not another context decision.
            if requested <= limit {
                return Err(invalid_concat(
                    "Prepared CONCAT packet prefix does not exceed its observed limit",
                ));
            }
        }
        Ok(())
    }
}

/// Encode a real coerced prefix, without joining bytes or deciding a SQL
/// result.
pub fn prepare_concat_args(
    kind: ConcatKind,
    total_sql_arity: usize,
    prefix: Vec<Option<Vec<u8>>>,
    terminal: ConcatTerminal,
    max_encoded_bytes: usize,
) -> LocalResult<PreparedConcatArgs> {
    let header = ConcatHeader {
        kind,
        total: total_sql_arity,
        prefix_count: prefix.len(),
        terminal,
    };
    let mut validation = ConcatValidation::new(header)?;
    let total = u64::try_from(total_sql_arity).map_err(|_| {
        LocalError::ResourceLimit("CONCAT SQL arity exceeds transport width".into())
    })?;
    let count = u64::try_from(prefix.len()).map_err(|_| {
        LocalError::ResourceLimit("CONCAT prefix count exceeds transport width".into())
    })?;
    let mut encoded = Vec::new();
    let header_len = if matches!(terminal, ConcatTerminal::PacketExceeded { .. }) {
        26
    } else {
        18
    };
    reserve_concat_encoding(&mut encoded, header_len, max_encoded_bytes)?;
    encoded.push(match kind {
        ConcatKind::Concat => 0,
        ConcatKind::ConcatWs => 1,
    });
    encoded.push(match terminal {
        ConcatTerminal::Complete => 0,
        ConcatTerminal::InputNull => 1,
        ConcatTerminal::PacketExceeded { .. } => 2,
    });
    encoded.extend_from_slice(&total.to_le_bytes());
    encoded.extend_from_slice(&count.to_le_bytes());
    if let ConcatTerminal::PacketExceeded { limit } = terminal {
        encoded.extend_from_slice(&limit.to_le_bytes());
    }
    for value in prefix {
        validation.observe(value.as_deref())?;
        match value {
            None => {
                reserve_concat_encoding(&mut encoded, 1, max_encoded_bytes)?;
                encoded.push(0);
            }
            Some(bytes) => {
                let length = u64::try_from(bytes.len()).map_err(|_| {
                    LocalError::ResourceLimit("CONCAT argument exceeds transport width".into())
                })?;
                let additional = 9_usize.checked_add(bytes.len()).ok_or_else(|| {
                    LocalError::ResourceLimit("CONCAT encoded argument size overflow".into())
                })?;
                reserve_concat_encoding(&mut encoded, additional, max_encoded_bytes)?;
                encoded.push(1);
                encoded.extend_from_slice(&length.to_le_bytes());
                encoded.extend_from_slice(&bytes);
            }
        }
    }
    validation.finish()?;
    Ok(PreparedConcatArgs { kind, encoded })
}

fn reserve_concat_encoding(
    encoded: &mut Vec<u8>,
    additional: usize,
    cap: usize,
) -> LocalResult<()> {
    let length = encoded
        .len()
        .checked_add(additional)
        .ok_or_else(|| LocalError::ResourceLimit("CONCAT encoded size overflow".into()))?;
    if length > cap {
        return Err(LocalError::ResourceLimit(
            "CONCAT encoded byte cap exceeded".into(),
        ));
    }
    // Amortized growth keeps a many-argument prefix linear, rather than
    // requesting an exact reallocation for every record.
    encoded.try_reserve(additional).map_err(|error| {
        LocalError::ResourceLimit(format!("CONCAT encoding allocation: {}", error))
    })
}

#[derive(Clone)]
struct ConcatArgIter<'a> {
    encoded: &'a [u8],
    remaining: usize,
    offset: usize,
    finished: bool,
}

impl<'a> ConcatArgIter<'a> {
    fn read_arg(&mut self) -> LocalResult<Option<&'a [u8]>> {
        let tag = self
            .encoded
            .get(self.offset)
            .copied()
            .ok_or_else(|| invalid_concat("Truncated CONCAT argument tag"))?;
        self.offset = self
            .offset
            .checked_add(1)
            .ok_or_else(|| invalid_concat("CONCAT argument offset overflow"))?;
        match tag {
            0 => Ok(None),
            1 => {
                let length = usize::try_from(read_concat_word(self.encoded, &mut self.offset)?)
                    .map_err(|_| invalid_concat("CONCAT argument length exceeds usize"))?;
                let end = self
                    .offset
                    .checked_add(length)
                    .ok_or_else(|| invalid_concat("CONCAT argument end overflow"))?;
                let bytes = self
                    .encoded
                    .get(self.offset..end)
                    .ok_or_else(|| invalid_concat("Truncated CONCAT argument bytes"))?;
                self.offset = end;
                Ok(Some(bytes))
            }
            _ => Err(invalid_concat("Invalid CONCAT argument tag")),
        }
    }
}

impl<'a> Iterator for ConcatArgIter<'a> {
    type Item = LocalResult<Option<&'a [u8]>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.remaining == 0 {
            self.finished = true;
            return (self.offset != self.encoded.len())
                .then(|| Err(invalid_concat("Trailing prepared CONCAT bytes")));
        }
        self.remaining -= 1;
        let value = self.read_arg();
        if value.is_err() {
            self.finished = true;
        }
        Some(value)
    }
}

fn read_concat_word(encoded: &[u8], offset: &mut usize) -> LocalResult<u64> {
    let end = (*offset)
        .checked_add(8)
        .ok_or_else(|| invalid_concat("CONCAT word offset overflow"))?;
    let bytes = encoded
        .get(*offset..end)
        .ok_or_else(|| invalid_concat("Truncated CONCAT word"))?;
    let bytes =
        <[u8; 8]>::try_from(bytes).map_err(|_| invalid_concat("Invalid CONCAT word width"))?;
    *offset = end;
    Ok(u64::from_le_bytes(bytes))
}

struct DecodedConcat<'a> {
    terminal: ConcatTerminal,
    args: ConcatArgIter<'a>,
}

fn decode_concat_args(encoded: &[u8], kind: ConcatKind) -> LocalResult<DecodedConcat<'_>> {
    let actual_kind = match encoded.first().copied() {
        Some(0) => ConcatKind::Concat,
        Some(1) => ConcatKind::ConcatWs,
        _ => return Err(invalid_concat("Invalid prepared CONCAT kind")),
    };
    if actual_kind != kind {
        return Err(invalid_concat(
            "Prepared CONCAT kind does not match its operation",
        ));
    }
    let terminal_tag = encoded
        .get(1)
        .copied()
        .ok_or_else(|| invalid_concat("Missing CONCAT terminal tag"))?;
    let mut offset = 2;
    let total = usize::try_from(read_concat_word(encoded, &mut offset)?)
        .map_err(|_| invalid_concat("CONCAT SQL arity exceeds usize"))?;
    let prefix_count = usize::try_from(read_concat_word(encoded, &mut offset)?)
        .map_err(|_| invalid_concat("CONCAT prefix count exceeds usize"))?;
    let terminal = match terminal_tag {
        0 => ConcatTerminal::Complete,
        1 => ConcatTerminal::InputNull,
        2 => ConcatTerminal::PacketExceeded {
            limit: read_concat_word(encoded, &mut offset)?,
        },
        _ => return Err(invalid_concat("Invalid CONCAT terminal tag")),
    };
    let mut validation = ConcatValidation::new(ConcatHeader {
        kind,
        total,
        prefix_count,
        terminal,
    })?;
    let bytes_left = encoded
        .len()
        .checked_sub(offset)
        .ok_or_else(|| invalid_concat("Invalid CONCAT header length"))?;
    if prefix_count > bytes_left {
        return Err(invalid_concat("Truncated CONCAT argument records"));
    }
    let args = ConcatArgIter {
        encoded,
        remaining: prefix_count,
        offset,
        finished: false,
    };
    for value in args.clone() {
        validation.observe(value?)?;
    }
    validation.finish()?;
    Ok(DecodedConcat { terminal, args })
}

pub(crate) fn prepared_concat_args_match(encoded: Option<&[u8]>, kind: ConcatKind) -> bool {
    encoded.is_some_and(|encoded| decode_concat_args(encoded, kind).is_ok())
}

fn concat_eval_error(error: LocalError) -> tidb_query_common::Error {
    other_err!("Invalid prepared CONCAT arguments: {}", error)
}

#[rpn_fn(writer)]
#[inline]
fn concat_native(encoded: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    concat_native_impl(encoded, ConcatKind::Concat, writer)
}

#[rpn_fn(writer)]
#[inline]
fn concat_ws_native(encoded: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    concat_native_impl(encoded, ConcatKind::ConcatWs, writer)
}

fn concat_native_impl(
    encoded: BytesRef,
    kind: ConcatKind,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let decoded = decode_concat_args(encoded, kind).map_err(concat_eval_error)?;
    if decoded.terminal != ConcatTerminal::Complete {
        return Ok(writer.write(None));
    }
    let mut args = decoded.args.map(|value| value.map_err(concat_eval_error));
    let separator = if kind == ConcatKind::ConcatWs {
        Some(
            args.next()
                .transpose()?
                .flatten()
                .ok_or_else(|| other_err!("Missing non-NULL prepared CONCAT_WS separator"))?,
        )
    } else {
        None
    };
    let mut writer = writer.begin();
    concat_join(
        args.filter_map(|value| value.transpose()),
        separator,
        |part| {
            writer.partial_write(part);
        },
    )?;
    Ok(writer.finish())
}

// Observed only by an isolated test that joins all of its workers. Ordinary
// parallel tests must not assert deltas of this process-wide test-only counter.
#[cfg(test)]
static ASCII_TEST_BODY_INVOCATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn ascii_test_body_invocations() -> u64 {
    ASCII_TEST_BODY_INVOCATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

#[rpn_fn]
#[inline]
pub fn ascii(arg: BytesRef) -> Result<Option<i64>> {
    #[cfg(test)]
    ASCII_TEST_BODY_INVOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let result = match arg.is_empty() {
        true => 0,
        false => i64::from(arg[0]),
    };

    Ok(Some(result))
}

#[rpn_fn(writer)]
#[inline]
pub fn reverse_utf8(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let arg = str::from_utf8(arg)?;
    Ok(writer.write(Some(arg.chars().rev().collect::<String>().into_bytes())))
}

#[rpn_fn(writer)]
#[inline]
pub fn hex_int_arg(arg: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(format!("{:X}", arg).into_bytes())))
}

#[rpn_fn(writer)]
#[inline]
pub fn ltrim(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let pos = arg.iter().position(|&x| x != SPACE);
    let result = if let Some(i) = pos { &arg[i..] } else { b"" };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn rtrim(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let pos = arg.iter().rposition(|&x| x != SPACE);
    let result = if let Some(i) = pos { &arg[..=i] } else { b"" };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn lpad(arg: BytesRef, len: &Int, pad: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    pad_impl(arg, len, pad, PadMode::Bytes, PadPolicy::Wire, true, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn lpad_utf8(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_impl(arg, len, pad, PadMode::Utf8, PadPolicy::Wire, true, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn rpad(arg: BytesRef, len: &Int, pad: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    pad_impl(
        arg,
        len,
        pad,
        PadMode::Bytes,
        PadPolicy::Wire,
        false,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
pub fn rpad_utf8(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_impl(arg, len, pad, PadMode::Utf8, PadPolicy::Wire, false, writer)
}

#[rpn_fn(writer)]
#[inline]
fn lpad_bytes_native(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    disposition: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_native(arg, len, pad, disposition, PadMode::Bytes, true, writer)
}

#[rpn_fn(writer)]
#[inline]
fn rpad_bytes_native(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    disposition: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_native(arg, len, pad, disposition, PadMode::Bytes, false, writer)
}

#[rpn_fn(writer)]
#[inline]
fn lpad_utf8_native(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    disposition: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_native(arg, len, pad, disposition, PadMode::Utf8, true, writer)
}

#[rpn_fn(writer)]
#[inline]
fn rpad_utf8_native(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    disposition: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    pad_native(arg, len, pad, disposition, PadMode::Utf8, false, writer)
}

#[derive(Clone, Copy)]
enum PadMode {
    Bytes,
    Utf8,
}

#[derive(Clone, Copy)]
enum PadPolicy {
    Wire,
    Native,
}

#[inline]
fn pad_native(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    disposition: &Int,
    mode: PadMode,
    left: bool,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    if suppress_native_string(disposition)? {
        return Ok(writer.write(None));
    }
    if *len < 0 || *len > i64::from(MAX_BLOB_WIDTH) {
        return Ok(writer.write(None));
    }
    pad_impl(arg, len, pad, mode, PadPolicy::Native, left, writer)
}

#[inline]
fn pad_impl(
    arg: BytesRef,
    len: &Int,
    pad: BytesRef,
    mode: PadMode,
    policy: PadPolicy,
    left: bool,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    // Wire UTF-8 calls must decode both operands before validating the length.
    let (input_utf8, pad_utf8, size_of_type) = match mode {
        PadMode::Bytes => (None, None, 1),
        PadMode::Utf8 => (Some(str::from_utf8(arg)?), Some(str::from_utf8(pad)?), 4),
    };
    let input_len = input_utf8.map_or(arg.len(), |input| input.chars().count());
    let pad_len = pad_utf8.map_or(pad.len(), |pad| pad.chars().count());
    let target_len = match policy {
        PadPolicy::Wire => {
            validate_target_len_for_pad(*len < 0, *len, input_len, size_of_type, pad.is_empty())
        }
        // The native range was checked before decoding in pad_native.
        PadPolicy::Native => Some(*len as usize),
    };
    match target_len {
        None => Ok(writer.write(None)),
        Some(0) => Ok(writer.write_ref(Some(b""))),
        Some(target_len)
            if target_len < input_len
                || (matches!(policy, PadPolicy::Native) && target_len == input_len) =>
        {
            let byte_end =
                input_utf8.map_or(target_len, |input| get_utf8_byte_index(input, target_len));
            Ok(writer.write_ref(Some(&arg[..byte_end])))
        }
        Some(target_len) => {
            if matches!(policy, PadPolicy::Native) && pad.is_empty() {
                return Ok(writer.write_ref(Some(b"")));
            }
            let mut writer = writer.begin();
            if !left {
                writer.partial_write(arg);
            }

            // Keep the wire equal-length/empty-pad case on its original
            // quotient path; only native equality takes the truncation arm.
            let num_pads = (target_len - input_len) / pad_len;
            for _ in 0..num_pads {
                writer.partial_write(pad);
            }
            let last_pad_len = (target_len - input_len) % pad_len;
            let byte_end =
                pad_utf8.map_or(last_pad_len, |pad| get_utf8_byte_index(pad, last_pad_len));
            writer.partial_write(&pad[..byte_end]);
            if left {
                writer.partial_write(arg);
            }
            Ok(writer.finish())
        }
    }
}

// when target_len is 0, return Some(0), means the pad function should return
// empty string currently there are three conditions it return None, which means
// pad function should return Null
// - target_len is negative
// - target_len of type in byte is larger then MAX_BLOB_WIDTH
// - target_len is greater than length of input string, *and* pad string is
//   empty
// otherwise return Some(target_len)
#[inline]
fn validate_target_len_for_pad(
    len_unsigned: bool,
    target_len: i64,
    input_len: usize,
    size_of_type: usize,
    pad_empty: bool,
) -> Option<usize> {
    if target_len == 0 {
        return Some(0);
    }
    let (target_len, target_len_positive) = super::i64_to_usize(target_len, len_unsigned);
    if !target_len_positive
        || target_len.saturating_mul(size_of_type) > MAX_BLOB_WIDTH as usize
        || (pad_empty && input_len < target_len)
    {
        return None;
    }
    Some(target_len)
}

#[rpn_fn(writer)]
#[inline]
pub fn replace(
    s: BytesRef,
    from_str: BytesRef,
    to_str: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    if from_str.is_empty() {
        return Ok(writer.write_ref(Some(s)));
    }
    let mut last = 0;
    let mut writer = writer.begin();
    while let Some(mut start) = memmem::find(&s[last..], from_str) {
        start += last;
        writer.partial_write(&s[last..start]);
        writer.partial_write(to_str);
        last = start + from_str.len();
    }
    writer.partial_write(&s[last..]);
    Ok(writer.finish())
}

#[rpn_fn(writer)]
#[inline]
pub fn left(lhs: BytesRef, rhs: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if *rhs <= 0 {
        return Ok(writer.write_ref(Some(b"")));
    }
    let rhs = *rhs as usize;
    let result = if lhs.len() < rhs { lhs } else { &lhs[..rhs] };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn left_utf8(lhs: BytesRef, rhs: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if *rhs <= 0 {
        return Ok(writer.write_ref(Some(b"")));
    }
    let s = str::from_utf8(lhs)?;

    let rhs = *rhs as usize;
    let len = s.chars().count();
    let result = if len > rhs {
        let idx = get_utf8_byte_index(s, rhs);
        &s.as_bytes()[..idx]
    } else {
        s.as_bytes()
    };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn right(lhs: BytesRef, rhs: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if *rhs <= 0 {
        return Ok(writer.write_ref(Some(b"")));
    }
    let rhs = *rhs as usize;
    let result = if lhs.len() < rhs {
        lhs
    } else {
        &lhs[(lhs.len() - rhs)..]
    };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn insert(
    s: BytesRef,
    pos: &Int,
    len: &Int,
    newstr: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let Some((start, end)) = insert_range(*pos, *len, s.len()) else {
        return Ok(writer.write_ref(Some(s)));
    };
    insert_splice(s, start, end, newstr, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn insert_utf8(
    s_utf8: BytesRef,
    pos: &Int,
    len: &Int,
    newstr_utf8: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let s = str::from_utf8(s_utf8)?;
    let newstr = str::from_utf8(newstr_utf8)?;
    let Some((start, end)) = insert_range(*pos, *len, s.chars().count()) else {
        return Ok(writer.write_ref(Some(s_utf8)));
    };
    // Preserve the wire use of character offsets as byte offsets.
    insert_splice(s.as_bytes(), start, end, newstr.as_bytes(), writer)
}

#[rpn_fn(writer)]
#[inline]
fn insert_utf8_native(
    s_utf8: BytesRef,
    pos: &Int,
    len: &Int,
    newstr: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let s = str::from_utf8(s_utf8)?;
    let Some((start, end)) = insert_range(*pos, *len, s.chars().count()) else {
        return Ok(writer.write_ref(Some(s_utf8)));
    };
    let start = get_utf8_byte_index(s, start);
    let end = get_utf8_byte_index(s, end);
    insert_splice(s_utf8, start, end, newstr, writer)
}

#[inline]
fn insert_range(pos: Int, len: Int, source_len: usize) -> Option<(usize, usize)> {
    let upos: usize = pos as usize;
    let mut ulen: usize = len as usize;
    if pos < 1 || upos > source_len {
        return None;
    }
    if ulen > source_len - upos + 1 || len < 0 {
        ulen = source_len - upos + 1;
    }
    Some((upos - 1, upos + ulen - 1))
}

#[inline]
fn insert_splice(
    src: BytesRef,
    start: usize,
    end: usize,
    replacement: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let mut writer = writer.begin();
    writer.partial_write(&src[..start]);
    writer.partial_write(replacement);
    writer.partial_write(&src[end..]);
    Ok(writer.finish())
}

#[rpn_fn(writer)]
#[inline]
pub fn right_utf8(lhs: BytesRef, rhs: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if *rhs <= 0 {
        return Ok(writer.write_ref(Some(b"")));
    }

    let s = str::from_utf8(lhs)?;

    let rhs = *rhs as usize;
    let len = s.chars().count();
    let result = if len > rhs {
        let idx = get_utf8_byte_index(s, len - rhs);
        &s.as_bytes()[idx..]
    } else {
        s.as_bytes()
    };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn upper_utf8<E: Encoding>(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let s = str::from_utf8(arg)?;
    Ok(E::upper(s, writer))
}

#[rpn_fn(writer)]
#[inline]
// upper is a noop in TiDB side, keep the same logic here.
// ref: https://github.com/pingcap/tidb/blob/master/expression/builtin_string_vec.go#L152-L158
pub fn upper(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write_ref(Some(arg)))
}

#[rpn_fn(writer)]
#[inline]
pub fn lower_utf8<E: Encoding>(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let s = str::from_utf8(arg)?;
    Ok(E::lower(s, writer))
}

#[rpn_fn(writer)]
#[inline]
pub fn lower(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    // Noop for binary strings
    Ok(writer.write_ref(Some(arg)))
}

#[rpn_fn]
#[inline]
fn lower_ascii_native(arg: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(arg.to_ascii_lowercase()))
}

#[rpn_fn]
#[inline]
fn upper_ascii_native(arg: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(arg.to_ascii_uppercase()))
}

#[rpn_fn(writer)]
#[inline]
pub fn hex_str_arg(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(log_wrappers::hex_encode_upper(arg).into_bytes())))
}

#[rpn_fn]
#[inline]
pub fn locate_2_args(substr: BytesRef, s: BytesRef) -> Result<Option<i64>> {
    Ok(search_bytes(s, substr).map(|i| 1 + i as i64).or(Some(0)))
}

#[rpn_fn(writer)]
#[inline]
pub fn reverse(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let mut result = arg.to_vec();
    result.reverse();
    Ok(writer.write(Some(result)))
}

#[rpn_fn]
#[inline]
pub fn locate_3_args(substr: BytesRef, s: BytesRef, pos: &Int) -> Result<Option<Int>> {
    if *pos < 1 || *pos as usize > s.len() + 1 {
        return Ok(Some(0));
    }
    Ok(search_bytes(&s[*pos as usize - 1..], substr)
        .map(|i| pos + i as i64)
        .or(Some(0)))
}

#[rpn_fn(nullable, varg, min_args = 1)]
#[inline]
fn field<T: Evaluable + EvaluableRet + PartialEq>(args: &[Option<&T>]) -> Result<Option<Int>> {
    Ok(Some(match args[0] {
        // As per the MySQL doc, if the first argument is NULL, this function always returns 0.
        None => 0,
        Some(val) => args
            .iter()
            .skip(1)
            .position(|&i| i == Some(val))
            .map_or(0, |pos| (pos + 1) as i64),
    }))
}

#[rpn_fn(nullable, varg, min_args = 1)]
#[inline]
fn field_bytes<C: Collator>(args: &[Option<BytesRef>]) -> Result<Option<Int>> {
    Ok(Some(match args[0] {
        // As per the MySQL doc, if the first argument is NULL, this function always returns 0.
        None => 0,
        Some(val) => {
            for (pos, arg) in args.iter().enumerate().skip(1) {
                if arg.is_none() {
                    continue;
                }
                match C::sort_compare(val, arg.unwrap(), false) {
                    Ok(Ordering::Equal) => return Ok(Some(pos as i64)),
                    _ => continue,
                }
            }
            0
        }
    }))
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = elt_validator)]
#[inline]
pub fn make_set(raw_args: &[ScalarValueRef]) -> Result<Option<Bytes>> {
    assert!(raw_args.len() >= 2);
    let mask = raw_args[0].as_int();
    let mut output = Vec::new();
    let mut pow2 = 1;
    let s = b",";
    let mut q = false;
    match mask {
        None => {
            return Ok(None);
        }
        Some(mask2) => {
            for raw_arg in raw_args.iter().skip(1) {
                if pow2 & mask2 != 0 {
                    let input = raw_arg.as_bytes();
                    match input {
                        None => {}
                        Some(s2) => {
                            if q {
                                output.extend_from_slice(s);
                            }
                            output.extend_from_slice(s2);
                            q = true;
                        }
                    };
                }
                pow2 <<= 1;
            }
        }
    };
    Ok(Some(output))
}

/// Returns a demanded SQL operand offset; total arity includes the index.
pub fn elt_selected_arg(index: Option<i64>, total_sql_arity: usize) -> Option<usize> {
    let index = usize::try_from(index?).ok()?;
    if index == 0 || index >= total_sql_arity {
        None
    } else {
        Some(index)
    }
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = elt_validator)]
#[inline]
pub fn elt(raw_args: &[ScalarValueRef]) -> Result<Option<Bytes>> {
    assert!(raw_args.len() >= 2);
    Ok(
        elt_selected_arg(raw_args[0].as_int().copied(), raw_args.len())
            .and_then(|selected| raw_args[selected].as_bytes().map(|bytes| bytes.to_vec())),
    )
}

#[rpn_fn]
#[inline]
fn elt_native(index: &Int, raw_arity: &Int, selected: BytesRef) -> Result<Option<Bytes>> {
    let total_sql_arity = usize::try_from(*raw_arity as u64)
        .map_err(|_| other_err!("ELT total arity exceeds usize"))?;
    if total_sql_arity < 2 {
        return Err(other_err!("ELT requires at least two SQL arguments"));
    }
    Ok(elt_selected_arg(Some(*index), total_sql_arity).map(|_| selected.to_vec()))
}

/// validate the arguments are `(Option<&Int>, &[Option<BytesRef>)])`
fn elt_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    let children = expr.args();
    assert!(children.len() >= 2);
    super::function::validate_field_type(children[0].field_type(), EvalType::Int)?;
    for child in children.iter().skip(1) {
        super::function::validate_field_type(child.field_type(), EvalType::Bytes)?;
    }
    Ok(())
}

// The closed factory transports Allow as 0 and SuppressByPacket as 1.
#[inline]
fn suppress_native_string(disposition: &Int) -> Result<bool> {
    match *disposition {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(other_err!("Invalid native string disposition {}", value)),
    }
}

#[rpn_fn(writer)]
#[inline]
pub fn space(len: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    let guard = if *len > i64::from(tidb_query_datatype::MAX_BLOB_WIDTH) {
        writer.write(None)
    } else if *len <= 0 {
        writer.write_ref(Some(b""))
    } else {
        writer.write(Some(vec![SPACE; *len as usize]))
    };

    Ok(guard)
}

#[rpn_fn(writer)]
#[inline]
fn space_native(len: &Int, disposition: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if suppress_native_string(disposition)? {
        return Ok(writer.write(None));
    }
    space(len, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn substring_index(
    s: BytesRef,
    delim: BytesRef,
    count: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_index_impl(s, delim, count, SubstringIndexPolicy::Wire, writer)
}

#[rpn_fn(writer)]
#[inline]
fn substring_index_signed_native(
    s: BytesRef,
    delim: BytesRef,
    count: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_index_impl(s, delim, count, SubstringIndexPolicy::NativeSigned, writer)
}

#[rpn_fn(writer)]
#[inline]
fn substring_index_unsigned_native(
    s: BytesRef,
    delim: BytesRef,
    count: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_index_impl(
        s,
        delim,
        count,
        SubstringIndexPolicy::NativeUnsigned,
        writer,
    )
}

enum SubstringIndexPolicy {
    Wire,
    NativeSigned,
    NativeUnsigned,
}

#[inline]
fn substring_index_impl(
    s: BytesRef,
    delim: BytesRef,
    count: &Int,
    policy: SubstringIndexPolicy,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let count = *count;
    if count == 0 || s.is_empty() || delim.is_empty() {
        return Ok(writer.write_ref(Some(b"")));
    }
    match policy {
        SubstringIndexPolicy::NativeUnsigned if count < 0 => {
            return Ok(writer.write_ref(Some(s)));
        }
        SubstringIndexPolicy::NativeSigned if count < 0 => {
            if count == i64::MIN {
                return Ok(writer.write_ref(Some(s)));
            }
            // Native suffixes use the same left-to-right non-overlapping
            // delimiters as prefixes, not the wire reverse scan.
            let (_, remaining) = substring_index_scan(s, delim, i64::MAX, true);
            let matches = i64::MAX - remaining;
            let wanted = count.abs();
            if wanted > matches {
                return Ok(writer.write_ref(Some(s)));
            }
            let (bound, _) = substring_index_scan(s, delim, matches - wanted + 1, true);
            return Ok(writer.write_ref(Some(&s[bound..])));
        }
        _ => {}
    }

    // Preserve the wire count.abs() behavior, including i64::MIN.
    let (bound, remaining_pattern_count) = substring_index_scan(s, delim, count.abs(), count > 0);
    let result = if remaining_pattern_count > 0 {
        s
    } else if count > 0 {
        &s[..bound - delim.len()]
    } else {
        &s[bound + delim.len()..]
    };

    Ok(writer.write_ref(Some(result)))
}

#[inline]
fn substring_index_scan(
    s: BytesRef,
    delim: BytesRef,
    mut remaining_pattern_count: Int,
    from_left: bool,
) -> (usize, Int) {
    let finder = if from_left {
        memmem::find
    } else {
        memmem::rfind
    };
    let mut remaining = s;
    let mut bound = 0;
    while remaining_pattern_count > 0 {
        if let Some(offset) = finder(remaining, delim) {
            if from_left {
                bound += offset + delim.len();
                remaining = &s[bound..];
            } else {
                bound = offset;
                remaining = &s[..bound];
            }
        } else {
            break;
        }
        remaining_pattern_count -= 1;
    }
    (bound, remaining_pattern_count)
}

#[inline]
fn strcmp_result(ordering: Ordering) -> Int {
    match ordering {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

#[rpn_fn]
#[inline]
pub fn strcmp<C: Collator>(left: BytesRef, right: BytesRef) -> Result<Option<i64>> {
    Ok(Some(strcmp_result(C::sort_compare(left, right, false)?)))
}

#[rpn_fn]
#[inline]
fn strcmp_native(left: BytesRef, right: BytesRef, policy: &Int) -> Result<Option<Int>> {
    Ok(Some(strcmp_result(
        decode_native_collation(policy)?.compare(left, right)?,
    )))
}

#[rpn_fn]
#[inline]
pub fn instr(s: BytesRef, substr: BytesRef) -> Result<Option<Int>> {
    Ok(search_bytes(s, substr).map(|i| 1 + i as i64).or(Some(0)))
}

#[rpn_fn]
#[inline]
pub fn instr_utf8(s: BytesRef, substr: BytesRef) -> Result<Option<Int>> {
    let s = String::from_utf8_lossy(s);
    let substr = String::from_utf8_lossy(substr);
    let index = search_bytes(
        s.to_lowercase().as_bytes(),
        substr.to_lowercase().as_bytes(),
    )
    .map(|i| s[..i].chars().count())
    .map(|i| 1 + i as i64)
    .or(Some(0));
    Ok(index)
}

#[rpn_fn]
#[inline]
pub fn find_in_set<C: Collator>(s: BytesRef, str_list: BytesRef) -> Result<Option<Int>> {
    if str_list.is_empty() {
        return Ok(Some(0));
    }
    let found = first_find_in_set_match(
        find_in_set_entries(str_list).map(|entry| {
            Ok(C::sort_compare(entry, s, false)
                .ok()
                .filter(|ordering| *ordering == Ordering::Equal)
                .is_some())
        }),
        false,
    )?;
    Ok(Some(found.map_or(0, |index| index as Int + 1)))
}

#[rpn_fn]
#[inline]
fn find_in_set_native(s: BytesRef, list: BytesRef, policy: &Int) -> Result<Option<Int>> {
    let policy = decode_native_collation(policy)?;
    if list.is_empty() {
        return Ok(Some(0));
    }
    let needle = policy.key(s, KeyOptions::NoPad)?;
    Ok(Some(find_in_set_key_position(
        &needle,
        find_in_set_keys(list, policy),
        false,
    )?))
}

#[rpn_fn]
#[inline]
fn find_in_set_prepared_native(
    s: BytesRef,
    encoded_keys: BytesRef,
    policy: &Int,
) -> Result<Option<Int>> {
    let policy = decode_native_collation(policy)?;
    let keys = PreparedFindInSetKeyIter::new(encoded_keys)?;
    // Even an empty, non-NULL cache probes the current policy once. In
    // particular this must not hide the existing Pinyin panic stub.
    let needle = policy.key(s, KeyOptions::NoPad)?;
    Ok(Some(find_in_set_key_position(&needle, keys, true)?))
}

fn find_in_set_entries(list: BytesRef) -> impl Iterator<Item = BytesRef<'_>> {
    list.split_str(",")
        .take(if list.is_empty() { 0 } else { usize::MAX })
}

fn find_in_set_keys(
    list: BytesRef,
    policy: NativeCollation,
) -> impl Iterator<Item = Result<Bytes>> + '_ {
    find_in_set_entries(list)
        .map(move |entry| policy.key(entry, KeyOptions::NoPad).map_err(Into::into))
}

fn first_find_in_set_match(
    matches: impl Iterator<Item = Result<bool>>,
    scan_all: bool,
) -> Result<Option<usize>> {
    let mut first = None;
    for (index, matches) in matches.enumerate() {
        if matches? && first.is_none() {
            first = Some(index);
            if !scan_all {
                break;
            }
        }
    }
    Ok(first)
}

fn find_in_set_key_position<K: AsRef<[u8]>>(
    needle: &[u8],
    keys: impl Iterator<Item = Result<K>>,
    scan_all: bool,
) -> Result<Int> {
    let found = first_find_in_set_match(keys.map(|key| Ok(key?.as_ref() == needle)), scan_all)?;
    match found {
        None => Ok(0),
        Some(index) => index
            .checked_add(1)
            .and_then(|ordinal| Int::try_from(ordinal).ok())
            .ok_or_else(|| other_err!("FIND_IN_SET ordinal exceeds signed Int")),
    }
}

/// Immutable original build-time keys; no source list or probe policy is kept.
#[derive(Clone, Debug)]
pub struct PreparedFindInSetKeys {
    encoded: Option<Arc<Vec<u8>>>,
}

impl PreparedFindInSetKeys {
    pub fn is_null(&self) -> bool {
        self.encoded.is_none()
    }

    pub(crate) fn into_encoded(self) -> LocalResult<Option<Vec<u8>>> {
        let Some(encoded) = self.encoded else {
            return Ok(None);
        };
        match Arc::try_unwrap(encoded) {
            Ok(encoded) => Ok(Some(encoded)),
            Err(shared) => {
                let mut encoded = Vec::new();
                encoded.try_reserve_exact(shared.len()).map_err(|error| {
                    LocalError::ResourceLimit(format!(
                        "Prepared FIND_IN_SET copy allocation: {}",
                        error
                    ))
                })?;
                encoded.extend_from_slice(shared.as_slice());
                Ok(Some(encoded))
            }
        }
    }
}

/// Build original ordered keys without executing a FIND_IN_SET query.
pub fn prepare_find_in_set_keys(
    list: Option<&[u8]>,
    key_policy: NativeCollation,
    max_encoded_bytes: usize,
) -> LocalResult<PreparedFindInSetKeys> {
    let Some(list) = list else {
        return Ok(PreparedFindInSetKeys { encoded: None });
    };
    let mut encoded = Vec::new();
    reserve_find_in_set_encoding(&mut encoded, 8, max_encoded_bytes)?;
    encoded.extend_from_slice(&0_u64.to_le_bytes());
    let mut count = 0_u64;
    for key in find_in_set_keys(list, key_policy) {
        let key = key.map_err(LocalError::Evaluation)?;
        count = count
            .checked_add(1)
            .filter(|count| *count <= Int::MAX as u64)
            .ok_or_else(|| {
                LocalError::ResourceLimit("Prepared FIND_IN_SET key count overflow".into())
            })?;
        let length = u64::try_from(key.len()).map_err(|_| {
            LocalError::ResourceLimit("Prepared FIND_IN_SET key length overflow".into())
        })?;
        let additional = 8_usize.checked_add(key.len()).ok_or_else(|| {
            LocalError::ResourceLimit("Prepared FIND_IN_SET encoded size overflow".into())
        })?;
        reserve_find_in_set_encoding(&mut encoded, additional, max_encoded_bytes)?;
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.extend_from_slice(&key);
    }
    encoded[..8].copy_from_slice(&count.to_le_bytes());
    Ok(PreparedFindInSetKeys {
        encoded: Some(Arc::new(encoded)),
    })
}

fn reserve_find_in_set_encoding(
    encoded: &mut Vec<u8>,
    additional: usize,
    max_encoded_bytes: usize,
) -> LocalResult<()> {
    let length = encoded.len().checked_add(additional).ok_or_else(|| {
        LocalError::ResourceLimit("Prepared FIND_IN_SET encoded size overflow".into())
    })?;
    if length > max_encoded_bytes {
        return Err(LocalError::ResourceLimit(
            "Prepared FIND_IN_SET encoded byte cap exceeded".into(),
        ));
    }
    encoded.try_reserve_exact(additional).map_err(|error| {
        LocalError::ResourceLimit(format!("Prepared FIND_IN_SET allocation: {}", error))
    })
}

// Closed physical protocol: LE-u64 count, then count repetitions of
// LE-u64 key length and the original key bytes. No trailing bytes are allowed.
struct PreparedFindInSetKeyIter<'a> {
    encoded: &'a [u8],
    remaining: usize,
    offset: usize,
    finished: bool,
}

impl<'a> PreparedFindInSetKeyIter<'a> {
    fn new(encoded: &'a [u8]) -> Result<Self> {
        let mut offset = 0;
        let count = read_find_in_set_word(encoded, &mut offset)?;
        if count > Int::MAX as u64 {
            return Err(other_err!(
                "Prepared FIND_IN_SET key count exceeds signed Int"
            ));
        }
        let remaining = usize::try_from(count)
            .map_err(|_| other_err!("Prepared FIND_IN_SET key count exceeds usize"))?;
        let bytes_left = encoded
            .len()
            .checked_sub(offset)
            .ok_or_else(|| other_err!("Invalid prepared FIND_IN_SET header"))?;
        if remaining > bytes_left / 8 {
            return Err(other_err!("Truncated prepared FIND_IN_SET key headers"));
        }
        Ok(Self {
            encoded,
            remaining,
            offset,
            finished: false,
        })
    }

    fn read_key(&mut self) -> Result<&'a [u8]> {
        let length = read_find_in_set_word(self.encoded, &mut self.offset)?;
        let length = usize::try_from(length)
            .map_err(|_| other_err!("Prepared FIND_IN_SET key length exceeds usize"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| other_err!("Prepared FIND_IN_SET key offset overflow"))?;
        let key = self
            .encoded
            .get(self.offset..end)
            .ok_or_else(|| other_err!("Truncated prepared FIND_IN_SET key"))?;
        self.offset = end;
        Ok(key)
    }
}

impl<'a> Iterator for PreparedFindInSetKeyIter<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.remaining == 0 {
            self.finished = true;
            return (self.offset != self.encoded.len())
                .then(|| Err(other_err!("Trailing bytes in prepared FIND_IN_SET keys")));
        }
        self.remaining -= 1;
        let key = self.read_key();
        if key.is_err() {
            self.finished = true;
        }
        Some(key)
    }
}

fn read_find_in_set_word(encoded: &[u8], offset: &mut usize) -> Result<u64> {
    let end = (*offset)
        .checked_add(8)
        .ok_or_else(|| other_err!("Prepared FIND_IN_SET header offset overflow"))?;
    let bytes = encoded
        .get(*offset..end)
        .ok_or_else(|| other_err!("Truncated prepared FIND_IN_SET header"))?;
    let bytes = <[u8; 8]>::try_from(bytes)
        .map_err(|_| other_err!("Invalid prepared FIND_IN_SET header width"))?;
    *offset = end;
    Ok(u64::from_le_bytes(bytes))
}

pub(crate) fn prepared_find_in_set_keys_match(encoded: Option<&[u8]>) -> bool {
    match encoded {
        None => true,
        Some(encoded) => PreparedFindInSetKeyIter::new(encoded)
            .and_then(|mut keys| keys.try_for_each(|key| key.map(|_| ())))
            .is_ok(),
    }
}

#[rpn_fn(writer)]
#[inline]
pub fn trim_1_arg(arg: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let l_pos = arg.iter().position(|&x| x != SPACE);

    let result = if let Some(i) = l_pos {
        let r_pos = arg.iter().rposition(|&x| x != SPACE);
        &arg[i..=r_pos.unwrap()]
    } else {
        b""
    };

    Ok(writer.write_ref(Some(result)))
}

#[rpn_fn(writer)]
#[inline]
pub fn trim_2_args(arg: BytesRef, pat: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let trimmed = trim(arg, pat, TrimDirection::Both, TrimPolicy::WireIndependent);
    Ok(writer.write_ref(Some(trimmed)))
}

#[rpn_fn(writer)]
#[inline]
pub fn trim_3_args(
    arg: BytesRef,
    pat: BytesRef,
    direction: &i64,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    match TrimDirection::from_i64(*direction) {
        Some(d) => {
            let trimmed = trim(arg, pat, d, TrimPolicy::WireIndependent);
            Ok(writer.write_ref(Some(trimmed)))
        }
        _ => Err(box_err!("invalid direction value: {}", direction)),
    }
}

#[rpn_fn(writer)]
#[inline]
fn trim_both_native(arg: BytesRef, pat: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let trimmed = trim(arg, pat, TrimDirection::Both, TrimPolicy::NativeSequential);
    Ok(writer.write_ref(Some(trimmed)))
}

#[rpn_fn(writer)]
#[inline]
fn trim_leading_native(arg: BytesRef, pat: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let trimmed = trim(
        arg,
        pat,
        TrimDirection::Leading,
        TrimPolicy::NativeSequential,
    );
    Ok(writer.write_ref(Some(trimmed)))
}

#[rpn_fn(writer)]
#[inline]
fn trim_trailing_native(arg: BytesRef, pat: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let trimmed = trim(
        arg,
        pat,
        TrimDirection::Trailing,
        TrimPolicy::NativeSequential,
    );
    Ok(writer.write_ref(Some(trimmed)))
}

#[derive(Clone, Copy)]
enum TrimPolicy {
    WireIndependent,
    NativeSequential,
}

enum TrimDirection {
    Both = 1,
    Leading,
    Trailing,
}

impl TrimDirection {
    fn from_i64(i: i64) -> Option<Self> {
        match i {
            1 => Some(TrimDirection::Both),
            2 => Some(TrimDirection::Leading),
            3 => Some(TrimDirection::Trailing),
            _ => None,
        }
    }
}

#[inline]
fn trim<'a>(
    string: &'a [u8],
    pattern: &[u8],
    direction: TrimDirection,
    policy: TrimPolicy,
) -> &'a [u8] {
    if pattern.is_empty() {
        return string;
    }
    let pat_length = pattern.len();
    let s_length = string.len();

    let left_position = match direction {
        TrimDirection::Trailing => 0,
        _ => string
            .chunks(pat_length)
            .position(|chunk| chunk != pattern)
            .map(|pos| pos * pat_length)
            .unwrap_or(s_length - (s_length % pat_length)),
    };

    let right_source = match policy {
        TrimPolicy::WireIndependent => string,
        TrimPolicy::NativeSequential => &string[left_position..],
    };
    let right_length = right_source.len();
    let right_position = match direction {
        TrimDirection::Leading => right_length,
        _ => right_source
            .rchunks(pat_length)
            .position(|chunk| chunk != pattern)
            .map(|pos| right_length - pos * pat_length)
            .unwrap_or(right_length % pat_length),
    };

    let right_position = match policy {
        TrimPolicy::WireIndependent => right_position.max(left_position),
        TrimPolicy::NativeSequential => left_position + right_position,
    };

    &string[left_position..right_position]
}

#[rpn_fn]
#[inline]
pub fn char_length(bs: BytesRef) -> Result<Option<Int>> {
    Ok(Some(bs.len() as i64))
}

#[rpn_fn]
#[inline]
pub fn char_length_utf8(bs: BytesRef) -> Result<Option<Int>> {
    let s = str::from_utf8(bs)?;
    Ok(Some(s.chars().count() as i64))
}

#[rpn_fn(writer)]
#[inline]
pub fn to_base64(bs: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    if bs.len() > tidb_query_datatype::MAX_BLOB_WIDTH as usize {
        return Ok(writer.write_ref(Some(b"")));
    }

    if let Some(buf) = to_base64_impl(bs) {
        Ok(writer.write(Some(buf)))
    } else {
        Ok(writer.write_ref(Some(b"")))
    }
}

#[rpn_fn(writer)]
#[inline]
fn to_base64_native(bs: BytesRef, disposition: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if suppress_native_string(disposition)? {
        return Ok(writer.write(None));
    }
    // Native encoded-length overflow is silent NULL, not a packet warning.
    if bs.len() as u64 > 6_827_690_988_321_067_803_u64 {
        return Ok(writer.write(None));
    }
    Ok(writer.write(to_base64_impl(bs)))
}

#[inline]
fn to_base64_impl(bs: BytesRef) -> Option<Bytes> {
    let size = encoded_size(bs.len())?;
    let mut buf = vec![0; size];
    let len_without_wrap = base64::encode_config_slice(bs, base64::STANDARD, &mut buf);
    line_wrap(&mut buf, len_without_wrap);
    Some(buf)
}

// similar logic to crate `line-wrap`, since we had call `encoded_size` before,
// there is no need to use checked_xxx math operation like `line-wrap` does.
#[inline]
fn line_wrap(buf: &mut [u8], input_len: usize) {
    let line_len = BASE64_LINE_WRAP_LENGTH;
    if input_len <= line_len {
        return;
    }
    let last_line_len = if input_len.is_multiple_of(line_len) {
        line_len
    } else {
        input_len % line_len
    };
    let lines_with_ending = (input_len - 1) / line_len;
    let line_with_ending_len = line_len + 1;
    let mut old_start = input_len - last_line_len;
    let mut new_start = buf.len() - last_line_len;
    buf.copy_within(old_start..old_start + last_line_len, new_start);
    for _ in 0..lines_with_ending {
        old_start -= line_len;
        new_start -= line_with_ending_len;
        buf.copy_within(old_start..old_start + line_len, new_start);
        buf[new_start + line_len] = BASE64_LINE_WRAP;
    }
}

#[inline]
fn encoded_size(len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    // size_without_wrap = (len + (3 - 1)) / 3 * 4
    // size = size_without_wrap + (size_withou_wrap - 1) / 76
    len.checked_add(BASE64_INPUT_CHUNK_LENGTH - 1)
        .and_then(|r| r.checked_div(BASE64_INPUT_CHUNK_LENGTH))
        .and_then(|r| r.checked_mul(BASE64_ENCODED_CHUNK_LENGTH))
        .and_then(|r| r.checked_add((r - 1) / BASE64_LINE_WRAP_LENGTH))
}

#[rpn_fn(writer)]
#[inline]
pub fn from_base64(bs: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    let input_copy = strip_whitespace(bs, b" \n\t\r\x0b\x0c");
    let will_overflow = input_copy
        .len()
        .checked_mul(BASE64_INPUT_CHUNK_LENGTH)
        .is_none();
    // mysql will return "" when the input is incorrectly padded
    let invalid_padding = !input_copy.len().is_multiple_of(BASE64_ENCODED_CHUNK_LENGTH);
    if will_overflow || invalid_padding {
        Ok(writer.write_ref(Some(b"")))
    } else {
        Ok(writer.write(from_base64_impl(&input_copy)))
    }
}

#[rpn_fn(writer)]
#[inline]
fn from_base64_native(bs: BytesRef, disposition: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if suppress_native_string(disposition)? {
        return Ok(writer.write(None));
    }
    // The packet-aware native entry checks the original length before cleanup.
    if bs.len() > (isize::MAX as usize) / BASE64_INPUT_CHUNK_LENGTH {
        return Ok(writer.write(None));
    }
    Ok(writer.write(from_base64_native_impl(bs)))
}

#[rpn_fn(writer)]
#[inline]
fn from_base64_value_native(bs: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(from_base64_native_impl(bs)))
}

#[inline]
fn from_base64_native_impl(bs: BytesRef) -> Option<Bytes> {
    let input_copy = strip_whitespace(bs, b" \t\r\n");
    if !input_copy.len().is_multiple_of(BASE64_ENCODED_CHUNK_LENGTH) {
        return None;
    }
    from_base64_impl(&input_copy)
}

#[inline]
fn from_base64_impl(bs: BytesRef) -> Option<Bytes> {
    base64::decode_config(bs, base64::STANDARD).ok()
}

#[inline]
fn strip_whitespace(input: &[u8], whitespace: &[u8]) -> Vec<u8> {
    let mut input_copy = Vec::<u8>::with_capacity(input.len());
    input_copy.extend(input.iter().filter(|b| !whitespace.contains(b)));
    input_copy
}

// See https://dev.mysql.com/doc/refman/5.7/en/string-functions.html#function_quote
#[rpn_fn(nullable)]
#[inline]
pub fn quote(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    match input {
        Some(bytes) => {
            let mut result = Vec::with_capacity(bytes.len() * 2 + 2);
            result.push(b'\'');
            for byte in bytes.iter() {
                if *byte == b'\'' || *byte == b'\\' {
                    result.push(b'\\');
                    result.push(*byte)
                } else if *byte == b'\0' {
                    result.push(b'\\');
                    result.push(b'0')
                } else if *byte == 26u8 {
                    result.push(b'\\');
                    result.push(b'Z');
                } else {
                    result.push(*byte)
                }
            }
            result.push(b'\'');
            Ok(Some(result))
        }
        _ => Ok(Some(Vec::from("NULL"))),
    }
}

#[rpn_fn(writer)]
#[inline]
pub fn repeat(input: BytesRef, cnt: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    repeat_impl(input, cnt, writer)
}

#[rpn_fn(writer)]
#[inline]
fn repeat_native(
    input: BytesRef,
    cnt: &Int,
    disposition: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    if suppress_native_string(disposition)? {
        return Ok(writer.write(None));
    }
    repeat_impl(input, cnt, writer)
}

#[inline]
fn repeat_impl(input: BytesRef, cnt: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    if input.is_empty() || *cnt <= 0 {
        return Ok(writer.write_ref(Some(b"")));
    }
    let cnt = if *cnt > i32::MAX.into() {
        i32::MAX.into()
    } else {
        *cnt
    };
    let mut writer = writer.begin();
    for _i in 0..cnt {
        writer.partial_write(input);
    }
    Ok(writer.finish())
}

#[rpn_fn(writer)]
#[inline]
pub fn substring_2_args_utf8(
    input: BytesRef,
    pos: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_wire(input, *pos, Int::MAX, true, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn substring_3_args_utf8(
    input: BytesRef,
    pos: &Int,
    len: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_wire(input, *pos, *len, true, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn substring_2_args(input: BytesRef, pos: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    substring_wire(input, *pos, input.len() as Int, false, writer)
}

#[rpn_fn(writer)]
#[inline]
pub fn substring_3_args(
    input: BytesRef,
    pos: &Int,
    len: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_wire(input, *pos, *len, false, writer)
}

#[rpn_fn(writer)]
#[inline]
fn substring_2_bytes_native(input: BytesRef, pos: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    substring_impl(
        input,
        i128::from(*pos),
        SubstringLength::Tail,
        false,
        SubstringPolicy::Native,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_3_bytes_native(
    input: BytesRef,
    pos: &Int,
    len: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        i128::from(*pos),
        SubstringLength::Native(*len),
        false,
        SubstringPolicy::Native,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_2_utf8_native(input: BytesRef, pos: &Int, writer: BytesWriter) -> Result<BytesGuard> {
    substring_impl(
        input,
        i128::from(*pos),
        SubstringLength::Tail,
        true,
        SubstringPolicy::Native,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_3_utf8_native(
    input: BytesRef,
    pos: &Int,
    len: &Int,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        i128::from(*pos),
        SubstringLength::Native(*len),
        true,
        SubstringPolicy::Native,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_2_bytes_legacy(
    input: BytesRef,
    pos: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        decode_substring_i128(pos)?,
        SubstringLength::Tail,
        false,
        SubstringPolicy::Legacy,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_3_bytes_legacy(
    input: BytesRef,
    pos: BytesRef,
    len: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        decode_substring_i128(pos)?,
        SubstringLength::Legacy(len),
        false,
        SubstringPolicy::Legacy,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_2_utf8_legacy(
    input: BytesRef,
    pos: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        decode_substring_i128(pos)?,
        SubstringLength::Tail,
        true,
        SubstringPolicy::Legacy,
        writer,
    )
}

#[rpn_fn(writer)]
#[inline]
fn substring_3_utf8_legacy(
    input: BytesRef,
    pos: BytesRef,
    len: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    substring_impl(
        input,
        decode_substring_i128(pos)?,
        SubstringLength::Legacy(len),
        true,
        SubstringPolicy::Legacy,
        writer,
    )
}

#[derive(Clone, Copy)]
enum SubstringPolicy {
    Wire,
    Native,
    Legacy,
}

enum SubstringLength<'a> {
    Tail,
    Wire(usize),
    Native(Int),
    Legacy(BytesRef<'a>),
}

enum SubstringView<'a> {
    Bytes(BytesRef<'a>),
    Utf8(Cow<'a, str>),
}

impl SubstringView<'_> {
    fn unit_len(&self) -> usize {
        match self {
            Self::Bytes(input) => input.len(),
            Self::Utf8(input) => input.chars().count(),
        }
    }

    fn write_range(
        self,
        start: usize,
        end: usize,
        policy: SubstringPolicy,
        writer: BytesWriter,
    ) -> Result<BytesGuard> {
        match self {
            Self::Bytes(input) => Ok(writer.write_ref(Some(&input[start..end]))),
            Self::Utf8(input) if matches!(policy, SubstringPolicy::Legacy) => {
                // Only legacy needs a real unit slice to retain unchecked-end
                // bounds behavior; do not add this allocation to wire calls.
                let units: Vec<char> = input.chars().collect();
                Ok(writer.write_from_char_iter(units[start..end].iter().copied()))
            }
            Self::Utf8(input) => {
                Ok(writer.write_from_char_iter(input.chars().skip(start).take(end - start)))
            }
        }
    }
}

#[inline]
fn legacy_substring_view(input: BytesRef, utf8: bool) -> SubstringView<'_> {
    if utf8 {
        SubstringView::Utf8(String::from_utf8_lossy(input))
    } else {
        SubstringView::Bytes(input)
    }
}

#[inline]
fn decode_substring_i128(input: BytesRef) -> Result<i128> {
    let bytes = <[u8; 16]>::try_from(input).map_err(|_| {
        other_err!(
            "Internal substring integer transport requires exactly 16 bytes, received {}",
            input.len()
        )
    })?;
    Ok(i128::from_le_bytes(bytes))
}

#[inline]
fn substring_position(position: i128, policy: SubstringPolicy) -> Option<Int> {
    let position = i64::try_from(position).ok()?;
    if matches!(policy, SubstringPolicy::Legacy) && position == 0 {
        None
    } else {
        Some(position)
    }
}

#[inline]
fn substring_start(position: Int, unit_len: usize, policy: SubstringPolicy) -> Option<usize> {
    if matches!(policy, SubstringPolicy::Wire) {
        let (position, positive) = i64_to_usize(position, position > 0);
        return Some(if positive {
            (position - 1).min(unit_len)
        } else {
            unit_len.checked_sub(position).unwrap_or(unit_len)
        });
    }

    let unit_len = unit_len as Int;
    let start = if position < 0 {
        position + unit_len
    } else {
        position - 1
    };
    if matches!(policy, SubstringPolicy::Legacy) {
        if start < 0 || start >= unit_len {
            None
        } else {
            Some(start as usize)
        }
    } else {
        Some(if !(0..=unit_len).contains(&start) {
            unit_len as usize
        } else {
            start as usize
        })
    }
}

/// Asks only whether a non-NULL legacy substring source/position demands
/// length.
pub fn legacy_substring_needs_len(source: &[u8], position: i128, utf8: bool) -> bool {
    let Some(position) = substring_position(position, SubstringPolicy::Legacy) else {
        return false;
    };
    let view = legacy_substring_view(source, utf8);
    substring_start(position, view.unit_len(), SubstringPolicy::Legacy).is_some()
}

#[inline]
fn substring_wire(
    input: BytesRef,
    pos: Int,
    len: Int,
    utf8: bool,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let (magnitude, _) = i64_to_usize(pos, pos > 0);
    let (len, positive) = i64_to_usize(len, len > 0);
    if magnitude == 0 || len == 0 || !positive {
        return Ok(writer.write_ref(Some(b"")));
    }
    substring_impl(
        input,
        i128::from(pos),
        SubstringLength::Wire(len),
        utf8,
        SubstringPolicy::Wire,
        writer,
    )
}

#[inline]
fn substring_impl(
    input: BytesRef,
    position: i128,
    length: SubstringLength<'_>,
    utf8: bool,
    policy: SubstringPolicy,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let Some(position) = substring_position(position, policy) else {
        return Ok(writer.write(None));
    };
    let view = if matches!(policy, SubstringPolicy::Legacy) {
        legacy_substring_view(input, utf8)
    } else if utf8 {
        SubstringView::Utf8(Cow::Borrowed(str::from_utf8(input)?))
    } else {
        SubstringView::Bytes(input)
    };
    let unit_len = view.unit_len();
    let Some(start) = substring_start(position, unit_len, policy) else {
        return Ok(writer.write_ref(Some(b"")));
    };
    let end = match length {
        SubstringLength::Tail => unit_len,
        SubstringLength::Wire(len) => start.saturating_add(len).min(unit_len),
        SubstringLength::Native(len) if len <= 0 => start,
        SubstringLength::Native(len) => {
            let Some(end) = (start as Int).checked_add(len) else {
                return Ok(writer.write_ref(Some(b"")));
            };
            (end as usize).min(unit_len)
        }
        SubstringLength::Legacy(len) => {
            let Ok(len) = i64::try_from(decode_substring_i128(len)?) else {
                return Ok(writer.write(None));
            };
            if len < 0 {
                return Ok(writer.write_ref(Some(b"")));
            }
            let start = start as Int;
            (start + len).min(unit_len as Int) as usize
        }
    };
    view.write_range(start, end, policy, writer)
}

#[cfg(test)]
mod tests {
    use std::f64;

    use tidb_query_datatype::{
        builder::FieldTypeBuilder,
        codec::mysql::charset::{CHARSET_GB18030, CHARSET_GBK, CHARSET_UTF8MB4},
    };
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_get_utf8_byte_index() {
        let s = "你好世界";

        assert_eq!(&s[..get_utf8_byte_index(s, 0)], "");
        assert_eq!(&s[..get_utf8_byte_index(s, 2)], "你好");
        assert_eq!(&s[..get_utf8_byte_index(s, 4)], "你好世界");
        assert_eq!(&s[..get_utf8_byte_index(s, 9)], "你好世界");
    }

    #[test]
    fn test_bin() {
        let cases = vec![
            (Some(10), Some(b"1010".to_vec())),
            (Some(0), Some(b"0".to_vec())),
            (Some(1), Some(b"1".to_vec())),
            (Some(365), Some(b"101101101".to_vec())),
            (Some(1024), Some(b"10000000000".to_vec())),
            (None, None),
            (
                Some(Int::MAX),
                Some(b"111111111111111111111111111111111111111111111111111111111111111".to_vec()),
            ),
            (
                Some(Int::MIN),
                Some(b"1000000000000000000000000000000000000000000000000000000000000000".to_vec()),
            ),
            (
                Some(-1),
                Some(b"1111111111111111111111111111111111111111111111111111111111111111".to_vec()),
            ),
            (
                Some(-365),
                Some(b"1111111111111111111111111111111111111111111111111111111010010011".to_vec()),
            ),
        ];
        for (arg0, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .evaluate(ScalarFuncSig::Bin)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_unhex() {
        let cases = vec![
            (Some(b"4D7953514C".to_vec()), Some(b"MySQL".to_vec())),
            (Some(b"GG".to_vec()), None),
            (Some(b"41\0".to_vec()), None),
            (Some(b"".to_vec()), Some(b"".to_vec())),
            (Some(b"b".to_vec()), Some(vec![0xb])),
            (Some(b"a1b".to_vec()), Some(vec![0xa, 0x1b])),
            (None, None),
        ];
        for (arg, expect_output) in cases {
            let output: Option<Bytes> = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::UnHex)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_oct_int() {
        let cases = vec![
            (Some(-1), Some(b"1777777777777777777777".to_vec())),
            (Some(0), Some(b"0".to_vec())),
            (Some(1), Some(b"1".to_vec())),
            (Some(8), Some(b"10".to_vec())),
            (Some(12), Some(b"14".to_vec())),
            (Some(20), Some(b"24".to_vec())),
            (Some(100), Some(b"144".to_vec())),
            (Some(1024), Some(b"2000".to_vec())),
            (Some(2048), Some(b"4000".to_vec())),
            (Some(i64::MAX), Some(b"777777777777777777777".to_vec())),
            (Some(i64::MIN), Some(b"1000000000000000000000".to_vec())),
            (None, None),
        ];
        for (arg0, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .evaluate(ScalarFuncSig::OctInt)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_oct_string() {
        let cases = vec![
            (Some(b"".to_vec()), None),
            (Some(b" ".to_vec()), Some(b"0".to_vec())),
            (Some(b"a".to_vec()), Some(b"0".to_vec())),
            (
                Some(b"-1".to_vec()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (Some(b"1.0".to_vec()), Some(b"1".to_vec())),
            (Some(b"9.5".to_vec()), Some(b"11".to_vec())),
            (
                Some(b"-2.7".to_vec()),
                Some(b"1777777777777777777776".to_vec()),
            ),
            (
                Some(b"-1.5".to_vec()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (Some(b"0".to_vec()), Some(b"0".to_vec())),
            (Some(b"1".to_vec()), Some(b"1".to_vec())),
            (Some(b"8".to_vec()), Some(b"10".to_vec())),
            (Some(b"12".to_vec()), Some(b"14".to_vec())),
            (Some(b"12a".to_vec()), Some(b"14".to_vec())),
            (Some(b"20".to_vec()), Some(b"24".to_vec())),
            (Some(b"100".to_vec()), Some(b"144".to_vec())),
            (Some(b"1024".to_vec()), Some(b"2000".to_vec())),
            (Some(b"2048".to_vec()), Some(b"4000".to_vec())),
            (
                Some(format!(" {}", i64::MAX).into_bytes()),
                Some(b"777777777777777777777".to_vec()),
            ),
            (
                Some(format!(" {}", u64::MAX).into_bytes()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (
                Some(format!(" +{}", u64::MAX).into_bytes()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (
                Some(format!(" +{}1", u64::MAX).into_bytes()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (
                Some(format!(" -{}", u64::MAX).into_bytes()),
                Some(b"1".to_vec()),
            ),
            (
                Some(format!("-{}", (1u64 << 63) + 1).into_bytes()),
                Some(b"777777777777777777777".to_vec()),
            ),
            (
                Some(format!(" -{}1", u64::MAX).into_bytes()),
                Some(b"1777777777777777777777".to_vec()),
            ),
            (Some(b" ++1".to_vec()), Some(b"0".to_vec())),
            (Some(b" +1".to_vec()), Some(b"1".to_vec())),
            (None, None),
        ];
        for (arg0, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .evaluate(ScalarFuncSig::OctString)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_length() {
        let test_cases = vec![
            (None, None),
            (Some(""), Some(0i64)),
            (Some("你好"), Some(6i64)),
            (Some("TiKV"), Some(4i64)),
            (Some("あなたのことが好きです"), Some(33i64)),
            (Some("분산 데이터베이스"), Some(25i64)),
            (Some("россия в мире  кубок"), Some(38i64)),
            (Some("قاعدة البيانات"), Some(27i64)),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::Length)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_concat() {
        let cases = vec![
            (
                vec![Some(b"abc".to_vec()), Some(b"defg".to_vec())],
                Some(b"abcdefg".to_vec()),
            ),
            (
                vec![
                    Some("忠犬ハチ公".as_bytes().to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                    Some("قاعدة البيانات".as_bytes().to_vec()),
                    Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
                ],
                Some(
                    "忠犬ハチ公CAFÉ数据库قاعدة البياناتНОЧЬ НА ОКРАИНЕ МОСКВЫ"
                        .as_bytes()
                        .to_vec(),
                ),
            ),
            (
                vec![
                    Some(b"abc".to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                ],
                Some("abcCAFÉ数据库".as_bytes().to_vec()),
            ),
            (
                vec![Some(b"abc".to_vec()), None, Some(b"defg".to_vec())],
                None,
            ),
            (vec![None], None),
        ];
        for (row, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::Concat)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_concat_ws() {
        let cases = vec![
            (
                vec![
                    Some(b",".to_vec()),
                    Some(b"abc".to_vec()),
                    Some(b"defg".to_vec()),
                ],
                Some(b"abc,defg".to_vec()),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    Some("忠犬ハチ公".as_bytes().to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                    Some("قاعدة البيانات".as_bytes().to_vec()),
                    Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
                ],
                Some(
                    "忠犬ハチ公,CAFÉ,数据库,قاعدة البيانات,НОЧЬ НА ОКРАИНЕ МОСКВЫ"
                        .as_bytes()
                        .to_vec(),
                ),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    Some(b"abc".to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                ],
                Some("abc,CAFÉ,数据库".as_bytes().to_vec()),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    Some(b"abc".to_vec()),
                    None,
                    Some(b"defg".to_vec()),
                ],
                Some(b"abc,defg".to_vec()),
            ),
            (
                vec![Some(b",".to_vec()), Some(b"abc".to_vec())],
                Some(b"abc".to_vec()),
            ),
            (
                vec![Some(b",".to_vec()), None, Some(b"abc".to_vec())],
                Some(b"abc".to_vec()),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    Some(b"".to_vec()),
                    Some(b"abc".to_vec()),
                ],
                Some(b",abc".to_vec()),
            ),
            (
                vec![
                    Some("忠犬ハチ公".as_bytes().to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                    Some("قاعدة البيانات".as_bytes().to_vec()),
                ],
                Some(
                    "CAFÉ忠犬ハチ公数据库忠犬ハチ公قاعدة البيانات"
                        .as_bytes()
                        .to_vec(),
                ),
            ),
            (vec![None, Some(b"abc".to_vec())], None),
            (
                vec![Some(b",".to_vec()), None, Some(b"abc".to_vec())],
                Some(b"abc".to_vec()),
            ),
            (
                vec![Some(b",".to_vec()), Some(b"abc".to_vec()), None],
                Some(b"abc".to_vec()),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    Some(b"".to_vec()),
                    Some(b"abc".to_vec()),
                ],
                Some(b",abc".to_vec()),
            ),
            (
                vec![
                    Some("忠犬ハチ公".as_bytes().to_vec()),
                    Some("CAFÉ".as_bytes().to_vec()),
                    Some("数据库".as_bytes().to_vec()),
                    Some("قاعدة البيانات".as_bytes().to_vec()),
                ],
                Some(
                    "CAFÉ忠犬ハチ公数据库忠犬ハチ公قاعدة البيانات"
                        .as_bytes()
                        .to_vec(),
                ),
            ),
            (
                vec![
                    Some(b",".to_vec()),
                    None,
                    Some(b"abc".to_vec()),
                    None,
                    None,
                    Some(b"defg".to_vec()),
                    None,
                ],
                Some(b"abc,defg".to_vec()),
            ),
        ];
        for (row, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::ConcatWs)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_locate_2_args_utf8() {
        let cases = vec![
            // normal cases
            (
                Some(b"bar".to_vec()),
                Some(b"foobarbar".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(4i64),
            ),
            (
                Some(b"xbar".to_vec()),
                Some(b"foobar".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0i64),
            ),
            (
                Some(b"".to_vec()),
                Some(b"foobar".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(1i64),
            ),
            (
                Some(b"foobar".to_vec()),
                Some(b"".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0i64),
            ),
            (
                Some(b"".to_vec()),
                Some(b"".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(1i64),
            ),
            (
                Some("好世".as_bytes().to_vec()),
                Some("你好世界".as_bytes().to_vec()),
                Collation::Utf8Mb4Bin,
                Some(2i64),
            ),
            (
                Some("界面".as_bytes().to_vec()),
                Some("你好世界".as_bytes().to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0i64),
            ),
            (
                Some(b"b".to_vec()),
                Some("中a英b文".as_bytes().to_vec()),
                Collation::Utf8Mb4Bin,
                Some(4i64),
            ),
            (
                Some(b"BaR".to_vec()),
                Some(b"foobArbar".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0i64),
            ),
            (
                Some(b"BaR".to_vec()),
                Some(b"foobArbar".to_vec()),
                Collation::Utf8Mb4GeneralCi,
                Some(4i64),
            ),
            // null cases
            (None, Some(b"".to_vec()), Collation::Utf8Mb4Bin, None),
            (None, Some(b"foobar".to_vec()), Collation::Utf8Mb4Bin, None),
            (Some(b"".to_vec()), None, Collation::Utf8Mb4Bin, None),
            (Some(b"foobar".to_vec()), None, Collation::Utf8Mb4Bin, None),
            (None, None, Collation::Utf8Mb4Bin, None),
            // invalid cases: use invalid value to sign error result
            (
                Some(b"bar".to_vec()),
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(b"foobar".to_vec()),
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(b"foo".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Some(b"foobar".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                None,
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                None,
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                None,
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                None,
                Collation::Utf8Mb4Bin,
                None,
            ),
        ];

        for (substr, s, collation, exp) in cases {
            match RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(substr)
                .push_param(s)
                .evaluate(ScalarFuncSig::Locate2ArgsUtf8)
            {
                Ok(output) => assert_eq!(output, exp),
                Err(_) => assert_eq!(exp.unwrap(), -1i64),
            };
        }
    }

    #[test]
    fn test_locate_3_args_utf8() {
        let cases = vec![
            // normal case
            (
                Some(b"bar".to_vec()),
                Some(b"foobarbar".to_vec()),
                Some(5),
                Collation::Utf8Mb4Bin,
                Some(7),
            ),
            (
                Some(b"xbar".to_vec()),
                Some(b"foobar".to_vec()),
                Some(1),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"".to_vec()),
                Some(b"foobar".to_vec()),
                Some(2),
                Collation::Utf8Mb4Bin,
                Some(2),
            ),
            (
                Some(b"foobar".to_vec()),
                Some(b"".to_vec()),
                Some(1),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"".to_vec()),
                Some(b"".to_vec()),
                Some(2),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"A".to_vec()),
                Some("大A写的A".as_bytes().to_vec()),
                Some(0),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"A".to_vec()),
                Some("大A写的A".as_bytes().to_vec()),
                Some(-1),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"A".to_vec()),
                Some("大A写的A".as_bytes().to_vec()),
                Some(1),
                Collation::Utf8Mb4Bin,
                Some(2),
            ),
            (
                Some(b"A".to_vec()),
                Some("大A写的A".as_bytes().to_vec()),
                Some(2),
                Collation::Utf8Mb4Bin,
                Some(2),
            ),
            (
                Some(b"A".to_vec()),
                Some("大A写的A".as_bytes().to_vec()),
                Some(3),
                Collation::Utf8Mb4Bin,
                Some(5),
            ),
            (
                Some(b"bAr".to_vec()),
                Some(b"foobarBaR".to_vec()),
                Some(5),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"bAr".to_vec()),
                Some(b"foobarBaR".to_vec()),
                Some(5),
                Collation::Utf8Mb4GeneralCi,
                Some(7),
            ),
            (
                Some(b"".to_vec()),
                Some(b"aa".to_vec()),
                Some(2),
                Collation::Utf8Mb4Bin,
                Some(2),
            ),
            (
                Some(b"".to_vec()),
                Some(b"aa".to_vec()),
                Some(3),
                Collation::Utf8Mb4Bin,
                Some(3),
            ),
            (
                Some(b"".to_vec()),
                Some(b"aa".to_vec()),
                Some(4),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            // null case
            (None, None, Some(1), Collation::Utf8Mb4Bin, None),
            (
                Some(b"".to_vec()),
                None,
                Some(1),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                None,
                Some(b"".to_vec()),
                Some(1),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                Some(b"foo".to_vec()),
                None,
                Some(-1),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                None,
                Some(b"bar".to_vec()),
                Some(0),
                Collation::Utf8Mb4Bin,
                None,
            ),
            // invalid cases: use invalid value to sign error result
            (
                Some(b"bar".to_vec()),
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(1),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(b"foobar".to_vec()),
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Some(2),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(b"foo".to_vec()),
                Some(3),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Some(b"foobar".to_vec()),
                Some(4),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Some(5),
                Collation::Utf8Mb4Bin,
                Some(-1i64),
            ),
            (
                None,
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                Some(6),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                None,
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                Some(7),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                Some(vec![0x00, 0x9f, 0x92, 0x96]),
                None,
                Some(8),
                Collation::Utf8Mb4Bin,
                None,
            ),
            (
                Some(b"Hello\xF0\x90\x80World".to_vec()),
                None,
                Some(9),
                Collation::Utf8Mb4Bin,
                None,
            ),
        ];

        for (substr, s, pos, collation, exp) in cases {
            match RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(substr)
                .push_param(s)
                .push_param(pos)
                .evaluate(ScalarFuncSig::Locate3ArgsUtf8)
            {
                Ok(output) => assert_eq!(output, exp),
                Err(_) => assert_eq!(exp.unwrap(), -1i64),
            }
        }
    }

    #[test]
    fn test_bit_length() {
        let test_cases = vec![
            (None, None),
            (Some(""), Some(0i64)),
            (Some("你好"), Some(48i64)),
            (Some("TiKV"), Some(32i64)),
            (Some("あなたのことが好きです"), Some(264i64)),
            (Some("분산 데이터베이스"), Some(200i64)),
            (Some("россия в мире  кубок"), Some(304i64)),
            (Some("قاعدة البيانات"), Some(216i64)),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::BitLength)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_ord() {
        let cases = vec![
            (Some("2"), Collation::Utf8Mb4Bin, Some(50i64)),
            (Some("23"), Collation::Utf8Mb4Bin, Some(50i64)),
            (Some("2.3"), Collation::Utf8Mb4Bin, Some(50i64)),
            (Some(""), Collation::Utf8Mb4Bin, Some(0i64)),
            (Some("你好"), Collation::Utf8Mb4Bin, Some(14990752i64)),
            (Some("にほん"), Collation::Utf8Mb4Bin, Some(14909867i64)),
            (Some("한국"), Collation::Utf8Mb4Bin, Some(15570332i64)),
            (Some("👍"), Collation::Utf8Mb4Bin, Some(4036989325i64)),
            (Some("א"), Collation::Utf8Mb4Bin, Some(55184i64)),
            (Some("2.3"), Collation::Utf8Mb4GeneralCi, Some(50i64)),
            (None, Collation::Utf8Mb4Bin, Some(0)),
            (Some("a"), Collation::Latin1Bin, Some(97i64)),
            (Some("ab"), Collation::Latin1Bin, Some(97i64)),
            (Some("你好"), Collation::Latin1Bin, Some(228i64)),
        ];

        for (arg, collation, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::Ord)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_ascii() {
        let test_cases = vec![
            (None, None),
            (Some(b"1010".to_vec()), Some(49i64)),
            (Some(b"-1".to_vec()), Some(45i64)),
            (Some(b"".to_vec()), Some(0i64)),
            (Some(b"999".to_vec()), Some(57i64)),
            (Some(b"hello".to_vec()), Some(104i64)),
            (Some("Grüße".as_bytes().to_vec()), Some(71i64)),
            (Some("München".as_bytes().to_vec()), Some(77i64)),
            (Some("数据库".as_bytes().to_vec()), Some(230i64)),
            (Some("忠犬ハチ公".as_bytes().to_vec()), Some(229i64)),
            (Some("Αθήνα".as_bytes().to_vec()), Some(206i64)),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::Ascii)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_reverse_utf8() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(b"olleh".to_vec())),
            (Some(b"".to_vec()), Some(b"".to_vec())),
            (
                Some("数据库".as_bytes().to_vec()),
                Some("库据数".as_bytes().to_vec()),
            ),
            (
                Some("忠犬ハチ公".as_bytes().to_vec()),
                Some("公チハ犬忠".as_bytes().to_vec()),
            ),
            (
                Some("あなたのことが好きです".as_bytes().to_vec()),
                Some("すでき好がとこのたなあ".as_bytes().to_vec()),
            ),
            (
                Some("Bayern München".as_bytes().to_vec()),
                Some("nehcnüM nreyaB".as_bytes().to_vec()),
            ),
            (
                Some("Η Αθηνά  ".as_bytes().to_vec()),
                Some("  άνηθΑ Η".as_bytes().to_vec()),
            ),
            (None, None),
        ];

        for (arg, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::ReverseUtf8)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_hex_int_arg() {
        let test_cases = vec![
            (Some(12), Some(b"C".to_vec())),
            (Some(0x12), Some(b"12".to_vec())),
            (Some(0b1100), Some(b"C".to_vec())),
            (Some(0), Some(b"0".to_vec())),
            (Some(-1), Some(b"FFFFFFFFFFFFFFFF".to_vec())),
            (None, None),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::HexIntArg)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_ltrim() {
        let test_cases = vec![
            (None, None),
            (Some("   bar   "), Some("bar   ")),
            (Some("   b   ar   "), Some("b   ar   ")),
            (Some("bar"), Some("bar")),
            (Some("    "), Some("")),
            (Some("\t  bar"), Some("\t  bar")),
            (Some("\r  bar"), Some("\r  bar")),
            (Some("\n  bar"), Some("\n  bar")),
            (Some("  \tbar"), Some("\tbar")),
            (Some(""), Some("")),
            (Some("  你好"), Some("你好")),
            (Some("  你  好"), Some("你  好")),
            (
                Some("  분산 데이터베이스    "),
                Some("분산 데이터베이스    "),
            ),
            (
                Some("   あなたのことが好きです   "),
                Some("あなたのことが好きです   "),
            ),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::LTrim)
                .unwrap();
            assert_eq!(output, expect_output.map(|s| s.as_bytes().to_vec()));
        }
    }

    #[test]
    fn test_rtrim() {
        let test_cases = vec![
            (None, None),
            (Some("   bar   "), Some("   bar")),
            (Some("bar"), Some("bar")),
            (Some("ba  r"), Some("ba  r")),
            (Some("    "), Some("")),
            (Some("  bar\t  "), Some("  bar\t")),
            (Some(" bar   \t"), Some(" bar   \t")),
            (Some("bar   \r"), Some("bar   \r")),
            (Some("bar   \n"), Some("bar   \n")),
            (Some(""), Some("")),
            (Some("  你好  "), Some("  你好")),
            (Some("  你  好  "), Some("  你  好")),
            (Some("  분산 데이터베이스    "), Some("  분산 데이터베이스")),
            (
                Some("   あなたのことが好きです   "),
                Some("   あなたのことが好きです"),
            ),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::RTrim)
                .unwrap();
            assert_eq!(output, expect_output.map(|s| s.as_bytes().to_vec()));
        }
    }

    #[allow(clippy::type_complexity)]
    fn common_lpad_cases() -> Vec<(Option<Bytes>, Option<Int>, Option<Bytes>, Option<Bytes>)> {
        vec![
            (
                Some(b"hi".to_vec()),
                Some(5),
                Some(b"?".to_vec()),
                Some(b"???hi".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(1),
                Some(b"?".to_vec()),
                Some(b"h".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(0),
                Some(b"?".to_vec()),
                Some(b"".to_vec()),
            ),
            (Some(b"hi".to_vec()), Some(-1), Some(b"?".to_vec()), None),
            (
                Some(b"hi".to_vec()),
                Some(1),
                Some(b"".to_vec()),
                Some(b"h".to_vec()),
            ),
            (Some(b"hi".to_vec()), Some(5), Some(b"".to_vec()), None),
            (
                Some(b"hi".to_vec()),
                Some(5),
                Some(b"ab".to_vec()),
                Some(b"abahi".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(6),
                Some(b"ab".to_vec()),
                Some(b"ababhi".to_vec()),
            ),
        ]
    }

    #[test]
    fn test_lpad() {
        let mut cases = vec![
            (
                Some(b"hello".to_vec()),
                Some(0),
                Some(b"h".to_vec()),
                Some(b"".to_vec()),
            ),
            (
                Some(b"hello".to_vec()),
                Some(1),
                Some(b"h".to_vec()),
                Some(b"h".to_vec()),
            ),
            (Some(b"hello".to_vec()), Some(-1), Some(b"h".to_vec()), None),
            (
                Some(b"hello".to_vec()),
                Some(3),
                Some(b"".to_vec()),
                Some(b"hel".to_vec()),
            ),
            (Some(b"hello".to_vec()), Some(8), Some(b"".to_vec()), None),
            (
                Some(b"hello".to_vec()),
                Some(8),
                Some(b"he".to_vec()),
                Some(b"hehhello".to_vec()),
            ),
            (
                Some(b"hello".to_vec()),
                Some(9),
                Some(b"he".to_vec()),
                Some(b"hehehello".to_vec()),
            ),
            (
                Some(b"hello".to_vec()),
                Some(5),
                Some("您好".as_bytes().to_vec()),
                Some(b"hello".to_vec()),
            ),
            (Some(b"hello".to_vec()), Some(6), Some(b"".to_vec()), None),
            (
                Some(b"\x61\x76\x5e".to_vec()),
                Some(2),
                Some(b"\x35".to_vec()),
                Some(b"\x61\x76".to_vec()),
            ),
            (
                Some(b"\x61\x76\x5e".to_vec()),
                Some(5),
                Some(b"\x35".to_vec()),
                Some(b"\x35\x35\x61\x76\x5e".to_vec()),
            ),
            (
                Some(b"hello".to_vec()),
                Some(i64::from(MAX_BLOB_WIDTH) + 1),
                Some(b"he".to_vec()),
                None,
            ),
            (None, Some(-1), Some(b"h".to_vec()), None),
            (None, None, None, None),
        ];
        cases.append(&mut common_lpad_cases());

        for (arg, len, pad, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(len)
                .push_param(pad)
                .evaluate(ScalarFuncSig::Lpad)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_lpad_utf8() {
        let mut cases = vec![
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(3),
                Some("测试".as_bytes().to_vec()),
                Some("a多字".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(4),
                Some("测试".as_bytes().to_vec()),
                Some("a多字节".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(5),
                Some("测试".as_bytes().to_vec()),
                Some("测a多字节".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(6),
                Some("测试".as_bytes().to_vec()),
                Some("测试a多字节".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(7),
                Some("测试".as_bytes().to_vec()),
                Some("测试测a多字节".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(i64::from(MAX_BLOB_WIDTH) / 4 + 1),
                Some("测试".as_bytes().to_vec()),
                None,
            ),
        ];
        cases.append(&mut common_lpad_cases());

        for (arg, len, pad, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(len)
                .push_param(pad)
                .evaluate(ScalarFuncSig::LpadUtf8)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[allow(clippy::type_complexity)]
    fn common_rpad_cases() -> Vec<(Option<Bytes>, Option<Int>, Option<Bytes>, Option<Bytes>)> {
        vec![
            (
                Some(b"hi".to_vec()),
                Some(5),
                Some(b"?".to_vec()),
                Some(b"hi???".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(1),
                Some(b"?".to_vec()),
                Some(b"h".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(0),
                Some(b"?".to_vec()),
                Some(b"".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(1),
                Some(b"".to_vec()),
                Some(b"h".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(5),
                Some(b"ab".to_vec()),
                Some(b"hiaba".to_vec()),
            ),
            (
                Some(b"hi".to_vec()),
                Some(6),
                Some(b"ab".to_vec()),
                Some(b"hiabab".to_vec()),
            ),
            (Some(b"hi".to_vec()), Some(-1), Some(b"?".to_vec()), None),
            (Some(b"hi".to_vec()), Some(5), Some(b"".to_vec()), None),
            (
                Some(b"hi".to_vec()),
                Some(0),
                Some(b"".to_vec()),
                Some(b"".to_vec()),
            ),
        ]
    }

    #[test]
    fn test_rpad() {
        let mut cases = vec![
            (
                Some(b"\x61\x76\x5e".to_vec()),
                Some(5),
                Some(b"\x35".to_vec()),
                Some(b"\x61\x76\x5e\x35\x35".to_vec()),
            ),
            (
                Some(b"\x61\x76\x5e".to_vec()),
                Some(2),
                Some(b"\x35".to_vec()),
                Some(b"\x61\x76".to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(13),
                Some("测试".as_bytes().to_vec()),
                Some("a多字节测".as_bytes().to_vec()),
            ),
            (
                Some(b"abc".to_vec()),
                Some(i64::from(MAX_BLOB_WIDTH) + 1),
                Some(b"aa".to_vec()),
                None,
            ),
        ];
        cases.append(&mut common_rpad_cases());

        for (arg, len, pad, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(len)
                .push_param(pad)
                .evaluate(ScalarFuncSig::Rpad)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_rpad_utf8() {
        let mut cases = vec![
            (
                Some("多字节a".as_bytes().to_vec()),
                Some(3),
                Some("测试".as_bytes().to_vec()),
                Some("多字节".as_bytes().to_vec()),
            ),
            (
                Some("多字节a".as_bytes().to_vec()),
                Some(4),
                Some("测试".as_bytes().to_vec()),
                Some("多字节a".as_bytes().to_vec()),
            ),
            (
                Some("多字节a".as_bytes().to_vec()),
                Some(5),
                Some("测试".as_bytes().to_vec()),
                Some("多字节a测".as_bytes().to_vec()),
            ),
            (
                Some("多字节a".as_bytes().to_vec()),
                Some(6),
                Some("测试".as_bytes().to_vec()),
                Some("多字节a测试".as_bytes().to_vec()),
            ),
            (
                Some("多字节a".as_bytes().to_vec()),
                Some(7),
                Some("测试".as_bytes().to_vec()),
                Some("多字节a测试测".as_bytes().to_vec()),
            ),
            (
                Some("a多字节".as_bytes().to_vec()),
                Some(i64::from(MAX_BLOB_WIDTH) / 4 + 1),
                Some("测试".as_bytes().to_vec()),
                None,
            ),
        ];
        cases.append(&mut common_rpad_cases());

        for (arg, len, pad, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(len)
                .push_param(pad)
                .evaluate(ScalarFuncSig::RpadUtf8)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_replace() {
        let cases = vec![
            (None, None, None, None),
            (None, Some(b"a".to_vec()), Some(b"b".to_vec()), None),
            (Some(b"a".to_vec()), None, Some(b"b".to_vec()), None),
            (Some(b"a".to_vec()), Some(b"b".to_vec()), None, None),
            (
                Some(b"www.mysql.com".to_vec()),
                Some(b"mysql".to_vec()),
                Some(b"pingcap".to_vec()),
                Some(b"www.pingcap.com".to_vec()),
            ),
            (
                Some(b"www.mysql.com".to_vec()),
                Some(b"w".to_vec()),
                Some(b"1".to_vec()),
                Some(b"111.mysql.com".to_vec()),
            ),
            (
                Some(b"1234".to_vec()),
                Some(b"2".to_vec()),
                Some(b"55".to_vec()),
                Some(b"15534".to_vec()),
            ),
            (
                Some(b"".to_vec()),
                Some(b"a".to_vec()),
                Some(b"b".to_vec()),
                Some(b"".to_vec()),
            ),
            (
                Some(b"abc".to_vec()),
                Some(b"".to_vec()),
                Some(b"d".to_vec()),
                Some(b"abc".to_vec()),
            ),
            (
                Some(b"aaa".to_vec()),
                Some(b"a".to_vec()),
                Some(b"".to_vec()),
                Some(b"".to_vec()),
            ),
            (
                Some(b"aaa".to_vec()),
                Some(b"A".to_vec()),
                Some(b"".to_vec()),
                Some(b"aaa".to_vec()),
            ),
            (
                Some("新年快乐".as_bytes().to_vec()),
                Some("年".as_bytes().to_vec()),
                Some("春".as_bytes().to_vec()),
                Some("新春快乐".as_bytes().to_vec()),
            ),
            (
                Some("心心相印".as_bytes().to_vec()),
                Some("心".as_bytes().to_vec()),
                Some("❤️".as_bytes().to_vec()),
                Some("❤️❤️相印".as_bytes().to_vec()),
            ),
            (
                Some(b"Hello \xF0\x90\x80World".to_vec()),
                Some(b"World".to_vec()),
                Some(b"123".to_vec()),
                Some(b"Hello \xF0\x90\x80123".to_vec()),
            ),
        ];

        for (s, from_str, to_str, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(s)
                .push_param(from_str)
                .push_param(to_str)
                .evaluate(ScalarFuncSig::Replace)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_left() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(0), Some(b"".to_vec())),
            (Some(b"hello".to_vec()), Some(1), Some(b"h".to_vec())),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(2),
                Some(vec![230u8, 149u8]),
            ),
            (
                Some("忠犬ハチ公".as_bytes().to_vec()),
                Some(3),
                Some(vec![229u8, 191u8, 160u8]),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(100),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(-1),
                Some(b"".to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("数据库".as_bytes().to_vec()),
            ),
            (None, Some(-1), None),
            (Some(b"hello".to_vec()), None, None),
            (None, None, None),
        ];

        for (lhs, rhs, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::Left)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_left_utf8() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(0i64), Some(b"".to_vec())),
            (Some(b"hello".to_vec()), Some(1i64), Some(b"h".to_vec())),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(2i64),
                Some("数据".as_bytes().to_vec()),
            ),
            (
                Some("忠犬ハチ公".as_bytes().to_vec()),
                Some(3i64),
                Some("忠犬ハ".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(100i64),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(-1i64),
                Some(b"".to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("数据库".as_bytes().to_vec()),
            ),
            (None, Some(-1), None),
            (Some(b"hello".to_vec()), None, None),
            (None, None, None),
        ];

        for (lhs, rhs, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::LeftUtf8)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_right() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(0), Some(b"".to_vec())),
            (Some(b"hello".to_vec()), Some(1), Some(b"o".to_vec())),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(2),
                Some(vec![186u8, 147u8]),
            ),
            (
                Some("忠犬ハチ公".as_bytes().to_vec()),
                Some(3),
                Some(vec![229u8, 133u8, 172u8]),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(100),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(-1),
                Some(b"".to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("数据库".as_bytes().to_vec()),
            ),
            (None, Some(-1), None),
            (Some(b"hello".to_vec()), None, None),
            (None, None, None),
        ];

        for (lhs, rhs, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::Right)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_insert() {
        let cases = vec![
            ("hello, world!", 1, 0, "asd", "asdhello, world!"),
            ("hello, world!", 0, -1, "asd", "hello, world!"),
            ("hello, world!", 0, 0, "asd", "hello, world!"),
            ("hello, world!", -1, 0, "asd", "hello, world!"),
            ("hello, world!", 1, -1, "asd", "asd"),
            ("hello, world!", 1, 1, "asd", "asdello, world!"),
            ("hello, world!", 1, 3, "asd", "asdlo, world!"),
            ("hello, world!", 2, 2, "asd", "hasdlo, world!"),
            ("hello", 5, 2, "asd", "hellasd"),
            ("hello", 5, 200, "asd", "hellasd"),
            ("hello", 2, 200, "asd", "hasd"),
            ("hello", -1, 200, "asd", "hello"),
            ("hello", 0, 200, "asd", "hello"),
        ];
        for (s1, i1, i2, s2, exp) in cases {
            let s1 = Some(s1.as_bytes().to_vec());
            let i1 = Some(i1);
            let i2 = Some(i2);
            let s2 = Some(s2.as_bytes().to_vec());
            let exp = Some(exp.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .push_param(s1)
                .push_param(i1)
                .push_param(i2)
                .push_param(s2)
                .evaluate(ScalarFuncSig::Insert)
                .unwrap();
            assert_eq!(got, exp);
        }

        let null_cases = vec![
            (None, Some(-1), Some(200), Some(b"asd".to_vec())),
            (
                Some(b"hello".to_vec()),
                None,
                Some(200),
                Some(b"asd".to_vec()),
            ),
            (
                Some(b"hello".to_vec()),
                Some(-1),
                None,
                Some(b"asd".to_vec()),
            ),
            (Some(b"hello".to_vec()), Some(-1), Some(200), None),
        ];
        for (s1, i1, i2, s2) in null_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(s1)
                .push_param(i1)
                .push_param(i2)
                .push_param(s2)
                .evaluate::<Bytes>(ScalarFuncSig::Insert)
                .unwrap();
            assert_eq!(got, None);
        }
    }

    #[test]
    fn test_insert_utf8() {
        let cases = vec![
            (
                "hello, world!".as_bytes(),
                1,
                0,
                "asd".as_bytes(),
                "asdhello, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                0,
                -1,
                "asd".as_bytes(),
                "hello, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                0,
                0,
                "asd".as_bytes(),
                "hello, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                -1,
                0,
                "asd".as_bytes(),
                "hello, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                1,
                -1,
                "asd".as_bytes(),
                "asd".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                1,
                1,
                "asd".as_bytes(),
                "asdello, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                1,
                3,
                "asd".as_bytes(),
                "asdlo, world!".as_bytes(),
            ),
            (
                "hello, world!".as_bytes(),
                2,
                2,
                "asd".as_bytes(),
                "hasdlo, world!".as_bytes(),
            ),
            (
                "hello".as_bytes(),
                5,
                2,
                "asd".as_bytes(),
                "hellasd".as_bytes(),
            ),
            (
                "hello".as_bytes(),
                5,
                200,
                "asd".as_bytes(),
                "hellasd".as_bytes(),
            ),
            (
                "hello".as_bytes(),
                2,
                200,
                "asd".as_bytes(),
                "hasd".as_bytes(),
            ),
            (
                "hello".as_bytes(),
                -1,
                200,
                "asd".as_bytes(),
                "hello".as_bytes(),
            ),
            (
                "hello".as_bytes(),
                0,
                200,
                "asd".as_bytes(),
                "hello".as_bytes(),
            ),
        ];
        for (s1, i1, i2, s2, exp) in cases {
            let s1 = Some(s1.as_bytes().to_vec());
            let i1 = Some(i1);
            let i2 = Some(i2);
            let s2 = Some(s2.as_bytes().to_vec());
            let exp = Some(exp.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .push_param(s1)
                .push_param(i1)
                .push_param(i2)
                .push_param(s2)
                .evaluate(ScalarFuncSig::InsertUtf8)
                .unwrap();
            assert_eq!(got, exp);
        }

        let null_cases = vec![
            (None, Some(-1), Some(200), Some("asd".as_bytes())),
            (
                Some("hello".as_bytes()),
                None,
                Some(200),
                Some("asd".as_bytes()),
            ),
            (
                Some("hello".as_bytes()),
                Some(-1),
                None,
                Some("asd".as_bytes()),
            ),
            (Some("hello".as_bytes()), Some(-1), Some(200), None),
        ];
        for (s1, i1, i2, s2) in null_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(s1)
                .push_param(i1)
                .push_param(i2)
                .push_param(s2)
                .evaluate::<Bytes>(ScalarFuncSig::InsertUtf8)
                .unwrap();
            assert_eq!(got, None);
        }
    }

    #[test]
    fn test_right_utf8() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(0), Some(b"".to_vec())),
            (Some(b"hello".to_vec()), Some(1), Some(b"o".to_vec())),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(2),
                Some("据库".as_bytes().to_vec()),
            ),
            (
                Some("忠犬ハチ公".as_bytes().to_vec()),
                Some(3),
                Some("ハチ公".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(100),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(-1),
                Some(b"".to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("数据库".as_bytes().to_vec()),
            ),
            (None, Some(-1), None),
            (Some(b"hello".to_vec()), None, None),
            (None, None, None),
        ];

        for (lhs, rhs, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::RightUtf8)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_upper_utf8() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(b"HELLO".to_vec())),
            (Some(b"123".to_vec()), Some(b"123".to_vec())),
            (
                Some("café".as_bytes().to_vec()),
                Some("CAFÉ".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("ночь на окраине москвы".as_bytes().to_vec()),
                Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
            ),
            (
                Some("قاعدة البيانات".as_bytes().to_vec()),
                Some("قاعدة البيانات".as_bytes().to_vec()),
            ),
            (
                Some("ßßåı".as_bytes().to_vec()),
                Some("ßßÅI".as_bytes().to_vec()),
            ),
            (None, None),
        ];

        for (arg, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    arg.clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .charset(CHARSET_UTF8MB4)
                        .build(),
                )
                .evaluate(ScalarFuncSig::UpperUtf8)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_upper() {
        // Test binary string case
        let cases = vec![
            (Some(b"hello".to_vec()), Some(b"hello".to_vec())),
            (Some(b"123".to_vec()), Some(b"123".to_vec())),
            (
                Some("café".as_bytes().to_vec()),
                Some("café".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("ночь на окраине москвы".as_bytes().to_vec()),
                Some("ночь на окраине москвы".as_bytes().to_vec()),
            ),
            (
                Some("قاعدة البيانات".as_bytes().to_vec()),
                Some("قاعدة البيانات".as_bytes().to_vec()),
            ),
            (None, None),
        ];

        for (arg, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    arg.clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .collation(Collation::Binary)
                        .build(),
                )
                .evaluate(ScalarFuncSig::Upper)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_gbk_lower_upper() {
        // Test GBK string case
        let cases = vec![
            (
                ScalarFuncSig::LowerUtf8,
                "àáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅪⅫ".as_bytes().to_vec(),
                "àáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅪⅫ".as_bytes().to_vec(),
            ),
            (
                ScalarFuncSig::UpperUtf8,
                "àáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅪⅫ".as_bytes().to_vec(),
                "àáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅪⅫ".as_bytes().to_vec(),
            ),
            (
                ScalarFuncSig::LowerUtf8,
                "İİIIÅI".as_bytes().to_vec(),
                "iiiiåi".as_bytes().to_vec(),
            ),
            (
                ScalarFuncSig::UpperUtf8,
                "ßßåı".as_bytes().to_vec(),
                "ßßÅI".as_bytes().to_vec(),
            ),
        ];
        for (s, input, output) in cases {
            let result = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    Some(input).clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .charset(CHARSET_GBK)
                        .build(),
                )
                .evaluate(s)
                .unwrap();
            assert_eq!(result, Some(output),);
        }
    }

    #[test]
    fn test_gb18030_lower_upper() {
        // Test GB18030 string case
        let raw_upper_lower: Vec<(&str, &str, &str)> = vec![
            ("µ", "µ", "μ"),       // "B5" "B5" "3BC"
            ("ǅǈǋ", "ǅǈǋ", "ǆǉǌ"), // "1C5" "1C8" "1CB"
            ("ǄǇǊ", "ǄǇǊ", "ǆǉǌ"), // "1C4" "1C7" "1CA"
            ("ǆǉǌ", "ǄǇǊ", "ǆǉǌ"), // "1C6" "1C9" "1CC"
            (
                "ɥɪჾᏸᏻᏽᵽꮕàáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅪⅫ",
                "ɥɪჾᏸᏻᏽᵽꮕÀÁÈÉÊÌÍÒÓÙÚÜĀĒĚĪŃŇŌŪǍǏǑǓǕǗǙǛⅪⅫ",
                "ɥɪჾᏸᏻᏽᵽꮕàáèéêìíòóùúüāēěīńňōūǎǐǒǔǖǘǚǜⅺⅻ",
            ),
            ("ǲɜɡ", "ǲɜɡ", "ǳɜɡ"), // "1F2" "25C" "261"
            (
                "𐒰𐓘𐲀𐳀𑢠𖹀𞤀", // "104B0 104D8 10C80 10CC0 118A0 16E40 1E900"
                "𐒰𐓘𐲀𐳀𑢠𖹀𞤀",
                "𐒰𐓘𐲀𐳀𑢠𖹀𞤀",
            ),
            (
                "ẛι", // 1E9B 1FBE
                "ẛι", "ṡι",
            ),
        ];

        for (i, test_case) in raw_upper_lower.iter().enumerate() {
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    Some((test_case.0).as_bytes().to_vec()).clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .charset(CHARSET_GB18030)
                        .build(),
                )
                .evaluate(ScalarFuncSig::UpperUtf8)
                .unwrap();
            assert_eq!(
                output,
                Some((test_case.1).as_bytes().to_vec()),
                "error in upper cases #{} ({})",
                i + 1,
                (test_case.0)
            );

            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    Some((test_case.0).as_bytes().to_vec()).clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .charset(CHARSET_GB18030)
                        .build(),
                )
                .evaluate(ScalarFuncSig::LowerUtf8)
                .unwrap();
            assert_eq!(
                output,
                Some((test_case.2).as_bytes().to_vec()),
                "error in lower cases #{} ({})",
                i + 1,
                (test_case.0)
            );
        }
    }

    #[test]
    fn test_lower() {
        // Test binary string case
        let cases = vec![
            (Some(b"hello".to_vec()), Some(b"hello".to_vec())),
            (
                Some("CAFÉ".as_bytes().to_vec()),
                Some("CAFÉ".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
                Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
            ),
            (
                Some("قاعدة البيانات".as_bytes().to_vec()),
                Some("قاعدة البيانات".as_bytes().to_vec()),
            ),
            (
                Some("İİIIÅI".as_bytes().to_vec()),
                Some("İİIIÅI".as_bytes().to_vec()),
            ),
            (None, None),
        ];

        for (arg, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    arg.clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .collation(Collation::Binary)
                        .build(),
                )
                .evaluate(ScalarFuncSig::Lower)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_lower_utf8() {
        // Test non-binary string case
        let cases = vec![
            (
                Some("HELLO".as_bytes().to_vec()),
                Some("hello".as_bytes().to_vec()),
            ),
            (
                Some("123".as_bytes().to_vec()),
                Some("123".as_bytes().to_vec()),
            ),
            (
                Some("CAFÉ".as_bytes().to_vec()),
                Some("café".as_bytes().to_vec()),
            ),
            (
                Some("数据库".as_bytes().to_vec()),
                Some("数据库".as_bytes().to_vec()),
            ),
            (
                Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()),
                Some("ночь на окраине москвы".as_bytes().to_vec()),
            ),
            (
                Some("قاعدة البيانات".as_bytes().to_vec()),
                Some("قاعدة البيانات".as_bytes().to_vec()),
            ),
            (
                Some("İİIIÅI".as_bytes().to_vec()),
                Some("iiiiåi".as_bytes().to_vec()),
            ),
            (None, None),
        ];

        for (arg, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(
                    arg.clone(),
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::VarString)
                        .charset(CHARSET_UTF8MB4)
                        .build(),
                )
                .evaluate(ScalarFuncSig::LowerUtf8)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_hex_str_arg() {
        let test_cases = vec![
            (Some(b"abc".to_vec()), Some(b"616263".to_vec())),
            (
                Some("你好".as_bytes().to_vec()),
                Some(b"E4BDA0E5A5BD".to_vec()),
            ),
            (None, None),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::HexStrArg)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_locate_2_args() {
        let test_cases = vec![
            (None, None, None),
            (None, Some("abc"), None),
            (Some("abc"), None, None),
            (Some(""), Some("foobArbar"), Some(1)),
            (Some(""), Some(""), Some(1)),
            (Some("xxx"), Some(""), Some(0)),
            (Some("BaR"), Some("foobArbar"), Some(0)),
            (Some("bar"), Some("foobArbar"), Some(7)),
            (
                Some("好世"),
                Some("你好世界"),
                Some(1 + "你好世界".find("好世").unwrap() as i64),
            ),
        ];

        for (substr, s, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(substr.map(|v| v.as_bytes().to_vec()))
                .push_param(s.map(|v| v.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::Locate2Args)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_reverse() {
        let cases = vec![
            (Some(b"hello".to_vec()), Some(b"olleh".to_vec())),
            (Some(b"".to_vec()), Some(b"".to_vec())),
            (
                Some("中国".as_bytes().to_vec()),
                Some(vec![0o275u8, 0o233u8, 0o345u8, 0o255u8, 0o270u8, 0o344u8]),
            ),
            (None, None),
        ];

        for (arg, expect_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::Reverse)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_locate_3_args() {
        let cases = vec![
            (None, None, None, None),
            (None, Some(""), Some(1), None),
            (Some(""), None, None, None),
            (Some(""), Some("foobArbar"), Some(1), Some(1)),
            (Some(""), Some("foobArbar"), Some(0), Some(0)),
            (Some(""), Some("foobArbar"), Some(2), Some(2)),
            (Some(""), Some("foobArbar"), Some(9), Some(9)),
            (Some(""), Some("foobArbar"), Some(10), Some(10)),
            (Some(""), Some("foobArbar"), Some(11), Some(0)),
            (Some(""), Some(""), Some(1), Some(1)),
            (Some("BaR"), Some("foobArbar"), Some(3), Some(0)),
            (Some("bar"), Some("foobArbar"), Some(1), Some(7)),
            (
                Some("好世"),
                Some("你好世界"),
                Some(1),
                Some(1 + "你好世界".find("好世").unwrap() as i64),
            ),
        ];

        for (substr, s, pos, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(substr.map(|v| v.as_bytes().to_vec()))
                .push_param(s.map(|v| v.as_bytes().to_vec()))
                .push_param(pos)
                .evaluate(ScalarFuncSig::Locate3Args)
                .unwrap();
            assert_eq!(output, exp)
        }
    }

    #[test]
    fn test_field_int() {
        let test_cases = vec![
            (vec![Some(1), Some(-2), Some(3)], Some(0)),
            (vec![Some(-1), Some(2), Some(-1), Some(2)], Some(2)),
            (
                vec![Some(i64::MAX), Some(0), Some(i64::MIN), Some(i64::MAX)],
                Some(3),
            ),
            (vec![None, Some(0), Some(0)], Some(0)),
            (vec![None, None, Some(0)], Some(0)),
            (vec![Some(100)], Some(0)),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate(ScalarFuncSig::FieldInt)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_field_real() {
        let test_cases = vec![
            (vec![Some(1.0), Some(-2.0), Some(9.0)], Some(0)),
            (vec![Some(-1.0), Some(2.0), Some(-1.0), Some(2.0)], Some(2)),
            (
                vec![Some(f64::MAX), Some(0.0), Some(f64::MIN), Some(f64::MAX)],
                Some(3),
            ),
            (vec![None, Some(1.0), Some(1.0)], Some(0)),
            (vec![None, None, Some(0.0)], Some(0)),
            (vec![Some(10.0)], Some(0)),
        ];

        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate(ScalarFuncSig::FieldReal)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_field_string() {
        let test_cases = vec![
            (
                vec![
                    Some(b"foo".to_vec()),
                    Some(b"foo".to_vec()),
                    Some(b"bar".to_vec()),
                    Some(b"baz".to_vec()),
                ],
                Some(1),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![
                    Some(b"foo".to_vec()),
                    Some(b"bar".to_vec()),
                    Some(b"baz".to_vec()),
                    Some(b"hello".to_vec()),
                ],
                Some(0),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![
                    Some(b"hello".to_vec()),
                    Some(b"world".to_vec()),
                    Some(b"world".to_vec()),
                    Some(b"hello".to_vec()),
                ],
                Some(3),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![
                    Some(b"Hello".to_vec()),
                    Some(b"Hola".to_vec()),
                    Some("Cześć".as_bytes().to_vec()),
                    Some("你好".as_bytes().to_vec()),
                    Some("Здравствуйте".as_bytes().to_vec()),
                    Some(b"Hello World!".to_vec()),
                    Some(b"Hello".to_vec()),
                ],
                Some(6),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![
                    None,
                    Some(b"DataBase".to_vec()),
                    Some(b"Hello World!".to_vec()),
                ],
                Some(0),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![None, None, Some(b"Hello World!".to_vec())],
                Some(0),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![Some(b"Hello World!".to_vec())],
                Some(0),
                Collation::Utf8Mb4Bin,
            ),
            (
                vec![
                    Some(b"a".to_vec()),
                    Some(b"A".to_vec()),
                    Some(b"a".to_vec()),
                ],
                Some(1),
                Collation::Utf8Mb4GeneralCi,
            ),
        ];

        for (args, expect_output, collation) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::Long)
                        .collation(collation),
                )
                .evaluate(ScalarFuncSig::FieldString)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_space() {
        let test_cases = vec![
            (None, None),
            (Some(0), Some(b"".to_vec())),
            (Some(0), Some(b"".to_vec())),
            (Some(3), Some(b"   ".to_vec())),
            (Some(-1), Some(b"".to_vec())),
            (Some(i64::MAX), None),
            (
                Some(i64::from(tidb_query_datatype::MAX_BLOB_WIDTH) + 1),
                None,
            ),
            (
                Some(i64::from(tidb_query_datatype::MAX_BLOB_WIDTH)),
                Some(vec![
                    super::SPACE;
                    tidb_query_datatype::MAX_BLOB_WIDTH as usize
                ]),
            ),
        ];

        for (len, exp) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(len)
                .evaluate(ScalarFuncSig::Space)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_make_set() {
        let test_cases: Vec<(Vec<ScalarValue>, _)> = vec![
            (
                vec![
                    Some(0b110).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"Hello World!".to_vec()),
            ),
            (
                vec![
                    Some(0b100).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"".to_vec()),
            ),
            (
                vec![
                    Some(0b0).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"".to_vec()),
            ),
            (
                vec![
                    Some(0b1).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"DataBase".to_vec()),
            ),
            (
                vec![
                    None::<Int>.into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (vec![None::<Int>.into(), None::<Bytes>.into()], None),
            (
                vec![
                    Some(0b1).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"".to_vec()),
            ),
            (
                vec![
                    Some(0b11).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"Hello World!".to_vec()),
            ),
            (
                vec![
                    Some(0b0).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"".to_vec()),
            ),
            (
                vec![
                    Some(0xffffffff).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                    None::<Bytes>.into(),
                ],
                Some(b"Hello World!".to_vec()),
            ),
            (
                vec![
                    Some(0b10).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"Hello World!".to_vec()),
            ),
            (
                vec![
                    Some(0xffffffff).into(),
                    Some(b"a".to_vec()).into(),
                    Some(b"b".to_vec()).into(),
                    Some(b"c".to_vec()).into(),
                ],
                Some(b"a,b,c".to_vec()),
            ),
            (
                vec![
                    Some(0xfffffffe).into(),
                    Some(b"a".to_vec()).into(),
                    Some(b"b".to_vec()).into(),
                    Some(b"c".to_vec()).into(),
                ],
                Some(b"b,c".to_vec()),
            ),
            (
                vec![
                    Some(0xfffffffd).into(),
                    Some(b"a".to_vec()).into(),
                    Some(b"b".to_vec()).into(),
                    Some(b"c".to_vec()).into(),
                ],
                Some(b"a,c".to_vec()),
            ),
        ];
        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate(ScalarFuncSig::MakeSet)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_substring_index() {
        let test_cases = vec![
            (None, None, None, None),
            (Some(vec![]), None, None, None),
            (Some(vec![]), Some(vec![]), Some(1i64), Some(vec![])),
            (Some(vec![0x1]), Some(vec![]), Some(1), Some(vec![])),
            (Some(vec![0x1]), Some(vec![]), Some(-1), Some(vec![])),
            (Some(vec![]), Some(vec![0x1]), Some(1), Some(vec![])),
            (Some(vec![]), Some(vec![0x1]), Some(-1), Some(vec![])),
            (
                Some(b"abc".to_vec()),
                Some(b"ab".to_vec()),
                Some(0),
                Some(vec![]),
            ),
            (
                Some(b"aaaaaaaa".to_vec()),
                Some(b"aa".to_vec()),
                Some(1),
                Some(vec![]),
            ),
            (
                Some(b"bbbbbbbb".to_vec()),
                Some(b"bb".to_vec()),
                Some(-1),
                Some(vec![]),
            ),
            (
                Some(b"cccccccc".to_vec()),
                Some(b"cc".to_vec()),
                Some(2),
                Some(b"cc".to_vec()),
            ),
            (
                Some(b"dddddddd".to_vec()),
                Some(b"dd".to_vec()),
                Some(-2),
                Some(b"dd".to_vec()),
            ),
            (
                Some(b"eeeeeeee".to_vec()),
                Some(b"ee".to_vec()),
                Some(5),
                Some(b"eeeeeeee".to_vec()),
            ),
            (
                Some(b"ffffffff".to_vec()),
                Some(b"ff".to_vec()),
                Some(-5),
                Some(b"ffffffff".to_vec()),
            ),
            (
                Some(b"gggggggg".to_vec()),
                Some(b"gg".to_vec()),
                Some(6),
                Some(b"gggggggg".to_vec()),
            ),
            (
                Some(b"hhhhhhhh".to_vec()),
                Some(b"hh".to_vec()),
                Some(-6),
                Some(b"hhhhhhhh".to_vec()),
            ),
            (
                Some(b"iiiii".to_vec()),
                Some(b"ii".to_vec()),
                Some(1),
                Some(vec![]),
            ),
            (
                Some(b"jjjjj".to_vec()),
                Some(b"jj".to_vec()),
                Some(-1),
                Some(vec![]),
            ),
            (
                Some(b"kkkkk".to_vec()),
                Some(b"kk".to_vec()),
                Some(3),
                Some(b"kkkkk".to_vec()),
            ),
            (
                Some(b"lllll".to_vec()),
                Some(b"ll".to_vec()),
                Some(-3),
                Some(b"lllll".to_vec()),
            ),
            (
                Some(b"www.mysql.com".to_vec()),
                Some(b".".to_vec()),
                Some(2),
                Some(b"www.mysql".to_vec()),
            ),
            (
                Some(b"www.mysql.com".to_vec()),
                Some(b".".to_vec()),
                Some(-2),
                Some(b"mysql.com".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(1),
                Some(vec![]),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(-1),
                Some(b"c".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(2),
                Some(b"abc".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(-2),
                Some(b"cabc".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(5),
                Some(b"abcabcabc".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"ab".to_vec()),
                Some(-5),
                Some(b"abcabcabc".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"d".to_vec()),
                Some(1),
                Some(b"abcabcabc".to_vec()),
            ),
            (
                Some(b"abcabcabc".to_vec()),
                Some(b"d".to_vec()),
                Some(-1),
                Some(b"abcabcabc".to_vec()),
            ),
        ];
        for (s, delim, count, exp) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(s)
                .push_param(delim)
                .push_param(count)
                .evaluate(ScalarFuncSig::SubstringIndex)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_elt() {
        let test_cases: Vec<(Vec<ScalarValue>, _)> = vec![
            (
                vec![
                    Some(1).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"DataBase".to_vec()),
            ),
            (
                vec![
                    Some(2).into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some(b"Hello World!".to_vec()),
            ),
            (
                vec![
                    None::<Int>.into(),
                    Some(b"DataBase".to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (vec![None::<Int>.into(), None::<Bytes>.into()], None),
            (
                vec![
                    Some(1).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(3).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(0).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(-1).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(9223372036854775807).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(4).into(),
                    None::<Bytes>.into(),
                    Some(b"Hello".to_vec()).into(),
                    Some(b"Hola".to_vec()).into(),
                    Some("Cześć".as_bytes().to_vec()).into(),
                    Some("你好".as_bytes().to_vec()).into(),
                    Some("Здравствуйте".as_bytes().to_vec()).into(),
                    Some(b"Hello World!".to_vec()).into(),
                ],
                Some("Cześć".as_bytes().to_vec()),
            ),
            (
                vec![
                    Some(1).into(),
                    tidb_query_datatype::codec::data_type::ScalarValue::Enum(Some(Enum::new(
                        "aaa".as_bytes().to_vec(),
                        1u64,
                    ))),
                ],
                Some("aaa".as_bytes().to_vec()),
            ),
            (
                vec![
                    tidb_query_datatype::codec::data_type::ScalarValue::Enum(Some(Enum::new(
                        "aaa".as_bytes().to_vec(),
                        1u64,
                    ))),
                    Some(b"bbb".to_vec()).into(),
                ],
                Some("bbb".as_bytes().to_vec()),
            ),
        ];
        for (args, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate(ScalarFuncSig::Elt)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_strcmp() {
        let test_cases = vec![
            (
                Some(b"123".to_vec()),
                Some(b"123".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"123".to_vec()),
                Some(b"1".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(1),
            ),
            (
                Some(b"1".to_vec()),
                Some(b"123".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1),
            ),
            (
                Some(b"123".to_vec()),
                Some(b"45".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1),
            ),
            (
                Some("你好".as_bytes().to_vec()),
                Some(b"hello".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(1),
            ),
            (
                Some(b"".to_vec()),
                Some(b"123".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(-1),
            ),
            (
                Some(b"123".to_vec()),
                Some(b"".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(1),
            ),
            (
                Some(b"".to_vec()),
                Some(b"".to_vec()),
                Collation::Utf8Mb4Bin,
                Some(0),
            ),
            (
                Some(b"ABC".to_vec()),
                Some(b"abc".to_vec()),
                Collation::Utf8Mb4GeneralCi,
                Some(0),
            ),
            (None, Some(b"123".to_vec()), Collation::Utf8Mb4Bin, None),
            (Some(b"123".to_vec()), None, Collation::Utf8Mb4Bin, None),
            (Some(b"".to_vec()), None, Collation::Utf8Mb4Bin, None),
            (None, Some(b"".to_vec()), Collation::Utf8Mb4Bin, None),
        ];

        for (left, right, collation, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(left)
                .push_param(right)
                .evaluate(ScalarFuncSig::Strcmp)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_instr() {
        let cases = vec![
            (Some(b"a".to_vec()), Some(b"abcdefg".to_vec()), Some(1)),
            (Some(b"0".to_vec()), Some(b"abcdefg".to_vec()), Some(0)),
            (Some(b"c".to_vec()), Some(b"abcdefg".to_vec()), Some(3)),
            (Some(b"F".to_vec()), Some(b"abcdefg".to_vec()), Some(0)),
            (Some(b"cd".to_vec()), Some(b"abcdefg".to_vec()), Some(3)),
            (Some(b" ".to_vec()), Some(b"abcdefg".to_vec()), Some(0)),
            (Some(b"".to_vec()), Some(b"".to_vec()), Some(1)),
            (Some(b" ".to_vec()), Some(b"".to_vec()), Some(0)),
            (Some(b"".to_vec()), Some(b" ".to_vec()), Some(1)),
            (Some(b"eFg".to_vec()), Some(b"abcdefg".to_vec()), Some(0)),
            (Some(b"deF".to_vec()), Some(b"abcdefg".to_vec()), Some(0)),
            (
                Some("字节".as_bytes().to_vec()),
                Some("a多字节".as_bytes().to_vec()),
                Some(5),
            ),
            (
                Some(b"a".to_vec()),
                Some("a多字节".as_bytes().to_vec()),
                Some(1),
            ),
            (Some(b"bar".to_vec()), Some(b"foobarbar".to_vec()), Some(4)),
            (Some(b"bAr".to_vec()), Some(b"foobarbar".to_vec()), Some(0)),
            (
                Some("好世".as_bytes().to_vec()),
                Some("你好世界".as_bytes().to_vec()),
                Some(4),
            ),
            (None, Some(b"".to_vec()), None),
            (None, Some(b"foobar".to_vec()), None),
            (Some(b"".to_vec()), None, None),
            (Some(b"bar".to_vec()), None, None),
            (None, None, None),
        ];

        for (substr, s, exp) in cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(s)
                .push_param(substr)
                .evaluate::<Int>(ScalarFuncSig::Instr)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_instr_utf8() {
        let cases: Vec<(&str, &str, i64)> = vec![
            ("a", "abcdefg", 1),
            ("0", "abcdefg", 0),
            ("c", "abcdefg", 3),
            ("F", "abcdefg", 6),
            ("cd", "abcdefg", 3),
            (" ", "abcdefg", 0),
            ("", "", 1),
            (" ", " ", 1),
            (" ", "", 0),
            ("", " ", 1),
            ("eFg", "abcdefg", 5),
            ("def", "abcdefg", 4),
            ("字节", "a多字节", 3),
            ("a", "a多字节", 1),
            ("bar", "foobarbar", 4),
            ("xbar", "foobarbar", 0),
            ("好世", "你好世界", 2),
        ];

        for (substr, s, exp) in cases {
            let substr = Some(substr.as_bytes().to_vec());
            let s = Some(s.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .push_param(s)
                .push_param(substr)
                .evaluate::<Int>(ScalarFuncSig::InstrUtf8)
                .unwrap();
            assert_eq!(got, Some(exp))
        }

        let null_cases = vec![
            (None, Some(b"".to_vec()), None),
            (None, Some(b"foobar".to_vec()), None),
            (Some(b"".to_vec()), None, None),
            (Some(b"bar".to_vec()), None, None),
            (None, None, None),
        ];
        for (substr, s, exp) in null_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(s)
                .push_param(substr)
                .evaluate::<Int>(ScalarFuncSig::InstrUtf8)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_find_in_set() {
        let cases = vec![
            ("foo", "foo,bar", Collation::Utf8Mb4Bin, 1),
            ("foo", "foobar,bar", Collation::Utf8Mb4Bin, 0),
            (" foo ", "foo, foo ", Collation::Utf8Mb4Bin, 2),
            ("", "foo,bar,", Collation::Utf8Mb4Bin, 3),
            ("", "", Collation::Utf8Mb4Bin, 0),
            ("a,b", "a,b,c", Collation::Utf8Mb4Bin, 0),
            ("测试", "中文,测试,英文", Collation::Utf8Mb4Bin, 2),
            ("foo", "A,FOO,BAR", Collation::Utf8Mb4GeneralCi, 2),
            ("b", "A,B,C", Collation::Utf8Mb4GeneralCi, 2),
        ];

        for (s, str_list, collation, exp) in cases {
            let s = Some(s.as_bytes().to_vec());
            let str_list = Some(str_list.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(s)
                .push_param(str_list)
                .evaluate::<Int>(ScalarFuncSig::FindInSet)
                .unwrap();
            assert_eq!(got, Some(exp))
        }

        let null_cases = vec![
            (Some(b"foo".to_vec()), None, Collation::Utf8Mb4Bin, None),
            (None, Some(b"bar".to_vec()), Collation::Utf8Mb4Bin, None),
            (None, None, Collation::Utf8Mb4Bin, None),
        ];
        for (s, str_list, collation, exp) in null_cases {
            let got = RpnFnScalarEvaluator::new()
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::LongLong)
                        .collation(collation)
                        .build(),
                )
                .push_param(s)
                .push_param(str_list)
                .evaluate::<Int>(ScalarFuncSig::FindInSet)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_trim_1_arg() {
        let test_cases = vec![
            (None, None),
            (Some("   bar   "), Some("bar")),
            (Some("   b   "), Some("b")),
            (Some("   b   ar   "), Some("b   ar")),
            (Some("bar"), Some("bar")),
            (Some("    "), Some("")),
            (Some("  \tbar\t   "), Some("\tbar\t")),
            (Some("  \rbar\r   "), Some("\rbar\r")),
            (Some("  \nbar\n   "), Some("\nbar\n")),
            (Some(""), Some("")),
            (Some("  你好"), Some("你好")),
            (Some("  你  好  "), Some("你  好")),
            (Some("  분산 데이터베이스    "), Some("분산 데이터베이스")),
            (
                Some("   あなたのことが好きです   "),
                Some("あなたのことが好きです"),
            ),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::Trim1Arg)
                .unwrap();
            assert_eq!(output, expect_output.map(|s| s.as_bytes().to_vec()));
        }

        let invalid_utf8_output = RpnFnScalarEvaluator::new()
            .push_param(Some(b"  \xF0 Hello \x90 World \x80 ".to_vec()))
            .evaluate(ScalarFuncSig::Trim1Arg)
            .unwrap();
        assert_eq!(
            invalid_utf8_output,
            Some(b"\xF0 Hello \x90 World \x80".to_vec())
        );
    }

    #[test]
    fn test_trim_2_args() {
        let test_cases = vec![
            (None, None, None),
            (Some("x"), None, None),
            (None, Some("x"), None),
            (Some("xxx"), Some("x"), Some("")),
            (Some("xxxbarxxx"), Some("x"), Some("bar")),
            (Some("xxxbarxxx"), Some("xx"), Some("xbarx")),
            (Some("xyxybarxyxy"), Some("xy"), Some("bar")),
            (Some("xyxybarxyxyx"), Some("xy"), Some("barxyxyx")),
            (Some("xyxy"), Some("xy"), Some("")),
            (Some("xyxyx"), Some("xy"), Some("x")),
            (Some("   bar   "), Some(""), Some("   bar   ")),
            (Some(""), Some("x"), Some("")),
            (Some("张三和张三"), Some("张三"), Some("和")),
            (Some("xxxbarxxxxx"), Some("x"), Some("bar")),
        ];

        for (arg, pat, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.map(|s| s.as_bytes().to_vec()))
                .push_param(pat.map(|s| s.as_bytes().to_vec()))
                .evaluate(ScalarFuncSig::Trim2Args)
                .unwrap();
            assert_eq!(output, expect.map(|s| s.as_bytes().to_vec()));
        }

        let invalid_utf8_cases = vec![
            (
                Some(b"  \xF0 Hello \x90 World \x80 ".to_vec()),
                Some(b" ".to_vec()),
                Some(b"\xF0 Hello \x90 World \x80".to_vec()),
            ),
            (
                Some(b"xy\xF0 Hello \x90 World \x80 ".to_vec()),
                Some(b"xy".to_vec()),
                Some(b"\xF0 Hello \x90 World \x80 ".to_vec()),
            ),
            (
                Some(b"\xF0 Hello \x90 World \x80 ".to_vec()),
                Some(b"\xF0".to_vec()),
                Some(b" Hello \x90 World \x80 ".to_vec()),
            ),
        ];

        for (arg, pat, expected) in invalid_utf8_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(pat)
                .evaluate(ScalarFuncSig::Trim2Args)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_trim_3_args() {
        let tests = vec![
            (
                Some("xxxbarxxx"),
                Some("x"),
                Some(TrimDirection::Leading as i64),
                Some("barxxx"),
            ),
            (
                Some("barxxyz"),
                Some("xyz"),
                Some(TrimDirection::Trailing as i64),
                Some("barx"),
            ),
            (
                Some("xxxbarxxx"),
                Some("x"),
                Some(TrimDirection::Both as i64),
                Some("bar"),
            ),
        ];
        for (arg, pat, direction, exp) in tests {
            let arg = arg.map(|s| s.as_bytes().to_vec());
            let pat = pat.map(|s| s.as_bytes().to_vec());
            let exp = exp.map(|s| s.as_bytes().to_vec());

            let got = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(pat)
                .push_param(direction)
                .evaluate(ScalarFuncSig::Trim3Args)
                .unwrap();
            assert_eq!(got, exp);
        }

        let invalid_tests = vec![
            (
                None,
                Some(b"x".to_vec()),
                Some(TrimDirection::Leading as i64),
                None as Option<Bytes>,
            ),
            (
                Some(b"bar".to_vec()),
                None,
                Some(TrimDirection::Leading as i64),
                None as Option<Bytes>,
            ),
        ];
        for (arg, pat, direction, exp) in invalid_tests {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(pat)
                .push_param(direction)
                .evaluate(ScalarFuncSig::Trim3Args)
                .unwrap();
            assert_eq!(got, exp);
        }

        // test invalid direction value
        let args = (Some(b"bar".to_vec()), Some(b"b".to_vec()), Some(0_i64));
        let got: Result<Option<Bytes>> = RpnFnScalarEvaluator::new()
            .push_param(args.0)
            .push_param(args.1)
            .push_param(args.2)
            .evaluate(ScalarFuncSig::Trim3Args);
        got.unwrap_err();

        let invalid_utf8_cases = vec![
            (
                Some(b"  \xF0 Hello \x90 World \x80 ".to_vec()),
                Some(b" ".to_vec()),
                Some(TrimDirection::Leading as i64),
                Some(b"\xF0 Hello \x90 World \x80 ".to_vec()),
            ),
            (
                Some(b"  \xF0 Hello \x90 World \x80 ".to_vec()),
                Some(b" ".to_vec()),
                Some(TrimDirection::Trailing as i64),
                Some(b"  \xF0 Hello \x90 World \x80".to_vec()),
            ),
        ];
        for (arg, pat, direction, expected) in invalid_utf8_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .push_param(pat)
                .push_param(direction)
                .evaluate(ScalarFuncSig::Trim3Args)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_char_length() {
        let cases = vec![
            (Some(b"HELLO".to_vec()), Some(5)),
            (Some(b"123".to_vec()), Some(3)),
            (Some(b"".to_vec()), Some(0)),
            (Some("CAFÉ".as_bytes().to_vec()), Some(5)),
            (Some("数据库".as_bytes().to_vec()), Some(9)),
            (Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()), Some(41)),
            (Some("قاعدة البيانات".as_bytes().to_vec()), Some(27)),
            (Some(vec![0x00, 0x9f, 0x92, 0x96]), Some(4)), // invalid utf8
            (Some(b"Hello\xF0\x90\x80World".to_vec()), Some(13)), // invalid utf8
            (None, None),
        ];

        for (arg, expected_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::CharLength)
                .unwrap();
            assert_eq!(output, expected_output);
        }
    }

    #[test]
    fn test_char_length_utf8() {
        let cases = vec![
            (Some(b"HELLO".to_vec()), Some(5)),
            (Some(b"123".to_vec()), Some(3)),
            (Some(b"".to_vec()), Some(0)),
            (Some("CAFÉ".as_bytes().to_vec()), Some(4)),
            (Some("数据库".as_bytes().to_vec()), Some(3)),
            (Some("НОЧЬ НА ОКРАИНЕ МОСКВЫ".as_bytes().to_vec()), Some(22)),
            (Some("قاعدة البيانات".as_bytes().to_vec()), Some(14)),
            (None, None),
        ];

        for (arg, expected_output) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::CharLengthUtf8)
                .unwrap();
            assert_eq!(output, expected_output);
        }

        let invalid_utf8_cases: Vec<Vec<u8>> = vec![
            vec![0xc0],
            vec![0xf6],
            vec![0x00, 0x9f],
            vec![0xc3, 0x28],
            vec![0xe2, 0x28, 0xa1],
            vec![0xe2, 0x82, 0x28],
            vec![0xf0, 0x28, 0x8c, 0xbc],
            vec![0xf0, 0x90, 0x28, 0xbc],
            vec![0xf0, 0x28, 0x8c, 0x28],
            vec![0xf8, 0xa1, 0xa1, 0xa1, 0xa0],
            vec![0xfc, 0xa1, 0xa1, 0xa1, 0xa1, 0xa0],
        ];

        for arg in invalid_utf8_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<i64>(ScalarFuncSig::CharLengthUtf8);
            output.unwrap_err();
        }
    }

    #[test]
    fn test_to_base64() {
        let cases = vec![
            ("", ""),
            ("abc", "YWJj"),
            ("ab c", "YWIgYw=="),
            ("1", "MQ=="),
            ("1.1", "MS4x"),
            ("ab\nc", "YWIKYw=="),
            ("ab\tc", "YWIJYw=="),
            ("qwerty123456", "cXdlcnR5MTIzNDU2"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0\nNTY3ODkrLw==",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0\nNTY3ODkrL0FCQ0RFRkdISUpLTE1OT1BRUlNUVVZXWFlaYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4\neXowMTIzNDU2Nzg5Ky9BQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWmFiY2RlZmdoaWprbG1ub3Bx\ncnN0dXZ3eHl6MDEyMzQ1Njc4OSsv",
            ),
            (
                "ABCD  EFGHI\nJKLMNOPQRSTUVWXY\tZabcdefghijklmnopqrstuv  wxyz012\r3456789+/",
                "QUJDRCAgRUZHSEkKSktMTU5PUFFSU1RVVldYWQlaYWJjZGVmZ2hpamtsbW5vcHFyc3R1diAgd3h5\nejAxMg0zNDU2Nzg5Ky8=",
            ),
            (
                "000000000000000000000000000000000000000000000000000000000",
                "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw",
            ),
            (
                "0000000000000000000000000000000000000000000000000000000000",
                "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw\nMA==",
            ),
            (
                "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw\nMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw",
            ),
        ];

        for (arg, expected) in cases {
            let param = Some(arg.to_string().into_bytes());
            let expected_output = Some(expected.to_string().into_bytes());
            let output = RpnFnScalarEvaluator::new()
                .push_param(param)
                .evaluate::<Bytes>(ScalarFuncSig::ToBase64)
                .unwrap();
            assert_eq!(output, expected_output);
        }
    }

    #[test]
    fn test_from_base64() {
        let tests = vec![
            ("", ""),
            ("YWJj", "abc"),
            ("YWIgYw==", "ab c"),
            ("YWIKYw==", "ab\nc"),
            ("YWIJYw==", "ab\tc"),
            ("cXdlcnR5MTIzNDU2", "qwerty123456"),
            (
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0\nNTY3ODkrL0FCQ0RFRkdISUpLTE1OT1BRUlNUVVZXWFlaYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4\neXowMTIzNDU2Nzg5Ky9BQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWmFiY2RlZmdoaWprbG1ub3Bx\ncnN0dXZ3eHl6MDEyMzQ1Njc4OSsv",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            ),
            (
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODkrLw==",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            ),
            (
                "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODkrLw==",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            ),
            (
                "QUJDREVGR0hJSkt\tMTU5PUFFSU1RVVld\nYWVphYmNkZ\rWZnaGlqa2xt   bm9wcXJzdHV2d3h5ejAxMjM0NTY3ODkrLw==",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            ),
        ];
        for (arg, expected) in tests {
            let param = Some(arg.to_string().into_bytes());
            let expected_output = Some(expected.to_string().into_bytes());
            let output = RpnFnScalarEvaluator::new()
                .push_param(param)
                .evaluate::<Bytes>(ScalarFuncSig::FromBase64)
                .unwrap();
            assert_eq!(output, expected_output);
        }

        let invalid_base64_output = RpnFnScalarEvaluator::new()
            .push_param(Some(b"src".to_vec()))
            .evaluate(ScalarFuncSig::FromBase64)
            .unwrap();
        assert_eq!(invalid_base64_output, Some(b"".to_vec()));
    }

    #[test]
    fn test_quote() {
        let cases: Vec<(&str, &str)> = vec![
            (r"Don\'t!", r"'Don\\\'t!'"),
            (r"Don't", r"'Don\'t'"),
            (r"\'", r"'\\\''"),
            (r#"\""#, r#"'\\"'"#),
            (r"萌萌哒(๑•ᴗ•๑)😊", r"'萌萌哒(๑•ᴗ•๑)😊'"),
            (r"㍿㌍㍑㌫", r"'㍿㌍㍑㌫'"),
            (str::from_utf8(&[26, 0]).unwrap(), r"'\Z\0'"),
        ];

        for (input, expect) in cases {
            let input = Bytes::from(input);
            let expect_vec = Bytes::from(expect);
            let got = quote(Some(&input)).unwrap();
            assert_eq!(got, Some(expect_vec))
        }

        // check for null
        let got = quote(None).unwrap();
        assert_eq!(got, Some(Bytes::from("NULL")))
    }

    #[test]
    fn test_repeat() {
        let cases = vec![
            ("hello, world!", -1, ""),
            ("hello, world!", 0, ""),
            ("hello, world!", 1, "hello, world!"),
            (
                "hello, world!",
                3,
                "hello, world!hello, world!hello, world!",
            ),
            ("你好世界", 3, "你好世界你好世界你好世界"),
            ("こんにちは", 2, "こんにちはこんにちは"),
            ("\x2f\x35", 5, "\x2f\x35\x2f\x35\x2f\x35\x2f\x35\x2f\x35"),
        ];

        for (input, cnt, expect) in cases {
            let input = Bytes::from(input);
            let expected_output = Bytes::from(expect);
            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(input))
                .push_param(Some(cnt))
                .evaluate::<Bytes>(ScalarFuncSig::Repeat)
                .unwrap();
            assert_eq!(output, Some(expected_output));
        }

        let null_string: Option<Bytes> = None;
        let null_cnt: Option<Int> = None;

        // test NULL case
        let output = RpnFnScalarEvaluator::new()
            .push_param(null_string.clone())
            .push_param(Some(42))
            .evaluate::<Bytes>(ScalarFuncSig::Repeat)
            .unwrap();
        assert_eq!(output, None);

        let output = RpnFnScalarEvaluator::new()
            .push_param(Some(b"hi".to_vec()))
            .push_param(null_cnt)
            .evaluate::<Bytes>(ScalarFuncSig::Repeat)
            .unwrap();
        assert_eq!(output, None);

        let output = RpnFnScalarEvaluator::new()
            .push_param(null_string)
            .push_param(null_cnt)
            .evaluate::<Bytes>(ScalarFuncSig::Repeat)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_validate_target_len_for_pad() {
        let cases = vec![
            // target_len, input_len, size_of_type, pad_empty, result
            (0, 10, 1, false, Some(0)),
            (-1, 10, 1, false, None),
            (12, 10, 1, true, None),
            (i64::from(super::MAX_BLOB_WIDTH) + 1, 10, 1, false, None),
            (i64::from(super::MAX_BLOB_WIDTH) / 4 + 1, 10, 4, false, None),
            (12, 10, 1, false, Some(12)),
        ];
        for case in cases {
            let got = super::validate_target_len_for_pad(false, case.0, case.1, case.2, case.3);
            assert_eq!(got, case.4);
        }

        let unsigned_cases = vec![
            (u64::MAX, 10, 1, false, None),
            (u64::MAX, 10, 4, false, None),
            (u64::MAX, 10, 1, true, None),
            (u64::MAX, 10, 4, true, None),
            (12u64, 10, 4, false, Some(12)),
        ];
        for case in unsigned_cases {
            let got =
                super::validate_target_len_for_pad(true, case.0 as i64, case.1, case.2, case.3);
            assert_eq!(got, case.4);
        }
    }

    #[test]
    fn test_substring_2_args() {
        let cases = vec![
            (
                Some("中文a测试bb".as_bytes().to_vec()),
                Some(1),
                Some("中文a测试bb".as_bytes().to_vec()),
            ),
            (
                Some("中文a测试".as_bytes().to_vec()),
                Some(-3),
                Some("试".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(-1),
                Some("\x35".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(2),
                Some("\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
            ),
            (
                Some("Quadratically".as_bytes().to_vec()),
                Some(5),
                Some("ratically".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(1),
                Some("Sakila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-3),
                Some("ila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(0),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(100),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-100),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(i64::MIN),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(-1),
                Some("".as_bytes().to_vec()),
            ),
        ];

        for (str, pos, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(str)
                .push_param(pos)
                .evaluate(ScalarFuncSig::Substring2Args)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_substring_3_args() {
        let cases = vec![
            (
                Some("Quadratically".as_bytes().to_vec()),
                Some(5),
                Some(6),
                Some("ratica".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-5),
                Some(3),
                Some("aki".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(0),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(-1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(100),
                Some("akila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(100),
                Some(5),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-100),
                Some(5),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(4),
                Some(3),
                Some("文".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(4),
                Some(4),
                Some("文a".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(-3),
                Some(3),
                Some("试".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(1),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(2),
                Some(2),
                Some("\x76\x5e".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(4),
                Some(100),
                Some("\x38\x2f\x35".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(-1),
                Some(2),
                Some("\x35".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(-2),
                Some(2),
                Some("\x2f\x35".as_bytes().to_vec()),
            ),
        ];

        for (str, pos, len, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(str)
                .push_param(pos)
                .push_param(len)
                .evaluate(ScalarFuncSig::Substring3Args)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_substring_2_args_utf8() {
        let cases = vec![
            (
                Some("中文a测试bb".as_bytes().to_vec()),
                Some(1),
                Some("中文a测试bb".as_bytes().to_vec()),
            ),
            (
                Some("中文a测试".as_bytes().to_vec()),
                Some(-3),
                Some("a测试".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(-1),
                Some("\x35".as_bytes().to_vec()),
            ),
            (
                Some("\x61\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
                Some(2),
                Some("\x76\x5e\x38\x2f\x35".as_bytes().to_vec()),
            ),
            (
                Some("Quadratically".as_bytes().to_vec()),
                Some(5),
                Some("ratically".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(1),
                Some("Sakila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-3),
                Some("ila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(0),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(100),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-100),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(i64::MAX),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(i64::MIN),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(-1),
                Some("".as_bytes().to_vec()),
            ),
        ];

        for (str, pos, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(str)
                .push_param(pos)
                .evaluate(ScalarFuncSig::Substring2ArgsUtf8)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_substring_3_args_utf8() {
        let cases = vec![
            (
                Some("Quadratically".as_bytes().to_vec()),
                Some(5),
                Some(6),
                Some("ratica".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-5),
                Some(3),
                Some("aki".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(0),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(-1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(2),
                Some(100),
                Some("akila".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(100),
                Some(5),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("Sakila".as_bytes().to_vec()),
                Some(-100),
                Some(5),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(4),
                Some(3),
                Some("测a试".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(4),
                Some(100),
                Some("测a试".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(100),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(100),
                Some(i64::MIN),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(100),
                Some(i64::MAX),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(i64::MIN),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(i64::MAX),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(4),
                Some(4),
                Some("测a试".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(-3),
                Some(3),
                Some("测a试".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(0),
                Some(3),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("中文a测a试".as_bytes().to_vec()),
                Some(1),
                Some(0),
                Some("".as_bytes().to_vec()),
            ),
            (
                Some("".as_bytes().to_vec()),
                Some(1),
                Some(1),
                Some("".as_bytes().to_vec()),
            ),
        ];

        for (str, pos, len, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(str)
                .push_param(pos)
                .push_param(len)
                .evaluate(ScalarFuncSig::Substring3ArgsUtf8)
                .unwrap();
            assert_eq!(output, exp);
        }
    }
}
