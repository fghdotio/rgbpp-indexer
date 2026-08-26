//! Asset recognition.
//!
//! The indexer's job is RGB++ *ownership*, not asset semantics, so this layer stays
//! deliberately thin: identify the type script well enough to group and total
//! balances, and decode the one field (`u128` amount) that every UDT flavour shares.
//! Anything richer — Spore content, token metadata — is left to consumers, which can
//! read the raw type script and cell data the indexer stores.

use serde::{Deserialize, Serialize};

use crate::ckb::{Script, H256};
use crate::protocol::ScriptId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    /// Extensible UDT — the usual RGB++ fungible token.
    Xudt,
    /// Simple UDT.
    Sudt,
    /// Spore DOB / NFT.
    Spore,
    SporeCluster,
    /// Cell has a type script we do not recognise.
    Unknown,
    /// Cell has no type script: plain CKBytes under an RGB++ lock.
    None,
}

impl AssetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AssetKind::Xudt => "xudt",
            AssetKind::Sudt => "sudt",
            AssetKind::Spore => "spore",
            AssetKind::SporeCluster => "spore_cluster",
            AssetKind::Unknown => "unknown",
            AssetKind::None => "none",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "xudt" => Some(AssetKind::Xudt),
            "sudt" => Some(AssetKind::Sudt),
            "spore" => Some(AssetKind::Spore),
            "spore_cluster" => Some(AssetKind::SporeCluster),
            "unknown" => Some(AssetKind::Unknown),
            "none" => Some(AssetKind::None),
            _ => None,
        }
    }

    /// Whether cell data carries a `u128` amount in its first 16 bytes.
    pub fn is_fungible(&self) -> bool {
        matches!(self, AssetKind::Xudt | AssetKind::Sudt)
    }
}

/// Type-script code hashes the indexer knows how to label, from configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetScripts {
    #[serde(default)]
    pub xudt: Vec<ScriptId>,
    #[serde(default)]
    pub sudt: Vec<ScriptId>,
    #[serde(default)]
    pub spore: Vec<ScriptId>,
    #[serde(default)]
    pub spore_cluster: Vec<ScriptId>,
}

impl AssetScripts {
    pub fn classify(&self, type_script: Option<&Script>) -> AssetKind {
        let Some(script) = type_script else {
            return AssetKind::None;
        };
        for (ids, kind) in [
            (&self.xudt, AssetKind::Xudt),
            (&self.sudt, AssetKind::Sudt),
            (&self.spore, AssetKind::Spore),
            (&self.spore_cluster, AssetKind::SporeCluster),
        ] {
            if ids.iter().any(|id| id.matches(script)) {
                return kind;
            }
        }
        AssetKind::Unknown
    }
}

/// UDT cells store the amount as a little-endian `u128` in the first 16 bytes.
pub fn parse_udt_amount(data: &[u8]) -> Option<u128> {
    if data.len() < 16 {
        return None;
    }
    Some(u128::from_le_bytes(
        data[0..16].try_into().expect("checked length"),
    ))
}

/// The identity used to group balances: the type script hash, or `None` for plain
/// capacity-only cells.
pub fn asset_id(type_script: Option<&Script>) -> Option<H256> {
    type_script.map(|s| s.calc_hash())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ckb::ScriptHashType;

    #[test]
    fn udt_amount_reads_little_endian_prefix() {
        let mut data = 1_234_567_890u128.to_le_bytes().to_vec();
        data.extend_from_slice(b"extension data is ignored");
        assert_eq!(parse_udt_amount(&data), Some(1_234_567_890));
        assert_eq!(parse_udt_amount(&[0u8; 15]), None);
    }

    #[test]
    fn classification_falls_through_to_unknown() {
        let code =
            H256::from_hex("0x1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let scripts = AssetScripts {
            xudt: vec![ScriptId::new(code, ScriptHashType::Data1)],
            ..Default::default()
        };
        let xudt = Script::new(code, ScriptHashType::Data1, vec![]);
        let other = Script::new(H256::ZERO, ScriptHashType::Type, vec![]);

        assert_eq!(scripts.classify(Some(&xudt)), AssetKind::Xudt);
        assert_eq!(scripts.classify(Some(&other)), AssetKind::Unknown);
        assert_eq!(scripts.classify(None), AssetKind::None);
    }
}
