//! The HTTP API.
//!
//! Read endpoints answer from the indexed range. Address and transaction endpoints
//! reconcile against the Bitcoin data source first, because the indexed range stops
//! `REORG_LAG` blocks short of the tip by design and cannot see a CKB transaction that
//! has not been committed yet. `/status` reports that lag explicitly, so a client can
//! tell "not there" from "not there yet".

pub mod dto;
pub mod error;
pub mod handlers;

use std::net::SocketAddr;

use axum::routing::{get, post};
use axum::Router;
use rgbpp_indexer::Engine;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

impl AppState {
    pub fn new(engine: Engine) -> Self {
        AppState { engine }
    }

    /// Clamp a caller-supplied page size to the configured bounds.
    pub fn page_size(&self, requested: Option<i64>) -> i64 {
        let api = &self.engine.config.api;
        requested
            .unwrap_or(api.default_page_size)
            .clamp(1, api.max_page_size)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/status", get(handlers::status))
        .route(
            "/v1/rgbpp/cells/by-btc-utxo/{txid}/{vout}",
            get(handlers::cells_by_btc_utxo),
        )
        .route(
            "/v1/rgbpp/cells/by-btc-txid/{txid}",
            get(handlers::cells_by_btc_txid),
        )
        .route(
            "/v1/rgbpp/cells/by-ckb-out-point/{tx_hash}/{index}",
            get(handlers::cell_by_ckb_out_point),
        )
        .route(
            "/v1/rgbpp/assets/by-btc-address/{address}",
            get(handlers::assets_by_btc_address),
        )
        .route(
            "/v1/rgbpp/balance/by-btc-address/{address}",
            get(handlers::balance_by_btc_address),
        )
        .route(
            "/v1/rgbpp/transactions/{txid}",
            get(handlers::transaction_status),
        )
        .route(
            "/v1/rgbpp/activity/by-btc-address/{address}",
            get(handlers::activity_by_btc_address),
        )
        .route("/v1/rgbpp/transitions", get(handlers::recent_transitions))
        .route(
            "/v1/rgbpp/transitions/{tx_hash}",
            get(handlers::transition_by_ckb_tx),
        )
        .route("/v1/rgbpp/refresh", post(handlers::refresh_outpoints))
        .route("/v1/anomalies", get(handlers::anomalies))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Serve until `shutdown` fires.
pub async fn serve(
    state: AppState,
    bind: &str,
    mut shutdown: rgbpp_indexer::Shutdown,
) -> std::io::Result<()> {
    let addr: SocketAddr = bind.parse().map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{bind}: {e}"))
    })?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "api listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await
}
