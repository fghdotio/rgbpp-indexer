//! Configuration.
//!
//! Deployed RGB++ code hashes are **not** compiled in. They differ per network and
//! change when contracts are redeployed, and an indexer that silently watches the
//! wrong script hash looks healthy while indexing nothing. Making them required
//! configuration turns that failure into a startup error.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::asset::AssetScripts;
use crate::error::{Error, Result};
use crate::protocol::ProtocolScripts;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    pub database: DatabaseConfig,
    pub ckb: CkbConfig,
    pub btc: BtcConfig,
    pub protocol: ProtocolScripts,
    #[serde(default)]
    pub assets: AssetScripts,
    #[serde(default)]
    pub reconcile: ReconcileConfig,
    #[serde(default)]
    pub sweep: SweepConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub verify: VerifyConfig,
    #[serde(default)]
    pub log: LogConfig,
}

/// Log cadence.
///
/// The scanner completes a round every few hundred milliseconds during initial sync.
/// These two intervals are what keep that from becoming thousands of near-identical
/// lines: progress is summarised per window, and liveness is a periodic one-liner.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LogConfig {
    /// How often to emit a sync progress summary while catching up.
    #[serde(default = "default_progress_interval")]
    pub progress_interval_secs: u64,
    /// How often to emit the operational one-liner (heights, counts, queue depth).
    /// Zero disables it.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
}

fn default_progress_interval() -> u64 {
    15
}
fn default_heartbeat_interval() -> u64 {
    300
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            progress_interval_secs: default_progress_interval(),
            heartbeat_interval_secs: default_heartbeat_interval(),
        }
    }
}

impl LogConfig {
    pub fn progress_interval(&self) -> Duration {
        Duration::from_secs(self.progress_interval_secs.max(1))
    }

    pub fn heartbeat_interval(&self) -> Option<Duration> {
        (self.heartbeat_interval_secs > 0).then(|| Duration::from_secs(self.heartbeat_interval_secs))
    }
}

/// Cross-chain verification switches.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerifyConfig {
    /// Compare the commitment computed from the CKB transaction against the one
    /// published in the Bitcoin transaction's `OP_RETURN`.
    ///
    /// On by default: the encoding is pinned by regression tests built from real
    /// on-chain transactions (`crates/indexer/tests/commitment_vectors.rs`). Turn it
    /// off to stop the verifier making Bitcoin requests. Discovery never depends on
    /// it either way — a mismatch records an anomaly, it never drops a fact.
    #[serde(default = "default_true")]
    pub commitments: bool,
    /// How many unchecked transitions to verify per pass.
    #[serde(default = "default_verify_batch")]
    pub batch_size: i64,
}

fn default_verify_batch() -> i64 {
    100
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig {
            commitments: true,
            batch_size: default_verify_batch(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GeneralConfig {
    /// Free-form label used in logs and the `/status` endpoint.
    #[serde(default = "default_network")]
    pub network: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        GeneralConfig {
            network: default_network(),
        }
    }
}

fn default_network() -> String {
    "mainnet".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_db_timeout_secs")]
    pub acquire_timeout_secs: u64,
    /// Run pending migrations on startup.
    #[serde(default = "default_true")]
    pub auto_migrate: bool,
}

fn default_max_connections() -> u32 {
    16
}
fn default_db_timeout_secs() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CkbConfig {
    /// CKB node JSON-RPC endpoint.
    pub rpc_url: String,
    /// Rich indexer endpoint. Usually the same node with `--indexer` enabled, but
    /// large deployments run it separately.
    #[serde(default)]
    pub indexer_rpc_url: Option<String>,
    /// Block where RGB++ went live. Scanning earlier is wasted work.
    pub start_block: u64,
    /// Stay this many blocks behind the tip. Everything indexed is treated as
    /// settled; the gap is covered by on-demand Bitcoin-driven refresh.
    #[serde(default = "default_reorg_lag")]
    pub reorg_lag: u64,
    /// Upper bound on the block span of a single scan round.
    #[serde(default = "default_batch_blocks")]
    pub batch_blocks: u64,
    /// `get_transactions` page size.
    #[serde(default = "default_page_limit")]
    pub page_limit: u32,
    #[serde(default = "default_ckb_poll_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// How many block headers to keep. Older ones are pruned.
    ///
    /// Nothing in this version reads them beyond the checkpoint sanity check; they
    /// are retained because the rollback design in `docs/reorg.md` needs this
    /// ancestry to find a common ancestor without one RPC per block.
    #[serde(default = "default_header_retention")]
    pub header_retention: u64,
}

fn default_reorg_lag() -> u64 {
    24
}
fn default_batch_blocks() -> u64 {
    500
}
fn default_page_limit() -> u32 {
    200
}
fn default_ckb_poll_secs() -> u64 {
    5
}
fn default_request_timeout_secs() -> u64 {
    30
}
fn default_header_retention() -> u64 {
    20_000
}

impl CkbConfig {
    pub fn indexer_url(&self) -> &str {
        self.indexer_rpc_url.as_deref().unwrap_or(&self.rpc_url)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BtcSourceKind {
    /// Esplora-compatible REST API: mempool.space, blockstream/electrs, self-hosted.
    Esplora,
    /// Trezor Blockbook REST API.
    Blockbook,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BtcConfig {
    pub source: BtcSourceKind,
    /// Base URL including any API path prefix, e.g. `https://mempool.space/api`.
    pub base_url: String,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Concurrent in-flight requests to the data source.
    #[serde(default = "default_btc_concurrency")]
    pub max_concurrency: usize,
    /// Minimum spacing between requests, for public endpoints with rate limits.
    #[serde(default = "default_min_interval_ms")]
    pub min_request_interval_ms: u64,
    /// How long an outpoint observation counts as fresh before a re-check.
    #[serde(default = "default_observation_ttl_secs")]
    pub observation_ttl_secs: u64,
    /// Confirmations after which a Bitcoin fact is treated as settled.
    #[serde(default = "default_confirmations")]
    pub confirmations_for_final: u32,
}

fn default_btc_concurrency() -> usize {
    8
}
fn default_min_interval_ms() -> u64 {
    50
}
fn default_observation_ttl_secs() -> u64 {
    60
}
fn default_confirmations() -> u32 {
    6
}
impl BtcConfig {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    pub fn observation_ttl(&self) -> Duration {
        Duration::from_secs(self.observation_ttl_secs)
    }
}

/// The on-demand path: applications tell us where to look.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReconcileConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cap on how many outpoints one request may force-refresh, so a single caller
    /// cannot saturate the Bitcoin data source.
    #[serde(default = "default_max_outpoints")]
    pub max_outpoints_per_request: usize,
    /// Interval of the background worker that drains the refresh queue.
    #[serde(default = "default_queue_interval_secs")]
    pub queue_interval_secs: u64,
    #[serde(default = "default_queue_batch")]
    pub queue_batch_size: i64,
    /// Give up on a queued refresh after this many failures.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: i32,
}

fn default_max_outpoints() -> usize {
    200
}
fn default_queue_interval_secs() -> u64 {
    3
}
fn default_queue_batch() -> i64 {
    100
}
fn default_max_attempts() -> i32 {
    5
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        ReconcileConfig {
            enabled: true,
            max_outpoints_per_request: default_max_outpoints(),
            queue_interval_secs: default_queue_interval_secs(),
            queue_batch_size: default_queue_batch(),
            max_attempts: default_max_attempts(),
        }
    }
}

/// The daily safety net over every bound outpoint we know about.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SweepConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_sweep_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_sweep_batch")]
    pub batch_size: i64,
    /// A UTXO spent on Bitcoin with no CKB transition after this many confirmations
    /// is flagged as a probable accidental spend.
    #[serde(default = "default_misspend_grace")]
    pub misspend_grace_confirmations: u32,
    /// Only run misspend detection when the CKB indexer is within this many blocks of
    /// its target.
    ///
    /// "No CKB transition exists" and "the CKB transition is in a range we have not
    /// indexed yet" look identical from the Bitcoin side. During initial sync that
    /// would flag every historical binding as a misspend, so detection waits until
    /// the indexed range is meaningful.
    #[serde(default = "default_sync_requirement")]
    pub require_synced_within_blocks: u64,
}

fn default_sync_requirement() -> u64 {
    1_000
}

fn default_sweep_interval_secs() -> u64 {
    86_400
}
fn default_sweep_batch() -> i64 {
    500
}
fn default_misspend_grace() -> u32 {
    6
}

impl Default for SweepConfig {
    fn default() -> Self {
        SweepConfig {
            enabled: true,
            interval_secs: default_sweep_interval_secs(),
            batch_size: default_sweep_batch(),
            misspend_grace_confirmations: default_misspend_grace(),
            require_synced_within_blocks: default_sync_requirement(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_page_size")]
    pub default_page_size: i64,
    #[serde(default = "default_max_page_size")]
    pub max_page_size: i64,
}

fn default_bind() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_page_size() -> i64 {
    100
}
fn default_max_page_size() -> i64 {
    1000
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            bind: default_bind(),
            default_page_size: default_page_size(),
            max_page_size: default_max_page_size(),
        }
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let mut config: Config =
            toml::from_str(s).map_err(|e| Error::Config(format!("invalid TOML: {e}")))?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// A small, fixed set of environment overrides — the ones that differ between
    /// deployments of the same config file.
    fn apply_env_overrides(&mut self) {
        self.apply_overrides(|key| {
            // An empty value counts as unset. `docker compose` renders an unset
            // variable as the empty string, so without this an untouched
            // `CKB_RPC_URL: ${CKB_RPC_URL:-}` in a compose file silently blanks the
            // URL the config file supplied.
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    }

    /// The override logic, with the environment injected so it can be tested.
    fn apply_overrides(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("DATABASE_URL") {
            self.database.url = v;
        }
        if let Some(v) = get("CKB_RPC_URL") {
            self.ckb.rpc_url = v;
        }
        if let Some(v) = get("CKB_INDEXER_RPC_URL") {
            self.ckb.indexer_rpc_url = Some(v);
        }
        if let Some(v) = get("CKB_START_BLOCK") {
            match v.replace('_', "").parse() {
                Ok(n) => self.ckb.start_block = n,
                Err(_) => eprintln!("ignoring CKB_START_BLOCK={v:?}: not a block number"),
            }
        }
        if let Some(v) = get("REORG_LAG") {
            match v.parse() {
                Ok(n) => self.ckb.reorg_lag = n,
                Err(_) => eprintln!("ignoring REORG_LAG={v:?}: not a number"),
            }
        }
        if let Some(v) = get("BTC_BASE_URL") {
            self.btc.base_url = v;
        }
        if let Some(v) = get("BTC_SOURCE") {
            match v.as_str() {
                "esplora" => self.btc.source = BtcSourceKind::Esplora,
                "blockbook" => self.btc.source = BtcSourceKind::Blockbook,
                other => eprintln!("ignoring BTC_SOURCE={other:?}: expected esplora or blockbook"),
            }
        }
        if let Some(v) = get("API_BIND") {
            self.api.bind = v;
        }
    }

    fn validate(&self) -> Result<()> {
        if self.database.url.is_empty() {
            return Err(Error::Config("database.url is empty".into()));
        }
        if self.ckb.rpc_url.is_empty() {
            return Err(Error::Config("ckb.rpc_url is empty".into()));
        }
        if self.btc.base_url.is_empty() {
            return Err(Error::Config("btc.base_url is empty".into()));
        }
        if self.ckb.page_limit == 0 {
            return Err(Error::Config("ckb.page_limit must be positive".into()));
        }
        if self.ckb.batch_blocks == 0 {
            return Err(Error::Config("ckb.batch_blocks must be positive".into()));
        }
        if self.protocol.rgbpp_lock.code_hash == crate::ckb::H256::ZERO {
            return Err(Error::Config(
                "protocol.rgbpp_lock.code_hash is unset — configure the code hash of the \
                 RGB++ lock deployment you intend to index"
                    .into(),
            ));
        }
        if self.protocol.btc_time_lock.code_hash == crate::ckb::H256::ZERO {
            return Err(Error::Config(
                "protocol.btc_time_lock.code_hash is unset".into(),
            ));
        }
        Ok(())
    }

    /// Highest block the CKB indexer is allowed to touch, given a node tip.
    pub fn safe_ckb_target(&self, tip: u64) -> Option<u64> {
        tip.checked_sub(self.ckb.reorg_lag)
            .filter(|target| *target >= self.ckb.start_block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[database]
url = "postgres://localhost/rgbpp"

[ckb]
rpc_url = "http://127.0.0.1:8114"
start_block = 100

[btc]
source = "esplora"
base_url = "https://mempool.space/api"

[protocol.rgbpp_lock]
code_hash = "0xbc6c568a1a0d0a09f6844dc9d74ddb4343c32143ff25f727c59edf4fb72d6936"
hash_type = "type"

[protocol.btc_time_lock]
code_hash = "0x70d64497a075bd651e98ac030455ea200637ee325a12ad08aff03f1a117e5a62"
hash_type = "type"
"#;

    #[test]
    fn minimal_config_parses_with_defaults() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(config.ckb.reorg_lag, 24);
        assert_eq!(config.ckb.indexer_url(), "http://127.0.0.1:8114");
        assert!(config.sweep.enabled);
    }

    #[test]
    fn empty_environment_values_do_not_clobber_the_config() {
        // `docker compose` writes an unset `${VAR:-}` through as an empty string.
        // Treating that as an override blanks required fields and the process dies
        // at startup with a confusing "ckb.rpc_url is empty".
        let mut config = Config::from_toml_str(MINIMAL).unwrap();
        let overrides: std::collections::HashMap<&str, &str> = [
            ("CKB_RPC_URL", ""),
            ("BTC_BASE_URL", "   "),
            ("DATABASE_URL", ""),
        ]
        .into_iter()
        .collect();

        config.apply_overrides(|key| {
            overrides
                .get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });

        assert_eq!(config.ckb.rpc_url, "http://127.0.0.1:8114");
        assert_eq!(config.btc.base_url, "https://mempool.space/api");
        assert_eq!(config.database.url, "postgres://localhost/rgbpp");
    }

    #[test]
    fn environment_overrides_win_when_they_carry_a_value() {
        let mut config = Config::from_toml_str(MINIMAL).unwrap();
        let overrides: std::collections::HashMap<&str, &str> = [
            ("CKB_RPC_URL", "https://mainnet.ckb.dev/rpc"),
            ("REORG_LAG", "48"),
            ("CKB_START_BLOCK", "11_800_000"),
            ("BTC_SOURCE", "blockbook"),
            ("REORG_LAG_TYPO", "nonsense"),
        ]
        .into_iter()
        .collect();

        config.apply_overrides(|key| overrides.get(key).map(|v| v.to_string()));

        assert_eq!(config.ckb.rpc_url, "https://mainnet.ckb.dev/rpc");
        assert_eq!(config.ckb.reorg_lag, 48);
        assert_eq!(config.ckb.start_block, 11_800_000, "underscores are tolerated");
        assert_eq!(config.btc.source, BtcSourceKind::Blockbook);
    }

    #[test]
    fn an_unparseable_override_is_ignored_rather_than_fatal() {
        let mut config = Config::from_toml_str(MINIMAL).unwrap();
        let overrides: std::collections::HashMap<&str, &str> =
            [("REORG_LAG", "soon"), ("BTC_SOURCE", "electrum")]
                .into_iter()
                .collect();

        config.apply_overrides(|key| overrides.get(key).map(|v| v.to_string()));

        assert_eq!(config.ckb.reorg_lag, 24, "kept the configured value");
        assert_eq!(config.btc.source, BtcSourceKind::Esplora);
    }

    #[test]
    fn zero_code_hash_is_rejected() {
        let bad = MINIMAL.replace(
            "0xbc6c568a1a0d0a09f6844dc9d74ddb4343c32143ff25f727c59edf4fb72d6936",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(Config::from_toml_str(&bad).is_err());
    }

    #[test]
    fn safe_target_respects_lag_and_start_block() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(config.safe_ckb_target(1_000), Some(976));
        // Below the start block there is nothing worth scanning yet.
        assert_eq!(config.safe_ckb_target(110), None);
        assert_eq!(config.safe_ckb_target(10), None);
    }
}
