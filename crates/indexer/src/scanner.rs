//! The CKB discovery loop.
//!
//! Scans two lock prefixes through the rich indexer up to `tip - REORG_LAG`. A round
//! is one database transaction and never calls Bitcoin. See `docs/indexing.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use rgbpp_ckb::types::{IoType, RpcHeader, TransactionWithStatus, TxRecord};
use rgbpp_ckb::CkbClient;
use rgbpp_store::models::{BlockRecord, IndexBatch, NewCell};
use rgbpp_store::state::CKB_STREAM;
use rgbpp_store::Store;
use rgbpp_types::ckb::{CellOutput, CkbOutPoint, Script, H256};
use rgbpp_types::config::Config;
use rgbpp_types::protocol::{LockBinding, LockKind, ProtocolScripts};
use tracing::{debug, error, info, warn};

use crate::error::{IndexerError, Result};
use crate::extract::{self, BlockContext, ResolvedInput};
use crate::progress::SyncReporter;
use crate::resolve::{self, UNKNOWN_TX_INDEX};
use crate::shutdown::Shutdown;

pub struct CkbScanner {
    store: Store,
    ckb: Arc<CkbClient>,
    config: Arc<Config>,
}

/// What one scan round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanRound {
    pub from: u64,
    pub to: u64,
    pub chain_tip: u64,
    pub target: u64,
    pub transactions: usize,
    pub cells: u64,
    pub spends: u64,
    /// Cells consumed that the indexer had never recorded as created, and fetched
    /// from the node to fill the gap.
    pub backfilled_cells: usize,
    pub idle: bool,
}

impl CkbScanner {
    pub fn new(store: Store, ckb: Arc<CkbClient>, config: Arc<Config>) -> Self {
        CkbScanner { store, ckb, config }
    }

    pub async fn run(self, mut shutdown: Shutdown) {
        let interval = self.config.ckb.poll_interval();
        let state = match self
            .store
            .init_stream(CKB_STREAM, self.config.ckb.start_block)
            .await
        {
            Ok(state) => state,
            Err(e) => {
                error!(error = %e, "cannot initialise the ckb indexer stream");
                return;
            }
        };

        info!(
            start_block = self.config.ckb.start_block,
            resume_from = state.last_block_number,
            reorg_lag = self.config.ckb.reorg_lag,
            batch_blocks = self.config.ckb.batch_blocks,
            poll_secs = self.config.ckb.poll_interval_secs,
            "ckb scanner started"
        );

        // Progress is measured from where this process resumed, so the ETA reflects
        // the work actually left rather than the whole history.
        let mut reporter = SyncReporter::new(
            self.config.log.progress_interval(),
            state.last_block_number.max(0) as u64,
        );
        let mut last_seen = (0u64, 0u64); // (target, chain tip), for the final flush

        loop {
            match self.scan_once().await {
                Ok(round) => {
                    last_seen = (round.target, round.chain_tip);
                    let backlog = !round.idle && round.to < round.target;
                    reporter.record(&round);
                    // Keep going without sleeping while there is a backlog.
                    if backlog && !shutdown.is_triggered() {
                        continue;
                    }
                }
                Err(e) => {
                    error!(error = %e, "ckb scan round failed");
                    let _ = self
                        .store
                        .set_stream_error(CKB_STREAM, Some(&e.to_string()))
                        .await;
                }
            }

            if shutdown.sleep(interval).await {
                reporter.flush(last_seen.0, last_seen.1);
                info!("ckb scanner stopped");
                return;
            }
        }
    }

    /// One bounded unit of work: scan a block range and commit it atomically.
    pub async fn scan_once(&self) -> Result<ScanRound> {
        let state = self
            .store
            .init_stream(CKB_STREAM, self.config.ckb.start_block)
            .await?;
        let checkpoint = state.last_block_number;

        let chain_tip = self.ckb.synced_tip().await?;
        let Some(target) = self.config.safe_ckb_target(chain_tip) else {
            self.store
                .record_chain_progress(CKB_STREAM, chain_tip, 0, self.config.ckb.reorg_lag)
                .await?;
            return Ok(ScanRound {
                chain_tip,
                idle: true,
                ..Default::default()
            });
        };

        self.store
            .record_chain_progress(CKB_STREAM, chain_tip, target, self.config.ckb.reorg_lag)
            .await?;

        if (target as i64) <= checkpoint {
            return Ok(ScanRound {
                chain_tip,
                target,
                idle: true,
                ..Default::default()
            });
        }

        self.assert_checkpoint_still_canonical(&state).await?;

        let from = (checkpoint + 1).max(self.config.ckb.start_block as i64) as u64;
        let to = target.min(from + self.config.ckb.batch_blocks - 1);

        let records = self.collect_range(from, to).await?;
        let mut round = ScanRound {
            from,
            to,
            chain_tip,
            target,
            transactions: records.len(),
            ..Default::default()
        };

        // Headers first, concurrently. Every record needs the hash and timestamp of
        // its block, and distinct blocks are far fewer than transactions, so this is
        // a small fan-out that removes an await from the processing loop entirely.
        let headers = self.fetch_headers(&records).await?;

        let mut batch = IndexBatch {
            chain_tip: chain_tip as i64,
            target: target as i64,
            reorg_lag: self.config.ckb.reorg_lag as i64,
            ..Default::default()
        };

        // The rich indexer matches transactions without returning their bodies, so
        // each match costs a second call. Those are issued concurrently while results
        // are still consumed in the original order: `buffered` (not
        // `buffer_unordered`) is what buys the concurrency without giving up the
        // ordering that batch construction depends on — a cell created and consumed
        // within the same round has to resolve against the earlier entry.
        //
        // It also bounds memory: at most `fetch_concurrency` transaction bodies are
        // held at once, regardless of how many the range contains.
        let mut fetched = stream::iter(records)
            .map(|record| async move {
                let result = self.ckb.get_transaction(&record.tx_hash).await;
                (record, result)
            })
            .buffered(self.config.ckb.fetch_concurrency);

        while let Some((record, result)) = fetched.next().await {
            let Some(with_status) = result? else {
                return Err(IndexerError::inconsistent(format!(
                    "rich indexer reported transaction {} which the node cannot return",
                    record.tx_hash
                )));
            };
            let Some(header) = headers.get(&record.block_number.0) else {
                return Err(IndexerError::inconsistent(format!(
                    "no header fetched for block {}",
                    record.block_number.0
                )));
            };
            let block = BlockContext {
                number: record.block_number.0,
                hash: header.hash,
                timestamp: extract::ckb_timestamp(header.timestamp.0),
                tx_index: record.tx_index.0,
            };
            self.process_transaction(&record, &block, with_status, &mut batch, &mut round)
                .await?;
        }

        // Header rows: every block that produced activity, plus the checkpoint. That
        // is the sparse ancestry a future reorg walk needs, without paying one RPC
        // per block during initial sync.
        for (number, header) in &headers {
            batch.blocks.push(BlockRecord {
                number: *number as i64,
                hash: header.hash.to_vec(),
                parent_hash: header.parent_hash.to_vec(),
                timestamp: extract::ckb_timestamp(header.timestamp.0),
                has_rgbpp_activity: true,
            });
        }
        // The checkpoint block itself, unless it already went in as an activity block.
        let checkpoint_header = match headers.get(&to) {
            Some(header) => header.clone(),
            None => self.header_at(to).await?,
        };
        if batch.blocks.iter().all(|b| b.number != to as i64) {
            batch.blocks.push(BlockRecord {
                number: to as i64,
                hash: checkpoint_header.hash.to_vec(),
                parent_hash: checkpoint_header.parent_hash.to_vec(),
                timestamp: extract::ckb_timestamp(checkpoint_header.timestamp.0),
                has_rgbpp_activity: false,
            });
        }

        batch.checkpoint_number = to as i64;
        batch.checkpoint_hash = Some(checkpoint_header.hash.to_vec());

        let stats = self.store.apply_batch(&batch).await?;
        round.cells = stats.cells;
        round.spends = stats.spends;

        // Header retention only needs to cover the depth a reorg could reach.
        let keep_from = to.saturating_sub(self.config.ckb.header_retention);
        if keep_from > 0 {
            let pruned = self.store.prune_blocks_below(keep_from as i64).await?;
            if pruned > 0 {
                debug!(pruned, keep_from, "pruned old ckb headers");
            }
        }

        Ok(round)
    }

    /// Merge the two protocol locks into one ordered transaction list.
    ///
    /// A single transaction can match both searches (a leap consumes RGB++ cells and
    /// creates BTC time lock cells), so matched cells are merged per transaction
    /// rather than processed twice.
    async fn collect_range(&self, from: u64, to: u64) -> Result<Vec<TxRecord>> {
        let mut merged: BTreeMap<(u64, u32, H256), TxRecord> = BTreeMap::new();

        for script in [
            lock_search_script(&self.config.protocol, LockKind::Rgbpp),
            lock_search_script(&self.config.protocol, LockKind::BtcTime),
        ] {
            let records = self
                .ckb
                .collect_transactions_in_range(script, from, to)
                .await?;
            for record in records {
                let key = (record.block_number.0, record.tx_index.0, record.tx_hash);
                match merged.get_mut(&key) {
                    Some(existing) => {
                        for cell in record.cells {
                            if !existing.cells.contains(&cell) {
                                existing.cells.push(cell);
                            }
                        }
                    }
                    None => {
                        merged.insert(key, record);
                    }
                }
            }
        }

        Ok(merged.into_values().collect())
    }

    async fn process_transaction(
        &self,
        record: &TxRecord,
        block: &BlockContext,
        with_status: TransactionWithStatus,
        batch: &mut IndexBatch,
        round: &mut ScanRound,
    ) -> Result<()> {
        let tx = with_status.transaction.ok_or_else(|| {
            IndexerError::inconsistent(format!(
                "node returned transaction {} without a body",
                record.tx_hash
            ))
        })?;

        let outputs: Vec<CellOutput> = tx
            .outputs
            .iter()
            .map(|o| CellOutput {
                capacity: o.capacity.0,
                lock: o.lock.clone(),
                type_: o.type_.clone(),
            })
            .collect();
        let outputs_data: Vec<Vec<u8>> = tx.outputs_data.iter().map(|d| d.0.clone()).collect();

        // Resolve the inputs the rich indexer flagged as RGB++ cells.
        let mut resolved_inputs = Vec::new();
        for matched in record.cells.iter().filter(|c| c.io_type == IoType::Input) {
            let Some(input) = tx.inputs.get(matched.io_index as usize) else {
                return Err(IndexerError::inconsistent(format!(
                    "transaction {} has no input at index {}",
                    record.tx_hash, matched.io_index
                )));
            };
            let out_point =
                CkbOutPoint::new(input.previous_output.tx_hash, input.previous_output.index.0);

            match self
                .resolve_input(out_point, matched.io_index, batch, round)
                .await?
            {
                Some(resolved) => resolved_inputs.push(resolved),
                None => warn!(
                    tx = %record.tx_hash,
                    input_index = matched.io_index,
                    consumed = %out_point,
                    "cannot resolve a consumed cell the rich indexer matched; skipping the spend"
                ),
            }
        }

        let extracted = match extract::extract(
            &self.config.protocol,
            &self.config.assets,
            &record.tx_hash,
            &outputs,
            &outputs_data,
            &resolved_inputs,
            block,
        ) {
            Ok(extracted) => extracted,
            Err(e) => {
                // One unreadable transaction must not stall the whole range.
                warn!(tx = %record.tx_hash, error = %e, "skipping transaction");
                return Ok(());
            }
        };

        batch.cells.extend(extracted.cells);
        batch.spends.extend(extracted.spends);
        batch.transitions.extend(extracted.transition);
        Ok(())
    }

    /// Find the consumed cell: from the database, from this batch, or from the node.
    async fn resolve_input(
        &self,
        out_point: CkbOutPoint,
        input_index: u32,
        batch: &mut IndexBatch,
        round: &mut ScanRound,
    ) -> Result<Option<ResolvedInput>> {
        let tx_hash_bytes = out_point.tx_hash.to_vec();

        // Created earlier in this very batch: it is not in the database yet.
        if let Some(pending) = batch
            .cells
            .iter()
            .find(|c| c.ckb_tx_hash == tx_hash_bytes && c.output_index == out_point.index as i32)
        {
            return Ok(Some(self.resolved_from_new_cell(
                pending,
                out_point,
                input_index,
            )?));
        }

        if let Some(row) = self
            .store
            .cell_by_out_point(&tx_hash_bytes, out_point.index as i32)
            .await?
        {
            return Ok(Some(ResolvedInput {
                input_index,
                out_point,
                binding: resolve::binding_from_row(&row)?,
                cell: resolve::cell_output_from_row(&self.config.protocol, &row)?,
                data: row.cell_data,
            }));
        }

        // The gap case: a cell created outside the indexed range. Fetch and backfill
        // it so the spend has something to attach to.
        self.backfill_input(out_point, input_index, batch, round)
            .await
    }

    fn resolved_from_new_cell(
        &self,
        cell: &NewCell,
        out_point: CkbOutPoint,
        input_index: u32,
    ) -> Result<ResolvedInput> {
        let script_id = match cell.lock_kind {
            LockKind::Rgbpp => self.config.protocol.rgbpp_lock,
            LockKind::BtcTime => self.config.protocol.btc_time_lock,
        };
        let lock = Script::new(
            script_id.code_hash,
            script_id.hash_type,
            cell.lock_args.clone(),
        );
        let type_ = cell
            .type_script
            .as_ref()
            .map(resolve::script_from_json)
            .transpose()?;
        Ok(ResolvedInput {
            input_index,
            out_point,
            binding: LockBinding::parse(cell.lock_kind, &cell.lock_args)?,
            cell: CellOutput {
                capacity: cell.capacity as u64,
                lock,
                type_,
            },
            data: cell.cell_data.clone(),
        })
    }

    async fn backfill_input(
        &self,
        out_point: CkbOutPoint,
        input_index: u32,
        batch: &mut IndexBatch,
        round: &mut ScanRound,
    ) -> Result<Option<ResolvedInput>> {
        let Some(with_status) = self.ckb.get_transaction(&out_point.tx_hash).await? else {
            return Ok(None);
        };
        let Some(tx) = with_status.transaction else {
            return Ok(None);
        };
        let Some(output) = tx.outputs.get(out_point.index as usize) else {
            return Ok(None);
        };

        let cell = CellOutput {
            capacity: output.capacity.0,
            lock: output.lock.clone(),
            type_: output.type_.clone(),
        };
        let data = tx
            .outputs_data
            .get(out_point.index as usize)
            .map(|d| d.0.clone())
            .unwrap_or_default();

        let Some(kind) = self.config.protocol.classify(&cell.lock) else {
            return Ok(None);
        };
        let binding = LockBinding::parse(kind, cell.lock.args.as_slice())?;

        // The creating block is where this cell's rollback anchor has to point.
        let created_number = with_status
            .tx_status
            .block_number
            .map(|n| n.0 as i64)
            .unwrap_or_default();
        let created_hash = with_status
            .tx_status
            .block_hash
            .map(|h| h.to_vec())
            .unwrap_or_default();

        let rgbpp_output = extract::RgbppOutput {
            index: out_point.index,
            binding: binding.clone(),
            cell: cell.clone(),
            data: data.clone(),
        };
        let mut new_cell = extract::build_new_cell(
            &self.config.assets,
            &out_point.tx_hash,
            &rgbpp_output,
            &BlockContext {
                number: created_number.max(0) as u64,
                hash: with_status.tx_status.block_hash.unwrap_or(H256::ZERO),
                timestamp: chrono::Utc::now(),
                tx_index: 0,
            },
        )?;
        new_cell.created_block_hash = created_hash;
        new_cell.created_tx_index = UNKNOWN_TX_INDEX;

        // Expected while `start_block` sits above the first RGB++ block; the count
        // surfaces in the periodic sync line, so this stays at debug.
        debug!(
            consumed = %out_point,
            created_block = created_number,
            "backfilled a cell created outside the indexed range"
        );
        batch.cells.push(new_cell);
        round.backfilled_cells += 1;

        Ok(Some(ResolvedInput {
            input_index,
            out_point,
            binding,
            cell,
            data,
        }))
    }

    /// Fetch the header of every block that produced a match, concurrently.
    ///
    /// Deduplicated first: a block with twenty RGB++ transactions still costs one
    /// header.
    async fn fetch_headers(&self, records: &[TxRecord]) -> Result<BTreeMap<u64, RpcHeader>> {
        let numbers: BTreeSet<u64> = records.iter().map(|r| r.block_number.0).collect();

        let fetched: Vec<Result<(u64, RpcHeader)>> = stream::iter(numbers)
            .map(|number| async move { Ok((number, self.header_at(number).await?)) })
            .buffer_unordered(self.config.ckb.fetch_concurrency)
            .collect()
            .await;

        fetched.into_iter().collect()
    }

    async fn header_at(&self, number: u64) -> Result<RpcHeader> {
        self.ckb
            .get_header_by_number(number)
            .await?
            .ok_or_else(|| IndexerError::inconsistent(format!("node has no header at {number}")))
    }

    /// Confirm the chain still agrees with our checkpoint before extending it.
    ///
    /// This version does not implement reorg handling; `REORG_LAG` is the defence.
    /// This check is the alarm that says the defence was not enough — the chain
    /// reorganised deeper than the lag, or the node was repointed at a different
    /// chain. There is no recovery path here on purpose: the scanner refuses to
    /// advance rather than write facts on top of a fork, and an operator decides what
    /// to do. See `docs/reorg.md` for the rollback design this leaves room for.
    async fn assert_checkpoint_still_canonical(
        &self,
        state: &rgbpp_store::models::StreamState,
    ) -> Result<()> {
        let (Some(stored_hash), true) =
            (state.last_block_hash.as_ref(), state.last_block_number > 0)
        else {
            return Ok(());
        };

        let number = state.last_block_number as u64;
        let Some(header) = self.ckb.get_header_by_number(number).await? else {
            return Err(IndexerError::inconsistent(format!(
                "node has no header at our checkpoint {number}"
            )));
        };
        if header.hash.to_vec() == *stored_hash {
            return Ok(());
        }

        let stored = hex::encode(stored_hash);
        let actual = header.hash.to_hex();
        error!(
            number,
            %stored,
            %actual,
            lag = self.config.ckb.reorg_lag,
            "REORG BELOW CHECKPOINT — indexing halted; \
             the chain reorganised deeper than reorg_lag and this version cannot roll back"
        );
        Err(IndexerError::Reorg {
            number,
            stored,
            actual,
        })
    }
}

/// The "match any args" form of a protocol lock, used as a search prefix.
pub fn lock_search_script(protocol: &ProtocolScripts, kind: LockKind) -> Script {
    let id = match kind {
        LockKind::Rgbpp => protocol.rgbpp_lock,
        LockKind::BtcTime => protocol.btc_time_lock,
    };
    Script::new(id.code_hash, id.hash_type, Vec::new())
}
