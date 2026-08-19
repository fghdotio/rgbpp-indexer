use thiserror::Error;

pub type Result<T, E = IndexerError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error(transparent)]
    Ckb(#[from] rgbpp_ckb::CkbError),

    #[error(transparent)]
    Btc(#[from] rgbpp_btc::BtcError),

    #[error(transparent)]
    Store(#[from] rgbpp_store::StoreError),

    #[error(transparent)]
    Types(#[from] rgbpp_types::Error),

    /// The chain moved under us in a way this version does not handle.
    #[error("chain reorganisation detected: stored block {number} has hash {stored}, node reports {actual}")]
    Reorg {
        number: u64,
        stored: String,
        actual: String,
    },

    #[error("inconsistent chain data: {0}")]
    Inconsistent(String),
}

impl IndexerError {
    pub fn inconsistent(reason: impl Into<String>) -> Self {
        IndexerError::Inconsistent(reason.into())
    }
}
