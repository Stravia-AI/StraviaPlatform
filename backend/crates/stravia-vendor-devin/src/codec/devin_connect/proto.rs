//! Minimal protobuf wire primitives for the Devin Connect codec.
//!
//! Field layouts were recovered from the reference implementation
//! (`dwgx/WindsurfAPI`, `src/devin-connect.js` + `src/proto.js`) and are
//! documented next to each encoder in `request.rs` / `stream.rs`. Only the
//! wire types this codec emits or consumes are implemented:
//! varint (0), 64-bit (1) and length-delimited (2).

use anyhow::{Context, bail};

/// Maximum field number allowed by the protobuf spec (2^29 - 1).
const MAX_FIELD_NUMBER: u32 = (1 << 29) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtoField<'a> {
    pub number: u32,
    pub wire_type: u8,
    /// Value for wire types 0/1/5 (varint, fixed64, fixed32).
    pub scalar: u64,
    /// Value for wire type 2 (length-delimited).
    pub bytes: &'a [u8],
}

pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_tag(out: &mut Vec<u8>, field: u32, wire_type: u8) {
    debug_assert!(field > 0 && field <= MAX_FIELD_NUMBER);
    write_varint(out, ((field as u64) << 3) | u64::from(wire_type));
}

pub fn write_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    write_tag(out, field, 0);
    write_varint(out, value);
}

/// IEEE-754 double, little-endian (wire type 1) — `CompletionConfig`
/// temperature / top_p travel this way.
pub fn write_fixed64_field(out: &mut Vec<u8>, field: u32, value: f64) {
    write_tag(out, field, 1);
    out.extend_from_slice(&value.to_le_bytes());
}

/// IEEE-754 single, little-endian (wire type 5) — upstream pricing rows and
/// usage dimension floats travel this way.
#[cfg(test)]
pub fn write_fixed32_field(out: &mut Vec<u8>, field: u32, value: f32) {
    write_tag(out, field, 5);
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn write_len_field(out: &mut Vec<u8>, field: u32, payload: &[u8]) {
    write_tag(out, field, 2);
    write_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

pub fn write_string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    write_len_field(out, field, value.as_bytes());
}

pub fn write_message_field(out: &mut Vec<u8>, field: u32, message: &[u8]) {
    write_len_field(out, field, message);
}

/// Parse a protobuf payload into its top-level fields.
///
/// Unknown wire types and truncated fields are hard errors: callers decide
/// whether a malformed upstream frame aborts the stream or is skipped.
pub fn parse_fields(payload: &[u8]) -> anyhow::Result<Vec<ProtoField<'_>>> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < payload.len() {
        let (tag, used) = read_varint(&payload[pos..]).context("protobuf tag")?;
        pos += used;
        let number = (tag >> 3) as u32;
        if number == 0 || number > MAX_FIELD_NUMBER {
            bail!("invalid protobuf field number {number}");
        }
        let wire_type = (tag & 0x07) as u8;
        let field = match wire_type {
            0 => {
                let (value, used) = read_varint(&payload[pos..]).context("varint field")?;
                pos += used;
                ProtoField {
                    number,
                    wire_type,
                    scalar: value,
                    bytes: &[],
                }
            }
            1 => {
                let raw = payload
                    .get(pos..pos + 8)
                    .context("truncated fixed64 field")?;
                pos += 8;
                ProtoField {
                    number,
                    wire_type,
                    scalar: u64::from_le_bytes(raw.try_into().expect("8-byte fixed64")),
                    bytes: &[],
                }
            }
            2 => {
                let (len, used) = read_varint(&payload[pos..]).context("length prefix")?;
                pos += used;
                let len = usize::try_from(len).context("length-delimited field too large")?;
                let bytes = payload
                    .get(pos..pos + len)
                    .context("truncated length-delimited field")?;
                pos += len;
                ProtoField {
                    number,
                    wire_type,
                    scalar: 0,
                    bytes,
                }
            }
            5 => {
                let raw = payload
                    .get(pos..pos + 4)
                    .context("truncated fixed32 field")?;
                pos += 4;
                ProtoField {
                    number,
                    wire_type,
                    scalar: u64::from(u32::from_le_bytes(raw.try_into().expect("4-byte fixed32"))),
                    bytes: &[],
                }
            }
            other => bail!("unsupported protobuf wire type {other}"),
        };
        fields.push(field);
    }
    Ok(fields)
}

fn read_varint(payload: &[u8]) -> anyhow::Result<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in payload.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    bail!("truncated or overlong varint")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip_boundaries() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            let (decoded, used) = read_varint(&buf).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn fields_round_trip() {
        let mut buf = Vec::new();
        write_string_field(&mut buf, 1, "chisel");
        write_varint_field(&mut buf, 2, 5);
        write_fixed64_field(&mut buf, 5, 0.95);
        write_message_field(&mut buf, 10, b"\x0a\x03abc");
        let fields = parse_fields(&buf).unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!((fields[0].number, fields[0].wire_type), (1, 2));
        assert_eq!(fields[0].bytes, b"chisel");
        assert_eq!((fields[1].number, fields[1].scalar), (2, 5));
        assert_eq!(fields[2].number, 5);
        assert_eq!(f64::from_le_bytes(fields[2].scalar.to_le_bytes()), 0.95);
        assert_eq!(
            (fields[3].number, fields[3].bytes),
            (10, &b"\x0a\x03abc"[..])
        );
    }

    #[test]
    fn rejects_truncated_len_field() {
        let mut buf = Vec::new();
        write_string_field(&mut buf, 3, "hello");
        buf.pop();
        assert!(parse_fields(&buf).is_err());
    }

    #[test]
    fn rejects_group_wire_types() {
        // Field 1, wire type 3 (start group) — unsupported.
        assert!(parse_fields(&[0x0b, 0x00]).is_err());
    }
}
