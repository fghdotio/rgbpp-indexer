use thiserror::Error;

pub type Result<T, E = CkbError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum CkbError {
    #[error("ckb rpc transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("ckb rpc `{method}` returned error {code}: {message}")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },

    #[error("ckb rpc `{method}` returned an unexpected payload: {reason}")]
    Payload { method: String, reason: String },

    #[error("decode error: {0}")]
    Decode(#[from] rgbpp_types::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
