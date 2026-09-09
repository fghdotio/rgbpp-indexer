//! Derived aggregates.
//!
//! Every number here is a `SELECT` over the fact tables. Nothing is incremented and
//! stored, because a stored counter is exactly the thing a reorg would force us to
//! correct in reverse — and a counter that drifts is far harder to notice than a
//! query that is a little slower.

use serde::Serialize;
use sqlx::Postgres;

use crate::error::Result;
use crate::models::{AssetBalanceRow, AssetRow};
use crate::Store;

#[derive(Clone, Debug, Default, Serialize)]
pub struct IndexerCounts {
    pub total_cells: i64,
    pub live_cells: i64,
    pub pending_ckb_cells: i64,
    pub transitions: i64,
    pub observed_outpoints: i64,
    pub open_anomalies: i64,
    pub refresh_queue_depth: i64,
    /// Distinct UDT type script hashes seen under an RGB++ lock — how many different
    /// fungible assets exist here, not how many cells hold them.
    pub udts: i64,
    /// Distinct Spore type script hashes. A spore's id lives in its type args, so one
    /// hash is one DOB however many times it has moved between cells.
    pub dobs: i64,
}

impl Store {
    pub async fn counts(&self) -> Result<IndexerCounts> {
        // One round trip: these are all cheap aggregates but there is no reason to
        // pay latency five times for a status endpoint.
        //
        // The two asset counts are COUNT(DISTINCT type_hash) rather than COUNT(*): a
        // cell is one *holding* of an asset, and the same asset appears in a new cell
        // every time it moves. `rgbpp_cells_type_hash_idx` covers both.
        let row: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT
                (SELECT COUNT(*) FROM rgbpp_cells),
                (SELECT COUNT(*) FROM rgbpp_cells WHERE consumed_block_number IS NULL),
                (SELECT COUNT(*) FROM rgbpp_cell_status WHERE status = 'pending_ckb'),
                (SELECT COUNT(*) FROM rgbpp_transitions),
                (SELECT COUNT(*) FROM btc_outpoints),
                (SELECT COUNT(*) FROM rgbpp_anomalies WHERE resolved_at IS NULL),
                (SELECT COUNT(*) FROM btc_refresh_queue),
                (SELECT COUNT(DISTINCT type_hash) FROM rgbpp_cells
                  WHERE asset_kind IN ('xudt', 'sudt')),
                (SELECT COUNT(DISTINCT type_hash) FROM rgbpp_cells
                  WHERE asset_kind = 'spore')",
        )
        .fetch_one(self.pool())
        .await?;

        Ok(IndexerCounts {
            total_cells: row.0,
            live_cells: row.1,
            pending_ckb_cells: row.2,
            transitions: row.3,
            observed_outpoints: row.4,
            open_anomalies: row.5,
            refresh_queue_depth: row.6,
            udts: row.7,
            dobs: row.8,
        })
    }

    /// Every distinct asset of the given kinds, newest activity first.
    ///
    /// Grouped rather than listed: an asset appears in a new cell every time it
    /// moves, so `SELECT DISTINCT type_hash` is the only way to answer "which assets
    /// exist". `live_seal_count` counts the Bitcoin outpoints currently holding it,
    /// which is as close to a holder count as RGB++ state gets — an address can own
    /// many seals, and the indexer only learns addresses opportunistically.
    pub async fn list_assets(
        &self,
        asset_kinds: &[String],
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AssetRow>> {
        Ok(sqlx::query_as::<Postgres, AssetRow>(
            "SELECT type_hash,
                    asset_kind,
                    COUNT(*)                                                   AS cell_count,
                    COUNT(*) FILTER (WHERE consumed_block_number IS NULL)      AS live_cell_count,
                    COUNT(DISTINCT (btc_txid, btc_vout))
                      FILTER (WHERE consumed_block_number IS NULL)             AS live_seal_count,
                    SUM(COALESCE(udt_amount, 0))
                      FILTER (WHERE consumed_block_number IS NULL)             AS total_amount,
                    MIN(created_block_number)                                  AS first_block_number,
                    (ARRAY_AGG(ckb_tx_hash ORDER BY created_block_number,
                                                    created_tx_index,
                                                    output_index))[1]          AS first_ckb_tx_hash,
                    MAX(created_block_number)                                  AS last_block_number
               FROM rgbpp_cells
              WHERE type_hash IS NOT NULL
                AND asset_kind = ANY($1)
              GROUP BY type_hash, asset_kind
              ORDER BY last_block_number DESC, live_cell_count DESC, type_hash
              LIMIT $2 OFFSET $3",
        )
        .bind(asset_kinds)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?)
    }

    /// How many distinct assets of these kinds exist, for paging.
    pub async fn count_assets(&self, asset_kinds: &[String]) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT type_hash) FROM rgbpp_cells
              WHERE type_hash IS NOT NULL AND asset_kind = ANY($1)",
        )
        .bind(asset_kinds)
        .fetch_one(self.pool())
        .await?;
        Ok(count)
    }

    // TODO: holder aggregates for the explorer's coin/statistic views need an L1/L2
    // split, where L2 means the same asset held under a plain CKB lock. Only RGB++ and
    // BTC time locks are indexed here, so L2 is out of scope by construction -- widening
    // it would make this a UDT indexer rather than an RGB++ one. Compose L2 in the
    // gateway instead, and note the count will be approximate.

    /// Balances across a set of Bitcoin outpoints, grouped by asset.
    ///
    /// `include_pending = false` excludes cells whose bound UTXO has already been
    /// spent on Bitcoin: that is the conservative view an application wants before
    /// treating a balance as spendable.
    pub async fn asset_balances_for_outpoints(
        &self,
        txids: &[Vec<u8>],
        vouts: &[i32],
        include_pending: bool,
    ) -> Result<Vec<AssetBalanceRow>> {
        Ok(sqlx::query_as::<Postgres, AssetBalanceRow>(
            "SELECT type_hash,
                    asset_kind,
                    COUNT(*)                     AS cell_count,
                    SUM(capacity)::numeric       AS total_capacity,
                    SUM(COALESCE(udt_amount, 0)) AS total_amount
               FROM rgbpp_cell_status
              WHERE (btc_txid, btc_vout) IN (SELECT * FROM UNNEST($1::bytea[], $2::int[]))
                AND status = ANY(CASE WHEN $3 THEN ARRAY['live', 'pending_ckb'] ELSE ARRAY['live'] END)
              GROUP BY type_hash, asset_kind
              ORDER BY asset_kind, type_hash",
        )
        .bind(txids)
        .bind(vouts)
        .bind(include_pending)
        .fetch_all(self.pool())
        .await?)
    }
}

impl Store {
    /// Open a sweep run, returning its id.
    pub async fn start_sweep_run(&self) -> Result<i64> {
        let (id,): (i64,) = sqlx::query_as("INSERT INTO sweep_runs DEFAULT VALUES RETURNING id")
            .fetch_one(self.pool())
            .await?;
        Ok(id)
    }

    pub async fn finish_sweep_run(
        &self,
        id: i64,
        outpoints_checked: i64,
        status_changed: i64,
        anomalies_found: i64,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sweep_runs
                SET finished_at = now(), outpoints_checked = $2, status_changed = $3,
                    anomalies_found = $4, status = $5, error = $6
              WHERE id = $1",
        )
        .bind(id)
        .bind(outpoints_checked)
        .bind(status_changed)
        .bind(anomalies_found)
        .bind(if error.is_some() {
            "failed"
        } else {
            "completed"
        })
        .bind(error)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn last_sweep_run(
        &self,
    ) -> Result<
        Option<(
            i64,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            i64,
            i64,
            String,
        )>,
    > {
        Ok(sqlx::query_as(
            "SELECT id, started_at, finished_at, outpoints_checked, anomalies_found, status
               FROM sweep_runs ORDER BY started_at DESC LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await?)
    }
}
