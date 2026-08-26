//! The periodic operational one-liner. Exists because the sync line goes quiet once
//! caught up, and silence is indistinguishable from a wedged process.

use std::sync::Arc;

use rgbpp_store::state::CKB_STREAM;
use rgbpp_store::Store;
use rgbpp_types::config::Config;
use tracing::{info, warn};

use crate::shutdown::Shutdown;

pub struct Heartbeat {
    store: Store,
    config: Arc<Config>,
}

impl Heartbeat {
    pub fn new(store: Store, config: Arc<Config>) -> Self {
        Heartbeat { store, config }
    }

    pub async fn run(self, mut shutdown: Shutdown) {
        let Some(interval) = self.config.log.heartbeat_interval() else {
            return;
        };
        loop {
            if shutdown.sleep(interval).await {
                return;
            }
            if let Err(e) = self.emit().await {
                warn!(error = %e, "heartbeat failed");
            }
        }
    }

    pub async fn emit(&self) -> crate::Result<()> {
        let state = self.store.get_stream_state(CKB_STREAM).await?;
        let counts = self.store.counts().await?;
        let unlabelled = self.store.bindings_missing_address().await?;

        let indexed = state.as_ref().map(|s| s.last_block_number).unwrap_or(0);
        let tip = state.as_ref().and_then(|s| s.chain_tip_number);
        let target = state.as_ref().and_then(|s| s.target_block_number);
        let error = state.as_ref().and_then(|s| s.last_error.clone());

        info!(
            indexed = indexed,
            target = target.unwrap_or(0),
            tip = tip.unwrap_or(0),
            behind = tip.map(|t| t - indexed).unwrap_or(0),
            lag = self.config.ckb.reorg_lag,
            cells = counts.total_cells,
            live = counts.live_cells,
            pending = counts.pending_ckb_cells,
            transitions = counts.transitions,
            queue = counts.refresh_queue_depth,
            anomalies = counts.open_anomalies,
            // Non-zero means address history is still incomplete, which otherwise
            // shows up only as a thin activity list.
            unlabelled = unlabelled,
            "status"
        );

        // A stuck stream is the one thing that must not be quiet.
        if let Some(error) = error {
            warn!(error, indexed, "ckb stream is holding an error");
        }
        Ok(())
    }
}
