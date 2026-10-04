// Copyright 2026 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The session's `time_zone` as a value the temporal code can convert with:
//! the Rust shape of the `*time.Location` Go threads out of
//! `SessionVars.Location()` (`pkg/sessionctx/variable/session.go`) and into
//! `types.Context` (`pkg/types/context.go`).
//!
//! # Why it lives here and implements `chrono::TimeZone`
//!
//! Go has exactly ONE zone type. `time.FixedZone("+08:00", 8*3600)` and
//! `time.LoadLocation("America/Los_Angeles")` are both `*time.Location`, so
//! every function that takes a zone -- `Time.ConvertTimeZone`,
//! `tablecodec.flatten`/`unflatten`, `codec.EncodeKey`, `ParseTime` --
//! takes the same parameter and needs no case analysis.
//!
//! Rust's `chrono` splits the two: a fixed offset is `FixedOffset` and an
//! IANA zone is `chrono_tz::Tz`, and they are distinct types. Matching on
//! the pair at each call site would put a two-arm `match` in front of every
//! conversion in the engine -- and, worse, would let a call site silently
//! handle only one arm. Implementing [`TimeZone`] for the union once
//! restores Go's shape: there is one zone type, it goes anywhere a zone
//! goes, and the DST-aware arm cannot be forgotten.
//!
//! The type sits in `tidb-datatype` rather than beside the session because
//! the storage codecs (`tidb-codec`, `tidb-tablecodec`) are the code that
//! needs it most and they are BELOW the session in the crate graph, exactly
//! as Go's `tablecodec` is below `sessionctx`.

use chrono::{FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone};
use chrono_tz_native::Tz;

/// The session `time_zone`: a fixed offset (Go `time.FixedZone`) or a named
/// IANA zone (Go `time.LoadLocation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTimeZone {
    /// Go `time.Local`, used when TiDB's `SystemLocation` cannot be resolved
    /// to an IANA name. Unlike a fixed snapshot, this retains historical and
    /// future daylight-saving transitions.
    Local,
    /// A fixed offset east of UTC with its display name.
    Fixed {
        /// The zone's display name.
        name: String,
        /// Seconds east of UTC.
        offset_secs: i32,
    },
    /// A named IANA zone.
    Named(Tz),
}

impl SessionTimeZone {
    /// UTC, the zone stored `TIMESTAMP` values are held in.
    #[must_use]
    pub fn utc() -> Self {
        Self::Fixed {
            name: "UTC".to_owned(),
            offset_secs: 0,
        }
    }

    /// Go `timeutil.Zone`: the `(TimeZoneName, TimeZoneOffset)` pair a
    /// coprocessor request carries, which is how the REGION is told which
    /// zone to evaluate a pushed condition in.
    ///
    /// The two halves are not redundant. TiKV prefers the NAME when it is
    /// non-empty, because only a named zone carries daylight saving, and
    /// falls back to the offset when it is empty. Go therefore sends an empty
    /// name for a fixed offset: `timeutil.ParseTimeZone`'s `+HH:MM` branch
    /// builds `time.FixedZone("", ofst)`, whose `String()` -- the very value
    /// `Zone` returns -- is the empty string. Sending `"+08:00"` as a zone
    /// NAME instead would be a name no zone database can load.
    ///
    /// A named zone's offset is a property of the INSTANT (daylight saving),
    /// so Go takes it at `time.Now()` and so does this.
    #[must_use]
    pub fn dag_zone(&self) -> (String, i64) {
        match self {
            Self::Local => (
                "System".to_owned(),
                i64::from(Local::now().offset().fix().local_minus_utc()),
            ),
            // Go's `SystemLocation()` keeps its own name, which `Zone`
            // rewrites from `"Local"` to `"System"`; every other fixed zone
            // this session builds is the anonymous offset one.
            Self::Fixed { name, offset_secs } => (
                if name.starts_with(['+', '-']) {
                    String::new()
                } else {
                    name.clone()
                },
                i64::from(*offset_secs),
            ),
            Self::Named(zone) => {
                let now = chrono::Utc::now().naive_utc();
                let offset = zone.offset_from_utc_datetime(&now).fix().local_minus_utc();
                (zone.name().to_owned(), i64::from(offset))
            }
        }
    }

    /// Whether this zone is UTC, which is what lets the storage codecs skip
    /// the conversion exactly where Go's `loc != time.UTC` guard does.
    #[must_use]
    pub fn is_utc(&self) -> bool {
        match self {
            Self::Local => false,
            Self::Fixed { name, offset_secs } => name == "UTC" && *offset_secs == 0,
            Self::Named(zone) => *zone == Tz::UTC,
        }
    }
}

impl Default for SessionTimeZone {
    fn default() -> Self {
        Self::utc()
    }
}

/// The resolved offset of a [`SessionTimeZone`] at one instant.
///
/// `chrono` requires a zone's offset to be its own type; this carries the
/// zone back so `Offset::from_offset` can reconstruct it, which is what lets
/// a `DateTime<SessionTimeZone>` be re-projected into another zone.
#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub struct SessionTimeZoneOffset {
    fixed: FixedOffset,
}

impl Offset for SessionTimeZoneOffset {
    fn fix(&self) -> FixedOffset {
        self.fixed
    }
}

impl std::fmt::Display for SessionTimeZoneOffset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fixed.fmt(formatter)
    }
}

/// Lifts a `LocalResult` over one zone's offset into this zone's offset,
/// keeping the ambiguity/gap verdict intact -- a local time that does not
/// exist in a named zone must stay `LocalResult::None` here, because that is
/// the DST-gap error Go reports for a `TIMESTAMP` written during a spring
/// forward.
fn lift<O: Offset>(result: LocalResult<O>) -> LocalResult<SessionTimeZoneOffset> {
    match result {
        LocalResult::None => LocalResult::None,
        LocalResult::Single(offset) => LocalResult::Single(SessionTimeZoneOffset {
            fixed: offset.fix(),
        }),
        LocalResult::Ambiguous(earliest, latest) => LocalResult::Ambiguous(
            SessionTimeZoneOffset {
                fixed: earliest.fix(),
            },
            SessionTimeZoneOffset {
                fixed: latest.fix(),
            },
        ),
    }
}

impl TimeZone for SessionTimeZone {
    type Offset = SessionTimeZoneOffset;

    fn from_offset(offset: &Self::Offset) -> Self {
        Self::Fixed {
            name: offset.fixed.to_string(),
            offset_secs: offset.fixed.local_minus_utc(),
        }
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
        match self {
            Self::Local => lift(Local.offset_from_local_date(local)),
            Self::Fixed { offset_secs, .. } => {
                lift(fixed(*offset_secs).offset_from_local_date(local))
            }
            Self::Named(zone) => lift(zone.offset_from_local_date(local)),
        }
    }

    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
        match self {
            Self::Local => lift(Local.offset_from_local_datetime(local)),
            Self::Fixed { offset_secs, .. } => {
                lift(fixed(*offset_secs).offset_from_local_datetime(local))
            }
            Self::Named(zone) => lift(zone.offset_from_local_datetime(local)),
        }
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
        match self {
            Self::Local => SessionTimeZoneOffset {
                fixed: Local.offset_from_utc_date(utc).fix(),
            },
            Self::Fixed { offset_secs, .. } => SessionTimeZoneOffset {
                fixed: fixed(*offset_secs).offset_from_utc_date(utc).fix(),
            },
            Self::Named(zone) => SessionTimeZoneOffset {
                fixed: zone.offset_from_utc_date(utc).fix(),
            },
        }
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
        match self {
            Self::Local => SessionTimeZoneOffset {
                fixed: Local.offset_from_utc_datetime(utc).fix(),
            },
            Self::Fixed { offset_secs, .. } => SessionTimeZoneOffset {
                fixed: fixed(*offset_secs).offset_from_utc_datetime(utc).fix(),
            },
            Self::Named(zone) => SessionTimeZoneOffset {
                fixed: zone.offset_from_utc_datetime(utc).fix(),
            },
        }
    }
}

/// The `FixedOffset` for `offset_secs`, clamped into `chrono`'s representable
/// range. MySQL's own `time_zone` grammar caps the offset far inside it
/// (`-14:00`..`+14:00`), so the clamp is unreachable from SQL.
fn fixed(offset_secs: i32) -> FixedOffset {
    FixedOffset::east_opt(offset_secs.clamp(-86_399, 86_399)).expect("clamped into range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_zone_preserves_raw_metadata_offset_identity_and_transition_verdicts() {
        let utc = NaiveDate::from_ymd_opt(2021, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        for (raw, converted) in [(i32::MIN, -86_399), (0, 0), (i32::MAX, 86_399)] {
            for (name, dag_name) in [
                ("+raw", ""),
                ("-raw", ""),
                ("Local", "Local"),
                ("kept", "kept"),
            ] {
                let zone = SessionTimeZone::Fixed {
                    name: name.to_owned(),
                    offset_secs: raw,
                };
                assert_eq!(zone.dag_zone(), (dag_name.to_owned(), i64::from(raw)));
                assert_eq!(
                    zone.offset_from_utc_datetime(&utc).fix().local_minus_utc(),
                    converted
                );
                assert_eq!(
                    zone.offset_from_utc_date(&utc.date())
                        .fix()
                        .local_minus_utc(),
                    converted
                );
                assert_eq!(
                    zone.offset_from_local_datetime(&utc)
                        .single()
                        .unwrap()
                        .fix()
                        .local_minus_utc(),
                    converted
                );
                assert!(!zone.is_utc());
            }
        }
        assert_eq!(SessionTimeZone::default(), SessionTimeZone::utc());
        assert!(SessionTimeZone::utc().is_utc());
        assert!(SessionTimeZone::Named(Tz::UTC).is_utc());
        assert!(!SessionTimeZone::Local.is_utc());
        for (name, offset_secs) in [("+00:00", 0), ("GMT", 0), ("UTC", 1)] {
            assert!(
                !SessionTimeZone::Fixed {
                    name: name.to_owned(),
                    offset_secs
                }
                .is_utc()
            );
        }
        let zone = SessionTimeZone::Named(chrono_tz_native::America::Los_Angeles);
        let offset = zone.offset_from_utc_datetime(&utc);
        assert_eq!(
            SessionTimeZone::from_offset(&offset),
            SessionTimeZone::Fixed {
                name: "-08:00".to_owned(),
                offset_secs: -28_800,
            }
        );
        assert_eq!(offset.to_string(), "-08:00");
        let zero = SessionTimeZone::utc().offset_from_utc_datetime(&utc);
        let rebuilt = SessionTimeZone::from_offset(&zero);
        assert_eq!(
            rebuilt,
            SessionTimeZone::Fixed {
                name: "+00:00".to_owned(),
                offset_secs: 0
            }
        );
        assert!(!rebuilt.is_utc());
        let gap = NaiveDate::from_ymd_opt(2021, 3, 14)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert!(matches!(
            zone.offset_from_local_datetime(&gap),
            LocalResult::None
        ));
        let fold = NaiveDate::from_ymd_opt(2021, 11, 7)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let LocalResult::Ambiguous(first, second) = zone.offset_from_local_datetime(&fold) else {
            panic!("the repeated wall clock must retain both ordered offsets");
        };
        assert_eq!(
            (
                first.fix().local_minus_utc(),
                second.fix().local_minus_utc()
            ),
            (-25_200, -28_800)
        );
    }
}
