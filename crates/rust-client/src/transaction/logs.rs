use miden_tx::auth::TransactionAuthenticator;

use crate::rpc::domain::{AccountLogPage, AccountLogQuery};
use crate::{Client, ClientError};

impl<AUTH: TransactionAuthenticator + Sync + 'static> Client<AUTH> {
    /// Fetches one bounded public-log page from the node for the requested emitter account.
    pub async fn get_account_logs(
        &self,
        query: AccountLogQuery,
    ) -> Result<AccountLogPage, ClientError> {
        Ok(self.rpc_api.get_account_logs(query).await?)
    }

    /// Synchronizes the requested public-log range, persisting each page before requesting the
    /// next. Repeating a range after interruption preserves one copy of every log occurrence.
    pub async fn sync_account_logs(
        &self,
        mut query: AccountLogQuery,
    ) -> Result<usize, ClientError> {
        let mut count = 0;
        loop {
            let page = self.rpc_api.get_account_logs(query.clone()).await?;
            page.validate(&query)?;
            count += page.records.len();
            let next_cursor = page.next_cursor;
            self.store.upsert_account_logs(query.account_id, page).await?;
            match next_cursor {
                Some(cursor) => query.after = Some(cursor),
                None => return Ok(count),
            }
        }
    }

    /// Reads persisted public logs. Locally executed private logs remain in transaction details.
    pub async fn get_cached_account_logs(
        &self,
        query: AccountLogQuery,
    ) -> Result<AccountLogPage, ClientError> {
        Ok(self.store.get_account_logs(query).await?)
    }
}
