//! CKB header bookkeeping.
//!
//! Headers are stored sparsely — activity blocks plus checkpoints — which is enough
//! to walk backwards looking for a common ancestor without paying one RPC per block
//! during initial sync.

use sqlx::Postgres;

use crate::error::Result;
use crate::models::{BlockRecord, BlockRow};
use crate::Store;

impl Store {
    pub async fn get_block(&self, number: i64) -> Result<Option<BlockRow>> {
        Ok(sqlx::query_as::<Postgres, BlockRow>(
            "SELECT number, hash, parent_hash, block_timestamp, has_rgbpp_activity
               FROM ckb_blocks WHERE number = $1",
        )
        .bind(number)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Stored headers at or below `number`, newest first — the walk-back sequence a
    /// reorg handler consumes to find the last block both chains agree on.
    pub async fn blocks_descending_from(&self, number: i64, limit: i64) -> Result<Vec<BlockRow>> {
        Ok(sqlx::query_as::<Postgres, BlockRow>(
            "SELECT number, hash, parent_hash, block_timestamp, has_rgbpp_activity
               FROM ckb_blocks
              WHERE number <= $1
              ORDER BY number DESC
              LIMIT $2",
        )
        .bind(number)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn upsert_block(&self, block: &BlockRecord) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        upsert_block_in(&mut tx, block).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drop headers below `keep_from`. Retention only needs to cover the depth a
    /// reorg could plausibly reach.
    pub async fn prune_blocks_below(&self, keep_from: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM ckb_blocks WHERE number < $1")
            .bind(keep_from)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

pub(crate) async fn upsert_block_in(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    block: &BlockRecord,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ckb_blocks (number, hash, parent_hash, block_timestamp, has_rgbpp_activity)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (number) DO UPDATE
            SET hash = EXCLUDED.hash,
                parent_hash = EXCLUDED.parent_hash,
                block_timestamp = EXCLUDED.block_timestamp,
                -- Never downgrade an activity flag: a later checkpoint write must not
                -- erase the fact that this block carried RGB++ transactions.
                has_rgbpp_activity = ckb_blocks.has_rgbpp_activity OR EXCLUDED.has_rgbpp_activity",
    )
    .bind(block.number)
    .bind(&block.hash)
    .bind(&block.parent_hash)
    .bind(block.timestamp)
    .bind(block.has_rgbpp_activity)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
