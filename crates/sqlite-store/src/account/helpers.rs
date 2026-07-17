//! Helper functions for account operations.

use std::collections::BTreeMap;

use miden_client::account::{
    AccountCode,
    AccountHeader,
    AccountId,
    Address,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
    StorageSlotType,
};
use miden_client::asset::Asset;
use miden_client::store::{AccountStatus, AccountStorageFilter, ClientAccountType, StoreError};
use miden_client::{Deserializable, Serializable, Word};
use rusqlite::types::Value;
use rusqlite::{Connection, Params, params, params_from_iter};

use crate::column_value_as_u64;
use crate::sql_error::SqlResultExt;

pub(crate) struct SerializedHeaderData {
    pub id: Vec<u8>,
    pub nonce: u64,
    pub vault_root: Vec<u8>,
    pub storage_commitment: Vec<u8>,
    pub code_commitment: Vec<u8>,
    pub account_seed: Option<Vec<u8>>,
    pub locked: bool,
}

/// Parse an account header from the provided serialized data.
pub(crate) fn parse_accounts(
    serialized_account_parts: SerializedHeaderData,
) -> Result<(AccountHeader, AccountStatus), StoreError> {
    let SerializedHeaderData {
        id,
        nonce,
        vault_root,
        storage_commitment,
        code_commitment,
        account_seed,
        locked,
    } = serialized_account_parts;
    let account_seed = account_seed.map(|seed| Word::read_from_bytes(&seed[..])).transpose()?;

    let status = match (account_seed, locked) {
        (seed, true) => AccountStatus::Locked { seed },
        (Some(seed), _) => AccountStatus::New { seed },
        _ => AccountStatus::Tracked,
    };

    let nonce = miden_client::Felt::new(nonce).expect("stored nonce must be a valid Felt");
    Ok((
        AccountHeader::new(
            AccountId::read_from_bytes(&id)
                .expect("Conversion from stored AccountID should not panic"),
            nonce,
            Word::read_from_bytes(&vault_root)?,
            Word::read_from_bytes(&storage_commitment)?,
            Word::read_from_bytes(&code_commitment)?,
        ),
        status,
    ))
}

/// Fetches rows from `latest_account_headers`. Each row includes the [`ClientAccountType`],
/// which `historical_account_headers` doesn't carry — that's why this query lives separately
/// from [`query_historical_account_headers`].
pub(crate) fn query_latest_account_headers(
    conn: &Connection,
    where_clause: &str,
    params: impl Params,
) -> Result<Vec<(AccountHeader, AccountStatus, ClientAccountType)>, StoreError> {
    let query = format!(
        "SELECT id, nonce, vault_root, storage_commitment, code_commitment, account_seed, locked, watched \
         FROM latest_account_headers WHERE {where_clause}"
    );
    conn.prepare(&query)
        .into_store_error()?
        .query_map(params, |row| {
            let id: Vec<u8> = row.get(0)?;
            let nonce: u64 = column_value_as_u64(row, 1)?;
            let vault_root: Vec<u8> = row.get(2)?;
            let storage_commitment: Vec<u8> = row.get(3)?;
            let code_commitment: Vec<u8> = row.get(4)?;
            let account_seed: Option<Vec<u8>> = row.get(5)?;
            let locked: bool = row.get(6)?;
            let watched: bool = row.get(7)?;

            Ok((
                SerializedHeaderData {
                    id,
                    nonce,
                    vault_root,
                    storage_commitment,
                    code_commitment,
                    account_seed,
                    locked,
                },
                watched,
            ))
        })
        .into_store_error()?
        .map(|result| {
            let (parts, watched) = result.into_store_error()?;
            let (header, status) = parse_accounts(parts)?;
            let client_type = if watched {
                ClientAccountType::Watched
            } else {
                ClientAccountType::Native
            };
            Ok((header, status, client_type))
        })
        .collect::<Result<Vec<_>, StoreError>>()
}

pub(crate) fn query_historical_account_headers(
    conn: &Connection,
    where_clause: &str,
    params: impl Params,
) -> Result<Vec<(AccountHeader, AccountStatus)>, StoreError> {
    let query = format!(
        "SELECT id, nonce, vault_root, storage_commitment, code_commitment, account_seed, locked \
         FROM historical_account_headers WHERE {where_clause}"
    );
    conn.prepare(&query)
        .into_store_error()?
        .query_map(params, |row| {
            let id: Vec<u8> = row.get(0)?;
            let nonce: u64 = column_value_as_u64(row, 1)?;
            let vault_root: Vec<u8> = row.get(2)?;
            let storage_commitment: Vec<u8> = row.get(3)?;
            let code_commitment: Vec<u8> = row.get(4)?;
            let account_seed: Option<Vec<u8>> = row.get(5)?;
            let locked: bool = row.get(6)?;

            Ok(SerializedHeaderData {
                id,
                nonce,
                vault_root,
                storage_commitment,
                code_commitment,
                account_seed,
                locked,
            })
        })
        .into_store_error()?
        .map(|result| parse_accounts(result.into_store_error()?))
        .collect::<Result<Vec<(AccountHeader, AccountStatus)>, StoreError>>()
}

// TODO: this function will probably be refactored to receive more complex where clauses and
// return multiple mast forests
pub(super) fn query_account_code(
    conn: &Connection,
    commitment: Word,
) -> Result<Option<AccountCode>, StoreError> {
    const CODE_QUERY: &str = "SELECT code FROM account_code WHERE commitment = ?";

    conn.prepare_cached(CODE_QUERY)
        .into_store_error()?
        .query_map(params![commitment.to_bytes()], |row| {
            let code: Vec<u8> = row.get(0)?;
            Ok(code)
        })
        .into_store_error()?
        .map(|result| {
            let bytes: Vec<u8> = result.into_store_error()?;
            Ok(AccountCode::read_from_bytes(&bytes)?)
        })
        .next()
        .transpose()
}

pub(crate) fn query_account_addresses(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Vec<Address>, StoreError> {
    const ADDRESS_QUERY: &str = "SELECT address FROM addresses WHERE account_id = ?";

    conn.prepare_cached(ADDRESS_QUERY)
        .into_store_error()?
        .query_map(params![account_id.to_bytes()], |row| {
            let address: Vec<u8> = row.get(0)?;
            Ok(address)
        })
        .into_store_error()?
        .map(|result| {
            let serialized_address = result.into_store_error()?;
            let address = Address::read_from_bytes(&serialized_address)?;
            Ok(address)
        })
        .collect::<Result<Vec<Address>, StoreError>>()
}

pub(crate) fn query_vault_assets(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Vec<Asset>, StoreError> {
    const VAULT_QUERY: &str =
        "SELECT vault_id, asset FROM latest_account_assets WHERE account_id = ?";

    conn.prepare(VAULT_QUERY)
        .into_store_error()?
        .query_map(params![account_id.to_bytes()], |row| {
            let vault_id: Vec<u8> = row.get(0)?;
            let asset: Vec<u8> = row.get(1)?;
            Ok((vault_id, asset))
        })
        .into_store_error()?
        .map(|result| {
            let (vault_id_bytes, asset_bytes): (Vec<u8>, Vec<u8>) = result.into_store_error()?;
            let key_word = Word::read_from_bytes(&vault_id_bytes)?;
            let value_word = Word::read_from_bytes(&asset_bytes)?;
            Ok(Asset::from_id_and_value_words(key_word, value_word)?)
        })
        .collect::<Result<Vec<Asset>, StoreError>>()
}

pub(crate) fn query_storage_slots(
    conn: &Connection,
    account_id: AccountId,
    filter: &AccountStorageFilter,
) -> Result<BTreeMap<StorageSlotName, StorageSlot>, StoreError> {
    // Build storage values query with filter pushed to SQL
    let base_query =
        "SELECT slot_name, slot_value, slot_type FROM latest_account_storage WHERE account_id = ?1";
    let mut values_params: Vec<Value> = vec![Value::Blob(account_id.to_bytes())];
    let query = match filter {
        AccountStorageFilter::All => base_query.to_string(),
        AccountStorageFilter::SlotName(name) => {
            values_params.push(Value::Text(name.to_string()));
            format!("{base_query} AND slot_name = ?2")
        },
        AccountStorageFilter::SlotNames(names) => {
            if names.is_empty() {
                return Ok(BTreeMap::new());
            }
            let placeholders =
                (0..names.len()).map(|i| format!("?{}", i + 2)).collect::<Vec<_>>().join(", ");
            for name in names {
                values_params.push(Value::Text(name.to_string()));
            }
            format!("{base_query} AND slot_name IN ({placeholders})")
        },
        AccountStorageFilter::Root(root) => {
            values_params.push(Value::Blob(root.to_bytes()));
            format!("{base_query} AND slot_value = ?2")
        },
    };

    let mut stmt = conn.prepare(&query).into_store_error()?;
    let storage_values = stmt
        .query_map(params_from_iter(values_params.iter()), |row| {
            let slot_name: String = row.get(0)?;
            let value: Vec<u8> = row.get(1)?;
            let slot_type: u8 = row.get(2)?;
            Ok((slot_name, value, slot_type))
        })
        .into_store_error()?
        .map(|result| {
            let (slot_name, value, slot_type) = result.into_store_error()?;
            let slot_name = StorageSlotName::new(slot_name)
                .map_err(|err| StoreError::ParsingError(err.to_string()))?;
            let slot_type = StorageSlotType::try_from(slot_type)
                .map_err(|e| StoreError::ParsingError(e.to_string()))?;
            Ok((slot_name, Word::read_from_bytes(&value)?, slot_type))
        })
        .collect::<Result<Vec<(StorageSlotName, Word, StorageSlotType)>, StoreError>>()?;

    // Restrict map entries query by slot name(s) when the filter narrows by name, so we don't
    // load map entries we'll discard.
    let map_filter: Option<Vec<String>> = match filter {
        AccountStorageFilter::SlotName(name) => Some(vec![name.to_string()]),
        AccountStorageFilter::SlotNames(names) => {
            Some(names.iter().map(StorageSlotName::to_string).collect())
        },
        AccountStorageFilter::All | AccountStorageFilter::Root(_) => None,
    };

    let has_map_slots = storage_values.iter().any(|(_, _, t)| *t == StorageSlotType::Map);
    let mut storage_maps = if has_map_slots {
        query_storage_maps(conn, account_id, map_filter.as_deref())?
    } else {
        BTreeMap::new()
    };

    Ok(storage_values
        .into_iter()
        .map(|(slot_name, value, slot_type)| {
            let key = slot_name.clone();
            let slot = match slot_type {
                StorageSlotType::Value => StorageSlot::with_value(slot_name, value),
                StorageSlotType::Map => StorageSlot::with_map(
                    slot_name.clone(),
                    storage_maps.remove(&slot_name).unwrap_or(StorageMap::new()),
                ),
            };
            (key, slot)
        })
        .collect())
}

pub(crate) fn query_storage_maps(
    conn: &Connection,
    account_id: AccountId,
    slot_name_filter: Option<&[String]>,
) -> Result<BTreeMap<StorageSlotName, StorageMap>, StoreError> {
    let base_query =
        "SELECT slot_name, key, value FROM latest_storage_map_entries WHERE account_id = ?1";
    let mut map_params: Vec<Value> = vec![Value::Blob(account_id.to_bytes())];
    let query = match slot_name_filter {
        Some(names) => {
            if names.is_empty() {
                return Ok(BTreeMap::new());
            }
            let placeholders =
                (0..names.len()).map(|i| format!("?{}", i + 2)).collect::<Vec<_>>().join(", ");
            for name in names {
                map_params.push(Value::Text(name.clone()));
            }
            format!("{base_query} AND slot_name IN ({placeholders})")
        },
        None => base_query.to_string(),
    };

    let mut stmt = conn.prepare(&query).into_store_error()?;
    let map_entries = stmt
        .query_map(params_from_iter(map_params.iter()), |row| {
            let slot_name: String = row.get(0)?;
            let key: Vec<u8> = row.get(1)?;
            let value: Vec<u8> = row.get(2)?;

            Ok((slot_name, key, value))
        })
        .into_store_error()?
        .map(|result| {
            let (slot_name, key, value) = result.into_store_error()?;
            let slot_name = StorageSlotName::new(slot_name)
                .map_err(|err| StoreError::ParsingError(err.to_string()))?;
            Ok((
                slot_name,
                StorageMapKey::new(Word::read_from_bytes(&key)?),
                Word::read_from_bytes(&value)?,
            ))
        })
        .collect::<Result<Vec<(StorageSlotName, StorageMapKey, Word)>, StoreError>>()?;

    let mut maps = BTreeMap::new();
    for (slot_name, key, value) in map_entries {
        let map = maps.entry(slot_name).or_insert_with(StorageMap::new);
        map.insert(key, value)?;
    }

    Ok(maps)
}

pub(crate) fn query_storage_values(
    conn: &Connection,
    account_id: AccountId,
) -> Result<BTreeMap<StorageSlotName, (StorageSlotType, Word)>, StoreError> {
    const STORAGE_QUERY: &str =
        "SELECT slot_name, slot_value, slot_type FROM latest_account_storage WHERE account_id = ?";

    conn.prepare(STORAGE_QUERY)
        .into_store_error()?
        .query_map(params![account_id.to_bytes()], |row| {
            let slot_name: String = row.get(0)?;
            let value: Vec<u8> = row.get(1)?;
            let slot_type: u8 = row.get(2)?;
            Ok((slot_name, value, slot_type))
        })
        .into_store_error()?
        .map(|result| {
            let (slot_name, value, slot_type) = result.into_store_error()?;
            let slot_name = StorageSlotName::new(slot_name)
                .map_err(|err| StoreError::ParsingError(err.to_string()))?;
            let slot_type = StorageSlotType::try_from(slot_type)
                .map_err(|e| StoreError::ParsingError(e.to_string()))?;
            Ok((slot_name, (slot_type, Word::read_from_bytes(&value)?)))
        })
        .collect()
}
