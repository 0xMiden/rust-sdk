//! PSWAP lineage storage over the `settings` table, encoded as protobuf messages.
//!
//! ```text
//! pswap/order/{order_id_hex}  →  PswapLineageRecord  (stable, never re-keyed)
//! pswap/tip/{tip_note_id}     →  order_id            (index of active tips, re-keyed each round)
//! ```

use std::string::String;
use std::vec::Vec;

use miden_client::note::NoteId;
use miden_client::pswap::{PswapLineageFilter, PswapLineageRecord, PswapLineageState};
use miden_client::store::{SettingScope, StoreError};
use miden_client_proto as proto;
use miden_protocol::Felt;
use rusqlite::{Connection, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;

const ORDER_PREFIX: &str = "pswap/order/";
const TIP_PREFIX: &str = "pswap/tip/";

fn order_key(order_id: Felt) -> String {
    format!("{ORDER_PREFIX}{:016x}", order_id.as_canonical_u64())
}

fn tip_key(tip: NoteId) -> String {
    format!("{TIP_PREFIX}{}", tip.as_word())
}

impl SqliteStore {
    pub(crate) fn get_pswap_lineage(
        conn: &Connection,
        order_id: Felt,
    ) -> Result<Option<PswapLineageRecord>, StoreError> {
        Self::get_client_setting(conn, &order_key(order_id))
    }

    pub(crate) fn get_pswap_order_id_by_tip(
        conn: &Connection,
        tip: NoteId,
    ) -> Result<Option<Felt>, StoreError> {
        Self::get_client_setting(conn, &tip_key(tip))
    }

    pub(crate) fn get_pswap_lineages(
        conn: &Connection,
        filter: &PswapLineageFilter,
    ) -> Result<Vec<PswapLineageRecord>, StoreError> {
        let mut stmt = conn
            .prepare("SELECT value FROM settings WHERE scope = $1 AND name LIKE $2")
            .into_store_error()?;
        let rows = stmt
            .query_map(params![SettingScope::Client.as_u8(), format!("{ORDER_PREFIX}%")], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .into_store_error()?;

        let mut lineages = Vec::new();
        for bytes in rows {
            let record: PswapLineageRecord = proto::decode_unchecked(&bytes.into_store_error()?)?;
            if filter.matches(&record) {
                lineages.push(record);
            }
        }
        Ok(lineages)
    }

    /// Writes the record and moves the tip index. The caller runs it in a write transaction, so the
    /// read of the previous tip and the writes are atomic.
    pub(crate) fn upsert_pswap_lineage(
        conn: &Connection,
        record: &PswapLineageRecord,
    ) -> Result<(), StoreError> {
        let order_id = record.order_id();
        if let Some(previous) = Self::get_pswap_lineage(conn, order_id)? {
            Self::remove_setting(
                conn,
                SettingScope::Client,
                &tip_key(previous.current_tip_note_id),
            )?;
        }
        Self::set_client_setting(conn, &order_key(order_id), record)?;
        if record.state == PswapLineageState::Active {
            Self::set_client_setting(conn, &tip_key(record.current_tip_note_id), &order_id)?;
        }
        Ok(())
    }
}
