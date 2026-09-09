# The indexing engine

How the workers in `crates/indexer` fit together, and the reasoning behind the parts
that are not obvious from the code.

## Scanner — CKB is the discovery entry point

Every RGB++ state transition ends in a CKB transaction touching an RGB++ lock, so
scanning two script prefixes through the rich indexer's `get_transactions` finds all
of them; `io_type` on each match says whether a cell was created or consumed.

The scanner stops at `tip - REORG_LAG`. Everything below that line is treated as
settled, which is what lets this version skip rollback entirely — see
[reorg.md](reorg.md).

Three things about a scan round:

- **A round is one database transaction.** Headers, cells, spends, transitions and the
  checkpoint move together, and the checkpoint moves *last*, so a crash replays the
  round rather than skipping it.
- **Fetching is concurrent, processing is ordered.** The rich indexer matches
  transactions without returning their bodies, so each match costs a second
  `get_transaction`. Those go out through `buffered` (not `buffer_unordered`):
  concurrency without giving up the ordering batch construction depends on, since a
  cell created and consumed in the same round must resolve against the earlier entry.
  It also bounds memory to `fetch_concurrency` bodies regardless of range size.
- **It never calls Bitcoin.** A round is pure CKB RPC plus one database transaction.
  Blocking CKB indexing on Bitcoin availability would let a data-source outage stall
  the chain that is actually the source of truth here.

`apply_spend` returns the number of rows it updated. Zero means a cell was consumed
that was never recorded as created — a gap the scanner backfills from the node rather
than treating as an error. Backfilled cells record `created_tx_index = -1`, because
`get_transaction` does not report a transaction's position in its block.

## Reconciler — closing the blind spot

The scanner cannot see a transition until the CKB transaction is committed *and*
`REORG_LAG` blocks deep. A Bitcoin transaction can be broadcast, and confirmed, well
before either. Nothing on the CKB side will announce that, so applications point us at
it:

- **Address diff** — the address's live UTXO set from the data source, diffed against
  the outpoints the indexer still believes are live. Anything that disappeared has
  moved; anything we recorded as spent but the source still lists is a replacement or
  a Bitcoin reorg.
- **Point refresh** — re-observe exactly the outpoints one transaction touches, and
  look *past* the lag by asking the rich indexer directly for cells bound to that
  transaction's outputs.

Absence from a UTXO listing is only ever a *hint about where to look*. Status always
comes from asking the data source about the outpoint itself — otherwise a lagging or
partially-synced backend could mass-invalidate live cells.

Neither path writes CKB facts. That is what keeps the two durability models from
leaking into each other.

## Binding address backfill — ownership from the funding transaction

A cell bound to `(txid, vout)` belongs to whoever controls that output, so the funding
transaction is the authoritative source of ownership.

The alternative — reading an address's live UTXO listing — can only label bindings
that are *still unspent when someone asks*. Spending a binding removes it from that
listing forever, so a transfer the indexer did not happen to observe beforehand becomes
permanently unattributable, and address history ends up depending on when a user last
opened their wallet rather than on what the chain says. Funding transactions do not
move: a binding spent long ago resolves exactly as well as a fresh one.

Most bindings are labelled for free, because the reconciler already fetches the
transactions that create them. The worker exists for the remainder — historical
bindings indexed before any Bitcoin lookup touched them. It walks *funding
transactions*, not bindings, since one RGB++ transaction typically funds several
across its outputs.

Address activity (`/v1/rgbpp/activity/by-btc-address`) is only as complete as this
backfill; `/status` reports `unlabelled` so an incomplete run is visible rather than
showing up as a thin history.

## Sweeper — the case nobody polls for

Incremental work is driven by confirmed transitions and by what applications ask
about. Neither covers a bound UTXO spent by a wallet that had no idea it was carrying
an RGB++ asset: nobody polls for that transaction, and no CKB transaction will ever
reference it.

So once a day every live binding is re-observed, and anything spent on Bitcoin with no
matching CKB transition — after a grace period measured in Bitcoin confirmations, the
unit the risk is actually in — is recorded in `rgbpp_anomalies`.

Detection is skipped while the CKB index is far behind its target: "no transition
exists" and "not indexed yet" are indistinguishable from the Bitcoin side, and running
anyway would flag every historical binding.

## Verifier — commitments are a label, not a gate

Discovery never depends on the commitment check. The indexer records what both chains
say and compares them; a mismatch produces an anomaly, never a dropped fact. An
indexer that silently discarded on-chain state because its own commitment computation
disagreed would be worse than one that reports the disagreement.

Issuance is exempt — see [commitments.md](commitments.md).

## Progress reporting

During initial sync the scanner finishes a round every few hundred milliseconds.
Logging each one buries the two numbers anyone wants — how fast, and how long left —
under thousands of lines. So rounds are accumulated and summarised on an interval, and
per-round detail stays at `debug`.

`rate` is the current window, so it responds to what is happening now. `eta` uses the
session average, because a per-window ETA swings between hours and days with RGB++
density and stops being worth reading.
