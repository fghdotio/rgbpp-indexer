use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use thiserror::Error;
use tracing::error;

use crate::dto::{ErrorBody, ErrorKind, ErrorResponse};

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
        // Built from the documented type rather than ad-hoc JSON, so the error schema
        // in the OpenAPI spec is the serialization, not a description of it.
        let kind = match status {
            StatusCode::BAD_REQUEST => ErrorKind::BadRequest,
            StatusCode::NOT_FOUND => ErrorKind::NotFound,
            StatusCode::BAD_GATEWAY => ErrorKind::UpstreamUnavailable,
            _ => ErrorKind::Internal,
        };
        let body = ErrorResponse {
            error: ErrorBody {
                kind,
                message: self.to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
