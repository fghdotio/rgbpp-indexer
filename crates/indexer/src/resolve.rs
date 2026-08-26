//! Reconstructing consumed cells.
//!
//! When a transaction spends an RGB++ cell, the indexer normally already holds that
//! cell — it indexed the transaction that created it. Rebuilding the `CellOutput`
//! from the stored row avoids an RPC round trip per input, which matters a great deal
//! during initial sync.
//!
//! The fallback path exists for the genuine gap: a cell created before `start_block`,
//! or before a database was rebuilt. Those are fetched from the node and backfilled,
//! so the spend has something to attach to.

use rgbpp_store::models::CellRow;
use rgbpp_types::ckb::{Bytes, CellOutput, Script, ScriptHashType, H256};
use rgbpp_types::protocol::{LockBinding, LockKind, ProtocolScripts};

use crate::error::{IndexerError, Result};

/// Position recorded for a cell that was backfilled from the node.
///
/// The node's `get_transaction` does not report where in its block the transaction
/// sat, and fetching the whole block just to learn that is not worth it. `-1` marks
/// the position as unknown rather than pretending it was first.
pub const UNKNOWN_TX_INDEX: i32 = -1;

pub fn script_from_json(value: &serde_json::Value) -> Result<Script> {
    let code_hash = value
        .get("code_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| IndexerError::inconsistent("stored script has no code_hash"))?;
    let hash_type = value
        .get("hash_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| IndexerError::inconsistent("stored script has no hash_type"))?;
    let args = value.get("args").and_then(|v| v.as_str()).unwrap_or("0x");

    Ok(Script {
        code_hash: H256::from_hex(code_hash)?,
        hash_type: ScriptHashType::from_str_opt(hash_type).ok_or_else(|| {
            IndexerError::inconsistent(format!("stored script has unknown hash_type {hash_type}"))
        })?,
        args: Bytes::from_hex(args)?,
    })
}

/// Rebuild the on-chain cell from a stored row.
///
/// The lock's code hash is not stored per row — it is implied by `lock_kind`, which
/// is why the protocol configuration has to be passed in. That also means a row
/// written under a previous deployment's code hash would rebuild incorrectly, so the
/// reconstructed lock hash is checked against the stored one.
pub fn cell_output_from_row(protocol: &ProtocolScripts, row: &CellRow) -> Result<CellOutput> {
    let kind = row.lock_kind()?;
    let script_id = match kind {
        LockKind::Rgbpp => protocol.rgbpp_lock,
        LockKind::BtcTime => protocol.btc_time_lock,
    };
    let lock = Script::new(
        script_id.code_hash,
        script_id.hash_type,
        row.lock_args.clone(),
    );

    if lock.calc_hash().to_vec() != row.lock_hash {
        return Err(IndexerError::inconsistent(format!(
            "stored cell {}:{} was indexed under a different {} deployment; \
             its lock hash does not match the configured code hash",
            hex::encode(&row.ckb_tx_hash),
            row.output_index,
            kind.as_str()
        )));
    }

    let type_ = row.type_script.as_ref().map(script_from_json).transpose()?;

    Ok(CellOutput {
        capacity: row.capacity as u64,
        lock,
        type_,
    })
}

pub fn binding_from_row(row: &CellRow) -> Result<LockBinding> {
    let kind = row.lock_kind()?;
    Ok(LockBinding::parse(kind, &row.lock_args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::extract::script_to_json;

    #[test]
    fn script_json_round_trips() {
        let script = Script::new(
            H256::from_hex("0x5555555555555555555555555555555555555555555555555555555555555555")
                .unwrap(),
            ScriptHashType::Data1,
            vec![0xde, 0xad],
        );
        let json = script_to_json(&script);
        assert_eq!(script_from_json(&json).unwrap(), script);
    }

    #[test]
    fn empty_args_round_trip() {
        let script = Script::new(H256::ZERO, ScriptHashType::Type, vec![]);
        assert_eq!(script_from_json(&script_to_json(&script)).unwrap(), script);
    }
}
