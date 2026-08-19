//! The refresh queue.
//!
//! Applications and the sweeper both push work here; a single background worker
//! drains it. Centralising the drain is what keeps a burst of API traffic from
//! turning into a burst of requests at the Bitcoin data source.

use sqlx::Postgres;

use crate::error::Result;
use crate::models::QueueItem;
use crate::Store;

/// Lower runs first.
pub mod priority {
    /// An application is waiting on this answer right now.
    pub const ON_DEMAND: i32 = 10;
    /// Follow-up on something we already believe is mid-transition.
    pub const PENDING_FOLLOW_UP: i32 = 50;
    /// Background sweep.
    pub const SWEEP: i32 = 100;
}

impl Store {
    /// Queue an outpoint for re-checking. Re-queuing at a higher urgency promotes
    /// the existing entry and makes it eligible immediately.
    pub async fn enqueue_refresh(
        &self,
        txid: &[u8],
        vout: i32,
        reason: &str,
        priority: i32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO btc_refresh_queue (txid, vout, reason, priority, requested_at, next_attempt_at)
             VALUES ($1, $2, $3, $4, now(), now())
             ON CONFLICT (txid, vout) DO UPDATE SET
                reason          = EXCLUDED.reason,
                priority        = LEAST(btc_refresh_queue.priority, EXCLUDED.priority),
                next_attempt_at = LEAST(btc_refresh_queue.next_attempt_at, EXCLUDED.next_attempt_at)",
        )
        .bind(txid)
        .bind(vout)
        .bind(reason)
        .bind(priority)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn enqueue_refresh_many(
        &self,
        outpoints: &[(Vec<u8>, i32)],
        reason: &str,
        priority: i32,
    ) -> Result<u64> {
        if outpoints.is_empty() {
            return Ok(0);
        }
        let txids: Vec<Vec<u8>> = outpoints.iter().map(|(t, _)| t.clone()).collect();
        let vouts: Vec<i32> = outpoints.iter().map(|(_, v)| *v).collect();
        let result = sqlx::query(
            "INSERT INTO btc_refresh_queue (txid, vout, reason, priority, requested_at, next_attempt_at)
             SELECT t, v, $3, $4, now(), now() FROM UNNEST($1::bytea[], $2::int[]) AS u(t, v)
             ON CONFLICT (txid, vout) DO UPDATE SET
                priority        = LEAST(btc_refresh_queue.priority, EXCLUDED.priority),
                next_attempt_at = LEAST(btc_refresh_queue.next_attempt_at, EXCLUDED.next_attempt_at)",
        )
        .bind(&txids)
        .bind(&vouts)
        .bind(reason)
        .bind(priority)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Take a batch of due work.
    ///
    /// `SKIP LOCKED` keeps several workers (or a restarted one) from fighting over
    /// the same rows, and the backoff is applied at claim time so a worker that dies
    /// mid-request does not leave an item spinning.
    pub async fn claim_refresh_batch(&self, limit: i64) -> Result<Vec<QueueItem>> {
        Ok(sqlx::query_as::<Postgres, QueueItem>(
            "WITH picked AS (
                 SELECT txid, vout FROM btc_refresh_queue
                  WHERE next_attempt_at <= now()
                  ORDER BY priority, next_attempt_at
                  LIMIT $1
                  FOR UPDATE SKIP LOCKED
             )
             UPDATE btc_refresh_queue q
                SET attempts = q.attempts + 1,
                    next_attempt_at = now()
                        + make_interval(secs => LEAST(300, POWER(2, q.attempts + 1))::double precision)
               FROM picked
              WHERE q.txid = picked.txid AND q.vout = picked.vout
              RETURNING q.txid, q.vout, q.reason, q.priority, q.attempts",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn complete_refresh(&self, txid: &[u8], vout: i32) -> Result<()> {
        sqlx::query("DELETE FROM btc_refresh_queue WHERE txid = $1 AND vout = $2")
            .bind(txid)
            .bind(vout)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn fail_refresh(&self, txid: &[u8], vout: i32, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE btc_refresh_queue SET last_error = $3 WHERE txid = $1 AND vout = $2",
        )
        .bind(txid)
        .bind(vout)
        .bind(error)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Drop entries that have exhausted their retries, returning what was dropped so
    /// the caller can record it rather than losing it silently.
    pub async fn drop_exhausted_refreshes(&self, max_attempts: i32) -> Result<Vec<QueueItem>> {
        Ok(sqlx::query_as::<Postgres, QueueItem>(
            "DELETE FROM btc_refresh_queue WHERE attempts >= $1
             RETURNING txid, vout, reason, priority, attempts",
        )
        .bind(max_attempts)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn refresh_queue_depth(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM btc_refresh_queue")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }
}
