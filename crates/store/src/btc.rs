//! Bitcoin observation cache.
//!
//! Nothing here is a fact — every row answers a question that can be asked again,
//! which is why these tables carry timestamps instead of block anchors.

use chrono::{DateTime, Utc};
use rgbpp_types::state::OutpointSpendStatus;
use sqlx::Postgres;

use crate::error::Result;
use crate::models::ObservationRow;
use crate::Store;

const OBSERVATION_COLUMNS: &str =
    "txid, vout, status, spender_txid, spent_height, address, source, observed_at, invalidated_at";

impl Store {
    pub async fn get_observation(&self, txid: &[u8], vout: i32) -> Result<Option<ObservationRow>> {
        let sql = format!(
            "SELECT {OBSERVATION_COLUMNS} FROM btc_outpoints WHERE txid = $1 AND vout = $2"
        );
        Ok(sqlx::query_as::<Postgres, ObservationRow>(&sql)
            .bind(txid)
            .bind(vout)
            .fetch_optional(self.pool())
            .await?)
    }

    pub async fn get_observations(
        &self,
        txids: &[Vec<u8>],
        vouts: &[i32],
    ) -> Result<Vec<ObservationRow>> {
        let sql = format!(
            "SELECT {OBSERVATION_COLUMNS} FROM btc_outpoints
              WHERE (txid, vout) IN (SELECT * FROM UNNEST($1::bytea[], $2::int[]))"
        );
        Ok(sqlx::query_as::<Postgres, ObservationRow>(&sql)
            .bind(txids)
            .bind(vouts)
            .fetch_all(self.pool())
            .await?)
    }

    /// Write an observation, returning `true` when the spend status actually changed.
    ///
    /// The caller uses that signal to decide whether anything downstream needs
    /// attention — a change from unspent to spent is what opens a pending window.
    pub async fn upsert_observation(
        &self,
        txid: &[u8],
        vout: i32,
        status: OutpointSpendStatus,
        address: Option<&str>,
        source: &str,
    ) -> Result<bool> {
        let spender = status.spender().map(|s| s.to_display_vec());
        let height = status.height().map(|h| h as i32);

        // The previous status is captured in the same statement as the write, so a
        // concurrent refresh of the same outpoint cannot make the two disagree.
        let row: (Option<String>,) = sqlx::query_as(
            "WITH previous AS (
                 SELECT status FROM btc_outpoints WHERE txid = $1 AND vout = $2
             )
             INSERT INTO btc_outpoints (txid, vout, status, spender_txid, spent_height, address,
                                        source, first_seen_at, observed_at, invalidated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7, now(), now(), NULL)
             ON CONFLICT (txid, vout) DO UPDATE SET
                status         = EXCLUDED.status,
                spender_txid   = EXCLUDED.spender_txid,
                spent_height   = EXCLUDED.spent_height,
                -- Keep a known address if this observation did not carry one.
                address        = COALESCE(EXCLUDED.address, btc_outpoints.address),
                source         = EXCLUDED.source,
                observed_at    = now(),
                invalidated_at = NULL
             RETURNING (SELECT status FROM previous)",
        )
        .bind(txid)
        .bind(vout)
        .bind(status.as_str())
        .bind(spender.as_deref())
        .bind(height)
        .bind(address)
        .bind(source)
        .fetch_one(self.pool())
        .await?;

        // A first observation counts as a change: it is new information either way.
        Ok(row.0.as_deref() != Some(status.as_str()))
    }

    /// Bound outpoints observed as spent on Bitcoin whose CKB transition has not
    /// been indexed — the pending window, and the seed list for misspend detection.
    pub async fn pending_ckb_outpoints(&self, limit: i64) -> Result<Vec<ObservationRow>> {
        let sql = "SELECT o.txid, o.vout, o.status, o.spender_txid, o.spent_height,
                          o.address, o.source, o.observed_at, o.invalidated_at
                     FROM btc_outpoints o
                     JOIN rgbpp_cells c ON c.btc_txid = o.txid AND c.btc_vout = o.vout
                    WHERE c.consumed_block_number IS NULL
                      AND o.invalidated_at IS NULL
                      AND o.status IN ('spent_unconfirmed', 'spent_confirmed')
                    ORDER BY o.observed_at
                    LIMIT $1";
        Ok(sqlx::query_as::<Postgres, ObservationRow>(sql)
            .bind(limit)
            .fetch_all(self.pool())
            .await?)
    }

    // --- Bitcoin transactions ------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_btc_tx(
        &self,
        txid: &[u8],
        block_height: Option<i32>,
        block_hash: Option<&[u8]>,
        block_time: Option<DateTime<Utc>>,
        commitment: Option<&[u8]>,
        input_count: i32,
        output_count: i32,
        fee: Option<i64>,
        source: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO btc_txs (txid, block_height, block_hash, block_time, commitment,
                                  input_count, output_count, fee, source, observed_at,
                                  invalidated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, now(), NULL)
             ON CONFLICT (txid) DO UPDATE SET
                block_height   = EXCLUDED.block_height,
                block_hash     = EXCLUDED.block_hash,
                block_time     = EXCLUDED.block_time,
                commitment     = EXCLUDED.commitment,
                input_count    = EXCLUDED.input_count,
                output_count   = EXCLUDED.output_count,
                -- Never downgrade a known fee to NULL: not every source reports one.
                fee            = COALESCE(EXCLUDED.fee, btc_txs.fee),
                source         = EXCLUDED.source,
                observed_at    = now(),
                invalidated_at = NULL",
        )
        .bind(txid)
        .bind(block_height)
        .bind(block_hash)
        .bind(block_time)
        .bind(commitment)
        .bind(input_count)
        .bind(output_count)
        .bind(fee)
        .bind(source)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

impl Store {
    /// Record binding ownership from the transaction that funded it.
    ///
    /// A cell bound to `(txid, vout)` is owned by whoever controls that output, so
    /// the funding transaction is the authoritative source — and unlike an address's
    /// live UTXO listing it is permanent, so a binding spent years ago is still
    /// attributable. `outputs` is `(vout, address)` for the funding transaction.
    ///
    /// Only outputs that actually carry an RGB++ cell are stored: a funding
    /// transaction usually has change and payment outputs that mean nothing here.
    pub async fn record_binding_addresses(
        &self,
        txid: &[u8],
        outputs: &[(i32, String)],
    ) -> Result<u64> {
        if outputs.is_empty() {
            return Ok(0);
        }
        let vouts: Vec<i32> = outputs.iter().map(|(v, _)| *v).collect();
        let addresses: Vec<String> = outputs.iter().map(|(_, a)| a.clone()).collect();

        let result = sqlx::query(
            "INSERT INTO btc_outpoints (txid, vout, status, address, source)
             SELECT $1, u.v, 'unknown', u.a, 'funding-tx'
               FROM UNNEST($2::int[], $3::text[]) AS u(v, a)
              WHERE EXISTS (
                    SELECT 1 FROM rgbpp_cells c
                     WHERE c.btc_txid = $1 AND c.btc_vout = u.v
              )
             -- Only the address: this says nothing about spend status, so it must not
             -- disturb the observation or its freshness.
             ON CONFLICT (txid, vout) DO UPDATE SET address = EXCLUDED.address",
        )
        .bind(txid)
        .bind(&vouts)
        .bind(&addresses)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Funding transactions of bindings whose owning address is still unknown.
    ///
    /// Grouped by transaction, because one RGB++ transaction typically funds several
    /// bindings across its outputs — resolving it costs one request and fills them
    /// all. `after` walks the set in txid order so a pass makes progress even when
    /// some transactions cannot be resolved.
    pub async fn funding_txids_missing_address(
        &self,
        after: Option<&[u8]>,
        limit: i64,
    ) -> Result<Vec<Vec<u8>>> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT DISTINCT c.btc_txid
               FROM rgbpp_cells c
               LEFT JOIN btc_outpoints o
                      ON o.txid = c.btc_txid AND o.vout = c.btc_vout
              WHERE c.btc_vout IS NOT NULL
                AND (o.txid IS NULL OR o.address IS NULL)
                AND ($1::bytea IS NULL OR c.btc_txid > $1::bytea)
              ORDER BY c.btc_txid
              LIMIT $2",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// How many bindings still have no owning address. Reported by the heartbeat so
    /// an incomplete backfill is visible rather than showing up as a thin history.
    pub async fn bindings_missing_address(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
               FROM rgbpp_cells c
               LEFT JOIN btc_outpoints o
                      ON o.txid = c.btc_txid AND o.vout = c.btc_vout
              WHERE c.btc_vout IS NOT NULL
                AND (o.txid IS NULL OR o.address IS NULL)",
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count)
    }

    /// Bound outpoints the indexer currently believes are live for an address.
    ///
    /// "Believes" is the operative word: this is the previous answer, and diffing it
    /// against the data source's live UTXO set is what surfaces transitions that
    /// happened on Bitcoin while the CKB side had not caught up.
    pub async fn live_bound_outpoints_for_address(
        &self,
        address: &str,
    ) -> Result<Vec<(Vec<u8>, i32)>> {
        Ok(sqlx::query_as(
            "SELECT DISTINCT c.btc_txid, c.btc_vout
               FROM rgbpp_cells c
               JOIN btc_outpoints o ON o.txid = c.btc_txid AND o.vout = c.btc_vout
              WHERE o.address = $1
                AND c.consumed_block_number IS NULL
                AND c.btc_vout IS NOT NULL
              ORDER BY c.btc_txid, c.btc_vout",
        )
        .bind(address)
        .fetch_all(self.pool())
        .await?)
    }

    /// Attach an address to the outpoints that actually carry RGB++ cells.
    ///
    /// A wallet address can have thousands of UTXOs, almost none of them RGB++
    /// bindings. Recording all of them would make this table grow with total wallet
    /// activity instead of with protocol activity, and buy nothing: the address diff
    /// only ever looks at outpoints that have a cell bound to them.
    pub async fn record_outpoint_addresses(
        &self,
        txids: &[Vec<u8>],
        vouts: &[i32],
        address: &str,
    ) -> Result<u64> {
        if txids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "INSERT INTO btc_outpoints (txid, vout, status, address, source)
             SELECT u.t, u.v, 'unknown', $3, 'address-lookup'
               FROM UNNEST($1::bytea[], $2::int[]) AS u(t, v)
              WHERE EXISTS (
                    SELECT 1 FROM rgbpp_cells c
                     WHERE c.btc_txid = u.t AND c.btc_vout = u.v
              )
             ON CONFLICT (txid, vout) DO UPDATE SET address = EXCLUDED.address",
        )
        .bind(txids)
        .bind(vouts)
        .bind(address)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }
}
