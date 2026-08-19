//! Indexer stream checkpoints.

use chrono::Utc;
use sqlx::Postgres;

use crate::error::Result;
use crate::models::StreamState;
use crate::Store;

/// The CKB discovery stream. Named because more streams are plausible later
/// (a second network, a backfill worker) and the table is keyed by name.
pub const CKB_STREAM: &str = "ckb";

impl Store {
    pub async fn get_stream_state(&self, stream: &str) -> Result<Option<StreamState>> {
        let row = sqlx::query_as::<Postgres, StreamState>(
            "SELECT stream, last_block_number, last_block_hash, target_block_number,
                    chain_tip_number, reorg_lag, last_error, updated_at
               FROM indexer_state
              WHERE stream = $1",
        )
        .bind(stream)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Create the checkpoint row if absent.
    ///
    /// `start_block` is stored as `start_block - 1`: the checkpoint means "fully
    /// processed up to and including this height", and nothing has been processed yet.
    pub async fn init_stream(&self, stream: &str, start_block: u64) -> Result<StreamState> {
        let initial = start_block.saturating_sub(1) as i64;
        sqlx::query(
            "INSERT INTO indexer_state (stream, last_block_number, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (stream) DO NOTHING",
        )
        .bind(stream)
        .bind(initial)
        .bind(Utc::now())
        .execute(self.pool())
        .await?;

        self.get_stream_state(stream)
            .await?
            .ok_or_else(|| crate::StoreError::mapping("stream row vanished after insert"))
    }

    pub async fn set_stream_error(&self, stream: &str, error: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE indexer_state SET last_error = $2, updated_at = $3 WHERE stream = $1",
        )
        .bind(stream)
        .bind(error)
        .bind(Utc::now())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Record observed chain progress without moving the checkpoint.
    ///
    /// Used when the scanner is idle (already caught up to `tip - lag`) so `/status`
    /// still reports a live view of how far behind the chain we are.
    pub async fn record_chain_progress(
        &self,
        stream: &str,
        chain_tip: u64,
        target: u64,
        reorg_lag: u64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE indexer_state
                SET chain_tip_number = $2, target_block_number = $3,
                    reorg_lag = $4, updated_at = $5
              WHERE stream = $1",
        )
        .bind(stream)
        .bind(chain_tip as i64)
        .bind(target as i64)
        .bind(reorg_lag as i64)
        .bind(Utc::now())
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
