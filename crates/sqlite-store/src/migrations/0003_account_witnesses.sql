-- Adds the registry of accounts whose account witness the sync keeps fresh.

-- ── Account witnesses ────────────────────────────────────────────────────

-- A row registers the account; `witness` stays NULL until the first refresh fills it in.
--
-- A witness only serves a transaction whose reference block is exactly `block_num`, so the two
-- columns are only meaningful together.
CREATE TABLE account_witnesses (
    account_id BLOB NOT NULL,          -- serialized account ID
    witness    BLOB NULL,              -- serialized AccountWitness; NULL until the first refresh
    block_num  UNSIGNED BIG INT NULL,  -- block the witness was fetched at

    PRIMARY KEY (account_id),
    CONSTRAINT witness_and_block_num_together CHECK ((witness IS NULL) = (block_num IS NULL))
) WITHOUT ROWID;
