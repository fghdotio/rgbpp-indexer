//! Bitcoin primitives, with one explicit rule about byte order.
//!
//! Bitcoin txids are printed by explorers in the *reverse* of their internal
//! serialisation. Mixing the two is the classic source of "the indexer sees
//! nothing" bugs, so this type pins the convention:
//!
//! * `BtcTxid` stores bytes in **display order** (what `mempool.space` shows).
//! * Everything that talks to a human, an HTTP API or the database uses display
//!   order.
//! * [`BtcTxid::to_internal_bytes`] / [`BtcTxid::from_internal_bytes`] are the only
//!   places the reversal happens — notably when reading RGB++ lock script args,
//!   which embed the txid in internal (consensus) order.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BtcTxid([u8; 32]);

impl BtcTxid {
    /// Bytes exactly as displayed (`0x`-less hex reads left to right).
    pub fn from_display_bytes(bytes: [u8; 32]) -> Self {
        BtcTxid(bytes)
    }

    pub fn from_display_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(Error::Length {
                what: "BtcTxid",
                expected: 32,
                got: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(BtcTxid(out))
    }

    /// Parse the hex form used by explorers and Bitcoin RPC (`0x` prefix optional).
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != 64 {
            return Err(Error::Length {
                what: "BtcTxid hex",
                expected: 64,
                got: s.len(),
            });
        }
        Self::from_display_slice(&hex::decode(s)?)
    }

    /// Consensus / little-endian order, as embedded in scripts and raw transactions.
    pub fn from_internal_bytes(bytes: &[u8]) -> Result<Self> {
        let mut v = bytes.to_vec();
        v.reverse();
        Self::from_display_slice(&v)
    }

    pub fn as_display_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_display_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    pub fn to_internal_bytes(&self) -> [u8; 32] {
        let mut out = self.0;
        out.reverse();
        out
    }

    /// Hex without `0x`, matching every Bitcoin explorer and REST API.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for BtcTxid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BtcTxid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BtcTxid({})", self.to_hex())
    }
}

impl Serialize for BtcTxid {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for BtcTxid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        BtcTxid::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A Bitcoin UTXO reference — the anchor an RGB++ cell is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BtcOutPoint {
    pub txid: BtcTxid,
    pub vout: u32,
}

impl BtcOutPoint {
    pub fn new(txid: BtcTxid, vout: u32) -> Self {
        BtcOutPoint { txid, vout }
    }
}

impl fmt::Display for BtcOutPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.txid, self.vout)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BtcBlockHash([u8; 32]);

impl BtcBlockHash {
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(Error::Length {
                what: "BtcBlockHash",
                expected: 32,
                got: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(BtcBlockHash(out))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Display for BtcBlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BtcBlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BtcBlockHash({})", self.to_hex())
    }
}

impl Serialize for BtcBlockHash {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for BtcBlockHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        BtcBlockHash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_internal_orders_are_mirrors() {
        let txid =
            BtcTxid::from_hex("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b")
                .unwrap();
        let internal = txid.to_internal_bytes();
        assert_eq!(internal[0], 0x3b);
        assert_eq!(BtcTxid::from_internal_bytes(&internal).unwrap(), txid);
    }
}
