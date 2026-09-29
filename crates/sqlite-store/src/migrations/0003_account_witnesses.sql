-- Adds the registry of accounts whose account witness the sync keeps fresh.

-- ── Account witnesses ────────────────────────────────────────────────────

-- A row registers the account; `witness` stays NULL until the first refresh fills it in.
--
-- The sync writes the witness together with the sync height, so a stored witness always opens
-- under the account root of the block at the sync height.
CREATE TABLE account_witnesses (
    account_id BLOB NOT NULL,  -- serialized account ID
    witness    BLOB NULL,      -- serialized AccountWitness; NULL until the first refresh

    PRIMARY KEY (account_id)
) WITHOUT ROWID;
