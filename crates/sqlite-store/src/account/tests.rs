use std::collections::BTreeMap;
use std::vec::Vec;

use anyhow::Context;
use miden_client::account::component::{AccountComponent, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountPatch,
    AccountStoragePatch,
    AccountType,
    AccountVaultPatch,
    Address,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
};
use miden_client::assembly::CodeBuilder;
use miden_client::asset::{Asset, FungibleAsset, NonFungibleAsset, NonFungibleAssetDetails};
use miden_client::auth::{AuthSchemeId, AuthSingleSig, PublicKeyCommitment};
use miden_client::store::{ClientAccountType, Store, StoreError};
use miden_client::testing::common::ACCOUNT_ID_REGULAR;
use miden_client::{EMPTY_WORD, Felt, ONE, Serializable, ZERO};
use miden_protocol::account::{
    AccountComponentMetadata,
    StorageMapPatch,
    StorageMapPatchEntries,
    StorageSlotPatch,
    StorageValuePatch,
};
use miden_protocol::asset::AssetCallbackFlag;
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
    ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET,
};
use miden_protocol::testing::constants::NON_FUNGIBLE_ASSET_DATA;
use miden_standards::account::auth::Approver;
use rusqlite::params;

use crate::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::tests::create_test_store;
use crate::transaction::with_forest_snapshot;

#[tokio::test]
async fn account_code_insertion_no_duplicates() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let component_code = CodeBuilder::default()
        .compile_component_code("miden::testing::dummy_component", "pub proc dummy nop end")?;
    let account_component = AccountComponent::new(
        component_code,
        vec![],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;
    let account_code = AccountCode::from_components(&[
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        ))
        .into(),
        account_component,
    ])?;

    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;

            // Table is empty at the beginning
            let mut actual: usize = tx
                .query_row("SELECT Count(*) FROM account_code", [], |row| row.get(0))
                .into_store_error()?;
            assert_eq!(actual, 0);

            // First insertion generates a new row
            SqliteStore::insert_account_code(&tx, &account_code)?;
            actual = tx
                .query_row("SELECT Count(*) FROM account_code", [], |row| row.get(0))
                .into_store_error()?;
            assert_eq!(actual, 1);

            // Second insertion passes but does not generate a new row
            assert!(SqliteStore::insert_account_code(&tx, &account_code).is_ok());
            actual = tx
                .query_row("SELECT Count(*) FROM account_code", [], |row| row.get(0))
                .into_store_error()?;
            assert_eq!(actual, 1);

            Ok(())
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn apply_account_patch_additions() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");
    let map_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::map").expect("valid slot name");
    // Second map slot starts empty (same root as map_slot_name) to verify that
    // modifying only one map slot doesn't corrupt the other when roots collide.
    let map_slot_b_name =
        StorageSlotName::new("miden::testing::sqlite_store::mapB").expect("valid slot name");

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![
            StorageSlot::with_empty_value(value_slot_name.clone()),
            StorageSlot::with_empty_map(map_slot_name.clone()),
            StorageSlot::with_empty_map(map_slot_b_name.clone()),
        ],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    // Create and insert an account
    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    let mut map_entries = StorageMapPatchEntries::new();
    map_entries
        .insert(StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into()), [ONE, ONE, ONE, ONE].into());
    let storage_patch = AccountStoragePatch::from_entries([
        (
            value_slot_name.clone(),
            StorageSlotPatch::Value(StorageValuePatch::Update {
                value: [ZERO, ZERO, ZERO, ONE].into(),
            }),
        ),
        (
            map_slot_name.clone(),
            StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
        ),
    ])?;

    // The account starts with an empty vault, so the absolute values of the added assets are the
    // assets themselves.
    let vault_patch = AccountVaultPatch::with_assets([
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?.into(),
        NonFungibleAsset::new(&NonFungibleAssetDetails::new(
            AccountId::try_from(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET)?,
            NON_FUNGIBLE_ASSET_DATA.into(),
        ))
        .into(),
    ]);

    let patch =
        AccountPatch::new(account.id(), storage_patch, vault_patch, None, Some(Felt::from(2u32)))?;

    let mut account_after_patch = account.clone();
    account_after_patch.apply_patch(&patch)?;

    let account_id = account.id();
    let final_state: AccountHeader = (&account_after_patch).into();
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");

            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &account.into(),
                &final_state,
                &BTreeMap::new(),
                &patch,
            )?;

            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    let updated_account: Account = store
        .get_account(account_id)
        .await?
        .context("failed to find inserted account")?
        .try_into()?;

    assert_eq!(updated_account, account_after_patch);

    // The untouched second map slot must still be empty despite sharing the same
    // initial root as the modified map slot.
    let map_b = updated_account
        .storage()
        .slots()
        .iter()
        .find(|slot| slot.name() == &map_slot_b_name)
        .expect("storage should contain map B");
    let StorageSlotContent::Map(map_b) = map_b.content() else {
        panic!("Expected map slot content");
    };
    assert_eq!(map_b.entries().count(), 0);

    Ok(())
}

/// Regression test: applying a fungible vault patch must preserve the asset's
/// [`AssetCallbackFlag`].
///
/// The callback flag is part of an asset's vault key *and* value encoding, so if the store
/// drops it while applying a patch, the locally recomputed vault root diverges from the one
/// the transaction kernel produced (which carries the flag). That divergence surfaces as a
/// `MerkleStoreError`/`ConflictingRoots` when `apply_account_vault_patch` compares the
/// recomputed root against `final_account_state.vault_root()`.
///
/// Callback-bearing fungible assets are produced by agglayer faucets (B2AGG), so this path
/// is exercised when a wallet consumes an agglayer-minted note. Ordinary assets use the
/// disabled flag, where preserving it is a no-op — which is why only agglayer hit the bug.
#[tokio::test]
async fn apply_account_patch_preserves_fungible_callback_flag() -> anyhow::Result<()> {
    let store = create_test_store().await;

    // Create and insert an account with an empty vault.
    let account = AccountBuilder::new([7; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .build_existing()?;
    store
        .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
        .await?;

    // A fungible asset that carries an *enabled* callback flag (as agglayer-minted assets do).
    let callback_asset: Asset =
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?
            .with_callbacks(AssetCallbackFlag::Enabled)
            .into();

    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let mut vault_patch = AccountVaultPatch::default();
    vault_patch.insert_asset(callback_asset);
    let patch = AccountPatch::new(
        account.id(),
        AccountStoragePatch::new(),
        vault_patch,
        None,
        Some(Felt::from(2u32)),
    )?;

    // `apply_patch` preserves the callback flag, the resulting header carries the authoritative
    // (with-callback) vault root.
    let mut account_after_patch = account.clone();
    account_after_patch.apply_patch(&patch)?;

    let account_id = account.id();
    let final_state: AccountHeader = (&account_after_patch).into();
    let expected_vault_root = final_state.vault_root();
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");

            // Without preserving the callback flag this fails with a `ConflictingRoots`
            // merkle store error (recomputed root != final_state.vault_root()).
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &account.into(),
                &final_state,
                &BTreeMap::new(),
                &patch,
            )?;

            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    let updated_account: Account = store
        .get_account(account_id)
        .await?
        .context("failed to find inserted account")?
        .try_into()?;

    assert_eq!(updated_account, account_after_patch);
    assert_eq!(updated_account.vault().root(), expected_vault_root);

    Ok(())
}

#[tokio::test]
async fn apply_account_patch_removals() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");
    let map_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::map").expect("valid slot name");

    let mut dummy_map = StorageMap::new();
    dummy_map
        .insert(StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into()), [ONE, ONE, ONE, ONE].into())?;

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![
            StorageSlot::with_value(value_slot_name.clone(), [ZERO, ZERO, ZERO, ONE].into()),
            StorageSlot::with_map(map_slot_name.clone(), dummy_map),
        ],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    // Create and insert an account
    let assets: Vec<Asset> = vec![
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?.into(),
        NonFungibleAsset::new(&NonFungibleAssetDetails::new(
            AccountId::try_from(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET)?,
            NON_FUNGIBLE_ASSET_DATA.into(),
        ))
        .into(),
    ];
    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .with_assets(assets.clone())
        .build_existing()?;
    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // A removed map entry is represented by an empty value for the key.
    let mut map_entries = StorageMapPatchEntries::new();
    map_entries.insert(StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into()), EMPTY_WORD);
    let storage_patch = AccountStoragePatch::from_entries([
        // A cleared value slot is represented by an empty value.
        (
            value_slot_name.clone(),
            StorageSlotPatch::Value(StorageValuePatch::Update { value: EMPTY_WORD }),
        ),
        (
            map_slot_name.clone(),
            StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
        ),
    ])?;

    // Both assets are removed: the absolute final state is an empty vault, so each asset's vault
    // key is marked as removed.
    let mut vault_patch = AccountVaultPatch::default();
    for asset in &assets {
        vault_patch.remove_asset(asset.vault_key());
    }

    let patch =
        AccountPatch::new(account.id(), storage_patch, vault_patch, None, Some(Felt::from(2u32)))?;

    let mut account_after_patch = account.clone();
    account_after_patch.apply_patch(&patch)?;

    let account_id = account.id();
    let final_state: AccountHeader = (&account_after_patch).into();

    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots =
                SqliteStore::get_storage_map_roots_for_patch(conn, account.id(), patch.storage())?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");

            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &account.into(),
                &final_state,
                &old_map_roots,
                &patch,
            )?;

            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    let updated_account: Account = store
        .get_account(account_id)
        .await?
        .context("failed to find inserted account")?
        .try_into()?;

    assert_eq!(updated_account, account_after_patch);
    assert!(updated_account.vault().is_empty());
    assert_eq!(updated_account.storage().get_item(&value_slot_name)?, EMPTY_WORD);
    let map_slot = updated_account
        .storage()
        .slots()
        .iter()
        .find(|slot| slot.name() == &map_slot_name)
        .expect("storage should contain map slot");
    let StorageSlotContent::Map(updated_map) = map_slot.content() else {
        panic!("Expected map slot content");
    };
    assert_eq!(updated_map.entries().count(), 0);

    Ok(())
}

#[tokio::test]
async fn get_account_storage_item_success() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");
    let test_value: [miden_client::Felt; 4] = [ONE, ONE, ONE, ONE];

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_value(value_slot_name.clone(), test_value.into())],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Test get_account_storage_item
    let result = store.get_account_storage_item(account.id(), value_slot_name).await?;

    assert_eq!(result, test_value.into());

    Ok(())
}

#[tokio::test]
async fn get_account_storage_item_not_found() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_empty_value(value_slot_name)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Test get_account_storage_item with missing slot name
    let missing_name =
        StorageSlotName::new("miden::testing::sqlite_store::missing").expect("valid slot name");
    let result = store.get_account_storage_item(account.id(), missing_name).await;

    assert!(result.is_err());

    Ok(())
}

#[tokio::test]
async fn get_account_map_item_success() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let map_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::map").expect("valid slot name");

    let test_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
    let test_value: miden_client::Word = [ONE, ONE, ONE, ONE].into();

    let mut storage_map = StorageMap::new();
    storage_map.insert(test_key, test_value)?;

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(map_slot_name.clone(), storage_map)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Test get_account_map_item
    let (value, _witness) =
        store.get_account_map_item(account.id(), map_slot_name, test_key).await?;

    assert_eq!(value, test_value);

    Ok(())
}

#[tokio::test]
async fn get_account_map_item_value_slot_error() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_empty_value(value_slot_name.clone())],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Test get_account_map_item on a value slot (should error)
    let test_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
    let result = store.get_account_map_item(account.id(), value_slot_name, test_key).await;

    assert!(result.is_err());

    Ok(())
}

#[tokio::test]
async fn get_account_code() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    let code = store.get_account_code(account.id()).await?;

    assert!(code.is_some());
    let code = code.unwrap();
    assert_eq!(code.commitment(), account.code().commitment());

    Ok(())
}

#[tokio::test]
async fn get_account_code_not_found() -> anyhow::Result<()> {
    let store = create_test_store().await;

    // Create a valid but non-existent account ID
    let non_existent_id = AccountId::try_from(ACCOUNT_ID_REGULAR)?;

    // Test get_account_code with non-existent account
    let result = store.get_account_code(non_existent_id).await?;

    assert!(result.is_none());

    Ok(())
}

// ACCOUNT READER TESTS
// ================================================================================================

#[tokio::test]
async fn account_reader_nonce_and_status() -> anyhow::Result<()> {
    use std::sync::Arc;

    use miden_client::account::AccountReader;

    let store = Arc::new(create_test_store().await);

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_with_schema_commitment()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Create an AccountReader
    let reader = AccountReader::new(store.clone(), account.id());

    // Test nonce access
    let nonce = reader.nonce().await?;
    assert_eq!(nonce, account.nonce());

    // Test status access
    let status = reader.status().await?;
    assert!(!status.is_locked());
    assert!(status.seed().is_some()); // New account should have a seed

    // Test commitment
    let commitment = reader.commitment().await?;
    assert_eq!(commitment, account.to_commitment());

    Ok(())
}

#[tokio::test]
async fn account_reader_not_found_error() -> anyhow::Result<()> {
    use std::sync::Arc;

    use miden_client::account::AccountReader;

    let store = Arc::new(create_test_store().await);

    // Create a valid but non-existent account ID
    let non_existent_id = AccountId::try_from(ACCOUNT_ID_REGULAR)?;

    // Create an AccountReader for non-existent account
    let reader = AccountReader::new(store.clone(), non_existent_id);

    // Test that header-based methods return AccountDataNotFound error
    let result = reader.nonce().await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), miden_client::ClientError::AccountDataNotFound(_)));

    // Test that status() returns AccountDataNotFound error
    let result = reader.status().await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), miden_client::ClientError::AccountDataNotFound(_)));

    Ok(())
}

#[tokio::test]
async fn account_reader_storage_access() -> anyhow::Result<()> {
    use std::sync::Arc;

    use miden_client::account::AccountReader;

    let store = Arc::new(create_test_store().await);

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");
    let test_value: [miden_client::Felt; 4] = [ONE, ONE, ONE, ONE];

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_value(value_slot_name.clone(), test_value.into())],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    // Create an AccountReader
    let reader = AccountReader::new(store.clone(), account.id());

    // Test storage access via integrated method
    let result = reader.get_storage_item(value_slot_name).await?;

    assert_eq!(result, test_value.into());

    Ok(())
}

#[tokio::test]
async fn account_reader_addresses_access() -> anyhow::Result<()> {
    use std::sync::Arc;

    use miden_client::account::AccountReader;

    let store = Arc::new(create_test_store().await);

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address.clone(), ClientAccountType::Native)
        .await?;

    // Create an AccountReader
    let reader = AccountReader::new(store.clone(), account.id());

    // Test addresses access
    let addresses = reader.addresses().await?;
    assert_eq!(addresses.len(), 1);
    assert_eq!(addresses[0], default_address);

    Ok(())
}

// ACCOUNT HISTORY PRUNE TESTS
// ================================================================================================

#[tokio::test]
async fn prune_account_history_removes_old_committed_states() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::prune::map").expect("valid slot name");

    // Insert account with 5 map entries (nonce 1)
    let mut account = setup_account_with_map(&store, 5, &map_slot_name).await?;
    let account_id = account.id();

    // Apply patch 1 (nonce 1 to 2)
    apply_single_entry_update(&store, &mut account, &map_slot_name, 2).await?;

    // Apply patch 2 (nonce 2 to 3)
    apply_single_entry_update(&store, &mut account, &map_slot_name, 3).await?;

    // Before prune: 2 historical headers (nonce 1, 2).
    // The latest state (nonce 3) is in latest_account_headers, not historical.
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 2);
    assert!(m.historical_storage_map_entries > 0);

    // Prune up to nonce 2 (should delete the nonce-1 historical entry, replaced_at_nonce = 2)
    let deleted = store
        .interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, account_id, Felt::from(2u32))
        })
        .await?;

    assert!(deleted > 0, "Should have deleted some rows");

    // After prune: only 1 historical header remains (nonce 2, replaced_at_nonce = 3).
    // Nonce 3 is in latest_account_headers (not historical).
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 1);

    // Latest tables should be untouched
    assert_eq!(m.latest_account_headers, 1);
    assert!(m.latest_storage_map_entries > 0);

    // The remaining historical header should be nonce 2
    let remaining_nonce: u64 = store
        .interact_with_connection(move |conn| {
            conn.query_row(
                "SELECT nonce FROM historical_account_headers WHERE id = ?",
                params![account_id.to_bytes()],
                |row| crate::column_value_as_u64(row, 0),
            )
            .into_store_error()
        })
        .await?;
    assert_eq!(remaining_nonce, 2);

    // Account data should still be fully readable
    let account_record = store.get_account(account_id).await?;
    assert!(account_record.is_some());

    Ok(())
}

#[tokio::test]
async fn prune_account_history_noop_with_single_state() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::prune_noop::map").expect("valid slot name");

    // Insert account (nonce 1 only)
    let account = setup_account_with_map(&store, 3, &map_slot_name).await?;
    let account_id = account.id();

    let m_before = get_storage_metrics(&store).await;

    // Prune with nonce 1: no historical entries have replaced_at_nonce <= 1
    let deleted = store
        .interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, account_id, Felt::from(1u32))
        })
        .await?;

    assert_eq!(deleted, 0, "Nothing to prune with a single state");

    let m_after = get_storage_metrics(&store).await;
    assert_eq!(m_before.historical_account_headers, m_after.historical_account_headers);

    Ok(())
}

#[tokio::test]
async fn prune_account_history_multiple_accounts() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name_a = StorageSlotName::new("test::prune_all::map_a").expect("valid slot name");
    let map_slot_name_b = StorageSlotName::new("test::prune_all::map_b").expect("valid slot name");

    // Account A: nonce 1 to 2 to 3
    let mut account_a = setup_account_with_map(&store, 3, &map_slot_name_a).await?;
    let a_id = account_a.id();
    apply_single_entry_update(&store, &mut account_a, &map_slot_name_a, 2).await?;
    apply_single_entry_update(&store, &mut account_a, &map_slot_name_a, 3).await?;

    // Account B: different seed  to different account. We need a different builder seed.
    let component_b = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_empty_map(map_slot_name_b.clone())],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;
    let account_b = AccountBuilder::new([1; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component_b)
        .build_existing()?;
    let b_id = account_b.id();
    store
        .insert_account(&account_b, Address::new(account_b.id()), ClientAccountType::Native)
        .await?;

    let mut account_b_mut = account_b.clone();
    apply_single_entry_update(&store, &mut account_b_mut, &map_slot_name_b, 2).await?;

    // Before prune: 2 headers for A (nonce 1, 2) + 1 for B (nonce 1) = 3.
    // Latest states are in latest_account_headers, not historical.
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 3);

    // Prune account A up to nonce 2, account B up to nonce 2
    let deleted_a = store
        .interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, a_id, Felt::from(2u32))
        })
        .await?;
    let deleted_b = store
        .interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, b_id, Felt::from(2u32))
        })
        .await?;

    assert!(deleted_a + deleted_b > 0);

    // After prune: 1 header for A (nonce 2) + 0 for B (nonce 1 was replaced_at_nonce 2, pruned)
    let m = get_storage_metrics(&store).await;
    assert!(m.historical_account_headers <= 2);

    // Both accounts should still be readable
    assert!(store.get_account(account_a.id()).await?.is_some());
    assert!(store.get_account(account_b.id()).await?.is_some());

    Ok(())
}

#[tokio::test]
async fn prune_removes_orphaned_account_code() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::prune_code::map").expect("valid slot name");

    // Insert account with map entries (nonce 1), then apply a patch (nonce 1 to 2).
    // The patch creates a historical header at nonce 1 whose code_commitment points
    // to the original account code.
    let mut account = setup_account_with_map(&store, 2, &map_slot_name).await?;
    let account_id = account.id();
    apply_single_entry_update(&store, &mut account, &map_slot_name, 2).await?;

    // Simulate the nonce-2 state having a different code commitment by updating
    // the latest header's code_commitment directly. This makes the nonce-1
    // historical header the only reference to the original code.
    let original_code_commitment: String = store
        .interact_with_connection(move |conn| {
            conn.query_row(
                "SELECT code_commitment FROM historical_account_headers WHERE id = ?",
                params![account_id.to_bytes()],
                |row| row.get(0),
            )
            .into_store_error()
        })
        .await?;

    // Insert a fake code entry for the latest header so the original code becomes
    // orphaned when we prune the historical header.
    store
        .interact_with_connection(move |conn| {
            conn.execute(
                "INSERT INTO account_code (commitment, code) VALUES (?, ?)",
                params!["new_code_commitment", vec![0u8; 16]],
            )
            .into_store_error()?;
            conn.execute(
                "UPDATE latest_account_headers SET code_commitment = ? WHERE id = ?",
                params!["new_code_commitment", account_id.to_bytes()],
            )
            .into_store_error()?;
            Ok(())
        })
        .await?;

    let code_count_before: usize = store
        .interact_with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM account_code", [], |r| r.get(0))
                .into_store_error()
        })
        .await?;

    // Prune nonce-1 history: the historical header referencing original_code_commitment
    // is deleted, and since no other header references it, the code should be removed.
    let deleted = store
        .interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, account_id, Felt::from(2u32))
        })
        .await?;
    assert!(deleted > 0);

    // Verify the original code was removed
    let original_still_exists: bool = store
        .interact_with_connection(move |conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM account_code WHERE commitment = ?)",
                params![original_code_commitment],
                |row| row.get(0),
            )
            .into_store_error()
        })
        .await?;
    assert!(!original_still_exists, "Original code should be removed after pruning");

    // The new code should still exist
    let code_count_after: usize = store
        .interact_with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM account_code", [], |r| r.get(0))
                .into_store_error()
        })
        .await?;
    assert_eq!(code_count_after, code_count_before - 1);

    Ok(())
}

// TEST HELPERS
// ================================================================================================

/// Row counts across the account-related tables.
struct StorageMetrics {
    latest_account_headers: usize,
    historical_account_headers: usize,
    latest_account_storage: usize,
    latest_storage_map_entries: usize,
    latest_account_assets: usize,
    historical_account_storage: usize,
    historical_storage_map_entries: usize,
    historical_account_assets: usize,
}

impl std::fmt::Display for StorageMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "latest_headers={:<3} hist_headers={:<3} latest_storage={:<3} latest_map={:<3} \
             latest_assets={:<3} hist_storage={:<3} hist_map={:<3} hist_assets={:<3}",
            self.latest_account_headers,
            self.historical_account_headers,
            self.latest_account_storage,
            self.latest_storage_map_entries,
            self.latest_account_assets,
            self.historical_account_storage,
            self.historical_storage_map_entries,
            self.historical_account_assets,
        )
    }
}

async fn get_storage_metrics(store: &SqliteStore) -> StorageMetrics {
    store
        .interact_with_connection(|conn| {
            let count = |table| {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .into_store_error()
            };
            Ok(StorageMetrics {
                latest_account_headers: count("latest_account_headers")?,
                historical_account_headers: count("historical_account_headers")?,
                latest_account_storage: count("latest_account_storage")?,
                latest_storage_map_entries: count("latest_storage_map_entries")?,
                latest_account_assets: count("latest_account_assets")?,
                historical_account_storage: count("historical_account_storage")?,
                historical_storage_map_entries: count("historical_storage_map_entries")?,
                historical_account_assets: count("historical_account_assets")?,
            })
        })
        .await
        .unwrap()
}

/// Creates an account with a storage map of `map_size` entries, inserts it into the store,
/// and returns the account. Uses `Store::insert_account` (public API).
async fn setup_account_with_map(
    store: &SqliteStore,
    map_size: u64,
    map_slot_name: &StorageSlotName,
) -> anyhow::Result<Account> {
    let mut map = StorageMap::new();
    for i in 1..=map_size {
        map.insert(
            StorageMapKey::new([Felt::new_unchecked(i), ZERO, ZERO, ZERO].into()),
            [Felt::new_unchecked(i * 100), ZERO, ZERO, ZERO].into(),
        )?;
    }

    let component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(map_slot_name.clone(), map)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component)
        .build_existing()?;

    store
        .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
        .await?;
    Ok(account)
}

/// Applies a delta that changes a single map entry (key=1) and persists it.
/// `target_nonce` must be strictly greater than the account's current nonce.
async fn apply_single_entry_update(
    store: &SqliteStore,
    account: &mut Account,
    map_slot_name: &StorageSlotName,
    target_nonce: u64,
) -> anyhow::Result<()> {
    let mut map_entries = StorageMapPatchEntries::new();
    map_entries.insert(
        StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into()),
        [Felt::new_unchecked(target_nonce * 1000), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
    )])?;

    let patch = AccountPatch::new(
        account.id(),
        storage_patch,
        AccountVaultPatch::default(),
        None,
        Some(Felt::new_unchecked(target_nonce)),
    )?;

    let prev_header: AccountHeader = (&*account).into();
    account.apply_patch(&patch)?;
    let final_header: AccountHeader = (&*account).into();

    let smt_forest = store.smt_forest.clone();
    let patch_clone = patch.clone();
    let account_id = account.id();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");

            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header,
                &final_header,
                &old_map_roots,
                &patch,
            )?;

            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    Ok(())
}

// UNDO & COMMITMENT LOOKUP TESTS
// ================================================================================================

/// Verifies that `undo_account_state` correctly reverts the latest tables to the previous state.
///
/// The patch includes both storage and vault changes so that the vault root changes between
/// nonce 1 and nonce 2. This is required because `undo_account_state` pops SMT roots from the
/// forest, and the vault root must differ to avoid removing the initial state's root.
#[tokio::test]
async fn undo_account_state_restores_previous_latest() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::undo::map").expect("valid slot name");

    // Insert account with 5 map entries (nonce 1)
    let mut account = setup_account_with_map(&store, 5, &map_slot_name).await?;
    let initial_commitment = account.to_commitment();

    // Apply a patch (nonce 2) that changes a map entry AND adds a fungible asset.
    // The vault change ensures the vault root differs between nonce 1 and 2,
    // which is needed for pop_roots to work correctly.
    let mut map_entries = StorageMapPatchEntries::new();
    map_entries.insert(
        StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into()),
        [Felt::from(1000u32), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
    )])?;
    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let mut vault_patch = AccountVaultPatch::default();
    vault_patch.insert_asset(
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?.into(),
    );
    let patch =
        AccountPatch::new(account.id(), storage_patch, vault_patch, None, Some(Felt::from(2u32)))?;

    let prev_header: AccountHeader = (&account).into();
    account.apply_patch(&patch)?;
    let final_header: AccountHeader = (&account).into();
    let post_patch_commitment = account.to_commitment();

    let smt_forest = store.smt_forest.clone();
    let account_id = account.id();
    let patch_clone = patch.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header,
                &final_header,
                &old_map_roots,
                &patch,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Pre-undo: 1 historical header (old nonce-1 header, replaced_at_nonce=2), 1 latest
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 1);
    assert_eq!(m.latest_account_headers, 1);
    assert_eq!(m.latest_account_assets, 1);

    // Undo the nonce-2 state
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::undo_account_state(
                &tx,
                &mut smt_forest,
                &[(account_id, post_patch_commitment)],
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After undo: historical entries consumed by undo (deleted), latest restored to nonce 1
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 0);
    assert_eq!(m.latest_account_headers, 1);
    assert_eq!(m.latest_storage_map_entries, 5);
    assert_eq!(m.historical_storage_map_entries, 0);
    assert_eq!(m.latest_account_assets, 0, "Vault should be empty after undo to nonce 1");

    // Latest header should reflect nonce 1 with the initial commitment
    let (header, _status) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account should still exist after undo");
    assert_eq!(header.nonce().as_canonical_u64(), 1);
    assert_eq!(header.to_commitment(), initial_commitment);

    Ok(())
}

/// Verifies that undoing the only state (nonce 0) of an account removes it entirely from both
/// latest and historical tables.
///
/// The account is created with assets so the vault root is non-trivial: the SMT forest
/// only ref-counts non-empty roots, so `pop_roots` after undo would underflow on an empty vault.
#[tokio::test]
async fn undo_account_state_deletes_account_entirely() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::undo_del::map").expect("valid slot name");

    // Build account with a map AND an asset so the vault root is non-trivial
    let mut map = StorageMap::new();
    for i in 1..=3u64 {
        map.insert(
            StorageMapKey::new([Felt::new_unchecked(i), ZERO, ZERO, ZERO].into()),
            [Felt::new_unchecked(i * 100), ZERO, ZERO, ZERO].into(),
        )?;
    }
    let component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(map_slot_name.clone(), map)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component)
        .with_assets(vec![
            FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?
                .into(),
        ])
        .build_existing()?;

    let account_id = account.id();
    let commitment = account.to_commitment();
    store
        .insert_account(&account, Address::new(account_id), ClientAccountType::Native)
        .await?;

    // Pre-undo: 1 latest header, 0 historical headers (initial insert has no old state)
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_account_headers, 1);
    assert_eq!(m.historical_account_headers, 0);
    assert!(m.latest_storage_map_entries > 0);
    assert_eq!(m.latest_account_assets, 1);

    // Undo the only state
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::undo_account_state(&tx, &mut smt_forest, &[(account_id, commitment)])?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After undo: all tables should be empty for this account
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_account_headers, 0);
    assert_eq!(m.historical_account_headers, 0);
    assert_eq!(m.latest_account_storage, 0);
    assert_eq!(m.latest_storage_map_entries, 0);
    assert_eq!(m.historical_account_storage, 0);
    assert_eq!(m.historical_storage_map_entries, 0);
    assert_eq!(m.latest_account_assets, 0);
    assert_eq!(m.historical_account_assets, 0);

    // get_account should return None
    let result = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?;
    assert!(result.is_none());

    Ok(())
}

/// Verifies that `lock_account_on_unexpected_commitment` sets `locked = true` in both the
/// latest and historical tables so that the lock survives undo/rebuild.
#[tokio::test]
async fn lock_account_affects_latest_and_historical() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::lock::map").expect("valid slot name");

    // Insert account (nonce 1)
    let mut account = setup_account_with_map(&store, 3, &map_slot_name).await?;
    let account_id = account.id();

    // Apply a patch (nonce 2) with vault change
    let mut map_entries = StorageMapPatchEntries::new();
    map_entries.insert(
        StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into()),
        [Felt::from(2000u32), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
    )])?;
    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let mut vault_patch = AccountVaultPatch::default();
    vault_patch.insert_asset(
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?.into(),
    );
    let patch =
        AccountPatch::new(account.id(), storage_patch, vault_patch, None, Some(Felt::from(2u32)))?;
    let prev_header: AccountHeader = (&account).into();
    account.apply_patch(&patch)?;
    let final_header: AccountHeader = (&account).into();

    let smt_forest = store.smt_forest.clone();
    let patch_clone = patch.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header,
                &final_header,
                &old_map_roots,
                &patch,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Pre-lock: 1 historical header (old nonce-1 header, replaced_at_nonce=2)
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 1);

    // Lock the account with a fake mismatched digest (not matching any historical commitment)
    let fake_digest =
        [Felt::from(999u32), Felt::from(888u32), Felt::from(777u32), Felt::from(666u32)].into();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            SqliteStore::lock_account_on_unexpected_commitment(&tx, &account_id, &fake_digest)?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Latest should be locked
    let (_header, status) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account should exist");
    assert!(status.is_locked(), "Latest header should be locked");

    // Historical entries should also be locked (so rebuild preserves the lock)
    let historical_locked: Vec<bool> = store
        .interact_with_connection(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT locked FROM historical_account_headers WHERE id = ? ORDER BY nonce",
                )
                .into_store_error()?;
            let rows = stmt
                .query_map(params![account_id.to_bytes()], |row| row.get(0))
                .into_store_error()?
                .collect::<Result<Vec<bool>, _>>()
                .into_store_error()?;
            Ok(rows)
        })
        .await?;
    assert_eq!(historical_locked.len(), 1, "Should have 1 historical entry (old nonce-1 state)");
    assert!(historical_locked[0], "Historical nonce-1 should be locked");

    Ok(())
}

/// Verifies that undoing a patch after `update_account_state` does not resurrect entries that
/// were removed by the update. This exercises the archival logic in `update_account_state`.
///
/// Flow:
/// 1. Insert account with map entries {A, B, C} and an asset X at nonce 1
/// 2. Apply patch at nonce 2: add asset Y (changes vault root)
/// 3. `update_account_state` with in-memory state at nonce 3: {A, B} and {X} (C and Y removed)
/// 4. Apply patch at nonce 4: change entry A, add asset Z
/// 5. Undo nonce 4
/// 6. Assert C and Y are not in latest tables
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn undo_after_update_account_state_does_not_resurrect_removed_entries() -> anyhow::Result<()>
{
    let store = create_test_store().await;
    let map_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::map").expect("valid slot name");

    let faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?;
    let nf_faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET)?;

    // Build initial map with 3 entries: A (key=1), B (key=2), C (key=3)
    let key_a = StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into());
    let key_c = StorageMapKey::new([Felt::from(3u32), ZERO, ZERO, ZERO].into());

    let mut initial_map = StorageMap::new();
    initial_map.insert(key_a, [Felt::from(100u32), ZERO, ZERO, ZERO].into())?;
    initial_map.insert(
        StorageMapKey::new([Felt::from(2u32), ZERO, ZERO, ZERO].into()),
        [Felt::from(200u32), ZERO, ZERO, ZERO].into(),
    )?;
    initial_map.insert(key_c, [Felt::from(300u32), ZERO, ZERO, ZERO].into())?;

    let component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(map_slot_name.clone(), initial_map)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    // Build an existing account at nonce 1: no initial assets
    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component)
        .build_existing()?;

    let account_id = account.id();
    store
        .insert_account(&account, Address::new(account_id), ClientAccountType::Native)
        .await?;

    // Step 1+2: Apply patch at nonce 2 adding assets X and Y
    let asset_x = FungibleAsset::new(faucet_id, 100)?;
    let asset_y = NonFungibleAsset::new(&NonFungibleAssetDetails::new(
        nf_faucet_id,
        NON_FUNGIBLE_ASSET_DATA.into(),
    ));

    // The account starts with an empty vault, so the absolute values of the added assets are the
    // assets themselves.
    let vault_patch_1 = AccountVaultPatch::with_assets([asset_x.into(), asset_y.into()]);
    let patch_1 = AccountPatch::new(
        account_id,
        AccountStoragePatch::new(),
        vault_patch_1,
        None,
        Some(Felt::from(2u32)),
    )?;

    let prev_header_1: AccountHeader = (&account).into();
    let mut account_nonce2 = account.clone();
    account_nonce2.apply_patch(&patch_1)?;
    let final_header_2: AccountHeader = (&account_nonce2).into();

    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header_1,
                &final_header_2,
                &BTreeMap::new(),
                &patch_1,
            )?;
            smt_forest.commit_roots(account_id);
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Now: map entries {A, B, C} and assets {X, Y} at nonce 2
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 3, "Should have 3 map entries");
    assert_eq!(m.latest_account_assets, 2, "Should have 2 assets (X + Y)");

    // Step 3: Build in-memory state with only {A, B} and {X} (C and Y removed)
    let mut map_entries_remove = StorageMapPatchEntries::new();
    map_entries_remove.insert(key_c, EMPTY_WORD);
    let storage_patch_remove = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries_remove }),
    )])?;
    // Y is removed, so its vault key is marked as removed (absolute final vault is {X}).
    let mut vault_patch_remove = AccountVaultPatch::default();
    vault_patch_remove.remove_asset(asset_y.vault_key());
    let patch_remove = AccountPatch::new(
        account_id,
        storage_patch_remove,
        vault_patch_remove,
        None,
        Some(Felt::from(3u32)),
    )?;

    let mut account_updated = account_nonce2.clone();
    account_updated.apply_patch(&patch_remove)?;
    let updated_nonce = account_updated.nonce().as_canonical_u64();

    // Call update_account_state with the updated state
    let smt_forest = store.smt_forest.clone();
    let account_updated_clone = account_updated.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::update_account_state(&tx, &mut smt_forest, &account_updated_clone)?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After update: 2 map entries (A, B), 1 asset (X)
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 2, "Should have 2 map entries after update");
    assert_eq!(m.latest_account_assets, 1, "Should have 1 asset after update");

    // Step 4: Apply a patch that changes entry A and adds asset Z
    let mut map_entries_next = StorageMapPatchEntries::new();
    map_entries_next.insert(key_a, [Felt::from(999u32), ZERO, ZERO, ZERO].into());
    let storage_patch_next = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries_next }),
    )])?;

    let asset_z =
        NonFungibleAsset::new(&NonFungibleAssetDetails::new(nf_faucet_id, vec![5, 6, 7, 8]));
    // The vault holds {X} here, so adding Z is the only vault change.
    let mut vault_patch_next = AccountVaultPatch::default();
    vault_patch_next.insert_asset(asset_z.into());

    let patch_next = AccountPatch::new(
        account_id,
        storage_patch_next,
        vault_patch_next,
        None,
        Some(Felt::from(4u32)),
    )?;

    let prev_header: AccountHeader = (&account_updated).into();
    let mut account_next = account_updated.clone();
    account_next.apply_patch(&patch_next)?;
    let final_header: AccountHeader = (&account_next).into();
    let commitment_next = account_next.to_commitment();

    let smt_forest = store.smt_forest.clone();
    let patch_next_clone = patch_next.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_next_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header,
                &final_header,
                &old_map_roots,
                &patch_next,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After patch: 2 map entries (A modified, B unchanged), 2 assets (X + Z)
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 2, "Should have 2 map entries after patch");
    assert_eq!(m.latest_account_assets, 2, "Should have 2 assets after patch (X + Z)");

    // Step 5: Undo the last patch
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::undo_account_state(
                &tx,
                &mut smt_forest,
                &[(account_id, commitment_next)],
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Step 6: Verify C and Y are NOT resurrected
    let m = get_storage_metrics(&store).await;
    assert_eq!(
        m.latest_storage_map_entries, 2,
        "C should NOT be resurrected: only A and B should be in latest"
    );
    assert_eq!(
        m.latest_account_assets, 1,
        "Y should NOT be resurrected: only X should be in latest"
    );

    // Also verify the header reverted to the post-update nonce
    let (header, _) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account should exist");
    assert_eq!(header.nonce().as_canonical_u64(), updated_nonce);

    Ok(())
}

/// Verifies that stale full-account snapshots don't roll a locally newer state back.
#[tokio::test]
async fn update_account_state_rejects_stale_full_snapshot_without_mutating() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::stale_update::map").expect("valid slot name");

    // Insert nonce-1 account, then advance the persisted state to nonce 2.
    let stale_account = setup_account_with_map(&store, 3, &map_slot_name).await?;
    let account_id = stale_account.id();
    let mut current_account = stale_account.clone();
    apply_single_entry_update(&store, &mut current_account, &map_slot_name, 2).await?;
    assert!(stale_account.nonce().as_canonical_u64() < current_account.nonce().as_canonical_u64());
    assert_ne!(stale_account.to_commitment(), current_account.to_commitment());

    let metrics_before_stale_update = get_storage_metrics(&store).await;

    // Feed the older nonce-1 full snapshot through the same path used for public account sync.
    let smt_forest = store.smt_forest.clone();
    let result = store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::update_account_state(&tx, &mut smt_forest, &stale_account)?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await;
    assert!(
        matches!(&result, Err(StoreError::DatabaseError(err)) if err.contains("new nonce 1 is less than old nonce 2")),
        "expected stale update to be rejected before mutating state, got {result:?}"
    );

    let persisted: Account = store
        .get_account(account_id)
        .await?
        .context("account should exist after stale update")?
        .try_into()?;
    assert_eq!(persisted, current_account);

    let metrics_after_stale_update = get_storage_metrics(&store).await;
    assert_eq!(
        metrics_after_stale_update.historical_account_headers,
        metrics_before_stale_update.historical_account_headers,
        "stale update must not archive the current header"
    );
    assert_eq!(
        metrics_after_stale_update.historical_storage_map_entries,
        metrics_before_stale_update.historical_storage_map_entries,
        "stale update must not archive storage entries"
    );

    Ok(())
}

/// Verifies that `get_account_header_by_commitment` retrieves historical states by commitment.
#[tokio::test]
async fn get_account_header_by_commitment_returns_historical() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::commitment::map").expect("valid slot name");

    // Insert account (nonce 1)
    let mut account = setup_account_with_map(&store, 3, &map_slot_name).await?;
    let initial_commitment = account.to_commitment();

    // Apply a patch (nonce 2)
    apply_single_entry_update(&store, &mut account, &map_slot_name, 2).await?;
    let post_patch_commitment = account.to_commitment();
    assert_ne!(initial_commitment, post_patch_commitment);

    // Look up the initial commitment: should find the nonce-1 state in historical
    let lookup = initial_commitment;
    let header = store
        .interact_with_connection(move |conn| {
            SqliteStore::get_account_header_by_commitment(conn, lookup)
        })
        .await?
        .expect("Initial commitment should exist in historical");
    assert_eq!(header.nonce().as_canonical_u64(), 1);
    assert_eq!(header.to_commitment(), initial_commitment);

    // Look up the post-patch commitment: should NOT be in historical (it's the current
    // latest state, not an old one that was replaced)
    let lookup = post_patch_commitment;
    let result = store
        .interact_with_connection(move |conn| {
            SqliteStore::get_account_header_by_commitment(conn, lookup)
        })
        .await?;
    assert!(result.is_none(), "Post-patch commitment should not be in historical");

    Ok(())
}

/// Verifies that undoing multiple nonces at once correctly reverts to the original state.
///
/// Flow:
/// 1. Insert account with 3 map entries at nonce 1
/// 2. Apply patch at nonce 2: change map entry + add asset
/// 3. Apply patch at nonce 3: change another map entry + add different asset
/// 4. Undo both nonces at once (pass both commitments to `undo_account_state`)
/// 5. Verify latest is restored to nonce 1 state
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn undo_multiple_nonces_at_once() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::multi_undo::map").expect("valid slot name");

    let faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?;
    let nf_faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET)?;

    // Insert account with 3 map entries at nonce 1
    let account = setup_account_with_map(&store, 3, &map_slot_name).await?;
    let account_id = account.id();
    let initial_commitment = account.to_commitment();

    // Verify nonce 1 state
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 3, "Initial: 3 map entries");
    assert_eq!(m.latest_account_assets, 0, "Initial: no assets");

    // Apply patch at nonce 2: change map entry key=1, add fungible asset
    let mut map_entries_1 = StorageMapPatchEntries::new();
    map_entries_1.insert(
        StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into()),
        [Felt::from(1000u32), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch_1 = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries_1 }),
    )])?;
    let asset_1 = FungibleAsset::new(faucet_id, 100)?;
    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let mut vault_patch_1 = AccountVaultPatch::default();
    vault_patch_1.insert_asset(asset_1.into());
    let patch_1 = AccountPatch::new(
        account_id,
        storage_patch_1,
        vault_patch_1,
        None,
        Some(Felt::from(2u32)),
    )?;

    let prev_header_1: AccountHeader = (&account).into();
    let mut account_nonce2 = account.clone();
    account_nonce2.apply_patch(&patch_1)?;
    let final_header_2: AccountHeader = (&account_nonce2).into();
    let commitment_nonce2 = account_nonce2.to_commitment();

    let smt_forest = store.smt_forest.clone();
    let patch_1_clone = patch_1.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_1_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header_1,
                &final_header_2,
                &old_map_roots,
                &patch_1,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Apply patch at nonce 3: change map entry key=2, add non-fungible asset
    let mut map_entries_2 = StorageMapPatchEntries::new();
    map_entries_2.insert(
        StorageMapKey::new([Felt::from(2u32), ZERO, ZERO, ZERO].into()),
        [Felt::from(2000u32), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch_2 = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries_2 }),
    )])?;
    let asset_2 = NonFungibleAsset::new(&NonFungibleAssetDetails::new(
        nf_faucet_id,
        NON_FUNGIBLE_ASSET_DATA.into(),
    ));
    // The vault holds {asset_1} here, so adding asset_2 is the only vault change.
    let mut vault_patch_2 = AccountVaultPatch::default();
    vault_patch_2.insert_asset(asset_2.into());
    let patch_2 = AccountPatch::new(
        account_id,
        storage_patch_2,
        vault_patch_2,
        None,
        Some(Felt::from(3u32)),
    )?;

    let prev_header_2: AccountHeader = (&account_nonce2).into();
    let mut account_nonce3 = account_nonce2.clone();
    account_nonce3.apply_patch(&patch_2)?;
    let final_header_3: AccountHeader = (&account_nonce3).into();
    let commitment_nonce3 = account_nonce3.to_commitment();

    let smt_forest = store.smt_forest.clone();
    let patch_2_clone = patch_2.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots = SqliteStore::get_storage_map_roots_for_patch(
                conn,
                account_id,
                patch_2_clone.storage(),
            )?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::apply_account_patch(
                &tx,
                &mut smt_forest,
                &prev_header_2,
                &final_header_3,
                &old_map_roots,
                &patch_2,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // Pre-undo: 2 historical headers (nonce 1 replaced at nonce 2, nonce 2 replaced at nonce 3)
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 2, "Should have 2 historical headers");
    assert_eq!(m.latest_account_assets, 2, "Should have 2 assets at nonce 3");

    // Undo BOTH nonces at once
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::undo_account_state(
                &tx,
                &mut smt_forest,
                &[(account_id, commitment_nonce2), (account_id, commitment_nonce3)],
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After undo: all historical entries consumed, latest restored to nonce 1
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_account_headers, 0, "All historical headers consumed by undo");
    assert_eq!(m.latest_account_headers, 1, "Latest header should still exist");
    assert_eq!(m.latest_storage_map_entries, 3, "All 3 original map entries should be restored");
    assert_eq!(m.historical_storage_map_entries, 0, "No historical map entries should remain");
    assert_eq!(m.latest_account_assets, 0, "Vault should be empty after undo to nonce 1");

    // Verify the header is at nonce 1 with original commitment
    let (header, _) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account should exist after undo");
    assert_eq!(header.nonce().as_canonical_u64(), 1);
    assert_eq!(header.to_commitment(), initial_commitment);

    Ok(())
}

/// Verifies that entries genuinely new in `update_account_state` (not in the previous state)
/// are correctly removed from latest on undo. These entries get NULL `old_value` in historical.
///
/// Flow:
/// 1. Insert account with map entries {A, B} at nonce 1
/// 2. `update_account_state` at nonce 2 with entries {A, B, C, D} (C and D are new)
/// 3. Undo nonce 2
/// 4. Verify C and D are gone from latest, only A and B remain
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn undo_after_update_removes_genuinely_new_entries() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::undo_new::map").expect("valid slot name");

    // Build initial map with 2 entries: A (key=1), B (key=2)
    let key_a = StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into());
    let key_b = StorageMapKey::new([Felt::from(2u32), ZERO, ZERO, ZERO].into());
    let key_c = StorageMapKey::new([Felt::from(3u32), ZERO, ZERO, ZERO].into());
    let key_d = StorageMapKey::new([Felt::from(4u32), ZERO, ZERO, ZERO].into());

    let mut initial_map = StorageMap::new();
    initial_map.insert(key_a, [Felt::from(100u32), ZERO, ZERO, ZERO].into())?;
    initial_map.insert(key_b, [Felt::from(200u32), ZERO, ZERO, ZERO].into())?;

    let component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(map_slot_name.clone(), initial_map)],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component)
        .build_existing()?;

    let account_id = account.id();
    store
        .insert_account(&account, Address::new(account_id), ClientAccountType::Native)
        .await?;

    // Verify nonce 1 state
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 2, "Initial: 2 map entries");

    // Build in-memory state at nonce 2 with {A, B, C, D}: C and D are genuinely new
    let mut map_entries_add = StorageMapPatchEntries::new();
    map_entries_add.insert(key_c, [Felt::from(300u32), ZERO, ZERO, ZERO].into());
    map_entries_add.insert(key_d, [Felt::from(400u32), ZERO, ZERO, ZERO].into());
    let storage_patch_add = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries_add }),
    )])?;

    // Also add an asset so the vault root changes (avoids SMT root collision on undo).
    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?;
    let asset = FungibleAsset::new(faucet_id, 100)?;
    let mut vault_patch_add = AccountVaultPatch::default();
    vault_patch_add.insert_asset(asset.into());
    let patch_add = AccountPatch::new(
        account_id,
        storage_patch_add,
        vault_patch_add,
        None,
        Some(Felt::from(2u32)),
    )?;

    let mut account_updated = account.clone();
    account_updated.apply_patch(&patch_add)?;

    // Call update_account_state with the updated state at nonce 2
    let smt_forest = store.smt_forest.clone();
    let account_updated_clone = account_updated.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::update_account_state(&tx, &mut smt_forest, &account_updated_clone)?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After update: 4 map entries (A, B, C, D), 1 asset
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.latest_storage_map_entries, 4, "Should have 4 map entries after update");
    assert_eq!(m.latest_account_assets, 1, "Should have 1 asset after update");

    // Verify historical has entries for C and D with NULL old_value (genuinely new)
    let null_count: usize = store
        .interact_with_connection(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM historical_storage_map_entries \
                 WHERE account_id = ? AND old_value IS NULL",
                params![account_id.to_bytes()],
                |row| row.get(0),
            )
            .into_store_error()
        })
        .await?;
    assert_eq!(null_count, 2, "Should have 2 NULL old_value entries (C and D) in historical");

    // Undo nonce 2
    let commitment = account_updated.to_commitment();
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            SqliteStore::undo_account_state(&tx, &mut smt_forest, &[(account_id, commitment)])?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;

    // After undo: only A and B should remain, C and D should be gone
    let m = get_storage_metrics(&store).await;
    assert_eq!(
        m.latest_storage_map_entries, 2,
        "Only original entries A and B should remain after undo"
    );
    assert_eq!(
        m.latest_account_assets, 0,
        "Asset should be gone after undo (didn't exist at nonce 1)"
    );
    assert_eq!(
        m.historical_storage_map_entries, 0,
        "Historical map entries should be cleaned up after undo"
    );

    // Verify the header is at nonce 1
    let (header, _) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account should exist after undo");
    assert_eq!(header.nonce().as_canonical_u64(), 1);

    Ok(())
}

// SMT FOREST SNAPSHOT ROLLBACK
// ================================================================================================

/// Builds a non-trivial patch over a freshly inserted account: a value-slot write, a map-slot
/// write, and a vault addition. Sufficient to drive `stage_roots` + multiple SMT mutations in
/// `apply_account_patch`.
fn build_patch_for_snapshot_test(
    account: &Account,
    value_slot_name: StorageSlotName,
    map_slot_name: StorageSlotName,
) -> anyhow::Result<(AccountPatch, Account)> {
    let mut map_entries = StorageMapPatchEntries::new();
    map_entries
        .insert(StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into()), [ONE, ONE, ONE, ONE].into());
    let storage_patch = AccountStoragePatch::from_entries([
        (
            value_slot_name,
            StorageSlotPatch::Value(StorageValuePatch::Update {
                value: [ZERO, ZERO, ZERO, ONE].into(),
            }),
        ),
        (
            map_slot_name,
            StorageSlotPatch::Map(StorageMapPatch::Update { entries: map_entries }),
        ),
    ])?;

    // The account starts with an empty vault, so the absolute value of the added asset is the
    // asset itself.
    let mut vault_patch = AccountVaultPatch::default();
    vault_patch.insert_asset(
        FungibleAsset::new(AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET)?, 100)?.into(),
    );

    let patch =
        AccountPatch::new(account.id(), storage_patch, vault_patch, None, Some(Felt::from(2u32)))?;

    let mut account_after_patch = account.clone();
    account_after_patch.apply_patch(&patch)?;
    Ok((patch, account_after_patch))
}

async fn insert_account_with_storage_for_snapshot_test()
-> anyhow::Result<(SqliteStore, Account, StorageSlotName, StorageSlotName)> {
    let store = create_test_store().await;

    let value_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::value").expect("valid slot name");
    let map_slot_name =
        StorageSlotName::new("miden::testing::sqlite_store::map").expect("valid slot name");

    let dummy_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![
            StorageSlot::with_empty_value(value_slot_name.clone()),
            StorageSlot::with_empty_map(map_slot_name.clone()),
        ],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )?;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(dummy_component)
        .build_existing()?;

    let default_address = Address::new(account.id());
    store
        .insert_account(&account, default_address, ClientAccountType::Native)
        .await?;

    Ok((store, account, value_slot_name, map_slot_name))
}

/// `with_forest_snapshot` must leave the in-memory `AccountSmtForest` unchanged when the
/// closure returns an error, even after `apply_account_patch` has already mutated the
/// working clone (vault tree, storage map tree, and staged roots).
#[tokio::test]
async fn with_forest_snapshot_leaves_forest_unchanged_on_error() -> anyhow::Result<()> {
    let (store, account, value_slot_name, map_slot_name) =
        insert_account_with_storage_for_snapshot_test().await?;

    let (patch, account_after_patch) =
        build_patch_for_snapshot_test(&account, value_slot_name, map_slot_name)?;
    let final_state: AccountHeader = (&account_after_patch).into();

    let forest_arc = store.smt_forest.clone();
    let forest_before = forest_arc.read().expect("read lock").clone();

    let init_header: AccountHeader = (&account).into();
    let smt_forest = forest_arc.clone();
    let outcome = store
        .interact_with_connection(move |conn| {
            with_forest_snapshot(conn, &smt_forest, |tx, forest| {
                SqliteStore::apply_account_patch(
                    tx,
                    forest,
                    &init_header,
                    &final_state,
                    &BTreeMap::new(),
                    &patch,
                )?;
                Err::<(), _>(StoreError::DatabaseError("forced rollback".to_string()))
            })
        })
        .await;

    assert!(matches!(outcome, Err(StoreError::DatabaseError(_))));

    let forest_after = forest_arc.read().expect("read lock").clone();
    assert_eq!(forest_after, forest_before, "forest must be unchanged after a failed closure");

    // The DB transaction was rolled back too; account state is still at nonce 1.
    let (header, _) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account.id()))
        .await?
        .expect("account header present");
    assert_eq!(header.nonce().as_canonical_u64(), 1);

    Ok(())
}

/// `with_forest_snapshot` must NOT touch the forest when the closure returns `Ok` and the
/// SQL commit succeeds. Mutations made inside the closure persist.
#[tokio::test]
async fn with_forest_snapshot_persists_forest_on_success() -> anyhow::Result<()> {
    let (store, account, value_slot_name, map_slot_name) =
        insert_account_with_storage_for_snapshot_test().await?;

    let (patch, account_after_patch) =
        build_patch_for_snapshot_test(&account, value_slot_name, map_slot_name)?;
    let final_state: AccountHeader = (&account_after_patch).into();

    let forest_arc = store.smt_forest.clone();
    let forest_before = forest_arc.read().expect("read lock").clone();

    let init_header: AccountHeader = (&account).into();
    let account_id = account.id();
    let smt_forest = forest_arc.clone();
    store
        .interact_with_connection(move |conn| {
            with_forest_snapshot(conn, &smt_forest, |tx, forest| {
                SqliteStore::apply_account_patch(
                    tx,
                    forest,
                    &init_header,
                    &final_state,
                    &BTreeMap::new(),
                    &patch,
                )
            })
        })
        .await?;

    let forest_after = forest_arc.read().expect("read lock").clone();
    assert_ne!(
        forest_after, forest_before,
        "forest must reflect the staged patch after a successful closure"
    );

    let (header, _) = store
        .interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
        .await?
        .expect("account header present");
    assert_eq!(header.nonce().as_canonical_u64(), 2);

    Ok(())
}

#[tokio::test]
async fn watched_status_survives_state_replacement() -> anyhow::Result<()> {
    let store = create_test_store().await;

    let account = AccountBuilder::new([0; 32])
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(AccountComponent::new(
            BasicWallet::code().as_library().clone(),
            vec![],
            AccountComponentMetadata::new("miden::testing::watched_replace"),
        )?)
        .build_existing()?;
    let account_id = account.id();
    store
        .insert_account(&account, Address::new(account_id), ClientAccountType::Watched)
        .await?;

    // Bump the account's nonce and run it through update_account.
    let mut updated = account.clone();
    let patch = AccountPatch::new(
        account_id,
        AccountStoragePatch::new(),
        AccountVaultPatch::default(),
        None,
        Some(Felt::from(2u32)),
    )?;
    updated.apply_patch(&patch)?;

    store.update_account(&updated).await?;

    let record = store
        .get_account(account_id)
        .await?
        .context("account should still be retrievable after update")?;
    assert!(record.is_watched(), "watched status must survive state replacement");

    Ok(())
}

// STORAGE MAP CREATE/REMOVE PATCH TESTS
// ================================================================================================

/// Applies a storage patch through the low-level store helpers, bypassing `Account::apply_patch`,
/// so map create/remove semantics can be exercised directly against the store tables.
async fn apply_storage_patch_directly(
    store: &SqliteStore,
    account_id: AccountId,
    nonce: u64,
    storage_patch: AccountStoragePatch,
) -> anyhow::Result<()> {
    let smt_forest = store.smt_forest.clone();
    store
        .interact_with_connection(move |conn| {
            let old_map_roots =
                SqliteStore::get_storage_map_roots_for_patch(conn, account_id, &storage_patch)?;
            let tx = conn.transaction().into_store_error()?;
            let mut smt_forest = smt_forest.write().expect("smt_forest write lock not poisoned");
            let updated_slots = SqliteStore::apply_account_storage_patch(
                &mut smt_forest,
                &old_map_roots,
                &storage_patch,
            )?;
            SqliteStore::write_storage_patch(
                &tx,
                account_id,
                nonce,
                &updated_slots,
                &storage_patch,
            )?;
            tx.commit().into_store_error()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Reads the latest map entries of a slot as a `key_hex -> value_hex` map.
async fn read_latest_map_entries(
    store: &SqliteStore,
    account_id: AccountId,
    slot_name: &StorageSlotName,
) -> anyhow::Result<BTreeMap<String, String>> {
    let account_id_bytes = account_id.to_bytes();
    let slot = slot_name.to_string();
    let entries = store
        .interact_with_connection(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT key, value FROM latest_storage_map_entries \
                     WHERE account_id = ? AND slot_name = ?",
                )
                .into_store_error()?;
            let rows = stmt
                .query_map(params![account_id_bytes, slot], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .into_store_error()?;
            let map: BTreeMap<String, String> =
                rows.collect::<Result<_, _>>().into_store_error()?;
            Ok(map)
        })
        .await?;
    Ok(entries)
}

/// Reads the latest top-level value (the map root, for map slots) of a storage slot.
async fn read_slot_value(
    store: &SqliteStore,
    account_id: AccountId,
    slot_name: &StorageSlotName,
) -> anyhow::Result<String> {
    let account_id_bytes = account_id.to_bytes();
    let slot = slot_name.to_string();
    let value = store
        .interact_with_connection(move |conn| {
            conn.query_row(
                "SELECT slot_value FROM latest_account_storage \
                 WHERE account_id = ? AND slot_name = ?",
                params![account_id_bytes, slot],
                |r| r.get::<_, String>(0),
            )
            .into_store_error()
        })
        .await?;
    Ok(value)
}

/// A `Create` patch on an already-populated map slot must discard the prior entries: the resulting
/// root and latest entries reflect only the created entries, not a merge with the old ones.
#[tokio::test]
async fn create_map_patch_replaces_existing_entries() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::create::map").expect("valid slot name");

    // Account starts with 5 entries (keys 1..=5, values i*100).
    let account = setup_account_with_map(&store, 5, &map_slot_name).await?;
    let account_id = account.id();

    // Create the map anew with a different entry set: key 1 changes value, key 6 is new,
    // keys 2..=5 disappear.
    let key1 = StorageMapKey::new([Felt::from(1u32), ZERO, ZERO, ZERO].into());
    let key6 = StorageMapKey::new([Felt::from(6u32), ZERO, ZERO, ZERO].into());
    let val1 = [Felt::from(999u32), ZERO, ZERO, ZERO].into();
    let val6 = [Felt::from(600u32), ZERO, ZERO, ZERO].into();

    let mut map_entries = StorageMapPatchEntries::new();
    map_entries.insert(key1, val1);
    map_entries.insert(key6, val6);
    let storage_patch = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Create { entries: map_entries }),
    )])?;

    apply_storage_patch_directly(&store, account_id, 2, storage_patch).await?;

    // Latest entries must be exactly the created set.
    let latest = read_latest_map_entries(&store, account_id, &map_slot_name).await?;
    let mut expected = StorageMap::new();
    expected.insert(key1, val1)?;
    expected.insert(key6, val6)?;
    let expected_entries: BTreeMap<String, String> =
        expected.entries().map(|(k, v)| (k.to_hex(), v.to_hex())).collect();
    assert_eq!(latest, expected_entries);

    // The stored root must match a map built from only the created entries.
    let root_hex = read_slot_value(&store, account_id, &map_slot_name).await?;
    assert_eq!(root_hex, expected.root().to_hex());

    // Every affected key (union of old {1..5} and new {1,6}) is archived exactly once.
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_storage_map_entries, 6);

    Ok(())
}

/// A `Remove` patch clears the map slot: its latest entries are dropped and its root collapses to
/// the empty-map root.
#[tokio::test]
async fn remove_map_patch_clears_slot() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let map_slot_name = StorageSlotName::new("test::remove::map").expect("valid slot name");

    let account = setup_account_with_map(&store, 5, &map_slot_name).await?;
    let account_id = account.id();

    let storage_patch = AccountStoragePatch::from_entries([(
        map_slot_name.clone(),
        StorageSlotPatch::Map(StorageMapPatch::Remove),
    )])?;

    apply_storage_patch_directly(&store, account_id, 2, storage_patch).await?;

    // No latest entries remain for the slot.
    let latest = read_latest_map_entries(&store, account_id, &map_slot_name).await?;
    assert!(latest.is_empty(), "removed map slot must have no latest entries");

    // The root collapses to the empty-map root.
    let root_hex = read_slot_value(&store, account_id, &map_slot_name).await?;
    assert_eq!(root_hex, StorageMap::new().root().to_hex());

    // The 5 prior entries are archived.
    let m = get_storage_metrics(&store).await;
    assert_eq!(m.historical_storage_map_entries, 5);

    Ok(())
}
