//! The RGB++ indexing engine.
//!
//! # How the two chains are used
//!
//! CKB is the **discovery** side. Every RGB++ state transition ends in a CKB
//! transaction touching an RGB++ lock, so scanning two script prefixes through the
//! rich indexer finds all of them — and `io_type` on each match says whether a cell
//! was created or consumed. That gives an ordered, dependency-bearing record of
//! protocol state, which is why the CKB tables carry block anchors and get a real
//! rollback.
//!
//! Bitcoin is the **observation** side. It answers "is this bound UTXO still unspent,
//! and if not, who spent it". Those answers are re-queryable, so they are cached with
//! timestamps rather than anchored to blocks, and a Bitcoin reorg degrades to expiring
//! the cache.
//!
//! # The gap between them
//!
//! The scanner deliberately stops at `tip - REORG_LAG`, and a CKB transaction may not
//! exist yet at all when its Bitcoin counterpart is broadcast. Nothing on the CKB side
//! can announce state in that window, so applications point us at it:
//! [`reconcile::Reconciler::reconcile_address`] diffs an address's live UTXO set
//! against what the indexer believes, and
//! [`reconcile::Reconciler::resolve_transition`] re-observes exactly the outpoints one
//! transaction touches.
//!
//! [`sweeper::Sweeper`] covers what nobody asks about: a bound UTXO spent by a wallet
//! that never knew it held an RGB++ asset.

pub mod address_backfill;
pub mod engine;
pub mod error;
pub mod extract;
pub mod heartbeat;
pub mod progress;
pub mod reconcile;
pub mod resolve;
pub mod scanner;
pub mod shutdown;
pub mod sweeper;
pub mod verify;

pub use address_backfill::{AddressBackfill, BackfillReport};
pub use engine::Engine;
pub use error::{IndexerError, Result};
pub use reconcile::{AddressReconcile, OutpointRefresh, Reconciler, TransitionResolution};
pub use scanner::{CkbScanner, ScanRound};
pub use shutdown::{Shutdown, ShutdownController};
pub use sweeper::{SweepReport, Sweeper};
