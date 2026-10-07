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
    Other(u8),
}

impl NativeStringTypeCode {
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
