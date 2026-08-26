-- Bitcoin transaction fee, for activity listings.
--
-- Added separately rather than folded into 0001 so that a database already carrying
-- indexed data picks it up by migrating rather than by being rebuilt. Existing rows
-- keep NULL until the transaction is fetched again.
ALTER TABLE btc_txs ADD COLUMN IF NOT EXISTS fee BIGINT;
