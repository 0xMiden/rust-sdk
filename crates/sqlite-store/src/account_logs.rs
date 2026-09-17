use miden_client::account::AccountId;
use miden_client::rpc::domain::{
    AccountLogCursor,
    AccountLogPage,
    AccountLogQuery,
    AccountLogRecord,
};
use miden_client::store::StoreError;
use miden_client::transaction::{TransactionId, TransactionLog};
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, OptionalExtension, params};

use crate::sql_error::SqlResultExt;

pub(crate) fn upsert(
    conn: &mut Connection,
    account: AccountId,
    page: AccountLogPage,
) -> Result<(), StoreError> {
    if page.records.len() > 256 {
        return Err(StoreError::ParsingError("log page is too large".into()));
    }
    if page
        .records
        .iter()
        .map(|record| record.log.get_size_hint() + 192)
        .sum::<usize>()
        > 1024 * 1024
    {
        return Err(StoreError::ParsingError("log page exceeds byte limit".into()));
    }
    let tx = conn.transaction().into_store_error()?;
    for record in page.records {
        if record.log.emitter() != account
            || !record.native_account_id.is_public()
            || record.cursor.block_num > page.chain_tip
            || record.cursor.transaction_index as usize
                >= miden_protocol::MAX_LOG_DATA_TRANSACTIONS_PER_BLOCK
            || record.cursor.log_index as usize >= miden_protocol::MAX_LOGS_PER_TX
        {
            return Err(StoreError::ParsingError("public log account mismatch".into()));
        }
        let existing: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx.query_row(
            "SELECT transaction_id, native_account_id, record FROM account_logs WHERE account_id = ? AND block_num = ? AND transaction_index = ? AND log_index = ?",
            params![account.to_bytes(), record.cursor.block_num.as_u32(), record.cursor.transaction_index, record.cursor.log_index],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().into_store_error()?;
        if existing.is_some_and(|existing| {
            existing
                != (
                    record.transaction_id.to_bytes(),
                    record.native_account_id.to_bytes(),
                    record.log.to_bytes(),
                )
        }) {
            return Err(StoreError::ParsingError("conflicting committed log occurrence".into()));
        }
        tx.execute("INSERT OR IGNORE INTO account_logs (account_id, block_num, transaction_index, log_index, transaction_id, native_account_id, topic, record) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", params![account.to_bytes(), record.cursor.block_num.as_u32(), record.cursor.transaction_index, record.cursor.log_index, record.transaction_id.to_bytes(), record.native_account_id.to_bytes(), record.log.topic().to_bytes(), record.log.to_bytes()]).into_store_error()?;
    }
    tx.execute("INSERT INTO account_log_sync (account_id, chain_tip) VALUES (?, ?) ON CONFLICT(account_id) DO UPDATE SET chain_tip = max(chain_tip, excluded.chain_tip)", params![account.to_bytes(), page.chain_tip.as_u32()]).into_store_error()?;
    tx.commit().into_store_error()
}

pub(crate) fn get(
    conn: &mut Connection,
    query: &AccountLogQuery,
) -> Result<AccountLogPage, StoreError> {
    if query.page_size == 0
        || query.page_size > 256
        || query.block_from > query.block_to
        || query.after.is_some_and(|cursor| {
            cursor.block_num < query.block_from
                || cursor.block_num > query.block_to
                || cursor.transaction_index as usize
                    >= miden_protocol::MAX_LOG_DATA_TRANSACTIONS_PER_BLOCK
                || cursor.log_index as usize >= miden_protocol::MAX_LOGS_PER_TX
        })
    {
        return Err(StoreError::ParsingError("invalid log range or page size".into()));
    }
    let mut sql = String::from(
        "SELECT block_num, transaction_index, log_index, transaction_id, native_account_id, record FROM account_logs WHERE account_id = ? AND block_num >= ? AND block_num <= ? AND (block_num, transaction_index, log_index) > (?, ?, ?)",
    );
    if query.topic.is_some() {
        sql = sql.replace(
            "FROM account_logs WHERE",
            "FROM account_logs INDEXED BY account_logs_topic WHERE",
        );
    }
    let after = query.after;
    let mut values: Vec<rusqlite::types::Value> = vec![
        query.account_id.to_bytes().into(),
        i64::from(query.block_from.as_u32()).into(),
        i64::from(query.block_to.as_u32()).into(),
        i64::from(after.map_or(query.block_from, |cursor| cursor.block_num).as_u32()).into(),
        after.map_or(-1, |cursor| i64::from(cursor.transaction_index)).into(),
        after.map_or(-1, |cursor| i64::from(cursor.log_index)).into(),
    ];
    if let Some(topic) = query.topic {
        sql.push_str(" AND topic = ?");
        values.push(topic.to_bytes().into());
    }
    sql.push_str(" ORDER BY block_num, transaction_index, log_index LIMIT ?");
    values.push((i64::from(query.page_size) + 1).into());
    let mut stmt = conn.prepare(&sql).into_store_error()?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
            ))
        })
        .into_store_error()?;
    let mut records = Vec::new();
    let mut next_cursor = None;
    let mut bytes = 0;
    for row in rows {
        let (block, transaction_index, log_index, transaction_id, native_account_id, record) =
            row.into_store_error()?;
        if records.len() >= query.page_size as usize || bytes + record.len() + 192 > 1024 * 1024 {
            next_cursor = records.last().map(|record: &AccountLogRecord| record.cursor);
            break;
        }
        bytes += record.len() + 192;
        records.push(AccountLogRecord {
            cursor: AccountLogCursor {
                block_num: block.into(),
                transaction_index,
                log_index,
            },
            transaction_id: TransactionId::read_from_bytes(&transaction_id)?,
            native_account_id: AccountId::read_from_bytes(&native_account_id)?,
            log: TransactionLog::read_from_bytes(&record)?,
        });
    }
    let tip = conn
        .query_row(
            "SELECT chain_tip FROM account_log_sync WHERE account_id = ?",
            [query.account_id.to_bytes()],
            |row| row.get::<_, u32>(0),
        )
        .optional()
        .into_store_error()?
        .unwrap_or(0);
    Ok(AccountLogPage {
        chain_tip: tip.into(),
        records,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use miden_client::transaction::LogTopic;
    use miden_client::{Felt, Word};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_SENDER,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
    };

    use super::*;
    use crate::db_management::migration::SqliteMigrator;

    fn record(index: u32) -> AccountLogRecord {
        let account =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE).unwrap();
        AccountLogRecord {
            cursor: AccountLogCursor {
                block_num: 5.into(),
                transaction_index: 0,
                log_index: index,
            },
            transaction_id: TransactionId::from_raw(Word::from([7u32; 4])),
            native_account_id: account,
            log: TransactionLog::new(
                account,
                LogTopic::new([Felt::from(index % 2), Felt::from(9u32)]),
                vec![Word::from([42u32; 4])],
            )
            .unwrap(),
        }
    }

    #[test]
    fn public_logs_resume_after_reopen_and_reject_conflicting_pages_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite");
        let account = record(0).log.emitter();
        let records: Vec<_> = (0..5).map(record).collect();
        {
            let mut conn = Connection::open(&path).unwrap();
            SqliteMigrator::client().apply(&mut conn).unwrap();
            let page = AccountLogPage {
                chain_tip: 7.into(),
                records: records.clone(),
                next_cursor: None,
            };
            upsert(&mut conn, account, page.clone()).unwrap();
            upsert(&mut conn, account, page).unwrap();
        }
        let mut conn = Connection::open(path).unwrap();
        SqliteMigrator::client().apply(&mut conn).unwrap();
        let mut query = AccountLogQuery::new(account, 5.into(), 7.into());
        query.page_size = 2;
        let first = get(&mut conn, &query).unwrap();
        assert_eq!(first.records, records[..2]);
        first.validate(&query).unwrap();
        query.after = first.next_cursor;
        let second = get(&mut conn, &query).unwrap();
        assert_eq!(second.records, records[2..4]);
        query.after = second.next_cursor;
        let last = get(&mut conn, &query).unwrap();
        assert_eq!(last.records, records[4..]);
        assert!(last.next_cursor.is_none());
        query.after = None;
        query.topic = Some(records[1].log.topic());
        assert_eq!(
            get(&mut conn, &query).unwrap().records,
            vec![records[1].clone(), records[3].clone()]
        );

        let mut conflicting = records[0].clone();
        conflicting.transaction_id = TransactionId::from_raw(Word::empty());
        assert!(
            upsert(
                &mut conn,
                account,
                AccountLogPage {
                    chain_tip: 8.into(),
                    records: vec![record(5), conflicting],
                    next_cursor: None
                }
            )
            .is_err()
        );
        let all = get(&mut conn, &AccountLogQuery::new(account, 0.into(), 8.into())).unwrap();
        assert_eq!(all.records, records);
        assert_eq!(all.chain_tip.as_u32(), 7);
        let private = AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap();
        assert!(
            get(&mut conn, &AccountLogQuery::new(private, 0.into(), 8.into()))
                .unwrap()
                .records
                .is_empty()
        );
        let mut invalid = record(5);
        invalid.native_account_id = private;
        assert!(
            upsert(
                &mut conn,
                account,
                AccountLogPage {
                    chain_tip: 8.into(),
                    records: vec![invalid],
                    next_cursor: None
                }
            )
            .is_err()
        );
    }
}
