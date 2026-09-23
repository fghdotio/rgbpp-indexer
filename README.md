# rgbpp-indexer

An RGB++ indexer built on two existing data layers: a **CKB rich indexer** for
discovery and a **Bitcoin data source** (Esplora/electrs today, Blockbook behind the
same interface) for UTXO observation.

Written in Rust. Requires PostgreSQL.

---

## The core idea

RGB++ state lives on two chains with very different properties, and the mistake to
avoid is treating them the same way.

|  | CKB side | Bitcoin side |
|---|---|---|
| What it holds | ordered state facts with dependencies | answers to re-queryable questions |
| Example | "cell X was consumed in block N by tx Y" | "outpoint T:2 is spent by tx Z" |
| Anchored to | block number + block hash | a timestamp |
| Table | `rgbpp_cells`, `rgbpp_transitions`, `ckb_blocks` | `btc_outpoints`, `btc_txs` |
| Reorg answer *(future)* | real rollback: revert and delete above the ancestor | cache expiry: mark stale, ask again |

Nothing on the Bitcoin side is a fact this indexer owns, so a Bitcoin reorg would never
need reverse accounting — every affected row can simply be re-observed. The CKB side
does own ordered facts, so every CKB-derived row carries block anchors.

**No aggregate is ever stored.** Balances and counts are `SELECT`s over the fact
tables (`crates/store/src/stats.rs`), and cell status is a plain — never materialised
— view (`rgbpp_cell_status`). A stored counter is precisely the thing a reorg would
force you to correct in reverse, and a counter that drifts is much harder to notice
than a query that is a little slower.

## Discovery: CKB is the entry point

Every RGB++ state transition ends in a CKB transaction that touches an RGB++ lock. So
the scanner asks the rich indexer's [`get_transactions`][gt] for two script prefixes —
the RGB++ lock and the BTC time lock — with `group_by_transaction: true`. Each match
carries `io_type`, which says whether the transaction *created* a cell or *consumed*
one. That is the whole discovery mechanism.

The Bitcoin side is then queried by RGB++ binding: the lock args of every indexed cell
name the exact UTXO that owns it.

[gt]: https://github.com/nervosnetwork/ckb/blob/develop/rpc/README.md#method-get_transactions-1

## Reorgs: not in this version

This version indexes to `tip - REORG_LAG` (default 24 blocks) instead of to the tip.
Everything below that line is treated as settled, so a reorg shallower than the lag is
invisible — the affected blocks were never indexed and there is nothing to undo. **No
reorg handling is implemented.**

The schema is nevertheless laid out for it, because retrofitting block anchors onto
existing rows is the part that would actually hurt. Columns and tables marked
`RESERVED` in the migration are written by nothing today.

One safety check exists: before extending its checkpoint the scanner confirms the node
still reports the same hash at that height. If not, the lag assumption was violated,
and the scanner **halts loudly rather than recovering** — writing more facts on top of
a fork is the outcome worth avoiding. [`docs/reorg.md`](docs/reorg.md) records the
full design, including the four statements a CKB rollback comes down to and why the
Bitcoin side needs none.

## The gap the lag creates

The lag creates a deliberate blind spot, and there is a second, larger one: a Bitcoin
transaction can be broadcast, and even confirmed, before the matching CKB transaction
is committed at all. **Nothing on the CKB side can announce either.** Three mechanisms
cover them, and none of them writes CKB facts:

1. **Address diff** — `GET /v1/rgbpp/assets/by-btc-address/{address}` fetches the
   address's live UTXO set and diffs it against the outpoints the indexer still
   believes are live. Anything that has disappeared moved on Bitcoin, and gets
   re-observed on the spot. The reverse case — we recorded a spend the data source
   still lists as unspent, i.e. a replacement or a Bitcoin reorg — is caught too.
2. **Point refresh** — `GET /v1/rgbpp/transactions/{btc_txid}` re-observes exactly the
   outpoints that transaction touches, and looks *past* the indexed range by asking
   the rich indexer directly for live cells bound to the transaction's outputs.
3. **Daily sweep** — everything nobody asks about. See below.

Absence from a UTXO listing is only ever treated as a *hint about where to look*;
status always comes from asking the data source about the outpoint itself. A lagging
or partially-synced backend must not be able to mass-invalidate live cells.

`/status` reports the lag explicitly, so a client can tell "not there" from "not there
yet".

## The daily sweep

Incremental work is driven by confirmed transitions and by what applications ask
about. Neither covers the case the sweep exists for: **a bound UTXO spent by a wallet
that had no idea it was carrying an RGB++ asset.** Nobody polls for that transaction,
and no CKB transaction will ever reference it.

So once a day every live binding is re-observed, and anything spent on Bitcoin with no
matching CKB transition — after a grace period in Bitcoin confirmations — is recorded
in `rgbpp_anomalies` for a human. Detection is skipped while the CKB index is far
behind its target, because "no transition exists" and "not indexed yet" are
indistinguishable from the Bitcoin side.

## Swapping the Bitcoin data source

Everything the indexer needs from Bitcoin is
[`BtcDataSource`](crates/btc/src/source.rs). Changing backend is two config lines:

```toml
[btc]
source = "esplora"      # mempool.space, blockstream/electrs, self-hosted electrs
base_url = "https://mempool.space/api"

# source = "blockbook"  # Trezor Blockbook
# base_url = "https://btc1.trezor.io"
```

One asymmetry is worth knowing: Esplora answers "is this outpoint spent, and by whom"
in one call (`/tx/:txid/outspend/:vout`). Blockbook reports a `spent` flag but not the
spender, so recovering it means walking the owning address's history —
`tx_outspends` resolves a whole transaction's outputs in one pass and is strongly
preferred there. A known-spent outpoint whose spender cannot be resolved stays
`unknown` rather than being reported `unspent`, so a cell that has already moved is
never resurrected.

## Logging

During initial sync the scanner finishes a round every few hundred milliseconds.
Logging each one buries the two numbers anyone actually wants — how fast is it going,
and when will it be done — under thousands of lines that each say almost nothing. So
rounds are **accumulated and summarised on an interval**; per-round detail lives at
`debug`.

```
INFO rgbpp_indexer::engine: engine ready network=testnet ckb=https://testnet.ckb.dev/rpc
     btc=esplora:https://mempool.space/testnet/api start_block=13075000 reorg_lag=24
     batch_blocks=500 rgbpp_lock=0x61ca…3248 btc_time_lock=0x00cd…9326 assets=xudt:1 sudt:1
     spore:0 reconcile=true sweep=true verify_commitments=true
INFO rgbpp_indexer::engine: workers started count=5 enabled=scanner,refresh-queue,heartbeat,sweeper,verifier
INFO rgbpp_indexer::progress: ckb sync range=13075000..13076999 blk=2000 rate=131/s tx=18
     cells=29 spends=14 backfilled=10 behind=9055529 tip=22132552 eta=19h10m pct=0.02
INFO rgbpp_indexer::heartbeat: status indexed=13080999 target=22132469 tip=22132493
     behind=9051494 lag=24 cells=46 live=25 pending=0 transitions=25 queue=0 anomalies=0
INFO rgbpp_indexer::verify: commitments checked=39 matched=30 mismatched=0 missing=0
     btc_unknown=0 skipped=9
```

(Wrapped here for the page; each is one line.)

- The **startup line** carries everything needed to answer "why is this indexing
  nothing" — endpoints, start block, lag, and the two protocol code hashes.
- **`ckb sync`** is one line per `log.progress_interval_secs`. `rate` is that window,
  so it responds to what is happening now; `eta` is derived from the session average,
  because a per-window ETA swings between hours and days with RGB++ density and stops
  being worth reading.
- **`status`** is the slow heartbeat: it exists because the sync line goes quiet once
  caught up, and silence is indistinguishable from a wedged process.
- Everything else logs **only when something changed**. A `btc refresh` line appears
  when an observation actually moved, not on every poll; an address diff that found
  nothing stays at `debug`.

Fields are structured, so `LOG_FORMAT=json` gives the same content as JSON without a
second copy of the numbers in the message. `RUST_LOG` overrides the default filter,
which quietens dependency chatter so the indexer's own lines survive at `info`.

---

## Quick start

Everything runs through one script, which is a thin wrapper over `docker compose` that
makes the distinction compose makes awkward: stopping versus deleting your data.

```bash
./scripts/rgbpp.sh up          # build the image, start Postgres and the indexer
./scripts/rgbpp.sh status      # GET /status
./scripts/rgbpp.sh logs -f     # follow the indexer
```

| Command | |
|---|---|
| `up` | start Postgres and the indexer |
| `db` | Postgres only, for `cargo run` from the host |
| `down` | stop everything, keep the data |
| `restart` | restart the indexer |
| `build` / `rebuild` | build the image, cached / `--no-cache` then recreate |
| `logs [-f\|N]` · `logs-db` | container logs |
| `status` · `ps` · `psql [sql]` · `sh` | inspect |
| `exec <args>` | run the indexer binary with any arguments |
| `migrate` · `test-db` | apply migrations · run the schema tests |
| `reset-db` | drop and recreate the schema — **destructive** |
| `destroy` | stop and delete the data volume — **destructive** |
| `clean` | `destroy`, plus remove the built image — **destructive** |

Destructive commands prompt first; pass `-y` or set `FORCE=1` to skip.

Settings come from `.env` (see [`.env.example`](.env.example)); the config file is
mounted read-only, so changing it is an edit plus `restart`, not a rebuild.

For host-side development:

```bash
./scripts/rgbpp.sh db
cargo run --release -- --config config/testnet.toml run
```

⚠ **Verify the `[protocol]` section before running.** Deployed code hashes are a
property of the deployment you intend to index, not of this software, so they are
required configuration rather than compiled-in constants. An indexer pointed at the
wrong code hash starts cleanly, reports healthy, and indexes nothing. The values in
`config/mainnet.toml` and `config/testnet.toml` are a starting point to cross-check
against the RGB++ deployment your application stack uses.

### Commands

```bash
rgbpp-indexer check-config    # resolved config, including env overrides
rgbpp-indexer migrate         # apply migrations and exit
rgbpp-indexer run             # scanner + workers + API
rgbpp-indexer scan-once       # one CKB scan round
rgbpp-indexer sweep-once      # one full sweep
rgbpp-indexer status          # indexer state as JSON
rgbpp-indexer refresh <txid:vout>...
```

### Configuration

TOML plus a fixed set of environment overrides, so one image can serve several
deployments. The environment carries **deployment identity** — `DATABASE_URL`,
`CKB_RPC_URL`, `CKB_INDEXER_RPC_URL`, `CKB_START_BLOCK`, `REORG_LAG`, `BTC_SOURCE`,
`BTC_BASE_URL`, `API_BIND` — plus the few **throughput** settings that differ by an
order of magnitude between a public endpoint and self-hosted infrastructure:
`CKB_PAGE_LIMIT`, `CKB_BATCH_BLOCKS`, `CKB_FETCH_CONCURRENCY`,
`CKB_POLL_INTERVAL_SECS`, `BTC_MAX_CONCURRENCY`, `BTC_MIN_REQUEST_INTERVAL_MS`,
`DB_MAX_CONNECTIONS`.

Asset code hashes are the same kind of value, with a quieter failure. A cell whose
type script matches nothing in `[[assets.*]]` is indexed `asset_kind = "unknown"` —
the index is complete, but the asset counts on `/status` and `/v1/rgbpp/assets` read
as though those assets do not exist. Classification happens once, at index time, and
re-scanning does not revisit it: the cell insert's `ON CONFLICT` clause refreshes only
the `created_*` columns. [`scripts/reclassify-assets.sql`](scripts/reclassify-assets.sql)
corrects existing rows in place, which is the alternative to a full resync.

Policy settings (the sweep, verify and log sections) are TOML-only: they are meant to
be reviewed as a set, and a value with two sources drifts. `.env.example` is the
complete list of what the binary reads — anything absent from it does nothing.

Self-hosting both backends, the defaults to change are:

```bash
CKB_PAGE_LIMIT=1000 CKB_BATCH_BLOCKS=2000 \
BTC_MIN_REQUEST_INTERVAL_MS=0 BTC_MAX_CONCURRENCY=64 \
DB_MAX_CONNECTIONS=32 ./scripts/rgbpp.sh up
```

The knobs that matter most:

| Key | Default | Why you would change it |
|---|---|---|
| `ckb.start_block` | — | Where RGB++ went live. Scanning earlier is wasted work; setting it too late is handled by backfill, but slowly. |
| `ckb.reorg_lag` | 24 | How far behind the tip to stay. Larger is safer and blinder. |
| `ckb.batch_blocks` | 500 | Block span per scan round. |
| `ckb.fetch_concurrency` | 8 | Concurrent `get_transaction` per round. The rich indexer matches transactions without their bodies, so each match costs a second call; raising this is the main lever on catch-up speed. |
| `log.progress_interval_secs` | 15 | How often to summarise sync progress. |
| `log.heartbeat_interval_secs` | 300 | Operational one-liner cadence; `0` disables. |
| `btc.observation_ttl_secs` | 60 | Below this age an observation is reused instead of re-queried. |
| `btc.min_request_interval_ms` | 50 | Politeness for public endpoints. **Set to 0 when self-hosting** — it is a global ceiling, so 50ms caps you at 20 req/s no matter what `max_concurrency` says. |
| `btc.max_concurrency` | 8 | Concurrent Bitcoin requests. |
| `verify.interval_secs` | 30 | Seconds between commitment-verification passes. |
| `sweep.misspend_grace_confirmations` | 6 | Bitcoin confirmations before flagging a spend as unmatched. |
| `verify.commitments` | true | Cross-check commitments; costs Bitcoin requests. |

## API

| Endpoint | Notes |
|---|---|
| `GET /health` | liveness |
| `GET /status` | indexed height, chain tip, lag, derived counts, last sweep |
| `GET /openapi.json` | OpenAPI 3.1 spec for everything below; also committed as [`docs/openapi.json`](docs/openapi.json) |
| `GET /v1/rgbpp/assets` | every distinct asset by type hash; `?kind=udt\|dob\|unknown\|all`, `?limit=`, `?offset=` |
| `GET /v1/rgbpp/assets/by-btc-address/{address}` | **reconciles first**; `?reconcile=false` to skip |
| `GET /v1/rgbpp/balance/by-btc-address/{address}` | derived on read; `?include_pending=true` counts in-flight cells |
| `GET /v1/rgbpp/transactions/{btc_txid}` | cross-chain status + point refresh |
| `GET /v1/rgbpp/cells/by-btc-utxo/{txid}/{vout}` | `?refresh=true`, `?include_spent=true` |
| `GET /v1/rgbpp/cells/by-btc-txid/{txid}` | every cell bound to any output |
| `GET /v1/rgbpp/cells/by-ckb-out-point/{tx_hash}/{index}` | |
| `GET /v1/rgbpp/activity/by-btc-address/{address}` | RGB++ history, newest first; keyset `?cursor=`, `?limit=` |
| `GET /v1/rgbpp/transitions` · `/{ckb_tx_hash}` | |
| `POST /v1/rgbpp/refresh` | `{"outpoints": ["txid:vout"], "synchronous": true}` |
| `GET /v1/anomalies` | `?kind=`, `?include_resolved=true` |

`/v1/rgbpp/assets` deliberately carries no `symbol` and no `decimals`. An asset's
identity here is its type script hash; the cells that publish token metadata live
under other locks and are not indexed, so a name would have to be invented. A client
that needs one resolves it itself.

Amounts and capacities are decimal **strings** — a `u128` UDT amount does not survive
a JSON number. CKB hashes are `0x`-prefixed; Bitcoin txids are bare hex, matching what
each chain's tooling expects.

Cell status is derived from both chains:

| Status | Meaning |
|---|---|
| `live` | unconsumed on CKB, bound UTXO unspent (or unobserved) |
| `pending_ckb` | bound UTXO spent on Bitcoin; the CKB transition is not in the indexed range — uncommitted, or inside `REORG_LAG` |
| `spent` | consumed by an indexed CKB transaction |

## Layout

```
crates/types      domain types, protocol decoding, molecule, commitment, config
crates/ckb        CKB node + rich indexer JSON-RPC client
crates/btc        BtcDataSource trait, Esplora and Blockbook implementations
crates/store      PostgreSQL: schema, queries, rollback, derived aggregates
crates/indexer    scanner, reconciler, sweeper, verifier, progress reporting
crates/api        axum HTTP API
crates/node       the binary
migrations/       schema
```

## Tests

```bash
cargo test                                     # unit + commitment vectors
TEST_DATABASE_URL=postgres://rgbpp:rgbpp@localhost:5432/rgbpp cargo test -p rgbpp-store
```

`docs/openapi.json` is generated from the handler annotations and checked by
`committed_spec_is_current`, so an API change fails the build until the spec is
regenerated — and shows up in review as a spec diff:

```bash
UPDATE_OPENAPI=1 cargo test -p rgbpp-api committed_spec
```

The store writes runtime SQL so that building never needs a database, which means the
queries are only validated by the schema tests. Each gets its own PostgreSQL schema,
so they run in parallel; without `TEST_DATABASE_URL` they skip.

`crates/indexer/tests/commitment_vectors.rs` pins commitment computation to two real
CKB testnet transactions and the commitments their Bitcoin counterparts actually
published. They exist because the pre-image encoding has several places where a
plausible guess yields a confident-looking wrong digest — see
[`docs/commitments.md`](docs/commitments.md). If they fail, the encoding has drifted;
do not "fix" them by recomputing the expected values.

## Known limitations

- **No reorg handling.** `REORG_LAG` is the whole defence; a deeper reorg halts the
  scanner for an operator. See [`docs/reorg.md`](docs/reorg.md).
- **`btc_txid` is derived from a transition's outputs.** A transaction that consumes
  RGB++ cells and produces none (`kind = "exit"`) leaves it unresolved on the CKB
  path. Reading it from the RGB++ unlock witness would close that gap without a second
  source, at the cost of parsing a raw Bitcoin transaction.
- **Backfilled cells record `created_tx_index = -1`.** `get_transaction` does not
  report a transaction's position in its block, and fetching the block to learn it is
  not worth the round trip.
- **An empty asset response does not distinguish "no assets" from "not indexed
  yet".** Read endpoints answer `200` with an empty list whether the address really
  holds nothing or the scanner has not reached the relevant blocks — during initial
  sync, after a rebuild, or while the stream is stalled. This is deliberate for now:
  the only party who can currently be misled is the operator, who is also watching
  the sync logs, and `/status` already exposes `blocks_behind` and `last_error`.

  *Deferred, with a deadline.* The fix is to answer `503` with an explicit `syncing`
  state (plus progress) instead of a confident empty list, and to carry `indexed_to` /
  `chain_tip` on normal responses. It should land **before the first external
  consumer**, not after: adding a `503` to an endpoint that has always returned `200`
  is a breaking change, so deferring past that point turns a cheap addition into a
  versioning exercise.
- **Blockbook spender resolution is capped** at 5 pages of address history.
- Bitcoin **addresses are learned opportunistically**, so an address diff is only as
  good as what has been observed for it before.
