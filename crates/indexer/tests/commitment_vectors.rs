//! Commitment computation, pinned to real on-chain transactions.
//!
//! Both fixtures are genuine CKB testnet transactions paired with the commitment
//! their Bitcoin counterpart actually published in an `OP_RETURN`. They exist because
//! the pre-image encoding has several places where a plausible guess produces a
//! confident-looking wrong digest — the txid placeholder in output lock args above
//! all — and a unit test built from the same assumptions as the implementation would
//! happily agree with a wrong one.
//!
//! If these ever fail, the commitment encoding has drifted from what the protocol
//! actually publishes. Do not "fix" them by recomputing the expected values.

use rgbpp_ckb::types::RpcTransaction;
use rgbpp_indexer::extract::{self, ResolvedInput};
use rgbpp_types::ckb::{CellOutput, CkbOutPoint, H256, ScriptHashType};
use rgbpp_types::protocol::{LockBinding, ProtocolScripts, ScriptId};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    note: String,
    expected_commitment: String,
    rgbpp_input_count: usize,
    rgbpp_output_count: usize,
    transaction: RpcTransaction,
}

/// The RGB++ deployment on CKB testnet, which these fixtures were captured from.
fn testnet_protocol() -> ProtocolScripts {
    ProtocolScripts {
        rgbpp_lock: ScriptId::new(
            H256::from_hex("0x61ca7a4796a4eb19ca4f0d065cb9b10ddcf002f10f7cbb810c706cb6bb5c3248")
                .unwrap(),
            ScriptHashType::Type,
        ),
        btc_time_lock: ScriptId::new(
            H256::from_hex("0x00cdf8fab0f8ac638758ebf5ea5e4052b1d71e8a77b9f43139718621f6849326")
                .unwrap(),
            ScriptHashType::Type,
        ),
    }
}

fn check(fixture_json: &str) {
    let fixture: Fixture = serde_json::from_str(fixture_json).expect("fixture parses");
    let protocol = testnet_protocol();
    let tx = fixture.transaction;

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

    let rgbpp_outputs = extract::classify_outputs(&protocol, &outputs, &outputs_data).unwrap();
    assert_eq!(
        rgbpp_outputs.len(),
        fixture.rgbpp_output_count,
        "recognised a different number of RGB++ outputs than the fixture records"
    );

    // The committed inputs are the leading RGB++ inputs. Only their outpoints enter
    // the pre-image, so the consumed cells themselves do not need resolving here.
    let resolved: Vec<ResolvedInput> = tx
        .inputs
        .iter()
        .take(fixture.rgbpp_input_count)
        .enumerate()
        .map(|(index, input)| ResolvedInput {
            input_index: index as u32,
            out_point: CkbOutPoint::new(
                input.previous_output.tx_hash,
                input.previous_output.index.0,
            ),
            binding: rgbpp_outputs[0].binding.clone(),
            cell: outputs[0].clone(),
            data: Vec::new(),
        })
        .collect();

    let computed = extract::expected_commitment(&resolved, &rgbpp_outputs)
        .expect("commitment is computable");
    assert_eq!(
        hex::encode(computed),
        fixture.expected_commitment,
        "computed commitment does not match the one published on Bitcoin"
    );
}

#[test]
fn transfer_commitment_matches_the_published_op_return() {
    check(include_str!("fixtures/testnet_transfer.json"));
}

#[test]
fn leap_to_ckb_commitment_matches_the_published_op_return() {
    check(include_str!("fixtures/testnet_leap_to_ckb.json"));
}

/// Guard the specific mistake the fixtures were captured to catch: committing the
/// on-chain lock args instead of the placeholdered ones.
#[test]
fn using_the_on_chain_txid_would_not_match() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/testnet_transfer.json")).unwrap();
    let protocol = testnet_protocol();
    let tx = fixture.transaction;

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
    let rgbpp_outputs = extract::classify_outputs(&protocol, &outputs, &outputs_data).unwrap();

    let inputs: Vec<CkbOutPoint> = tx
        .inputs
        .iter()
        .take(fixture.rgbpp_input_count)
        .map(|i| CkbOutPoint::new(i.previous_output.tx_hash, i.previous_output.index.0))
        .collect();
    let naive_outputs: Vec<(CellOutput, Vec<u8>)> = rgbpp_outputs
        .iter()
        .map(|o| (o.cell.clone(), o.data.clone()))
        .collect();

    let naive = rgbpp_types::commitment::CommitmentPreimage::new(&inputs, &naive_outputs)
        .commitment();
    assert_ne!(hex::encode(naive), fixture.expected_commitment);
}

/// Sanity: the fixtures really are the two different protocol shapes.
#[test]
fn fixtures_cover_both_output_lock_kinds() {
    let transfer: Fixture =
        serde_json::from_str(include_str!("fixtures/testnet_transfer.json")).unwrap();
    let leap: Fixture =
        serde_json::from_str(include_str!("fixtures/testnet_leap_to_ckb.json")).unwrap();
    let protocol = testnet_protocol();

    for (fixture, expected_kind) in [
        (transfer, rgbpp_types::protocol::LockKind::Rgbpp),
        (leap, rgbpp_types::protocol::LockKind::BtcTime),
    ] {
        let tx = fixture.transaction;
        let outputs: Vec<CellOutput> = tx
            .outputs
            .iter()
            .map(|o| CellOutput {
                capacity: o.capacity.0,
                lock: o.lock.clone(),
                type_: o.type_.clone(),
            })
            .collect();
        let data: Vec<Vec<u8>> = tx.outputs_data.iter().map(|d| d.0.clone()).collect();
        let found = extract::classify_outputs(&protocol, &outputs, &data).unwrap();
        assert!(found.iter().all(|o| o.binding.kind() == expected_kind));
        assert!(matches!(
            found[0].binding,
            LockBinding::Rgbpp(_) | LockBinding::BtcTime(_)
        ));
    }
}
