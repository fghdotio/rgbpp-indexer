# Data model

The schema lives in [`migrations/0001_init.sql`](../migrations/0001_init.sql). This
page explains the shape of it.

## Facts, observations, and nothing else

Tables fall into exactly two groups, and which group a table is in determines how it
behaves under a reorg.

### CKB facts — ordered, dependency-bearing

`rgbpp_cells`, `rgbpp_transitions`, `ckb_blocks`.

Every row is anchored to the block that produced it. `rgbpp_cells` has two anchors,
because a cell has two events:

```sql
created_block_number,  created_block_hash,  created_tx_index
consumed_block_number, consumed_block_hash, consumed_tx_hash, consumed_tx_index, consumed_input_index
```

`consumed_block_number IS NULL` means live on CKB. This version implements no reorg
handling, but that pair of anchors is what would make a rollback expressible directly
(see [reorg.md](reorg.md)):

```sql
UPDATE rgbpp_cells SET consumed_* = NULL WHERE consumed_block_number > $ancestor;
DELETE FROM rgbpp_cells               WHERE created_block_number  > $ancestor;
DELETE FROM rgbpp_transitions         WHERE block_number          > $ancestor;
DELETE FROM ckb_blocks                WHERE number                > $ancestor;
```

Order matters: reverting spends before deleting cells brings back a cell created below
the ancestor but consumed above it, while a cell created above it disappears entirely.

`ckb_blocks` is deliberately **sparse** — every block that produced RGB++ activity,
plus each checkpoint. Only the checkpoint row is read today, by the sanity check that
refuses to extend a checkpoint the node no longer agrees with; the rest is retained
because an ancestor search needs it and rebuilding it later would mean one RPC per
block.

### Bitcoin observations — re-queryable, cacheable

`btc_outpoints`, `btc_txs`.

These carry `observed_at` and `invalidated_at` instead of block anchors, because every
row answers a question that can simply be asked again.

`invalidated_at` is reserved: nothing sets it, but **readers already honour it** — the
status view only treats a spend as real when it is `NULL`. That is what would let a
Bitcoin reorg degrade to marking rows stale rather than rewriting history, and it is
also why the existing freshness machinery (the daily sweep, the on-demand paths)
already corrects such a reorg eventually, without any reorg-specific code.

`btc_outpoints.address` is learned opportunistically and only for outpoints that
actually carry an RGB++ cell. A wallet address may have thousands of UTXOs and almost
no bindings; recording all of them would make the table grow with wallet activity
instead of protocol activity.

## Derived, never stored

`rgbpp_cell_status` is a plain view joining the two sides:

```sql
CASE
  WHEN consumed_block_number IS NOT NULL                      THEN 'spent'
  WHEN btc says spent AND the observation is not invalidated  THEN 'pending_ckb'
  ELSE 'live'
END
```

Balances (`asset_balances_for_outpoints`) and counts (`counts`) are aggregates
computed at read time. Nothing is incremented and stored, so a rollback never has to
reverse-correct a total.

The sweeper's work list is derived the same way — `SELECT DISTINCT` over live
bindings, rather than a separate watch table that could drift out of sync with the
cells it is supposed to track.

## Work queues and findings

- `btc_refresh_queue` — outpoints to re-check. Priority-ordered (`ON_DEMAND` < 
  `PENDING_FOLLOW_UP` < `SWEEP`), claimed with `FOR UPDATE SKIP LOCKED`, exponential
  backoff applied *at claim time* so a worker that dies mid-request does not leave an
  item spinning. Centralising the drain keeps an API traffic burst from becoming a
  burst at the Bitcoin data source.
- `rgbpp_anomalies` — deduplicated on a caller-supplied `dedup_key`, so a daily sweep
  re-detecting the same problem updates one row. A finding that reappears after being
  resolved is reopened rather than silently ignored.
- `sweep_runs` — audit trail for the daily sweep.
- `ckb_reorgs`, `btc_blocks` — **reserved**. Nothing writes to them in this version;
  they exist so that adding reorg handling is a code change rather than a migration on
  a live database.

## Atomicity

One scan round is one database transaction: headers, cells, spends, transitions and
the checkpoint all move together, and the checkpoint moves **last**. A crash mid-round
replays the round rather than skipping it.

Spends are applied after cells are inserted, so a cell created and consumed within the
same round resolves correctly. `apply_spend` returns the number of rows it updated;
`0` means a cell was consumed that the indexer never recorded as created, which the
scanner treats as a gap to backfill from the node rather than as an error.
