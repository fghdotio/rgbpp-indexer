//! Trezor Blockbook implementation.
//!
//! Blockbook exposes a different shape than Esplora, and one gap matters: a
//! transaction's outputs carry a `spent` flag but **not** the spending transaction.
//! Recovering the spender means walking the owning address's transaction history and
//! finding the input that consumes the outpoint.
//!
//! That costs one extra request per address (not per outpoint), so
//! [`BlockbookSource::tx_outspends`] resolves a whole transaction's outputs at once
//! and is strongly preferred over per-outpoint lookups on this backend.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use rgbpp_types::bitcoin::{BtcBlockHash, BtcOutPoint, BtcTxid};
use rgbpp_types::state::OutpointSpendStatus;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::error::{BtcError, Result};
use crate::source::{BtcBlockRef, BtcDataSource, BtcTip, BtcTxInfo, BtcTxOutput, BtcUtxo};
use crate::throttle::Throttle;

/// How deep to walk an address history looking for a spender before giving up.
/// Blockbook pages default to 1000 transactions, so this is generous in practice.
const MAX_ADDRESS_PAGES: u32 = 5;

#[derive(Debug)]
pub struct BlockbookSource {
    http: reqwest::Client,
    base_url: String,
    throttle: Throttle,
}

impl BlockbookSource {
    pub fn new(base_url: &str, timeout: Duration, throttle: Throttle) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("rgbpp-indexer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(BlockbookSource {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            throttle,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn get_opt<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let url = self.url(path);
        let _permit = self.throttle.acquire().await;
        let response = self.http.get(&url).send().await?;
        let status = response.status();
        debug!(%url, %status, "blockbook request");

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.as_u16() == 429 {
            return Err(BtcError::RateLimited { url });
        }
        if !status.is_success() {
            return Err(BtcError::Status { status, url });
        }
        let text = response.text().await?;
        // Blockbook answers "not found" with 400 + an `error` body on some builds.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
                if error.to_lowercase().contains("not found") {
                    return Ok(None);
                }
                return Err(BtcError::decode("blockbook response", error.to_string()));
            }
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| BtcError::decode("blockbook response", format!("{path}: {e}")))
    }

    /// Find which transactions spend the given outputs of `txid`.
    ///
    /// Returns a map from vout to spend status. Outputs whose spender cannot be
    /// located stay absent from the map rather than being reported as unspent.
    async fn resolve_spenders(
        &self,
        txid: &BtcTxid,
        spent_vouts: &[(u32, Option<String>)],
    ) -> HashMap<u32, OutpointSpendStatus> {
        let mut resolved = HashMap::new();

        // One history walk per address, not per output.
        let mut addresses: Vec<String> = spent_vouts
            .iter()
            .filter_map(|(_, address)| address.clone())
            .collect();
        addresses.sort();
        addresses.dedup();

        for address in addresses {
            let mut page = 1;
            loop {
                let path = format!("api/v2/address/{address}?details=txs&page={page}");
                let response: Option<BlockbookAddress> = match self.get_opt(&path).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(%address, error = %e, "blockbook address history lookup failed");
                        break;
                    }
                };
                let Some(address_page) = response else { break };

                for tx in &address_page.transactions {
                    for vin in &tx.vin {
                        let Some(prev_txid) = vin.txid.as_deref() else {
                            continue;
                        };
                        if BtcTxid::from_hex(prev_txid).ok().as_ref() != Some(txid) {
                            continue;
                        }
                        let Ok(spender) = BtcTxid::from_hex(&tx.txid) else {
                            continue;
                        };
                        let status = match tx.block_height {
                            Some(height) if height > 0 => OutpointSpendStatus::SpentConfirmed {
                                spender,
                                height: height as u32,
                            },
                            _ => OutpointSpendStatus::SpentUnconfirmed { spender },
                        };
                        resolved.insert(vin.vout, status);
                    }
                }

                let total_pages = address_page.total_pages.unwrap_or(1).max(1);
                if page >= total_pages as u32 || page >= MAX_ADDRESS_PAGES {
                    if page >= MAX_ADDRESS_PAGES && (total_pages as u32) > MAX_ADDRESS_PAGES {
                        warn!(
                            %address,
                            total_pages,
                            "stopped at the address history page cap; some spenders may be unresolved"
                        );
                    }
                    break;
                }
                page += 1;
            }
        }
        resolved
    }
}

#[derive(Debug, Deserialize)]
struct BlockbookVin {
    txid: Option<String>,
    #[serde(default)]
    vout: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookVout {
    #[serde(default)]
    value: String,
    #[serde(default)]
    n: u32,
    #[serde(default)]
    hex: String,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    spent: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookTx {
    txid: String,
    #[serde(default)]
    vin: Vec<BlockbookVin>,
    #[serde(default)]
    vout: Vec<BlockbookVout>,
    block_hash: Option<String>,
    block_height: Option<i64>,
    block_time: Option<i64>,
    #[serde(default)]
    fees: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookAddress {
    #[serde(default)]
    total_pages: Option<i64>,
    #[serde(default)]
    transactions: Vec<BlockbookTx>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookUtxo {
    txid: String,
    #[serde(default)]
    vout: u32,
    #[serde(default)]
    value: String,
    #[serde(default)]
    height: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookStatus {
    blockbook: BlockbookStatusInner,
    backend: BlockbookBackend,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookStatusInner {
    best_height: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookBackend {
    best_block_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockbookBlockIndex {
    block_hash: String,
}

/// Blockbook reports satoshi amounts as decimal strings.
fn parse_amount(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

impl BlockbookTx {
    fn block_ref(&self) -> Option<BtcBlockRef> {
        let height = self.block_height.filter(|h| *h > 0)? as u32;
        Some(BtcBlockRef {
            height,
            hash: BtcBlockHash::from_hex(self.block_hash.as_deref()?).ok()?,
            time: self.block_time.unwrap_or(0),
        })
    }
}

#[async_trait]
impl BtcDataSource for BlockbookSource {
    fn name(&self) -> &'static str {
        "blockbook"
    }

    async fn tip(&self) -> Result<BtcTip> {
        let status: BlockbookStatus = self
            .get_opt("api/")
            .await?
            .ok_or_else(|| BtcError::decode("blockbook status", "endpoint returned 404"))?;
        Ok(BtcTip {
            height: status.blockbook.best_height.max(0) as u32,
            hash: BtcBlockHash::from_hex(&status.backend.best_block_hash)?,
        })
    }

    async fn block_hash_at(&self, height: u32) -> Result<Option<BtcBlockHash>> {
        let Some(index): Option<BlockbookBlockIndex> = self
            .get_opt(&format!("api/v2/block-index/{height}"))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(BtcBlockHash::from_hex(&index.block_hash)?))
    }

    async fn transaction(&self, txid: &BtcTxid) -> Result<Option<BtcTxInfo>> {
        let Some(tx): Option<BlockbookTx> = self.get_opt(&format!("api/v2/tx/{txid}")).await?
        else {
            return Ok(None);
        };
        let confirmation = tx.block_ref();
        let mut inputs = Vec::with_capacity(tx.vin.len());
        for vin in &tx.vin {
            // Coinbase inputs have no previous txid.
            let Some(prev) = vin.txid.as_deref() else {
                continue;
            };
            inputs.push(BtcOutPoint::new(BtcTxid::from_hex(prev)?, vin.vout));
        }
        let outputs = tx
            .vout
            .iter()
            .map(|vout| {
                Ok(BtcTxOutput {
                    value: parse_amount(&vout.value),
                    script_pubkey: hex::decode(&vout.hex)
                        .map_err(|e| BtcError::decode("vout hex", e.to_string()))?,
                    address: vout.addresses.first().cloned(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(BtcTxInfo {
            txid: BtcTxid::from_hex(&tx.txid)?,
            confirmation,
            inputs,
            outputs,
            fee: tx.fees.as_deref().map(parse_amount),
        }))
    }

    async fn outpoint_status(&self, outpoint: &BtcOutPoint) -> Result<OutpointSpendStatus> {
        let Some(tx): Option<BlockbookTx> = self
            .get_opt(&format!("api/v2/tx/{}", outpoint.txid))
            .await?
        else {
            return Ok(OutpointSpendStatus::Unknown);
        };
        let Some(vout) = tx.vout.iter().find(|v| v.n == outpoint.vout) else {
            return Ok(OutpointSpendStatus::Unknown);
        };
        if !vout.spent {
            return Ok(OutpointSpendStatus::Unspent);
        }
        let spent_vouts = vec![(outpoint.vout, vout.addresses.first().cloned())];
        let resolved = self.resolve_spenders(&outpoint.txid, &spent_vouts).await;
        // Known-spent but unresolvable spender stays `Unknown`: claiming `Unspent`
        // here would silently resurrect a cell that has already moved.
        Ok(resolved
            .get(&outpoint.vout)
            .copied()
            .unwrap_or(OutpointSpendStatus::Unknown))
    }

    async fn tx_outspends(&self, txid: &BtcTxid) -> Result<Option<Vec<OutpointSpendStatus>>> {
        let Some(tx): Option<BlockbookTx> = self.get_opt(&format!("api/v2/tx/{txid}")).await?
        else {
            return Ok(None);
        };
        let spent_vouts: Vec<(u32, Option<String>)> = tx
            .vout
            .iter()
            .filter(|v| v.spent)
            .map(|v| (v.n, v.addresses.first().cloned()))
            .collect();

        let resolved = if spent_vouts.is_empty() {
            HashMap::new()
        } else {
            self.resolve_spenders(txid, &spent_vouts).await
        };

        let max_n = tx.vout.iter().map(|v| v.n).max().unwrap_or(0);
        let mut statuses = vec![OutpointSpendStatus::Unknown; max_n as usize + 1];
        for vout in &tx.vout {
            statuses[vout.n as usize] = if vout.spent {
                resolved
                    .get(&vout.n)
                    .copied()
                    .unwrap_or(OutpointSpendStatus::Unknown)
            } else {
                OutpointSpendStatus::Unspent
            };
        }
        Ok(Some(statuses))
    }

    async fn address_utxos(&self, address: &str) -> Result<Vec<BtcUtxo>> {
        let utxos: Vec<BlockbookUtxo> = self
            .get_opt(&format!("api/v2/utxo/{address}"))
            .await?
            .unwrap_or_default();
        utxos
            .into_iter()
            .map(|u| {
                // Blockbook's UTXO listing carries a height but no block hash, so the
                // confirmation reference is left unset; callers that need the hash
                // fetch the transaction.
                let confirmation = None;
                let _ = u.height;
                Ok(BtcUtxo {
                    outpoint: BtcOutPoint::new(BtcTxid::from_hex(&u.txid)?, u.vout),
                    value: parse_amount(&u.value),
                    confirmation,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_parses_with_omitted_zero_fields() {
        // Blockbook omits `vout` on an input spending output 0.
        let json = r#"{
            "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
            "vin": [{"txid": "0000000000000000000000000000000000000000000000000000000000000001"}],
            "vout": [{"value": "12345", "n": 0, "hex": "6a20", "addresses": ["bc1qexample"], "spent": true}],
            "blockHeight": -1
        }"#;
        let tx: BlockbookTx = serde_json::from_str(json).unwrap();
        assert_eq!(tx.vin[0].vout, 0);
        assert_eq!(parse_amount(&tx.vout[0].value), 12345);
        assert!(tx.block_ref().is_none());
    }
}
