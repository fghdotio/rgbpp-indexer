//! The HTTP API.
//!
//! Read endpoints answer from the indexed range; address and transaction endpoints
//! reconcile against Bitcoin first, because that range stops `REORG_LAG` blocks short
//! of the tip by design. `/status` reports the lag so a client can tell "not there"
//! from "not there yet".
//!
//! The OpenAPI spec is served at `/openapi.json` and committed as
//! `docs/openapi.json`; `tests::committed_spec_is_current` keeps the two in step.

pub mod dto;
pub mod error;
pub mod handlers;

use std::net::SocketAddr;

use axum::http::header;
use axum::routing::get;
use axum::Router;
use rgbpp_indexer::Engine;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

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

#[derive(OpenApi)]
#[openapi(
    info(
        title = "RGB++ Indexer API",
        description = "RGB++ state indexed from CKB, reconciled against Bitcoin.\n\n\
            Amounts and capacities are decimal strings, since a u128 UDT amount does not \
            survive a JSON number. CKB hashes are `0x`-prefixed; Bitcoin txids are bare hex \
            in display order."
    ),
    tags(
        (name = "addresses", description = "RGB++ holdings and history of a Bitcoin address"),
        (name = "cells", description = "RGB++ cells by their Bitcoin or CKB location"),
        (name = "transactions", description = "RGB++ state transitions and cross-chain status"),
        (name = "assets", description = "Distinct assets in the index"),
        (name = "ops", description = "Health, index status, and operational controls"),
    )
)]
struct ApiDoc;

/// Every documented route, each registered from its `#[utoipa::path]` annotation.
///
/// That is the point of routing through `OpenApiRouter`: a path is written exactly
/// once, so an endpoint cannot be served without also appearing in the spec, and
/// the spec cannot describe a path that is not served.
fn documented_routes() -> OpenApiRouter<AppState> {
    let mut router = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(handlers::health))
        .routes(routes!(handlers::status))
        .routes(routes!(handlers::cells_by_btc_utxo))
        .routes(routes!(handlers::cells_by_btc_txid))
        .routes(routes!(handlers::cell_by_ckb_out_point))
        .routes(routes!(handlers::assets_by_btc_address))
        .routes(routes!(handlers::balance_by_btc_address))
        .routes(routes!(handlers::transaction_status))
        .routes(routes!(handlers::activity_by_btc_address))
        .routes(routes!(handlers::list_assets))
        .routes(routes!(handlers::recent_transitions))
        .routes(routes!(handlers::transition_by_ckb_tx))
        .routes(routes!(handlers::refresh_outpoints))
        .routes(routes!(handlers::anomalies));
    require_every_response_field(router.get_openapi_mut());
    router
}

/// Schemas used only as request bodies. `tests::request_bodies_are_listed` fails if
/// an operation takes a body that is missing here.
const REQUEST_BODIES: &[&str] = &["RefreshRequest"];

/// Mark every property of every response schema as required.
///
/// Responses always serialize every field — `None` goes out as `null`, never as a
/// missing key — but utoipa marks `Option` fields as not required, and there is no
/// switch to change that. Left alone, the spec would promise less than the API
/// delivers, and generated clients would make callers handle an `undefined` that
/// never arrives. Request bodies are exempt: there, leaving a field out means
/// something.
///
/// This relies on no response DTO skipping fields, which
/// `tests::response_fields_are_never_skipped` enforces.
fn require_every_response_field(api: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;

    fn require_all(schema: &mut Schema) {
        match schema {
            Schema::Object(object) => {
                object.required = object.properties.keys().cloned().collect();
            }
            // Internally tagged enums: each variant is an inline object.
            Schema::OneOf(one_of) => {
                for item in &mut one_of.items {
                    if let RefOr::T(variant) = item {
                        require_all(variant);
                    }
                }
            }
            _ => {}
        }
    }

    let Some(components) = api.components.as_mut() else {
        return;
    };
    for (name, schema) in components.schemas.iter_mut() {
        if REQUEST_BODIES.contains(&name.as_str()) {
            continue;
        }
        if let RefOr::T(schema) = schema {
            require_all(schema);
        }
    }
}

/// The OpenAPI document for this API.
pub fn openapi() -> utoipa::openapi::OpenApi {
    documented_routes().into_openapi()
}

pub fn router(state: AppState) -> Router {
    let (router, api) = documented_routes().split_for_parts();

    // Serialised once at startup: the spec is fixed for the life of the process.
    let spec = api
        .to_pretty_json()
        .expect("the OpenAPI document always serialises");

    router
        .route(
            "/openapi.json",
            get(move || {
                let spec = spec.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], spec) }
            }),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");

    /// The committed spec is what API consumers generate clients from, and what code
    /// review sees. This fails whenever the API changes without it.
    ///
    /// Regenerate with `UPDATE_OPENAPI=1 cargo test -p rgbpp-api committed_spec`.
    #[test]
    fn committed_spec_is_current() {
        let generated = openapi().to_pretty_json().unwrap() + "\n";

        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::write(COMMITTED, &generated).unwrap();
            return;
        }

        let committed = std::fs::read_to_string(COMMITTED).unwrap_or_default();
        assert!(
            committed == generated,
            "docs/openapi.json is out of date with the API. Regenerate it with:\n\n  \
             UPDATE_OPENAPI=1 cargo test -p rgbpp-api committed_spec\n"
        );
    }

    #[test]
    fn every_operation_is_documented() {
        let spec = serde_json::to_value(openapi()).unwrap();
        let paths = spec["paths"].as_object().unwrap();
        assert_eq!(paths.len(), 14);

        for (path, item) in paths {
            for (method, op) in item.as_object().unwrap() {
                let at = format!("{method} {path}");
                assert!(op["summary"].is_string(), "{at} has no summary");
                assert!(op["tags"][0].is_string(), "{at} has no tag");
                let ok = &op["responses"]["200"]["content"]["application/json"]["schema"];
                assert!(!ok.is_null(), "{at} has no 200 response schema");
                for status in ["400", "404", "500", "502"] {
                    if let Some(err) = op["responses"].get(status) {
                        assert_eq!(
                            err["content"]["application/json"]["schema"]["$ref"],
                            "#/components/schemas/ErrorResponse",
                            "{at} {status} does not use the error schema"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn response_fields_are_never_skipped() {
        // `require_every_response_field` marks every response property as required,
        // which is only true while no response field is ever left out.
        for (file, src) in [
            ("dto.rs", include_str!("dto.rs")),
            ("handlers.rs", include_str!("handlers.rs")),
        ] {
            assert!(
                !src.contains("skip_serializing"),
                "{file} skips a field when serializing; the spec marks every response \
                 field as required, so that field must be exempted or not skipped"
            );
        }
    }

    #[test]
    fn request_bodies_are_listed() {
        let spec = serde_json::to_value(openapi()).unwrap();
        for item in spec["paths"].as_object().unwrap().values() {
            for op in item.as_object().unwrap().values() {
                let schema = &op["requestBody"]["content"]["application/json"]["schema"]["$ref"];
                if let Some(reference) = schema.as_str() {
                    let name = reference.rsplit('/').next().unwrap();
                    assert!(
                        REQUEST_BODIES.contains(&name),
                        "{name} is not in REQUEST_BODIES"
                    );
                }
            }
        }
    }

    #[test]
    fn nullable_response_fields_are_required() {
        let spec = serde_json::to_value(openapi()).unwrap();
        let schemas = &spec["components"]["schemas"];
        let required = |schema: &str| -> Vec<String> {
            serde_json::from_value(schemas[schema]["required"].clone()).unwrap_or_default()
        };

        // Always present, sometimes null.
        assert!(required("CellDto").contains(&"amount".to_string()));
        assert!(required("CellDto").contains(&"consumed".to_string()));
        // Inside an internally tagged enum variant too.
        let awaiting = schemas["TransitionResolutionDto"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["properties"]["state"]["enum"][0] == "awaiting_ckb")
            .unwrap();
        assert!(awaiting["required"]
            .as_array()
            .unwrap()
            .contains(&"btc_height".into()));
        // A request body keeps its optional fields optional.
        assert!(!required("RefreshRequest").contains(&"synchronous".to_string()));
    }

    /// Query flags that default server-side must not be marked required, or generated
    /// clients will insist on sending them.
    #[test]
    fn defaulted_query_parameters_are_optional() {
        let spec = serde_json::to_value(openapi()).unwrap();
        let params =
            &spec["paths"]["/v1/rgbpp/assets/by-btc-address/{address}"]["get"]["parameters"];
        for param in params.as_array().unwrap() {
            let name = param["name"].as_str().unwrap();
            let required = param["required"].as_bool().unwrap_or(false);
            assert_eq!(required, name == "address", "{name}");
        }
    }
}
