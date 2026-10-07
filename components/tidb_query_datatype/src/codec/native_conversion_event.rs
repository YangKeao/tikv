// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Numeric conversion outcomes and event precedence, without diagnostic
//! delivery.

/// Retains the converted value even when its conversion also produced an error.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeConvertedOutcome<T, E> {
    pub value: T,
    pub error: Option<E>,
}

/// Moves the original value and error without applying warning or error policy.
pub fn native_numeric_outcome<T, E>(result: Result<T, (T, E)>) -> NativeConvertedOutcome<T, E> {
    match result {
        Ok(value) => NativeConvertedOutcome { value, error: None },
        Err((value, error)) => NativeConvertedOutcome {
            value,
            error: Some(error),
        },
    }
}

/// Identifies an existing event; it neither copies nor constructs a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEventSource {
    None,
    First,
    Second,
}

/// A later event takes precedence when one is present.
pub const fn native_prefer_event_source(
    first_present: bool,
    second_present: bool,
) -> NativeEventSource {
    match (first_present, second_present) {
        (_, true) => NativeEventSource::Second,
        (true, false) => NativeEventSource::First,
        (false, false) => NativeEventSource::None,
    }
}

/// The distinction needed to select between parsing and bounding events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeConversionEventKind {
    Truncated,
    Other,
}

/// Nonfatal parsed truncation yields to a bounding event; other parsed events
/// retain precedence. Selection alone does not deliver or suppress an event.
pub const fn native_numeric_event_source(
    parsed: Option<NativeConversionEventKind>,
    bounded_present: bool,
    truncation_nonfatal: bool,
) -> NativeEventSource {
    match parsed {
        Some(NativeConversionEventKind::Truncated) if truncation_nonfatal => {
            native_prefer_event_source(true, bounded_present)
        }
        Some(_) => NativeEventSource::First,
        None => native_prefer_event_source(false, bounded_present),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    #[test]
    fn event_selection_exhaustive_truth_tables() {
        use NativeConversionEventKind::{Other, Truncated};
        use NativeEventSource::{First, None as NoEvent, Second};

        for (first, second, expected) in [
            (false, false, NoEvent),
            (false, true, Second),
            (true, false, First),
            (true, true, Second),
        ] {
            assert_eq!(native_prefer_event_source(first, second), expected);
        }
        for (parsed, bounded, nonfatal, expected) in [
            (None, false, false, NoEvent),
            (None, false, true, NoEvent),
            (None, true, false, Second),
            (None, true, true, Second),
            (Some(Truncated), false, false, First),
            (Some(Truncated), false, true, First),
            (Some(Truncated), true, false, First),
            (Some(Truncated), true, true, Second),
            (Some(Other), false, false, First),
            (Some(Other), false, true, First),
            (Some(Other), true, false, First),
            (Some(Other), true, true, First),
        ] {
            assert_eq!(
                native_numeric_event_source(parsed, bounded, nonfatal),
                expected,
                "parsed={parsed:?}, bounded={bounded}, nonfatal={nonfatal}",
            );
        }
        const PREFERRED: NativeEventSource = native_prefer_event_source(true, true);
        const NUMERIC: NativeEventSource = native_numeric_event_source(Some(Other), true, true);
        assert_eq!(PREFERRED, Second);
        assert_eq!(NUMERIC, First);
    }

    #[test]
    fn numeric_outcome_preserves_owned_value_and_error_without_early_drop() {
        struct ErrorIdentity {
            message: String,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for ErrorIdentity {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }

        let success = String::from("unchanged success");
        let success_ptr = success.as_ptr();
        let success_capacity = success.capacity();
        let outcome = native_numeric_outcome::<String, ErrorIdentity>(Ok(success));
        assert_eq!(outcome.value, "unchanged success");
        assert_eq!(outcome.value.as_ptr(), success_ptr);
        assert_eq!(outcome.value.capacity(), success_capacity);
        assert!(outcome.error.is_none());
        drop(outcome);

        let drops = Rc::new(Cell::new(0));
        let value = String::from("original fallback");
        let value_ptr = value.as_ptr();
        let value_capacity = value.capacity();
        let error = ErrorIdentity {
            message: String::from("original error"),
            drops: Rc::clone(&drops),
        };
        let error_ptr = error.message.as_ptr();
        let outcome = native_numeric_outcome(Err((value, error)));
        assert_eq!(outcome.value, "original fallback");
        assert_eq!(outcome.value.as_ptr(), value_ptr);
        assert_eq!(outcome.value.capacity(), value_capacity);
        let retained_error = outcome.error.as_ref().expect("the original error survives");
        assert_eq!(retained_error.message, "original error");
        assert_eq!(retained_error.message.as_ptr(), error_ptr);
        assert!(Rc::ptr_eq(&retained_error.drops, &drops));
        assert_eq!(drops.get(), 0);
        drop(outcome);
        assert_eq!(drops.get(), 1);
    }
}
