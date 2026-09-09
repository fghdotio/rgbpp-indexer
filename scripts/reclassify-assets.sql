-- Reclassify cells that were indexed before their asset's code hash was configured.
--
-- WHY THIS EXISTS
--
-- `asset_kind` is decided at index time (crates/indexer/src/extract.rs, via
-- AssetScripts::classify) and written once. A cell whose type script matched no
-- configured code hash is stored as `unknown`, and adding the code hash to
-- config afterwards does not go back and fix it.
--
-- Re-scanning does not fix it either, which is the part worth knowing: the insert
-- in crates/store/src/cells.rs carries
--
--     ON CONFLICT (ckb_tx_hash, output_index) DO UPDATE SET
--        created_block_number, created_block_hash, created_tx_index
--
-- so a second pass over the same blocks refreshes those three columns and leaves
-- `asset_kind` alone. Short of `reset-db` and a full resync, this UPDATE is the
-- way to correct existing rows.
--
-- HOW TO RUN
--
--     docker compose exec -T postgres psql -U rgbpp -d rgbpp \
--       < scripts/reclassify-assets.sql
--
-- To preview without writing, change the final COMMIT to ROLLBACK: the counts are
-- reported either way.
--
-- SCOPE — READ BEFORE USING THIS FOR ANYTHING ELSE
--
-- This script is only safe for non-fungible kinds (`spore`, `spore_cluster`).
-- `udt_amount` is parsed at index time and only for fungible assets
-- (extract.rs::fungible_amount), so a cell indexed as `unknown` has a NULL amount.
-- For spores that is already correct. Reclassifying a cell to `xudt` or `sudt`
-- would leave it fungible with a NULL amount, which silently understates every
-- balance that sums it — those need `udt_amount` backfilled from the first 16
-- bytes of `cell_data` as a little-endian u128 in the same statement, and this
-- script deliberately does not attempt that.
--
-- `rgbpp_cell_status` is a view, not a materialised one, so nothing downstream
-- needs a second update.
--
-- PERFORMANCE
--
-- `type_script->>'code_hash'` has no index; this is a sequential scan of
-- rgbpp_cells. Fine at testnet size, worth estimating before running against a
-- large mainnet index.

\set ON_ERROR_STOP on

-- Deployment identity, not a property of this software. The values below are the
-- Spore scripts observed on CKB testnet; cross-check them against the deployment
-- you are indexing before running this, exactly as you would the [protocol]
-- section of the config.
\set spore_code_hash '0x685a60219309029d01310311dba953d67029170ca4848a4ff638e57002130a0d'
\set cluster_code_hash '0x0bbe768b519d8ea7b96d58f1182eb7e6ef96c541fbd9526975077ee09f049058'
\set hash_type 'data1'

BEGIN;

\echo ''
\echo '== unknown cells, grouped by type script =='
SELECT type_script ->> 'code_hash' AS code_hash,
       type_script ->> 'hash_type' AS hash_type,
       count(*)                    AS cells,
       count(DISTINCT type_hash)   AS assets
  FROM rgbpp_cells
 WHERE asset_kind = 'unknown'
   AND type_script IS NOT NULL
 GROUP BY 1, 2
 ORDER BY cells DESC;

UPDATE rgbpp_cells
   SET asset_kind = 'spore'
 WHERE asset_kind = 'unknown'
   AND type_script ->> 'code_hash' = :'spore_code_hash'
   AND type_script ->> 'hash_type' = :'hash_type';

UPDATE rgbpp_cells
   SET asset_kind = 'spore_cluster'
 WHERE asset_kind = 'unknown'
   AND type_script ->> 'code_hash' = :'cluster_code_hash'
   AND type_script ->> 'hash_type' = :'hash_type';

\echo ''
\echo '== after: cells and distinct assets by kind =='
SELECT asset_kind,
       count(*)                  AS cells,
       count(DISTINCT type_hash) AS assets
  FROM rgbpp_cells
 GROUP BY 1
 ORDER BY cells DESC;

-- Change to ROLLBACK to preview the counts without writing.
COMMIT;
