//! Resolving which Bitcoin address owns each RGB++ binding, from the transaction that
//! funded it.
//!
//! Ownership taken from an address's live UTXO listing would silently exclude every
//! binding already spent, which is most of what a history is made of. See
//! `docs/indexing.md`.

use std::sync::Arc;

use rgbpp_btc::BtcDataSource;
use rgbpp_store::Store;
use rgbpp_types::bitcoin::BtcTxid;
use rgbpp_types::config::Config;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::shutdown::Shutdown;

pub struct AddressBackfill {
    store: Store,
    btc: Arc<dyn BtcDataSource>,
    config: Arc<Config>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct BackfillReport {
    /// Funding transactions examined this pass.
    pub transactions: usize,
    /// Bindings that gained an owning address.
    pub bindings_labelled: u64,
    /// Transactions the data source could not return.
    pub unresolved: usize,
}

impl AddressBackfill {
    pub fn new(store: Store, btc: Arc<dyn BtcDataSource>, config: Arc<Config>) -> Self {
        AddressBackfill { store, btc, config }
    }

    pub async fn run(self, mut shutdown: Shutdown) {
        let interval =
            std::time::Duration::from_secs(self.config.reconcile.address_backfill_interval_secs);

        // Walks funding transactions in txid order. Held in memory rather than
        // persisted because resolved bindings drop out of the query on their own:
        // restarting re-walks only what is still missing, which is exactly the set
        // worth retrying.
        let mut cursor: Option<Vec<u8>> = None;

        loop {
            match self.backfill_once(cursor.clone()).await {
                Ok((report, next)) => {
                    if report.bindings_labelled > 0 || report.unresolved > 0 {
                        info!(
                            txs = report.transactions,
                            labelled = report.bindings_labelled,
                            unresolved = report.unresolved,
                            "binding addresses"
                        );
                    }
                    // A short pass means the end of the set; start over so anything
                    // that could not be resolved gets another chance later.
                    cursor = if report.transactions
                        < self.config.reconcile.address_backfill_batch as usize
                    {
                        None
                    } else {
                        next
                    };
                }
                Err(e) => warn!(error = %e, "binding address backfill failed"),
            }

            if shutdown.sleep(interval).await {
                return;
            }
        }
    }

    /// One pass. Returns the report and the cursor to resume from.
    pub async fn backfill_once(
        &self,
        after: Option<Vec<u8>>,
    ) -> Result<(BackfillReport, Option<Vec<u8>>)> {
        let txids = self
            .store
            .funding_txids_missing_address(
                after.as_deref(),
                self.config.reconcile.address_backfill_batch,
            )
            .await?;

        let mut report = BackfillReport {
            transactions: txids.len(),
            ..Default::default()
        };
        let next = txids.last().cloned();

        for raw in &txids {
            let txid = BtcTxid::from_display_slice(raw)?;
            match self.resolve(&txid).await {
                Ok(0) => {
                    // The transaction resolved but named no address for any bound
                    // output — a bare script the data source cannot render as an
                    // address. Nothing more to try.
                    debug!(%txid, "funding transaction has no addressable bound output");
                }
                Ok(labelled) => report.bindings_labelled += labelled,
                Err(e) => {
                    report.unresolved += 1;
                    debug!(%txid, error = %e, "cannot resolve funding transaction");
                }
            }
        }

        Ok((report, next))
    }

    /// Fetch one funding transaction and label every bound output it created.
    async fn resolve(&self, txid: &BtcTxid) -> Result<u64> {
        let Some(tx) = self.btc.transaction(txid).await? else {
            return Ok(0);
        };

        // A binding's funding transaction is also the Bitcoin side of the transition
        // that created it, so caching it here means activity listings get their
        // confirmation and fee details without a second fetch.
        self.store
            .upsert_btc_tx(
                &txid.to_display_vec(),
                tx.confirmation.map(|c| c.height as i32),
                tx.confirmation.map(|c| c.hash.to_vec()).as_deref(),
                tx.confirmation
                    .and_then(|c| chrono::DateTime::from_timestamp(c.time, 0)),
                tx.commitment().as_ref().map(|c| &c[..]),
                tx.inputs.len() as i32,
                tx.outputs.len() as i32,
                tx.fee.map(|f| f as i64),
                self.btc.name(),
            )
            .await?;

        Ok(record_addresses(&self.store, txid, &tx).await?)
    }
}

/// Write the owning address of every bound output of a fetched transaction.
///
/// Shared with the reconciler so that transactions fetched for other reasons label
/// their bindings at no extra cost.
pub async fn record_addresses(
    store: &Store,
    txid: &BtcTxid,
    tx: &rgbpp_btc::BtcTxInfo,
) -> rgbpp_store::Result<u64> {
    let outputs: Vec<(i32, String)> = tx
        .outputs
        .iter()
        .enumerate()
        .filter_map(|(vout, output)| output.address.as_ref().map(|a| (vout as i32, a.clone())))
        .collect();

    if outputs.is_empty() {
        return Ok(0);
    }
    store
        .record_binding_addresses(&txid.to_display_vec(), &outputs)
        .await
}
