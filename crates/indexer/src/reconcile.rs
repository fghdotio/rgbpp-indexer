//! Bitcoin-driven reconciliation — the path that closes the CKB blind spot.
//!
//! The CKB scanner only sees transitions once they are `REORG_LAG` blocks deep, and
//! it cannot see a transition at all until the CKB transaction is committed. A
//! Bitcoin transaction that spends a bound UTXO can be broadcast, and even confirmed,
//! well before either of those. Nothing on the CKB side will announce it.
//!
//! So the application tells us where to look, and this module does the looking:
//!
//! * **Address-level diff** — when an application asks for an address's UTXOs, the
//!   live set from the Bitcoin data source is diffed against the outpoints the
//!   indexer still believes are live. Anything that disappeared has moved on
//!   Bitcoin, and its stale state is refreshed on the spot.
//! * **Point refresh** — when an application polls one RGB++ transaction, exactly the
//!   outpoints that transaction touches get re-observed.
//!
//! Both paths only ever *refresh observations*. They never write CKB facts, which is
//! what keeps the two durability models from leaking into each other.

use std::collections::HashSet;
use std::sync::Arc;

use rgbpp_btc::{BtcDataSource, BtcTxInfo};
use rgbpp_ckb::types::SearchKey;
use rgbpp_ckb::CkbClient;
use rgbpp_store::queue::priority;
use rgbpp_store::Store;
use rgbpp_types::bitcoin::{BtcOutPoint, BtcTxid};
use rgbpp_types::ckb::{CkbOutPoint, Script};
use rgbpp_types::config::Config;
use rgbpp_types::protocol::RgbppLockArgs;
use rgbpp_types::state::OutpointSpendStatus;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::shutdown::Shutdown;

/// How many Bitcoin outputs to probe on CKB when the data source cannot tell us how
/// many the transaction actually has.
const DEFAULT_POINT_LOOKUP_VOUTS: u32 = 4;
/// Upper bound on point lookups for one transaction, so a large Bitcoin transaction
/// cannot turn one API call into hundreds of RPCs.
const MAX_POINT_LOOKUP_VOUTS: u32 = 32;

#[derive(Clone)]
pub struct Reconciler {
    store: Store,
    btc: Arc<dyn BtcDataSource>,
    ckb: Arc<CkbClient>,
    config: Arc<Config>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutpointRefresh {
    pub outpoint: BtcOutPoint,
    pub status: OutpointSpendStatus,
    /// Whether this observation differs from what we previously held.
    pub changed: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AddressReconcile {
    pub address: String,
    /// Outpoints currently unspent according to the Bitcoin data source.
    pub source_utxo_count: usize,
    /// We believed these were live; the data source no longer lists them. These are
    /// the state changes the diff exists to catch.
    pub disappeared: Vec<BtcOutPoint>,
    /// We recorded these as spent, yet the data source still lists them as unspent —
    /// a replaced transaction or a Bitcoin reorg.
    pub contradicted: Vec<BtcOutPoint>,
    pub refreshed: Vec<OutpointRefresh>,
}

/// Where an RGB++ transaction has got to, answered across both chains.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum TransitionResolution {
    /// The Bitcoin data source has never heard of this transaction.
    UnknownBtcTx,
    /// Seen on Bitcoin; no CKB transaction found yet, anywhere.
    AwaitingCkb {
        btc_confirmed: bool,
        btc_height: Option<u32>,
    },
    /// Cells for this transaction exist on CKB, but above the indexed range. The
    /// transition is real; the indexer simply has not reached it yet.
    CkbSeenAboveLag {
        cells: Vec<CkbOutPoint>,
        indexed_to: i64,
    },
    /// Indexed: a CKB transition is recorded for this Bitcoin transaction.
    Indexed {
        ckb_tx_hash: String,
        block_number: i64,
    },
}

impl Reconciler {
    pub fn new(
        store: Store,
        btc: Arc<dyn BtcDataSource>,
        ckb: Arc<CkbClient>,
        config: Arc<Config>,
    ) -> Self {
        Reconciler {
            store,
            btc,
            ckb,
            config,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn btc(&self) -> &Arc<dyn BtcDataSource> {
        &self.btc
    }

    /// Re-observe a set of outpoints and write the results.
    pub async fn refresh_outpoints(
        &self,
        outpoints: &[BtcOutPoint],
    ) -> Result<Vec<OutpointRefresh>> {
        if outpoints.is_empty() {
            return Ok(Vec::new());
        }
        let capped = &outpoints[..outpoints
            .len()
            .min(self.config.reconcile.max_outpoints_per_request)];
        if capped.len() < outpoints.len() {
            // Queue the overflow rather than dropping it: the caller gets a fast
            // answer now, and the rest is picked up by the drain worker.
            let overflow: Vec<(Vec<u8>, i32)> = outpoints[capped.len()..]
                .iter()
                .map(|o| (o.txid.to_display_vec(), o.vout as i32))
                .collect();
            self.store
                .enqueue_refresh_many(&overflow, "request-overflow", priority::ON_DEMAND)
                .await?;
        }

        let observations =
            rgbpp_btc::observe_many(self.btc.as_ref(), capped, self.config.btc.max_concurrency)
                .await;

        let mut out = Vec::with_capacity(observations.len());
        let mut failed = 0usize;
        let mut newly_spent = 0usize;
        for (outpoint, result) in observations {
            match result {
                Ok(status) => {
                    let changed = self.record_observation(&outpoint, status, None).await?;
                    if changed && status.is_spent() {
                        newly_spent += 1;
                        self.follow_up_spend(&outpoint, status).await?;
                    }
                    if !status.is_spent() {
                        // The cell is unambiguously live again: any pending-spend
                        // finding attached to it no longer holds.
                        self.store
                            .resolve_anomalies_for_outpoint(
                                &outpoint.txid.to_display_vec(),
                                outpoint.vout as i32,
                            )
                            .await?;
                    }
                    out.push(OutpointRefresh {
                        outpoint,
                        status,
                        changed,
                    });
                }
                Err(e) => {
                    failed += 1;
                    warn!(%outpoint, error = %e, "outpoint refresh failed; queued for retry");
                    self.store
                        .enqueue_refresh(
                            &outpoint.txid.to_display_vec(),
                            outpoint.vout as i32,
                            "refresh-failed",
                            priority::ON_DEMAND,
                        )
                        .await?;
                }
            }
        }

        // Only worth a line when the world moved; a refresh that confirms what we
        // already knew is the common case and says nothing.
        let changed = out.iter().filter(|r| r.changed).count();
        if changed > 0 || failed > 0 {
            info!(
                checked = out.len(),
                changed,
                newly_spent,
                failed,
                src = self.btc.name(),
                "btc refresh"
            );
        }
        Ok(out)
    }

    async fn record_observation(
        &self,
        outpoint: &BtcOutPoint,
        status: OutpointSpendStatus,
        address: Option<&str>,
    ) -> Result<bool> {
        Ok(self
            .store
            .upsert_observation(
                &outpoint.txid.to_display_vec(),
                outpoint.vout as i32,
                status,
                address,
                self.btc.name(),
            )
            .await?)
    }

    /// A newly observed spend: record the spending transaction so its commitment and
    /// confirmation state are available without another round trip later.
    async fn follow_up_spend(
        &self,
        outpoint: &BtcOutPoint,
        status: OutpointSpendStatus,
    ) -> Result<()> {
        let Some(spender) = status.spender() else {
            return Ok(());
        };
        debug!(%outpoint, %spender, "bound utxo was spent on bitcoin");
        self.refresh_btc_tx(&spender).await?;
        Ok(())
    }

    /// Fetch and cache a Bitcoin transaction, including any RGB++ commitment it
    /// publishes.
    pub async fn refresh_btc_tx(&self, txid: &BtcTxid) -> Result<Option<BtcTxInfo>> {
        let Some(tx) = self.btc.transaction(txid).await? else {
            return Ok(None);
        };
        let commitment = tx.commitment();
        self.store
            .upsert_btc_tx(
                &txid.to_display_vec(),
                tx.confirmation.map(|c| c.height as i32),
                tx.confirmation.map(|c| c.hash.to_vec()).as_deref(),
                tx.confirmation
                    .and_then(|c| chrono::DateTime::from_timestamp(c.time, 0)),
                commitment.as_ref().map(|c| &c[..]),
                tx.inputs.len() as i32,
                tx.outputs.len() as i32,
                self.btc.name(),
            )
            .await?;
        Ok(Some(tx))
    }

    /// The address-level diff.
    ///
    /// Note what is *not* done here: nothing is marked spent because it is missing
    /// from the data source's UTXO list. Absence is only a hint about where to look;
    /// the actual status still comes from asking the data source about the outpoint.
    /// A UTXO list from a lagging or partially-synced backend would otherwise be
    /// enough to mass-invalidate live cells.
    pub async fn reconcile_address(&self, address: &str) -> Result<AddressReconcile> {
        let utxos = self.btc.address_utxos(address).await?;
        let live_now: HashSet<BtcOutPoint> = utxos.iter().map(|u| u.outpoint).collect();

        // Remember which outpoints belong to this address, so the next diff has a
        // "before" to compare against.
        let known: Vec<(Vec<u8>, i32)> = live_now
            .iter()
            .map(|o| (o.txid.to_display_vec(), o.vout as i32))
            .collect();
        self.store
            .record_outpoint_addresses(
                &known.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
                &known.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
                address,
            )
            .await?;

        let believed_live = self.store.live_bound_outpoints_for_address(address).await?;
        let mut disappeared = Vec::new();
        for (txid, vout) in &believed_live {
            let outpoint = BtcOutPoint::new(BtcTxid::from_display_slice(txid)?, *vout as u32);
            if !live_now.contains(&outpoint) {
                disappeared.push(outpoint);
            }
        }

        // The mirror case: we hold a spent observation for something the data source
        // still lists as unspent. A replacement or a Bitcoin reorg looks like this.
        let mut contradicted = Vec::new();
        let txids: Vec<Vec<u8>> = live_now.iter().map(|o| o.txid.to_display_vec()).collect();
        let vouts: Vec<i32> = live_now.iter().map(|o| o.vout as i32).collect();
        for row in self.store.get_observations(&txids, &vouts).await? {
            if row.spend_status()?.is_spent() && row.invalidated_at.is_none() {
                contradicted.push(row.out_point()?);
            }
        }

        let mut to_refresh: Vec<BtcOutPoint> = disappeared.clone();
        to_refresh.extend(contradicted.iter().copied());
        to_refresh.sort();
        to_refresh.dedup();

        // A clean diff is the expected outcome for a polling client, so it stays at
        // debug; a diff that found something is what an operator wants to see.
        if disappeared.is_empty() && contradicted.is_empty() {
            debug!(address, utxos = live_now.len(), "address diff clean");
        } else {
            info!(
                address,
                utxos = live_now.len(),
                disappeared = disappeared.len(),
                contradicted = contradicted.len(),
                bound = believed_live.len(),
                "address diff found stale state"
            );
        }

        let refreshed = self.refresh_outpoints(&to_refresh).await?;

        Ok(AddressReconcile {
            address: address.to_string(),
            source_utxo_count: live_now.len(),
            disappeared,
            contradicted,
            refreshed,
        })
    }

    /// Answer "where is my RGB++ transaction" across both chains, refreshing the
    /// outpoints it touches on the way.
    pub async fn resolve_transition(&self, txid: &BtcTxid) -> Result<TransitionResolution> {
        let txid_bytes = txid.to_display_vec();

        // Already indexed: nothing else to ask.
        let transitions = self.store.transitions_by_btc_txid(&txid_bytes).await?;
        if let Some(transition) = transitions.first() {
            return Ok(TransitionResolution::Indexed {
                ckb_tx_hash: format!("0x{}", hex::encode(&transition.ckb_tx_hash)),
                block_number: transition.block_number,
            });
        }

        let btc_tx = self.refresh_btc_tx(txid).await?;
        let Some(btc_tx) = btc_tx else {
            return Ok(TransitionResolution::UnknownBtcTx);
        };

        // The point refresh: exactly the outpoints this transaction consumes.
        if !btc_tx.inputs.is_empty() {
            self.refresh_outpoints(&btc_tx.inputs).await?;
        }

        // Look past the lag: ask the rich indexer directly whether cells bound to
        // this transaction's outputs exist right now.
        let vout_count = (btc_tx.outputs.len() as u32).min(MAX_POINT_LOOKUP_VOUTS);
        let cells = self.point_lookup_cells(txid, vout_count).await?;
        if !cells.is_empty() {
            let state = self
                .store
                .get_stream_state(rgbpp_store::state::CKB_STREAM)
                .await?;
            return Ok(TransitionResolution::CkbSeenAboveLag {
                cells,
                indexed_to: state.map(|s| s.last_block_number).unwrap_or_default(),
            });
        }

        Ok(TransitionResolution::AwaitingCkb {
            btc_confirmed: btc_tx.confirmation.is_some(),
            btc_height: btc_tx.confirmation.map(|c| c.height),
        })
    }

    /// Ask the rich indexer for live cells bound to specific outputs of a Bitcoin
    /// transaction.
    ///
    /// The binding lives in the *suffix* of the lock args (`out_index || txid`), so a
    /// prefix search cannot express "any output of this transaction" — hence one
    /// exact lookup per candidate output. A `partial` search on the txid bytes would
    /// collapse this to a single call where the indexer supports it.
    pub async fn point_lookup_cells(
        &self,
        txid: &BtcTxid,
        vout_count: u32,
    ) -> Result<Vec<CkbOutPoint>> {
        let count = if vout_count == 0 {
            DEFAULT_POINT_LOOKUP_VOUTS
        } else {
            vout_count
        };
        let lock = self.config.protocol.rgbpp_lock;

        let mut found = Vec::new();
        for vout in 0..count {
            let args = RgbppLockArgs {
                out_index: vout,
                txid: *txid,
            }
            .encode();
            let script = Script::new(lock.code_hash, lock.hash_type, args);
            let records = self
                .ckb
                .get_cells(&SearchKey::lock_exact(script), 16)
                .await?;
            for record in records {
                found.push(CkbOutPoint::new(
                    record.out_point.tx_hash,
                    record.out_point.index.0,
                ));
            }
        }
        Ok(found)
    }

    /// Drain the refresh queue.
    pub async fn run_queue_worker(self, mut shutdown: Shutdown) {
        let interval = std::time::Duration::from_secs(self.config.reconcile.queue_interval_secs);

        loop {
            match self.drain_queue_once().await {
                Ok(0) => {}
                Ok(n) => debug!(processed = n, "drained refresh queue"),
                Err(e) => warn!(error = %e, "refresh queue drain failed"),
            }

            if shutdown.sleep(interval).await {
                return;
            }
        }
    }

    pub async fn drain_queue_once(&self) -> Result<usize> {
        let dropped = self
            .store
            .drop_exhausted_refreshes(self.config.reconcile.max_attempts)
            .await?;
        for item in &dropped {
            warn!(
                txid = %hex::encode(&item.txid),
                vout = item.vout,
                attempts = item.attempts,
                "giving up on a refresh after repeated failures"
            );
        }

        let batch = self
            .store
            .claim_refresh_batch(self.config.reconcile.queue_batch_size)
            .await?;
        if batch.is_empty() {
            return Ok(0);
        }

        let claimed: Vec<BtcOutPoint> = batch
            .iter()
            .map(|item| item.out_point())
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Several paths queue the same outpoint — an address diff, a spend follow-up,
        // the sweeper. By the time an entry is claimed, another path may already have
        // answered it. An observation younger than the TTL is still authoritative, so
        // re-querying it would spend a request on the Bitcoin data source to learn
        // nothing.
        let ttl = self.config.btc.observation_ttl_secs as i64;
        let now = chrono::Utc::now();
        let txids: Vec<Vec<u8>> = claimed.iter().map(|o| o.txid.to_display_vec()).collect();
        let vouts: Vec<i32> = claimed.iter().map(|o| o.vout as i32).collect();

        let mut already_fresh: HashSet<BtcOutPoint> = HashSet::new();
        for row in self.store.get_observations(&txids, &vouts).await? {
            if row.is_fresh(ttl, now) {
                already_fresh.insert(row.out_point()?);
            }
        }

        let outpoints: Vec<BtcOutPoint> = claimed
            .iter()
            .copied()
            .filter(|o| !already_fresh.contains(o))
            .collect();

        // A fresh observation satisfies the queue entry: clear it rather than
        // leaving it to be reclaimed and skipped again on the next pass.
        for outpoint in &already_fresh {
            self.store
                .complete_refresh(&outpoint.txid.to_display_vec(), outpoint.vout as i32)
                .await?;
        }
        if !already_fresh.is_empty() {
            debug!(
                skipped = already_fresh.len(),
                remaining = outpoints.len(),
                ttl_secs = ttl,
                "dropped queue entries already answered within the observation ttl"
            );
        }
        if outpoints.is_empty() {
            return Ok(0);
        }

        let refreshed = self.refresh_outpoints(&outpoints).await?;
        let succeeded: HashSet<BtcOutPoint> = refreshed.iter().map(|r| r.outpoint).collect();

        for outpoint in &outpoints {
            if succeeded.contains(outpoint) {
                self.store
                    .complete_refresh(&outpoint.txid.to_display_vec(), outpoint.vout as i32)
                    .await?;
            } else {
                self.store
                    .fail_refresh(
                        &outpoint.txid.to_display_vec(),
                        outpoint.vout as i32,
                        "data source did not answer",
                    )
                    .await?;
            }
        }

        Ok(refreshed.len())
    }
}
