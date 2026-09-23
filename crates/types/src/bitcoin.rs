//! Bitcoin primitives, with one rule about byte order.
//!
//! `BtcTxid` stores **display order** — what explorers show. The reversal to
//! consensus order happens only in `to_internal_bytes` / `from_internal_bytes`, which
//! is what RGB++ lock args use. Mixing the two is the classic "the indexer sees
//! nothing" bug.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BtcTxid([u8; 32]);

impl BtcTxid {
    /// Bytes exactly as displayed (`0x`-less hex reads left to right).
    pub fn from_display_bytes(bytes: [u8; 32]) -> Self {
        BtcTxid(bytes)
    }

    pub fn from_display_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(Error::Length {
                what: "BtcTxid",
                expected: 32,
                got: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(BtcTxid(out))
    }

    /// Parse the hex form used by explorers and Bitcoin RPC (`0x` prefix optional).
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != 64 {
            return Err(Error::Length {
                what: "BtcTxid hex",
                expected: 64,
                got: s.len(),
            });
        }
        Self::from_display_slice(&hex::decode(s)?)
    }

    /// Consensus / little-endian order, as embedded in scripts and raw transactions.
    pub fn from_internal_bytes(bytes: &[u8]) -> Result<Self> {
        let mut v = bytes.to_vec();
        v.reverse();
        Self::from_display_slice(&v)
    }

    pub fn as_display_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_display_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    pub fn to_internal_bytes(&self) -> [u8; 32] {
        let mut out = self.0;
        out.reverse();
        out
    }

    /// Hex without `0x`, matching every Bitcoin explorer and REST API.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for BtcTxid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BtcTxid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BtcTxid({})", self.to_hex())
    }
}

impl Serialize for BtcTxid {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for BtcTxid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        BtcTxid::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A Bitcoin UTXO reference — the anchor an RGB++ cell is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BtcOutPoint {
    pub txid: BtcTxid,
    pub vout: u32,
}

impl BtcOutPoint {
    pub fn new(txid: BtcTxid, vout: u32) -> Self {
        BtcOutPoint { txid, vout }
    }
}

impl fmt::Display for BtcOutPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.txid, self.vout)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BtcBlockHash([u8; 32]);

impl BtcBlockHash {
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(Error::Length {
                what: "BtcBlockHash",
                expected: 32,
                got: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(BtcBlockHash(out))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Display for BtcBlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BtcBlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BtcBlockHash({})", self.to_hex())
    }
}

impl Serialize for BtcBlockHash {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for BtcBlockHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        BtcBlockHash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Which Bitcoin network addresses are checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtcNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl BtcNetwork {
    /// Read the labels deployments already put in `general.network`.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.to_ascii_lowercase().as_str() {
            "mainnet" | "main" | "bitcoin" => Some(BtcNetwork::Mainnet),
            "testnet" | "testnet3" | "testnet4" => Some(BtcNetwork::Testnet),
            "signet" => Some(BtcNetwork::Signet),
            "regtest" => Some(BtcNetwork::Regtest),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BtcNetwork::Mainnet => "mainnet",
            BtcNetwork::Testnet => "testnet",
            BtcNetwork::Signet => "signet",
            BtcNetwork::Regtest => "regtest",
        }
    }

    /// Testnet and signet share address formats, so an address alone cannot tell
    /// them apart; both accept the other's addresses.
    fn address_family(self) -> AddressFamily {
        match self {
            BtcNetwork::Mainnet => AddressFamily::Main,
            BtcNetwork::Testnet | BtcNetwork::Signet => AddressFamily::Test,
            BtcNetwork::Regtest => AddressFamily::Regtest,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddressFamily {
    Main,
    Test,
    Regtest,
}

impl AddressFamily {
    fn label(self) -> &'static str {
        match self {
            AddressFamily::Main => "mainnet",
            AddressFamily::Test => "testnet or signet",
            AddressFamily::Regtest => "regtest",
        }
    }

    fn of_segwit_hrp(hrp: &bech32::Hrp) -> Option<Self> {
        if *hrp == bech32::hrp::BC {
            Some(AddressFamily::Main)
        } else if *hrp == bech32::hrp::TB {
            Some(AddressFamily::Test)
        } else if *hrp == bech32::hrp::BCRT {
            Some(AddressFamily::Regtest)
        } else {
            None
        }
    }

    /// P2PKH and P2SH version bytes. Regtest reuses testnet's.
    fn of_base58_version(version: u8) -> Option<&'static [AddressFamily]> {
        match version {
            0x00 | 0x05 => Some(&[AddressFamily::Main]),
            0x6f | 0xc4 => Some(&[AddressFamily::Test, AddressFamily::Regtest]),
            _ => None,
        }
    }
}

/// Longer than any Bitcoin address: bech32 is capped at 90 characters, base58
/// addresses are 34 or 35. Checked first because base58 decoding is quadratic in
/// its input.
const MAX_ADDRESS_LEN: usize = 90;

/// Check that `address` is a well-formed Bitcoin address on `network`.
///
/// Segwit addresses are decoded per BIP-173 and BIP-350 — bech32 for version 0,
/// bech32m for later versions, with program length rules — and legacy addresses as
/// base58check P2PKH or P2SH.
///
/// Callers pass addresses straight from a URL, and they end up in a URL path on the
/// Bitcoin data source. Validating here is what keeps `%2F..` in an "address" from
/// steering that request to another path on the data source's host.
pub fn validate_address(address: &str, network: BtcNetwork) -> Result<()> {
    let invalid = |reason: String| Error::malformed("bitcoin address", reason);
    let expected = network.address_family();

    if address.len() > MAX_ADDRESS_LEN {
        return Err(invalid("longer than any Bitcoin address".into()));
    }

    if let Ok((hrp, _version, _program)) = bech32::segwit::decode(address) {
        return match AddressFamily::of_segwit_hrp(&hrp) {
            Some(family) if family == expected => Ok(()),
            Some(family) => Err(invalid(format!(
                "a {} address, but this index follows {}",
                family.label(),
                network.as_str()
            ))),
            None => Err(invalid(format!("`{hrp}` is not a Bitcoin address prefix"))),
        };
    }

    if let Ok(payload) = bs58::decode(address).with_check(None).into_vec() {
        if payload.len() != 21 {
            return Err(invalid(format!(
                "base58check payload is {} bytes, not 21",
                payload.len()
            )));
        }
        return match AddressFamily::of_base58_version(payload[0]) {
            Some(families) if families.contains(&expected) => Ok(()),
            Some(families) => Err(invalid(format!(
                "a {} address, but this index follows {}",
                families[0].label(),
                network.as_str()
            ))),
            None => Err(invalid(format!(
                "unknown address version byte 0x{:02x}",
                payload[0]
            ))),
        };
    }

    Err(invalid(
        "not a valid segwit (bech32/bech32m) or base58check address".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txid_byte_order() {
        let txid =
            BtcTxid::from_hex("4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b")
                .unwrap();
        let internal = txid.to_internal_bytes();
        assert_eq!(internal[0], 0x3b);
        assert_eq!(BtcTxid::from_internal_bytes(&internal).unwrap(), txid);
    }

    mod addresses {
        use super::super::*;

        fn ok(address: &str, network: BtcNetwork) {
            if let Err(e) = validate_address(address, network) {
                panic!("{address} rejected on {network:?}: {e}");
            }
        }

        fn rejected(address: &str, network: BtcNetwork) -> String {
            match validate_address(address, network) {
                Ok(()) => panic!("{address} accepted on {network:?}"),
                Err(e) => e.to_string(),
            }
        }

        #[test]
        fn bip_vectors() {
            // BIP-173 and BIP-350 valid addresses, as used by the bech32 crate's suite.
            ok(
                "BC1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KV8F3T4",
                BtcNetwork::Mainnet,
            );
            ok(
                "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0",
                BtcNetwork::Mainnet,
            );
            ok(
                "tb1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3q0sl5k7",
                BtcNetwork::Testnet,
            );
            ok(
                "tb1pqqqqp399et2xygdj5xreqhjjvcmzhxw4aywxecjdzew6hylgvsesf3hn0c",
                BtcNetwork::Testnet,
            );

            // BIP-350 invalid: every one must be refused on its own network.
            for address in [
                "tc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vq5zuyut",
                "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqh2y7hd",
                "BC1S0XLXVLHEMJA6C4DQV22UAPCTQUPFHLXM9H8Z3K2E72Q4K9HCZ7VQ54WELL",
                "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kemeawh",
                "bc1p38j9r5y49hruaue7wxjce0updqjuyyx0kh56v8s25huc6995vvpql3jow4",
                "BC130XLXVLHEMJA6C4DQV22UAPCTQUPFHLXM9H8Z3K2E72Q4K9HCZ7VQ7ZWS8R",
                "bc1pw5dgrnzv",
                "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7v8n0nx0muaewav253zgeav",
                "BC1QR508D6QEJXTDG4Y5R3ZARVARYV98GJ9P",
                "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7v07qwwzcrf",
                "bc1gmk9yu",
            ] {
                rejected(address, BtcNetwork::Mainnet);
            }
            for address in [
                "tb1z0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqglt7rf",
                "tb1q0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vq24jc47",
                "tb1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vq47Zagq",
                "tb1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vpggkg4j",
            ] {
                rejected(address, BtcNetwork::Testnet);
            }
        }

        #[test]
        fn legacy_base58() {
            ok("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2", BtcNetwork::Mainnet);
            ok("3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy", BtcNetwork::Mainnet);
            ok("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn", BtcNetwork::Testnet);
            ok("2MzQwSSnBHWHqSAqtTVQ6v47XtaisrJa1Vc", BtcNetwork::Testnet);
            // Regtest shares testnet's version bytes.
            ok("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn", BtcNetwork::Regtest);

            // One character changed: the checksum no longer matches.
            rejected("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN3", BtcNetwork::Mainnet);
        }

        #[test]
        fn addresses_seen_on_testnet() {
            // Taken from real RGB++ bindings the indexer resolved on testnet.
            ok(
                "tb1qkwkjlukvklvu9le8zcudqwte6gae0c9t32cgns",
                BtcNetwork::Testnet,
            );
            ok(
                "tb1pnprtrusgvq9tsf7dsm7yfwxf7qsazpdguw6xkducu020pc0wlw4s4f6w3n",
                BtcNetwork::Testnet,
            );
            // Signet uses the same formats.
            ok(
                "tb1qkwkjlukvklvu9le8zcudqwte6gae0c9t32cgns",
                BtcNetwork::Signet,
            );
        }

        #[test]
        fn wrong_network_names_the_network() {
            let message = rejected(
                "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
                BtcNetwork::Testnet,
            );
            assert!(message.contains("a mainnet address"), "{message}");
            let message = rejected("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn", BtcNetwork::Mainnet);
            assert!(message.contains("testnet"), "{message}");
        }

        #[test]
        fn path_fragments_and_oversized_input_are_refused() {
            for address in [
                "x/../../blocks/tip/height?",
                "not-an-address",
                "",
                "tb1q%2F..",
            ] {
                rejected(address, BtcNetwork::Testnet);
            }
            // Refused by length before any decoding is attempted.
            let huge = "1".repeat(100_000);
            let message = rejected(&huge, BtcNetwork::Mainnet);
            assert!(message.contains("longer than"), "{message}");
        }

        #[test]
        fn network_labels() {
            assert_eq!(BtcNetwork::from_label("mainnet"), Some(BtcNetwork::Mainnet));
            assert_eq!(BtcNetwork::from_label("Testnet"), Some(BtcNetwork::Testnet));
            assert_eq!(
                BtcNetwork::from_label("testnet4"),
                Some(BtcNetwork::Testnet)
            );
            assert_eq!(BtcNetwork::from_label("prod"), None);
        }
    }
}
