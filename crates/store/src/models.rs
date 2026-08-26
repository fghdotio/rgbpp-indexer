//! Row and batch types.
//!
//! Database rows use plain SQL-shaped fields (`Vec<u8>`, `i64`) and convert to the
//! domain types at the edges. Keeping the conversion explicit means a schema change
//! shows up as a compile error rather than a silently mistyped column.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use rgbpp_types::asset::AssetKind;
use rgbpp_types::bitcoin::{BtcOutPoint, BtcTxid};
use rgbpp_types::ckb::{CkbOutPoint, H256};
use rgbpp_types::protocol::LockKind;
use rgbpp_types::state::{CellStatus, OutpointSpendStatus, TransitionKind};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::error::{Result, StoreError};

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct BlockRecord {
    pub number: i64,
    pub hash: Vec<u8>,
    pub parent_hash: Vec<u8>,
    pub timestamp: DateTime<Utc>,
    pub has_rgbpp_activity: bool,
}

/// An RGB++ cell as created by a CKB transaction.
#[derive(Clone, Debug)]
pub struct NewCell {
    pub ckb_tx_hash: Vec<u8>,
    pub output_index: i32,
    pub lock_kind: LockKind,
    pub btc_txid: Vec<u8>,
    pub btc_vout: Option<i32>,
    pub btc_time_after: Option<i32>,
    pub btc_time_target_lock_hash: Option<Vec<u8>>,
    pub btc_time_target_lock: Option<serde_json::Value>,
    pub lock_hash: Vec<u8>,
    pub lock_args: Vec<u8>,
    pub type_hash: Option<Vec<u8>>,
    pub type_script: Option<serde_json::Value>,
    pub asset_kind: AssetKind,
    pub udt_amount: Option<BigDecimal>,
    pub capacity: i64,
    pub cell_data: Vec<u8>,
    pub created_block_number: i64,
    pub created_block_hash: Vec<u8>,
    pub created_tx_index: i32,
}

/// The consumption half of a cell's life.
#[derive(Clone, Debug)]
pub struct CellSpend {
    pub ckb_tx_hash: Vec<u8>,
    pub output_index: i32,
    pub consumed_block_number: i64,
    pub consumed_block_hash: Vec<u8>,
    pub consumed_tx_hash: Vec<u8>,
    pub consumed_tx_index: i32,
    pub consumed_input_index: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentStatus {
    /// Bitcoin side not observed yet.
    Unchecked,
    Match,
    Mismatch,
    /// The Bitcoin transaction is not visible to the data source.
    BtcUnknown,
    /// The Bitcoin transaction exists but publishes no commitment.
    Missing,
    /// There is nothing to check. Issuance (and leaping *from* CKB) binds cells to a
    /// pre-existing Bitcoin UTXO; no Bitcoin transaction commits to that CKB
    /// transaction, so a commitment comparison is meaningless rather than failing.
    NotApplicable,
}

impl CommitmentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CommitmentStatus::Unchecked => "unchecked",
            CommitmentStatus::Match => "match",
            CommitmentStatus::Mismatch => "mismatch",
            CommitmentStatus::BtcUnknown => "btc_unknown",
            CommitmentStatus::Missing => "missing",
            CommitmentStatus::NotApplicable => "not_applicable",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "unchecked" => Some(CommitmentStatus::Unchecked),
            "match" => Some(CommitmentStatus::Match),
            "mismatch" => Some(CommitmentStatus::Mismatch),
            "btc_unknown" => Some(CommitmentStatus::BtcUnknown),
            "missing" => Some(CommitmentStatus::Missing),
            "not_applicable" => Some(CommitmentStatus::NotApplicable),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewTransition {
    pub ckb_tx_hash: Vec<u8>,
    pub block_number: i64,
    pub block_hash: Vec<u8>,
    pub tx_index: i32,
    pub block_timestamp: Option<DateTime<Utc>>,
    pub kind: TransitionKind,
    pub btc_txid: Option<Vec<u8>>,
    pub input_cell_count: i32,
    pub output_cell_count: i32,
    pub expected_commitment: Option<Vec<u8>>,
}

/// Everything one scan round produces, applied as a single transaction.
#[derive(Clone, Debug, Default)]
pub struct IndexBatch {
    pub blocks: Vec<BlockRecord>,
    pub cells: Vec<NewCell>,
    pub spends: Vec<CellSpend>,
    pub transitions: Vec<NewTransition>,
    /// Checkpoint to publish once the rest of the batch lands.
    pub checkpoint_number: i64,
    pub checkpoint_hash: Option<Vec<u8>>,
    pub chain_tip: i64,
    pub target: i64,
    pub reorg_lag: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub blocks: u64,
    pub cells: u64,
    pub spends: u64,
    pub transitions: u64,
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A row of the `rgbpp_cell_status` view: the cell plus its derived status.
#[derive(Clone, Debug, FromRow)]
pub struct CellRow {
    pub ckb_tx_hash: Vec<u8>,
    pub output_index: i32,
    pub lock_kind: String,
    pub btc_txid: Vec<u8>,
    pub btc_vout: Option<i32>,
    pub btc_time_after: Option<i32>,
    pub btc_time_target_lock: Option<serde_json::Value>,
    pub lock_hash: Vec<u8>,
    pub lock_args: Vec<u8>,
    pub type_hash: Option<Vec<u8>>,
    pub type_script: Option<serde_json::Value>,
    pub asset_kind: String,
    pub udt_amount: Option<BigDecimal>,
    pub capacity: i64,
    pub cell_data: Vec<u8>,
    pub created_block_number: i64,
    pub created_tx_index: i32,
    pub consumed_block_number: Option<i64>,
    pub consumed_tx_hash: Option<Vec<u8>>,
    pub btc_status: Option<String>,
    pub btc_spender_txid: Option<Vec<u8>>,
    pub btc_spent_height: Option<i32>,
    pub btc_observed_at: Option<DateTime<Utc>>,
    pub btc_address: Option<String>,
    pub status: String,
}

impl CellRow {
    pub fn out_point(&self) -> Result<CkbOutPoint> {
        Ok(CkbOutPoint::new(
            H256::from_slice(&self.ckb_tx_hash)?,
            self.output_index as u32,
        ))
    }

    pub fn btc_out_point(&self) -> Result<Option<BtcOutPoint>> {
        let Some(vout) = self.btc_vout else {
            return Ok(None);
        };
        Ok(Some(BtcOutPoint::new(
            BtcTxid::from_display_slice(&self.btc_txid)?,
            vout as u32,
        )))
    }

    pub fn cell_status(&self) -> CellStatus {
        match self.status.as_str() {
            "spent" => CellStatus::Spent,
            "pending_ckb" => CellStatus::PendingCkb,
            "time_locked" => CellStatus::TimeLocked,
            _ => CellStatus::Live,
        }
    }

    pub fn lock_kind(&self) -> Result<LockKind> {
        LockKind::from_str_opt(&self.lock_kind)
            .ok_or_else(|| StoreError::mapping(format!("unknown lock_kind {}", self.lock_kind)))
    }

    pub fn asset_kind(&self) -> AssetKind {
        AssetKind::from_str_opt(&self.asset_kind).unwrap_or(AssetKind::Unknown)
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct TransitionRow {
    pub ckb_tx_hash: Vec<u8>,
    pub block_number: i64,
    pub block_hash: Vec<u8>,
    pub tx_index: i32,
    pub block_timestamp: Option<DateTime<Utc>>,
    pub kind: String,
    pub btc_txid: Option<Vec<u8>>,
    pub input_cell_count: i32,
    pub output_cell_count: i32,
    pub expected_commitment: Option<Vec<u8>>,
    pub observed_commitment: Option<Vec<u8>>,
    pub commitment_status: String,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, FromRow)]
pub struct ObservationRow {
    pub txid: Vec<u8>,
    pub vout: i32,
    pub status: String,
    pub spender_txid: Option<Vec<u8>>,
    pub spent_height: Option<i32>,
    pub address: Option<String>,
    pub source: String,
    pub observed_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
}

impl ObservationRow {
    pub fn out_point(&self) -> Result<BtcOutPoint> {
        Ok(BtcOutPoint::new(
            BtcTxid::from_display_slice(&self.txid)?,
            self.vout as u32,
        ))
    }

    pub fn spend_status(&self) -> Result<OutpointSpendStatus> {
        let spender = self
            .spender_txid
            .as_deref()
            .map(BtcTxid::from_display_slice)
            .transpose()?;
        Ok(match self.status.as_str() {
            "unspent" => OutpointSpendStatus::Unspent,
            "spent_unconfirmed" => match spender {
                Some(spender) => OutpointSpendStatus::SpentUnconfirmed { spender },
                None => OutpointSpendStatus::Unknown,
            },
            "spent_confirmed" => match spender {
                Some(spender) => OutpointSpendStatus::SpentConfirmed {
                    spender,
                    height: self.spent_height.unwrap_or(0).max(0) as u32,
                },
                None => OutpointSpendStatus::Unknown,
            },
            _ => OutpointSpendStatus::Unknown,
        })
    }

    /// Whether this observation is still fresh enough to answer from.
    pub fn is_fresh(&self, ttl_secs: i64, now: DateTime<Utc>) -> bool {
        self.invalidated_at.is_none() && (now - self.observed_at).num_seconds() < ttl_secs
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct StreamState {
    pub stream: String,
    pub last_block_number: i64,
    pub last_block_hash: Option<Vec<u8>>,
    pub target_block_number: Option<i64>,
    pub chain_tip_number: Option<i64>,
    pub reorg_lag: i64,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, FromRow)]
pub struct BlockRow {
    pub number: i64,
    pub hash: Vec<u8>,
    pub parent_hash: Vec<u8>,
    pub block_timestamp: DateTime<Utc>,
    pub has_rgbpp_activity: bool,
}

#[derive(Clone, Debug, FromRow)]
pub struct QueueItem {
    pub txid: Vec<u8>,
    pub vout: i32,
    pub reason: String,
    pub priority: i32,
    pub attempts: i32,
}

impl QueueItem {
    pub fn out_point(&self) -> Result<BtcOutPoint> {
        Ok(BtcOutPoint::new(
            BtcTxid::from_display_slice(&self.txid)?,
            self.vout as u32,
        ))
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct AnomalyRow {
    pub id: i64,
    pub kind: String,
    pub dedup_key: String,
    pub btc_txid: Option<Vec<u8>>,
    pub btc_vout: Option<i32>,
    pub ckb_tx_hash: Option<Vec<u8>>,
    pub ckb_output_index: Option<i32>,
    pub detail: serde_json::Value,
    pub detected_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// One asset's total across a set of cells, computed on read.
#[derive(Clone, Debug, FromRow)]
pub struct AssetBalanceRow {
    pub type_hash: Option<Vec<u8>>,
    pub asset_kind: String,
    pub cell_count: i64,
    pub total_capacity: Option<BigDecimal>,
    pub total_amount: Option<BigDecimal>,
}

/// Convert a `u128` UDT amount to the NUMERIC representation.
pub fn udt_amount_to_decimal(amount: u128) -> Result<BigDecimal> {
    use std::str::FromStr;
    BigDecimal::from_str(&amount.to_string())
        .map_err(|e| StoreError::mapping(format!("u128 -> NUMERIC: {e}")))
}

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

/// One RGB++ state transition that touched an address's holdings, joined with the
/// Bitcoin transaction that authorised it.
#[derive(Clone, Debug, FromRow)]
pub struct ActivityRow {
    pub ckb_tx_hash: Vec<u8>,
    pub block_number: i64,
    pub tx_index: i32,
    pub block_timestamp: Option<DateTime<Utc>>,
    pub kind: String,
    pub btc_txid: Option<Vec<u8>>,
    /// Left-joined from `btc_txs`: absent until that transaction has been observed.
    pub btc_block_height: Option<i32>,
    pub btc_block_hash: Option<Vec<u8>>,
    pub btc_block_time: Option<DateTime<Utc>>,
    pub btc_fee: Option<i64>,
}

impl ActivityRow {
    /// Keyset cursor. Ordering is `(block_number, tx_index)` because a block can hold
    /// several RGB++ transitions and a block number alone would drop or repeat some.
    pub fn cursor(&self) -> String {
        format!("{}:{}", self.block_number, self.tx_index)
    }
}

/// A cell an address gained or lost in a transition.
#[derive(Clone, Debug, FromRow)]
pub struct ActivityCellRow {
    /// The transition this row belongs to.
    pub tx_hash: Vec<u8>,
    /// `received` when the transition created the cell, `sent` when it consumed it.
    pub role: String,
    pub cell_tx_hash: Vec<u8>,
    pub output_index: i32,
    pub btc_txid: Vec<u8>,
    pub btc_vout: Option<i32>,
    pub asset_kind: String,
    pub type_hash: Option<Vec<u8>>,
    pub udt_amount: Option<BigDecimal>,
    pub capacity: i64,
}

/// Parse a `block_number:tx_index` cursor.
pub fn parse_activity_cursor(cursor: &str) -> Option<(i64, i32)> {
    let (block, index) = cursor.split_once(':')?;
    Some((block.parse().ok()?, index.parse().ok()?))
}
