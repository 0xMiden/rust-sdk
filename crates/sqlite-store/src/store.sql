-- Table for storing database migrations data.
-- Note: we can store values of different types in the same `value` field.
CREATE TABLE migrations (
    name  TEXT NOT NULL,
    value ANY,

    PRIMARY KEY (name),
    CONSTRAINT migration_name_is_not_empty CHECK (length(name) > 0)
) STRICT, WITHOUT ROWID;

-- Table for storing different settings in run-time, which need to persist over runs.
CREATE TABLE settings (
    name  TEXT NOT NULL,
    value BLOB NOT NULL,

    PRIMARY KEY (name),
    CONSTRAINT setting_name_is_not_empty CHECK (length(name) > 0)
) STRICT, WITHOUT ROWID;

-- Create account_code table
CREATE TABLE account_code (
    commitment BLOB NOT NULL,   -- commitment to the account code
    code BLOB NOT NULL,         -- serialized account code.
    PRIMARY KEY (commitment)
);

-- ── Account headers ──────────────────────────────────────────────────────

-- Latest account header: one row per account (current state).
CREATE TABLE latest_account_headers (
    id BLOB NOT NULL,                        -- serialized account ID
    account_commitment BLOB NOT NULL UNIQUE,  -- account state commitment
    code_commitment BLOB NOT NULL,            -- commitment to the account code
    storage_commitment BLOB NOT NULL,         -- commitment to the account storage
    vault_root BLOB NOT NULL,                 -- root of the account vault Merkle tree
    nonce BIGINT NOT NULL,                   -- account nonce
    account_seed BLOB NULL,                  -- seed used to generate the ID; NULL for non-new accounts
    locked BOOLEAN NOT NULL,                 -- whether the account is locked
    watched BOOLEAN NOT NULL DEFAULT FALSE, -- Whether the account is tracked in watch mode
    PRIMARY KEY (id),
    FOREIGN KEY (code_commitment) REFERENCES account_code(commitment)
);

-- Historical account headers: stores old headers that were replaced by newer states.
-- Each row represents a previous account state that was superseded at replaced_at_nonce.
CREATE TABLE historical_account_headers (
    id BLOB NOT NULL,                        -- serialized account ID
    account_commitment BLOB NOT NULL UNIQUE,  -- commitment of this old state
    code_commitment BLOB NOT NULL,            -- commitment to the old account code
    storage_commitment BLOB NOT NULL,         -- commitment to the old account storage
    vault_root BLOB NOT NULL,                 -- root of the old account vault Merkle tree
    nonce BIGINT NOT NULL,                   -- nonce of this old state
    account_seed BLOB NULL,                  -- seed used to generate the ID; NULL for non-new accounts
    locked BOOLEAN NOT NULL,                 -- whether the account was locked
    replaced_at_nonce BIGINT NOT NULL,       -- nonce of the new state that replaced this one
    PRIMARY KEY (account_commitment),
    FOREIGN KEY (code_commitment) REFERENCES account_code(commitment),

    CONSTRAINT check_seed_nonzero CHECK (NOT (nonce = 0 AND account_seed IS NULL))
);
CREATE INDEX idx_historical_account_headers_id_replaced_at ON historical_account_headers(id, replaced_at_nonce DESC);

-- ── Account storage (latest + historical) ────────────────────────────────

CREATE TABLE latest_account_storage (
    account_id BLOB NOT NULL,     -- serialized account ID
    slot_name TEXT NOT NULL,      -- name of the storage slot
    slot_value BLOB NULL,         -- top-level value of the slot (for maps, contains the root)
    slot_type INTEGER NOT NULL,   -- type of the slot (0 = Value, 1 = Map)
    PRIMARY KEY (account_id, slot_name)
) WITHOUT ROWID;

-- Historical account storage: stores old slot values that were replaced.
-- NULL old_slot_value means the slot didn't exist before (was created at replaced_at_nonce).
CREATE TABLE historical_account_storage (
    account_id BLOB NOT NULL,           -- serialized account ID
    replaced_at_nonce BIGINT NOT NULL,  -- nonce at which this old value was replaced
    slot_name TEXT NOT NULL,            -- name of the storage slot
    old_slot_value BLOB NULL,           -- old top-level value (NULL = slot was new)
    slot_type INTEGER NOT NULL,         -- type of the slot (0 = Value, 1 = Map)
    PRIMARY KEY (account_id, replaced_at_nonce, slot_name)
) WITHOUT ROWID;

-- ── Storage map entries (latest + historical) ────────────────────────────

CREATE TABLE latest_storage_map_entries (
    account_id BLOB NOT NULL,   -- account ID
    slot_name TEXT NOT NULL,    -- name of the storage slot this entry belongs to
    key BLOB NOT NULL,          -- map entry key
    value BLOB NOT NULL,        -- map entry value
    PRIMARY KEY (account_id, slot_name, key)
) WITHOUT ROWID;

-- Historical storage map entries: stores old map entry values that were replaced.
-- NULL old_value means the entry didn't exist before (was created at replaced_at_nonce).
CREATE TABLE historical_storage_map_entries (
    account_id BLOB NOT NULL,           -- account ID
    replaced_at_nonce BIGINT NOT NULL,  -- nonce at which this old entry was replaced
    slot_name TEXT NOT NULL,            -- name of the storage slot this entry belongs to
    key BLOB NOT NULL,                  -- map entry key
    old_value BLOB NULL,                -- old map entry value (NULL = entry was new)
    PRIMARY KEY (account_id, replaced_at_nonce, slot_name, key)
) WITHOUT ROWID;

-- ── Account assets (latest + historical) ─────────────────────────────────

CREATE TABLE latest_account_assets (
    account_id BLOB NOT NULL,        -- account ID
    asset_id BLOB NOT NULL,          -- asset's asset id
    asset BLOB NOT NULL,             -- serialized asset value
    PRIMARY KEY (account_id, asset_id)
) WITHOUT ROWID;

-- Historical account assets: stores old assets that were replaced.
-- NULL old_asset means the asset didn't exist before (was created at replaced_at_nonce).
CREATE TABLE historical_account_assets (
    account_id BLOB NOT NULL,           -- account ID
    replaced_at_nonce BIGINT NOT NULL,  -- nonce at which this old asset was replaced
    asset_id BLOB NOT NULL,             -- asset key in the vault's underlying SMT
    old_asset BLOB NULL,                -- old serialized asset value (NULL = asset was new)
    PRIMARY KEY (account_id, replaced_at_nonce, asset_id)
) WITHOUT ROWID;

-- ── Foreign account code ─────────────────────────────────────────────────

CREATE TABLE foreign_account_code(
    account_id BLOB NOT NULL,
    code_commitment BLOB NOT NULL,
    PRIMARY KEY (account_id),
    FOREIGN KEY (code_commitment) REFERENCES account_code(commitment)
);

-- ── Transactions ─────────────────────────────────────────────────────────

CREATE TABLE transactions (
    id BLOB NOT NULL,                                -- Transaction ID (commitment of various components)
    details BLOB NOT NULL,                           -- Serialized transaction details
    script_root BLOB,                                -- Transaction script root
    block_num UNSIGNED BIG INT,                      -- Block number for the block against which the transaction was executed.
    status_variant INT NOT NULL,                     -- Status variant identifier
    status BLOB NOT NULL,                            -- Serialized transaction status
    FOREIGN KEY (script_root) REFERENCES transaction_scripts(script_root),
    PRIMARY KEY (id)
) WITHOUT ROWID;
CREATE INDEX idx_transactions_uncommitted ON transactions(status_variant);


CREATE TABLE transaction_scripts (
    script_root BLOB NOT NULL,                       -- Transaction script root
    script BLOB,                                     -- serialized Transaction script

    PRIMARY KEY (script_root)
) WITHOUT ROWID;

-- ── Notes ────────────────────────────────────────────────────────────────

CREATE TABLE input_notes (
    details_commitment BLOB NOT NULL,                       -- commitment to the note details (recipient + assets); stable across the note's lifecycle and independent of metadata
    note_id BLOB NULL,                                      -- the full note id (hash(details_commitment, metadata_commitment)); NULL until metadata is known
    assets BLOB NOT NULL,                                   -- the serialized list of assets
    attachments BLOB NOT NULL,                              -- the serialized NoteAttachments
    serial_number BLOB NOT NULL,                            -- the serial number of the note
    inputs BLOB NOT NULL,                                   -- the serialized list of note inputs
    script_root BLOB NOT NULL,                              -- the script root of the note, used to join with the notes_scripts table
    nullifier BLOB NULL,                                    -- the nullifier of the note, used to query by nullifier; NULL until metadata is known
    state_discriminant UNSIGNED INT NOT NULL,               -- state discriminant of the note, used to query by state
    state BLOB NOT NULL,                                    -- serialized note state
    created_at UNSIGNED BIG INT NOT NULL,                   -- timestamp of the note creation/import
    consumed_block_height INTEGER NULL,                     -- block height at which the note was consumed; NULL for non-consumed notes
    consumed_tx_order INTEGER NULL,                         -- per-account position of the consuming tx in the account's execution chain within the block; NULL for external consumption or non-consumed notes
    consumer_account_id BLOB NULL,                          -- serialized account ID that consumed this note; NULL for non-consumed or externally consumed notes

    PRIMARY KEY (details_commitment),
    FOREIGN KEY (script_root) REFERENCES notes_scripts(script_root)
) WITHOUT ROWID;
CREATE INDEX idx_input_notes_state ON input_notes(state_discriminant);
CREATE INDEX idx_input_notes_nullifier ON input_notes(nullifier);
CREATE INDEX idx_input_notes_note_id ON input_notes(note_id);
CREATE INDEX idx_input_notes_consumption ON input_notes(consumed_block_height, consumed_tx_order);
CREATE INDEX idx_input_notes_script_root ON input_notes(script_root);

CREATE TABLE output_notes (
    details_commitment BLOB NOT NULL,                       -- commitment to the note details (recipient + assets); primary key
    note_id BLOB NOT NULL,                                  -- the full note id (hash(details_commitment, metadata_commitment))
    recipient_digest BLOB NOT NULL,                                -- the note recipient
    assets BLOB NOT NULL,                                   -- the serialized NoteAssets, including vault commitment and list of assets
    metadata BLOB NOT NULL,                                 -- serialized metadata
    nullifier BLOB NULL,
    expected_height UNSIGNED INT NOT NULL,                  -- the block height after which the note is expected to be created
-- TODO: normalize script data for output notes
--     script_commitment BLOB NULL,
    state_discriminant UNSIGNED INT NOT NULL,               -- state discriminant of the note, used to query by state
    state BLOB NOT NULL,                                    -- serialized note state
    attachments BLOB NOT NULL,

    PRIMARY KEY (details_commitment)
) WITHOUT ROWID;
CREATE INDEX idx_output_notes_state ON output_notes(state_discriminant);
CREATE INDEX idx_output_notes_nullifier ON output_notes(nullifier);
CREATE INDEX idx_output_notes_note_id ON output_notes(note_id);

CREATE TABLE notes_scripts (
    script_root BLOB NOT NULL,                       -- Note script root
    serialized_note_script BLOB,                     -- NoteScript, serialized

    PRIMARY KEY (script_root)
);

-- ── Blockchain checkpoint & tags ─────────────────────────────────────────

CREATE TABLE blockchain_checkpoint (
    block_num UNSIGNED BIG INT NOT NULL,    -- the block number of the most recent state sync
    partial_blockchain_peaks BLOB NOT NULL, -- serialized MMR peaks at the current sync height
    PRIMARY KEY (block_num)
);

CREATE TABLE tags (
    tag BLOB NOT NULL,     -- the serialized tag
    source BLOB NOT NULL   -- the serialized tag source
);
-- Enforces tag idempotency: `add_note_tag` uses `INSERT OR IGNORE` against this index so a
-- repeated (tag, source) pair is a no-op instead of a duplicated row.
CREATE UNIQUE INDEX idx_tags_tag_source ON tags(tag, source);

-- insert initial row into blockchain_checkpoint table
INSERT OR IGNORE INTO blockchain_checkpoint (block_num, partial_blockchain_peaks)
SELECT 0, X''
WHERE (
    SELECT COUNT(*) FROM blockchain_checkpoint
) = 0;

-- ── Block headers & partial blockchain ───────────────────────────────────

CREATE TABLE block_headers (
    block_num UNSIGNED BIG INT NOT NULL,  -- block number
    header BLOB NOT NULL,                 -- serialized block header
    has_client_notes BOOL NOT NULL,       -- whether the block has notes relevant to the client
    PRIMARY KEY (block_num)
);
CREATE INDEX IF NOT EXISTS idx_block_headers_has_notes ON block_headers(block_num) WHERE has_client_notes = 1;

CREATE TABLE partial_blockchain_nodes (
    id UNSIGNED BIG INT NOT NULL,   -- in-order index of the internal MMR node
    node BLOB NOT NULL,             -- internal node value (commitment)
    PRIMARY KEY (id)
) WITHOUT ROWID;

-- ── Addresses ────────────────────────────────────────────────────────────

CREATE TABLE addresses (
    address BLOB NOT NULL,          -- the address
    account_id BLOB NOT NULL,       -- associated serialized account ID

    PRIMARY KEY (address)
) WITHOUT ROWID;

CREATE INDEX idx_addresses_account_id ON addresses(account_id);
