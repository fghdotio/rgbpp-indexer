//! The slice of molecule encoding the indexer needs.
//!
//! RGB++ commitments are computed over molecule-serialised CKB structures, so we
//! need canonical encoders for `Script`, `CellOutput`, `Bytes` and `OutPoint`.
//! Pulling in `ckb-types` for four encoders would drag a large dependency tree in;
//! the schema for these types is stable and small enough to implement directly.
//!
//! Encoding rules used here (from the molecule spec):
//! * `struct` / fixed-size types: fields concatenated, no header.
//! * `fixvec` of bytes (`Bytes`): `u32le` item count, then the items.
//! * `table`: `u32le` full size, then one `u32le` offset per field, then the bodies.
//! * `option`: empty slice for `None`, the inner encoding for `Some`.

use crate::ckb::{CellOutput, CkbOutPoint, Script};

pub fn encode_uint32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

pub fn encode_uint64(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// `Bytes` is a fixvec of `byte`: length prefix in items (== bytes here).
pub fn encode_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Generic molecule `table` assembly.
fn encode_table(fields: &[Vec<u8>]) -> Vec<u8> {
    let header_len = 4 + 4 * fields.len();
    let body_len: usize = fields.iter().map(|f| f.len()).sum();
    let full_size = header_len + body_len;

    let mut out = Vec::with_capacity(full_size);
    out.extend_from_slice(&(full_size as u32).to_le_bytes());

    let mut offset = header_len;
    for field in fields {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += field.len();
    }
    for field in fields {
        out.extend_from_slice(field);
    }
    out
}

pub fn encode_script(script: &Script) -> Vec<u8> {
    encode_table(&[
        script.code_hash.0.to_vec(),
        vec![script.hash_type.to_byte()],
        encode_bytes(script.args.as_slice()),
    ])
}

/// `ScriptOpt`: `None` encodes to zero bytes.
pub fn encode_script_opt(script: Option<&Script>) -> Vec<u8> {
    match script {
        Some(s) => encode_script(s),
        None => Vec::new(),
    }
}

pub fn encode_cell_output(output: &CellOutput) -> Vec<u8> {
    encode_table(&[
        encode_uint64(output.capacity).to_vec(),
        encode_script(&output.lock),
        encode_script_opt(output.type_.as_ref()),
    ])
}

/// `OutPoint` is a molecule `struct`: 32-byte tx hash followed by a `u32le` index.
pub fn encode_out_point(out_point: &CkbOutPoint) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&out_point.tx_hash.0);
    out.extend_from_slice(&out_point.index.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ckb::{ScriptHashType, H256};

    #[test]
    fn script_table_layout_is_canonical() {
        let script = Script::new(H256::ZERO, ScriptHashType::Type, vec![0xaa, 0xbb]);
        let encoded = encode_script(&script);

        // 4 (size) + 3*4 (offsets) + 32 (code_hash) + 1 (hash_type) + 4+2 (args)
        assert_eq!(encoded.len(), 4 + 12 + 32 + 1 + 6);
        assert_eq!(
            u32::from_le_bytes(encoded[0..4].try_into().unwrap()) as usize,
            encoded.len()
        );
        assert_eq!(u32::from_le_bytes(encoded[4..8].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(encoded[8..12].try_into().unwrap()), 48);
        assert_eq!(u32::from_le_bytes(encoded[12..16].try_into().unwrap()), 49);
        assert_eq!(encoded[48], ScriptHashType::Type.to_byte());
    }

    #[test]
    fn out_point_struct_is_36_bytes() {
        let op = CkbOutPoint::new(H256::ZERO, 7);
        let encoded = encode_out_point(&op);
        assert_eq!(encoded.len(), 36);
        assert_eq!(&encoded[32..], &[7, 0, 0, 0]);
    }

    #[test]
    fn empty_bytes_is_just_the_length_prefix() {
        assert_eq!(encode_bytes(&[]), vec![0, 0, 0, 0]);
    }
}

// --- decoding -------------------------------------------------------------
//
// Only tables need a decoder: BTC time lock args are a molecule table, and its
// first field is a nested `Script`.

use crate::ckb::{Bytes as CkbBytes, ScriptHashType, H256};
use crate::error::{Error, Result};

/// Split a molecule `table` into its field slices, validating the header.
pub fn read_table_fields(data: &[u8]) -> Result<Vec<&[u8]>> {
    if data.len() < 4 {
        return Err(Error::malformed(
            "molecule table",
            "shorter than the size header",
        ));
    }
    let full_size = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if full_size != data.len() {
        return Err(Error::malformed(
            "molecule table",
            format!("size header {full_size} != actual length {}", data.len()),
        ));
    }
    if full_size == 4 {
        return Ok(Vec::new()); // empty table
    }
    if data.len() < 8 {
        return Err(Error::malformed("molecule table", "missing first offset"));
    }
    let first_offset = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    if first_offset < 8 || first_offset % 4 != 0 || first_offset > full_size {
        return Err(Error::malformed(
            "molecule table",
            format!("implausible first offset {first_offset}"),
        ));
    }
    let field_count = (first_offset - 4) / 4;

    let mut offsets = Vec::with_capacity(field_count + 1);
    for i in 0..field_count {
        let start = 4 + i * 4;
        offsets.push(u32::from_le_bytes(data[start..start + 4].try_into().unwrap()) as usize);
    }
    offsets.push(full_size);

    let mut fields = Vec::with_capacity(field_count);
    for i in 0..field_count {
        let (start, end) = (offsets[i], offsets[i + 1]);
        if start > end || end > full_size {
            return Err(Error::malformed(
                "molecule table",
                format!("field {i} has invalid range {start}..{end}"),
            ));
        }
        fields.push(&data[start..end]);
    }
    Ok(fields)
}

/// Read a molecule `Bytes` (fixvec of byte).
pub fn read_bytes(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 4 {
        return Err(Error::malformed("molecule Bytes", "missing length prefix"));
    }
    let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if 4 + len != data.len() {
        return Err(Error::malformed(
            "molecule Bytes",
            format!(
                "length prefix {len} does not match payload {}",
                data.len() - 4
            ),
        ));
    }
    Ok(&data[4..])
}

pub fn decode_script(data: &[u8]) -> Result<Script> {
    let fields = read_table_fields(data)?;
    if fields.len() != 3 {
        return Err(Error::malformed(
            "molecule Script",
            format!("expected 3 fields, got {}", fields.len()),
        ));
    }
    let code_hash = H256::from_slice(fields[0])?;
    if fields[1].len() != 1 {
        return Err(Error::malformed(
            "molecule Script",
            "hash_type is not one byte",
        ));
    }
    let hash_type = match fields[1][0] {
        0 => ScriptHashType::Data,
        1 => ScriptHashType::Type,
        2 => ScriptHashType::Data1,
        4 => ScriptHashType::Data2,
        other => {
            return Err(Error::Unsupported {
                what: "script hash_type byte",
                value: other.to_string(),
            })
        }
    };
    let args = read_bytes(fields[2])?.to_vec();
    Ok(Script {
        code_hash,
        hash_type,
        args: CkbBytes(args),
    })
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    #[test]
    fn script_roundtrips_through_molecule() {
        let script = Script::new(
            H256::from_hex("0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")
                .unwrap(),
            ScriptHashType::Data1,
            vec![1, 2, 3, 4, 5],
        );
        let decoded = decode_script(&encode_script(&script)).unwrap();
        assert_eq!(decoded, script);
    }

    #[test]
    fn truncated_table_is_rejected() {
        let script = Script::new(H256::ZERO, ScriptHashType::Type, vec![9]);
        let mut encoded = encode_script(&script);
        encoded.pop();
        assert!(decode_script(&encoded).is_err());
    }
}
