# RGB++ commitments

An RGB++ transaction pair is bound together by a commitment: the Bitcoin transaction
publishes, in an `OP_RETURN`, a hash of the CKB transaction it authorises. The indexer
recomputes that hash from the CKB side and compares.

## Pre-image

```text
"RGB++"                                   5 bytes, ASCII
version                                   u16, big endian (currently 0)
input_len                                 u8
output_len                                u8
input_len  x  OutPoint                    molecule struct, 36 bytes each
output_len x (CellOutput || Bytes(data))  molecule table, then molecule Bytes
```

`commitment = sha256(sha256(pre-image))`, compared in the byte order the `OP_RETURN`
carries.

`input_len` counts the **leading RGB++ inputs** of the CKB transaction and
`output_len` the **leading RGB++ outputs**; the protocol requires the committed cells
to come first, so both are prefixes.

## The part that is easy to get wrong

The committed output cells carry a **zeroed Bitcoin txid** in their lock args.

The reason is a chicken-and-egg. The commitment goes into the very Bitcoin transaction
whose outputs those cells will be bound to, so when the commitment is computed that
transaction has no txid yet. The pre-image therefore holds a placeholder, and the real
txid only appears in the args once the Bitcoin transaction is finalised — which is the
form the indexer later reads off-chain.

Both protocol locks are affected:

- **RGB++ lock** — args are `out_index (u32le) || txid (32 bytes)`; the trailing 32
  bytes are zeroed. `out_index` is *not*: it is part of the commitment.
- **BTC time lock** — args are a molecule table `{ lock_script, after, btc_txid }`;
  the `btc_txid` field is zeroed and the rest is preserved.

Committing the on-chain args instead produces a perfectly plausible 32-byte digest
that never matches anything. `rgbpp_types::protocol::args_with_placeholder_txid` is
the only place this rewrite happens, and
`crates/indexer/tests/commitment_vectors.rs` asserts both that the placeholdered form
matches real published commitments and that the naive form does not.

## Byte order

RGB++ lock args embed the Bitcoin txid in **consensus (internal) order** — the reverse
of what explorers display. `rgbpp_types::bitcoin::BtcTxid` stores display order and
converts only in `from_internal_bytes` / `to_internal_bytes`, so the reversal happens
in exactly one place. Everything else — database, API, logs — is display order.

## What a mismatch means

Nothing is dropped. Discovery comes from CKB alone; the commitment only *labels* a
transition:

| `commitment_status` | Meaning |
|---|---|
| `unchecked` | the Bitcoin side has not been observed yet |
| `match` | the two chains agree |
| `mismatch` | they disagree — recorded as a `commitment_mismatch` anomaly |
| `missing` | the Bitcoin transaction exists but publishes no commitment |
| `btc_unknown` | the data source cannot see the Bitcoin transaction |
| `not_applicable` | issuance: see below |

**Issuance is exempt.** Issuing assets, or leaping from CKB to Bitcoin, binds cells to
a Bitcoin UTXO that already existed. The transaction that created that UTXO knows
nothing about RGB++ and commits to nothing, so comparing against it would manufacture
a mismatch on every mint.

An indexer that silently discarded on-chain state because its own commitment
computation disagreed would be worse than one that reports the disagreement — hence
anomalies rather than rejection.
