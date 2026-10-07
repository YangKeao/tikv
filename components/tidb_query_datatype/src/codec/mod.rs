// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

// TODO: Replace by failure crate.
/// A shortcut to box an error.
macro_rules! invalid_type {
    ($e:expr) => ({
        use crate::codec::Error;
        Error::InvalidDataType(($e).into())
    });
    ($f:tt, $($arg:expr),+) => ({
        use crate::codec::Error;
        Error::InvalidDataType(format!($f, $($arg),+))
    });
}

pub mod batch;
pub mod chunk;
pub mod collation;
pub mod convert;
pub mod data_type;
pub mod datum;
pub mod datum_codec;
pub mod error;
pub mod mysql;
pub mod native_conversion_event;
pub mod native_decimal_context;
pub mod native_decimal_convert;
pub mod native_duration_convert;
pub mod native_eval_type;
pub mod native_field_value;
pub mod native_float_convert;
pub mod native_float_parse;
pub mod native_integer_convert;
pub mod native_json_construct;
pub mod native_json_parse;
pub mod native_mysql_json;
pub mod native_numeric;
pub mod native_reverse_bound;
pub mod native_scalar_convert;
pub mod native_sql_string;
pub mod native_string_convert;
pub mod native_string_type;
pub mod native_temporal_convert;
pub mod native_temporal_number;
pub mod native_type_name;
pub mod native_vector_convert;
mod overflow;
pub mod row;
pub mod table;

pub use self::{
    datum::Datum,
    error::{Error, Result},
    overflow::{div_i64, div_i64_with_u64, div_u64_with_i64},
};

const TEN_POW: &[u32] = &[
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];
