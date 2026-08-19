//! CKB JSON-RPC payload types.
//!
//! CKB encodes every integer as `0x`-prefixed hex, so numbers get newtype wrappers
//! rather than plain `u64`/`u32`.

use std::fmt;

use rgbpp_types::ckb::{Bytes, H256, Script};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! hex_uint {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $inner);

        impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                $name(v)
            }
        }

        impl From<$name> for $inner {
            fn from(v: $name) -> Self {
                v.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "0x{:x}", self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(&format!("0x{:x}", self.0))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                let stripped = s.strip_prefix("0x").ok_or_else(|| {
                    serde::de::Error::custom(format!("expected 0x-prefixed integer, got {s}"))
                })?;
                <$inner>::from_str_radix(stripped, 16)
                    .map($name)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_uint!(Uint32, u32);
hex_uint!(Uint64, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptType {
    Lock,
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptSearchMode {
    /// Match scripts whose args start with the given bytes. RGB++ discovery uses
    /// this with empty args: "any cell under this lock code".
    Prefix,
    Exact,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoType {
    Input,
    Output,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchKeyFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<Script>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_len_range: Option<[Uint64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_data_len_range: Option<[Uint64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_capacity_range: Option<[Uint64; 2]>,
    /// Half-open `[from, to)` block range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_range: Option<[Uint64; 2]>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchKey {
    pub script: Script,
    pub script_type: ScriptType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_search_mode: Option<ScriptSearchMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<SearchKeyFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_data: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by_transaction: Option<bool>,
}

impl SearchKey {
    /// "Every live cell whose lock is exactly this script."
    ///
    /// Used for point lookups by RGB++ binding: the args encode a specific Bitcoin
    /// outpoint, so an exact match answers "does a cell for this UTXO exist right
    /// now" without waiting for the lagged scan to reach it.
    pub fn lock_exact(script: Script) -> Self {
        SearchKey {
            script,
            script_type: ScriptType::Lock,
            script_search_mode: Some(ScriptSearchMode::Exact),
            filter: None,
            with_data: Some(true),
            group_by_transaction: None,
        }
    }

    /// "Every transaction touching a cell whose lock has this code hash."
    pub fn lock_prefix(script: Script) -> Self {
        SearchKey {
            script,
            script_type: ScriptType::Lock,
            script_search_mode: Some(ScriptSearchMode::Prefix),
            filter: None,
            with_data: None,
            group_by_transaction: Some(true),
        }
    }

    pub fn with_block_range(mut self, from: u64, to_exclusive: u64) -> Self {
        let filter = self.filter.get_or_insert_with(SearchKeyFilter::default);
        filter.block_range = Some([Uint64(from), Uint64(to_exclusive)]);
        self
    }
}

/// One matched cell inside a grouped `get_transactions` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchedCell {
    pub io_type: IoType,
    pub io_index: u32,
}

impl<'de> Deserialize<'de> for MatchedCell {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // The grouped response encodes each cell as a two-element array:
        // `["input", "0x1"]`.
        let (io_type, io_index): (IoType, Uint32) = Deserialize::deserialize(d)?;
        Ok(MatchedCell {
            io_type,
            io_index: io_index.0,
        })
    }
}

/// A `get_transactions` record in grouped mode.
#[derive(Clone, Debug, Deserialize)]
pub struct TxRecord {
    pub tx_hash: H256,
    pub block_number: Uint64,
    pub tx_index: Uint32,
    pub cells: Vec<MatchedCell>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pagination<T> {
    pub objects: Vec<T>,
    pub last_cursor: Bytes,
}

/// A `get_cells` record.
#[derive(Clone, Debug, Deserialize)]
pub struct CellRecord {
    pub out_point: RpcOutPoint,
    pub output: RpcCellOutput,
    pub output_data: Option<Bytes>,
    pub block_number: Uint64,
    pub tx_index: Uint32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexerTip {
    pub block_hash: H256,
    pub block_number: Uint64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CellsCapacity {
    pub capacity: Uint64,
    pub block_hash: H256,
    pub block_number: Uint64,
}

// --- node RPC ------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct RpcOutPoint {
    pub tx_hash: H256,
    pub index: Uint32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RpcCellInput {
    pub previous_output: RpcOutPoint,
    pub since: Uint64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RpcCellOutput {
    pub capacity: Uint64,
    pub lock: Script,
    #[serde(rename = "type")]
    pub type_: Option<Script>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RpcTransaction {
    pub hash: H256,
    pub version: Uint32,
    pub inputs: Vec<RpcCellInput>,
    pub outputs: Vec<RpcCellOutput>,
    pub outputs_data: Vec<Bytes>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxStatusKind {
    Pending,
    Proposed,
    Committed,
    Unknown,
    Rejected,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TxStatus {
    pub status: TxStatusKind,
    pub block_hash: Option<H256>,
    pub block_number: Option<Uint64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransactionWithStatus {
    pub transaction: Option<RpcTransaction>,
    pub tx_status: TxStatus,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RpcHeader {
    pub hash: H256,
    pub number: Uint64,
    pub parent_hash: H256,
    pub timestamp: Uint64,
    pub epoch: Uint64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_uints_round_trip_through_json() {
        let v: Uint64 = serde_json::from_str("\"0x1a2b\"").unwrap();
        assert_eq!(v.0, 0x1a2b);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"0x1a2b\"");
        assert!(serde_json::from_str::<Uint64>("\"1a2b\"").is_err());
    }

    #[test]
    fn grouped_tx_record_parses() {
        let json = r#"{
            "block_number": "0x2a",
            "cells": [["output", "0x0"], ["input", "0x3"]],
            "tx_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "tx_index": "0x2"
        }"#;
        let record: TxRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.block_number.0, 42);
        assert_eq!(record.tx_index.0, 2);
        assert_eq!(record.cells.len(), 2);
        assert_eq!(record.cells[0].io_type, IoType::Output);
        assert_eq!(record.cells[1].io_index, 3);
    }

    #[test]
    fn search_key_serialises_the_shape_the_rich_indexer_expects() {
        let script = Script::new(H256::ZERO, rgbpp_types::ckb::ScriptHashType::Type, vec![]);
        let key = SearchKey::lock_prefix(script).with_block_range(10, 20);
        let json = serde_json::to_value(&key).unwrap();
        assert_eq!(json["script_type"], "lock");
        assert_eq!(json["script_search_mode"], "prefix");
        assert_eq!(json["group_by_transaction"], true);
        assert_eq!(json["filter"]["block_range"][0], "0xa");
        assert_eq!(json["filter"]["block_range"][1], "0x14");
        assert_eq!(json["script"]["args"], "0x");
    }
}
