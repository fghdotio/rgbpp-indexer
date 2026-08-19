use thiserror::Error;

pub type Result<T, E = BtcError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum BtcError {
    #[error("bitcoin data source transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("bitcoin data source returned {status} for {url}")]
    Status {
        status: reqwest::StatusCode,
        url: String,
    },

    #[error("bitcoin data source rate limited us on {url}")]
    RateLimited { url: String },

    #[error("cannot decode {what} from bitcoin data source: {reason}")]
    Decode { what: &'static str, reason: String },

    #[error("`{feature}` is not supported by the {source_name} data source")]
    Unsupported {
        feature: &'static str,
        source_name: &'static str,
    },

    #[error("type error: {0}")]
    Types(#[from] rgbpp_types::Error),
}

impl BtcError {
    pub fn decode(what: &'static str, reason: impl Into<String>) -> Self {
        BtcError::Decode {
            what,
            reason: reason.into(),
        }
    }

    /// Whether retrying later could plausibly succeed. Used to decide between
    /// re-queueing a refresh and giving up on it.
    pub fn is_transient(&self) -> bool {
        match self {
            BtcError::Transport(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            BtcError::RateLimited { .. } => true,
            BtcError::Status { status, .. } => status.is_server_error() || status.as_u16() == 429,
            _ => false,
        }
    }
}
