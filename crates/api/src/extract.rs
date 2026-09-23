//! Request extractors whose rejections use the API's error body.
//!
//! axum's own `Json`, `Query` and `Path` reject with a plain-text body, and `Json`
//! with 415 or 422 as well, while the spec promises every 400 is an `ErrorResponse`.
//! These wrap them and turn any rejection into `ApiError::BadRequest`, keeping
//! axum's message. `Json` is also the response type, so handlers use one name for
//! both directions.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Json(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

impl<T> IntoResponse for Json<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

pub struct Query<T>(pub T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    axum::extract::Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Query(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    axum::extract::Path<T>: FromRequestParts<S, Rejection = PathRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Path(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::Router;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Keys {
        #[allow(dead_code)]
        keys: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Page {
        #[allow(dead_code)]
        limit: Option<i64>,
    }

    fn app() -> Router {
        Router::new()
            .route("/body", post(|_: Json<Keys>| async {}))
            .route("/query", get(|_: Query<Page>| async {}))
            .route("/path/{vout}", get(|_: Path<u32>| async {}))
    }

    async fn send(request: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }

    fn post_json(body: &str, content_type: Option<&str>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::post("/body");
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn every_rejection_is_a_400_error_response() {
        let get = |uri: &str| axum::http::Request::get(uri).body(Body::empty()).unwrap();
        let cases = [
            ("missing field", post_json("{}", Some("application/json"))),
            (
                "wrong type",
                post_json(r#"{"keys": 1}"#, Some("application/json")),
            ),
            ("not JSON", post_json("{", Some("application/json"))),
            ("no content type", post_json(r#"{"keys": []}"#, None)),
            ("bad query", get("/query?limit=abc")),
            ("bad path", get("/path/abc")),
        ];
        for (case, request) in cases {
            let (status, body) = send(request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{case}");
            assert_eq!(body["error"]["kind"], "bad_request", "{case}");
            assert!(
                !body["error"]["message"].as_str().unwrap().is_empty(),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn the_message_names_the_problem() {
        let (_, body) = send(post_json("{}", Some("application/json"))).await;
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("missing field `keys`"), "{message}");
    }
}
