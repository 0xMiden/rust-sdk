-- Aligns the indices with the queries the store actually runs, and drops the transaction column no
-- query reads.

-- ── Account code references ──────────────────────────────────────────────

-- SQLite does not index foreign key child columns automatically. Without these, account code garbage
-- collection scans the whole table once per candidate commitment.
CREATE INDEX idx_latest_account_headers_code_commitment ON latest_account_headers(code_commitment);
CREATE INDEX idx_historical_account_headers_code_commitment ON historical_account_headers(code_commitment);
CREATE INDEX idx_foreign_account_code_code_commitment ON foreign_account_code(code_commitment);

-- ── Transactions ─────────────────────────────────────────────────────────

-- The block a transaction was executed against is part of the serialized details, so the column is
-- redundant.
ALTER TABLE transactions DROP COLUMN block_num;

-- Only pending transactions (status 0) are ever filtered by status, so the index covers just those rows.
DROP INDEX idx_transactions_uncommitted;
CREATE INDEX idx_transactions_pending ON transactions(status_variant) WHERE status_variant = 0;

-- ── Notes ────────────────────────────────────────────────────────────────

-- `nullifier` is the second column so the unspent nullifier query reads this index alone, and
-- `state_discriminant` stays first so the other state filters still match on the prefix.
DROP INDEX idx_input_notes_state;
CREATE INDEX idx_input_notes_state ON input_notes(state_discriminant, nullifier);

-- `consumer_account_id` leads so a walk over one account's consumed notes is a single index seek,
-- with the block height and the position within the block ordering the rows it finds.
DROP INDEX idx_input_notes_consumption;
CREATE INDEX idx_input_notes_consumption ON input_notes(consumer_account_id, consumed_block_height, consumed_tx_order);
