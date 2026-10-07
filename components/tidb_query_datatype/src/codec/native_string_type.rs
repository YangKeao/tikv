// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native named-type classification for string conversion. A raw unknown type
//! byte is not reinterpreted as a known variant, even when the numbers
//! coincide.

/// Actual source type identity needed by native string conversion. Known
/// non-string types other than Year can be projected to Other for these
/// predicates; this is not a replacement for their full field-type metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeStringTypeCode {
    Year,
    Unspecified,
    VarChar,
    TinyBlob,
    MediumBlob,
    LongBlob,
    Blob,
    VarString,
    String,
    Enum,
    Set,
    Bit,
    Json,
    VectorFloat32,
    Other(u8),
}

impl NativeStringTypeCode {
    pub const fn is_hybrid(self) -> bool {
        matches!(self, Self::Enum | Self::Bit | Self::Set)
    }

    pub const fn is_var_length_type(self) -> bool {
        matches!(
            self,
            Self::VarChar
                | Self::VarString
                | Self::Json
                | Self::TinyBlob
                | Self::MediumBlob
                | Self::LongBlob
                | Self::Blob
                | Self::VectorFloat32
        )
    }

    pub fn is_character_string(self, collation: &str) -> bool {
        self.is_string() && !self.is_binary_string(collation)
    }

    /// Exact native IsTypeBlob named identities, never raw Other bytes.
    pub const fn is_blob(self) -> bool {
        matches!(
            self,
            Self::TinyBlob | Self::MediumBlob | Self::LongBlob | Self::Blob
        )
    }
    /// Native IsTypeChar excludes VarString.
    pub const fn is_char(self) -> bool {
        matches!(self, Self::String | Self::VarChar)
    }
    /// Native IsTypeVarchar includes VarString, unlike IsTypeChar.
    pub const fn is_varchar(self) -> bool {
        matches!(self, Self::VarString | Self::VarChar)
    }

    /// Exact native named-variant IsString policy. Other never becomes a
    /// string type by interpreting its payload as a MySQL type number.
    pub const fn is_string(self) -> bool {
        matches!(
            self,
            Self::Unspecified
                | Self::VarChar
                | Self::TinyBlob
                | Self::MediumBlob
                | Self::LongBlob
                | Self::Blob
                | Self::VarString
                | Self::String
        )
    }

    /// Native IsBinaryStr uses the actual collation name, case-sensitively.
    /// Charset names and flags are deliberately not inputs to this predicate.
    pub fn is_binary_string(self, collation: &str) -> bool {
        self.is_string() && collation == "binary"
    }
}

pub fn native_need_restored_data(
    code: NativeStringTypeCode,
    collation: &str,
    use_new_collation: bool,
    is_bin_collation: bool,
) -> bool {
    if !use_new_collation || !code.is_character_string(collation) {
        return false;
    }
    if collation == "utf8mb4_0900_bin" {
        return false;
    }
    !is_bin_collation || code.is_varchar()
}

pub fn native_enum_set_display_length(
    code: NativeStringTypeCode,
    lengths: impl IntoIterator<Item = usize>,
) -> i64 {
    let lengths = lengths.into_iter().map(|length| length as i64);
    match code {
        NativeStringTypeCode::Enum => lengths.max().unwrap_or(0),
        NativeStringTypeCode::Set => {
            let lengths: Vec<i64> = lengths.collect();
            lengths.iter().sum::<i64>() + lengths.len().saturating_sub(1) as i64
        }
        _ => -1,
    }
}

pub const fn native_field_type_has_charset(code: NativeStringTypeCode, binary_flag: bool) -> bool {
    match code {
        NativeStringTypeCode::VarChar
        | NativeStringTypeCode::String
        | NativeStringTypeCode::VarString
        | NativeStringTypeCode::TinyBlob
        | NativeStringTypeCode::MediumBlob
        | NativeStringTypeCode::LongBlob
        | NativeStringTypeCode::Blob => !binary_flag,
        NativeStringTypeCode::Enum | NativeStringTypeCode::Set => true,
        _ => false,
    }
}

#[cfg(test)]
#[test]
fn native_string_type_preserves_named_identity_and_exact_collation() {
    use NativeStringTypeCode::*;
    for code in [
        Unspecified,
        VarChar,
        TinyBlob,
        MediumBlob,
        LongBlob,
        Blob,
        VarString,
        String,
    ] {
        assert!(code.is_string());
        for collation in [
            "binary",
            "BINARY",
            "Binary",
            "binary ",
            "binary\0",
            "utf8mb4_bin",
            "",
        ] {
            assert_eq!(
                code.is_binary_string(collation),
                collation == "binary",
                "{code:?}/{collation:?}"
            );
        }
    }
    assert!(!Year.is_string());
    assert!(!Year.is_binary_string("binary"));
    for raw in u8::MIN..=u8::MAX {
        // Includes 0/13/253 and every other byte corresponding to a named
        // type. Unknown variant identity must survive native projection.
        assert!(!Other(raw).is_string());
        assert!(!Other(raw).is_binary_string("binary"));
    }
}

#[cfg(test)]
#[test]
fn field_string_metadata_preserves_lengths_binary_flags_and_named_identity() {
    use NativeStringTypeCode::*;
    assert_eq!(native_enum_set_display_length(Enum, []), 0);
    assert_eq!(native_enum_set_display_length(Enum, [1, 4, 2]), 4);
    assert_eq!(native_enum_set_display_length(Set, []), 0);
    assert_eq!(native_enum_set_display_length(Set, [1, 4, 0]), 7);
    assert_eq!(native_enum_set_display_length(Other(247), [9]), -1);
    for code in [
        VarChar, String, VarString, TinyBlob, MediumBlob, LongBlob, Blob,
    ] {
        assert!(native_field_type_has_charset(code, false), "{code:?}");
        assert!(!native_field_type_has_charset(code, true), "{code:?}");
    }
    for code in [Enum, Set] {
        assert!(!code.is_string());
        assert!(native_field_type_has_charset(code, false));
        assert!(native_field_type_has_charset(code, true));
    }
    for code in [Unspecified, Year, Other(247)] {
        assert!(!native_field_type_has_charset(code, false), "{code:?}");
        assert!(!native_field_type_has_charset(code, true), "{code:?}");
    }
}

#[cfg(test)]
#[test]
fn field_string_policy_preserves_named_unknown_and_restored_data_rules() {
    use NativeStringTypeCode::*;
    for code in [Enum, Bit, Set] {
        assert!(code.is_hybrid(), "{code:?}");
    }
    for raw in u8::MIN..=u8::MAX {
        assert!(!Other(raw).is_hybrid());
        assert!(!Other(raw).is_var_length_type());
    }
    for code in [
        VarChar,
        VarString,
        Json,
        TinyBlob,
        MediumBlob,
        LongBlob,
        Blob,
        VectorFloat32,
    ] {
        assert!(code.is_var_length_type(), "{code:?}");
    }
    assert!(String.is_character_string("utf8mb4_bin"));
    assert!(!String.is_character_string("binary"));
    assert!(!Enum.is_character_string("utf8mb4_bin"));
    assert!(native_need_restored_data(
        String,
        "utf8mb4_general_ci",
        true,
        false
    ));
    assert!(!native_need_restored_data(
        String,
        "utf8mb4_bin",
        true,
        true
    ));
    assert!(native_need_restored_data(
        VarChar,
        "utf8mb4_bin",
        true,
        true
    ));
    assert!(native_need_restored_data(String, "gbk_bin", true, false));
    assert!(!native_need_restored_data(
        String,
        "utf8mb4_0900_bin",
        true,
        false
    ));
    assert!(!native_need_restored_data(
        String,
        "utf8mb4_general_ci",
        false,
        false
    ));
    assert!(!native_need_restored_data(String, "binary", true, false));
}
