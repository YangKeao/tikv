// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Lossless operand framing for the existing nullable byte-identity worker.
//! Tags identify actual native datum representations, never operations. SQL
//! NULL is physical absence and has no tag. No SQL-value normalization occurs.

/// A borrowed view of one non-NULL native datum's actual representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIdentityRef<'a> {
    MinNotNull,
    MaxValue,
    Int(i64),
    UInt(u64),
    Decimal {
        negative: bool,
        scale: u32,
        storage_scale: u32,
        declared_shape: Option<(i64, i64)>,
        coefficient: &'a [u8],
    },
    Real(u64),
    Float32(u64),
    String {
        collation: u8,
        bytes: &'a [u8],
    },
    Bytes(&'a [u8]),
    BinaryLiteral(&'a [u8]),
    Duration {
        nanos: i64,
        fsp: i64,
    },
    Enum {
        collation: u8,
        value: u64,
        name: &'a [u8],
    },
    Bit(&'a [u8]),
    Set {
        collation: u8,
        value: u64,
        name: &'a [u8],
    },
    Time {
        core: u64,
        kind: u8,
        fsp: u8,
    },
    Json {
        type_code: u8,
        bytes: &'a [u8],
    },
    Raw(&'a [u8]),
    Vector(&'a [u8]),
}

/// Framing failures only, not SQL errors or runtime outcome categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIdentityFrameError {
    Invalid,
    Capacity,
}

use NativeIdentityFrameError::{Capacity, Invalid};
use NativeIdentityRef as View;

fn validate_view(value: View<'_>) -> Result<(), NativeIdentityFrameError> {
    match value {
        View::String { collation, .. }
        | View::Enum { collation, .. }
        | View::Set { collation, .. }
            if collation > 15 =>
        {
            Err(Invalid)
        }
        View::Time { kind, .. } if kind > 2 => Err(Invalid),
        View::Vector(bytes) if bytes.len() % 4 != 0 => Err(Invalid),
        _ => Ok(()),
    }
}

/// Encodes an actual non-NULL representation. Header bytes and payload lengths
/// are checked before reserving; opaque tails are copied without
/// interpretation.
pub fn encode_native_identity(
    value: NativeIdentityRef<'_>,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    validate_view(value)?;
    let mut header = [0_u8; 27];
    let (header_len, tail): (usize, &[u8]) = match value {
        View::MinNotNull => {
            header[0] = 1;
            (1, &[])
        }
        View::MaxValue => {
            header[0] = 2;
            (1, &[])
        }
        View::Int(value) => {
            header[0] = 3;
            header[1..9].copy_from_slice(&value.to_le_bytes());
            (9, &[])
        }
        View::UInt(bits) | View::Real(bits) | View::Float32(bits) => {
            header[0] = match value {
                View::UInt(_) => 4,
                View::Real(_) => 6,
                View::Float32(_) => 7,
                _ => unreachable!("the matched scalar has one of three fixed tags"),
            };
            header[1..9].copy_from_slice(&bits.to_le_bytes());
            (9, &[])
        }
        View::Decimal {
            negative,
            scale,
            storage_scale,
            declared_shape,
            coefficient,
        } => {
            header[0] = 5;
            header[1] = u8::from(negative);
            header[2..6].copy_from_slice(&scale.to_le_bytes());
            header[6..10].copy_from_slice(&storage_scale.to_le_bytes());
            header[10] = u8::from(declared_shape.is_some());
            if let Some((precision, scale)) = declared_shape {
                header[11..19].copy_from_slice(&precision.to_le_bytes());
                header[19..27].copy_from_slice(&scale.to_le_bytes());
            }
            (27, coefficient)
        }
        View::String { collation, bytes } => {
            header[0] = 8;
            header[1] = collation;
            (2, bytes)
        }
        View::Bytes(bytes) => {
            header[0] = 9;
            (1, bytes)
        }
        View::BinaryLiteral(bytes) => {
            header[0] = 10;
            (1, bytes)
        }
        View::Duration { nanos, fsp } => {
            header[0] = 11;
            header[1..9].copy_from_slice(&nanos.to_le_bytes());
            header[9..17].copy_from_slice(&fsp.to_le_bytes());
            (17, &[])
        }
        View::Enum {
            collation,
            value,
            name,
        } => {
            header[0] = 12;
            header[1] = collation;
            header[2..10].copy_from_slice(&value.to_le_bytes());
            (10, name)
        }
        View::Bit(bytes) => {
            header[0] = 13;
            (1, bytes)
        }
        View::Set {
            collation,
            value,
            name,
        } => {
            header[0] = 14;
            header[1] = collation;
            header[2..10].copy_from_slice(&value.to_le_bytes());
            (10, name)
        }
        View::Time { core, kind, fsp } => {
            header[0] = 15;
            header[1..9].copy_from_slice(&core.to_le_bytes());
            header[9] = kind;
            header[10] = fsp;
            (11, &[])
        }
        View::Json { type_code, bytes } => {
            header[0] = 16;
            header[1] = type_code;
            (2, bytes)
        }
        View::Raw(bytes) => {
            header[0] = 17;
            (1, bytes)
        }
        View::Vector(bytes) => {
            header[0] = 18;
            (1, bytes)
        }
    };
    let length = header_len.checked_add(tail.len()).ok_or(Capacity)?;
    let mut frame = Vec::new();
    frame.try_reserve_exact(length).map_err(|_| Capacity)?;
    frame.extend_from_slice(&header[..header_len]);
    frame.extend_from_slice(tail);
    Ok(frame)
}

fn fixed<const N: usize>(bytes: &[u8]) -> Result<&[u8; N], NativeIdentityFrameError> {
    bytes.try_into().map_err(|_| Invalid)
}

fn header<const N: usize>(bytes: &[u8]) -> Result<(&[u8; N], &[u8]), NativeIdentityFrameError> {
    let head = bytes.get(..N).ok_or(Invalid)?;
    Ok((fixed(head)?, &bytes[N..]))
}

/// Decodes without allocation, retaining every opaque tail as a frame borrow.
/// Only physical widths, representation tags, and canonical absent metadata
/// are checked; time, decimal, floating, and JSON semantics are not validated.
pub fn decode_native_identity(
    frame: &[u8],
) -> Result<NativeIdentityRef<'_>, NativeIdentityFrameError> {
    let (&tag, body) = frame.split_first().ok_or(Invalid)?;
    let value = match tag {
        1 if body.is_empty() => View::MinNotNull,
        2 if body.is_empty() => View::MaxValue,
        3 => View::Int(i64::from_le_bytes(*fixed(body)?)),
        4 => View::UInt(u64::from_le_bytes(*fixed(body)?)),
        5 => {
            let (head, coefficient) = header::<26>(body)?;
            if head[0] > 1 || head[9] > 1 {
                return Err(Invalid);
            }
            let precision = i64::from_le_bytes(*fixed(&head[10..18])?);
            let shape_scale = i64::from_le_bytes(*fixed(&head[18..26])?);
            let declared_shape = if head[9] == 1 {
                Some((precision, shape_scale))
            } else {
                if precision != 0 || shape_scale != 0 {
                    return Err(Invalid);
                }
                None
            };
            View::Decimal {
                negative: head[0] != 0,
                scale: u32::from_le_bytes(*fixed(&head[1..5])?),
                storage_scale: u32::from_le_bytes(*fixed(&head[5..9])?),
                declared_shape,
                coefficient,
            }
        }
        6 => View::Real(u64::from_le_bytes(*fixed(body)?)),
        7 => View::Float32(u64::from_le_bytes(*fixed(body)?)),
        8 => {
            let (&collation, bytes) = body.split_first().ok_or(Invalid)?;
            View::String { collation, bytes }
        }
        9 => View::Bytes(body),
        10 => View::BinaryLiteral(body),
        11 => {
            let body = fixed::<16>(body)?;
            View::Duration {
                nanos: i64::from_le_bytes(*fixed(&body[..8])?),
                fsp: i64::from_le_bytes(*fixed(&body[8..])?),
            }
        }
        12 | 14 => {
            let (head, name) = header::<9>(body)?;
            let value = u64::from_le_bytes(*fixed(&head[1..])?);
            if tag == 12 {
                View::Enum {
                    collation: head[0],
                    value,
                    name,
                }
            } else {
                View::Set {
                    collation: head[0],
                    value,
                    name,
                }
            }
        }
        13 => View::Bit(body),
        15 => {
            let body = fixed::<10>(body)?;
            View::Time {
                core: u64::from_le_bytes(*fixed(&body[..8])?),
                kind: body[8],
                fsp: body[9],
            }
        }
        16 => {
            let (&type_code, bytes) = body.split_first().ok_or(Invalid)?;
            View::Json { type_code, bytes }
        }
        17 => View::Raw(body),
        18 => View::Vector(body),
        _ => return Err(Invalid),
    };
    validate_view(value)?;
    Ok(value)
}

/// SQL NULL is physical byte absence; all non-NULL arguments require one frame.
pub fn native_identity_args_valid(frame: Option<&[u8]>) -> bool {
    frame.is_none_or(|frame| decode_native_identity(frame).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_frames_preserve_all_kinds_and_reject_only_bad_shapes() {
        let cases = [
            (View::MinNotNull, vec![1]),
            (View::MaxValue, vec![2]),
            (
                View::Int(-1),
                vec![3, 255, 255, 255, 255, 255, 255, 255, 255],
            ),
            (
                View::UInt(u64::MAX),
                vec![4, 255, 255, 255, 255, 255, 255, 255, 255],
            ),
            (
                View::Decimal {
                    negative: true,
                    scale: 2,
                    storage_scale: 9,
                    declared_shape: Some((-1, 7)),
                    coefficient: &[255, 0, b'x'],
                },
                vec![
                    5, 1, 2, 0, 0, 0, 9, 0, 0, 0, 1, 255, 255, 255, 255, 255, 255, 255, 255, 7, 0,
                    0, 0, 0, 0, 0, 0, 255, 0, b'x',
                ],
            ),
            (
                View::Real(0x7ff8_0000_0000_0042),
                vec![6, 0x42, 0, 0, 0, 0, 0, 0xf8, 0x7f],
            ),
            (
                View::Float32(0x8000_0000_0000_0000),
                vec![7, 0, 0, 0, 0, 0, 0, 0, 128],
            ),
            (
                View::String {
                    collation: 11,
                    bytes: &[255, 0],
                },
                vec![8, 11, 255, 0],
            ),
            (View::Bytes(&[]), vec![9]),
            (View::BinaryLiteral(&[0, 255]), vec![10, 0, 255]),
            (
                View::Duration {
                    nanos: -1,
                    fsp: i64::MIN,
                },
                vec![
                    11, 255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 128,
                ],
            ),
            (
                View::Enum {
                    collation: 15,
                    value: 2,
                    name: &[255],
                },
                vec![12, 15, 2, 0, 0, 0, 0, 0, 0, 0, 255],
            ),
            (View::Bit(&[0, 0, 255]), vec![13, 0, 0, 255]),
            (
                View::Set {
                    collation: 0,
                    value: 1,
                    name: &[0, 255],
                },
                vec![14, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 255],
            ),
            (
                View::Time {
                    core: u64::MAX,
                    kind: 2,
                    fsp: 255,
                },
                vec![15, 255, 255, 255, 255, 255, 255, 255, 255, 2, 255],
            ),
            (
                View::Json {
                    type_code: 255,
                    bytes: &[0, 255],
                },
                vec![16, 255, 0, 255],
            ),
            (View::Raw(&[255]), vec![17, 255]),
            (
                View::Vector(&[1, 0, 128, 127, 0, 0, 0, 128]),
                vec![18, 1, 0, 128, 127, 0, 0, 0, 128],
            ),
        ];
        for (view, expected) in cases {
            let encoded = encode_native_identity(view).unwrap();
            assert_eq!(encoded, expected);
            assert_eq!(decode_native_identity(&encoded), Ok(view));
            assert!(native_identity_args_valid(Some(&encoded)));
        }
        let absent_shape = View::Decimal {
            negative: true,
            scale: u32::MAX,
            storage_scale: 0,
            declared_shape: None,
            coefficient: &[],
        };
        let absent_frame = encode_native_identity(absent_shape).unwrap();
        assert_eq!(absent_frame.len(), 27);
        assert_eq!(&absent_frame[10..27], &[0; 17]);
        assert_eq!(decode_native_identity(&absent_frame), Ok(absent_shape));
        assert_eq!(encode_native_identity(View::Vector(&[])).unwrap(), vec![18]);
        assert_eq!(
            decode_native_identity(&[16, 255]),
            Ok(View::Json {
                type_code: 255,
                bytes: &[]
            })
        );
        let bytes_frame = [9, 255, 0, 1];
        let View::Bytes(tail) = decode_native_identity(&bytes_frame).unwrap() else {
            panic!("expected borrowed byte payload");
        };
        assert_eq!(tail.as_ptr(), bytes_frame[1..].as_ptr());
        assert!(native_identity_args_valid(None));

        for frame in [
            vec![],
            vec![0],
            vec![19],
            vec![1, 0],
            vec![2, 0],
            vec![3; 8],
            vec![4; 10],
            vec![6; 8],
            vec![7; 10],
            vec![8],
            vec![8, 16],
            vec![11; 16],
            vec![11; 18],
            vec![12; 9],
            vec![14; 9],
            vec![15; 10],
            vec![15; 12],
            vec![15, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0],
            vec![16],
            vec![18, 0],
            vec![5; 26],
        ] {
            assert_eq!(decode_native_identity(&frame), Err(Invalid), "{frame:?}");
            assert!(!native_identity_args_valid(Some(&frame)));
        }
        for (index, bad) in [(1, 2), (10, 2), (11, 1), (19, 1)] {
            let mut invalid = absent_frame.clone();
            invalid[index] = bad;
            assert_eq!(decode_native_identity(&invalid), Err(Invalid));
        }
        for invalid in [
            View::String {
                collation: 16,
                bytes: &[],
            },
            View::Enum {
                collation: 255,
                value: 0,
                name: &[],
            },
            View::Set {
                collation: 16,
                value: 0,
                name: &[],
            },
            View::Time {
                core: 0,
                kind: 3,
                fsp: 0,
            },
            View::Vector(&[0, 0, 0]),
        ] {
            assert_eq!(encode_native_identity(invalid), Err(Invalid));
        }
    }
}
