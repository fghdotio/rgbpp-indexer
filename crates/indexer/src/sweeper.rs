//! The daily sweep over every live binding.
//!
//! Exists for the case nobody polls for: a bound UTXO spent by a wallet that did not
//! know it held an RGB++ asset. See `docs/indexing.md`.

use std::sync::Arc;

use rgbpp_store::anomalies::dedup_key;
use rgbpp_store::Store;
use rgbpp_types::bitcoin::{BtcOutPoint, BtcTxid};
use rgbpp_types::config::Config;
use rgbpp_types::state::AnomalyKind;
use serde::Serialize;
use tracing::{info, warn};

use crate::error::Result;
use crate::reconcile::Reconciler;
use crate::shutdown::Shutdown;

pub struct Sweeper {
    store: Store,
    reconciler: Reconciler,
    config: Arc<Config>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SweepReport {
    pub outpoints_checked: u64,
    pub status_changed: u64,
    pub anomalies_found: u64,
}

impl Sweeper {
    pub fn new(store: Store, reconciler: Reconciler, config: Arc<Config>) -> Self {
        Sweeper {
            store,
            reconciler,
            config,
        }
    }

    pub async fn run(self, mut shutdown: Shutdown) {
        let interval = std::time::Duration::from_secs(self.config.sweep.interval_secs);

        loop {
            // Sleep first: a restart loop should not re-run a full sweep every time.
            if shutdown.sleep(interval).await {
                return;
            }
            let started = std::time::Instant::now();
            info!("sweep started");
            match self.sweep_once().await {
                Ok(report) => info!(
                    checked = report.outpoints_checked,
                    changed = report.status_changed,
                    anomalies = report.anomalies_found,
                    took = %crate::progress::human_duration(started.elapsed().as_secs()),
                    "sweep complete"
                ),
                Err(e) => warn!(error = %e, "sweep failed"),
            }
        }
    }

    /// Walk every live binding, then look for spends that never landed on CKB.
    pub async fn sweep_once(&self) -> Result<SweepReport> {
        let run_id = self.store.start_sweep_run().await?;
        let mut report = SweepReport::default();

        let result = self.sweep_inner(&mut report).await;

        let error = result.as_ref().err().map(|e| e.to_string());
        self.store
            .finish_sweep_run(
                run_id,
                report.outpoints_checked as i64,
                report.status_changed as i64,
                report.anomalies_found as i64,
                error.as_deref(),
            )
            .await?;

        result.map(|_| report)
    }

    async fn sweep_inner(&self, report: &mut SweepReport) -> Result<()> {
        let batch_size = self.config.sweep.batch_size;
        let mut cursor: Option<(Vec<u8>, i32)> = None;

        loop {
            let page = self
                .store
                .live_bound_outpoints_after(
                    cursor.as_ref().map(|(t, _)| t.as_slice()),
                    cursor.as_ref().map(|(_, v)| *v).unwrap_or(0),
                    batch_size,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().cloned();

            let outpoints: Vec<BtcOutPoint> = page
                .iter()
                .map(|(txid, vout)| {
                    Ok(BtcOutPoint::new(
                        BtcTxid::from_display_slice(txid)?,
                        *vout as u32,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;

            let refreshed = self.reconciler.refresh_outpoints(&outpoints).await?;
            report.outpoints_checked += refreshed.len() as u64;
            report.status_changed += refreshed.iter().filter(|r| r.changed).count() as u64;
        }

        report.anomalies_found = self.detect_misspends().await?;
        Ok(())
    }

    /// Misspend detection is only sound once the CKB side is close to its target.
    ///
    /// The whole signal is "Bitcoin says spent, CKB has no matching transition". While
    /// the indexer is still catching up, the second half of that is unknowable, and
    /// running anyway would flag every historical binding.
    async fn indexer_is_caught_up(&self) -> Result<bool> {
        let Some(state) = self
            .store
            .get_stream_state(rgbpp_store::state::CKB_STREAM)
            .await?
        else {
            return Ok(false);
        };
        let Some(target) = state.target_block_number else {
            return Ok(false);
        };
        let behind = target.saturating_sub(state.last_block_number).max(0) as u64;
        let allowed = self.config.sweep.require_synced_within_blocks;

        if behind > allowed {
            warn!(
                behind,
                allowed,
                indexed_to = state.last_block_number,
                target,
                "skipping misspend detection: the ckb index is too far behind for \
                 \"no transition exists\" to mean anything"
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// Flag bound UTXOs spent on Bitcoin with no CKB transition to match.
    ///
    /// The grace period is measured in Bitcoin confirmations rather than wall time,
    /// because that is the unit the risk is actually in: a CKB transaction that is
    /// going to be submitted is submitted within a few Bitcoin blocks.
    pub async fn detect_misspends(&self) -> Result<u64> {
        if !self.indexer_is_caught_up().await? {
            return Ok(0);
        }

        let tip = self.reconciler.btc().tip().await?;
        let grace = self.config.sweep.misspend_grace_confirmations;
        let pending = self.store.pending_ckb_outpoints(10_000).await?;

        let mut found = 0;
        for row in pending {
            let outpoint = row.out_point()?;
            let status = row.spend_status()?;
            let Some(spender) = status.spender() else {
                continue;
            };

            // Unconfirmed spends are still perfectly normal in-flight transfers.
            let Some(height) = status.height() else {
                continue;
            };
            let confirmations = tip.height.saturating_sub(height) + 1;
            if confirmations < grace {
                continue;
            }

            // A transition may have been indexed since the observation was written.
            let transitions = self
                .store
                .transitions_by_btc_txid(&spender.to_display_vec())
                .await?;
            if !transitions.is_empty() {
                self.store
                    .resolve_anomalies_for_outpoint(
                        &outpoint.txid.to_display_vec(),
                        outpoint.vout as i32,
                    )
                    .await?;
                continue;
            }

            // Whether the spender carries an RGB++ commitment separates "the CKB side
            // is late" from "a wallet spent this UTXO without knowing what it was".
            let commitment = self
                .reconciler
                .refresh_btc_tx(&spender)
                .await?
                .and_then(|tx| tx.commitment());
            let kind = if commitment.is_some() {
                AnomalyKind::BtcSpentWithoutCkb
            } else {
                AnomalyKind::NoCommitmentInSpender
            };

            let detail = serde_json::json!({
                "spender_txid": spender.to_hex(),
                "spent_height": height,
                "confirmations": confirmations,
                "grace_confirmations": grace,
                "spender_has_rgbpp_commitment": commitment.is_some(),
            });

            warn!(
                %outpoint,
                %spender,
                confirmations,
                kind = kind.as_str(),
                "bound utxo spent on bitcoin with no matching ckb transition"
            );

            self.store
                .record_anomaly(
                    kind,
                    &dedup_key(kind, &outpoint.to_string()),
                    Some(&outpoint.txid.to_display_vec()),
                    Some(outpoint.vout as i32),
                    None,
                    None,
                    detail,
                )
                .await?;
            found += 1;
        }

        Ok(found)
    }
}
