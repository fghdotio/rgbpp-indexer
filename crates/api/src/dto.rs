//! Response shapes.
//!
//! Two conventions, both chosen to survive contact with JavaScript clients:
//! amounts and capacities are decimal **strings** (a `u128` UDT amount and a large
//! capacity both exceed `Number.MAX_SAFE_INTEGER`), and every hash is hex — CKB
//! hashes `0x`-prefixed as their RPC does, Bitcoin txids bare as every explorer does.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use rgbpp_indexer::reconcile::{AddressReconcile, OutpointRefresh, TransitionResolution};
use rgbpp_store::models::{AnomalyRow, AssetBalanceRow, AssetRow, CellRow, TransitionRow};
use rgbpp_store::stats::IndexerCounts;
use rgbpp_types::state::OutpointSpendStatus;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct CkbOutPointDto {
    pub tx_hash: String,
    pub index: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BtcOutPointDto {
    pub txid: String,
    pub vout: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BtcTimeDto {
    /// Bitcoin confirmations `btc_txid` needs before the cell unlocks.
    pub after: u32,
    pub btc_txid: String,
    /// Lock the cell falls back to once `after` is met.
    #[schema(value_type = Option<ScriptDto>)]
    pub target_lock: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedDto {
    pub block_number: i64,
    /// `-1` means the cell was backfilled from the node, where the position inside
    /// the block is not reported.
    pub tx_index: i32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConsumedDto {
    pub block_number: i64,
    pub tx_hash: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BtcObservationDto {
    pub status: String,
    pub spender: Option<String>,
    pub height: Option<i32>,
    pub observed_at: Option<DateTime<Utc>>,
    pub address: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellDto {
    pub ckb_out_point: CkbOutPointDto,
    pub lock_kind: String,
    /// Present for RGB++ locks. BTC time locks reference a transaction, not a UTXO.
    pub btc_out_point: Option<BtcOutPointDto>,
    pub btc_time: Option<BtcTimeDto>,
    pub type_hash: Option<String>,
    #[schema(value_type = Option<ScriptDto>)]
    pub type_script: Option<serde_json::Value>,
    /// `xudt`, `sudt`, `spore`, `spore_cluster`, `unknown` (a type script matching
    /// no configured code hash) or `none` (no type script).
    pub asset_kind: String,
    /// UDT amount as a decimal string. Null for non-fungible assets.
    pub amount: Option<String>,
    /// Shannons, as a decimal string.
    pub capacity: String,
    /// Cell data, `0x`-prefixed hex.
    pub data: String,
    /// `live`, `pending_ckb` (bound UTXO spent on Bitcoin, CKB transition not
    /// indexed yet) or `spent`. Derived from the CKB fact and the Bitcoin
    /// observation together.
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

#[derive(Debug, Serialize, ToSchema)]
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

#[derive(Debug, Serialize, ToSchema)]
pub struct AssetBalanceDto {
    pub asset_kind: String,
    pub type_hash: Option<String>,
    pub cell_count: i64,
    pub total_capacity: String,
    /// Null for non-fungible assets.
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

/// One distinct asset in the index.
///
/// There is no `symbol` and no `decimals` field, and there will not be one until
/// something indexes the cells that publish them: the type script hash is the whole
/// identity of an asset here. A client that needs a name has to resolve it itself.
#[derive(Debug, Serialize, ToSchema)]
pub struct AssetDto {
    pub type_hash: String,
    pub asset_kind: String,
    pub cell_count: i64,
    pub live_cell_count: i64,
    /// Distinct Bitcoin outpoints currently holding it.
    pub live_seal_count: i64,
    /// Null for non-fungible assets.
    pub total_amount: Option<String>,
    pub first_block_number: i64,
    pub first_ckb_tx_hash: String,
    pub last_block_number: i64,
}

impl From<AssetRow> for AssetDto {
    fn from(row: AssetRow) -> Self {
        let is_fungible = matches!(row.asset_kind.as_str(), "xudt" | "sudt");
        AssetDto {
            type_hash: hex0x(&row.type_hash),
            asset_kind: row.asset_kind,
            cell_count: row.cell_count,
            live_cell_count: row.live_cell_count,
            live_seal_count: row.live_seal_count,
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
            first_block_number: row.first_block_number,
            first_ckb_tx_hash: hex0x(&row.first_ckb_tx_hash),
            last_block_number: row.last_block_number,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AnomalyDto {
    pub id: i64,
    pub kind: String,
    pub btc_out_point: Option<BtcOutPointDto>,
    pub ckb_out_point: Option<CkbOutPointDto>,
    #[schema(value_type = Object)]
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
#[derive(Debug, Serialize, ToSchema)]
pub struct StatusDto {
    pub network: String,
    pub btc_source: String,
    pub ckb: CkbStatusDto,
    pub counts: CountsDto,
    pub last_sweep: Option<SweepStatusDto>,
}

#[derive(Debug, Serialize, ToSchema)]
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

#[derive(Debug, Serialize, ToSchema)]
pub struct SweepStatusDto {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outpoints_checked: i64,
    pub anomalies_found: i64,
    pub status: String,
}

/// A CKB script as stored with the cell.
///
/// Documentation only: the field it describes is passed through as stored JSON, but
/// always has exactly this shape.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScriptDto {
    /// `0x`-prefixed hex.
    pub code_hash: String,
    /// `data`, `type`, `data1` or `data2`.
    pub hash_type: String,
    /// `0x`-prefixed hex.
    pub args: String,
}

/// Derived counts, computed on every request.
#[derive(Debug, Serialize, ToSchema)]
pub struct CountsDto {
    pub total_cells: i64,
    pub live_cells: i64,
    pub pending_ckb_cells: i64,
    pub transitions: i64,
    pub observed_outpoints: i64,
    pub open_anomalies: i64,
    pub refresh_queue_depth: i64,
    /// Distinct UDT type script hashes seen under an RGB++ lock.
    pub udts: i64,
    /// Distinct Spore type script hashes.
    pub dobs: i64,
}

impl From<IndexerCounts> for CountsDto {
    fn from(c: IndexerCounts) -> Self {
        CountsDto {
            total_cells: c.total_cells,
            live_cells: c.live_cells,
            pending_ckb_cells: c.pending_ckb_cells,
            transitions: c.transitions,
            observed_outpoints: c.observed_outpoints,
            open_anomalies: c.open_anomalies,
            refresh_queue_depth: c.refresh_queue_depth,
            udts: c.udts,
            dobs: c.dobs,
        }
    }
}

// The types below mirror domain types from `rgbpp-indexer` and `rgbpp-types` that
// responses used to serialize directly. Owning them here keeps the HTTP contract in
// the crate that documents it: a refactor elsewhere can no longer change the wire
// format without touching this file. `tests::mirrors_serialize_identically` pins the
// output to what the domain types produced.

/// What the Bitcoin data source last said about one outpoint.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum SpendStatusDto {
    /// Never observed, or the observation has been invalidated.
    Unknown,
    Unspent,
    /// Spent by a transaction still in the mempool.
    SpentUnconfirmed {
        spender: String,
    },
    SpentConfirmed {
        spender: String,
        height: u32,
    },
}

impl From<OutpointSpendStatus> for SpendStatusDto {
    fn from(s: OutpointSpendStatus) -> Self {
        match s {
            OutpointSpendStatus::Unknown => SpendStatusDto::Unknown,
            OutpointSpendStatus::Unspent => SpendStatusDto::Unspent,
            OutpointSpendStatus::SpentUnconfirmed { spender } => SpendStatusDto::SpentUnconfirmed {
                spender: spender.to_hex(),
            },
            OutpointSpendStatus::SpentConfirmed { spender, height } => {
                SpendStatusDto::SpentConfirmed {
                    spender: spender.to_hex(),
                    height,
                }
            }
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OutpointRefreshDto {
    pub outpoint: BtcOutPointDto,
    pub status: SpendStatusDto,
    /// Whether this observation differs from what the indexer previously held.
    pub changed: bool,
}

impl From<OutpointRefresh> for OutpointRefreshDto {
    fn from(r: OutpointRefresh) -> Self {
        OutpointRefreshDto {
            outpoint: BtcOutPointDto {
                txid: r.outpoint.txid.to_hex(),
                vout: r.outpoint.vout,
            },
            status: r.status.into(),
            changed: r.changed,
        }
    }
}

/// Outcome of diffing an address's live UTXO set against the index.
#[derive(Debug, Serialize, ToSchema)]
pub struct AddressReconcileDto {
    pub address: String,
    /// Outpoints currently unspent according to the Bitcoin data source.
    pub source_utxo_count: usize,
    /// Believed live by the index, no longer listed by the data source.
    pub disappeared: Vec<BtcOutPointDto>,
    /// Recorded as spent by the index, still listed as unspent by the data source —
    /// a replaced transaction or a Bitcoin reorg.
    pub contradicted: Vec<BtcOutPointDto>,
    pub refreshed: Vec<OutpointRefreshDto>,
}

impl From<AddressReconcile> for AddressReconcileDto {
    fn from(r: AddressReconcile) -> Self {
        let outpoints = |v: Vec<rgbpp_types::BtcOutPoint>| {
            v.into_iter()
                .map(|o| BtcOutPointDto {
                    txid: o.txid.to_hex(),
                    vout: o.vout,
                })
                .collect()
        };
        AddressReconcileDto {
            address: r.address,
            source_utxo_count: r.source_utxo_count,
            disappeared: outpoints(r.disappeared),
            contradicted: outpoints(r.contradicted),
            refreshed: r.refreshed.into_iter().map(Into::into).collect(),
        }
    }
}

/// Where an RGB++ transaction has got to, across both chains.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum TransitionResolutionDto {
    /// The Bitcoin data source has never heard of this transaction.
    UnknownBtcTx,
    /// Seen on Bitcoin; no CKB transaction found yet.
    AwaitingCkb {
        btc_confirmed: bool,
        btc_height: Option<u32>,
    },
    /// Cells for this transaction exist on CKB above the indexed range: the
    /// transition is real, the index has not reached it yet.
    CkbSeenAboveLag {
        cells: Vec<CkbOutPointDto>,
        indexed_to: i64,
    },
    /// A CKB transition is indexed for this Bitcoin transaction.
    Indexed {
        ckb_tx_hash: String,
        block_number: i64,
    },
}

impl From<TransitionResolution> for TransitionResolutionDto {
    fn from(r: TransitionResolution) -> Self {
        match r {
            TransitionResolution::UnknownBtcTx => TransitionResolutionDto::UnknownBtcTx,
            TransitionResolution::AwaitingCkb {
                btc_confirmed,
                btc_height,
            } => TransitionResolutionDto::AwaitingCkb {
                btc_confirmed,
                btc_height,
            },
            TransitionResolution::CkbSeenAboveLag { cells, indexed_to } => {
                TransitionResolutionDto::CkbSeenAboveLag {
                    cells: cells
                        .into_iter()
                        .map(|c| CkbOutPointDto {
                            tx_hash: c.tx_hash.to_hex(),
                            index: c.index,
                        })
                        .collect(),
                    indexed_to,
                }
            }
            TransitionResolution::Indexed {
                ckb_tx_hash,
                block_number,
            } => TransitionResolutionDto::Indexed {
                ckb_tx_hash,
                block_number,
            },
        }
    }
}

/// Every error response has this shape.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// A malformed parameter. Retrying the same request will not help.
    BadRequest,
    NotFound,
    /// The Bitcoin data source or CKB node did not answer. Safe to retry.
    UpstreamUnavailable,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn no_scientific_notation() {
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
    fn summed_numeric_precision() {
        // SUM() over NUMERIC(40, 0) can exceed u128; the string form must survive it.
        let total = BigDecimal::from_str("340282366920938463463374607431768211455").unwrap()
            * BigDecimal::from(3);
        assert_eq!(
            plain_decimal(&total),
            "1020847100762815390390123822295304634365"
        );
    }

    /// Responses used to serialize these domain types directly. The mirrors must
    /// produce exactly the same JSON, or moving to them was a breaking change.
    #[test]
    fn mirrors_serialize_identically() {
        use rgbpp_types::bitcoin::{BtcOutPoint, BtcTxid};
        use rgbpp_types::ckb::{CkbOutPoint, H256};
        use serde_json::to_value;

        let out = BtcOutPoint::new(BtcTxid::from_display_bytes([0xab; 32]), 3);
        let spender = BtcTxid::from_display_bytes([0xcd; 32]);
        let statuses = [
            OutpointSpendStatus::Unknown,
            OutpointSpendStatus::Unspent,
            OutpointSpendStatus::SpentUnconfirmed { spender },
            OutpointSpendStatus::SpentConfirmed {
                spender,
                height: 800_000,
            },
        ];
        for status in statuses {
            assert_eq!(
                to_value(status).unwrap(),
                to_value(SpendStatusDto::from(status)).unwrap()
            );
        }

        let reconcile = AddressReconcile {
            address: "bc1qexample".into(),
            source_utxo_count: 2,
            disappeared: vec![out],
            contradicted: vec![out],
            refreshed: statuses
                .iter()
                .map(|&status| OutpointRefresh {
                    outpoint: out,
                    status,
                    changed: true,
                })
                .collect(),
        };
        assert_eq!(
            to_value(&reconcile).unwrap(),
            to_value(AddressReconcileDto::from(reconcile.clone())).unwrap()
        );

        let resolutions = [
            TransitionResolution::UnknownBtcTx,
            TransitionResolution::AwaitingCkb {
                btc_confirmed: true,
                btc_height: Some(800_000),
            },
            TransitionResolution::CkbSeenAboveLag {
                cells: vec![CkbOutPoint::new(H256([0x11; 32]), 1)],
                indexed_to: 13_000_000,
            },
            TransitionResolution::Indexed {
                ckb_tx_hash: "0x1111".into(),
                block_number: 13_000_001,
            },
        ];
        for resolution in resolutions {
            assert_eq!(
                to_value(&resolution).unwrap(),
                to_value(TransitionResolutionDto::from(resolution.clone())).unwrap()
            );
        }

        let counts = IndexerCounts {
            total_cells: 1,
            live_cells: 2,
            pending_ckb_cells: 3,
            transitions: 4,
            observed_outpoints: 5,
            open_anomalies: 6,
            refresh_queue_depth: 7,
            udts: 8,
            dobs: 9,
        };
        assert_eq!(
            to_value(&counts).unwrap(),
            to_value(CountsDto::from(counts.clone())).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityBtcDto {
    pub txid: String,
    pub confirmed: bool,
    pub block_height: Option<i32>,
    pub block_hash: Option<String>,
    pub block_time: Option<DateTime<Utc>>,
    /// Satoshis. Null when the transaction has not been observed yet, or when the
    /// data source does not report a fee.
    pub fee: Option<String>,
}

/// A cell an address gained or lost.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityCellDto {
    pub ckb_out_point: CkbOutPointDto,
    pub btc_out_point: Option<BtcOutPointDto>,
    pub asset_kind: String,
    pub type_hash: Option<String>,
    pub amount: Option<String>,
    pub capacity: String,
}

/// Net change in one asset, from this address's point of view.
///
/// Computed here rather than left to the caller: every client would otherwise
/// reimplement the same signed sum over received minus sent, and a history row that
/// says "−100 TOKEN" is the whole point of the endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct AssetDeltaDto {
    pub asset_kind: String,
    pub type_hash: Option<String>,
    /// Signed decimal string. Null for non-fungible assets.
    pub amount: Option<String>,
    pub capacity: String,
    pub cell_delta: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivityDirection {
    /// Only gained cells.
    In,
    /// Only lost cells.
    Out,
    /// Both — an ordinary transfer that returns change, or a self-transfer.
    Self_,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityEntryDto {
    pub ckb_tx_hash: String,
    pub block_number: i64,
    pub tx_index: i32,
    pub block_timestamp: Option<DateTime<Utc>>,
    pub kind: String,
    pub direction: ActivityDirection,
    /// `None` when the transition has no resolved Bitcoin side — issuance, or a
    /// transaction whose txid could not be derived from its outputs.
    pub btc: Option<ActivityBtcDto>,
    pub received: Vec<ActivityCellDto>,
    pub sent: Vec<ActivityCellDto>,
    pub deltas: Vec<AssetDeltaDto>,
    /// Pass as `cursor` to continue after this entry.
    pub cursor: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AddressActivityDto {
    pub address: String,
    pub entries: Vec<ActivityEntryDto>,
    /// Null when the page reached the end of the history.
    pub next_cursor: Option<String>,
    /// Bindings anywhere in the index whose owning address is still unresolved.
    ///
    /// Deliberately global, not per-address: a binding with no known owner cannot be
    /// attributed to *this* address either, so a per-address count would always be
    /// zero and would say nothing. Non-zero means the backfill is still running and
    /// any history may be incomplete — better than presenting a partial list as the
    /// whole story.
    pub unresolved_bindings_total: i64,
}

/// Identity of an asset within a transition: its kind plus its type script hash.
/// Plain capacity-only cells share the `None` hash, which is what groups them.
type AssetKey = (String, Option<Vec<u8>>);

/// One asset's signed movement within a single transition.
#[derive(Default)]
struct DeltaAccumulator {
    amount: BigDecimal,
    capacity: BigDecimal,
    cell_delta: i64,
}

impl DeltaAccumulator {
    fn apply(&mut self, cell: &rgbpp_store::models::ActivityCellRow, sign: i64) {
        if let Some(amount) = cell.udt_amount.as_ref() {
            self.amount += amount * BigDecimal::from(sign);
        }
        self.capacity += BigDecimal::from(cell.capacity * sign);
        self.cell_delta += sign;
    }
}

/// Assemble a page from the two row sets the store returns.
pub fn build_activity(
    address: String,
    rows: Vec<rgbpp_store::models::ActivityRow>,
    cells: Vec<rgbpp_store::models::ActivityCellRow>,
    page_size: i64,
    unresolved_bindings_total: i64,
) -> AddressActivityDto {
    use std::collections::HashMap;

    let mut by_tx: HashMap<Vec<u8>, (Vec<ActivityCellDto>, Vec<ActivityCellDto>)> = HashMap::new();
    let mut deltas: HashMap<Vec<u8>, HashMap<AssetKey, DeltaAccumulator>> = HashMap::new();

    for cell in cells {
        let received = cell.role == "received";
        let sign = if received { 1i64 } else { -1i64 };

        deltas
            .entry(cell.tx_hash.clone())
            .or_default()
            .entry((cell.asset_kind.clone(), cell.type_hash.clone()))
            .or_default()
            .apply(&cell, sign);

        let dto = ActivityCellDto {
            ckb_out_point: CkbOutPointDto {
                tx_hash: hex0x(&cell.cell_tx_hash),
                index: cell.output_index as u32,
            },
            btc_out_point: cell.btc_vout.map(|vout| BtcOutPointDto {
                txid: hex::encode(&cell.btc_txid),
                vout: vout as u32,
            }),
            asset_kind: cell.asset_kind.clone(),
            type_hash: cell.type_hash.as_deref().map(hex0x),
            amount: decimal_string(&cell.udt_amount),
            capacity: cell.capacity.to_string(),
        };

        let slot = by_tx.entry(cell.tx_hash).or_default();
        if received {
            slot.0.push(dto);
        } else {
            slot.1.push(dto);
        }
    }

    let reached_end = (rows.len() as i64) < page_size;
    let next_cursor = if reached_end {
        None
    } else {
        rows.last().map(|row| row.cursor())
    };

    let entries = rows
        .into_iter()
        .map(|row| {
            let (received, sent) = by_tx.remove(&row.ckb_tx_hash).unwrap_or_default();
            let direction = match (received.is_empty(), sent.is_empty()) {
                (false, true) => ActivityDirection::In,
                (true, false) => ActivityDirection::Out,
                _ => ActivityDirection::Self_,
            };

            let mut asset_deltas: Vec<AssetDeltaDto> = deltas
                .remove(&row.ckb_tx_hash)
                .unwrap_or_default()
                .into_iter()
                .map(|((asset_kind, type_hash), delta)| {
                    let fungible = matches!(asset_kind.as_str(), "xudt" | "sudt");
                    AssetDeltaDto {
                        asset_kind,
                        type_hash: type_hash.as_deref().map(hex0x),
                        amount: fungible.then(|| plain_decimal(&delta.amount)),
                        capacity: plain_decimal(&delta.capacity),
                        cell_delta: delta.cell_delta,
                    }
                })
                .collect();
            asset_deltas.sort_by(|a, b| {
                a.asset_kind
                    .cmp(&b.asset_kind)
                    .then(a.type_hash.cmp(&b.type_hash))
            });

            ActivityEntryDto {
                cursor: row.cursor(),
                ckb_tx_hash: hex0x(&row.ckb_tx_hash),
                block_number: row.block_number,
                tx_index: row.tx_index,
                block_timestamp: row.block_timestamp,
                kind: row.kind,
                direction,
                btc: row.btc_txid.as_deref().map(|txid| ActivityBtcDto {
                    txid: hex::encode(txid),
                    confirmed: row.btc_block_height.is_some(),
                    block_height: row.btc_block_height,
                    block_hash: row.btc_block_hash.as_deref().map(hex::encode),
                    block_time: row.btc_block_time,
                    fee: row.btc_fee.map(|f| f.to_string()),
                }),
                received,
                sent,
                deltas: asset_deltas,
            }
        })
        .collect();

    AddressActivityDto {
        address,
        entries,
        next_cursor,
        unresolved_bindings_total,
    }
}
