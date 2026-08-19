use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;
use tracing::error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),

    #[error("not found")]
    NotFound,

    #[error(transparent)]
    Indexer(#[from] rgbpp_indexer::IndexerError),

    #[error(transparent)]
    Store(#[from] rgbpp_store::StoreError),

    #[error(transparent)]
    Btc(#[from] rgbpp_btc::BtcError),

    #[error(transparent)]
    Ckb(#[from] rgbpp_ckb::CkbError),

    #[error(transparent)]
    Types(#[from] rgbpp_types::Error),
}

impl ApiError {
    pub fn bad_request(reason: impl Into<String>) -> Self {
        ApiError::BadRequest(reason.into())
    }

    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) | ApiError::Types(_) => StatusCode::BAD_REQUEST,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            // An upstream data source being unreachable is not the caller's fault, and
            // saying so lets clients retry instead of treating it as a bad request.
            ApiError::Btc(e) if e.is_transient() => StatusCode::BAD_GATEWAY,
            ApiError::Btc(_) | ApiError::Ckb(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            error!(error = %self, "request failed");
        }
        (
            status,
            Json(json!({
                "error": {
                    "kind": match status {
                        StatusCode::BAD_REQUEST => "bad_request",
                        StatusCode::NOT_FOUND => "not_found",
                        StatusCode::BAD_GATEWAY => "upstream_unavailable",
                        _ => "internal",
                    },
                    "message": self.to_string(),
                }
            })),
        )
            .into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
