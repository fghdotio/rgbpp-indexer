//! The RGB++ indexing engine.
//!
//! CKB is the discovery side and owns ordered facts; Bitcoin is the observation side
//! and owns re-queryable answers. See `docs/indexing.md` for how the workers fit
//! together and `docs/reorg.md` for why the scanner runs behind a lag.

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
