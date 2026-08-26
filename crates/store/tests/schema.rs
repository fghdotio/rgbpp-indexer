//! Schema and query tests against a real PostgreSQL.
//!
//! The store writes runtime SQL so that building never needs a database — which
//! means the queries themselves are only ever validated here. These tests exist to
//! catch the class of bug that would otherwise reach production: a renamed column, a
//! view whose status derivation drifts from the Rust model, an `UNNEST` binding that
//! does not typecheck.
//!
//! Each test gets its own PostgreSQL schema, so they run in parallel without a
//! shared-fixture dance. Set `TEST_DATABASE_URL` to run them; without it they skip,
//! so `cargo test` stays green on a machine with no database.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::Utc;
use rgbpp_store::models::*;
use rgbpp_store::state::CKB_STREAM;
use rgbpp_store::Store;
use rgbpp_types::asset::AssetKind;
use rgbpp_types::bitcoin::BtcTxid;
use rgbpp_types::protocol::LockKind;
use rgbpp_types::state::{AnomalyKind, OutpointSpendStatus, TransitionKind};
use sqlx::postgres::PgPoolOptions;

/// Build an isolated store, or return `None` when no test database is configured.
async fn store_for(test_name: &str) -> Option<Store> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let schema = format!("test_{test_name}");

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to the test database");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin)
        .await
        .expect("drop schema");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .expect("create schema");
    admin.close().await;

    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {search_path}"))
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect with an isolated schema");

    let store = Store::from_pool(pool);
    store.migrate().await.expect("migrations apply cleanly");
    Some(store)
}

fn txid(byte: u8) -> Vec<u8> {
    BtcTxid::from_display_bytes([byte; 32]).to_display_vec()
}

fn hash(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

fn rgbpp_cell(tx_byte: u8, index: i32, btc_byte: u8, vout: i32, block: i64) -> NewCell {
    NewCell {
        ckb_tx_hash: hash(tx_byte),
        output_index: index,
        lock_kind: LockKind::Rgbpp,
        btc_txid: txid(btc_byte),
        btc_vout: Some(vout),
        btc_time_after: None,
        btc_time_target_lock_hash: None,
        btc_time_target_lock: None,
        lock_hash: hash(tx_byte.wrapping_add(100)),
        lock_args: vec![1, 2, 3],
        type_hash: Some(hash(0xaa)),
        type_script: Some(serde_json::json!({
            "code_hash": "0x00", "hash_type": "type", "args": "0x"
        })),
        asset_kind: AssetKind::Xudt,
        udt_amount: Some(BigDecimal::from_str("340282366920938463463374607431768211455").unwrap()),
        capacity: 100 * 100_000_000,
        cell_data: vec![0xff; 16],
        created_block_number: block,
        created_block_hash: hash(block as u8),
        created_tx_index: 0,
    }
}

fn block_record(number: i64) -> BlockRecord {
    BlockRecord {
        number,
        hash: hash(number as u8),
        parent_hash: hash((number - 1) as u8),
        timestamp: Utc::now(),
        has_rgbpp_activity: true,
    }
}

fn batch(number: i64) -> IndexBatch {
    IndexBatch {
        blocks: vec![block_record(number)],
        checkpoint_number: number,
        checkpoint_hash: Some(hash(number as u8)),
        chain_tip: number + 24,
        target: number,
        reorg_lag: 24,
        ..Default::default()
    }
}

#[tokio::test]
async fn cell_status_is_derived_from_both_chains() {
    let Some(store) = store_for("lifecycle").await else {
        eprintln!("skipping: TEST_DATABASE_URL is not set");
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    // A u128 amount at its maximum: the reason the column is NUMERIC(40, 0) and the
    // API renders amounts as strings.
    let cell = rgbpp_cell(1, 0, 0x77, 3, 100);
    let mut first = batch(100);
    first.cells.push(cell.clone());
    first.transitions.push(NewTransition {
        ckb_tx_hash: hash(1),
        block_number: 100,
        block_hash: hash(100),
        tx_index: 0,
        block_timestamp: Some(Utc::now()),
        kind: TransitionKind::Issuance,
        btc_txid: Some(txid(0x77)),
        input_cell_count: 0,
        output_cell_count: 1,
        expected_commitment: Some(vec![0xab; 32]),
    });
    let stats = store.apply_batch(&first).await.unwrap();
    assert_eq!(stats.cells, 1);
    assert_eq!(stats.transitions, 1);

    // No Bitcoin observation yet: the cell reads as live.
    let rows = store
        .cells_by_btc_outpoints(&[txid(0x77)], &[3], false)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "live");
    assert_eq!(
        rows[0].udt_amount.as_ref().unwrap().to_string(),
        "340282366920938463463374607431768211455"
    );

    // Bitcoin says the bound UTXO is spent; CKB has not caught up. This is the gap
    // the whole on-demand path exists to expose.
    let changed = store
        .upsert_observation(
            &txid(0x77),
            3,
            OutpointSpendStatus::SpentUnconfirmed {
                spender: BtcTxid::from_display_bytes([0x99; 32]),
            },
            Some("bc1qexample"),
            "test",
        )
        .await
        .unwrap();
    assert!(changed, "a first observation counts as a change");

    let rows = store
        .cells_by_btc_outpoints(&[txid(0x77)], &[3], false)
        .await
        .unwrap();
    assert_eq!(rows[0].status, "pending_ckb");
    assert_eq!(rows[0].btc_address.as_deref(), Some("bc1qexample"));

    // Re-observing the same status is not a change.
    let changed = store
        .upsert_observation(
            &txid(0x77),
            3,
            OutpointSpendStatus::SpentUnconfirmed {
                spender: BtcTxid::from_display_bytes([0x99; 32]),
            },
            None,
            "test",
        )
        .await
        .unwrap();
    assert!(!changed);
    // ... and the address survives an observation that did not carry one.
    let observation = store
        .get_observation(&txid(0x77), 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.address.as_deref(), Some("bc1qexample"));

    // The CKB transition lands: the cell is now unambiguously spent.
    let mut second = batch(101);
    second.spends.push(CellSpend {
        ckb_tx_hash: hash(1),
        output_index: 0,
        consumed_block_number: 101,
        consumed_block_hash: hash(101),
        consumed_tx_hash: hash(2),
        consumed_tx_index: 1,
        consumed_input_index: 0,
    });
    let stats = store.apply_batch(&second).await.unwrap();
    assert_eq!(stats.spends, 1, "the spend attached to a known cell");

    let rows = store
        .cells_by_btc_outpoints(&[txid(0x77)], &[3], true)
        .await
        .unwrap();
    assert_eq!(rows[0].status, "spent");
    assert_eq!(rows[0].consumed_block_number, Some(101));

    // Spent cells are excluded unless asked for.
    let live = store
        .cells_by_btc_outpoints(&[txid(0x77)], &[3], false)
        .await
        .unwrap();
    assert!(live.is_empty());

    // The checkpoint moved with the data, in the same transaction.
    let state = store.get_stream_state(CKB_STREAM).await.unwrap().unwrap();
    assert_eq!(state.last_block_number, 101);
    assert_eq!(state.reorg_lag, 24);

    let counts = store.counts().await.unwrap();
    assert_eq!(counts.total_cells, 1);
    assert_eq!(counts.live_cells, 0);
    assert_eq!(counts.transitions, 1);
}

/// The status rule exists twice: as SQL in the `rgbpp_cell_status` view, and as Rust
/// in `rgbpp_types::state::derive_cell_status`. Two implementations of one rule drift,
/// and the drift would be invisible — the view feeds the API while the Rust function
/// feeds everything reasoning in-process. This pins them together.
#[tokio::test]
async fn the_view_and_the_rust_status_model_agree() {
    let Some(store) = store_for("status_parity").await else {
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    let spender = BtcTxid::from_display_bytes([0x99; 32]);
    let cases = [
        (0x11u8, false, OutpointSpendStatus::Unknown),
        (0x22, false, OutpointSpendStatus::Unspent),
        (
            0x33,
            false,
            OutpointSpendStatus::SpentUnconfirmed { spender },
        ),
        (
            0x44,
            false,
            OutpointSpendStatus::SpentConfirmed {
                spender,
                height: 800_000,
            },
        ),
        (0x55, true, OutpointSpendStatus::Unspent),
        (
            0x66,
            true,
            OutpointSpendStatus::SpentConfirmed {
                spender,
                height: 800_000,
            },
        ),
    ];

    let mut b = batch(100);
    for (index, (btc_byte, _, _)) in cases.iter().enumerate() {
        b.cells.push(rgbpp_cell(1, index as i32, *btc_byte, 0, 100));
    }
    store.apply_batch(&b).await.unwrap();

    let mut consumed = batch(101);
    for (index, (_, is_consumed, _)) in cases.iter().enumerate() {
        if *is_consumed {
            consumed.spends.push(CellSpend {
                ckb_tx_hash: hash(1),
                output_index: index as i32,
                consumed_block_number: 101,
                consumed_block_hash: hash(101),
                consumed_tx_hash: hash(2),
                consumed_tx_index: 0,
                consumed_input_index: index as i32,
            });
        }
    }
    store.apply_batch(&consumed).await.unwrap();

    for (btc_byte, _, status) in cases {
        if status != OutpointSpendStatus::Unknown {
            store
                .upsert_observation(&txid(btc_byte), 0, status, None, "test")
                .await
                .unwrap();
        }
    }

    for (index, (btc_byte, is_consumed, status)) in cases.iter().enumerate() {
        let row = store
            .cell_by_out_point(&hash(1), index as i32)
            .await
            .unwrap()
            .unwrap();
        let from_rust = rgbpp_types::state::derive_cell_status(*is_consumed, *status);
        assert_eq!(
            row.status,
            from_rust.as_str(),
            "view and Rust disagree for btc {btc_byte:#x} (consumed: {is_consumed}, btc: {status:?})"
        );
    }
}

#[tokio::test]
async fn a_spend_with_no_matching_cell_reports_zero() {
    let Some(store) = store_for("orphan_spend").await else {
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    let mut b = batch(100);
    b.spends.push(CellSpend {
        ckb_tx_hash: hash(0xee),
        output_index: 0,
        consumed_block_number: 100,
        consumed_block_hash: hash(100),
        consumed_tx_hash: hash(2),
        consumed_tx_index: 0,
        consumed_input_index: 0,
    });
    // The scanner relies on this count to notice gaps it has to backfill.
    assert_eq!(store.apply_batch(&b).await.unwrap().spends, 0);
}

#[tokio::test]
async fn refresh_queue_claims_backs_off_and_completes() {
    let Some(store) = store_for("queue").await else {
        return;
    };

    store
        .enqueue_refresh(&txid(1), 0, "sweep", rgbpp_store::queue::priority::SWEEP)
        .await
        .unwrap();
    // Re-queueing at higher urgency promotes the existing entry rather than duplicating it.
    store
        .enqueue_refresh(&txid(1), 0, "api", rgbpp_store::queue::priority::ON_DEMAND)
        .await
        .unwrap();
    store
        .enqueue_refresh_many(
            &[(txid(2), 1), (txid(3), 0)],
            "sweep",
            rgbpp_store::queue::priority::SWEEP,
        )
        .await
        .unwrap();
    assert_eq!(store.refresh_queue_depth().await.unwrap(), 3);

    let claimed = store.claim_refresh_batch(10).await.unwrap();
    assert_eq!(claimed.len(), 3);
    assert_eq!(claimed[0].priority, rgbpp_store::queue::priority::ON_DEMAND);
    assert_eq!(claimed[0].attempts, 1);

    // Backoff was applied at claim time, so nothing is immediately due again.
    assert!(store.claim_refresh_batch(10).await.unwrap().is_empty());

    store.complete_refresh(&txid(1), 0).await.unwrap();
    assert_eq!(store.refresh_queue_depth().await.unwrap(), 2);

    store
        .fail_refresh(&txid(2), 1, "upstream timeout")
        .await
        .unwrap();
    let dropped = store.drop_exhausted_refreshes(1).await.unwrap();
    assert_eq!(
        dropped.len(),
        2,
        "both remaining entries hit the attempt cap"
    );
    assert_eq!(store.refresh_queue_depth().await.unwrap(), 0);
}

#[tokio::test]
async fn anomalies_deduplicate_and_reopen() {
    let Some(store) = store_for("anomalies").await else {
        return;
    };
    let key = rgbpp_store::anomalies::dedup_key(AnomalyKind::BtcSpentWithoutCkb, "abc:0");

    for confirmations in [6, 7] {
        store
            .record_anomaly(
                AnomalyKind::BtcSpentWithoutCkb,
                &key,
                Some(&txid(1)),
                Some(0),
                None,
                None,
                serde_json::json!({ "confirmations": confirmations }),
            )
            .await
            .unwrap();
    }
    let open = store.list_anomalies(None, false, 10).await.unwrap();
    assert_eq!(open.len(), 1, "the same finding is one row, not two");
    assert_eq!(open[0].detail["confirmations"], 7, "detail is refreshed");
    assert_eq!(store.open_anomaly_count().await.unwrap(), 1);

    assert!(store.resolve_anomaly(&key).await.unwrap());
    assert!(
        !store.resolve_anomaly(&key).await.unwrap(),
        "already resolved"
    );
    assert!(store
        .list_anomalies(None, false, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(store.list_anomalies(None, true, 10).await.unwrap().len(), 1);

    // A finding that comes back reopens rather than staying silently closed.
    store
        .record_anomaly(
            AnomalyKind::BtcSpentWithoutCkb,
            &key,
            Some(&txid(1)),
            Some(0),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(
        store.list_anomalies(None, false, 10).await.unwrap().len(),
        1
    );

    // Resolving by outpoint is what happens when the CKB transition finally lands.
    assert_eq!(
        store
            .resolve_anomalies_for_outpoint(&txid(1), 0)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn address_and_sweep_work_lists_are_derived_from_cells() {
    let Some(store) = store_for("worklists").await else {
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    let mut b = batch(100);
    b.cells.push(rgbpp_cell(1, 0, 0x11, 0, 100));
    b.cells.push(rgbpp_cell(1, 1, 0x22, 1, 100));
    store.apply_batch(&b).await.unwrap();

    // An address's UTXO set is mostly not RGB++. Only the bound outpoints are kept,
    // so this table tracks protocol activity rather than total wallet activity.
    let recorded = store
        .record_outpoint_addresses(
            &[txid(0x11), txid(0xde), txid(0xad)],
            &[0, 0, 7],
            "bc1qalice",
        )
        .await
        .unwrap();
    assert_eq!(
        recorded, 1,
        "only the outpoint with a bound cell is recorded"
    );

    let believed = store
        .live_bound_outpoints_for_address("bc1qalice")
        .await
        .unwrap();
    assert_eq!(believed, vec![(txid(0x11), 0)]);

    store
        .upsert_observation(&txid(0x11), 0, OutpointSpendStatus::Unspent, None, "test")
        .await
        .unwrap();

    // Paging over the sweep work list.
    let page = store.live_bound_outpoints_after(None, 0, 1).await.unwrap();
    assert_eq!(page.len(), 1);
    let next = store
        .live_bound_outpoints_after(Some(&page[0].0), page[0].1, 10)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_ne!(next[0], page[0]);

    // A spent binding with no CKB transition is exactly the pending window.
    store
        .upsert_observation(
            &txid(0x22),
            1,
            OutpointSpendStatus::SpentConfirmed {
                spender: BtcTxid::from_display_bytes([0x99; 32]),
                height: 800_000,
            },
            None,
            "test",
        )
        .await
        .unwrap();
    let pending = store.pending_ckb_outpoints(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].vout, 1);
    assert_eq!(
        pending[0].spend_status().unwrap(),
        OutpointSpendStatus::SpentConfirmed {
            spender: BtcTxid::from_display_bytes([0x99; 32]),
            height: 800_000,
        }
    );

    let counts = store.counts().await.unwrap();
    assert_eq!(counts.pending_ckb_cells, 1);
    assert_eq!(counts.live_cells, 2);
}

#[tokio::test]
async fn balances_are_computed_on_read() {
    let Some(store) = store_for("balances").await else {
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    let mut b = batch(100);
    for (index, btc_byte, vout) in [(0i32, 0x11u8, 0i32), (1, 0x11, 1), (2, 0x22, 0)] {
        let mut cell = rgbpp_cell(1, index, btc_byte, vout, 100);
        cell.udt_amount = Some(BigDecimal::from(1_000));
        b.cells.push(cell);
    }
    store.apply_batch(&b).await.unwrap();

    let txids = vec![txid(0x11), txid(0x11), txid(0x22)];
    let vouts = vec![0, 1, 0];
    let balances = store
        .asset_balances_for_outpoints(&txids, &vouts, false)
        .await
        .unwrap();
    assert_eq!(balances.len(), 1, "one asset");
    assert_eq!(balances[0].cell_count, 3);
    assert_eq!(
        balances[0].total_amount.as_ref().unwrap().to_string(),
        "3000"
    );
    assert_eq!(
        balances[0].total_capacity.as_ref().unwrap().to_string(),
        (3 * 100 * 100_000_000i64).to_string()
    );

    // One binding moves on Bitcoin. The conservative view drops it; the pending view keeps it.
    store
        .upsert_observation(
            &txid(0x22),
            0,
            OutpointSpendStatus::SpentUnconfirmed {
                spender: BtcTxid::from_display_bytes([0x99; 32]),
            },
            None,
            "test",
        )
        .await
        .unwrap();

    let conservative = store
        .asset_balances_for_outpoints(&txids, &vouts, false)
        .await
        .unwrap();
    assert_eq!(conservative[0].cell_count, 2);
    assert_eq!(
        conservative[0].total_amount.as_ref().unwrap().to_string(),
        "2000"
    );

    let optimistic = store
        .asset_balances_for_outpoints(&txids, &vouts, true)
        .await
        .unwrap();
    assert_eq!(optimistic[0].cell_count, 3);
}

#[tokio::test]
async fn transitions_and_commitment_status_round_trip() {
    let Some(store) = store_for("transitions").await else {
        return;
    };
    store.init_stream(CKB_STREAM, 100).await.unwrap();

    let mut b = batch(100);
    b.transitions.push(NewTransition {
        ckb_tx_hash: hash(1),
        block_number: 100,
        block_hash: hash(100),
        tx_index: 0,
        block_timestamp: Some(Utc::now()),
        kind: TransitionKind::Transfer,
        btc_txid: Some(txid(0x55)),
        input_cell_count: 1,
        output_cell_count: 1,
        expected_commitment: Some(vec![0xab; 32]),
    });
    store.apply_batch(&b).await.unwrap();

    assert_eq!(store.unchecked_transitions(10).await.unwrap().len(), 1);
    store
        .set_commitment_result(&hash(1), Some(&[0xab; 32]), CommitmentStatus::Match)
        .await
        .unwrap();
    assert!(store.unchecked_transitions(10).await.unwrap().is_empty());

    let row = store.transition_by_ckb_tx(&hash(1)).await.unwrap().unwrap();
    assert_eq!(row.commitment_status, "match");
    assert_eq!(
        store
            .transitions_by_btc_txid(&txid(0x55))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.recent_transitions(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn sweep_runs_are_recorded() {
    let Some(store) = store_for("sweeps").await else {
        return;
    };
    let id = store.start_sweep_run().await.unwrap();
    store.finish_sweep_run(id, 42, 3, 1, None).await.unwrap();

    let (run_id, _started, finished, checked, anomalies, status) =
        store.last_sweep_run().await.unwrap().unwrap();
    assert_eq!(run_id, id);
    assert!(finished.is_some());
    assert_eq!(checked, 42);
    assert_eq!(anomalies, 1);
    assert_eq!(status, "completed");
}

#[tokio::test]
async fn block_headers_keep_their_activity_flag_and_prune() {
    let Some(store) = store_for("headers").await else {
        return;
    };

    store.upsert_block(&block_record(100)).await.unwrap();
    // A checkpoint write for the same height must not clear the activity flag.
    let mut checkpoint = block_record(100);
    checkpoint.has_rgbpp_activity = false;
    store.upsert_block(&checkpoint).await.unwrap();
    assert!(
        store
            .get_block(100)
            .await
            .unwrap()
            .unwrap()
            .has_rgbpp_activity
    );

    for number in [101, 102, 103] {
        store.upsert_block(&block_record(number)).await.unwrap();
    }
    let descending = store.blocks_descending_from(102, 10).await.unwrap();
    assert_eq!(
        descending.iter().map(|b| b.number).collect::<Vec<_>>(),
        vec![102, 101, 100]
    );

    assert_eq!(store.prune_blocks_below(102).await.unwrap(), 2);
    assert!(store.get_block(100).await.unwrap().is_none());
    assert!(store.get_block(103).await.unwrap().is_some());
}
