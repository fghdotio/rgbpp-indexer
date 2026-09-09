//! HTTP handlers.
//!
//! The read endpoints answer from the indexed range. The endpoints that matter for
//! freshness — address assets and transaction status — reconcile against the Bitcoin
//! data source first, because the indexed range deliberately stops `REORG_LAG` blocks
//! short of the tip and cannot see an uncommitted CKB transaction at all.

use axum::extract::{Path, Query, State};
use axum::Json;
use rgbpp_indexer::reconcile::{AddressReconcile, TransitionResolution};
use rgbpp_store::state::CKB_STREAM;
use rgbpp_types::bitcoin::{BtcOutPoint, BtcTxid};
use rgbpp_types::ckb::H256;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dto::*;
use crate::error::{ApiError, ApiResult};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct RefreshQuery {
    /// Re-observe the relevant outpoints before answering.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Deserialize)]
pub struct CellsQuery {
    #[serde(default)]
    pub refresh: bool,
    /// Include cells already consumed on CKB.
    #[serde(default)]
    pub include_spent: bool,
}

/// Address endpoints reconcile by default — that is the whole point of them — so the
/// flag is opt-*out*, unlike the outpoint endpoints where refreshing is opt-in.
#[derive(Debug, Deserialize)]
pub struct AddressQuery {
    pub reconcile: Option<bool>,
    #[serde(default)]
    pub include_spent: bool,
}

impl AddressQuery {
    fn reconcile(&self, enabled_globally: bool) -> bool {
        enabled_globally && self.reconcile.unwrap_or(true)
    }
}

#[derive(Debug, Deserialize)]
pub struct BalanceQuery {
    pub reconcile: Option<bool>,
    /// Count cells whose bound UTXO is already spent on Bitcoin but whose CKB
    /// transition is not indexed yet. Off by default: that is the conservative view.
    #[serde(default)]
    pub include_pending: bool,
}

#[derive(Debug, Deserialize)]
pub struct AnomalyQuery {
    pub kind: Option<String>,
    #[serde(default)]
    pub include_resolved: bool,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<i64>,
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn status(State(state): State<AppState>) -> ApiResult<Json<StatusDto>> {
    let engine = &state.engine;
    let stream = engine.store.get_stream_state(CKB_STREAM).await?;
    let counts = engine.store.counts().await?;
    let sweep = engine.store.last_sweep_run().await?;

    let ckb = match stream {
        Some(s) => CkbStatusDto {
            indexed_to: s.last_block_number,
            target: s.target_block_number,
            chain_tip: s.chain_tip_number,
            reorg_lag: s.reorg_lag,
            blocks_behind: s.chain_tip_number.map(|tip| tip - s.last_block_number),
            last_error: s.last_error,
            updated_at: s.updated_at,
        },
        None => CkbStatusDto {
            indexed_to: 0,
            target: None,
            chain_tip: None,
            reorg_lag: engine.config.ckb.reorg_lag as i64,
            blocks_behind: None,
            last_error: Some("indexer stream not initialised yet".to_string()),
            updated_at: chrono::Utc::now(),
        },
    };

    Ok(Json(StatusDto {
        network: engine.config.general.network.clone(),
        btc_source: engine.btc.name().to_string(),
        ckb,
        counts,
        last_sweep: sweep.map(
            |(id, started_at, finished_at, checked, anomalies, status)| SweepStatusDto {
                id,
                started_at,
                finished_at,
                outpoints_checked: checked,
                anomalies_found: anomalies,
                status,
            },
        ),
    }))
}

/// Cells bound to one Bitcoin UTXO.
pub async fn cells_by_btc_utxo(
    State(state): State<AppState>,
    Path((txid, vout)): Path<(String, u32)>,
    Query(query): Query<CellsQuery>,
) -> ApiResult<Json<Vec<CellDto>>> {
    let txid = BtcTxid::from_hex(&txid).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let outpoint = BtcOutPoint::new(txid, vout);

    if query.refresh {
        state
            .engine
            .reconciler
            .refresh_outpoints(&[outpoint])
            .await?;
    }

    let rows = state
        .engine
        .store
        .cells_by_btc_outpoints(
            &[txid.to_display_vec()],
            &[vout as i32],
            query.include_spent,
        )
        .await?;
    Ok(Json(rows.into_iter().map(CellDto::from).collect()))
}

/// Every cell bound to any output of one Bitcoin transaction.
pub async fn cells_by_btc_txid(
    State(state): State<AppState>,
    Path(txid): Path<String>,
) -> ApiResult<Json<Vec<CellDto>>> {
    let txid = BtcTxid::from_hex(&txid).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let rows = state
        .engine
        .store
        .cells_by_btc_txid(&txid.to_display_vec())
        .await?;
    Ok(Json(rows.into_iter().map(CellDto::from).collect()))
}

pub async fn cell_by_ckb_out_point(
    State(state): State<AppState>,
    Path((tx_hash, index)): Path<(String, u32)>,
) -> ApiResult<Json<CellDto>> {
    let tx_hash = H256::from_hex(&tx_hash).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let row = state
        .engine
        .store
        .cell_by_out_point(&tx_hash.to_vec(), index as i32)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(CellDto::from(row)))
}

#[derive(Debug, Serialize)]
pub struct AddressAssetsResponse {
    pub address: String,
    /// What the diff against the Bitcoin data source turned up. Present unless
    /// reconciliation was skipped.
    pub reconciliation: Option<AddressReconcile>,
    pub cells: Vec<CellDto>,
}

/// RGB++ assets held by a Bitcoin address.
///
/// This is the endpoint requirement 3 is about. Before answering, the address's live
/// UTXO set is fetched from the Bitcoin data source and diffed against the outpoints
/// the indexer still believes are live; anything that has moved gets re-observed, so
/// the answer reflects Bitcoin's view even when the CKB side has not caught up.
// TODO: an empty result cannot be told apart from "not indexed yet". Answer 503 with
// an explicit syncing state (plus progress) while blocks_behind is large or the stream
// holds an error, and carry indexed_to/chain_tip on normal responses. Must land before
// the first external consumer -- adding a 503 to an endpoint that has always returned
// 200 is a breaking change afterwards. See README "Known limitations".
pub async fn assets_by_btc_address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(query): Query<AddressQuery>,
) -> ApiResult<Json<AddressAssetsResponse>> {
    let engine = &state.engine;

    let reconciliation = if query.reconcile(engine.config.reconcile.enabled) {
        Some(engine.reconciler.reconcile_address(&address).await?)
    } else {
        None
    };

    // The answer itself is the set of cells bound to the address's current UTXOs.
    let utxos = engine.btc.address_utxos(&address).await?;
    let txids: Vec<Vec<u8>> = utxos
        .iter()
        .map(|u| u.outpoint.txid.to_display_vec())
        .collect();
    let vouts: Vec<i32> = utxos.iter().map(|u| u.outpoint.vout as i32).collect();

    let rows = engine
        .store
        .cells_by_btc_outpoints(&txids, &vouts, query.include_spent)
        .await?;

    Ok(Json(AddressAssetsResponse {
        address,
        reconciliation,
        cells: rows.into_iter().map(CellDto::from).collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct BalanceResponse {
    pub address: String,
    pub include_pending: bool,
    pub assets: Vec<AssetBalanceDto>,
}

/// Balances for an address, computed from the cell table on every request.
pub async fn balance_by_btc_address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(query): Query<BalanceQuery>,
) -> ApiResult<Json<BalanceResponse>> {
    let engine = &state.engine;
    if engine.config.reconcile.enabled && query.reconcile.unwrap_or(true) {
        engine.reconciler.reconcile_address(&address).await?;
    }

    let utxos = engine.btc.address_utxos(&address).await?;
    let txids: Vec<Vec<u8>> = utxos
        .iter()
        .map(|u| u.outpoint.txid.to_display_vec())
        .collect();
    let vouts: Vec<i32> = utxos.iter().map(|u| u.outpoint.vout as i32).collect();

    let rows = engine
        .store
        .asset_balances_for_outpoints(&txids, &vouts, query.include_pending)
        .await?;

    Ok(Json(BalanceResponse {
        address,
        include_pending: query.include_pending,
        assets: rows.into_iter().map(AssetBalanceDto::from).collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct TransactionStatusResponse {
    pub btc_txid: String,
    pub resolution: TransitionResolution,
    pub transitions: Vec<TransitionDto>,
    pub cells: Vec<CellDto>,
}

/// Status of one RGB++ transaction, resolved across both chains.
///
/// Polling this is the second half of requirement 3: it re-observes exactly the
/// outpoints the transaction touches, and looks past the indexed range by asking the
/// rich indexer directly for cells bound to the transaction's outputs.
pub async fn transaction_status(
    State(state): State<AppState>,
    Path(txid): Path<String>,
) -> ApiResult<Json<TransactionStatusResponse>> {
    let txid = BtcTxid::from_hex(&txid).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let engine = &state.engine;

    let resolution = engine.reconciler.resolve_transition(&txid).await?;
    let transitions = engine
        .store
        .transitions_by_btc_txid(&txid.to_display_vec())
        .await?;
    let cells = engine
        .store
        .cells_by_btc_txid(&txid.to_display_vec())
        .await?;

    Ok(Json(TransactionStatusResponse {
        btc_txid: txid.to_hex(),
        resolution,
        transitions: transitions.into_iter().map(TransitionDto::from).collect(),
        cells: cells.into_iter().map(CellDto::from).collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    /// Outpoints as `txid:vout`.
    pub outpoints: Vec<String>,
    /// Wait for the refresh instead of queueing it.
    #[serde(default = "default_true")]
    pub synchronous: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct RefreshResponse {
    pub requested: usize,
    pub refreshed: Vec<rgbpp_indexer::OutpointRefresh>,
    pub queued: usize,
}

/// Force a re-observation of specific outpoints.
pub async fn refresh_outpoints(
    State(state): State<AppState>,
    Json(request): Json<RefreshRequest>,
) -> ApiResult<Json<RefreshResponse>> {
    let outpoints = request
        .outpoints
        .iter()
        .map(|s| parse_outpoint(s))
        .collect::<ApiResult<Vec<_>>>()?;

    if request.synchronous {
        let refreshed = state
            .engine
            .reconciler
            .refresh_outpoints(&outpoints)
            .await?;
        Ok(Json(RefreshResponse {
            requested: outpoints.len(),
            refreshed,
            queued: 0,
        }))
    } else {
        let pairs: Vec<(Vec<u8>, i32)> = outpoints
            .iter()
            .map(|o| (o.txid.to_display_vec(), o.vout as i32))
            .collect();
        let queued = state
            .engine
            .store
            .enqueue_refresh_many(
                &pairs,
                "api-request",
                rgbpp_store::queue::priority::ON_DEMAND,
            )
            .await?;
        Ok(Json(RefreshResponse {
            requested: outpoints.len(),
            refreshed: Vec::new(),
            queued: queued as usize,
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct AssetsQuery {
    /// `udt` (xUDT + sUDT), `dob` (Spore), or absent for both.
    pub kind: Option<String>,
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, Serialize)]
pub struct AssetsResponse {
    pub assets: Vec<AssetDto>,
    /// Distinct assets of this kind in the whole index, for paging.
    pub total: i64,
}

/// Every distinct asset the index has seen, by type script hash.
///
/// Grouped over the cell table on read, like every other aggregate here. Note what
/// this cannot answer: an asset has no name, and no holder count — the indexer learns
/// Bitcoin addresses opportunistically, so `live_seal_count` (distinct outpoints
/// currently holding it) is the honest stand-in.
pub async fn list_assets(
    State(state): State<AppState>,
    Query(query): Query<AssetsQuery>,
) -> ApiResult<Json<AssetsResponse>> {
    // `unknown` is queryable on purpose. A cell whose type script matches no
    // configured code hash is classified `unknown`, and that is a configuration
    // problem rather than an absence of assets -- an indexer missing a code hash
    // would otherwise report an empty asset list while happily indexing the cells.
    //
    // A cluster is a DOB collection, so it is listed alongside its members instead
    // of matching neither `udt` nor `dob` and falling out of both.
    let kinds: Vec<String> = match query.kind.as_deref() {
        None | Some("all") => vec![
            "xudt".to_string(),
            "sudt".to_string(),
            "spore".to_string(),
            "spore_cluster".to_string(),
            "unknown".to_string(),
        ],
        Some("udt") => vec!["xudt".to_string(), "sudt".to_string()],
        Some("dob") => vec!["spore".to_string(), "spore_cluster".to_string()],
        Some("unknown") => vec!["unknown".to_string()],
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown asset kind `{other}`; expected `udt`, `dob`, `unknown` or `all`"
            )))
        }
    };

    let limit = state.page_size(query.limit);
    let offset = query.offset.max(0);
    let store = &state.engine.store;

    let (rows, total) = tokio::try_join!(
        store.list_assets(&kinds, limit, offset),
        store.count_assets(&kinds),
    )?;

    Ok(Json(AssetsResponse {
        assets: rows.into_iter().map(AssetDto::from).collect(),
        total,
    }))
}

pub async fn recent_transitions(
    State(state): State<AppState>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<Vec<TransitionDto>>> {
    let limit = state.page_size(query.limit);
    let rows = state.engine.store.recent_transitions(limit).await?;
    Ok(Json(rows.into_iter().map(TransitionDto::from).collect()))
}

pub async fn transition_by_ckb_tx(
    State(state): State<AppState>,
    Path(tx_hash): Path<String>,
) -> ApiResult<Json<TransitionDto>> {
    let tx_hash = H256::from_hex(&tx_hash).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let row = state
        .engine
        .store
        .transition_by_ckb_tx(&tx_hash.to_vec())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(TransitionDto::from(row)))
}

#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

/// RGB++ history for a Bitcoin address, newest first.
///
/// Deliberately does not reconcile against the Bitcoin data source: history is
/// settled, and the indexed range already lags the tip by `REORG_LAG` by design.
/// A very recent transition appears here a few minutes after it confirms; use
/// `/v1/rgbpp/transactions/{btc_txid}` to watch one in flight.
pub async fn activity_by_btc_address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(query): Query<ActivityQuery>,
) -> ApiResult<Json<AddressActivityDto>> {
    let limit = state.page_size(query.limit);
    let store = &state.engine.store;

    let rows = store
        .address_activity(&address, query.cursor.as_deref(), limit)
        .await?;
    let tx_hashes: Vec<Vec<u8>> = rows.iter().map(|r| r.ckb_tx_hash.clone()).collect();
    let cells = store.address_activity_cells(&address, &tx_hashes).await?;
    let unresolved = store.bindings_missing_address().await?;

    Ok(Json(build_activity(
        address, rows, cells, limit, unresolved,
    )))
}

pub async fn anomalies(
    State(state): State<AppState>,
    Query(query): Query<AnomalyQuery>,
) -> ApiResult<Json<Vec<AnomalyDto>>> {
    let limit = state.page_size(query.limit);
    let rows = state
        .engine
        .store
        .list_anomalies(query.kind.as_deref(), query.include_resolved, limit)
        .await?;
    Ok(Json(rows.into_iter().map(AnomalyDto::from).collect()))
}

fn parse_outpoint(s: &str) -> ApiResult<BtcOutPoint> {
    let (txid, vout) = s
        .split_once(':')
        .ok_or_else(|| ApiError::bad_request(format!("expected `txid:vout`, got `{s}`")))?;
    let txid = BtcTxid::from_hex(txid).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let vout: u32 = vout
        .parse()
        .map_err(|_| ApiError::bad_request(format!("`{vout}` is not a valid output index")))?;
    Ok(BtcOutPoint::new(txid, vout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outpoint_parsing() {
        let outpoint =
            parse_outpoint("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:2")
                .unwrap();
        assert_eq!(outpoint.vout, 2);
        assert!(parse_outpoint("nope").is_err());
        assert!(parse_outpoint("4a5e:2").is_err());
        assert!(parse_outpoint(
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b:x"
        )
        .is_err());
    }
}
