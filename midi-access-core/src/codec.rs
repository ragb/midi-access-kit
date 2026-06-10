//! Shared codec primitives used by device area models.
//!
//! MIDI devices store parameters in a handful of recurring shapes; this module
//! collects the conversions so each device crate doesn't re-implement them:
//!
//! - **single byte** — a plain `0..=n` value or `0/1` boolean ([`bool_byte`],
//!   [`ranged`]).
//! - **centred signed** — `value = byte − center` ([`signed_center`] /
//!   [`to_signed_center`]); e.g. `center = 0x40` for `−12..=+12`.
//! - **14-bit split** — `hi` = bits 13..7, `lo` = bits 6..0 ([`read_u14`] /
//!   [`write_u14`]).
//! - **16-bit nibble-packed** — four bytes, one nibble each, MSN first
//!   ([`read_u16_nibbles`] / [`write_u16_nibbles`]).
//! - **ASCII** — space-padded fixed-width names ([`read_ascii`] / [`write_ascii`]).
//! - **reserved bytes** — capture/re-apply undocumented payload bytes so a
//!   decode→encode round-trips byte-exact ([`RawByte`], [`capture_reserved`],
//!   [`apply_reserved`]).
//! - **checksum** — the Roland/Yamaha "sum to 0 mod 128" rule ([`checksum`]).
//! - **framing** — split a buffer into SysEx frames ([`split_sysex`]).
//!
//! The [`byte_enum!`] macro defines a C-like enum mapping to/from one wire byte.

use thiserror::Error;

/// Errors from decoding/encoding typed area models over their byte buffers.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("expected at least {expected} bytes, got {actual}")]
    WrongLength { expected: usize, actual: usize },

    #[error("invalid {field} value {value:#04x} (valid: {valid})")]
    InvalidValue {
        field: &'static str,
        value: u8,
        valid: &'static str,
    },

    #[error("{field} out of range: {value} (valid: {valid})")]
    OutOfRange {
        field: &'static str,
        value: i32,
        valid: &'static str,
    },

    #[error("{field}: invalid ASCII string ({reason})")]
    BadString {
        field: &'static str,
        reason: &'static str,
    },

    #[error("YAML: {0}")]
    Yaml(String),

    #[error("missing required block: {0}")]
    MissingBlock(&'static str),
}

/// Define a C-like enum that maps to/from a single wire byte, deriving the
/// `serde` / `tsify` / `schemars` attributes a device's typed model wants.
#[macro_export]
macro_rules! byte_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $value:expr),+ $(,)? } valid = $valid:expr) => {
        $(#[$meta])*
        #[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
        #[cfg_attr(feature = "tsify", tsify(into_wasm_abi, from_wasm_abi))]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            const FIELD: &'static str = stringify!($name);

            /// Decode the wire byte.
            pub fn from_byte(b: u8) -> Result<Self, $crate::codec::CodecError> {
                match b {
                    $($value => Ok(Self::$variant),)+
                    _ => Err($crate::codec::CodecError::InvalidValue {
                        field: Self::FIELD, value: b, valid: $valid,
                    }),
                }
            }

            /// Encode to the wire byte.
            pub fn to_byte(self) -> u8 {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }
    };
}

/// `0 => false`, `1 => true`, anything else is an error.
pub fn bool_byte(b: u8, field: &'static str) -> Result<bool, CodecError> {
    match b {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CodecError::InvalidValue {
            field,
            value: b,
            valid: "0=off, 1=on",
        }),
    }
}

/// Validate that `b` lies in `lo..=hi` and return it unchanged.
pub fn ranged(b: u8, lo: u8, hi: u8, field: &'static str) -> Result<u8, CodecError> {
    if (lo..=hi).contains(&b) {
        Ok(b)
    } else {
        Err(CodecError::OutOfRange {
            field,
            value: b as i32,
            valid: leak_range(lo as i32, hi as i32),
        })
    }
}

/// Decode a centred-signed byte: actual value = `b − center`, validated to
/// `lo..=hi` (e.g. `center = 0x40` for `−12..=+12`).
pub fn signed_center(
    b: u8,
    center: u8,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<i8, CodecError> {
    let v = b as i32 - center as i32;
    if (lo..=hi).contains(&v) {
        Ok(v as i8)
    } else {
        Err(CodecError::OutOfRange {
            field,
            value: v,
            valid: leak_range(lo, hi),
        })
    }
}

/// Encode a centred-signed value back to its wire byte (`v + center`), validated.
pub fn to_signed_center(
    v: i8,
    center: u8,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<u8, CodecError> {
    let vi = v as i32;
    if (lo..=hi).contains(&vi) {
        Ok((vi + center as i32) as u8)
    } else {
        Err(CodecError::OutOfRange {
            field,
            value: vi,
            valid: leak_range(lo, hi),
        })
    }
}

/// Decode a 14-bit value: `hi` carries bits 13..7, `lo` carries bits 6..0.
pub fn read_u14(hi: u8, lo: u8) -> u16 {
    ((hi as u16 & 0x7F) << 7) | (lo as u16 & 0x7F)
}

/// Encode a 14-bit value into its `(hi, lo)` byte pair.
pub fn write_u14(v: u16) -> (u8, u8) {
    (((v >> 7) & 0x7F) as u8, (v & 0x7F) as u8)
}

/// Decode a nibble-packed 16-bit value: four bytes, one nibble each, MSN first.
pub fn read_u16_nibbles(b: &[u8]) -> u16 {
    ((b[0] as u16 & 0x0F) << 12)
        | ((b[1] as u16 & 0x0F) << 8)
        | ((b[2] as u16 & 0x0F) << 4)
        | (b[3] as u16 & 0x0F)
}

/// Encode a 16-bit value into four nibble-carrying bytes, MSN first.
pub fn write_u16_nibbles(v: u16) -> [u8; 4] {
    [
        ((v >> 12) & 0x0F) as u8,
        ((v >> 8) & 0x0F) as u8,
        ((v >> 4) & 0x0F) as u8,
        (v & 0x0F) as u8,
    ]
}

/// Decode an ASCII field, trimming trailing spaces and NULs.
pub fn read_ascii(bytes: &[u8], field: &'static str) -> Result<String, CodecError> {
    if bytes.iter().any(|&b| b >= 0x80) {
        return Err(CodecError::BadString {
            field,
            reason: "byte >= 0x80",
        });
    }
    let s: String = bytes.iter().map(|&b| b as char).collect();
    Ok(s.trim_end_matches([' ', '\0']).to_string())
}

/// Encode a name into a fixed-width ASCII field, space-padded and truncated.
pub fn write_ascii(s: &str, width: usize, field: &'static str) -> Result<Vec<u8>, CodecError> {
    if !s.is_ascii() {
        return Err(CodecError::BadString {
            field,
            reason: "non-ASCII character",
        });
    }
    let mut out = vec![0x20u8; width];
    for (slot, b) in out.iter_mut().zip(s.bytes()) {
        *slot = b;
    }
    Ok(out)
}

/// A single raw byte the typed model doesn't interpret — a documented-reserved
/// offset the device populates, or a trailing byte added by newer firmware.
/// Captured so every area round-trips byte-exact even where the manual is silent.
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "tsify", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RawByte {
    pub offset: u16,
    pub value: u8,
}

/// Collect the offsets where the device payload differs from what the typed
/// fields alone produce. Applied via [`apply_reserved`] to make
/// `from_bytes`→`to_bytes` byte-exact.
pub fn capture_reserved(input: &[u8], typed_only: &[u8]) -> Vec<RawByte> {
    let n = input.len().max(typed_only.len());
    (0..n)
        .filter_map(|i| {
            let inp = input.get(i).copied().unwrap_or(0);
            let typ = typed_only.get(i).copied().unwrap_or(0);
            (inp != typ).then_some(RawByte {
                offset: i as u16,
                value: inp,
            })
        })
        .collect()
}

/// Overlay captured reserved bytes onto an encode buffer, growing it if a
/// trailing offset lies past the current end.
pub fn apply_reserved(buf: &mut Vec<u8>, reserved: &[RawByte]) {
    for r in reserved {
        let i = r.offset as usize;
        if i >= buf.len() {
            buf.resize(i + 1, 0);
        }
        buf[i] = r.value;
    }
}

/// The Roland/Yamaha bulk checksum: the value that makes the lower 7 bits of the
/// sum of `region` plus the checksum equal zero, i.e. `(sum + cc) & 0x7F == 0`.
///
/// The *formula* is shared; vendors differ only in which bytes are summed — pass
/// the appropriate region. Yamaha sums Model-ID + Address + Data; Roland sums
/// Address + Data. The caller selects the slice.
pub fn checksum(region: &[u8]) -> u8 {
    let sum: u32 = region.iter().map(|&b| b as u32).sum();
    ((0x80 - (sum % 0x80)) % 0x80) as u8
}

/// Split a buffer into complete SysEx frames (each `F0 … F7`). Bytes outside a
/// frame are ignored. Useful for sending a multi-frame request and for decoding
/// a collected multi-block dump stream.
pub fn split_sysex(bytes: &[u8]) -> Vec<Vec<u8>> {
    const START: u8 = 0xF0;
    const END: u8 = 0xF7;
    let mut out = Vec::new();
    let mut cur: Option<Vec<u8>> = None;
    for &b in bytes {
        if b == START {
            cur = Some(vec![START]);
        } else if let Some(buf) = cur.as_mut() {
            buf.push(b);
            if b == END {
                out.push(cur.take().unwrap());
            }
        }
    }
    out
}

/// Format a `lo..=hi` range into a `'static` string for error messages.
fn leak_range(lo: i32, hi: i32) -> &'static str {
    Box::leak(format!("{lo}..={hi}").into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    byte_enum! {
        /// doc
        Mode { Off = 0x00, On = 0x01, Auto = 0x02 } valid = "0=off,1=on,2=auto"
    }

    #[test]
    fn byte_enum_round_trips() {
        assert_eq!(Mode::from_byte(0x02).unwrap(), Mode::Auto);
        assert_eq!(Mode::Auto.to_byte(), 0x02);
        assert!(Mode::from_byte(0x09).is_err());
    }

    #[test]
    fn numeric_shapes_round_trip() {
        for v in [0u16, 1, 127, 128, 16383] {
            let (hi, lo) = write_u14(v);
            assert_eq!(read_u14(hi, lo), v);
        }
        for v in [0u16, 0x0400, 0x1234, 0xFFFF] {
            assert_eq!(read_u16_nibbles(&write_u16_nibbles(v)), v);
        }
        assert_eq!(signed_center(0x34, 0x40, -12, 12, "t").unwrap(), -12);
        assert_eq!(to_signed_center(-12, 0x40, -12, 12, "t").unwrap(), 0x34);
        assert_eq!(read_ascii(b"Init      ", "n").unwrap(), "Init");
        assert_eq!(write_ascii("Init", 6, "n").unwrap(), b"Init  ");
    }

    #[test]
    fn checksum_makes_sum_zero_mod_128() {
        let region = [0x05, 0x10, 0x0B, 0x20, 0x00, 0x00, 0x7F, 0x01];
        let cc = checksum(&region);
        let total: u32 = region.iter().map(|&b| b as u32).sum::<u32>() + cc as u32;
        assert_eq!(total % 0x80, 0);
    }

    #[test]
    fn reserved_round_trips() {
        let input = [0x00, 0x7F, 0x00, 0x55];
        let typed = [0x00, 0x00, 0x00];
        let res = capture_reserved(&input, &typed);
        let mut buf = vec![0x00, 0x00, 0x00];
        apply_reserved(&mut buf, &res);
        assert_eq!(buf, input);
    }

    #[test]
    fn splits_frames() {
        let buf = [0xF0, 0x01, 0xF7, 0x99, 0xF0, 0x02, 0x03, 0xF7];
        let frames = split_sysex(&buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], [0xF0, 0x01, 0xF7]);
        assert_eq!(frames[1], [0xF0, 0x02, 0x03, 0xF7]);
    }
}
