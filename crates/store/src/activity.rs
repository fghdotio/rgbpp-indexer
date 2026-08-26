//! Address activity.
//!
//! "What has this address done" is a question about *transitions*, but transitions
//! carry no address — ownership lives on the binding. So the join runs
//! `address → bindings → cells → the transitions that created or consumed them`.
//!
//! That first hop is only as good as the binding ownership recorded by the address
//! backfill. Ownership derived from an address's live UTXO listing would restrict
//! this to bindings still unspent when someone last looked — which excludes every
//! transfer out, and transfers out are most of what a history shows.

use sqlx::Postgres;

use crate::error::Result;
use crate::models::{parse_activity_cursor, ActivityCellRow, ActivityRow};
use crate::Store;

impl Store {
    /// Transitions touching an address's holdings, newest first.
    ///
    /// Keyset pagination on `(block_number, tx_index)`: an offset would drift as the
    /// scanner appends, silently repeating or skipping entries mid-scroll.
    pub async fn address_activity(
        &self,
        address: &str,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ActivityRow>> {
        let (cursor_block, cursor_index) = match cursor.and_then(parse_activity_cursor) {
            Some((block, index)) => (Some(block), index),
            None => (None, 0),
        };

        Ok(sqlx::query_as::<Postgres, ActivityRow>(
            "WITH owned AS (
                 SELECT c.ckb_tx_hash, c.consumed_tx_hash
                   FROM rgbpp_cells c
                   JOIN btc_outpoints o
                     ON o.txid = c.btc_txid AND o.vout = c.btc_vout
                  WHERE o.address = $1
             ),
             touched AS (
                 SELECT ckb_tx_hash AS tx FROM owned
                 UNION
                 SELECT consumed_tx_hash FROM owned WHERE consumed_tx_hash IS NOT NULL
             )
             SELECT t.ckb_tx_hash, t.block_number, t.tx_index, t.block_timestamp,
                    t.kind, t.btc_txid,
                    b.block_height AS btc_block_height,
                    b.block_hash   AS btc_block_hash,
                    b.block_time   AS btc_block_time,
                    b.fee          AS btc_fee
               FROM rgbpp_transitions t
               JOIN touched ON t.ckb_tx_hash = touched.tx
               LEFT JOIN btc_txs b ON b.txid = t.btc_txid
              WHERE ($2::bigint IS NULL
                     OR (t.block_number, t.tx_index) < ($2::bigint, $3::int))
              ORDER BY t.block_number DESC, t.tx_index DESC
              LIMIT $4",
        )
        .bind(address)
        .bind(cursor_block)
        .bind(cursor_index)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    /// The cells an address gained or lost in each of the given transitions.
    ///
    /// Fetched for a whole page at once rather than per entry: a history page is
    /// otherwise `N + 1` queries for no reason.
    pub async fn address_activity_cells(
        &self,
        address: &str,
        tx_hashes: &[Vec<u8>],
    ) -> Result<Vec<ActivityCellRow>> {
        if tx_hashes.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<Postgres, ActivityCellRow>(
            "WITH owned AS (
                 SELECT c.*
                   FROM rgbpp_cells c
                   JOIN btc_outpoints o
                     ON o.txid = c.btc_txid AND o.vout = c.btc_vout
                  WHERE o.address = $1
             )
             SELECT ckb_tx_hash AS tx_hash, 'received' AS role,
                    ckb_tx_hash AS cell_tx_hash, output_index, btc_txid, btc_vout,
                    asset_kind, type_hash, udt_amount, capacity
               FROM owned
              WHERE ckb_tx_hash = ANY($2::bytea[])
             UNION ALL
             SELECT consumed_tx_hash AS tx_hash, 'sent' AS role,
                    ckb_tx_hash AS cell_tx_hash, output_index, btc_txid, btc_vout,
                    asset_kind, type_hash, udt_amount, capacity
               FROM owned
              WHERE consumed_tx_hash = ANY($2::bytea[])
              ORDER BY tx_hash, role, output_index",
        )
        .bind(address)
        .bind(tx_hashes)
        .fetch_all(self.pool())
        .await?)
    }
}
