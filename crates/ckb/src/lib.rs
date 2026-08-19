//! The CKB half of the data layer.
//!
//! Two endpoints, one client: the node's JSON-RPC (headers, full transactions) and
//! the rich indexer's `get_transactions` (discovery by lock script). The indexer
//! treats CKB as the *entry point* for discovery — every RGB++ state transition
//! eventually lands in a CKB transaction that touches an RGB++ lock, so scanning
//! that one script prefix finds all of them.

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
