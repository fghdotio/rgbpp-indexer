//! Findings that need a human.
//!
//! Anomalies are deduplicated on a caller-supplied key so a daily sweep re-detecting
//! the same problem updates one row instead of growing an unbounded log. A finding
//! that reappears after being resolved is reopened rather than silently ignored.

use rgbpp_types::state::AnomalyKind;
use sqlx::Postgres;

use crate::error::Result;
use crate::models::AnomalyRow;
use crate::Store;

const ANOMALY_COLUMNS: &str = "id, kind, dedup_key, btc_txid, btc_vout, ckb_tx_hash, \
     ckb_output_index, detail, detected_at, last_seen_at, resolved_at";

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub async fn record_anomaly(
        &self,
        kind: AnomalyKind,
        dedup_key: &str,
        btc_txid: Option<&[u8]>,
        btc_vout: Option<i32>,
        ckb_tx_hash: Option<&[u8]>,
        ckb_output_index: Option<i32>,
        detail: serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO rgbpp_anomalies (kind, dedup_key, btc_txid, btc_vout, ckb_tx_hash,
                                          ckb_output_index, detail)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (dedup_key) DO UPDATE SET
                detail       = EXCLUDED.detail,
                last_seen_at = now(),
                resolved_at  = NULL",
        )
        .bind(kind.as_str())
        .bind(dedup_key)
        .bind(btc_txid)
        .bind(btc_vout)
        .bind(ckb_tx_hash)
        .bind(ckb_output_index)
        .bind(detail)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn resolve_anomaly(&self, dedup_key: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE rgbpp_anomalies SET resolved_at = now()
              WHERE dedup_key = $1 AND resolved_at IS NULL",
        )
        .bind(dedup_key)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Resolve every open anomaly attached to a bound outpoint.
    ///
    /// Called when the matching CKB transition finally shows up: the finding was
    /// real at the time and is now genuinely closed.
    pub async fn resolve_anomalies_for_outpoint(&self, txid: &[u8], vout: i32) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE rgbpp_anomalies SET resolved_at = now()
              WHERE resolved_at IS NULL AND btc_txid = $1 AND btc_vout = $2",
        )
        .bind(txid)
        .bind(vout)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn list_anomalies(
        &self,
        kind: Option<&str>,
        include_resolved: bool,
        limit: i64,
    ) -> Result<Vec<AnomalyRow>> {
        let sql = format!(
            "SELECT {ANOMALY_COLUMNS} FROM rgbpp_anomalies
              WHERE ($1::text IS NULL OR kind = $1)
                AND ($2 OR resolved_at IS NULL)
              ORDER BY last_seen_at DESC
              LIMIT $3"
        );
        Ok(sqlx::query_as::<Postgres, AnomalyRow>(&sql)
            .bind(kind)
            .bind(include_resolved)
            .bind(limit)
            .fetch_all(self.pool())
            .await?)
    }

    pub async fn open_anomaly_count(&self) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM rgbpp_anomalies WHERE resolved_at IS NULL")
                .fetch_one(self.pool())
                .await?;
        Ok(count)
    }
}

/// Stable identity for an anomaly, so re-detection updates one row.
pub fn dedup_key(kind: AnomalyKind, subject: &str) -> String {
    format!("{}:{}", kind.as_str(), subject)
}
