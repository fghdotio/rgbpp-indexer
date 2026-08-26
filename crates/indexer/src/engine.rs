//! Wiring.
//!
//! One place that builds every component from configuration and hands out the pieces
//! the API needs, so nothing has to reconstruct a client or a pool of its own.

use std::sync::Arc;

use rgbpp_btc::BtcDataSource;
use rgbpp_ckb::CkbClient;
use rgbpp_store::Store;
use rgbpp_types::config::Config;
use tokio::task::JoinHandle;
use tracing::info;

use crate::address_backfill::AddressBackfill;
use crate::error::Result;
use crate::heartbeat::Heartbeat;
use crate::reconcile::Reconciler;
use crate::scanner::CkbScanner;
use crate::shutdown::{Shutdown, ShutdownController};
use crate::sweeper::Sweeper;
use crate::verify::CommitmentVerifier;

#[derive(Clone)]
pub struct Engine {
    pub config: Arc<Config>,
    pub store: Store,
    pub ckb: Arc<CkbClient>,
    pub btc: Arc<dyn BtcDataSource>,
    pub reconciler: Reconciler,
}

impl Engine {
    /// Connect everything and run migrations. Fails fast rather than starting
    /// workers that would each rediscover the same misconfiguration.
    pub async fn bootstrap(config: Config) -> Result<Self> {
        let config = Arc::new(config);

        let store = Store::connect(&config.database).await?;
        if config.database.auto_migrate {
            store.migrate().await?;
        }
        store.ping().await?;

        let ckb = Arc::new(CkbClient::new(
            &config.ckb.rpc_url,
            config.ckb.indexer_url(),
            config.ckb.request_timeout(),
            config.ckb.page_limit,
        )?);
        let btc = rgbpp_btc::build_source(&config.btc)?;

        // Everything needed to answer "why is this indexing nothing", on one line.
        info!(
            network = %config.general.network,
            ckb = %config.ckb.rpc_url,
            ckb_indexer = %config.ckb.indexer_url(),
            btc = %format_args!("{}:{}", btc.name(), config.btc.base_url),
            start_block = config.ckb.start_block,
            reorg_lag = config.ckb.reorg_lag,
            batch_blocks = config.ckb.batch_blocks,
            rgbpp_lock = %config.protocol.rgbpp_lock.code_hash,
            btc_time_lock = %config.protocol.btc_time_lock.code_hash,
            assets = %format_args!(
                "xudt:{} sudt:{} spore:{}",
                config.assets.xudt.len(),
                config.assets.sudt.len(),
                config.assets.spore.len()
            ),
            reconcile = config.reconcile.enabled,
            sweep = config.sweep.enabled,
            verify_commitments = config.verify.commitments,
            "engine ready"
        );

        let reconciler = Reconciler::new(store.clone(), btc.clone(), ckb.clone(), config.clone());

        Ok(Engine {
            config,
            store,
            ckb,
            btc,
            reconciler,
        })
    }

    /// Start the background workers. Each is independent: one failing loop does not
    /// take the others down, and the API keeps serving whatever is already indexed.
    pub fn spawn_workers(&self, shutdown: &ShutdownController) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        // Named rather than counted: "which workers are running" is the question,
        // and it is answered by configuration that is easy to get wrong.
        let mut enabled: Vec<&str> = vec!["scanner"];

        let scanner = CkbScanner::new(self.store.clone(), self.ckb.clone(), self.config.clone());
        handles.push(tokio::spawn(scanner.run(shutdown.subscribe())));

        if self.config.reconcile.enabled {
            enabled.push("refresh-queue");
            handles.push(tokio::spawn(
                self.reconciler
                    .clone()
                    .run_queue_worker(shutdown.subscribe()),
            ));
        }

        if self.config.reconcile.enabled && self.config.reconcile.address_backfill {
            enabled.push("address-backfill");
            handles.push(tokio::spawn(
                AddressBackfill::new(self.store.clone(), self.btc.clone(), self.config.clone())
                    .run(shutdown.subscribe()),
            ));
        }

        if self.config.log.heartbeat_interval().is_some() {
            enabled.push("heartbeat");
            handles.push(tokio::spawn(
                Heartbeat::new(self.store.clone(), self.config.clone()).run(shutdown.subscribe()),
            ));
        }

        if self.config.sweep.enabled {
            enabled.push("sweeper");
            handles.push(tokio::spawn(
                Sweeper::new(
                    self.store.clone(),
                    self.reconciler.clone(),
                    self.config.clone(),
                )
                .run(shutdown.subscribe()),
            ));
        }

        if self.config.verify.commitments {
            enabled.push("verifier");
            handles.push(tokio::spawn(
                CommitmentVerifier::new(
                    self.store.clone(),
                    self.reconciler.clone(),
                    self.config.clone(),
                )
                .run(shutdown.subscribe()),
            ));
        }

        info!(
            count = handles.len(),
            enabled = %enabled.join(","),
            "workers started"
        );
        handles
    }

    /// Run a single pass of every worker and return. Used by the one-shot CLI
    /// commands, which is also what makes each loop testable in isolation.
    pub async fn run_once(&self) -> Result<()> {
        let round = CkbScanner::new(self.store.clone(), self.ckb.clone(), self.config.clone())
            .scan_once()
            .await?;
        info!(?round, "scan round");
        Ok(())
    }

    pub fn sweeper(&self) -> Sweeper {
        Sweeper::new(
            self.store.clone(),
            self.reconciler.clone(),
            self.config.clone(),
        )
    }

    pub fn scanner(&self) -> CkbScanner {
        CkbScanner::new(self.store.clone(), self.ckb.clone(), self.config.clone())
    }

    pub fn address_backfill(&self) -> AddressBackfill {
        AddressBackfill::new(self.store.clone(), self.btc.clone(), self.config.clone())
    }

    pub fn verifier(&self) -> CommitmentVerifier {
        CommitmentVerifier::new(
            self.store.clone(),
            self.reconciler.clone(),
            self.config.clone(),
        )
    }
}

/// Wait for every worker to finish after shutdown is signalled.
pub async fn join_all(handles: Vec<JoinHandle<()>>, mut shutdown: Shutdown) {
    shutdown.wait().await;
    for handle in handles {
        let _ = handle.await;
    }
}
