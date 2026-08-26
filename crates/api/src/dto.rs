//! Response shapes.
//!
//! Two conventions, both chosen to survive contact with JavaScript clients:
//! amounts and capacities are decimal **strings** (a `u128` UDT amount and a large
//! capacity both exceed `Number.MAX_SAFE_INTEGER`), and every hash is hex — CKB
//! hashes `0x`-prefixed as their RPC does, Bitcoin txids bare as every explorer does.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use rgbpp_store::models::{AnomalyRow, AssetBalanceRow, CellRow, TransitionRow};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct CkbOutPointDto {
    pub tx_hash: String,
    pub index: u32,
}

#[derive(Debug, Serialize)]
pub struct BtcOutPointDto {
    pub txid: String,
    pub vout: u32,
}

#[derive(Debug, Serialize)]
pub struct BtcTimeDto {
    pub after: u32,
    pub btc_txid: String,
    pub target_lock: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CreatedDto {
    pub block_number: i64,
    /// `-1` means the cell was backfilled from the node, where the position inside
    /// the block is not reported.
    pub tx_index: i32,
}

#[derive(Debug, Serialize)]
pub struct ConsumedDto {
    pub block_number: i64,
    pub tx_hash: String,
}

#[derive(Debug, Serialize)]
pub struct BtcObservationDto {
    pub status: String,
    pub spender: Option<String>,
    pub height: Option<i32>,
    pub observed_at: Option<DateTime<Utc>>,
    pub address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CellDto {
    pub ckb_out_point: CkbOutPointDto,
    pub lock_kind: String,
    /// Present for RGB++ locks. BTC time locks reference a transaction, not a UTXO.
    pub btc_out_point: Option<BtcOutPointDto>,
    pub btc_time: Option<BtcTimeDto>,
    pub type_hash: Option<String>,
    pub type_script: Option<serde_json::Value>,
    pub asset_kind: String,
    pub amount: Option<String>,
    pub capacity: String,
    pub data: String,
    /// Derived from the CKB fact and the Bitcoin observation together.
    pub status: String,
    pub created: CreatedDto,
    pub consumed: Option<ConsumedDto>,
    pub btc_observation: Option<BtcObservationDto>,
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// Render a NUMERIC as plain digits.
///
/// `BigDecimal`'s `Display` switches to scientific notation for round numbers, so a
/// one-token balance of `1e18` would go out as `"100e+16"`. Token amounts have to be
/// exact digits — a client parsing that as an integer either fails or, worse, does
/// not.
fn decimal_string(value: &Option<BigDecimal>) -> Option<String> {
    value.as_ref().map(plain_decimal)
}

fn plain_decimal(value: &BigDecimal) -> String {
    value.with_scale(0).to_plain_string()
}

impl From<CellRow> for CellDto {
    fn from(row: CellRow) -> Self {
        let btc_txid = hex::encode(&row.btc_txid);
        let btc_out_point = row.btc_vout.map(|vout| BtcOutPointDto {
            txid: btc_txid.clone(),
            vout: vout as u32,
        });
        let btc_time = row.btc_time_after.map(|after| BtcTimeDto {
            after: after as u32,
            btc_txid: btc_txid.clone(),
            target_lock: row.btc_time_target_lock.clone(),
        });
        let btc_observation = row.btc_status.as_ref().map(|status| BtcObservationDto {
            status: status.clone(),
            spender: row.btc_spender_txid.as_deref().map(hex::encode),
            height: row.btc_spent_height,
            observed_at: row.btc_observed_at,
            address: row.btc_address.clone(),
        });

        CellDto {
            ckb_out_point: CkbOutPointDto {
                tx_hash: hex0x(&row.ckb_tx_hash),
                index: row.output_index as u32,
            },
            lock_kind: row.lock_kind.clone(),
            btc_out_point,
            btc_time,
            type_hash: row.type_hash.as_deref().map(hex0x),
            type_script: row.type_script.clone(),
            asset_kind: row.asset_kind.clone(),
            amount: decimal_string(&row.udt_amount),
            capacity: row.capacity.to_string(),
            data: hex0x(&row.cell_data),
            status: row.status.clone(),
            created: CreatedDto {
                block_number: row.created_block_number,
                tx_index: row.created_tx_index,
            },
            consumed: match (row.consumed_block_number, row.consumed_tx_hash.as_deref()) {
                (Some(block_number), Some(tx_hash)) => Some(ConsumedDto {
                    block_number,
                    tx_hash: hex0x(tx_hash),
                }),
                _ => None,
            },
            btc_observation,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TransitionDto {
    pub ckb_tx_hash: String,
    pub block_number: i64,
    pub tx_index: i32,
    pub block_timestamp: Option<DateTime<Utc>>,
    pub kind: String,
    pub btc_txid: Option<String>,
    pub input_cell_count: i32,
    pub output_cell_count: i32,
    pub expected_commitment: Option<String>,
    pub observed_commitment: Option<String>,
    pub commitment_status: String,
}

impl From<TransitionRow> for TransitionDto {
    fn from(row: TransitionRow) -> Self {
        TransitionDto {
            ckb_tx_hash: hex0x(&row.ckb_tx_hash),
            block_number: row.block_number,
            tx_index: row.tx_index,
            block_timestamp: row.block_timestamp,
            kind: row.kind,
            btc_txid: row.btc_txid.as_deref().map(hex::encode),
            input_cell_count: row.input_cell_count,
            output_cell_count: row.output_cell_count,
            expected_commitment: row.expected_commitment.as_deref().map(hex::encode),
            observed_commitment: row.observed_commitment.as_deref().map(hex::encode),
            commitment_status: row.commitment_status,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AssetBalanceDto {
    pub asset_kind: String,
    pub type_hash: Option<String>,
    pub cell_count: i64,
    pub total_capacity: String,
    /// Absent for non-fungible assets.
    pub total_amount: Option<String>,
}

impl From<AssetBalanceRow> for AssetBalanceDto {
    fn from(row: AssetBalanceRow) -> Self {
        let is_fungible = matches!(row.asset_kind.as_str(), "xudt" | "sudt");
        AssetBalanceDto {
            asset_kind: row.asset_kind,
            type_hash: row.type_hash.as_deref().map(hex0x),
            cell_count: row.cell_count,
            total_capacity: row
                .total_capacity
                .as_ref()
                .map(plain_decimal)
                .unwrap_or_else(|| "0".to_string()),
            total_amount: if is_fungible {
                Some(
                    row.total_amount
                        .as_ref()
                        .map(plain_decimal)
                        .unwrap_or_else(|| "0".to_string()),
                )
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AnomalyDto {
    pub id: i64,
    pub kind: String,
    pub btc_out_point: Option<BtcOutPointDto>,
    pub ckb_out_point: Option<CkbOutPointDto>,
    pub detail: serde_json::Value,
    pub detected_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

impl From<AnomalyRow> for AnomalyDto {
    fn from(row: AnomalyRow) -> Self {
        AnomalyDto {
            id: row.id,
            kind: row.kind,
            btc_out_point: match (row.btc_txid.as_deref(), row.btc_vout) {
                (Some(txid), Some(vout)) => Some(BtcOutPointDto {
                    txid: hex::encode(txid),
                    vout: vout as u32,
                }),
                _ => None,
            },
            ckb_out_point: row.ckb_tx_hash.as_deref().map(|tx_hash| CkbOutPointDto {
                tx_hash: hex0x(tx_hash),
                index: row.ckb_output_index.unwrap_or(0) as u32,
            }),
            detail: row.detail,
            detected_at: row.detected_at,
            last_seen_at: row.last_seen_at,
            resolved_at: row.resolved_at,
        }
    }
}

/// What `/status` reports. Deliberately explicit about the lag: an application that
/// does not know how far behind the indexed range is cannot tell "not there" from
/// "not there yet".
#[derive(Debug, Serialize)]
pub struct StatusDto {
    pub network: String,
    pub btc_source: String,
    pub ckb: CkbStatusDto,
    pub counts: rgbpp_store::stats::IndexerCounts,
    pub last_sweep: Option<SweepStatusDto>,
}

#[derive(Debug, Serialize)]
pub struct CkbStatusDto {
    pub indexed_to: i64,
    pub target: Option<i64>,
    pub chain_tip: Option<i64>,
    pub reorg_lag: i64,
    /// Blocks between the chain tip and what is queryable — the size of the window
    /// that only the on-demand path can see into.
    pub blocks_behind: Option<i64>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct SweepStatusDto {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outpoints_checked: i64,
    pub anomalies_found: i64,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn large_round_amounts_render_as_digits_not_science() {
        let cases = [
            ("1000000000000000000", "1000000000000000000"),
            (
                "340282366920938463463374607431768211455",
                "340282366920938463463374607431768211455",
            ),
            ("0", "0"),
            ("25400000000", "25400000000"),
        ];
        for (input, expected) in cases {
            let value = BigDecimal::from_str(input).unwrap();
            assert_eq!(plain_decimal(&value), expected, "input {input}");
        }
    }

    #[test]
    fn a_summed_numeric_keeps_full_precision() {
        // SUM() over NUMERIC(40, 0) can exceed u128; the string form must survive it.
        let total = BigDecimal::from_str("340282366920938463463374607431768211455").unwrap()
            * BigDecimal::from(3);
        assert_eq!(
            plain_decimal(&total),
            "1020847100762815390390123822295304634365"
        );
    }
}
