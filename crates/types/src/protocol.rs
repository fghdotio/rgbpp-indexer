//! RGB++ protocol decoding: which CKB locks matter, and what their args mean.
//!
//! Two lock scripts define the protocol surface an indexer must watch:
//!
//! * **RGB++ lock** — `args = out_index (u32le) || btc_txid (32 bytes, internal
//!   order)`. A cell under this lock is *owned by* the referenced Bitcoin UTXO;
//!   spending the UTXO on Bitcoin is what authorises spending the cell on CKB.
//! * **BTC time lock** — `args = molecule BTCTimeLock { lock_script: Script,
//!   after: Uint32, btc_txid: Byte32 }`. This is the landing zone for a "leap to
//!   CKB": the cell becomes spendable under `lock_script` once `btc_txid` has
//!   `after` confirmations.

use serde::{Deserialize, Serialize};

use crate::bitcoin::{BtcOutPoint, BtcTxid};
use crate::ckb::{H256, Script, ScriptHashType};
use crate::error::{Error, Result};
use crate::molecule;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    /// Cell owned by a Bitcoin UTXO.
    Rgbpp,
    /// Cell waiting out Bitcoin confirmations before unlocking to its target lock.
    BtcTime,
}

impl LockKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LockKind::Rgbpp => "rgbpp",
            LockKind::BtcTime => "btc_time",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "rgbpp" => Some(LockKind::Rgbpp),
            "btc_time" => Some(LockKind::BtcTime),
            _ => None,
        }
    }
}

/// A code hash + hash type pair: the "which contract is this" half of a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScriptId {
    pub code_hash: H256,
    pub hash_type: ScriptHashType,
}

impl ScriptId {
    pub fn new(code_hash: H256, hash_type: ScriptHashType) -> Self {
        ScriptId {
            code_hash,
            hash_type,
        }
    }

    pub fn matches(&self, script: &Script) -> bool {
        script.code_hash == self.code_hash && script.hash_type == self.hash_type
    }
}

/// The set of scripts that define RGB++ on a given network.
///
/// Deployed code hashes differ between mainnet and testnet and can change when the
/// contracts are redeployed, so these always come from configuration rather than
/// being compiled in as constants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolScripts {
    pub rgbpp_lock: ScriptId,
    pub btc_time_lock: ScriptId,
}

impl ProtocolScripts {
    /// Classify a lock script. Returns `None` for everything that is not RGB++.
    pub fn classify(&self, lock: &Script) -> Option<LockKind> {
        if self.rgbpp_lock.matches(lock) {
            Some(LockKind::Rgbpp)
        } else if self.btc_time_lock.matches(lock) {
            Some(LockKind::BtcTime)
        } else {
            None
        }
    }
}

/// Decoded `args` of an RGB++ lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbppLockArgs {
    pub out_index: u32,
    pub txid: BtcTxid,
}

impl RgbppLockArgs {
    pub const LEN: usize = 36;

    pub fn parse(args: &[u8]) -> Result<Self> {
        if args.len() != Self::LEN {
            return Err(Error::Length {
                what: "rgbpp lock args",
                expected: Self::LEN,
                got: args.len(),
            });
        }
        let out_index = u32::from_le_bytes(args[0..4].try_into().unwrap());
        // The script embeds the txid in consensus (internal) order.
        let txid = BtcTxid::from_internal_bytes(&args[4..36])?;
        Ok(RgbppLockArgs { out_index, txid })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.out_index.to_le_bytes());
        out.extend_from_slice(&self.txid.to_internal_bytes());
        out
    }

    pub fn out_point(&self) -> BtcOutPoint {
        BtcOutPoint::new(self.txid, self.out_index)
    }
}

/// Decoded `args` of a BTC time lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtcTimeLockArgs {
    /// Lock the cell falls back to once the confirmation requirement is met.
    pub target_lock: Script,
    /// Required confirmations of `txid`.
    pub after: u32,
    pub txid: BtcTxid,
}

impl BtcTimeLockArgs {
    pub fn parse(args: &[u8]) -> Result<Self> {
        let fields = molecule::read_table_fields(args)?;
        if fields.len() != 3 {
            return Err(Error::malformed(
                "btc time lock args",
                format!("expected 3 fields, got {}", fields.len()),
            ));
        }
        let target_lock = molecule::decode_script(fields[0])?;
        if fields[1].len() != 4 {
            return Err(Error::malformed(
                "btc time lock args",
                "`after` is not a Uint32",
            ));
        }
        let after = u32::from_le_bytes(fields[1].try_into().unwrap());
        let txid = BtcTxid::from_internal_bytes(fields[2])?;
        Ok(BtcTimeLockArgs {
            target_lock,
            after,
            txid,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let fields = vec![
            molecule::encode_script(&self.target_lock),
            self.after.to_le_bytes().to_vec(),
            self.txid.to_internal_bytes().to_vec(),
        ];
        // Re-use the table encoder via a Script-shaped call would be wrong; build here.
        let header_len = 4 + 4 * fields.len();
        let body_len: usize = fields.iter().map(|f| f.len()).sum();
        let full_size = header_len + body_len;
        let mut out = Vec::with_capacity(full_size);
        out.extend_from_slice(&(full_size as u32).to_le_bytes());
        let mut offset = header_len;
        for f in &fields {
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += f.len();
        }
        for f in &fields {
            out.extend_from_slice(f);
        }
        out
    }
}

/// Rewrite a lock's args with a zeroed Bitcoin txid.
///
/// This is what an RGB++ commitment is computed over, and the reason is a
/// chicken-and-egg: the output cells of a CKB transaction are bound to *outputs of
/// the Bitcoin transaction that is still being built*, so its txid does not exist
/// yet when the commitment goes into that transaction's `OP_RETURN`. The committed
/// pre-image therefore carries a placeholder in the txid position, and the real txid
/// is only filled in once the Bitcoin transaction is finalised.
///
/// Both protocol locks are affected: the RGB++ lock's trailing 32 bytes, and the BTC
/// time lock's `btc_txid` field.
pub fn args_with_placeholder_txid(kind: LockKind, args: &[u8]) -> Result<Vec<u8>> {
    let placeholder = BtcTxid::from_display_bytes([0u8; 32]);
    match kind {
        LockKind::Rgbpp => {
            let mut parsed = RgbppLockArgs::parse(args)?;
            parsed.txid = placeholder;
            Ok(parsed.encode())
        }
        LockKind::BtcTime => {
            let mut parsed = BtcTimeLockArgs::parse(args)?;
            parsed.txid = placeholder;
            Ok(parsed.encode())
        }
    }
}

/// What a matched cell's lock tells us about its Bitcoin binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockBinding {
    /// Owned by a specific Bitcoin UTXO — this is what drives spend detection.
    Rgbpp(RgbppLockArgs),
    /// Waiting on confirmations of a Bitcoin transaction; not UTXO-owned.
    BtcTime(BtcTimeLockArgs),
}

impl LockBinding {
    pub fn kind(&self) -> LockKind {
        match self {
            LockBinding::Rgbpp(_) => LockKind::Rgbpp,
            LockBinding::BtcTime(_) => LockKind::BtcTime,
        }
    }

    pub fn txid(&self) -> BtcTxid {
        match self {
            LockBinding::Rgbpp(a) => a.txid,
            LockBinding::BtcTime(a) => a.txid,
        }
    }

    /// Only RGB++ locks name a UTXO; BTC time locks reference a transaction.
    pub fn out_point(&self) -> Option<BtcOutPoint> {
        match self {
            LockBinding::Rgbpp(a) => Some(a.out_point()),
            LockBinding::BtcTime(_) => None,
        }
    }

    pub fn parse(kind: LockKind, args: &[u8]) -> Result<Self> {
        match kind {
            LockKind::Rgbpp => Ok(LockBinding::Rgbpp(RgbppLockArgs::parse(args)?)),
            LockKind::BtcTime => Ok(LockBinding::BtcTime(BtcTimeLockArgs::parse(args)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_txid() -> BtcTxid {
        BtcTxid::from_hex("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b")
            .unwrap()
    }

    #[test]
    fn rgbpp_args_roundtrip() {
        let args = RgbppLockArgs {
            out_index: 3,
            txid: sample_txid(),
        };
        let encoded = args.encode();
        assert_eq!(encoded.len(), 36);
        assert_eq!(&encoded[0..4], &[3, 0, 0, 0]);
        // Byte 4 is the *last* byte of the displayed txid, because the script stores
        // it in internal order.
        assert_eq!(encoded[4], 0x3b);
        assert_eq!(RgbppLockArgs::parse(&encoded).unwrap(), args);
    }

    #[test]
    fn rgbpp_args_reject_wrong_length() {
        assert!(RgbppLockArgs::parse(&[0u8; 35]).is_err());
        assert!(RgbppLockArgs::parse(&[0u8; 37]).is_err());
    }

    #[test]
    fn placeholder_only_clears_the_txid() {
        let args = RgbppLockArgs {
            out_index: 5,
            txid: sample_txid(),
        };
        let zeroed = args_with_placeholder_txid(LockKind::Rgbpp, &args.encode()).unwrap();
        let parsed = RgbppLockArgs::parse(&zeroed).unwrap();
        assert_eq!(parsed.out_index, 5, "the output index is part of the commitment");
        assert_eq!(parsed.txid, BtcTxid::from_display_bytes([0u8; 32]));

        let time_args = BtcTimeLockArgs {
            target_lock: Script::new(H256::ZERO, ScriptHashType::Type, vec![9, 9]),
            after: 6,
            txid: sample_txid(),
        };
        let zeroed = args_with_placeholder_txid(LockKind::BtcTime, &time_args.encode()).unwrap();
        let parsed = BtcTimeLockArgs::parse(&zeroed).unwrap();
        assert_eq!(parsed.after, 6);
        assert_eq!(parsed.target_lock, time_args.target_lock);
        assert_eq!(parsed.txid, BtcTxid::from_display_bytes([0u8; 32]));
    }

    #[test]
    fn btc_time_args_roundtrip() {
        let args = BtcTimeLockArgs {
            target_lock: Script::new(H256::ZERO, ScriptHashType::Type, vec![7, 7]),
            after: 6,
            txid: sample_txid(),
        };
        let encoded = args.encode();
        assert_eq!(BtcTimeLockArgs::parse(&encoded).unwrap(), args);
    }
}
