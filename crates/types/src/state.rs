//! The status model.
//!
//! Statuses are *derived*, never stored as an authoritative column. Two independent
//! facts feed every answer:
//!
//! 1. **CKB fact** — has the cell been consumed in a block we have indexed? This is
//!    ordered and dependency-bearing, so it is what a reorg rolls back.
//! 2. **Bitcoin observation** — is the bound UTXO still unspent? This is a cache of
//!    a re-queryable question, so it never needs rollback, only invalidation.
//!
//! The interesting states live in the gap between them: a Bitcoin transaction can be
//! broadcast (or even confirmed) long before the matching CKB transaction shows up
//! in the indexed range, which is exactly the window `PendingCkb` describes. That
//! window is widened deliberately by `REORG_LAG`, so on-demand refresh is not an
//! optimisation here — it is how applications see fresh state at all.

use serde::{Deserialize, Serialize};

use crate::bitcoin::BtcTxid;

/// What the Bitcoin data source last told us about a bound UTXO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum OutpointSpendStatus {
    /// Never observed, or the observation has been invalidated.
    Unknown,
    Unspent,
    /// Spent by a transaction still in the mempool.
    SpentUnconfirmed {
        spender: BtcTxid,
    },
    SpentConfirmed {
        spender: BtcTxid,
        height: u32,
    },
}

impl OutpointSpendStatus {
    pub fn is_spent(&self) -> bool {
        matches!(
            self,
            OutpointSpendStatus::SpentUnconfirmed { .. }
                | OutpointSpendStatus::SpentConfirmed { .. }
        )
    }

    pub fn spender(&self) -> Option<BtcTxid> {
        match self {
            OutpointSpendStatus::SpentUnconfirmed { spender }
            | OutpointSpendStatus::SpentConfirmed { spender, .. } => Some(*spender),
            _ => None,
        }
    }

    pub fn height(&self) -> Option<u32> {
        match self {
            OutpointSpendStatus::SpentConfirmed { height, .. } => Some(*height),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            OutpointSpendStatus::Unknown => "unknown",
            OutpointSpendStatus::Unspent => "unspent",
            OutpointSpendStatus::SpentUnconfirmed { .. } => "spent_unconfirmed",
            OutpointSpendStatus::SpentConfirmed { .. } => "spent_confirmed",
        }
    }
}

/// The answer an application actually wants about an RGB++ cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    /// Unconsumed on CKB, and the bound UTXO is still unspent (or unobserved).
    Live,
    /// The bound UTXO has been spent on Bitcoin, but the CKB transaction that
    /// completes the transition is not in the indexed range yet. Either it has not
    /// been committed, or it is inside the `REORG_LAG` window.
    PendingCkb,
    /// Consumed by a CKB transaction inside the indexed range.
    Spent,
    /// A BTC time lock cell whose confirmation requirement is not met yet.
    TimeLocked,
}

impl CellStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CellStatus::Live => "live",
            CellStatus::PendingCkb => "pending_ckb",
            CellStatus::Spent => "spent",
            CellStatus::TimeLocked => "time_locked",
        }
    }
}

/// Derive a cell's status from the two independent facts.
pub fn derive_cell_status(consumed_on_ckb: bool, btc: OutpointSpendStatus) -> CellStatus {
    if consumed_on_ckb {
        CellStatus::Spent
    } else if btc.is_spent() {
        CellStatus::PendingCkb
    } else {
        CellStatus::Live
    }
}

/// What a CKB transaction did to RGB++ state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Created RGB++ cells without consuming any (issuance / leap from CKB).
    Issuance,
    /// Consumed and re-created RGB++ cells — an ordinary Bitcoin-side transfer.
    Transfer,
    /// Consumed RGB++ cells into BTC time lock cells (leap to CKB).
    LeapToCkb,
    /// Consumed BTC time lock cells after their confirmation requirement.
    BtcTimeUnlock,
    /// Consumed RGB++ cells with no RGB++ output — exits the protocol surface.
    Exit,
}

impl TransitionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransitionKind::Issuance => "issuance",
            TransitionKind::Transfer => "transfer",
            TransitionKind::LeapToCkb => "leap_to_ckb",
            TransitionKind::BtcTimeUnlock => "btc_time_unlock",
            TransitionKind::Exit => "exit",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "issuance" => Some(TransitionKind::Issuance),
            "transfer" => Some(TransitionKind::Transfer),
            "leap_to_ckb" => Some(TransitionKind::LeapToCkb),
            "btc_time_unlock" => Some(TransitionKind::BtcTimeUnlock),
            "exit" => Some(TransitionKind::Exit),
            _ => None,
        }
    }
}

/// Why a bound outpoint was flagged for a human to look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyKind {
    /// The bound UTXO was spent on Bitcoin and stayed spent for a long time without a
    /// matching CKB transition — the "accidentally spent an RGB++ UTXO" case.
    BtcSpentWithoutCkb,
    /// The CKB transition's commitment does not match the one published on Bitcoin.
    CommitmentMismatch,
    /// A CKB transition claims a Bitcoin txid that the data source cannot find.
    UnknownBtcTx,
    /// The bound UTXO was spent by a transaction that carries no RGB++ commitment.
    NoCommitmentInSpender,
}

impl AnomalyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnomalyKind::BtcSpentWithoutCkb => "btc_spent_without_ckb",
            AnomalyKind::CommitmentMismatch => "commitment_mismatch",
            AnomalyKind::UnknownBtcTx => "unknown_btc_tx",
            AnomalyKind::NoCommitmentInSpender => "no_commitment_in_spender",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txid() -> BtcTxid {
        BtcTxid::from_hex("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b")
            .unwrap()
    }

    #[test]
    fn ckb_consumption_wins_over_any_btc_observation() {
        for btc in [
            OutpointSpendStatus::Unknown,
            OutpointSpendStatus::Unspent,
            OutpointSpendStatus::SpentUnconfirmed { spender: txid() },
        ] {
            assert_eq!(derive_cell_status(true, btc), CellStatus::Spent);
        }
    }

    #[test]
    fn btc_spend_without_ckb_is_the_pending_window() {
        assert_eq!(
            derive_cell_status(
                false,
                OutpointSpendStatus::SpentUnconfirmed { spender: txid() }
            ),
            CellStatus::PendingCkb
        );
        assert_eq!(
            derive_cell_status(
                false,
                OutpointSpendStatus::SpentConfirmed {
                    spender: txid(),
                    height: 900_000
                }
            ),
            CellStatus::PendingCkb
        );
    }

    #[test]
    fn unobserved_utxo_reads_as_live() {
        assert_eq!(
            derive_cell_status(false, OutpointSpendStatus::Unknown),
            CellStatus::Live
        );
    }
}
