//! RGB++ cell writes and reads.

use sqlx::{Postgres, Transaction};

use crate::error::Result;
use crate::models::{ApplyStats, CellRow, CellSpend, IndexBatch, NewCell};
use crate::Store;

/// Column list for the derived-status view. Spelled out so a schema change surfaces
/// as a query error naming the column, not a silent `NULL`.
pub const CELL_COLUMNS: &str = "ckb_tx_hash, output_index, lock_kind, btc_txid, btc_vout, \
     btc_time_after, btc_time_target_lock, lock_hash, lock_args, type_hash, type_script, \
     asset_kind, udt_amount, capacity, cell_data, created_block_number, created_tx_index, \
     consumed_block_number, consumed_tx_hash, btc_status, btc_spender_txid, btc_spent_height, \
     btc_observed_at, btc_address, status";

impl Store {
    /// Apply one scan round atomically.
    ///
    /// Order matters inside the transaction: cells are inserted before spends are
    /// applied, so a cell created and consumed within the same round resolves
    /// correctly. The checkpoint moves last, which is what makes a crash mid-round
    /// replay the round instead of skipping it.
    pub async fn apply_batch(&self, batch: &IndexBatch) -> Result<ApplyStats> {
        let mut tx = self.pool().begin().await?;
        let mut stats = ApplyStats::default();

        for block in &batch.blocks {
            crate::blocks::upsert_block_in(&mut tx, block).await?;
            stats.blocks += 1;
        }

        for cell in &batch.cells {
            insert_cell(&mut tx, cell).await?;
            stats.cells += 1;
        }

        for spend in &batch.spends {
            stats.spends += apply_spend(&mut tx, spend).await?;
        }

        for transition in &batch.transitions {
            crate::transitions::insert_transition(&mut tx, transition).await?;
            stats.transitions += 1;
        }

        sqlx::query(
            "UPDATE indexer_state
                SET last_block_number = $2,
                    last_block_hash = $3,
                    target_block_number = $4,
                    chain_tip_number = $5,
                    reorg_lag = $6,
                    last_error = NULL,
                    updated_at = now()
              WHERE stream = $1",
        )
        .bind(crate::state::CKB_STREAM)
        .bind(batch.checkpoint_number)
        .bind(batch.checkpoint_hash.as_deref())
        .bind(batch.target)
        .bind(batch.chain_tip)
        .bind(batch.reorg_lag)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(stats)
    }

    /// Cells bound to any of the given Bitcoin outpoints.
    pub async fn cells_by_btc_outpoints(
        &self,
        txids: &[Vec<u8>],
        vouts: &[i32],
        include_spent: bool,
    ) -> Result<Vec<CellRow>> {
        let sql = format!(
            "SELECT {CELL_COLUMNS} FROM rgbpp_cell_status
              WHERE (btc_txid, btc_vout) IN (SELECT * FROM UNNEST($1::bytea[], $2::int[]))
                AND ($3 OR consumed_block_number IS NULL)
              ORDER BY created_block_number, created_tx_index, output_index"
        );
        Ok(sqlx::query_as::<Postgres, CellRow>(&sql)
            .bind(txids)
            .bind(vouts)
            .bind(include_spent)
            .fetch_all(self.pool())
            .await?)
    }

    /// Every cell bound to a Bitcoin transaction, regardless of vout.
    pub async fn cells_by_btc_txid(&self, txid: &[u8]) -> Result<Vec<CellRow>> {
        let sql = format!(
            "SELECT {CELL_COLUMNS} FROM rgbpp_cell_status
              WHERE btc_txid = $1
              ORDER BY created_block_number, created_tx_index, output_index"
        );
        Ok(sqlx::query_as::<Postgres, CellRow>(&sql)
            .bind(txid)
            .fetch_all(self.pool())
            .await?)
    }

    pub async fn cell_by_out_point(
        &self,
        ckb_tx_hash: &[u8],
        output_index: i32,
    ) -> Result<Option<CellRow>> {
        let sql = format!(
            "SELECT {CELL_COLUMNS} FROM rgbpp_cell_status
              WHERE ckb_tx_hash = $1 AND output_index = $2"
        );
        Ok(sqlx::query_as::<Postgres, CellRow>(&sql)
            .bind(ckb_tx_hash)
            .bind(output_index)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Cells consumed by a specific CKB transaction.
    pub async fn cells_consumed_by(&self, ckb_tx_hash: &[u8]) -> Result<Vec<CellRow>> {
        let sql = format!(
            "SELECT {CELL_COLUMNS} FROM rgbpp_cell_status
              WHERE consumed_tx_hash = $1
              ORDER BY output_index"
        );
        Ok(sqlx::query_as::<Postgres, CellRow>(&sql)
            .bind(ckb_tx_hash)
            .fetch_all(self.pool())
            .await?)
    }

    /// Whether the indexer already knows a cell at this outpoint.
    pub async fn cell_exists(&self, ckb_tx_hash: &[u8], output_index: i32) -> Result<bool> {
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM rgbpp_cells WHERE ckb_tx_hash = $1 AND output_index = $2",
        )
        .bind(ckb_tx_hash)
        .bind(output_index)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.is_some())
    }

    /// Page through the Bitcoin outpoints backing live RGB++ cells.
    ///
    /// This is the sweeper's work list, derived from the cell table rather than
    /// maintained as a separate watch list that could drift out of sync.
    pub async fn live_bound_outpoints_after(
        &self,
        after_txid: Option<&[u8]>,
        after_vout: i32,
        limit: i64,
    ) -> Result<Vec<(Vec<u8>, i32)>> {
        let rows: Vec<(Vec<u8>, i32)> = sqlx::query_as(
            "SELECT DISTINCT btc_txid, btc_vout
               FROM rgbpp_cells
              WHERE consumed_block_number IS NULL
                AND btc_vout IS NOT NULL
                AND ($1::bytea IS NULL OR (btc_txid, btc_vout) > ($1::bytea, $2::int))
              ORDER BY btc_txid, btc_vout
              LIMIT $3",
        )
        .bind(after_txid)
        .bind(after_vout)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

async fn insert_cell(tx: &mut Transaction<'_, Postgres>, cell: &NewCell) -> Result<()> {
    sqlx::query(
        "INSERT INTO rgbpp_cells (
            ckb_tx_hash, output_index, lock_kind, btc_txid, btc_vout, btc_time_after,
            btc_time_target_lock_hash, btc_time_target_lock, lock_hash, lock_args,
            type_hash, type_script, asset_kind, udt_amount, capacity, cell_data,
            created_block_number, created_block_hash, created_tx_index)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
         ON CONFLICT (ckb_tx_hash, output_index) DO UPDATE SET
            created_block_number = EXCLUDED.created_block_number,
            created_block_hash   = EXCLUDED.created_block_hash,
            created_tx_index     = EXCLUDED.created_tx_index",
    )
    .bind(&cell.ckb_tx_hash)
    .bind(cell.output_index)
    .bind(cell.lock_kind.as_str())
    .bind(&cell.btc_txid)
    .bind(cell.btc_vout)
    .bind(cell.btc_time_after)
    .bind(cell.btc_time_target_lock_hash.as_deref())
    .bind(cell.btc_time_target_lock.as_ref())
    .bind(&cell.lock_hash)
    .bind(&cell.lock_args)
    .bind(cell.type_hash.as_deref())
    .bind(cell.type_script.as_ref())
    .bind(cell.asset_kind.as_str())
    .bind(cell.udt_amount.as_ref())
    .bind(cell.capacity)
    .bind(&cell.cell_data)
    .bind(cell.created_block_number)
    .bind(&cell.created_block_hash)
    .bind(cell.created_tx_index)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Returns the number of rows updated: `0` means we saw a cell consumed that we
/// never recorded as created, which the scanner treats as a gap to backfill.
async fn apply_spend(tx: &mut Transaction<'_, Postgres>, spend: &CellSpend) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE rgbpp_cells
            SET consumed_block_number = $3,
                consumed_block_hash   = $4,
                consumed_tx_hash      = $5,
                consumed_tx_index     = $6,
                consumed_input_index  = $7,
                consumed_at           = now()
          WHERE ckb_tx_hash = $1 AND output_index = $2",
    )
    .bind(&spend.ckb_tx_hash)
    .bind(spend.output_index)
    .bind(spend.consumed_block_number)
    .bind(&spend.consumed_block_hash)
    .bind(&spend.consumed_tx_hash)
    .bind(spend.consumed_tx_index)
    .bind(spend.consumed_input_index)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}
