//! The data-source interface.

use std::fmt::Debug;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use rgbpp_types::bitcoin::{BtcBlockHash, BtcOutPoint, BtcTxid};
use rgbpp_types::commitment;
use rgbpp_types::state::OutpointSpendStatus;
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcTip {
    pub height: u32,
    pub hash: BtcBlockHash,
}

/// Where a transaction or UTXO sits in the chain. `None` means "in the mempool".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcBlockRef {
    pub height: u32,
    pub hash: BtcBlockHash,
    /// Block time, seconds since epoch.
    pub time: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcTxOutput {
    pub value: u64,
    #[serde(with = "hex_bytes")]
    pub script_pubkey: Vec<u8>,
    pub address: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcTxInfo {
    pub txid: BtcTxid,
    /// `None` while the transaction is unconfirmed.
    pub confirmation: Option<BtcBlockRef>,
    /// Outpoints this transaction spends. These are what tie it back to RGB++
    /// cells: an RGB++ transfer must spend the UTXOs its cells are bound to.
    pub inputs: Vec<BtcOutPoint>,
    pub outputs: Vec<BtcTxOutput>,
    /// Fee in satoshis. `None` when the data source does not report one.
    pub fee: Option<u64>,
}

impl BtcTxInfo {
    /// The RGB++ commitment published in this transaction, if any.
    pub fn commitment(&self) -> Option<[u8; 32]> {
        commitment::find_commitment(self.outputs.iter().map(|o| o.script_pubkey.as_slice()))
    }

    pub fn spends(&self, outpoint: &BtcOutPoint) -> bool {
        self.inputs.contains(outpoint)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcUtxo {
    pub outpoint: BtcOutPoint,
    pub value: u64,
    pub confirmation: Option<BtcBlockRef>,
}

/// A point-in-time answer about one outpoint.
///
/// This is a cache entry, not a fact: `observed_at` is what makes staleness — and
/// therefore reorg recovery — expressible without any rollback machinery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutpointObservation {
    pub outpoint: BtcOutPoint,
    pub status: OutpointSpendStatus,
    pub observed_at: DateTime<Utc>,
    pub source: String,
}

impl OutpointObservation {
    pub fn new(outpoint: BtcOutPoint, status: OutpointSpendStatus, source: &str) -> Self {
        OutpointObservation {
            outpoint,
            status,
            observed_at: Utc::now(),
            source: source.to_string(),
        }
    }
}

#[async_trait]
pub trait BtcDataSource: Send + Sync + Debug {
    /// Stable identifier recorded alongside every observation, so a mixed or
    /// migrated deployment can tell which backend produced a given answer.
    fn name(&self) -> &'static str;

    async fn tip(&self) -> Result<BtcTip>;

    /// Canonical block hash at a height — the input to reorg detection.
    async fn block_hash_at(&self, height: u32) -> Result<Option<BtcBlockHash>>;

    /// `None` when the data source has never heard of the transaction.
    async fn transaction(&self, txid: &BtcTxid) -> Result<Option<BtcTxInfo>>;

    async fn outpoint_status(&self, outpoint: &BtcOutPoint) -> Result<OutpointSpendStatus>;

    /// Spend status of *every* output of one transaction, when the backend can
    /// answer that in a single call.
    ///
    /// RGB++ transactions routinely bind several cells to vouts of the same Bitcoin
    /// transaction, so this collapses a fan-out into one request. Returning
    /// `Ok(None)` means "not supported", and callers fall back to per-outpoint queries.
    async fn tx_outspends(&self, _txid: &BtcTxid) -> Result<Option<Vec<OutpointSpendStatus>>> {
        Ok(None)
    }

    /// Current UTXO set of an address, straight from the data source.
    ///
    /// This is the reference the on-demand reconciliation path diffs against.
    async fn address_utxos(&self, address: &str) -> Result<Vec<BtcUtxo>>;
}

/// Observe many outpoints, grouping by transaction where the backend supports it.
pub async fn observe_many(
    source: &dyn BtcDataSource,
    outpoints: &[BtcOutPoint],
    concurrency: usize,
) -> Vec<(BtcOutPoint, Result<OutpointSpendStatus>)> {
    use std::collections::BTreeMap;

    let mut by_txid: BTreeMap<BtcTxid, Vec<u32>> = BTreeMap::new();
    for outpoint in outpoints {
        by_txid
            .entry(outpoint.txid)
            .or_default()
            .push(outpoint.vout);
    }

    let groups: Vec<(BtcTxid, Vec<u32>)> = by_txid.into_iter().collect();
    let results = stream::iter(groups)
        .map(|(txid, mut vouts)| async move {
            vouts.sort_unstable();
            vouts.dedup();

            // One call for the whole transaction when it is worth it.
            if vouts.len() > 1 {
                match source.tx_outspends(&txid).await {
                    Ok(Some(statuses)) => {
                        return vouts
                            .into_iter()
                            .map(|vout| {
                                let outpoint = BtcOutPoint::new(txid, vout);
                                let status = statuses
                                    .get(vout as usize)
                                    .copied()
                                    .unwrap_or(OutpointSpendStatus::Unknown);
                                (outpoint, Ok(status))
                            })
                            .collect::<Vec<_>>();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Fall through to per-outpoint queries; a batch failure should
                        // not poison outpoints that could still be answered.
                        tracing::debug!(%txid, error = %e, "batched outspend lookup failed");
                    }
                }
            }

            let mut out = Vec::with_capacity(vouts.len());
            for vout in vouts {
                let outpoint = BtcOutPoint::new(txid, vout);
                let status = source.outpoint_status(&outpoint).await;
                out.push((outpoint, status));
            }
            out
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;

    results.into_iter().flatten().collect()
}

/// Serde helper: byte vectors as plain (un-prefixed) hex, matching Bitcoin APIs.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s.strip_prefix("0x").unwrap_or(&s)).map_err(serde::de::Error::custom)
    }
}
