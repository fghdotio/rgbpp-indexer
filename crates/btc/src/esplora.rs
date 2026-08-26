//! Esplora REST implementation, compatible with `mempool.space`, `blockstream/electrs`
//! and self-hosted electrs.
//!
//! Answers "is this outpoint spent, and by whom" in one call, which is what the whole
//! pending-state model is built on.

use std::time::Duration;

use async_trait::async_trait;
use rgbpp_types::bitcoin::{BtcBlockHash, BtcOutPoint, BtcTxid};
use rgbpp_types::state::OutpointSpendStatus;
use serde::Deserialize;
use tracing::debug;

use crate::error::{BtcError, Result};
use crate::source::{BtcBlockRef, BtcDataSource, BtcTip, BtcTxInfo, BtcTxOutput, BtcUtxo};
use crate::throttle::Throttle;

#[derive(Debug)]
pub struct EsploraSource {
    http: reqwest::Client,
    base_url: String,
    throttle: Throttle,
}

impl EsploraSource {
    pub fn new(base_url: &str, timeout: Duration, throttle: Throttle) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("rgbpp-indexer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(EsploraSource {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            throttle,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// `None` on 404 — "the data source has no such object" is an answer, not an error.
    async fn get_opt<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        match self.get_text_opt(path).await? {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| BtcError::decode("esplora response", format!("{path}: {e}"))),
        }
    }

    async fn get_text_opt(&self, path: &str) -> Result<Option<String>> {
        let url = self.url(path);
        let _permit = self.throttle.acquire().await;
        let response = self.http.get(&url).send().await?;
        let status = response.status();
        debug!(%url, %status, "esplora request");

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.as_u16() == 429 {
            return Err(BtcError::RateLimited { url });
        }
        if !status.is_success() {
            return Err(BtcError::Status { status, url });
        }
        Ok(Some(response.text().await?))
    }
}

#[derive(Debug, Deserialize)]
struct EsploraStatus {
    confirmed: bool,
    block_height: Option<u32>,
    block_hash: Option<String>,
    block_time: Option<i64>,
}

impl EsploraStatus {
    fn to_block_ref(&self) -> Option<BtcBlockRef> {
        if !self.confirmed {
            return None;
        }
        Some(BtcBlockRef {
            height: self.block_height?,
            hash: BtcBlockHash::from_hex(self.block_hash.as_deref()?).ok()?,
            time: self.block_time.unwrap_or(0),
        })
    }
}

#[derive(Debug, Deserialize)]
struct EsploraVin {
    txid: String,
    vout: u32,
    #[serde(default)]
    is_coinbase: bool,
}

#[derive(Debug, Deserialize)]
struct EsploraVout {
    #[serde(default)]
    scriptpubkey: String,
    scriptpubkey_address: Option<String>,
    #[serde(default)]
    value: u64,
}

#[derive(Debug, Deserialize)]
struct EsploraTx {
    txid: String,
    #[serde(default)]
    fee: Option<u64>,
    vin: Vec<EsploraVin>,
    vout: Vec<EsploraVout>,
    status: EsploraStatus,
}

#[derive(Debug, Deserialize)]
struct EsploraOutspend {
    spent: bool,
    txid: Option<String>,
    #[allow(dead_code)]
    vin: Option<u32>,
    status: Option<EsploraStatus>,
}

impl EsploraOutspend {
    fn to_status(&self) -> Result<OutpointSpendStatus> {
        if !self.spent {
            return Ok(OutpointSpendStatus::Unspent);
        }
        let spender = self
            .txid
            .as_deref()
            .ok_or_else(|| BtcError::decode("outspend", "spent=true without a spender txid"))?;
        let spender = BtcTxid::from_hex(spender)?;
        match self.status.as_ref().and_then(|s| s.to_block_ref()) {
            Some(block) => Ok(OutpointSpendStatus::SpentConfirmed {
                spender,
                height: block.height,
            }),
            None => Ok(OutpointSpendStatus::SpentUnconfirmed { spender }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct EsploraUtxo {
    txid: String,
    vout: u32,
    value: u64,
    status: EsploraStatus,
}

#[async_trait]
impl BtcDataSource for EsploraSource {
    fn name(&self) -> &'static str {
        "esplora"
    }

    async fn tip(&self) -> Result<BtcTip> {
        let height = self
            .get_text_opt("blocks/tip/height")
            .await?
            .ok_or_else(|| BtcError::decode("tip height", "endpoint returned 404"))?;
        let height: u32 = height
            .trim()
            .parse()
            .map_err(|e| BtcError::decode("tip height", format!("{height:?}: {e}")))?;
        let hash = self
            .get_text_opt("blocks/tip/hash")
            .await?
            .ok_or_else(|| BtcError::decode("tip hash", "endpoint returned 404"))?;
        Ok(BtcTip {
            height,
            hash: BtcBlockHash::from_hex(hash.trim())?,
        })
    }

    async fn block_hash_at(&self, height: u32) -> Result<Option<BtcBlockHash>> {
        let Some(text) = self.get_text_opt(&format!("block-height/{height}")).await? else {
            return Ok(None);
        };
        Ok(Some(BtcBlockHash::from_hex(text.trim())?))
    }

    async fn transaction(&self, txid: &BtcTxid) -> Result<Option<BtcTxInfo>> {
        let Some(tx): Option<EsploraTx> = self.get_opt(&format!("tx/{txid}")).await? else {
            return Ok(None);
        };
        let confirmation = tx.status.to_block_ref();
        let mut inputs = Vec::with_capacity(tx.vin.len());
        for vin in &tx.vin {
            if vin.is_coinbase {
                continue;
            }
            inputs.push(BtcOutPoint::new(BtcTxid::from_hex(&vin.txid)?, vin.vout));
        }
        let outputs = tx
            .vout
            .iter()
            .map(|vout| {
                Ok(BtcTxOutput {
                    value: vout.value,
                    script_pubkey: hex::decode(&vout.scriptpubkey)
                        .map_err(|e| BtcError::decode("scriptpubkey", e.to_string()))?,
                    address: vout.scriptpubkey_address.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(BtcTxInfo {
            txid: BtcTxid::from_hex(&tx.txid)?,
            confirmation,
            inputs,
            outputs,
            fee: tx.fee,
        }))
    }

    async fn outpoint_status(&self, outpoint: &BtcOutPoint) -> Result<OutpointSpendStatus> {
        let path = format!("tx/{}/outspend/{}", outpoint.txid, outpoint.vout);
        match self.get_opt::<EsploraOutspend>(&path).await? {
            // A 404 means the data source cannot see the funding transaction at all,
            // which is genuinely "unknown" rather than "unspent".
            None => Ok(OutpointSpendStatus::Unknown),
            Some(outspend) => outspend.to_status(),
        }
    }

    async fn tx_outspends(&self, txid: &BtcTxid) -> Result<Option<Vec<OutpointSpendStatus>>> {
        let Some(outspends): Option<Vec<EsploraOutspend>> =
            self.get_opt(&format!("tx/{txid}/outspends")).await?
        else {
            return Ok(None);
        };
        outspends
            .iter()
            .map(EsploraOutspend::to_status)
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }

    async fn address_utxos(&self, address: &str) -> Result<Vec<BtcUtxo>> {
        let utxos: Vec<EsploraUtxo> = self
            .get_opt(&format!("address/{address}/utxo"))
            .await?
            .unwrap_or_default();
        utxos
            .into_iter()
            .map(|u| {
                Ok(BtcUtxo {
                    outpoint: BtcOutPoint::new(BtcTxid::from_hex(&u.txid)?, u.vout),
                    value: u.value,
                    confirmation: u.status.to_block_ref(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outspend_states() {
        let unspent: EsploraOutspend = serde_json::from_str(r#"{"spent": false}"#).unwrap();
        assert_eq!(unspent.to_status().unwrap(), OutpointSpendStatus::Unspent);

        let mempool: EsploraOutspend = serde_json::from_str(
            r#"{"spent": true,
                "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
                "vin": 0,
                "status": {"confirmed": false}}"#,
        )
        .unwrap();
        assert!(matches!(
            mempool.to_status().unwrap(),
            OutpointSpendStatus::SpentUnconfirmed { .. }
        ));

        let confirmed: EsploraOutspend = serde_json::from_str(
            r#"{"spent": true,
                "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
                "vin": 0,
                "status": {"confirmed": true, "block_height": 800000,
                           "block_hash": "00000000000000000002a7c4c1e48d76c5a37902165a270156b7a8d72728a054",
                           "block_time": 1690000000}}"#,
        )
        .unwrap();
        assert_eq!(
            confirmed.to_status().unwrap(),
            OutpointSpendStatus::SpentConfirmed {
                spender: BtcTxid::from_hex(
                    "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b"
                )
                .unwrap(),
                height: 800_000,
            }
        );
    }

    #[test]
    fn base_url_trailing_slash() {
        let source = EsploraSource::new(
            "https://mempool.space/api/",
            Duration::from_secs(1),
            Throttle::unlimited(),
        )
        .unwrap();
        assert_eq!(source.url("/tx/abc"), "https://mempool.space/api/tx/abc");
        assert_eq!(source.url("tx/abc"), "https://mempool.space/api/tx/abc");
    }
}
