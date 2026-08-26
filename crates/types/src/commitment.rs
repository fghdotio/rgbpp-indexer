//! RGB++ commitments: the value a Bitcoin transaction publishes in an `OP_RETURN` to
//! bind itself to a CKB transaction.
//!
//! `sha256(sha256(preimage))` over the leading RGB++ inputs and outputs. The committed
//! outputs carry a **zeroed** Bitcoin txid — see `docs/commitments.md`, which is worth
//! reading before touching any of this.

use sha2::{Digest, Sha256};

use crate::ckb::{CellOutput, CkbOutPoint};
use crate::molecule;

pub const RGBPP_TAG: &[u8; 5] = b"RGB++";
pub const DEFAULT_VERSION: u16 = 0;

/// The committed prefix of a CKB transaction.
#[derive(Clone, Debug, Default)]
pub struct CommitmentPreimage<'a> {
    pub version: u16,
    pub inputs: &'a [CkbOutPoint],
    pub outputs: &'a [(CellOutput, Vec<u8>)],
}

impl<'a> CommitmentPreimage<'a> {
    pub fn new(inputs: &'a [CkbOutPoint], outputs: &'a [(CellOutput, Vec<u8>)]) -> Self {
        CommitmentPreimage {
            version: DEFAULT_VERSION,
            inputs,
            outputs,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64 + self.inputs.len() * 36 + self.outputs.len() * 128);
        buf.extend_from_slice(RGBPP_TAG);
        buf.extend_from_slice(&self.version.to_be_bytes());
        buf.push(self.inputs.len() as u8);
        buf.push(self.outputs.len() as u8);
        for input in self.inputs {
            buf.extend_from_slice(&molecule::encode_out_point(input));
        }
        for (output, data) in self.outputs {
            buf.extend_from_slice(&molecule::encode_cell_output(output));
            buf.extend_from_slice(&molecule::encode_bytes(data));
        }
        buf
    }

    pub fn commitment(&self) -> [u8; 32] {
        double_sha256(&self.encode())
    }
}

pub fn double_sha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// Pull the pushed payload out of an `OP_RETURN` output script.
///
/// Returns `None` for any script that is not `OP_RETURN <data>`.
pub fn op_return_payload(script_pubkey: &[u8]) -> Option<Vec<u8>> {
    const OP_RETURN: u8 = 0x6a;
    const OP_PUSHDATA1: u8 = 0x4c;
    const OP_PUSHDATA2: u8 = 0x4d;
    const OP_PUSHDATA4: u8 = 0x4e;

    let script = script_pubkey.strip_prefix(&[OP_RETURN][..])?;
    let (len, rest) = match *script.first()? {
        n @ 0x01..=0x4b => (n as usize, &script[1..]),
        OP_PUSHDATA1 => {
            let n = *script.get(1)? as usize;
            (n, &script[2..])
        }
        OP_PUSHDATA2 => {
            let bytes = script.get(1..3)?;
            (
                u16::from_le_bytes(bytes.try_into().ok()?) as usize,
                &script[3..],
            )
        }
        OP_PUSHDATA4 => {
            let bytes = script.get(1..5)?;
            (
                u32::from_le_bytes(bytes.try_into().ok()?) as usize,
                &script[5..],
            )
        }
        _ => return None,
    };
    if rest.len() < len {
        return None;
    }
    Some(rest[..len].to_vec())
}

/// Scan a transaction's output scripts for an RGB++ commitment.
///
/// A commitment is a bare 32-byte `OP_RETURN` payload. Payloads carrying the
/// literal `RGB++` tag are also accepted, since some builders prefix the data.
pub fn find_commitment<'a, I>(script_pubkeys: I) -> Option<[u8; 32]>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut fallback: Option<[u8; 32]> = None;
    for script in script_pubkeys {
        let Some(payload) = op_return_payload(script) else {
            continue;
        };
        if let Some(tagged) = payload.strip_prefix(&RGBPP_TAG[..]) {
            if tagged.len() == 32 {
                return Some(tagged.try_into().expect("checked length"));
            }
        }
        if payload.len() == 32 && fallback.is_none() {
            fallback = Some(payload.try_into().expect("checked length"));
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ckb::{Script, ScriptHashType, H256};

    #[test]
    fn preimage_layout() {
        let inputs = vec![CkbOutPoint::new(H256::ZERO, 1)];
        let outputs: Vec<(CellOutput, Vec<u8>)> = vec![(
            CellOutput {
                capacity: 100,
                lock: Script::new(H256::ZERO, ScriptHashType::Type, vec![]),
                type_: None,
            },
            vec![1, 2, 3],
        )];
        let preimage = CommitmentPreimage::new(&inputs, &outputs);
        let encoded = preimage.encode();
        assert_eq!(&encoded[0..5], RGBPP_TAG);
        assert_eq!(&encoded[5..7], &[0, 0]); // version 0, big endian
        assert_eq!(encoded[7], 1); // input count
        assert_eq!(encoded[8], 1); // output count
        assert_eq!(&encoded[9..45], &molecule::encode_out_point(&inputs[0])[..]);
        assert_eq!(preimage.commitment(), double_sha256(&encoded));
    }

    #[test]
    fn op_return_push_forms() {
        // OP_RETURN <32 bytes>
        let mut script = vec![0x6a, 0x20];
        script.extend_from_slice(&[0xab; 32]);
        assert_eq!(op_return_payload(&script), Some(vec![0xab; 32]));

        // OP_RETURN OP_PUSHDATA1 <32 bytes>
        let mut script = vec![0x6a, 0x4c, 0x20];
        script.extend_from_slice(&[0xcd; 32]);
        assert_eq!(op_return_payload(&script), Some(vec![0xcd; 32]));

        // Not an OP_RETURN at all.
        assert_eq!(op_return_payload(&[0x76, 0xa9]), None);
        // Truncated payload.
        assert_eq!(op_return_payload(&[0x6a, 0x20, 0x01]), None);
    }

    #[test]
    fn prefers_tagged_commitment() {
        let mut untagged = vec![0x6a, 0x20];
        untagged.extend_from_slice(&[0x11; 32]);

        let mut tagged = vec![0x6a, 0x25];
        tagged.extend_from_slice(RGBPP_TAG);
        tagged.extend_from_slice(&[0x22; 32]);

        let scripts: Vec<&[u8]> = vec![&untagged, &tagged];
        assert_eq!(find_commitment(scripts), Some([0x22; 32]));

        let only_untagged: Vec<&[u8]> = vec![&untagged];
        assert_eq!(find_commitment(only_untagged), Some([0x11; 32]));
    }
}
