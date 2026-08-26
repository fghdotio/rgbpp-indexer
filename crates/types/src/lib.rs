//! Shared domain types for the RGB++ indexer.
//!
//! This crate deliberately has no I/O dependencies: it holds the vocabulary that
//! the CKB data layer, the Bitcoin data layer, the store and the API all agree on,
//! plus the pure protocol logic (script argument parsing, molecule encoding and
//! RGB++ commitment computation).

pub mod asset;
pub mod bitcoin;
pub mod ckb;
pub mod commitment;
pub mod config;
pub mod error;
pub mod molecule;
pub mod protocol;
pub mod state;

pub use bitcoin::{BtcOutPoint, BtcTxid};
pub use ckb::{CellOutput, CkbOutPoint, Script, ScriptHashType, H256};
pub use error::{Error, Result};
pub use protocol::{BtcTimeLockArgs, LockKind, RgbppLockArgs};
pub use state::{CellStatus, OutpointSpendStatus, TransitionKind};
