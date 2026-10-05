// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use super::Result;

/// `UNSPECIFIED_FSP` is the unspecified fractional seconds part.
pub const UNSPECIFIED_FSP: i8 = -1;
/// `MAX_FSP` is the maximum digit of fractional seconds part.
pub const MAX_FSP: i8 = 6;
/// `MIN_FSP` is the minimum digit of fractional seconds part.
pub const MIN_FSP: i8 = 0;
/// `DEFAULT_FSP` is the default digit of fractional seconds part.
/// `MySQL` use 0 as the default Fsp.
pub const DEFAULT_FSP: i8 = 0;
/// `DEFAULT_DIV_FRAC_INCR` is the default value of decimal divide precision
/// inrements.
pub const DEFAULT_DIV_FRAC_INCR: u8 = 4;

pub fn check_fsp(fsp: i8) -> Result<u8> {
    if fsp == UNSPECIFIED_FSP {
        return Ok(DEFAULT_FSP as u8);
    }
    if !(MIN_FSP..=MAX_FSP).contains(&fsp) {
        return Err(invalid_type!("Invalid fsp {}", fsp));
    }
    Ok(fsp as u8)
}

pub mod binary_literal;
pub mod charset;
pub mod decimal;
pub mod duration;
pub mod enums;
pub mod json;
pub mod locale;
mod native_decimal_codec;
mod native_mydecimal;
pub mod set;
pub mod time;
pub mod vector;
mod vector_native;

pub use self::{
    decimal::{
        Decimal, DecimalDecoder, DecimalEncoder, DecimalParts, DecimalWordsRef,
        NativeDecimalCmpParts, Res, RoundMode, dec_encoded_len, native_decimal_cmp,
    },
    duration::{Duration, DurationDecoder, DurationEncoder},
    enums::{Enum, EnumDecoder, EnumEncoder, EnumRef},
    json::{
        Json, JsonDatumPayloadChunkEncoder, JsonDecoder, JsonEncoder, JsonType, ModifyType,
        PathExpression, parse_json_path_expr,
    },
    native_decimal_codec::{
        DecimalCodecError as NativeDecimalCodecError,
        DecimalCodecWarning as NativeDecimalCodecWarning,
        checked_bin_size as native_decimal_checked_bin_size,
        decimal_bin_size as native_decimal_bin_size,
        remove_leading_zeros as native_decimal_remove_leading_zeros,
        write_bin as native_decimal_write_bin,
    },
    native_mydecimal::{
        DecimalError as NativeMyDecimalError,
        MAX_WORD_BUF_LEN as NATIVE_MYDECIMAL_MAX_WORD_BUF_LEN,
        MYDECIMAL_STRUCT_SIZE as NATIVE_MYDECIMAL_STRUCT_SIZE, MyDecimal as NativeMyDecimal,
        RoundMode as NativeMyDecimalRoundMode, digits_to_words as native_mydecimal_digits_to_words,
    },
    set::{Set, SetRef},
    time::{Time, TimeDecoder, TimeEncoder, TimeType, Tz},
    vector::{VectorFloat32, VectorFloat32Decoder, VectorFloat32Encoder, VectorFloat32Ref},
    vector_native::{
        NATIVE_MAX_VECTOR_DIMENSION, NativeVectorError, NativeVectorFloat32,
        check_native_vector_dim_valid, deserialize_native_vector_float32,
        peek_native_vector_float32,
    },
};
