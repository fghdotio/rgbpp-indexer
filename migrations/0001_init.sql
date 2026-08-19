-- RGB++ indexer schema.
--
-- REORG: this version does not implement reorg handling. It indexes to
-- `tip - REORG_LAG` and treats everything below that line as settled. The schema is
-- nevertheless laid out so that reorg handling can be added later without a
-- migration, because retrofitting block anchors onto existing rows is the part that
-- would actually hurt. Columns and tables marked "reserved" below are written by
-- nothing today. See `docs/reorg.md` for the design they are shaped for.
--
-- The layout follows from one distinction between the two data layers:
--
--   * CKB-derived tables hold ordered, dependency-bearing facts. Every row is
--     anchored to the block that created it and, where applicable, the block that
--     consumed it. Those anchors are what would later make a rollback expressible as
--     a couple of statements per table.
--
--   * Bitcoin-derived tables hold observations of re-queryable questions. They carry
--     timestamps instead of block anchors, so a Bitcoin reorg never needs a rollback
--     at all — an observation a reorg invalidated is simply an observation that has
--     gone stale, and staleness is already how every reader treats it.
--
-- No aggregate is ever stored. Balances and counts are computed from these tables on
-- read, which is also what keeps a future rollback from having to reverse-correct a
-- running total.

-- ---------------------------------------------------------------------------
-- Indexer bookkeeping
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS indexer_state (
    stream                TEXT PRIMARY KEY,
    -- Highest block fully processed. Everything at or below this is queryable.
    last_block_number     BIGINT      NOT NULL,
    last_block_hash       BYTEA,
    -- Highest block this stream is currently allowed to reach (tip - reorg_lag).
    target_block_number   BIGINT,
    -- Chain tip as last observed, for lag reporting.
    chain_tip_number      BIGINT,
    reorg_lag             BIGINT      NOT NULL DEFAULT 0,
    last_error            TEXT,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- CKB headers we have indexed. Kept sparse on purpose: every block that produced
-- RGB++ activity, plus each checkpoint. Today only the checkpoint row is read, by the
-- sanity check that refuses to extend a checkpoint the node no longer agrees with.
-- The sparse ancestry is retained because a future rollback needs it to find a common
-- ancestor without fetching a header per block.
CREATE TABLE IF NOT EXISTS ckb_blocks (
    number               BIGINT PRIMARY KEY,
    hash                 BYTEA       NOT NULL,
    parent_hash          BYTEA       NOT NULL,
    block_timestamp      TIMESTAMPTZ NOT NULL,
    has_rgbpp_activity   BOOLEAN     NOT NULL DEFAULT FALSE,
    indexed_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ckb_blocks_hash_idx ON ckb_blocks (hash);
CREATE INDEX IF NOT EXISTS ckb_blocks_activity_idx
    ON ckb_blocks (number DESC) WHERE has_rgbpp_activity;

-- RESERVED. Audit trail for chain reorganisations. Nothing writes to this table in
-- this version; it exists so that adding reorg handling is a code change rather than
-- a migration on a live database.
CREATE TABLE IF NOT EXISTS ckb_reorgs (
    id                      BIGSERIAL PRIMARY KEY,
    detected_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    common_ancestor_number  BIGINT      NOT NULL,
    common_ancestor_hash    BYTEA,
    stale_tip_number        BIGINT      NOT NULL,
    stale_tip_hash          BYTEA,
    new_tip_number          BIGINT,
    new_tip_hash            BYTEA,
    rolled_back_cells       BIGINT      NOT NULL DEFAULT 0,
    rolled_back_spends      BIGINT      NOT NULL DEFAULT 0,
    rolled_back_transitions BIGINT      NOT NULL DEFAULT 0,
    auto_rolled_back        BOOLEAN     NOT NULL DEFAULT FALSE,
    note                    TEXT
);

-- ---------------------------------------------------------------------------
-- CKB facts: RGB++ cells
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS rgbpp_cells (
    ckb_tx_hash                BYTEA  NOT NULL,
    output_index               INT    NOT NULL,

    -- 'rgbpp' (owned by a BTC UTXO) or 'btc_time' (waiting on confirmations).
    lock_kind                  TEXT   NOT NULL,

    -- Bitcoin binding, txid stored in display order.
    btc_txid                   BYTEA  NOT NULL,
    btc_vout                   INT,             -- NULL for btc_time locks
    btc_time_after             INT,             -- NULL for rgbpp locks
    btc_time_target_lock_hash  BYTEA,
    btc_time_target_lock       JSONB,

    -- Script identity.
    lock_hash                  BYTEA  NOT NULL,
    lock_args                  BYTEA  NOT NULL,
    type_hash                  BYTEA,
    type_script                JSONB,

    -- Asset view. `udt_amount` is decoded from cell data, not accumulated.
    asset_kind                 TEXT   NOT NULL,
    udt_amount                 NUMERIC(40, 0),
    capacity                   BIGINT NOT NULL,
    cell_data                  BYTEA  NOT NULL,

    -- Creation anchor: rollback deletes rows above the common ancestor.
    created_block_number       BIGINT NOT NULL,
    created_block_hash         BYTEA  NOT NULL,
    created_tx_index           INT    NOT NULL,
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Consumption anchor: NULL means live on CKB. Rollback nulls these out above
    -- the common ancestor rather than deleting the row.
    consumed_block_number      BIGINT,
    consumed_block_hash        BYTEA,
    consumed_tx_hash           BYTEA,
    consumed_tx_index          INT,
    consumed_input_index       INT,
    consumed_at                TIMESTAMPTZ,

    PRIMARY KEY (ckb_tx_hash, output_index)
);

-- The hot path: "which live cell is bound to this UTXO".
CREATE INDEX IF NOT EXISTS rgbpp_cells_live_binding_idx
    ON rgbpp_cells (btc_txid, btc_vout) WHERE consumed_block_number IS NULL;
CREATE INDEX IF NOT EXISTS rgbpp_cells_btc_txid_idx ON rgbpp_cells (btc_txid);
CREATE INDEX IF NOT EXISTS rgbpp_cells_type_hash_idx ON rgbpp_cells (type_hash);
CREATE INDEX IF NOT EXISTS rgbpp_cells_lock_hash_idx ON rgbpp_cells (lock_hash);
-- Rollback and range queries walk these.
CREATE INDEX IF NOT EXISTS rgbpp_cells_created_block_idx ON rgbpp_cells (created_block_number);
CREATE INDEX IF NOT EXISTS rgbpp_cells_consumed_block_idx
    ON rgbpp_cells (consumed_block_number) WHERE consumed_block_number IS NOT NULL;
CREATE INDEX IF NOT EXISTS rgbpp_cells_consumed_tx_idx
    ON rgbpp_cells (consumed_tx_hash) WHERE consumed_tx_hash IS NOT NULL;

-- ---------------------------------------------------------------------------
-- CKB facts: state transitions
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS rgbpp_transitions (
    ckb_tx_hash          BYTEA  PRIMARY KEY,
    block_number         BIGINT NOT NULL,
    block_hash           BYTEA  NOT NULL,
    tx_index             INT    NOT NULL,
    block_timestamp      TIMESTAMPTZ,

    kind                 TEXT   NOT NULL,
    -- The Bitcoin transaction that authorised this transition, taken from the
    -- consumed cells' lock args. NULL for issuance, which has no RGB++ inputs.
    btc_txid             BYTEA,

    input_cell_count     INT    NOT NULL,
    output_cell_count    INT    NOT NULL,

    -- Commitment cross-check. `unchecked` until the Bitcoin side is observed.
    expected_commitment  BYTEA,
    observed_commitment  BYTEA,
    commitment_status    TEXT   NOT NULL DEFAULT 'unchecked',

    indexed_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS rgbpp_transitions_block_idx ON rgbpp_transitions (block_number);
CREATE INDEX IF NOT EXISTS rgbpp_transitions_btc_txid_idx
    ON rgbpp_transitions (btc_txid) WHERE btc_txid IS NOT NULL;
CREATE INDEX IF NOT EXISTS rgbpp_transitions_commitment_idx
    ON rgbpp_transitions (commitment_status) WHERE commitment_status <> 'match';

-- ---------------------------------------------------------------------------
-- Bitcoin observations (cache, not facts)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS btc_outpoints (
    txid            BYTEA NOT NULL,
    vout            INT   NOT NULL,
    -- unknown | unspent | spent_unconfirmed | spent_confirmed
    status          TEXT  NOT NULL,
    spender_txid    BYTEA,
    spent_height    INT,
    -- Owning address, learned opportunistically from address queries and funding
    -- transactions. This is what makes the address-level diff in the on-demand path
    -- possible: without it we could not say which outpoints we *used* to believe
    -- were live for an address.
    address         TEXT,
    source          TEXT  NOT NULL,
    first_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    observed_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- RESERVED. The expiry marker a Bitcoin reorg would set instead of rewriting
    -- history: the row stops being trusted and gets re-queried on next use. Readers
    -- already honour it (see the `rgbpp_cell_status` view); nothing sets it yet.
    invalidated_at  TIMESTAMPTZ,
    PRIMARY KEY (txid, vout)
);

CREATE INDEX IF NOT EXISTS btc_outpoints_observed_at_idx ON btc_outpoints (observed_at);
CREATE INDEX IF NOT EXISTS btc_outpoints_spender_idx
    ON btc_outpoints (spender_txid) WHERE spender_txid IS NOT NULL;
CREATE INDEX IF NOT EXISTS btc_outpoints_spent_idx
    ON btc_outpoints (status) WHERE status IN ('spent_unconfirmed', 'spent_confirmed');
CREATE INDEX IF NOT EXISTS btc_outpoints_address_idx
    ON btc_outpoints (address) WHERE address IS NOT NULL;

-- Bitcoin transactions relevant to RGB++, with the commitment we read from them.
CREATE TABLE IF NOT EXISTS btc_txs (
    txid          BYTEA PRIMARY KEY,
    block_height  INT,
    block_hash    BYTEA,
    block_time    TIMESTAMPTZ,
    commitment    BYTEA,
    input_count   INT   NOT NULL DEFAULT 0,
    output_count  INT   NOT NULL DEFAULT 0,
    source        TEXT  NOT NULL,
    observed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    invalidated_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS btc_txs_height_idx ON btc_txs (block_height);

-- RESERVED. Rolling window of recent Bitcoin block hashes, for noticing a Bitcoin
-- reorg and expiring observations above the fork point. Unused in this version.
CREATE TABLE IF NOT EXISTS btc_blocks (
    height       INT PRIMARY KEY,
    hash         BYTEA       NOT NULL,
    observed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Work queues and findings
-- ---------------------------------------------------------------------------

-- Outpoints an application (or the sweeper) asked us to re-check.
CREATE TABLE IF NOT EXISTS btc_refresh_queue (
    txid             BYTEA NOT NULL,
    vout             INT   NOT NULL,
    reason           TEXT  NOT NULL,
    -- Lower runs first: on-demand requests outrank background sweeps.
    priority         INT   NOT NULL DEFAULT 100,
    attempts         INT   NOT NULL DEFAULT 0,
    last_error       TEXT,
    requested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (txid, vout)
);

CREATE INDEX IF NOT EXISTS btc_refresh_queue_ready_idx
    ON btc_refresh_queue (priority, next_attempt_at);

CREATE TABLE IF NOT EXISTS rgbpp_anomalies (
    id                BIGSERIAL PRIMARY KEY,
    kind              TEXT  NOT NULL,
    -- Stable identity so re-detection updates rather than duplicates.
    dedup_key         TEXT  NOT NULL UNIQUE,
    btc_txid          BYTEA,
    btc_vout          INT,
    ckb_tx_hash       BYTEA,
    ckb_output_index  INT,
    detail            JSONB NOT NULL DEFAULT '{}'::jsonb,
    detected_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at       TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS rgbpp_anomalies_open_idx
    ON rgbpp_anomalies (kind, detected_at DESC) WHERE resolved_at IS NULL;

CREATE TABLE IF NOT EXISTS sweep_runs (
    id                 BIGSERIAL PRIMARY KEY,
    started_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at        TIMESTAMPTZ,
    outpoints_checked  BIGINT NOT NULL DEFAULT 0,
    status_changed     BIGINT NOT NULL DEFAULT 0,
    anomalies_found    BIGINT NOT NULL DEFAULT 0,
    status             TEXT   NOT NULL DEFAULT 'running',
    error              TEXT
);

-- ---------------------------------------------------------------------------
-- Derived views. These are plain views, never materialised: status is computed
-- from the two independent facts every time it is read.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE VIEW rgbpp_cell_status AS
SELECT
    c.*,
    o.status         AS btc_status,
    o.spender_txid   AS btc_spender_txid,
    o.spent_height   AS btc_spent_height,
    o.observed_at    AS btc_observed_at,
    o.address        AS btc_address,
    CASE
        WHEN c.consumed_block_number IS NOT NULL THEN 'spent'
        WHEN o.invalidated_at IS NULL
             AND o.status IN ('spent_unconfirmed', 'spent_confirmed') THEN 'pending_ckb'
        ELSE 'live'
    END AS status
FROM rgbpp_cells c
LEFT JOIN btc_outpoints o
       ON o.txid = c.btc_txid
      AND o.vout = c.btc_vout;
