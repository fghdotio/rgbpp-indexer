//! The CKB client the indexer talks to.

use std::time::Duration;

use rgbpp_types::ckb::{Script, H256};
use serde_json::{json, Value};
use tracing::debug;

use crate::error::{CkbError, Result};
use crate::rpc::JsonRpcClient;
use crate::types::{
    CellRecord, IndexerTip, Order, Pagination, RpcHeader, SearchKey, TransactionWithStatus,
    TxRecord, Uint32, Uint64,
};

#[derive(Debug)]
pub struct CkbClient {
    node: JsonRpcClient,
    indexer: JsonRpcClient,
    /// `get_transactions` page size.
    page_limit: u32,
}

impl CkbClient {
    pub fn new(
        node_url: &str,
        indexer_url: &str,
        timeout: Duration,
        page_limit: u32,
    ) -> Result<Self> {
        Ok(CkbClient {
            node: JsonRpcClient::new(node_url, timeout)?,
            indexer: JsonRpcClient::new(indexer_url, timeout)?,
            page_limit: page_limit.max(1),
        })
    }

    pub fn node_url(&self) -> &str {
        self.node.url()
    }

    // --- node ------------------------------------------------------------

    pub async fn get_tip_block_number(&self) -> Result<u64> {
        let n: Uint64 = self.node.call("get_tip_block_number", json!([])).await?;
        Ok(n.0)
    }

    pub async fn get_header_by_number(&self, number: u64) -> Result<Option<RpcHeader>> {
        self.node
            .call("get_header_by_number", json!([Uint64(number)]))
            .await
    }

    pub async fn get_header(&self, hash: &H256) -> Result<Option<RpcHeader>> {
        self.node.call("get_header", json!([hash])).await
    }

    /// Full transaction plus its chain status.
    ///
    /// Verbosity 2 asks for the decoded transaction; `only_committed = false` keeps
    /// pending and proposed transactions visible, which is what makes point-lookups
    /// useful inside the `REORG_LAG` window.
    pub async fn get_transaction(&self, hash: &H256) -> Result<Option<TransactionWithStatus>> {
        self.node
            .call("get_transaction", json!([hash, Uint32(2), false]))
            .await
    }

    // --- rich indexer ----------------------------------------------------

    pub async fn get_indexer_tip(&self) -> Result<Option<IndexerTip>> {
        self.indexer.call("get_indexer_tip", json!([])).await
    }

    /// The highest block it is safe to read from *both* endpoints.
    ///
    /// The rich indexer can trail the node it follows. Using the node tip alone
    /// would make the scanner ask for a range the indexer has not populated, and it
    /// would answer "no transactions" rather than "not yet" — a silent data loss.
    pub async fn synced_tip(&self) -> Result<u64> {
        let node_tip = self.get_tip_block_number().await?;
        match self.get_indexer_tip().await? {
            Some(indexer_tip) => Ok(node_tip.min(indexer_tip.block_number.0)),
            None => Ok(node_tip),
        }
    }

    pub async fn get_transactions_page(
        &self,
        search_key: &SearchKey,
        order: Order,
        limit: u32,
        after: Option<&str>,
    ) -> Result<Pagination<TxRecord>> {
        let params = match after {
            Some(cursor) => json!([search_key, order, Uint32(limit), cursor]),
            None => json!([search_key, order, Uint32(limit)]),
        };
        self.indexer.call("get_transactions", params).await
    }

    /// Collect every transaction touching `script` (as a lock, prefix match) inside
    /// `[from, to]`.
    ///
    /// The block range is sent as the half-open `[from, to + 1)` the rich indexer
    /// documents, and the result is filtered again in memory. That belt-and-braces
    /// check costs nothing and makes the scanner immune to an off-by-one in the
    /// range's inclusivity — an error that would otherwise silently skip a block.
    pub async fn collect_transactions_in_range(
        &self,
        script: Script,
        from: u64,
        to: u64,
    ) -> Result<Vec<TxRecord>> {
        if from > to {
            return Ok(Vec::new());
        }
        let search_key = SearchKey::lock_prefix(script).with_block_range(from, to + 1);

        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .get_transactions_page(&search_key, Order::Asc, self.page_limit, cursor.as_deref())
                .await?;
            let returned = page.objects.len();
            for record in page.objects {
                let number = record.block_number.0;
                if number < from || number > to {
                    debug!(number, from, to, "dropping out-of-range record");
                    continue;
                }
                out.push(record);
            }
            // An empty `last_cursor` (or a short page) means the range is exhausted.
            if returned < self.page_limit as usize || page.last_cursor.is_empty() {
                break;
            }
            cursor = Some(page.last_cursor.to_hex());
        }

        // The indexer orders by (block_number, tx_index) but two search keys get
        // merged by the caller, so make the ordering explicit here.
        out.sort_by_key(|r| (r.block_number.0, r.tx_index.0));
        Ok(out)
    }

    /// Live cells matching a search key.
    ///
    /// This is the point-lookup path: it reads the rich indexer's current view, not
    /// the lagged range the scanner has processed, so it can confirm that a cell for
    /// a given Bitcoin outpoint exists before the scanner reaches that block.
    pub async fn get_cells(&self, search_key: &SearchKey, limit: u32) -> Result<Vec<CellRecord>> {
        let page: Pagination<CellRecord> = self
            .indexer
            .call("get_cells", json!([search_key, Order::Asc, Uint32(limit)]))
            .await?;
        Ok(page.objects)
    }

    /// Raw escape hatch for methods this client does not wrap.
    pub async fn node_call(&self, method: &str, params: Value) -> Result<Value> {
        self.node.call_value(method, params).await
    }
}

/// Convenience: the "match any args" form of a lock script.
pub fn any_args(code_hash: H256, hash_type: rgbpp_types::ckb::ScriptHashType) -> Script {
    Script::new(code_hash, hash_type, Vec::new())
}

impl From<CkbError> for rgbpp_types::Error {
    fn from(e: CkbError) -> Self {
        rgbpp_types::Error::malformed("ckb rpc", e.to_string())
    }
}
