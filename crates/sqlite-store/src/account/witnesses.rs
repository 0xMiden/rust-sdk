//! Registry of accounts whose [`AccountWitness`] the sync keeps fresh.

use std::string::ToString;
use std::vec::Vec;

use miden_client::account::AccountId;
use miden_client::block::{AccountWitness, BlockNumber};
use miden_client::store::StoreError;
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst};

impl SqliteStore {
    pub(crate) fn track_account_witness(
        conn: &mut Connection,
        account_id: AccountId,
    ) -> Result<(), StoreError> {
        // IGNORE rather than REPLACE: re-registering must not drop an already cached witness.
        const QUERY: &str = insert_sql!(account_witnesses { account_id } | IGNORE);

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .execute(params![account_id.to_bytes()])
            .into_store_error()?;

        Ok(())
    }

    pub(crate) fn untrack_account_witness(
        conn: &mut Connection,
        account_id: AccountId,
    ) -> Result<bool, StoreError> {
        const QUERY: &str = "DELETE FROM account_witnesses WHERE account_id = ?";

        let removed = conn
            .prepare_cached(QUERY)
            .into_store_error()?
            .execute(params![account_id.to_bytes()])
            .into_store_error()?;

        Ok(removed > 0)
    }

    pub(crate) fn tracked_account_witnesses(
        conn: &mut Connection,
    ) -> Result<Vec<AccountId>, StoreError> {
        const QUERY: &str = "SELECT account_id FROM account_witnesses";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                let id: Vec<u8> =
                    result.map_err(|err| StoreError::ParsingError(err.to_string()))?;
                AccountId::read_from_bytes(&id).map_err(StoreError::DataDeserializationError)
            })
            .collect()
    }

    pub(crate) fn get_account_witness(
        conn: &mut Connection,
        account_id: AccountId,
    ) -> Result<Option<(AccountWitness, BlockNumber)>, StoreError> {
        const QUERY: &str = "SELECT witness, block_num FROM account_witnesses \
                             WHERE account_id = ? AND witness IS NOT NULL";

        let row: Option<(Vec<u8>, u32)> = conn
            .prepare_cached(QUERY)
            .into_store_error()?
            .query_row(params![account_id.to_bytes()], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .into_store_error()?;

        row.map(|(witness, block_num)| {
            let witness = AccountWitness::read_from_bytes(&witness)
                .map_err(StoreError::DataDeserializationError)?;
            Ok((witness, BlockNumber::from(block_num)))
        })
        .transpose()
    }

    pub(crate) fn update_account_witness(
        conn: &mut Connection,
        account_id: AccountId,
        witness: &AccountWitness,
        block_num: BlockNumber,
    ) -> Result<bool, StoreError> {
        let tx = conn.transaction().into_store_error()?;
        let updated = Self::update_account_witness_tx(&tx, account_id, witness, block_num)?;
        tx.commit().into_store_error()?;

        Ok(updated)
    }

    /// Caches a witness inside an open transaction, so that the sync can persist the witnesses it
    /// already validated together with the account states they belong to.
    pub(crate) fn update_account_witness_tx(
        tx: &Transaction<'_>,
        account_id: AccountId,
        witness: &AccountWitness,
        block_num: BlockNumber,
    ) -> Result<bool, StoreError> {
        // UPDATE rather than an upsert: the row must already exist, since only
        // `track_account_witness` registers an account.
        const QUERY: &str =
            "UPDATE account_witnesses SET witness = ?, block_num = ? WHERE account_id = ?";

        let updated = tx
            .prepare_cached(QUERY)
            .into_store_error()?
            .execute(params![witness.to_bytes(), block_num.as_u32(), account_id.to_bytes()])
            .into_store_error()?;

        Ok(updated > 0)
    }
}
