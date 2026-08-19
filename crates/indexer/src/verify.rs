//! Commitment cross-checking.
//!
//! Discovery never depends on this. The indexer records what both chains say and
//! compares them; a mismatch produces an anomaly, never a dropped fact. That ordering
//! is deliberate — an indexer that silently discards on-chain state because its own
//! commitment computation disagreed would be worse than one that reports the
//! disagreement.
//!
//! Off by default. Enable `verify.commitments` once the computed values have been
//! checked against known-good transactions on the network being indexed.

use std::sync::Arc;

use rgbpp_types::bitcoin::BtcTxid;
use rgbpp_types::config::Config;
use rgbpp_types::state::{AnomalyKind, TransitionKind};
use rgbpp_store::anomalies::dedup_key;
use rgbpp_store::models::CommitmentStatus;
use rgbpp_store::Store;
use serde::Serialize;
use tracing::{info, warn};

use crate::error::Result;
use crate::reconcile::Reconciler;
use crate::shutdown::Shutdown;

pub struct CommitmentVerifier {
    store: Store,
    reconciler: Reconciler,
    config: Arc<Config>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VerifyReport {
    pub checked: u64,
    pub matched: u64,
    pub mismatched: u64,
    pub btc_unknown: u64,
    pub missing_commitment: u64,
    pub not_applicable: u64,
}

impl CommitmentVerifier {
    pub fn new(store: Store, reconciler: Reconciler, config: Arc<Config>) -> Self {
        CommitmentVerifier {
            store,
            reconciler,
            config,
        }
    }

    pub async fn run(self, mut shutdown: Shutdown) {
        loop {
            match self.verify_once().await {
                Ok(report) if report.checked > 0 => info!(
                    checked = report.checked,
                    matched = report.matched,
                    mismatched = report.mismatched,
                    missing = report.missing_commitment,
                    btc_unknown = report.btc_unknown,
                    skipped = report.not_applicable,
                    "commitments"
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "commitment verification failed"),
            }
            if shutdown.sleep(self.config.verify.interval()).await {
                return;
            }
        }
    }

    pub async fn verify_once(&self) -> Result<VerifyReport> {
        let transitions = self
            .store
            .unchecked_transitions(self.config.verify.batch_size)
            .await?;
        let mut report = VerifyReport::default();

        for transition in transitions {
            let Some(btc_txid) = transition.btc_txid.as_deref() else {
                continue;
            };

            // Issuance (and leaping from CKB to Bitcoin) binds cells to a Bitcoin UTXO
            // that already existed. The transaction that created that UTXO knows
            // nothing about RGB++ and commits to nothing, so comparing against it
            // would manufacture a mismatch on every mint.
            if transition.kind == TransitionKind::Issuance.as_str() {
                self.store
                    .set_commitment_result(
                        &transition.ckb_tx_hash,
                        None,
                        CommitmentStatus::NotApplicable,
                    )
                    .await?;
                report.not_applicable += 1;
                report.checked += 1;
                continue;
            }

            let txid = BtcTxid::from_display_slice(btc_txid)?;

            let Some(btc_tx) = self.reconciler.refresh_btc_tx(&txid).await? else {
                self.store
                    .set_commitment_result(
                        &transition.ckb_tx_hash,
                        None,
                        CommitmentStatus::BtcUnknown,
                    )
                    .await?;
                self.record(
                    AnomalyKind::UnknownBtcTx,
                    &transition.ckb_tx_hash,
                    Some(btc_txid),
                    serde_json::json!({ "btc_txid": txid.to_hex() }),
                )
                .await?;
                report.btc_unknown += 1;
                report.checked += 1;
                continue;
            };

            let observed = btc_tx.commitment();
            let status = match (transition.expected_commitment.as_deref(), observed) {
                (_, None) => {
                    report.missing_commitment += 1;
                    CommitmentStatus::Missing
                }
                (Some(expected), Some(observed)) if expected == observed => {
                    report.matched += 1;
                    CommitmentStatus::Match
                }
                (Some(expected), Some(observed)) => {
                    report.mismatched += 1;
                    warn!(
                        ckb_tx = %hex::encode(&transition.ckb_tx_hash),
                        btc_tx = %txid,
                        expected = %hex::encode(expected),
                        observed = %hex::encode(observed),
                        "commitment mismatch"
                    );
                    self.record(
                        AnomalyKind::CommitmentMismatch,
                        &transition.ckb_tx_hash,
                        Some(btc_txid),
                        serde_json::json!({
                            "expected": hex::encode(expected),
                            "observed": hex::encode(observed),
                            "btc_txid": txid.to_hex(),
                        }),
                    )
                    .await?;
                    CommitmentStatus::Mismatch
                }
                (None, Some(_)) => CommitmentStatus::Unchecked,
            };

            self.store
                .set_commitment_result(
                    &transition.ckb_tx_hash,
                    observed.as_ref().map(|c| &c[..]),
                    status,
                )
                .await?;
            report.checked += 1;
        }

        Ok(report)
    }

    async fn record(
        &self,
        kind: AnomalyKind,
        ckb_tx_hash: &[u8],
        btc_txid: Option<&[u8]>,
        detail: serde_json::Value,
    ) -> Result<()> {
        self.store
            .record_anomaly(
                kind,
                &dedup_key(kind, &hex::encode(ckb_tx_hash)),
                btc_txid,
                None,
                Some(ckb_tx_hash),
                None,
                detail,
            )
            .await?;
        Ok(())
    }
}
