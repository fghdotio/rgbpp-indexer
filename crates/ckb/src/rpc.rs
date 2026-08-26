//! A small JSON-RPC 2.0 client with bounded retries.
//!
//! Transport hiccups against a public CKB node are common and self-healing; RPC
//! *application* errors are not, so only the former are retried.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::error::{CkbError, Result};

#[derive(Debug)]
pub struct JsonRpcClient {
    http: reqwest::Client,
    url: String,
    next_id: AtomicU64,
    max_retries: u32,
}

impl JsonRpcClient {
    pub fn new(url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("rgbpp-indexer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(JsonRpcClient {
            http,
            url: url.into(),
            next_id: AtomicU64::new(1),
            max_retries: 3,
        })
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let value = self
            .call_value(method, serde_json::to_value(params)?)
            .await?;
        serde_json::from_value(value).map_err(|e| CkbError::Payload {
            method: method.to_string(),
            reason: e.to_string(),
        })
    }

    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut attempt = 0;
        loop {
            match self.try_once(method, &body).await {
                Ok(value) => return Ok(value),
                // Application-level errors are deterministic: retrying just burns time.
                Err(e @ CkbError::Rpc { .. }) | Err(e @ CkbError::Payload { .. }) => return Err(e),
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(e);
                    }
                    let backoff = Duration::from_millis(200 << attempt);
                    warn!(method, attempt, error = %e, ?backoff, "ckb rpc retry");
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn try_once(&self, method: &str, body: &Value) -> Result<Value> {
        let response = self.http.post(&self.url).json(body).send().await?;
        let status = response.status();
        let payload: Value = response.error_for_status()?.json().await?;
        debug!(method, %status, "ckb rpc ok");

        if let Some(error) = payload.get("error") {
            if !error.is_null() {
                return Err(CkbError::Rpc {
                    method: method.to_string(),
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("<no message>")
                        .to_string(),
                });
            }
        }

        payload
            .get("result")
            .cloned()
            .ok_or_else(|| CkbError::Payload {
                method: method.to_string(),
                reason: "response had neither `result` nor `error`".to_string(),
            })
    }
}
