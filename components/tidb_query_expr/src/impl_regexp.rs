// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use std::{borrow::Cow, sync::Arc};

use regex::{Regex, RegexBuilder};
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::codec::{Error, collation::Collator, data_type::*};

use crate::{
    NativeCachedRegexp, NativeRegexpError, NativeRegexpInvocation,
    NativeReplacementPart as ReplaceInstruction, RegexpPolicyError, RegexpReplacementEncoding,
    local::{LocalError, NativeRegexpCallMetadata, NativeRegexpKind},
    native_regexp::{known_pattern_cache_bytes, known_replacement_cache_bytes},
    regexp_instr_match, regexp_match_flags, regexp_replace_matches, regexp_replacement_parts,
    regexp_substr_match, regexp_trim_at,
    types::function::CallBuild,
};

const PATTERN_IDX: usize = 1;
const LIKE_MATCH_IDX: usize = 2;
const SUBSTR_MATCH_IDX: usize = 4;
const INSTR_MATCH_IDX: usize = 5;
const REPLACE_MATCH_IDX: usize = 5;

fn invalid_pos_error(pos: i64, count: usize) -> Error {
    Error::regexp_error(format!("Invalid pos: {} in regexp, count: {}", pos, count))
}

fn wire_regexp_policy_error(error: RegexpPolicyError) -> tidb_query_common::Error {
    match error {
        RegexpPolicyError::InvalidMatchType(m) => {
            Error::regexp_error(format!("Invalid match type: {} in regexp", m)).into()
        }
        RegexpPolicyError::InvalidPosition { pos, count } => invalid_pos_error(pos, count).into(),
        RegexpPolicyError::InvalidSubstitution(num) => {
            Error::regexp_error(format!("Substitution number is out of range: {}", num)).into()
        }
        RegexpPolicyError::InvalidReplacementUtf8(error) => error.into(),
    }
}

fn get_match_type<C: Collator>(match_type: &[u8]) -> Result<String> {
    let match_type = std::str::from_utf8(match_type)?;
    let flag_set =
        regexp_match_flags(match_type, C::IS_CASE_INSENSITIVE).map_err(wire_regexp_policy_error)?;

    // Preserve the wire HashSet-to-inline-flags representation, including the
    // pattern text used in the original Regex Debug compilation diagnostic.
    let mut flag = String::new();
    for m in flag_set {
        flag.push(m);
    }

    Ok(flag)
}

fn build_regexp<C: Collator>(pattern: &[u8], match_type: &[u8]) -> Result<Regex> {
    if pattern.is_empty() {
        return Err(Error::regexp_error("Empty pattern is invalid in regexp".into()).into());
    }

    let match_type = get_match_type::<C>(match_type)?;

    let pattern = String::from_utf8(pattern.to_vec())?;
    let pattern_with_tp = if !match_type.is_empty() {
        format!("(?{}){}", match_type, pattern)
    } else {
        pattern
    };

    Regex::new(&pattern_with_tp)
        .map_err(|e| Error::regexp_error(format!("Invalid regexp pattern: {:?}", e)).into())
}

fn build_regexp_from_args<C: Collator>(
    args: &[ScalarValueRef<'_>],
    match_idx: usize,
) -> Result<Option<Regex>> {
    let pattern = match args[PATTERN_IDX].as_bytes() {
        Some(b) => b,
        None => return Ok(None),
    };

    let match_type = if args.len() > match_idx {
        match args[match_idx].as_bytes() {
            Some(b) => b,
            None => return Ok(None),
        }
    } else {
        b""
    };

    build_regexp::<C>(pattern, match_type).map(Some)
}

fn init_regexp_data<C: Collator, const N: usize>(expr: &mut CallBuild) -> Result<Option<Regex>> {
    let children = expr.args();
    if children.len() <= PATTERN_IDX {
        return Ok(None);
    }
    let Some(pattern) = children[PATTERN_IDX].constant_bytes() else {
        return Ok(None);
    };
    let match_type = if children.len() > N {
        let Some(value) = children[N].constant_bytes() else {
            return Ok(None);
        };
        value
    } else {
        b""
    };
    build_regexp::<C>(pattern, match_type).map(Some)
}

/// Currently, TiDB only supports regular expressions for utf-8 strings.
/// See https://dev.mysql.com/doc/refman/8.0/en/regexp.html#function_regexp-like.
#[rpn_fn(nullable, raw_varg, min_args = 2, max_args = 3, capture = [metadata], metadata_mapper = init_regexp_data::<C, LIKE_MATCH_IDX>)]
#[inline]
pub fn regexp_like<C: Collator>(
    metadata: &Option<Regex>,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<i64>> {
    let expr = match args[0].as_bytes() {
        Some(e) => std::str::from_utf8(e)?,
        None => return Ok(None),
    };
    let regex = match metadata {
        Some(r) => Cow::Borrowed(r),
        None => match build_regexp_from_args::<C>(args, LIKE_MATCH_IDX)? {
            Some(r) => Cow::Owned(r),
            None => return Ok(None),
        },
    };

    Ok(Some(regex.is_match(expr) as i64))
}

/// Currently, TiDB only supports regular expressions for utf-8 strings.
/// See https://dev.mysql.com/doc/refman/8.0/en/regexp.html#function_regexp-substr.
#[rpn_fn(nullable, raw_varg, min_args = 2, max_args = 5, capture = [metadata], metadata_mapper = init_regexp_data::<C, SUBSTR_MATCH_IDX>)]
#[inline]
pub fn regexp_substr<C: Collator>(
    metadata: &Option<Regex>,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Bytes>> {
    let mut expr = match args[0].as_bytes() {
        Some(e) => std::str::from_utf8(e)?,
        None => return Ok(None),
    };
    let regex = match metadata {
        Some(r) => Cow::Borrowed(r),
        None => match build_regexp_from_args::<C>(args, SUBSTR_MATCH_IDX)? {
            Some(r) => Cow::Owned(r),
            None => return Ok(None),
        },
    };

    if args.len() >= 3 {
        let pos = match EvaluableRef::borrow_scalar_value_ref(args[2]) {
            Some::<&i64>(p) => *p,
            None => return Ok(None),
        };

        expr = regexp_trim_at(expr, pos)
            .map_err(wire_regexp_policy_error)?
            .1;
    }

    let mut occurrence = 1i64;
    if args.len() >= 4 {
        occurrence = match EvaluableRef::borrow_scalar_value_ref(args[3]) {
            Some::<&i64>(o) => *o,
            None => return Ok(None),
        };
    };

    Ok(regexp_substr_match(&regex, expr, occurrence).map(|matched| matched.as_bytes().to_vec()))
}

/// Currently, TiDB only supports regular expressions for utf-8 strings.
/// See https://dev.mysql.com/doc/refman/8.0/en/regexp.html#function_regexp-instr.
#[rpn_fn(nullable, raw_varg, min_args = 2, max_args = 6, capture = [metadata], metadata_mapper = init_regexp_data::<C, INSTR_MATCH_IDX>)]
#[inline]
pub fn regexp_instr<C: Collator>(
    metadata: &Option<Regex>,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<i64>> {
    let mut expr = match args[0].as_bytes() {
        Some(e) => std::str::from_utf8(e)?,
        None => return Ok(None),
    };
    let regex = match metadata {
        Some(r) => Cow::Borrowed(r),
        None => match build_regexp_from_args::<C>(args, INSTR_MATCH_IDX)? {
            Some(r) => Cow::Owned(r),
            None => return Ok(None),
        },
    };

    let mut pos = 1i64;
    if args.len() >= 3 {
        pos = match EvaluableRef::borrow_scalar_value_ref(args[2]) {
            Some::<&i64>(p) => *p,
            None => return Ok(None),
        };

        expr = regexp_trim_at(expr, pos)
            .map_err(wire_regexp_policy_error)?
            .1;
    }

    let mut occurrence = 1i64;
    if args.len() >= 4 {
        occurrence = match EvaluableRef::borrow_scalar_value_ref(args[3]) {
            Some::<&i64>(o) => *o,
            None => return Ok(None),
        };
    };

    let mut return_option = 0i64;
    if args.len() >= 5 {
        return_option = match EvaluableRef::borrow_scalar_value_ref(args[4]) {
            Some::<&i64>(o) => *o,
            None => return Ok(None),
        };

        if return_option != 0 && return_option != 1 {
            let err = format!("Invalid regexp return option: {}", return_option);
            return Err(Error::regexp_error(err).into());
        }
    };

    Ok(Some(regexp_instr_match(
        &regex,
        expr,
        pos,
        occurrence,
        return_option,
    )))
}

pub struct ReplaceMetaData {
    regex: Option<Regex>,
    instructions: Option<Vec<ReplaceInstruction>>,
}

fn init_regexp_replace_data<C: Collator>(expr: &mut CallBuild) -> Result<ReplaceMetaData> {
    let mut meta = ReplaceMetaData {
        regex: init_regexp_data::<C, REPLACE_MATCH_IDX>(expr)?,
        instructions: None,
    };

    let children = expr.args();
    if let Some(value) = children.get(2).and_then(|arg| arg.constant_bytes()) {
        meta.instructions = Some(init_replace_instructions(value));
    }

    Ok(meta)
}

fn init_replace_instructions(replace_expr: &[u8]) -> Vec<ReplaceInstruction> {
    regexp_replacement_parts(replace_expr)
}

/// Currently, TiDB only supports regular expressions for utf-8 strings.
/// See https://dev.mysql.com/doc/refman/8.0/en/regexp.html#function_regexp-replace.
#[rpn_fn(nullable, raw_varg, min_args = 3, max_args = 6, capture = [metadata], metadata_mapper = init_regexp_replace_data::<C>)]
#[inline]
pub fn regexp_replace<C: Collator>(
    metadata: &ReplaceMetaData,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Bytes>> {
    let expr = match args[0].as_bytes() {
        Some(e) => std::str::from_utf8(e)?,
        None => return Ok(None),
    };
    let regex = match metadata.regex.as_ref() {
        Some(r) => Cow::Borrowed(r),
        None => match build_regexp_from_args::<C>(args, REPLACE_MATCH_IDX)? {
            Some(r) => Cow::Owned(r),
            None => return Ok(None),
        },
    };
    let replace_inst = match metadata.instructions.as_ref() {
        Some(i) => Cow::Borrowed(i),
        None => match args[2].as_bytes() {
            Some(e) => Cow::Owned(init_replace_instructions(e)),
            None => return Ok(None),
        },
    };

    let (mut before_trimmed, mut trimmed) = ("", expr);
    if args.len() >= 4 {
        let pos = match EvaluableRef::borrow_scalar_value_ref(args[3]) {
            Some::<&i64>(p) => *p,
            None => return Ok(None),
        };

        let (idx, suffix) = regexp_trim_at(expr, pos).map_err(wire_regexp_policy_error)?;
        before_trimmed = &expr[..idx];
        trimmed = suffix;
    }

    let mut occurrence = 0i64;
    if args.len() >= 5 {
        occurrence = match EvaluableRef::borrow_scalar_value_ref(args[4]) {
            Some::<&i64>(o) => *o,
            None => return Ok(None),
        };
    };

    regexp_replace_matches(
        before_trimmed,
        trimmed,
        &regex,
        replace_inst.as_ref(),
        occurrence,
        RegexpReplacementEncoding::WireBytes,
    )
    .map(Some)
    .map_err(wire_regexp_policy_error)
}

// Native-only fixed recipes. Their typed invocation metadata is initially
// empty; the guarded worker binds actual statement-owned cache handles for one
// call. Ordinary wire recipes and their construction-time metadata remain
// above.
fn native_regexp_error(error: NativeRegexpError) -> tidb_query_common::Error {
    EvaluateError::Caused(Box::new(error)).into()
}

fn native_regexp_infrastructure_error(error: LocalError) -> tidb_query_common::Error {
    EvaluateError::Caused(Box::new(error)).into()
}

fn native_regexp_policy_error(error: RegexpPolicyError) -> tidb_query_common::Error {
    native_regexp_error(NativeRegexpError::Policy(error))
}

fn native_regexp_bytes<'a>(
    args: &[ScalarValueRef<'a>],
    index: usize,
) -> Result<Option<BytesRef<'a>>> {
    match args.get(index).copied() {
        Some(ScalarValueRef::Bytes(value)) => Ok(value),
        _ => Err(other_err!(
            "Native regexp argument {} requires Bytes",
            index
        )),
    }
}

fn native_regexp_text<'a>(args: &[ScalarValueRef<'a>], index: usize) -> Result<&'a str> {
    let bytes = native_regexp_bytes(args, index)?.ok_or_else(|| {
        other_err!(
            "Native regexp argument {} requires a non-NULL string",
            index
        )
    })?;
    // Native frontends retain SQL string coercion and its original ordering.
    // A malformed prepared string is a transport fault, never a regex SQL cause.
    std::str::from_utf8(bytes).map_err(|error| {
        other_err!(
            "Native regexp argument {} is not prepared UTF-8: {}",
            index,
            error
        )
    })
}

fn native_regexp_int(args: &[ScalarValueRef<'_>], index: usize) -> Result<Int> {
    match args.get(index).copied() {
        Some(ScalarValueRef::Int(Some(value))) => Ok(*value),
        _ => Err(other_err!(
            "Native regexp argument {} requires a non-NULL Int",
            index
        )),
    }
}

fn native_regexp_instr_tail<'a>(args: &[ScalarValueRef<'a>]) -> Result<(Int, &'a str)> {
    let return_option = native_regexp_int(args, 4)?;
    if return_option != 0 && return_option != 1 {
        return Err(native_regexp_error(NativeRegexpError::InvalidReturnOption(
            return_option,
        )));
    }
    // The closed role permits an Undemanded flags slot only for the rejected
    // return_option above. Do not inspect even its physical representation early.
    Ok((return_option, native_regexp_text(args, 5)?))
}

fn native_regexp_resolve_pattern(
    metadata: &NativeRegexpCallMetadata,
    invocation: &NativeRegexpInvocation,
    pattern: &str,
    match_type: &str,
) -> Result<Arc<NativeCachedRegexp>> {
    let cached = invocation.resolve_pattern(pattern, match_type);
    // Observe the very Arc the kernel will borrow, including a cached failure.
    // Never re-read the owner's context slot after another context can replace it.
    metadata
        .record_known_cache_bytes(known_pattern_cache_bytes(&cached))
        .map_err(native_regexp_infrastructure_error)?;
    Ok(cached)
}

fn native_regexp_pattern(cached: &NativeCachedRegexp) -> Result<&Regex> {
    cached
        .result
        .as_ref()
        .map_err(|error| native_regexp_error(NativeRegexpError::Compile(error.clone())))
}

fn init_regexp_like_native_data(_expr: &mut CallBuild) -> Result<NativeRegexpCallMetadata> {
    Ok(NativeRegexpCallMetadata::new(NativeRegexpKind::Like))
}

fn init_regexp_substr_native_data(_expr: &mut CallBuild) -> Result<NativeRegexpCallMetadata> {
    Ok(NativeRegexpCallMetadata::new(NativeRegexpKind::Substr))
}

fn init_regexp_instr_native_data(_expr: &mut CallBuild) -> Result<NativeRegexpCallMetadata> {
    Ok(NativeRegexpCallMetadata::new(NativeRegexpKind::Instr))
}

fn init_regexp_replace_native_data(_expr: &mut CallBuild) -> Result<NativeRegexpCallMetadata> {
    Ok(NativeRegexpCallMetadata::new(NativeRegexpKind::Replace))
}

#[rpn_fn(nullable, raw_varg, min_args = 3, max_args = 3, capture = [metadata], metadata_mapper = init_regexp_like_native_data)]
fn get_regexp_like_native(
    metadata: &NativeRegexpCallMetadata,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Int>> {
    let invocation = metadata
        .invocation()
        .map_err(native_regexp_infrastructure_error)?;
    let text = native_regexp_text(args, 0)?;
    let pattern = native_regexp_text(args, 1)?;
    // The caller has already prepended only its original collation-derived flag.
    let match_type = native_regexp_text(args, 2)?;
    let cached = native_regexp_resolve_pattern(metadata, &invocation, pattern, match_type)?;
    Ok(Some(i64::from(
        native_regexp_pattern(&cached)?.is_match(text),
    )))
}

#[rpn_fn(nullable, raw_varg, min_args = 5, max_args = 5, capture = [metadata], metadata_mapper = init_regexp_substr_native_data)]
fn get_regexp_substr_native(
    metadata: &NativeRegexpCallMetadata,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Bytes>> {
    let invocation = metadata
        .invocation()
        .map_err(native_regexp_infrastructure_error)?;
    let text = native_regexp_text(args, 0)?;
    let pattern = native_regexp_text(args, 1)?;
    let pos = native_regexp_int(args, 2)?;
    let occurrence = native_regexp_int(args, 3)?;
    let match_type = native_regexp_text(args, 4)?;
    let (_, trimmed) = regexp_trim_at(text, pos).map_err(native_regexp_policy_error)?;
    let cached = native_regexp_resolve_pattern(metadata, &invocation, pattern, match_type)?;
    Ok(
        regexp_substr_match(native_regexp_pattern(&cached)?, trimmed, occurrence)
            .map(|matched| matched.as_bytes().to_vec()),
    )
}

#[rpn_fn(nullable, raw_varg, min_args = 6, max_args = 6, capture = [metadata], metadata_mapper = init_regexp_instr_native_data)]
fn get_regexp_instr_native(
    metadata: &NativeRegexpCallMetadata,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Int>> {
    let invocation = metadata
        .invocation()
        .map_err(native_regexp_infrastructure_error)?;
    let text = native_regexp_text(args, 0)?;
    let pattern = native_regexp_text(args, 1)?;
    let pos = native_regexp_int(args, 2)?;
    let occurrence = native_regexp_int(args, 3)?;
    let (return_option, match_type) = native_regexp_instr_tail(args)?;
    let (_, trimmed) = regexp_trim_at(text, pos).map_err(native_regexp_policy_error)?;
    let cached = native_regexp_resolve_pattern(metadata, &invocation, pattern, match_type)?;
    Ok(Some(regexp_instr_match(
        native_regexp_pattern(&cached)?,
        trimmed,
        pos,
        occurrence,
        return_option,
    )))
}

#[rpn_fn(nullable, raw_varg, min_args = 6, max_args = 6, capture = [metadata], metadata_mapper = init_regexp_replace_native_data)]
fn get_regexp_replace_native(
    metadata: &NativeRegexpCallMetadata,
    args: &[ScalarValueRef<'_>],
) -> Result<Option<Bytes>> {
    let invocation = metadata
        .invocation()
        .map_err(native_regexp_infrastructure_error)?;
    let text = native_regexp_text(args, 0)?;
    let pattern = native_regexp_text(args, 1)?;
    let replacement = native_regexp_text(args, 2)?;
    let pos = native_regexp_int(args, 3)?;
    let occurrence = native_regexp_int(args, 4)?;
    let match_type = native_regexp_text(args, 5)?;
    let (prefix_len, trimmed) = regexp_trim_at(text, pos).map_err(native_regexp_policy_error)?;
    let cached = native_regexp_resolve_pattern(metadata, &invocation, pattern, match_type)?;
    let regex = native_regexp_pattern(&cached)?;
    let parts = invocation.resolve_replacement(replacement);
    // This is an additional checked charge, not a replacement for the pattern's.
    metadata
        .record_known_cache_bytes(known_replacement_cache_bytes(&parts))
        .map_err(native_regexp_infrastructure_error)?;
    regexp_replace_matches(
        &text[..prefix_len],
        trimmed,
        regex,
        parts.as_ref(),
        occurrence,
        RegexpReplacementEncoding::NativeUtf8,
    )
    .map(Some)
    .map_err(native_regexp_policy_error)
}

fn native_regexp_legacy_like(
    args: &[ScalarValueRef<'_>],
    case_insensitive: bool,
) -> Result<Option<Int>> {
    // Both evaluated operands belong to the old legacy demand frontier.
    let (text, pattern) = (native_regexp_bytes(args, 0)?, native_regexp_bytes(args, 1)?);
    let (Some(text), Some(pattern)) = (text, pattern) else {
        return Ok(None);
    };
    let text = std::str::from_utf8(text).unwrap_or_default();
    let pattern = std::str::from_utf8(pattern).unwrap_or_default();
    match RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(regex) => Ok(Some(i64::from(regex.is_match(text)))),
        // Only the original regex compilation failure domain becomes NULL.
        // Argument, binding, budget and outer worker failures are never folded.
        Err(_) => Ok(None),
    }
}

#[rpn_fn(nullable, raw_varg, min_args = 2, max_args = 2)]
fn get_regexp_like_legacy_ci_native(args: &[ScalarValueRef<'_>]) -> Result<Option<Int>> {
    native_regexp_legacy_like(args, true)
}

#[rpn_fn(nullable, raw_varg, min_args = 2, max_args = 2)]
fn get_regexp_like_legacy_bin_native(args: &[ScalarValueRef<'_>]) -> Result<Option<Int>> {
    native_regexp_legacy_like(args, false)
}

#[rpn_fn(nullable)]
fn get_regexp_null_int_native(value: Option<&Int>) -> Result<Option<Int>> {
    match value {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native regexp NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn(nullable)]
fn get_regexp_null_bytes_native(value: Option<&Int>) -> Result<Option<Bytes>> {
    match value {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native regexp NULL witness must be an actual NULL"
        )),
    }
}

// A missing legacy child is not an evaluated SQL NULL operand. This recipe
// consumes no value at all; the caller retains the original child-presence
// gate.
#[rpn_fn]
fn get_regexp_missing_legacy_native() -> Result<Option<Int>> {
    Ok(None)
}

#[cfg(test)]
mod native_tests {
    use tidb_query_common::error::ErrorInner;

    use super::*;

    fn native_cause(error: &tidb_query_common::Error) -> Option<&NativeRegexpError> {
        match error.0.as_ref() {
            ErrorInner::Evaluate(EvaluateError::Caused(cause)) => cause.downcast_ref(),
            _ => None,
        }
    }

    #[test]
    fn native_regexp_legacy_policies_and_real_null_witnesses() {
        // Hand-derived literals, not a provider-produced expected table.
        let mixed_case = [
            ScalarValueRef::Bytes(Some(b"A")),
            ScalarValueRef::Bytes(Some(b"a")),
        ];
        assert_eq!(
            get_regexp_like_legacy_ci_native(&mixed_case).unwrap(),
            Some(1)
        );
        assert_eq!(
            get_regexp_like_legacy_bin_native(&mixed_case).unwrap(),
            Some(0)
        );
        for (text, pattern, expected) in [
            (&b"abc"[..], &b""[..], Some(1)),
            (&b"abc"[..], &b"("[..], None),
            (&b"\xff"[..], &b"^$"[..], Some(1)),
            (&b"abc"[..], &b"\xff"[..], Some(1)),
        ] {
            let args = [
                ScalarValueRef::Bytes(Some(text)),
                ScalarValueRef::Bytes(Some(pattern)),
            ];
            assert_eq!(get_regexp_like_legacy_bin_native(&args).unwrap(), expected);
        }
        let null = [
            ScalarValueRef::Bytes(None),
            ScalarValueRef::Bytes(Some(b"(")),
        ];
        assert_eq!(get_regexp_like_legacy_ci_native(&null).unwrap(), None);
        let wrong_type = [
            ScalarValueRef::Bytes(Some(b"a")),
            ScalarValueRef::Int(Some(&1)),
        ];
        assert!(get_regexp_like_legacy_bin_native(&wrong_type).is_err());
        assert_eq!(get_regexp_null_int_native(None).unwrap(), None);
        assert_eq!(get_regexp_null_bytes_native(None).unwrap(), None);
        assert_eq!(get_regexp_missing_legacy_native().unwrap(), None);
        assert!(get_regexp_null_int_native(Some(&0)).is_err());
        assert!(get_regexp_null_bytes_native(Some(&0)).is_err());
    }

    #[test]
    fn native_regexp_instr_rejects_return_option_before_reading_flags() {
        let mut args = [
            ScalarValueRef::Bytes(Some(b"a")),
            ScalarValueRef::Bytes(Some(b"(")),
            ScalarValueRef::Int(Some(&0)),
            ScalarValueRef::Int(Some(&1)),
            ScalarValueRef::Int(Some(&2)),
            ScalarValueRef::Bytes(Some(b"\xff")),
        ];
        let error = native_regexp_instr_tail(&args).unwrap_err();
        assert!(matches!(
            native_cause(&error),
            Some(NativeRegexpError::InvalidReturnOption(2))
        ));
        // Validating the tail really demands flags only after the option passes.
        args[4] = ScalarValueRef::Int(Some(&0));
        let error = native_regexp_instr_tail(&args).unwrap_err();
        assert!(native_cause(&error).is_none());
        args[5] = ScalarValueRef::Bytes(Some(b""));
        assert_eq!(native_regexp_instr_tail(&args).unwrap(), (0, ""));
        let error = regexp_trim_at("a", 0)
            .map_err(native_regexp_policy_error)
            .unwrap_err();
        assert!(matches!(
            native_cause(&error),
            Some(NativeRegexpError::Policy(
                RegexpPolicyError::InvalidPosition { pos: 0, count: 1 }
            ))
        ));
    }

    #[test]
    fn native_regexp_unbound_metadata_is_infrastructure_not_sql() {
        let bytes = ScalarValueRef::Bytes(Some(b"a"));
        let flags = ScalarValueRef::Bytes(Some(b""));
        let one = ScalarValueRef::Int(Some(&1));
        let zero = ScalarValueRef::Int(Some(&0));
        let errors = [
            get_regexp_like_native(
                &NativeRegexpCallMetadata::new(NativeRegexpKind::Like),
                &[bytes, bytes, flags],
            )
            .unwrap_err(),
            get_regexp_substr_native(
                &NativeRegexpCallMetadata::new(NativeRegexpKind::Substr),
                &[bytes, bytes, one, one, flags],
            )
            .unwrap_err(),
            get_regexp_instr_native(
                &NativeRegexpCallMetadata::new(NativeRegexpKind::Instr),
                &[bytes, bytes, one, one, zero, flags],
            )
            .unwrap_err(),
            get_regexp_replace_native(
                &NativeRegexpCallMetadata::new(NativeRegexpKind::Replace),
                &[bytes, bytes, bytes, one, zero, flags],
            )
            .unwrap_err(),
        ];
        for error in errors {
            assert!(native_cause(&error).is_none());
            match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(cause)) => {
                    assert!(matches!(
                        cause.downcast_ref::<LocalError>(),
                        Some(LocalError::InvalidSpec(_))
                    ));
                }
                _ => panic!("unbound metadata lost its actual LocalError: {error:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::{
        EvalType, FieldTypeTp,
        codec::batch::{LazyBatchColumn, LazyBatchColumnVec},
        expr::EvalContext,
    };
    use tipb::ScalarFuncSig;
    use tipb_helper::ExprDefBuilder;

    use crate::RpnExpressionBuilder;

    #[test]
    fn test_regexp_like() {
        let cases = vec![
            ("a", "^$", None, Some(0)),
            ("a", "a", None, Some(1)),
            ("b", "a", None, Some(0)),
            ("aA", "Aa", None, Some(0)),
            ("aaa", ".", None, Some(1)),
            ("ab", "^.$", None, Some(0)),
            ("b", "..", None, Some(0)),
            ("aab", ".ab", None, Some(1)),
            ("abcd", ".*", None, Some(1)),
            ("你", "^.$", None, Some(1)),
            ("你好", "你好", None, Some(1)),
            ("你好", "^你好$", None, Some(1)),
            ("你好", "^您好$", None, Some(0)),
            // Test wrong pattern
            ("", "(", None, None),
            ("", "(*", None, None),
            ("", "[a", None, None),
            ("aa", "", None, None),
            ("你好", "", None, None),
            // Test case-insensitive
            ("abc", "AbC", Some(""), Some(0)),
            ("abc", "AbC", Some("i"), Some(1)),
            // Test multiple-line mode
            ("123\n321", "23$", Some(""), Some(0)),
            ("123\n321", "23$", Some("m"), Some(1)),
            ("good\nday", "^day", Some("m"), Some(1)),
            // Test s flag(in mysql it's n flag)
            ("\n", ".", Some(""), Some(0)),
            ("\n", ".", Some("s"), Some(1)),
            // Test rightmost rule
            ("abc", "aBc", Some("ic"), Some(0)),
            ("abc", "aBc", Some("ci"), Some(1)),
            // Test invalid match type
            ("abc", "abc", Some("p"), None),
            ("abc", "abc", Some("cpi"), None),
        ];

        for (expr, pattern, match_type, expected) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpLikeSig, FieldTypeTp::LongLong);
            builder = builder
                .push_child(ExprDefBuilder::constant_bytes(expr.as_bytes().to_vec()))
                .push_child(ExprDefBuilder::constant_bytes(pattern.as_bytes().to_vec()));
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1);
            if expected.is_none() {
                assert!(exp.is_err());
                continue;
            }
            let exp = exp.unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1).unwrap();

            assert!(val.is_vector());
            let v = val.vector_value().unwrap().as_ref().to_int_vec();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], expected);
        }

        // Test null
        let cases = vec![
            (None, Some("a"), Some("i")),
            (Some("a"), None, Some("i")),
            (Some("a"), Some("a"), None),
        ];
        for (expr, pattern, match_type) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpSig, FieldTypeTp::LongLong);
            if let Some(e) = expr {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(e.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pattern {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(p.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1).unwrap();
            assert!(val.is_vector());
            let v = val.vector_value().unwrap().as_ref().to_int_vec();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], None);
        }
    }

    #[test]
    fn test_regexp_substr() {
        let cases = vec![
            ("abc", "bc", None, None, None, Some("bc"), false),
            ("你好啊", "好", None, None, None, Some("好"), false),
            ("abc", "bc", Some(2), None, None, Some("bc"), false),
            ("你好啊", "好", Some(2), None, None, Some("好"), false),
            ("你好啊", "好", Some(3), None, None, None, false),
            // Test invalid position index
            ("你好啊", "好", Some(4), None, None, None, true),
            ("你好啊", "好", Some(-1), None, None, None, true),
            ("", "a", Some(1), None, None, None, false),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(1),
                None,
                Some("abc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(0),
                None,
                Some("abc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(-1),
                None,
                Some("abc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(2),
                None,
                Some("abd"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(3),
                Some(1),
                None,
                Some("abd"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(3),
                Some(2),
                None,
                Some("abe"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(6),
                Some(1),
                None,
                Some("abe"),
                false,
            ),
            ("abc abd abe", "ab.", Some(6), Some(100), None, None, false),
            ("abc abd abe", "ab.", Some(100), Some(1), None, None, true),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(1),
                None,
                Some("嗯嗯"),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(2),
                None,
                Some("嗯好"),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(5),
                Some(1),
                None,
                Some("嗯呐"),
                false,
            ),
            ("嗯嗯 嗯好 嗯呐", "嗯.", Some(5), Some(2), None, None, false),
            ("abc", "ab.", Some(1), Some(1), Some(""), Some("abc"), false),
            (
                "abc",
                "aB.",
                Some(1),
                Some(1),
                Some("i"),
                Some("abc"),
                false,
            ),
            ("abc", "aB.", Some(100), Some(1), Some("i"), None, true),
            (
                "good\nday",
                "od",
                Some(1),
                Some(1),
                Some("m"),
                Some("od"),
                false,
            ),
            ("\n", ".", Some(1), Some(1), Some("s"), Some("\n"), false),
        ];

        for (expr, pattern, pos, occur, match_type, expected, error) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpSubstrSig, FieldTypeTp::String);
            builder = builder
                .push_child(ExprDefBuilder::constant_bytes(expr.as_bytes().to_vec()))
                .push_child(ExprDefBuilder::constant_bytes(pattern.as_bytes().to_vec()));
            if let Some(p) = pos {
                builder = builder.push_child(ExprDefBuilder::constant_int(p));
            }
            if let Some(o) = occur {
                builder = builder.push_child(ExprDefBuilder::constant_int(o));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();
            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();
            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1);
            match val {
                Ok(val) => {
                    assert!(val.is_vector());
                    let v = val.vector_value().unwrap().as_ref().to_bytes_vec();
                    assert_eq!(v.len(), 1);
                    assert_eq!(
                        v[0],
                        expected.map(|e| e.as_bytes().to_vec()),
                        "{:?} {:?} {:?}",
                        expr,
                        pattern,
                        pos
                    );
                }
                Err(e) => {
                    assert!(error, "val has error {:?}", e);
                }
            }
        }

        // Test null
        let cases = vec![
            (None, Some("a"), Some(1), Some(1), Some("i")),
            (Some("a"), None, Some(1), Some(1), Some("i")),
            (Some("a"), Some("a"), None, Some(1), Some("i")),
            (Some("a"), Some("a"), Some(1), None, Some("i")),
            (Some("a"), Some("a"), Some(1), Some(1), None),
        ];
        for (expr, pattern, pos, occur, match_type) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpSubstrSig, FieldTypeTp::String);
            if let Some(e) = expr {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(e.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pattern {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(p.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pos {
                builder = builder.push_child(ExprDefBuilder::constant_int(p));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(o) = occur {
                builder = builder.push_child(ExprDefBuilder::constant_int(o));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1).unwrap();
            assert!(val.is_vector());
            let v = val.vector_value().unwrap().as_ref().to_bytes_vec();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], None);
        }
    }

    #[test]
    fn test_regexp_instr() {
        let cases = vec![
            ("abc", "bc", None, None, None, None, Some(2), false),
            ("你好啊", "好", None, None, None, None, Some(2), false),
            ("abc", "bc", Some(2), None, None, None, Some(2), false),
            ("你好啊", "好", Some(2), None, None, None, Some(2), false),
            ("你好啊", "好", Some(3), None, None, None, Some(0), false),
            // Test invalid position index
            ("你好啊", "好", Some(4), None, None, None, Some(0), true),
            ("你好啊", "好", Some(-1), None, None, None, Some(0), true),
            ("", "a", Some(1), None, None, None, Some(0), false),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(1),
                None,
                None,
                Some(1),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(0),
                None,
                None,
                Some(1),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(-1),
                None,
                None,
                Some(1),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(2),
                None,
                None,
                Some(5),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(3),
                Some(1),
                None,
                None,
                Some(5),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(3),
                Some(2),
                None,
                None,
                Some(9),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(6),
                Some(1),
                None,
                None,
                Some(9),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(6),
                Some(100),
                None,
                None,
                Some(0),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(1),
                None,
                None,
                Some(1),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(2),
                None,
                None,
                Some(4),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(5),
                Some(1),
                None,
                None,
                Some(7),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(5),
                Some(2),
                None,
                None,
                Some(0),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(100),
                None,
                None,
                Some(0),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(1),
                Some(0),
                None,
                Some(1),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                Some(1),
                Some(1),
                Some(1),
                None,
                Some(4),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(1),
                Some(0),
                None,
                Some(1),
                false,
            ),
            (
                "嗯嗯 嗯好 嗯呐",
                "嗯.",
                Some(1),
                Some(1),
                Some(1),
                None,
                Some(3),
                false,
            ),
            ("", "^$", Some(1), Some(1), Some(0), None, Some(1), false),
            ("", "^$", Some(1), Some(1), Some(1), None, Some(1), false),
            (
                "abc",
                "ab.",
                Some(1),
                Some(1),
                Some(0),
                Some(""),
                Some(1),
                false,
            ),
            (
                "abc",
                "aB.",
                Some(1),
                Some(1),
                Some(0),
                Some("i"),
                Some(1),
                false,
            ),
            (
                "good\nday",
                "od$",
                Some(1),
                Some(1),
                Some(0),
                Some("m"),
                Some(3),
                false,
            ),
            (
                "good\nday",
                "oD$",
                Some(1),
                Some(1),
                Some(0),
                Some("mi"),
                Some(3),
                false,
            ),
            (
                "\n",
                ".",
                Some(1),
                Some(1),
                Some(0),
                Some("s"),
                Some(1),
                false,
            ),
        ];

        for (expr, pattern, pos, occur, ret_opt, match_type, expected, error) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpInStrSig, FieldTypeTp::LongLong);
            builder = builder
                .push_child(ExprDefBuilder::constant_bytes(expr.as_bytes().to_vec()))
                .push_child(ExprDefBuilder::constant_bytes(pattern.as_bytes().to_vec()));
            if let Some(p) = pos {
                builder = builder.push_child(ExprDefBuilder::constant_int(p));
            }
            if let Some(o) = occur {
                builder = builder.push_child(ExprDefBuilder::constant_int(o));
            }
            if let Some(r) = ret_opt {
                builder = builder.push_child(ExprDefBuilder::constant_int(r));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1);

            match val {
                Ok(val) => {
                    assert!(val.is_vector());
                    let v = val.vector_value().unwrap().as_ref().to_int_vec();
                    assert_eq!(v.len(), 1);
                    assert_eq!(v[0], expected, "{:?} {:?} {:?}", expr, pattern, pos);
                }
                Err(e) => {
                    assert!(error, "val has error {:?}", e);
                }
            }
        }

        // Test null
        let cases = vec![
            (None, Some("a"), Some(1), Some(1), Some(0), Some("i")),
            (Some("a"), None, Some(1), Some(1), Some(0), Some("i")),
            (Some("a"), Some("a"), None, Some(1), Some(0), Some("i")),
            (Some("a"), Some("a"), Some(1), None, Some(0), Some("i")),
            (Some("a"), Some("a"), Some(1), Some(1), None, Some("i")),
            (Some("a"), Some("a"), Some(1), Some(1), Some(0), None),
        ];
        for (expr, pattern, pos, occur, ret_opt, match_type) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpInStrSig, FieldTypeTp::LongLong);
            if let Some(e) = expr {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(e.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pattern {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(p.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pos {
                builder = builder.push_child(ExprDefBuilder::constant_int(p));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(o) = occur {
                builder = builder.push_child(ExprDefBuilder::constant_int(o));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(r) = ret_opt {
                builder = builder.push_child(ExprDefBuilder::constant_int(r));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1).unwrap();
            assert!(val.is_vector());
            let v = val.vector_value().unwrap().as_ref().to_int_vec();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], None);
        }
    }

    #[test]
    fn test_regexp_replace() {
        let cases = vec![
            (
                "abc abd abe",
                "ab.",
                "cz",
                None,
                None,
                None,
                Some("cz cz cz"),
                false,
            ),
            (
                "你好 好的",
                "好",
                "逸",
                None,
                None,
                None,
                Some("你逸 逸的"),
                false,
            ),
            ("", "^$", "123", None, None, None, Some("123"), false),
            ("abc", "ab.", "cc", Some(1), None, None, Some("cc"), false),
            ("abc", "bc", "cc", Some(3), None, None, Some("abc"), false),
            ("你好", "好", "的", Some(2), None, None, Some("你的"), false),
            (
                "你好啊",
                "好",
                "的",
                Some(3),
                None,
                None,
                Some("你好啊"),
                false,
            ),
            ("", "^$", "cc", Some(1), None, None, Some("cc"), false),
            // Test invalid position index
            ("", "^$", "a", Some(2), None, None, None, true),
            ("", "^&", "a", Some(0), None, None, None, true),
            ("abc", "bc", "a", Some(-1), None, None, None, true),
            ("abc", "bc", "a", Some(4), None, None, None, true),
            (
                "abc abd",
                "ab.",
                "cc",
                Some(1),
                Some(1),
                None,
                Some("cc abd"),
                false,
            ),
            (
                "abc abd",
                "ab.",
                "cc",
                Some(1),
                Some(-1),
                None,
                Some("cc abd"),
                false,
            ),
            (
                "abc abd",
                "ab.",
                "cc",
                Some(1),
                Some(2),
                None,
                Some("abc cc"),
                false,
            ),
            (
                "abc abd",
                "ab.",
                "cc",
                Some(1),
                Some(0),
                None,
                Some("cc cc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                "cc",
                Some(1),
                Some(3),
                None,
                Some("abc abd cc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                "cc",
                Some(3),
                Some(2),
                None,
                Some("abc abd cc"),
                false,
            ),
            (
                "abc abd abe",
                "ab.",
                "cc",
                Some(3),
                Some(10),
                None,
                Some("abc abd abe"),
                false,
            ),
            (
                "你好 好啊",
                "好",
                "的",
                Some(1),
                Some(1),
                None,
                Some("你的 好啊"),
                false,
            ),
            (
                "你好 好啊",
                "好",
                "的",
                Some(3),
                Some(1),
                None,
                Some("你好 的啊"),
                false,
            ),
            ("", "^$", "cc", Some(1), Some(1), None, Some("cc"), false),
            ("", "^$", "cc", Some(1), Some(2), None, Some(""), false),
            ("", "^$", "cc", Some(1), Some(-1), None, Some("cc"), false),
            (
                "abc",
                "ab.",
                "cc",
                Some(1),
                Some(0),
                Some(""),
                Some("cc"),
                false,
            ),
            (
                "abc",
                "aB.",
                "cc",
                Some(1),
                Some(0),
                Some("i"),
                Some("cc"),
                false,
            ),
            (
                "good\nday",
                "od$",
                "cc",
                Some(1),
                Some(0),
                Some("m"),
                Some("gocc\nday"),
                false,
            ),
            (
                "good\nday",
                "oD$",
                "cc",
                Some(1),
                Some(0),
                Some("mi"),
                Some("gocc\nday"),
                false,
            ),
            (
                "\n",
                ".",
                "cc",
                Some(1),
                Some(0),
                Some("s"),
                Some("cc"),
                false,
            ),
            (
                "好的 好滴 好~",
                ".",
                "的",
                Some(1),
                Some(0),
                Some("msi"),
                Some("的的的的的的的的"),
                false,
            ),
            (
                r"abc",
                r"\d*",
                r"d",
                None,
                None,
                None,
                Some(r"dadbdcd"),
                false,
            ),
            // Test capture group
            (
                r"seafood fool",
                r"foo(.?)",
                r"123",
                Some(3),
                None,
                None,
                Some(r"sea123 123"),
                false,
            ),
            (
                r"seafood fool",
                r"foo(.?)",
                r"123",
                Some(5),
                None,
                None,
                Some(r"seafood 123"),
                false,
            ),
            (
                r"seafood fool",
                r"foo(.?)",
                r"z\12",
                Some(3),
                None,
                None,
                Some(r"seazd2 zl2"),
                false,
            ),
            (
                r"seafood fool",
                r"foo(.?)",
                r"z\12",
                Some(5),
                None,
                None,
                Some(r"seafood zl2"),
                false,
            ),
            (
                r"seafood fool",
                r"foo(.?)",
                r"z\12",
                Some(3),
                Some(0),
                None,
                Some(r"seazd2 zl2"),
                false,
            ),
            (
                r"seafood foo",
                r"foo(.?)",
                r"z\12",
                Some(3),
                Some(2),
                None,
                Some(r"seafood z2"),
                false,
            ),
            (
                r"stackoverflow",
                r"(.{5})(.*)",
                r"\\+\2+\1+\2+\1\",
                None,
                None,
                None,
                Some(r"\+overflow+stack+overflow+stack"),
                false,
            ),
            (
                r"fooabcdefghij fooABCDEFGHIJ",
                r"foo(.)(.)(.)(.)(.)(.)(.)(.)(.)(.)",
                r"\\\9\\\8-\7\\\6-\5\\\4-\3\\\2-\1\\",
                Some(1),
                Some(0),
                None,
                Some(r"\i\h-g\f-e\d-c\b-a\ \I\H-G\F-E\D-C\B-A\"),
                false,
            ),
            (
                r"fool food foo",
                r"foo(.?)",
                r"\0+\1",
                None,
                None,
                None,
                Some(r"fool+l food+d foo+"),
                false,
            ),
            // \2 is out of capture group's range.
            (
                r"fool food foo",
                r"foo(.?)",
                r"\0+\2",
                None,
                None,
                None,
                None,
                true,
            ),
            (
                r"https://go.mail/folder-1/online/ru-en/#lingvo/#1О 50000&price_ashka/rav4/page=/check.xml",
                r"^https?://(?:www\\.)?([^/]+)/.*$",
                r"a\12\13",
                None,
                None,
                None,
                Some(r"ago.mail2go.mail3"),
                false,
            ),
            (
                r"http://saint-peters-total=меньше 1000-rublyayusche/catalogue/kolasuryat-v-2-kadyirovka-personal/serial_id=0&input_state/apartments/mokrotochki.net/upravda.ru/yandex.ru/GameMain.aspx?mult]/on/orders/50195&text=мыс и орелка в Балаш смотреть онлайн бесплатно в хорошем камбалакс&lr=20030393833539353862643188&op_promo=C-Teaser_id=06d162.html",
                r"^https?://(?:www\\.)?([^/]+)/.*$",
                r"aaa\1233",
                None,
                None,
                None,
                Some(r"aaasaint-peters-total=меньше 1000-rublyayusche233"),
                false,
            ),
        ];

        for (expr, pattern, replace, pos, occur, match_type, expected, error) in cases {
            for i in 0..2 {
                let use_column_ref = i == 1;

                let mut ctx = EvalContext::default();

                let mut builder = ExprDefBuilder::scalar_func(
                    ScalarFuncSig::RegexpReplaceSig,
                    FieldTypeTp::String,
                );

                let mut schema = Vec::new();
                let mut columns = LazyBatchColumnVec::empty();
                if use_column_ref {
                    schema.extend_from_slice(&[
                        FieldTypeTp::String.into(),
                        FieldTypeTp::String.into(),
                        FieldTypeTp::String.into(),
                    ]);
                    builder = builder
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::String))
                        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::String))
                        .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::String));
                    let mut col_vec = Vec::new();
                    let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Bytes);
                    col.mut_decoded().push_bytes(Some(expr.as_bytes().to_vec()));
                    col_vec.push(col);

                    let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Bytes);
                    col.mut_decoded()
                        .push_bytes(Some(pattern.as_bytes().to_vec()));
                    col_vec.push(col);

                    let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Bytes);
                    col.mut_decoded()
                        .push_bytes(Some(replace.as_bytes().to_vec()));
                    col_vec.push(col);

                    let mut count = 0;
                    if let Some(p) = pos {
                        count += 1;
                        schema.push(FieldTypeTp::Long.into());
                        let mut col =
                            LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Int);
                        col.mut_decoded().push_int(Some(p));
                        col_vec.push(col);

                        builder =
                            builder.push_child(ExprDefBuilder::column_ref(3, FieldTypeTp::Long));
                    }
                    if let Some(o) = occur {
                        assert_eq!(count, 1);
                        count += 1;
                        schema.push(FieldTypeTp::Long.into());
                        let mut col =
                            LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Int);
                        col.mut_decoded().push_int(Some(o));
                        col_vec.push(col);

                        builder =
                            builder.push_child(ExprDefBuilder::column_ref(4, FieldTypeTp::Long));
                    }
                    if let Some(m) = match_type {
                        assert_eq!(count, 2);
                        schema.push(FieldTypeTp::String.into());
                        let mut col =
                            LazyBatchColumn::decoded_with_capacity_and_tp(1, EvalType::Bytes);
                        col.mut_decoded().push_bytes(Some(m.as_bytes().to_vec()));
                        col_vec.push(col);

                        builder =
                            builder.push_child(ExprDefBuilder::column_ref(5, FieldTypeTp::String));
                    }

                    columns = LazyBatchColumnVec::from(col_vec);
                } else {
                    builder = builder
                        .push_child(ExprDefBuilder::constant_bytes(expr.as_bytes().to_vec()))
                        .push_child(ExprDefBuilder::constant_bytes(pattern.as_bytes().to_vec()))
                        .push_child(ExprDefBuilder::constant_bytes(replace.as_bytes().to_vec()));
                    let mut count = 0;
                    if let Some(p) = pos {
                        count += 1;
                        builder = builder.push_child(ExprDefBuilder::constant_int(p));
                    }
                    if let Some(o) = occur {
                        assert_eq!(count, 1);
                        count += 1;
                        builder = builder.push_child(ExprDefBuilder::constant_int(o));
                    }
                    if let Some(m) = match_type {
                        assert_eq!(count, 2);
                        builder = builder
                            .push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
                    }
                }

                let node = builder.build();
                let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, schema.len())
                    .unwrap();

                let val = exp.eval(&mut ctx, &schema, &mut columns, &[0], 1);

                match val {
                    Ok(val) => {
                        assert!(val.is_vector());
                        let v = val.vector_value().unwrap().as_ref().to_bytes_vec();
                        assert_eq!(v.len(), 1);
                        assert_eq!(
                            v[0],
                            expected.map(|e| e.as_bytes().to_vec()),
                            "{:?} {:?} {:?}",
                            expr,
                            pattern,
                            replace,
                        );
                    }
                    Err(e) => {
                        assert!(error, "val has error {:?}", e);
                    }
                }
            }
        }

        // Test null
        let cases = vec![
            (None, Some("a"), Some("a"), Some(1), Some(0), Some("i")),
            (Some("a"), None, Some("a"), Some(1), Some(0), Some("i")),
            (Some("a"), Some("a"), None, Some(1), Some(0), Some("i")),
            (Some("a"), Some("a"), Some("a"), None, Some(0), Some("i")),
            (Some("a"), Some("a"), Some("a"), Some(1), None, Some("i")),
            (Some("a"), Some("a"), Some("a"), Some(1), Some(0), None),
        ];
        for (expr, pattern, replace, pos, occur, match_type) in cases {
            let mut ctx = EvalContext::default();

            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::RegexpReplaceSig, FieldTypeTp::String);
            if let Some(e) = expr {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(e.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pattern {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(p.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(r) = replace {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(r.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }
            if let Some(p) = pos {
                builder = builder.push_child(ExprDefBuilder::constant_int(p));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(o) = occur {
                builder = builder.push_child(ExprDefBuilder::constant_int(o));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong));
            }
            if let Some(m) = match_type {
                builder = builder.push_child(ExprDefBuilder::constant_bytes(m.as_bytes().to_vec()));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::String));
            }

            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();

            let val = exp.eval(&mut ctx, schema, &mut columns, &[], 1).unwrap();
            assert!(val.is_vector());
            let v = val.vector_value().unwrap().as_ref().to_bytes_vec();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], None);
        }
    }
}
