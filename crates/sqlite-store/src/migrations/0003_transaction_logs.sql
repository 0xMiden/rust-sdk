CREATE TABLE account_logs (
    account_id BLOB NOT NULL,
    block_num INTEGER NOT NULL,
    transaction_index INTEGER NOT NULL,
    log_index INTEGER NOT NULL,
    transaction_id BLOB NOT NULL,
    native_account_id BLOB NOT NULL,
    topic BLOB NOT NULL,
    record BLOB NOT NULL,
    PRIMARY KEY (account_id, block_num, transaction_index, log_index)
) WITHOUT ROWID;
CREATE INDEX account_logs_topic ON account_logs
    (account_id, topic, block_num, transaction_index, log_index);
CREATE TABLE account_log_sync (
    account_id BLOB PRIMARY KEY NOT NULL,
    chain_tip INTEGER NOT NULL
) WITHOUT ROWID;

-- Existing local transaction details have no logs or private opening.
UPDATE transactions SET details = CAST(details || X'0000000000000000000000000000000000000000000000000000000000000000000000' AS BLOB);
