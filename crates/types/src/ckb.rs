//! Minimal CKB primitives.
//!
//! We intentionally re-declare these instead of pulling in `ckb-types`: the indexer
//! only needs the handful of structures that appear in RPC payloads, and keeping our
//! own definitions lets us control serde/hex behaviour and database mapping.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// A 32-byte CKB hash (block hash, tx hash, script hash, ...).
///
/// CKB hashes have no byte-order ambiguity: the hex string in the RPC is the byte
/// sequence itself, so `H256` stores exactly what is on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct H256(pub [u8; 32]);

impl H256 {
    pub const ZERO: H256 = H256([0u8; 32]);

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(Error::Length {
                what: "H256",
                expected: 32,
                got: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(H256(out))
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        Self::from_slice(&hex::decode(s)?)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    /// `0x`-prefixed hex, the form every CKB RPC uses.
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Display for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H256({})", self.to_hex())
    }
}

impl Serialize for H256 {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for H256 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        H256::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Arbitrary-length byte string, serialised as `0x`-prefixed hex (CKB `JsonBytes`).
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn new(v: Vec<u8>) -> Self {
        Bytes(v)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        Ok(Bytes(hex::decode(s)?))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(&self.0))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes({})", self.to_hex())
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Bytes::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptHashType {
    Data,
    Type,
    Data1,
    Data2,
}

impl ScriptHashType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptHashType::Data => "data",
            ScriptHashType::Type => "type",
            ScriptHashType::Data1 => "data1",
            ScriptHashType::Data2 => "data2",
        }
    }

    /// Molecule byte encoding, per CKB's `ScriptHashType` schema.
    pub fn to_byte(self) -> u8 {
        match self {
            ScriptHashType::Data => 0,
            ScriptHashType::Type => 1,
            ScriptHashType::Data1 => 2,
            ScriptHashType::Data2 => 4,
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "data" => Some(ScriptHashType::Data),
            "type" => Some(ScriptHashType::Type),
            "data1" => Some(ScriptHashType::Data1),
            "data2" => Some(ScriptHashType::Data2),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Script {
    pub code_hash: H256,
    pub hash_type: ScriptHashType,
    pub args: Bytes,
}

impl Script {
    pub fn new(code_hash: H256, hash_type: ScriptHashType, args: Vec<u8>) -> Self {
        Script {
            code_hash,
            hash_type,
            args: Bytes(args),
        }
    }

    /// `ckbhash(molecule(script))` — the identity CKB uses for lock/type scripts.
    pub fn calc_hash(&self) -> H256 {
        H256(ckb_blake2b_256(&crate::molecule::encode_script(self)))
    }

    /// True when `self` and `other` share code hash + hash type. RGB++ recognition
    /// is always "same code, any args", because the args carry the BTC binding.
    pub fn same_code(&self, code_hash: &H256, hash_type: ScriptHashType) -> bool {
        self.code_hash == *code_hash && self.hash_type == hash_type
    }
}

/// A CKB cell reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CkbOutPoint {
    pub tx_hash: H256,
    pub index: u32,
}

impl CkbOutPoint {
    pub fn new(tx_hash: H256, index: u32) -> Self {
        CkbOutPoint { tx_hash, index }
    }
}

impl fmt::Display for CkbOutPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.tx_hash, self.index)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellOutput {
    pub capacity: u64,
    pub lock: Script,
    #[serde(rename = "type")]
    pub type_: Option<Script>,
}

/// CKB's blake2b-256 with the `ckb-default-hash` personalisation.
pub fn ckb_blake2b_256(data: &[u8]) -> [u8; 32] {
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .personal(b"ckb-default-hash")
        .to_state()
        .update(data)
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ckb_hash_vector() {
        // The empty-input digest of CKB's blake2b personalisation is a well-known
        // constant (it is `CKB_HASH_EMPTY` in ckb-hash).
        assert_eq!(
            hex::encode(ckb_blake2b_256(b"")),
            "44f4c69744d5f8c55d642062949dcae49bc4e7ef43d388c5a12f42b5633d163e"
        );
    }

    #[test]
    fn h256_hex() {
        let h =
            H256::from_hex("0xbc6c568a1a0d0a09f6844dc9d74ddb4343c32143ff25f727c59edf4fb72d6936")
                .unwrap();
        assert_eq!(
            h.to_hex(),
            "0xbc6c568a1a0d0a09f6844dc9d74ddb4343c32143ff25f727c59edf4fb72d6936"
        );
    }
}
