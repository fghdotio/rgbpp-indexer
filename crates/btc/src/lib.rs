//! The Bitcoin half of the data layer.
//!
//! Everything the indexer needs from Bitcoin is [`BtcDataSource`], so the backing
//! service is a deployment decision. Esplora and Blockbook implementations ship here.

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
