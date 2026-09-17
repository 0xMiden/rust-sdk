use alloc::vec::Vec;

use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::{LogTopic, TransactionId, TransactionLog};
use miden_protocol::utils::serde::Serializable;

/// Position of a public log in the committed chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountLogCursor {
    pub block_num: BlockNumber,
    pub transaction_index: u32,
    pub log_index: u32,
}

/// A query within one emitter account. Both block endpoints are inclusive.
#[derive(Clone, Debug)]
pub struct AccountLogQuery {
    pub account_id: AccountId,
    pub block_from: BlockNumber,
    pub block_to: BlockNumber,
    pub topic: Option<LogTopic>,
    pub after: Option<AccountLogCursor>,
    pub page_size: u32,
}

impl AccountLogQuery {
    pub fn new(account_id: AccountId, block_from: BlockNumber, block_to: BlockNumber) -> Self {
        Self {
            account_id,
            block_from,
            block_to,
            topic: None,
            after: None,
            page_size: 128,
        }
    }
}

/// A public record returned for its emitter, including native-account and transaction metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountLogRecord {
    pub cursor: AccountLogCursor,
    pub transaction_id: TransactionId,
    pub native_account_id: AccountId,
    pub log: TransactionLog,
}

#[derive(Clone, Debug)]
pub struct AccountLogPage {
    pub chain_tip: BlockNumber,
    pub records: Vec<AccountLogRecord>,
    pub next_cursor: Option<AccountLogCursor>,
}

impl AccountLogPage {
    /// Checks scope and strict cursor progress before records are exposed or persisted.
    pub fn validate(&self, query: &AccountLogQuery) -> Result<(), super::super::RpcError> {
        let invalid = || super::super::RpcError::InvalidResponse("invalid account log page".into());
        if self.records.len() > query.page_size as usize
            || query.page_size == 0
            || query.page_size > 256
            || self.chain_tip < query.block_to
            || query.block_from > query.block_to
            || query.after.is_some_and(|cursor| {
                cursor.block_num < query.block_from
                    || cursor.block_num > query.block_to
                    || cursor.transaction_index as usize
                        >= miden_protocol::MAX_LOG_DATA_TRANSACTIONS_PER_BLOCK
                    || cursor.log_index as usize >= miden_protocol::MAX_LOGS_PER_TX
            })
            || self
                .records
                .iter()
                .map(|record| record.log.get_size_hint() + 192)
                .sum::<usize>()
                > 1024 * 1024
        {
            return Err(invalid());
        }
        let mut previous = query.after;
        for record in &self.records {
            if record.log.emitter() != query.account_id
                || !record.native_account_id.is_public()
                || query.topic.is_some_and(|topic| record.log.topic() != topic)
                || record.cursor.block_num < query.block_from
                || record.cursor.block_num > query.block_to
                || record.cursor.log_index as usize >= miden_protocol::MAX_LOGS_PER_TX
                || record.cursor.transaction_index as usize
                    >= miden_protocol::MAX_LOG_DATA_TRANSACTIONS_PER_BLOCK
                || previous.is_some_and(|previous| previous >= record.cursor)
            {
                return Err(invalid());
            }
            previous = Some(record.cursor);
        }
        if self.next_cursor.is_some() && (self.records.is_empty() || self.next_cursor != previous) {
            return Err(invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use miden_protocol::Word;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_SENDER,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
    };

    use super::*;

    #[test]
    fn rejects_cross_account_private_and_nonprogressing_pages() {
        let account =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE).unwrap();
        let other = AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap();
        let query = AccountLogQuery::new(account, 1.into(), 2.into());
        let record = AccountLogRecord {
            cursor: AccountLogCursor {
                block_num: 1.into(),
                transaction_index: 0,
                log_index: 0,
            },
            transaction_id: TransactionId::from_raw(Word::empty()),
            native_account_id: account,
            log: TransactionLog::new(account, LogTopic::new([1u32.into(), 2u32.into()]), vec![])
                .unwrap(),
        };
        let page = AccountLogPage {
            chain_tip: 2.into(),
            records: vec![record.clone()],
            next_cursor: Some(record.cursor),
        };
        page.validate(&query).unwrap();
        let mut invalid = page.clone();
        invalid.records[0].native_account_id = other;
        assert!(invalid.validate(&query).is_err());
        invalid = page.clone();
        invalid.records[0].log = TransactionLog::new(other, record.log.topic(), vec![]).unwrap();
        assert!(invalid.validate(&query).is_err());
        invalid = page.clone();
        invalid.records.push(record.clone());
        assert!(invalid.validate(&query).is_err());
        invalid = page.clone();
        invalid.records.clear();
        assert!(invalid.validate(&query).is_err());
        let mut after = query.clone();
        after.after = Some(record.cursor);
        assert!(page.validate(&after).is_err());
        let mut filtered = query;
        filtered.topic = Some(LogTopic::new([3u32.into(), 4u32.into()]));
        assert!(page.validate(&filtered).is_err());
    }
}
