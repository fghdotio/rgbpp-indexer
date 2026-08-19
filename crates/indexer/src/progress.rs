//! Sync progress reporting.
//!
//! During initial sync the scanner completes a round every few hundred milliseconds.
//! Logging each one buries the two numbers anyone actually wants — how fast is it
//! going, and when will it be done — under thousands of lines that each say almost
//! nothing.
//!
//! So rounds are accumulated and reported on a fixed interval: one line per window,
//! carrying the block range covered, throughput, what was found, and a rate-derived
//! ETA. Per-round detail stays at `debug`. The line is emitted as structured fields
//! so it reads the same whether the sink is a terminal or JSON.

use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::scanner::ScanRound;

/// Accumulates scan rounds and emits a summary line per interval.
pub struct SyncReporter {
    interval: Duration,
    window_start: Instant,
    first_block: Option<u64>,
    last_block: u64,
    rounds: u64,
    blocks: u64,
    transactions: u64,
    cells: u64,
    spends: u64,
    backfilled: u64,
    /// The height the scanner started from, for the percentage.
    origin: u64,
    /// Cumulative totals since this process started. The window rate is responsive
    /// but jumps around with RGB++ density; an ETA built on it swings between hours
    /// and days and stops being worth reading. The session average is what the ETA
    /// uses, so it converges instead.
    session_start: Instant,
    session_blocks: u64,
    /// Whether the last report said we were caught up, so the transition is logged
    /// once rather than on every idle poll.
    caught_up: bool,
}

impl SyncReporter {
    pub fn new(interval: Duration, origin: u64) -> Self {
        SyncReporter {
            interval,
            window_start: Instant::now(),
            session_start: Instant::now(),
            session_blocks: 0,
            first_block: None,
            last_block: 0,
            rounds: 0,
            blocks: 0,
            transactions: 0,
            cells: 0,
            spends: 0,
            backfilled: 0,
            origin,
            caught_up: false,
        }
    }

    /// Fold in a completed round, emitting a summary if the window has elapsed.
    pub fn record(&mut self, round: &ScanRound) {
        if round.idle {
            self.note_idle(round);
            return;
        }

        debug!(
            from = round.from,
            to = round.to,
            tx = round.transactions,
            cells = round.cells,
            spends = round.spends,
            backfilled = round.backfilled_cells,
            "round"
        );

        self.caught_up = false;
        self.first_block.get_or_insert(round.from);
        self.last_block = round.to;
        self.rounds += 1;
        let scanned = round.to.saturating_sub(round.from) + 1;
        self.blocks += scanned;
        self.session_blocks += scanned;
        self.transactions += round.transactions as u64;
        self.cells += round.cells;
        self.spends += round.spends;
        self.backfilled += round.backfilled_cells as u64;

        if self.window_start.elapsed() >= self.interval {
            self.emit(round.target, round.chain_tip);
        }
    }

    /// Report reaching the target, once per catch-up rather than once per poll.
    fn note_idle(&mut self, round: &ScanRound) {
        if self.blocks > 0 {
            self.emit(round.target, round.chain_tip);
        }
        if !self.caught_up {
            self.caught_up = true;
            info!(
                indexed_to = round.target,
                tip = round.chain_tip,
                lag = round.chain_tip.saturating_sub(round.target),
                "ckb caught up"
            );
        }
    }

    /// Flush whatever is pending, e.g. on shutdown.
    pub fn flush(&mut self, target: u64, chain_tip: u64) {
        if self.blocks > 0 {
            self.emit(target, chain_tip);
        }
    }

    fn emit(&mut self, target: u64, chain_tip: u64) {
        let secs = self.window_start.elapsed().as_secs_f64().max(0.001);
        let rate = self.blocks as f64 / secs;

        let session_secs = self.session_start.elapsed().as_secs_f64().max(0.001);
        let session_rate = self.session_blocks as f64 / session_secs;

        let behind = target.saturating_sub(self.last_block);

        // Percentage of the whole catch-up, not of this window.
        let span = target.saturating_sub(self.origin).max(1);
        let done = self.last_block.saturating_sub(self.origin);
        let pct = (done as f64 / span as f64) * 100.0;

        info!(
            range = %format_args!("{}..{}", self.first_block.unwrap_or(self.last_block), self.last_block),
            blk = self.blocks,
            rate = %format_args!("{rate:.0}/s"),
            tx = self.transactions,
            cells = self.cells,
            spends = self.spends,
            backfilled = self.backfilled,
            behind = behind,
            tip = chain_tip,
            eta = %eta(behind, session_rate),
            pct = %format_args!("{pct:.2}"),
            "ckb sync"
        );

        self.window_start = Instant::now();
        self.first_block = None;
        self.rounds = 0;
        self.blocks = 0;
        self.transactions = 0;
        self.cells = 0;
        self.spends = 0;
        self.backfilled = 0;
    }
}

/// Estimated time to cover `remaining` blocks at `rate` blocks per second.
fn eta(remaining: u64, rate: f64) -> String {
    if remaining == 0 {
        return "0s".to_string();
    }
    if !rate.is_finite() || rate <= 0.0 {
        return "?".to_string();
    }
    human_duration((remaining as f64 / rate).ceil() as u64)
}

/// Compact duration: at most two units, largest first.
pub fn human_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_stay_two_units_wide() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(90), "1m30s");
        assert_eq!(human_duration(3_600), "1h0m");
        assert_eq!(human_duration(3_661), "1h1m");
        assert_eq!(human_duration(90_000), "1d1h");
    }

    #[test]
    fn eta_handles_the_degenerate_cases() {
        assert_eq!(eta(0, 100.0), "0s");
        assert_eq!(eta(1_000, 0.0), "?");
        assert_eq!(eta(1_000, f64::NAN), "?");
        assert_eq!(eta(1_000, 100.0), "10s");
        assert_eq!(eta(9_000_000, 5_000.0), "30m0s");
    }

    fn round(from: u64, to: u64, target: u64) -> ScanRound {
        ScanRound {
            from,
            to,
            chain_tip: target + 24,
            target,
            transactions: 2,
            cells: 3,
            spends: 1,
            backfilled_cells: 0,
            idle: false,
        }
    }

    #[test]
    fn the_eta_uses_the_session_average_not_the_window() {
        let mut reporter = SyncReporter::new(Duration::from_secs(3600), 0);
        reporter.record(&round(0, 999, 10_000));
        assert_eq!(reporter.session_blocks, 1_000);
        reporter.flush(10_000, 10_024);
        // Flushing resets the window but never the session totals: that is what makes
        // the ETA converge instead of swinging with each window's block density.
        assert_eq!(reporter.session_blocks, 1_000);
        assert_eq!(reporter.blocks, 0);

        reporter.record(&round(1_000, 1_999, 10_000));
        assert_eq!(reporter.session_blocks, 2_000);
    }

    #[test]
    fn rounds_accumulate_until_the_window_elapses() {
        let mut reporter = SyncReporter::new(Duration::from_secs(3600), 100);
        reporter.record(&round(100, 199, 10_000));
        reporter.record(&round(200, 299, 10_000));
        // Nothing emitted yet, so the accumulator still holds both rounds.
        assert_eq!(reporter.blocks, 200);
        assert_eq!(reporter.transactions, 4);
        assert_eq!(reporter.last_block, 299);
        assert_eq!(reporter.first_block, Some(100));

        reporter.flush(10_000, 10_024);
        assert_eq!(reporter.blocks, 0, "flushing resets the window");
        assert_eq!(reporter.first_block, None);
    }

    #[test]
    fn an_idle_round_flushes_and_latches_caught_up() {
        let mut reporter = SyncReporter::new(Duration::from_secs(3600), 100);
        reporter.record(&round(100, 199, 199));

        let idle = ScanRound {
            chain_tip: 223,
            target: 199,
            idle: true,
            ..Default::default()
        };
        reporter.record(&idle);
        assert!(reporter.caught_up);
        assert_eq!(reporter.blocks, 0);

        // Staying idle must not keep re-announcing it.
        reporter.record(&idle);
        assert!(reporter.caught_up);
    }
}
