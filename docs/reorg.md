# Reorgs

**This version does not implement reorg handling.** It indexes to `tip - REORG_LAG`
and treats everything below that line as settled. This page records why, what is
already in place, and the design the schema is shaped for — so that adding it later is
a code change rather than a migration on a live database.

## What this version does

```toml
[ckb]
reorg_lag = 24
```

The scanner never reads above `tip - reorg_lag`. A reorg shallower than the lag is
invisible: the affected blocks were never indexed, so there is nothing to undo.

The cost is a blind spot near the tip, and it is a real one — a confirmed CKB
transaction stays unqueryable for `reorg_lag` blocks. That gap is covered by the
Bitcoin-driven on-demand path rather than by indexing closer to the tip: an
application polling a transaction gets an answer from
`Reconciler::resolve_transition`, which asks the rich indexer directly and reports
`ckb_seen_above_lag`. See the README.

There is one safety check. Before extending the checkpoint, the scanner confirms the
node still reports the same hash at that height. If it does not, the lag assumption
was violated — a deeper reorg, or a node repointed at a different chain — and the
scanner **halts with an error rather than recovering**, because writing more facts on
top of a fork is the one outcome worth avoiding. An operator decides what to do.

## Why the two sides need different answers

The asymmetry is the whole design, and it is not about which chain is more reliable.

**CKB carries ordered facts with dependencies between them.** "Cell X was created in
block N" and "cell X was consumed in block M" are claims about a sequence. Drop block
M and the second claim must be withdrawn while the first survives. Nothing can
re-derive that from scratch cheaply, so it has to be undone precisely. That is a
rollback.

**Bitcoin carries observations of re-queryable questions.** "Is outpoint T:2 spent?"
has an answer that can always be asked again. A reorg does not make the stored answer
*wrong to have recorded* — it makes it *stale*, which is a state the system already
models. So there is nothing to undo, only something to expire.

This is why the two sides get different column shapes: block anchors on one,
timestamps on the other.

## CKB side: the rollback design

Every CKB-derived row already carries its anchor. `rgbpp_cells` carries two, because a
cell has two events:

```sql
created_block_number,  created_block_hash,  created_tx_index
consumed_block_number, consumed_block_hash, consumed_tx_hash, ...
```

Given a common ancestor height, the rollback is four statements:

```sql
UPDATE rgbpp_cells
   SET consumed_block_number = NULL, consumed_block_hash = NULL,
       consumed_tx_hash = NULL, consumed_tx_index = NULL,
       consumed_input_index = NULL, consumed_at = NULL
 WHERE consumed_block_number > $ancestor;

DELETE FROM rgbpp_cells       WHERE created_block_number > $ancestor;
DELETE FROM rgbpp_transitions WHERE block_number         > $ancestor;
DELETE FROM ckb_blocks        WHERE number               > $ancestor;
```

Three things make that correct, and all three are already true today:

1. **Order matters.** Reverting spends *before* deleting cells brings a cell created
   below the ancestor but consumed above it back to life, while a cell created above
   it disappears entirely. The reverse order would resurrect nothing and delete too
   little.

2. **No aggregate is stored.** Balances and counts are `SELECT`s over these tables, so
   deleting rows is the entire correction. Had the indexer maintained running totals,
   each would need its own compensating update — and a total that drifts is far harder
   to notice than a query that is a little slower.

3. **A round is already one transaction.** Headers, cells, spends, transitions and the
   checkpoint move together, so there is no half-applied state for a rollback to trip
   over.

### Finding the common ancestor

`ckb_blocks` holds a **sparse** history: every block that produced RGB++ activity,
plus each checkpoint. Walk it downwards, comparing each stored hash against the node,
and the first agreement is the ancestor. Sparse is deliberate — storing every header
would cost one RPC per block during initial sync, and the blocks between activity
blocks contain nothing to roll back anyway.

If no stored header agrees within `ckb.header_retention`, the only safe answer is the
configured start block: anything higher would keep some forked state.

### What would need building

- Ancestor search (walk `ckb_blocks` against the node).
- The rollback transaction above, plus resetting `indexer_state` to the ancestor.
- Writing `ckb_reorgs` — the audit table already exists and nothing writes to it.
- A decision on the tip: with the lag reduced, a rollback becomes routine rather than
  an alarm, so the scanner should recover and continue instead of halting.

## Bitcoin side: expiry, not rollback

A Bitcoin reorg can change two things the indexer holds: whether an outpoint is spent,
and at what height. Both are observations.

`btc_outpoints.invalidated_at` is the expiry marker. **Readers already honour it** —
the `rgbpp_cell_status` view only treats a spend as real when `invalidated_at IS
NULL`, so an expired observation degrades a cell from `pending_ckb` back to `live`,
which is exactly right: we no longer know that it moved. Nothing sets the column yet.

Detection would be a rolling hash check over the last N heights, with `btc_blocks` as
the reference (reserved, unused today). On a disagreement at height `h`:

```sql
UPDATE btc_outpoints SET invalidated_at = now()
 WHERE invalidated_at IS NULL
   AND (spent_height >= $h OR status = 'spent_unconfirmed');
```

Unconfirmed spends are included because a reorg reshuffles the mempool.

Note what is *not* needed: no ordering, no ancestor search, no compensating updates.
Marking rows stale is unconditionally safe, because the worst case is asking the data
source a question it can answer.

### It partly self-heals already

Because expiry is the mechanism, the existing freshness machinery covers a good deal
of it without any reorg-specific code. The daily sweep re-observes every live binding,
and the on-demand paths re-observe whatever an application asks about. A Bitcoin reorg
that changes a bound outpoint's status is therefore corrected at the next sweep or the
next query — just not immediately. The dedicated detector would close the gap between
"eventually" and "promptly", which is a latency improvement rather than a correctness
one.

That asymmetry — the Bitcoin side needing only a latency fix while the CKB side needs
real machinery — is the practical payoff of keeping observations and facts apart.
