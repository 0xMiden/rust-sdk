//! PSWAP lineage storage in the `pswap_lineages` table.
//!
//! The `record` column holds the lineage record as a protobuf message. The other columns repeat the
//! record fields that the queries filter on. The queries write the active state as the literal `0`,
//! so that `SQLite` can use the partial index on the active tip.

use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_client_core::pswap::{PswapLineageFilter, PswapLineageRecord};
use miden_client_proto as proto;
use miden_protocol::Felt;
use miden_protocol::note::NoteId;
use miden_protocol::utils::serde::Serializable;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{column_value_as_u64, u64_to_value};

impl SqliteStore {
    pub(crate) fn get_pswap_lineage(
        conn: &Connection,
        order_id: Felt,
    ) -> Result<Option<PswapLineageRecord>, StoreError> {
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT record FROM pswap_lineages WHERE order_id = ?",
                params![u64_to_value(order_id.as_canonical_u64())],
                |row| row.get(0),
            )
            .optional()
            .into_store_error()?;
        Ok(bytes.map(|bytes| proto::decode_unchecked(&bytes)).transpose()?)
    }

    pub(crate) fn get_pswap_order_id_by_tip(
        conn: &Connection,
        tip: NoteId,
    ) -> Result<Option<Felt>, StoreError> {
        let order_id = conn
            .query_row(
                "SELECT order_id FROM pswap_lineages WHERE current_tip_note_id = ? AND state = 0",
                params![tip.to_bytes()],
                |row| column_value_as_u64(row, 0),
            )
            .optional()
            .into_store_error()?;
        order_id
            .map(|order_id| {
                Felt::new(order_id).map_err(|err| {
                    StoreError::ParsingError(format!("stored order ID is not a valid Felt: {err}"))
                })
            })
            .transpose()
    }

    pub(crate) fn get_pswap_lineages(
        conn: &Connection,
        filter: &PswapLineageFilter,
    ) -> Result<Vec<PswapLineageRecord>, StoreError> {
        let (query, values) = match filter {
            PswapLineageFilter::All => ("SELECT record FROM pswap_lineages", Vec::new()),
            PswapLineageFilter::Active => {
                ("SELECT record FROM pswap_lineages WHERE state = 0", Vec::new())
            },
            PswapLineageFilter::ByCreator(creator) => (
                "SELECT record FROM pswap_lineages WHERE creator_account_id = ?",
                vec![Value::Blob(creator.to_bytes())],
            ),
        };

        let mut stmt = conn.prepare_cached(query).into_store_error()?;
        let rows = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, Vec<u8>>(0))
            .into_store_error()?;
        rows.map(|bytes| Ok(proto::decode_unchecked(&bytes.into_store_error()?)?))
            .collect()
    }

    /// Inserts the record, or replaces the stored record of the same order.
    ///
    /// The tip lookup reads the columns of the row, so the tip moves with the record. The statement
    /// handles a conflict on the order ID only. An active record whose tip is the tip of another
    /// active lineage fails on the unique index.
    pub(crate) fn upsert_pswap_lineage(
        conn: &Connection,
        record: &PswapLineageRecord,
    ) -> Result<(), StoreError> {
        conn.execute(
            "INSERT INTO pswap_lineages
                 (order_id, creator_account_id, current_tip_note_id, state, record)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (order_id) DO UPDATE SET
                 creator_account_id = excluded.creator_account_id,
                 current_tip_note_id = excluded.current_tip_note_id,
                 state = excluded.state,
                 record = excluded.record",
            params![
                u64_to_value(record.order_id().as_canonical_u64()),
                record.creator_account_id().to_bytes(),
                record.current_tip_note_id.to_bytes(),
                record.state.as_u8(),
                proto::encode(record),
            ],
        )
        .into_store_error()?;
        Ok(())
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::account::AccountId;
    use miden_client::asset::AssetAmount;
    use miden_client::note::NoteId;
    use miden_client::pswap::{PswapLineageRecord, PswapLineageState};
    use miden_client::store::{Store, StoreError};
    use miden_client::{Felt, Word};
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;

    use crate::tests::create_test_store;

    fn felt(value: u64) -> Felt {
        Felt::new(value).unwrap()
    }

    fn note_id(value: u32) -> NoteId {
        NoteId::from_raw(Word::from([value, 0, 0, 0]))
    }

    fn creator() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap()
    }

    /// Builds the lineage of `order_id` at `depth`, with `tip` as its current tip.
    fn lineage(
        order_id: Felt,
        creator: AccountId,
        tip: NoteId,
        depth: u32,
        state: PswapLineageState,
    ) -> PswapLineageRecord {
        PswapLineageRecord::from_parts(
            note_id(0),
            order_id,
            creator,
            tip,
            depth,
            AssetAmount::new(100).unwrap(),
            AssetAmount::new(50).unwrap(),
            state,
        )
    }

    #[tokio::test]
    async fn tip_lookup_follows_the_latest_tip() {
        let store = create_test_store().await;
        let order_id = felt(1);

        store
            .upsert_pswap_lineage(&lineage(
                order_id,
                creator(),
                note_id(1),
                0,
                PswapLineageState::Active,
            ))
            .await
            .unwrap();
        assert_eq!(store.get_pswap_order_id_by_tip(note_id(1)).await.unwrap(), Some(order_id));

        store
            .upsert_pswap_lineage(&lineage(
                order_id,
                creator(),
                note_id(2),
                1,
                PswapLineageState::Active,
            ))
            .await
            .unwrap();
        assert_eq!(store.get_pswap_order_id_by_tip(note_id(1)).await.unwrap(), None);
        assert_eq!(store.get_pswap_order_id_by_tip(note_id(2)).await.unwrap(), Some(order_id));

        // A terminal round keeps the last tip in the record, but the lineage has no tip to resolve.
        store
            .upsert_pswap_lineage(&lineage(
                order_id,
                creator(),
                note_id(2),
                2,
                PswapLineageState::FullyFilled,
            ))
            .await
            .unwrap();
        assert_eq!(store.get_pswap_order_id_by_tip(note_id(2)).await.unwrap(), None);
        assert_eq!(
            store.get_pswap_lineage(order_id).await.unwrap().unwrap().state,
            PswapLineageState::FullyFilled
        );
    }

    #[tokio::test]
    async fn two_active_lineages_cannot_share_a_tip() {
        let store = create_test_store().await;
        let tip = note_id(1);
        store
            .upsert_pswap_lineage(&lineage(felt(1), creator(), tip, 0, PswapLineageState::Active))
            .await
            .unwrap();

        let result = store
            .upsert_pswap_lineage(&lineage(felt(2), creator(), tip, 0, PswapLineageState::Active))
            .await;

        assert!(
            matches!(result, Err(StoreError::DatabasePermanentError(_))),
            "a second active lineage on the same tip should fail, got {result:?}"
        );
        assert!(store.get_pswap_lineage(felt(2)).await.unwrap().is_none());
        assert_eq!(store.get_pswap_order_id_by_tip(tip).await.unwrap(), Some(felt(1)));
    }
}
