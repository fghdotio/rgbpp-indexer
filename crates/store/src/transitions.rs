//! RGB++ state transitions (CKB transactions that move protocol state).

use sqlx::{Postgres, Transaction};

use crate::error::Result;
use crate::models::{CommitmentStatus, NewTransition, TransitionRow};
use crate::Store;

const TRANSITION_COLUMNS: &str = "ckb_tx_hash, block_number, block_hash, tx_index, \
     block_timestamp, kind, btc_txid, input_cell_count, output_cell_count, \
     expected_commitment, observed_commitment, commitment_status, indexed_at";

impl Store {
    pub async fn transition_by_ckb_tx(&self, ckb_tx_hash: &[u8]) -> Result<Option<TransitionRow>> {
        let sql = format!("SELECT {TRANSITION_COLUMNS} FROM rgbpp_transitions WHERE ckb_tx_hash = $1");
        Ok(sqlx::query_as::<Postgres, TransitionRow>(&sql)
            .bind(ckb_tx_hash)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Transitions authorised by a Bitcoin transaction. Usually one, but a Bitcoin
    /// transaction can carry commitments for several CKB transactions over time
    /// (for example a retried or replaced CKB submission), so this returns a list.
    pub async fn transitions_by_btc_txid(&self, btc_txid: &[u8]) -> Result<Vec<TransitionRow>> {
        let sql = format!(
            "SELECT {TRANSITION_COLUMNS} FROM rgbpp_transitions
              WHERE btc_txid = $1 ORDER BY block_number, tx_index"
        );
        Ok(sqlx::query_as::<Postgres, TransitionRow>(&sql)
            .bind(btc_txid)
            .fetch_all(self.pool())
            .await?)
    }

    pub async fn recent_transitions(&self, limit: i64) -> Result<Vec<TransitionRow>> {
        let sql = format!(
            "SELECT {TRANSITION_COLUMNS} FROM rgbpp_transitions
              ORDER BY block_number DESC, tx_index DESC LIMIT $1"
        );
        Ok(sqlx::query_as::<Postgres, TransitionRow>(&sql)
            .bind(limit)
            .fetch_all(self.pool())
            .await?)
    }

    /// Record the Bitcoin side of a commitment check.
    pub async fn set_commitment_result(
        &self,
        ckb_tx_hash: &[u8],
        observed: Option<&[u8]>,
        status: CommitmentStatus,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE rgbpp_transitions
                SET observed_commitment = $2, commitment_status = $3
              WHERE ckb_tx_hash = $1",
        )
        .bind(ckb_tx_hash)
        .bind(observed)
        .bind(status.as_str())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Transitions whose Bitcoin side has not been checked yet.
    pub async fn unchecked_transitions(&self, limit: i64) -> Result<Vec<TransitionRow>> {
        let sql = format!(
            "SELECT {TRANSITION_COLUMNS} FROM rgbpp_transitions
              WHERE commitment_status = 'unchecked' AND btc_txid IS NOT NULL
              ORDER BY block_number LIMIT $1"
        );
        Ok(sqlx::query_as::<Postgres, TransitionRow>(&sql)
            .bind(limit)
            .fetch_all(self.pool())
            .await?)
    }
}

pub(crate) async fn insert_transition(
    tx: &mut Transaction<'_, Postgres>,
    transition: &NewTransition,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rgbpp_transitions (
            ckb_tx_hash, block_number, block_hash, tx_index, block_timestamp, kind,
            btc_txid, input_cell_count, output_cell_count, expected_commitment)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         ON CONFLICT (ckb_tx_hash) DO UPDATE SET
            block_number        = EXCLUDED.block_number,
            block_hash          = EXCLUDED.block_hash,
            tx_index            = EXCLUDED.tx_index,
            block_timestamp     = EXCLUDED.block_timestamp,
            kind                = EXCLUDED.kind,
            btc_txid            = EXCLUDED.btc_txid,
            input_cell_count    = EXCLUDED.input_cell_count,
            output_cell_count   = EXCLUDED.output_cell_count,
            expected_commitment = EXCLUDED.expected_commitment",
    )
    .bind(&transition.ckb_tx_hash)
    .bind(transition.block_number)
    .bind(&transition.block_hash)
    .bind(transition.tx_index)
    .bind(transition.block_timestamp)
    .bind(transition.kind.as_str())
    .bind(transition.btc_txid.as_deref())
    .bind(transition.input_cell_count)
    .bind(transition.output_cell_count)
    .bind(transition.expected_commitment.as_deref())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
