//! Turning a CKB transaction into RGB++ facts.
//!
//! This module is deliberately pure: it takes an already-fetched transaction plus
//! its already-resolved inputs and returns rows to write. All the I/O — RPC calls,
//! database lookups, backfilling inputs the indexer has never seen — lives in the
//! scanner, so the protocol interpretation can be tested exhaustively without a node.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use rgbpp_store::models::{udt_amount_to_decimal, CellSpend, NewCell, NewTransition};
use rgbpp_types::asset::{self, AssetKind, AssetScripts};
use rgbpp_types::bitcoin::BtcTxid;
use rgbpp_types::ckb::{CellOutput, CkbOutPoint, Script, H256};
use rgbpp_types::commitment::CommitmentPreimage;
use rgbpp_types::protocol::{self, LockBinding, LockKind, ProtocolScripts};
use rgbpp_types::state::TransitionKind;

use crate::error::{IndexerError, Result};

/// An input whose consumed cell the scanner has already identified as RGB++.
#[derive(Clone, Debug)]
pub struct ResolvedInput {
    pub input_index: u32,
    pub out_point: CkbOutPoint,
    pub binding: LockBinding,
    pub cell: CellOutput,
    pub data: Vec<u8>,
}

/// Where in the chain a transaction sits.
#[derive(Clone, Debug)]
pub struct BlockContext {
    pub number: u64,
    pub hash: H256,
    pub timestamp: DateTime<Utc>,
    pub tx_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ExtractedTx {
    pub cells: Vec<NewCell>,
    pub spends: Vec<CellSpend>,
    pub transition: Option<NewTransition>,
}

/// An output recognised as belonging to the RGB++ surface.
#[derive(Clone, Debug)]
pub struct RgbppOutput {
    pub index: u32,
    pub binding: LockBinding,
    pub cell: CellOutput,
    pub data: Vec<u8>,
}

/// Classify a transaction's outputs.
pub fn classify_outputs(
    protocol: &ProtocolScripts,
    outputs: &[CellOutput],
    outputs_data: &[Vec<u8>],
) -> Result<Vec<RgbppOutput>> {
    let mut found = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let Some(kind) = protocol.classify(&output.lock) else {
            continue;
        };
        let binding = match LockBinding::parse(kind, output.lock.args.as_slice()) {
            Ok(binding) => binding,
            Err(e) => {
                // A cell can carry the right code hash with unparseable args — a
                // malformed or future-versioned deployment. Skipping it keeps the
                // scanner moving; ignoring the whole transaction would not.
                tracing::warn!(
                    output_index = index,
                    error = %e,
                    "output has an RGB++ lock code hash but unreadable args; skipping"
                );
                continue;
            }
        };
        found.push(RgbppOutput {
            index: index as u32,
            binding,
            cell: output.clone(),
            data: outputs_data.get(index).cloned().unwrap_or_default(),
        });
    }
    Ok(found)
}

/// Decide what this transaction did to RGB++ state.
///
/// Ordering of the checks matters. A BTC time lock input means the transaction is
/// releasing a leap, whatever else it does; issuance is the absence of RGB++ inputs;
/// and a transaction with RGB++ inputs but no RGB++ outputs has left the protocol
/// surface entirely.
pub fn classify_transition(inputs: &[ResolvedInput], outputs: &[RgbppOutput]) -> TransitionKind {
    let btc_time_in = inputs
        .iter()
        .filter(|i| i.binding.kind() == LockKind::BtcTime)
        .count();
    let rgbpp_in = inputs.len() - btc_time_in;
    let rgbpp_out = outputs
        .iter()
        .filter(|o| o.binding.kind() == LockKind::Rgbpp)
        .count();
    let btc_time_out = outputs.len() - rgbpp_out;

    if btc_time_in > 0 {
        TransitionKind::BtcTimeUnlock
    } else if rgbpp_in == 0 {
        TransitionKind::Issuance
    } else if rgbpp_out == 0 && btc_time_out > 0 {
        TransitionKind::LeapToCkb
    } else if rgbpp_out > 0 {
        TransitionKind::Transfer
    } else {
        TransitionKind::Exit
    }
}

/// The Bitcoin transaction that authorised this CKB transaction.
///
/// Derived from the *outputs*: a transfer's new RGB++ cells (or the BTC time lock
/// cells a leap produces) are bound to outputs of the very transaction that spent
/// the old bindings, so their args name it.
///
/// Transactions that produce no RGB++ outputs leave this unresolved here. The
/// reconciler fills it in later from the Bitcoin side, where the spender of the
/// consumed bindings is exactly the same transaction. Reading it from the RGB++
/// unlock witness would also work and would not need a second source, at the cost of
/// parsing a raw Bitcoin transaction inside the CKB path.
pub fn derive_btc_txid(outputs: &[RgbppOutput]) -> Option<BtcTxid> {
    let mut txid: Option<BtcTxid> = None;
    for output in outputs {
        let candidate = output.binding.txid();
        match txid {
            None => txid = Some(candidate),
            // Mixed txids mean the outputs are not all bound to one Bitcoin
            // transaction, so no single value describes the transition.
            Some(existing) if existing != candidate => return None,
            Some(_) => {}
        }
    }
    txid
}

/// Compute the commitment this CKB transaction should be committed to by.
///
/// The pre-image covers the RGB++ inputs and RGB++ outputs in transaction order, with
/// the Bitcoin txid in each output's lock args replaced by a placeholder — the
/// commitment is published in the very Bitcoin transaction those outputs will be
/// bound to, so its txid is not knowable at commitment time. See
/// [`rgbpp_types::commitment`] for the encoding and for why the result is recorded
/// rather than enforced.
///
/// Returns `None` if any output's args cannot be rewritten, since a partially
/// placeholdered pre-image would produce a confident-looking wrong answer.
pub fn expected_commitment(inputs: &[ResolvedInput], outputs: &[RgbppOutput]) -> Option<[u8; 32]> {
    if inputs.is_empty() && outputs.is_empty() {
        return None;
    }
    let input_points: Vec<CkbOutPoint> = inputs.iter().map(|i| i.out_point).collect();

    let mut output_pairs: Vec<(CellOutput, Vec<u8>)> = Vec::with_capacity(outputs.len());
    for output in outputs {
        let args = protocol::args_with_placeholder_txid(
            output.binding.kind(),
            output.cell.lock.args.as_slice(),
        )
        .ok()?;
        let mut committed = output.cell.clone();
        committed.lock.args = rgbpp_types::ckb::Bytes(args);
        output_pairs.push((committed, output.data.clone()));
    }

    Some(CommitmentPreimage::new(&input_points, &output_pairs).commitment())
}

/// Build the database row for a newly created RGB++ cell.
pub fn build_new_cell(
    assets: &AssetScripts,
    tx_hash: &H256,
    output: &RgbppOutput,
    block: &BlockContext,
) -> Result<NewCell> {
    let asset_kind = assets.classify(output.cell.type_.as_ref());
    let udt_amount = fungible_amount(asset_kind, &output.data)?;

    let (btc_vout, btc_time_after, target_lock_hash, target_lock_json) = match &output.binding {
        LockBinding::Rgbpp(args) => (Some(args.out_index as i32), None, None, None),
        LockBinding::BtcTime(args) => (
            None,
            Some(args.after as i32),
            Some(args.target_lock.calc_hash().to_vec()),
            Some(script_to_json(&args.target_lock)),
        ),
    };

    Ok(NewCell {
        ckb_tx_hash: tx_hash.to_vec(),
        output_index: output.index as i32,
        lock_kind: output.binding.kind(),
        btc_txid: output.binding.txid().to_display_vec(),
        btc_vout,
        btc_time_after,
        btc_time_target_lock_hash: target_lock_hash,
        btc_time_target_lock: target_lock_json,
        lock_hash: output.cell.lock.calc_hash().to_vec(),
        lock_args: output.cell.lock.args.as_slice().to_vec(),
        type_hash: output.cell.type_.as_ref().map(|t| t.calc_hash().to_vec()),
        type_script: output.cell.type_.as_ref().map(script_to_json),
        asset_kind,
        udt_amount,
        capacity: output.cell.capacity as i64,
        cell_data: output.data.clone(),
        created_block_number: block.number as i64,
        created_block_hash: block.hash.to_vec(),
        created_tx_index: block.tx_index as i32,
    })
}

fn fungible_amount(kind: AssetKind, data: &[u8]) -> Result<Option<BigDecimal>> {
    if !kind.is_fungible() {
        return Ok(None);
    }
    match asset::parse_udt_amount(data) {
        Some(amount) => Ok(Some(udt_amount_to_decimal(amount)?)),
        // A UDT cell with under 16 bytes of data is malformed rather than zero-valued;
        // recording NULL keeps it out of totals instead of understating them silently.
        None => Ok(None),
    }
}

pub fn script_to_json(script: &Script) -> serde_json::Value {
    serde_json::json!({
        "code_hash": script.code_hash.to_hex(),
        "hash_type": script.hash_type.as_str(),
        "args": script.args.to_hex(),
    })
}

/// Assemble everything one transaction contributes.
pub fn extract(
    protocol: &ProtocolScripts,
    assets: &AssetScripts,
    tx_hash: &H256,
    outputs: &[CellOutput],
    outputs_data: &[Vec<u8>],
    resolved_inputs: &[ResolvedInput],
    block: &BlockContext,
) -> Result<ExtractedTx> {
    let rgbpp_outputs = classify_outputs(protocol, outputs, outputs_data)?;

    if rgbpp_outputs.is_empty() && resolved_inputs.is_empty() {
        // The rich indexer matched this transaction, but nothing survived parsing.
        return Err(IndexerError::inconsistent(format!(
            "transaction {tx_hash} matched an RGB++ lock but has no readable RGB++ cells"
        )));
    }

    let cells = rgbpp_outputs
        .iter()
        .map(|output| build_new_cell(assets, tx_hash, output, block))
        .collect::<Result<Vec<_>>>()?;

    let spends = resolved_inputs
        .iter()
        .map(|input| CellSpend {
            ckb_tx_hash: input.out_point.tx_hash.to_vec(),
            output_index: input.out_point.index as i32,
            consumed_block_number: block.number as i64,
            consumed_block_hash: block.hash.to_vec(),
            consumed_tx_hash: tx_hash.to_vec(),
            consumed_tx_index: block.tx_index as i32,
            consumed_input_index: input.input_index as i32,
        })
        .collect();

    let kind = classify_transition(resolved_inputs, &rgbpp_outputs);
    let btc_txid = derive_btc_txid(&rgbpp_outputs).map(|t| t.to_display_vec());
    let commitment = expected_commitment(resolved_inputs, &rgbpp_outputs);

    let transition = NewTransition {
        ckb_tx_hash: tx_hash.to_vec(),
        block_number: block.number as i64,
        block_hash: block.hash.to_vec(),
        tx_index: block.tx_index as i32,
        block_timestamp: Some(block.timestamp),
        kind,
        btc_txid,
        input_cell_count: resolved_inputs.len() as i32,
        output_cell_count: rgbpp_outputs.len() as i32,
        expected_commitment: commitment.map(|c| c.to_vec()),
    };

    Ok(ExtractedTx {
        cells,
        spends,
        transition: Some(transition),
    })
}

/// Convenience for the scanner: the current time as a `DateTime<Utc>` from a CKB
/// millisecond timestamp.
pub fn ckb_timestamp(millis: u64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis as i64).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rgbpp_types::ckb::ScriptHashType;
    use rgbpp_types::protocol::{BtcTimeLockArgs, RgbppLockArgs, ScriptId};

    fn protocol() -> ProtocolScripts {
        ProtocolScripts {
            rgbpp_lock: ScriptId::new(
                H256::from_hex(
                    "0x1111111111111111111111111111111111111111111111111111111111111111",
                )
                .unwrap(),
                ScriptHashType::Type,
            ),
            btc_time_lock: ScriptId::new(
                H256::from_hex(
                    "0x2222222222222222222222222222222222222222222222222222222222222222",
                )
                .unwrap(),
                ScriptHashType::Type,
            ),
        }
    }

    fn txid(byte: u8) -> BtcTxid {
        BtcTxid::from_display_bytes([byte; 32])
    }

    fn rgbpp_cell(protocol: &ProtocolScripts, vout: u32, id: u8) -> CellOutput {
        CellOutput {
            capacity: 100 * 100_000_000,
            lock: Script::new(
                protocol.rgbpp_lock.code_hash,
                protocol.rgbpp_lock.hash_type,
                RgbppLockArgs {
                    out_index: vout,
                    txid: txid(id),
                }
                .encode(),
            ),
            type_: None,
        }
    }

    fn btc_time_cell(protocol: &ProtocolScripts, id: u8) -> CellOutput {
        CellOutput {
            capacity: 100,
            lock: Script::new(
                protocol.btc_time_lock.code_hash,
                protocol.btc_time_lock.hash_type,
                BtcTimeLockArgs {
                    target_lock: Script::new(H256::ZERO, ScriptHashType::Type, vec![1]),
                    after: 6,
                    txid: txid(id),
                }
                .encode(),
            ),
            type_: None,
        }
    }

    fn resolved(protocol: &ProtocolScripts, kind: LockKind, id: u8) -> ResolvedInput {
        let cell = match kind {
            LockKind::Rgbpp => rgbpp_cell(protocol, 0, id),
            LockKind::BtcTime => btc_time_cell(protocol, id),
        };
        ResolvedInput {
            input_index: 0,
            out_point: CkbOutPoint::new(H256::ZERO, 0),
            binding: LockBinding::parse(kind, cell.lock.args.as_slice()).unwrap(),
            cell,
            data: vec![],
        }
    }

    #[test]
    fn outputs_under_other_locks_are_ignored() {
        let protocol = protocol();
        let outputs = vec![
            rgbpp_cell(&protocol, 0, 1),
            CellOutput {
                capacity: 1,
                lock: Script::new(H256::ZERO, ScriptHashType::Data, vec![]),
                type_: None,
            },
        ];
        let found = classify_outputs(&protocol, &outputs, &[vec![], vec![]]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].index, 0);
    }

    #[test]
    fn malformed_args_do_not_abort_the_transaction() {
        let protocol = protocol();
        let mut bad = rgbpp_cell(&protocol, 0, 1);
        bad.lock.args = rgbpp_types::ckb::Bytes(vec![0u8; 10]); // not 36 bytes
        let outputs = vec![bad, rgbpp_cell(&protocol, 1, 1)];
        let found = classify_outputs(&protocol, &outputs, &[vec![], vec![]]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].index, 1);
    }

    #[test]
    fn transition_kinds_cover_the_protocol_shapes() {
        let protocol = protocol();
        let rgbpp_out =
            classify_outputs(&protocol, &[rgbpp_cell(&protocol, 0, 1)], &[vec![]]).unwrap();
        let time_out =
            classify_outputs(&protocol, &[btc_time_cell(&protocol, 1)], &[vec![]]).unwrap();

        assert_eq!(
            classify_transition(&[], &rgbpp_out),
            TransitionKind::Issuance
        );
        assert_eq!(
            classify_transition(&[resolved(&protocol, LockKind::Rgbpp, 1)], &rgbpp_out),
            TransitionKind::Transfer
        );
        assert_eq!(
            classify_transition(&[resolved(&protocol, LockKind::Rgbpp, 1)], &time_out),
            TransitionKind::LeapToCkb
        );
        assert_eq!(
            classify_transition(&[resolved(&protocol, LockKind::BtcTime, 1)], &[]),
            TransitionKind::BtcTimeUnlock
        );
        assert_eq!(
            classify_transition(&[resolved(&protocol, LockKind::Rgbpp, 1)], &[]),
            TransitionKind::Exit
        );
    }

    #[test]
    fn btc_txid_comes_from_outputs_and_needs_agreement() {
        let protocol = protocol();
        let same = classify_outputs(
            &protocol,
            &[rgbpp_cell(&protocol, 0, 7), rgbpp_cell(&protocol, 1, 7)],
            &[vec![], vec![]],
        )
        .unwrap();
        assert_eq!(derive_btc_txid(&same), Some(txid(7)));

        let mixed = classify_outputs(
            &protocol,
            &[rgbpp_cell(&protocol, 0, 7), rgbpp_cell(&protocol, 0, 8)],
            &[vec![], vec![]],
        )
        .unwrap();
        assert_eq!(derive_btc_txid(&mixed), None);
        assert_eq!(derive_btc_txid(&[]), None);
    }

    #[test]
    fn extract_produces_cells_spends_and_a_transition() {
        let protocol = protocol();
        let assets = AssetScripts::default();
        let block = BlockContext {
            number: 1_000,
            hash: H256::from_hex(
                "0x3333333333333333333333333333333333333333333333333333333333333333",
            )
            .unwrap(),
            timestamp: Utc::now(),
            tx_index: 4,
        };
        let outputs = vec![rgbpp_cell(&protocol, 0, 9)];
        let inputs = vec![ResolvedInput {
            input_index: 2,
            out_point: CkbOutPoint::new(H256::ZERO, 5),
            ..resolved(&protocol, LockKind::Rgbpp, 8)
        }];

        let extracted = extract(
            &protocol,
            &assets,
            &H256::from_hex("0x4444444444444444444444444444444444444444444444444444444444444444")
                .unwrap(),
            &outputs,
            &[vec![]],
            &inputs,
            &block,
        )
        .unwrap();

        assert_eq!(extracted.cells.len(), 1);
        assert_eq!(extracted.cells[0].created_block_number, 1_000);
        assert_eq!(extracted.cells[0].btc_vout, Some(0));

        assert_eq!(extracted.spends.len(), 1);
        assert_eq!(extracted.spends[0].output_index, 5);
        assert_eq!(extracted.spends[0].consumed_input_index, 2);
        assert_eq!(extracted.spends[0].consumed_block_number, 1_000);

        let transition = extracted.transition.unwrap();
        assert_eq!(transition.kind, TransitionKind::Transfer);
        assert_eq!(transition.btc_txid, Some(txid(9).to_display_vec()));
        assert!(transition.expected_commitment.is_some());
    }

    #[test]
    fn a_transaction_with_nothing_readable_is_an_error() {
        let protocol = protocol();
        let err = extract(
            &protocol,
            &AssetScripts::default(),
            &H256::ZERO,
            &[],
            &[],
            &[],
            &BlockContext {
                number: 1,
                hash: H256::ZERO,
                timestamp: Utc::now(),
                tx_index: 0,
            },
        );
        assert!(err.is_err());
    }
}
