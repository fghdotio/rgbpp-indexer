//! The Bitcoin half of the data layer.
//!
//! Everything the indexer needs from Bitcoin is expressed as [`BtcDataSource`], so
//! the backing service is a deployment decision rather than an architectural one.
//! Two implementations ship here:
//!
//! * [`esplora::EsploraSource`] — the Esplora REST API, as served by
//!   `mempool.space`, `blockstream/electrs` and self-hosted electrs.
//! * [`blockbook::BlockbookSource`] — Trezor Blockbook.
//!
//! Everything this trait returns is a *re-queryable observation*, never a
//! dependency-bearing fact. That is what lets the Bitcoin side treat a reorg as
//! cache invalidation instead of a rollback: any answer can simply be asked again.

pub mod blockbook;
pub mod error;
pub mod esplora;
pub mod source;
pub mod throttle;

pub use error::{BtcError, Result};
pub use source::{
    observe_many, BtcBlockRef, BtcDataSource, BtcTip, BtcTxInfo, BtcTxOutput, BtcUtxo,
    OutpointObservation,
};

use std::sync::Arc;

use rgbpp_types::config::{BtcConfig, BtcSourceKind};

/// Build the configured data source.
pub fn build_source(config: &BtcConfig) -> Result<Arc<dyn BtcDataSource>> {
    let throttle = throttle::Throttle::new(config.max_concurrency, config.min_request_interval_ms);
    Ok(match config.source {
        BtcSourceKind::Esplora => Arc::new(esplora::EsploraSource::new(
            &config.base_url,
            config.request_timeout(),
            throttle,
        )?),
        BtcSourceKind::Blockbook => Arc::new(blockbook::BlockbookSource::new(
            &config.base_url,
            config.request_timeout(),
            throttle,
        )?),
    })
}
