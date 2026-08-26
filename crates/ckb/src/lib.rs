//! The CKB half of the data layer: the node's JSON-RPC for headers and transactions,
//! and the rich indexer's `get_transactions` for discovery by lock script.

pub mod client;
pub mod error;
pub mod rpc;
pub mod types;

pub use client::CkbClient;
pub use error::{CkbError, Result};
pub use types::{
    CellsCapacity, IndexerTip, IoType, Order, ScriptSearchMode, ScriptType, SearchKey,
    SearchKeyFilter, TransactionWithStatus, TxRecord, TxStatus,
};
