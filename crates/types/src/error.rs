use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid hex: {0}")]
    Hex(#[from] hex::FromHexError),

    #[error("invalid length for {what}: expected {expected}, got {got}")]
    Length {
        what: &'static str,
        expected: usize,
        got: usize,
    },

    #[error("malformed {what}: {reason}")]
    Malformed { what: &'static str, reason: String },

    #[error("unsupported {what}: {value}")]
    Unsupported { what: &'static str, value: String },

    #[error("config error: {0}")]
    Config(String),
}

impl Error {
    pub fn malformed(what: &'static str, reason: impl Into<String>) -> Self {
        Error::Malformed {
            what,
            reason: reason.into(),
        }
    }
}
