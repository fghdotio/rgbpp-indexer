//! Persistence.
//!
//! Queries are written at runtime (`sqlx::query`), not with the compile-time macros,
//! so building the project never requires a live database.
//!
//! Two invariants shape everything here:
//!
//! * **A scan round is one transaction.** Headers, cells, spends, transitions and the
//!   checkpoint advance together, so a crash can never leave the checkpoint ahead of
//!   the data it claims to cover.
//! * **Nothing aggregated is stored.** Balances and counts are `SELECT`s over the
//!   fact tables. This version does not implement reorg handling, but that choice is
//!   what will make it a pure deletion later instead of a reverse-accounting
//!   exercise — see `docs/reorg.md`.

pub mod activity;
pub mod anomalies;
pub mod blocks;
pub mod btc;
pub mod cells;
pub mod error;
pub mod models;
pub mod queue;
pub mod state;
pub mod stats;
pub mod transitions;

pub use error::{Result, StoreError};
pub use models::*;

use std::time::Duration;

use rgbpp_types::config::DatabaseConfig;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tracing::info;

#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
            .connect(&config.url)
            .await?;
        Ok(Store { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Store { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        info!("database migrations applied");
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}
