//! The `rgbpp-indexer` binary.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rgbpp_indexer::{Engine, ShutdownController};
use rgbpp_types::config::Config;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "rgbpp-indexer",
    version,
    about = "An RGB++ indexer over a CKB rich indexer and a Bitcoin data source"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(
        short,
        long,
        env = "RGBPP_CONFIG",
        default_value = "config/default.toml"
    )]
    config: PathBuf,

    /// Log format: `text` or `json`.
    #[arg(long, env = "LOG_FORMAT", default_value = "text")]
    log_format: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the indexer and the API (default).
    Run,
    /// Apply database migrations and exit.
    Migrate,
    /// Validate the configuration and print what it resolves to.
    CheckConfig,
    /// Run one CKB scan round and exit.
    ScanOnce,
    /// Run one full sweep of every bound outpoint and exit.
    SweepOnce,
    /// Print indexer status as JSON.
    Status,
    /// Re-observe specific outpoints, given as `txid:vout`.
    Refresh {
        #[arg(required = true)]
        outpoints: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_format);

    let config = Config::load(&cli.config)
        .with_context(|| format!("loading configuration from {}", cli.config.display()))?;

    match cli.command.unwrap_or(Command::Run) {
        Command::CheckConfig => {
            // Print the resolved view, including environment overrides, so a
            // deployment can be verified without starting anything.
            println!("{}", toml::to_string_pretty(&config)?);
            println!("# rgbpp lock:    {}", config.protocol.rgbpp_lock.code_hash);
            println!(
                "# btc time lock: {}",
                config.protocol.btc_time_lock.code_hash
            );
            Ok(())
        }
        Command::Migrate => {
            let store = rgbpp_store::Store::connect(&config.database).await?;
            store.migrate().await?;
            info!("migrations applied");
            Ok(())
        }
        Command::ScanOnce => {
            let engine = Engine::bootstrap(config).await?;
            let round = engine.scanner().scan_once().await?;
            println!("{}", serde_json::to_string_pretty(&round_json(&round))?);
            Ok(())
        }
        Command::SweepOnce => {
            let engine = Engine::bootstrap(config).await?;
            let report = engine.sweeper().sweep_once().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Status => {
            let engine = Engine::bootstrap(config).await?;
            let state = engine
                .store
                .get_stream_state(rgbpp_store::state::CKB_STREAM)
                .await?;
            let counts = engine.store.counts().await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "indexed_to": state.as_ref().map(|s| s.last_block_number),
                    "chain_tip": state.as_ref().and_then(|s| s.chain_tip_number),
                    "reorg_lag": state.as_ref().map(|s| s.reorg_lag),
                    "last_error": state.as_ref().and_then(|s| s.last_error.clone()),
                    "counts": counts,
                }))?
            );
            Ok(())
        }
        Command::Refresh { outpoints } => {
            let engine = Engine::bootstrap(config).await?;
            let parsed = outpoints
                .iter()
                .map(|s| parse_outpoint(s))
                .collect::<Result<Vec<_>>>()?;
            let refreshed = engine.reconciler.refresh_outpoints(&parsed).await?;
            println!("{}", serde_json::to_string_pretty(&refreshed)?);
            Ok(())
        }
        Command::Run => run(config).await,
    }
}

async fn run(config: Config) -> Result<()> {
    let bind = config.api.bind.clone();
    let engine = Engine::bootstrap(config).await?;

    let shutdown = ShutdownController::new();
    let workers = engine.spawn_workers(&shutdown);

    let state = rgbpp_api::AppState::new(engine.clone());
    let api = tokio::spawn({
        let shutdown = shutdown.subscribe();
        async move {
            if let Err(e) = rgbpp_api::serve(state, &bind, shutdown).await {
                error!(error = %e, "api server stopped with an error");
            }
        }
    });

    wait_for_signal().await;
    info!("shutdown signalled; draining workers");
    shutdown.trigger();

    for worker in workers {
        let _ = worker.await;
    }
    let _ = api.await;
    info!("stopped");
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "cannot listen for SIGTERM; falling back to Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_tracing(format: &str) {
    // The defaults exist so that `RUST_LOG` is a tuning knob rather than a
    // requirement: without them, dependency chatter drowns out the indexer's own
    // lines at `info`.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,sqlx=warn,hyper=warn,hyper_util=warn,h2=warn,reqwest=warn,\
             rustls=warn,tower_http=warn",
        )
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if format == "json" {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn parse_outpoint(s: &str) -> Result<rgbpp_types::BtcOutPoint> {
    let (txid, vout) = s
        .split_once(':')
        .with_context(|| format!("expected `txid:vout`, got `{s}`"))?;
    Ok(rgbpp_types::BtcOutPoint::new(
        rgbpp_types::BtcTxid::from_hex(txid)?,
        vout.parse()
            .with_context(|| format!("bad output index `{vout}`"))?,
    ))
}

fn round_json(round: &rgbpp_indexer::ScanRound) -> serde_json::Value {
    serde_json::json!({
        "from": round.from,
        "to": round.to,
        "chain_tip": round.chain_tip,
        "target": round.target,
        "transactions": round.transactions,
        "cells": round.cells,
        "spends": round.spends,
        "backfilled_cells": round.backfilled_cells,
        "idle": round.idle,
    })
}
