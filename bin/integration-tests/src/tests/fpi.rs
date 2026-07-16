use std::collections::BTreeMap;

use anyhow::{Context, Result};
use miden_client::account::component::{AccountComponent, AccountComponentMetadata};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountType,
    PartialAccount,
    PartialStorage,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
};
use miden_client::assembly::CodeBuilder;
use miden_client::auth::{
    Approver,
    AuthSchemeId,
    AuthSecretKey,
    AuthSingleSig,
    RPO_FALCON_SCHEME_ID,
};
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::testing::common::*;
use miden_client::transaction::{AdviceInputs, ForeignAccount, TransactionRequestBuilder};
use miden_client::{Felt, Word};
use tracing::info;

use crate::tests::config::ClientConfig;

// FPI TESTS
// ================================================================================================

pub(crate) const MAP_KEY: [Felt; 4] = [
    Felt::new_unchecked(15),
    Felt::new_unchecked(15),
    Felt::new_unchecked(15),
    Felt::new_unchecked(15),
];
pub(crate) const MAP_SLOT_NAME: &str = "miden::testing::fpi::map";
pub(crate) const FPI_STORAGE_VALUE: [Felt; 4] = [
    Felt::new_unchecked(9u64),
    Felt::new_unchecked(12u64),
    Felt::new_unchecked(18u64),
    Felt::new_unchecked(30u64),
];

pub async fn test_standard_fpi_public(client_config: ClientConfig) -> Result<()> {
    standard_fpi(AccountType::Public, client_config, RPO_FALCON_SCHEME_ID).await
}

pub async fn test_standard_fpi_private(client_config: ClientConfig) -> Result<()> {
    standard_fpi(AccountType::Private, client_config, RPO_FALCON_SCHEME_ID).await
}

pub async fn test_fpi_execute_program(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    client.sync_state().await?;

    // Deploy a foreign account
    let (foreign_account, proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        "
            use miden::protocol::active_account
            @account_procedure
            pub proc get_fpi_map_item
                # inputs are passed as foreign_procedure_inputs:
                # [slot_id_prefix, slot_id_suffix, KEY, pad(10)]
                exec.active_account::get_map_item
            end"
        .to_string(),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let foreign_account_id = foreign_account.id();
    let code = format!(
        "
        use miden::protocol::tx
        use miden::core::sys
        const MAP_STORAGE_SLOT = word(\"{MAP_SLOT_NAME}\")
        @transaction_script
        pub proc main
            # pad the stack for the foreign procedure inputs
            padw padw push.0.0

            # push the key of the desired storage item
            push.{map_key}

            # push the slot name of the desired storage item
            push.MAP_STORAGE_SLOT[0..2]

            # push the root of the `get_fpi_map_item` account procedure
            push.{proc_root}

            # push the foreign account id
            push.{account_id_prefix} push.{account_id_suffix}
            # => [foreign_id_suffix, foreign_id_prefix, FOREIGN_PROC_ROOT,
            #     slot_id_prefix, slot_id_suffix, KEY, pad(10)]

            exec.tx::execute_foreign_procedure
            # => [VALUE, pad(12)]

            exec.sys::truncate_stack
        end
        ",
        map_key = Word::from(MAP_KEY),
        account_id_prefix = foreign_account_id.prefix().as_u64(),
        account_id_suffix = foreign_account_id.suffix(),
    );

    let tx_script = client.code_builder().compile_tx_script(&code)?;
    client.sync_state().await?;

    let map_slot_name = StorageSlotName::new(MAP_SLOT_NAME).expect("slot name should be valid");
    let storage_requirements =
        AccountStorageRequirements::new([(map_slot_name, &[StorageMapKey::new(MAP_KEY.into())])]);

    // We create a new client here to force the creation of a new, fresh prover with no previous
    // MAST forest data.
    let (mut client2, keystore2) = client_config.clone().into_client().await?;

    // NOTE: Syncing the client is important because the client needs to be beyond the account
    // creation block
    client2.sync_state().await?;

    let (wallet, ..) =
        insert_new_wallet(&mut client2, AccountType::Private, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    let output_stack = client2
        .execute_program(
            wallet.id(),
            tx_script,
            AdviceInputs::default(),
            BTreeMap::from([(
                foreign_account_id,
                ForeignAccount::public(foreign_account_id, storage_requirements)?,
            )]),
        )
        .await?;

    let mut expected_stack = [Felt::ZERO; 16];
    expected_stack[..4].copy_from_slice(&FPI_STORAGE_VALUE);

    assert_eq!(output_stack, expected_stack);
    Ok(())
}

pub async fn test_nested_fpi_calls(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;

    let (inner_foreign_account, inner_proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        "
            use miden::protocol::active_account
            @account_procedure
            pub proc get_fpi_map_item
                # inputs are passed as foreign_procedure_inputs:
                # [slot_id_prefix, slot_id_suffix, KEY, pad(10)]
                exec.active_account::get_map_item
            end"
        .to_string(),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let inner_foreign_account_id = inner_foreign_account.id();

    let (outer_foreign_account, outer_proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        format!(
            "
            use miden::protocol::tx
            use miden::core::sys
            const STORAGE_MAP_SLOT = word(\"{MAP_SLOT_NAME}\")
            @account_procedure
            pub proc get_fpi_map_item
                # The outer foreign procedure receives foreign_procedure_inputs(16) on the stack.
                # We need to set up the inner FPI call with map key and slot as inputs.

                # pad the stack for the inner foreign procedure inputs
                padw padw push.0.0

                # push the key of the desired storage item
                push.{map_key}

                # push the slot name of the desired storage item
                push.STORAGE_MAP_SLOT[0..2]

                # push the hash of the inner account procedure
                push.{inner_proc_root}

                # push the foreign account id
                push.{account_id_prefix} push.{account_id_suffix}
                # => [foreign_id_suffix, foreign_id_prefix, FOREIGN_PROC_ROOT,
                #     slot_id_prefix, slot_id_suffix, KEY, pad(10)]

                exec.tx::execute_foreign_procedure
                # => [VALUE, pad(12)]

                # add one to the first element of the result
                add.1

                # truncate any remaining stack items to ensure stack depth is 16
                exec.sys::truncate_stack
            end
            ",
            map_key = Word::from(MAP_KEY),
            account_id_prefix = inner_foreign_account_id.prefix().as_u64(),
            account_id_suffix = inner_foreign_account_id.suffix(),
        ),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let outer_foreign_account_id = outer_foreign_account.id();

    info!(inner_id = %inner_foreign_account_id, outer_id = %outer_foreign_account_id, "Executing nested FPI call");

    let tx_script = format!(
        "
        use miden::protocol::tx
        use miden::core::sys
        @transaction_script
        pub proc main
            # pad the stack for the outer foreign procedure inputs (it doesn't use inputs directly)
            padw padw padw push.0.0.0.0

            # push the root of the outer account procedure
            push.{outer_proc_root}

            # push the foreign account id
            push.{account_id_prefix} push.{account_id_suffix}
            # => [foreign_id_suffix, foreign_id_prefix, FOREIGN_PROC_ROOT, pad(16)]

            exec.tx::execute_foreign_procedure
            # => [result(16)]

            # assert the top word equals FPI_STORAGE_VALUE + 1
            push.{fpi_value} add.1 assert_eqw

            # truncate any remaining stack items
            exec.sys::truncate_stack
        end
        ",
        fpi_value = Word::from(FPI_STORAGE_VALUE),
        account_id_prefix = outer_foreign_account_id.prefix().as_u64(),
        account_id_suffix = outer_foreign_account_id.suffix(),
    );

    let tx_script = client.code_builder().compile_tx_script(&tx_script)?;
    client.sync_state().await?;

    // Create transaction request with FPI
    let builder = TransactionRequestBuilder::new().custom_script(tx_script);

    // We will require slot 0, key `MAP_KEY` as well as account proof
    let map_slot_name = StorageSlotName::new(MAP_SLOT_NAME).expect("slot name should be valid");
    let storage_requirements =
        AccountStorageRequirements::new([(map_slot_name, &[StorageMapKey::new(MAP_KEY.into())])]);

    let foreign_accounts = [
        ForeignAccount::public(inner_foreign_account_id, storage_requirements.clone())?,
        ForeignAccount::public(outer_foreign_account_id, storage_requirements)?,
    ];

    let tx_request = builder.foreign_accounts(foreign_accounts).build()?;

    // We create a new client here to force the creation of a new, fresh prover with no previous
    // MAST forest data.
    let (mut client2, keystore2) = client_config.clone().into_client().await?;

    let (native_account, ..) =
        insert_new_wallet(&mut client2, AccountType::Public, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    _ = client2.submit_new_transaction(native_account.id(), tx_request).await?;

    Ok(())
}

/// Tests that foreign accounts are lazily loaded via RPC when not specified upfront
/// in the `TransactionRequestBuilder`.
pub async fn test_lazy_fpi_loading(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;

    // Create a simple foreign account with a constant-returning procedure.
    let constant_value: Word =
        [Felt::from(9u32), Felt::from(12u32), Felt::from(18u32), Felt::from(30u32)].into();

    let (foreign_account, proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        format!(
            r#"
            @account_procedure
            pub proc get_constant
                push.{constant_value}
                swapw dropw
            end"#,
        ),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let foreign_account_id = foreign_account.id();

    // Build FPI transaction script.
    let tx_script = format!(
        "
        use miden::protocol::tx
        @transaction_script
        pub proc main
            push.{proc_root}
            push.{account_id_prefix} push.{account_id_suffix}
            exec.tx::execute_foreign_procedure
            push.{constant_value} assert_eqw
        end
        ",
        account_id_prefix = foreign_account_id.prefix().as_u64(),
        account_id_suffix = foreign_account_id.suffix(),
    );
    let tx_script = client.code_builder().compile_tx_script(&tx_script)?;
    client.sync_state().await?;

    // Wait for blocks so the account is committed on-chain.
    wait_for_blocks(&mut client, 2).await;

    // Create a new client to ensure no cached data.
    let (mut client2, keystore2) = client_config.clone().into_client().await?;

    client2.sync_state().await?;

    let (native_account, ..) =
        insert_new_wallet(&mut client2, AccountType::Public, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    wait_for_blocks_no_sync(&mut client2, 2).await;

    // Before the transaction there are no cached foreign accounts.
    let cached = client2.test_store().get_foreign_account_code(vec![foreign_account_id]).await?;
    assert!(cached.is_empty());

    // Build request WITHOUT specifying foreign accounts — lazy loading should handle it.
    let tx_request = TransactionRequestBuilder::new().custom_script(tx_script).build()?;

    let _ = client2.submit_new_transaction(native_account.id(), tx_request).await?;

    // After the transaction the foreign account code should be cached.
    let cached = client2.test_store().get_foreign_account_code(vec![foreign_account_id]).await?;
    assert_eq!(cached.len(), 1);

    Ok(())
}

/// Tests that lazy loading a public foreign account that reads from a storage map works
/// even when no `AccountStorageRequirements` are specified upfront.
///
/// The executor first lazy-loads the foreign account (with empty storage requirements),
/// then when the procedure reads from the storage map, `get_storage_map_witness` detects
/// the cache miss and makes a second RPC call to fetch the storage map entries.
pub async fn test_lazy_fpi_loading_with_storage_map(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;

    // Deploy a foreign account with a storage map (same as standard FPI tests).
    let (foreign_account, proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        format!(
            r#"
            const STORAGE_MAP_SLOT = word("{MAP_SLOT_NAME}")
            @account_procedure
            pub proc get_fpi_map_item
                push.{map_key}
                push.STORAGE_MAP_SLOT[0..2]
                exec.::miden::protocol::active_account::get_map_item
                swapw dropw
            end"#,
            map_key = Word::from(MAP_KEY)
        ),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let foreign_account_id = foreign_account.id();

    let tx_script = format!(
        "
        use miden::protocol::tx
        @transaction_script
        pub proc main
            push.{proc_root}
            push.{account_id_prefix} push.{account_id_suffix}
            exec.tx::execute_foreign_procedure
            push.{fpi_value} assert_eqw
        end
        ",
        fpi_value = Word::from(FPI_STORAGE_VALUE),
        account_id_prefix = foreign_account_id.prefix().as_u64(),
        account_id_suffix = foreign_account_id.suffix(),
    );

    let tx_script = client.code_builder().compile_tx_script(&tx_script)?;
    client.sync_state().await?;

    wait_for_blocks(&mut client, 2).await;

    // Create a new client to ensure no cached data.
    let (mut client2, keystore2) = client_config.clone().into_client().await?;
    client2.sync_state().await?;

    let (native_account, ..) =
        insert_new_wallet(&mut client2, AccountType::Public, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    wait_for_blocks_no_sync(&mut client2, 2).await;

    // Build request WITHOUT specifying the foreign account — lazy loading should handle
    // both the account inputs and the storage map entries via separate RPC calls.
    let tx_request = TransactionRequestBuilder::new().custom_script(tx_script).build()?;

    let _ = client2.submit_new_transaction(native_account.id(), tx_request).await?;

    Ok(())
}

// HELPERS
// ================================================================================================

/// Tests the standard FPI functionality for the given storage mode.
///
/// This function sets up a foreign account with a custom component that retrieves a value from its
/// storage. It then deploys the foreign account and creates a native account to execute a
/// transaction that calls the foreign account's procedure via FPI. The test also verifies that the
/// foreign account's code is correctly cached after the transaction.
async fn standard_fpi(
    account_type: AccountType,
    client_config: ClientConfig,
    auth_scheme: AuthSchemeId,
) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;

    let (foreign_account, proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        account_type,
        "
            use miden::protocol::active_account
            @account_procedure
            pub proc get_fpi_map_item
                # inputs are passed as foreign_procedure_inputs:
                # [slot_id_prefix, slot_id_suffix, KEY, pad(10)]
                exec.active_account::get_map_item
            end"
        .to_string(),
        auth_scheme,
    )
    .await?;

    let foreign_account_id = foreign_account.id();

    info!(foreign_id = %foreign_account_id, "Executing FPI call");

    let tx_script = format!(
        "
        use miden::protocol::tx
        use miden::core::sys
        const STORAGE_MAP_SLOT = word(\"{MAP_SLOT_NAME}\")
        @transaction_script
        pub proc main
            # pad the stack for the foreign procedure inputs
            padw padw push.0.0

            # push the key of the desired storage item
            push.{map_key}

            # push the slot name of the desired storage item
            push.STORAGE_MAP_SLOT[0..2]

            # push the hash of the `get_fpi_map_item` account procedure
            push.{proc_root}

            # push the foreign account id
            push.{account_id_prefix} push.{account_id_suffix}
            # => [foreign_id_suffix, foreign_id_prefix, FOREIGN_PROC_ROOT,
            #     slot_id_prefix, slot_id_suffix, KEY, pad(10)]

            exec.tx::execute_foreign_procedure
            # => [VALUE, pad(12)]

            push.{fpi_value} assert_eqw

            # truncate any remaining stack items
            exec.sys::truncate_stack
        end
        ",
        map_key = Word::from(MAP_KEY),
        fpi_value = Word::from(FPI_STORAGE_VALUE),
        account_id_prefix = foreign_account_id.prefix().as_u64(),
        account_id_suffix = foreign_account_id.suffix(),
    );

    let tx_script = client.code_builder().compile_tx_script(&tx_script)?;
    client.sync_state().await?;

    // Before the transaction there are no cached foreign accounts
    let foreign_accounts =
        client.test_store().get_foreign_account_code(vec![foreign_account_id]).await?;
    assert!(foreign_accounts.is_empty());

    // Create transaction request with FPI
    let builder = TransactionRequestBuilder::new().custom_script(tx_script);

    // We will require slot 0, key `MAP_KEY` as well as account proof
    let map_slot_name = StorageSlotName::new(MAP_SLOT_NAME).expect("slot name should be valid");
    let storage_requirements =
        AccountStorageRequirements::new([(map_slot_name, &[StorageMapKey::new(MAP_KEY.into())])]);

    let foreign_account = if account_type == AccountType::Public {
        ForeignAccount::public(foreign_account_id, storage_requirements)
    } else {
        // Get current foreign account current state from the store (after 1st deployment tx)
        let foreign_account: Account = client
            .get_account(foreign_account_id)
            .await?
            .context("failed to find foreign account after deploying")?;

        let (id, _vault, storage, code, nonce, seed) = foreign_account.into_parts();
        let acc = PartialAccount::new(
            id,
            nonce,
            code,
            PartialStorage::new_full(storage),
            Default::default(),
            seed,
        )?;

        ForeignAccount::private(acc)
    };

    let tx_request = builder.foreign_accounts([foreign_account?]).build()?;

    // We create a new client here to force the creation of a new, fresh prover with no previous
    // MAST forest data.
    let (mut client2, keystore2) = client_config.clone().into_client().await?;

    // NOTE: Syncing the client is important because the client needs to be beyond the account
    // creation block
    client2.sync_state().await?;

    let (native_account, ..) =
        insert_new_wallet(&mut client2, AccountType::Public, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    let block_before_wait = client2.get_sync_height().await.unwrap();
    wait_for_blocks_no_sync(&mut client2, 2).await;

    // Second client should be able to submit a transaction
    // Without being synced to latest state
    let _ = client2.submit_new_transaction(native_account.id(), tx_request).await?;

    // After the transaction the foreign account should be cached (for public accounts only)
    if account_type == AccountType::Public {
        let foreign_accounts =
            client2.test_store().get_foreign_account_code(vec![foreign_account_id]).await?;
        assert_eq!(foreign_accounts.len(), 1);
    }

    let block_after_wait = client2.get_sync_height().await.unwrap();

    // Submitted transaction should not have provoked a sync
    assert_eq!(block_before_wait, block_after_wait);

    client2.sync_state().await?;
    let block_after_sync = client2.get_sync_height().await.unwrap();

    // After syncing with the network, the client should be synced to the latest block
    assert!(block_after_wait < block_after_sync);

    Ok(())
}

/// Builds a foreign account with a custom component that exports the specified code.
///
/// # Returns
///
/// A tuple containing:
/// - `Account` - The constructed foreign account.
/// - `Word` - The seed used to initialize the account.
/// - `Word` - The procedure root of the custom component's procedure.
/// - `AuthSecretKey` - The secret key used for authentication.
fn foreign_account_with_code(
    account_type: AccountType,
    code: String,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, Word, AuthSecretKey)> {
    // store our expected value on map from slot 0 (map key 15)
    let mut storage_map = StorageMap::new();
    storage_map.insert(StorageMapKey::new(MAP_KEY.into()), FPI_STORAGE_VALUE.into())?;

    let map_slot_name = StorageSlotName::new(MAP_SLOT_NAME).expect("slot name should be valid");
    let map_slot = StorageSlot::with_map(map_slot_name, storage_map);
    let component_code = CodeBuilder::default()
        .compile_component_code("miden::testing::fpi_component", code)
        .context("failed to compile foreign account component code")?;
    let get_item_component = AccountComponent::new(
        component_code,
        vec![map_slot],
        AccountComponentMetadata::new("miden::testing::fpi_component"),
    )
    .map_err(|err| anyhow::anyhow!(err))
    .context("failed to create foreign account component")?;

    let (key_pair, auth_component) = match auth_scheme {
        AuthSchemeId::Falcon512Poseidon2 => {
            let key_pair = AuthSecretKey::new_falcon512_poseidon2();
            let auth_component: AccountComponent = AuthSingleSig::new(Approver::new(
                key_pair.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            ))
            .into();
            (key_pair, auth_component)
        },
        AuthSchemeId::EcdsaK256Keccak => {
            let key_pair = AuthSecretKey::new_ecdsa_k256_keccak();
            let auth_component: AccountComponent = AuthSingleSig::new(Approver::new(
                key_pair.public_key().to_commitment(),
                AuthSchemeId::EcdsaK256Keccak,
            ))
            .into();
            (key_pair, auth_component)
        },
        scheme => {
            return Err(anyhow::anyhow!(format!("Unsupported auth scheme ID {}", scheme.as_u8())));
        },
    };

    let account = AccountBuilder::new(Default::default())
        .with_component(get_item_component.clone())
        .with_auth_component(auth_component)
        .account_type(account_type)
        .build_with_schema_commitment()
        .context("failed to build foreign account")?;

    let proc_root = get_item_component
        .mast_forest()
        .procedure_digests()
        .next()
        .context("failed to get procedure root from component MAST forest")?;
    Ok((account, proc_root, key_pair))
}

/// Deploys a foreign account to the network with the specified code and storage mode. The account
/// is also inserted into the client and keystore.
///
/// # Returns
///
/// A tuple containing:
/// - `Account` - The deployed foreign account.
/// - `Word` - The procedure root of the foreign account.
pub(crate) async fn deploy_foreign_account(
    client: &mut TestClient,
    keystore: &FilesystemKeyStore,
    account_type: AccountType,
    code: String,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, Word)> {
    let (foreign_account, proc_root, secret_key) =
        foreign_account_with_code(account_type, code, auth_scheme)?;
    let foreign_account_id = foreign_account.id();

    keystore
        .add_key(&secret_key, foreign_account_id)
        .await
        .with_context(|| "failed to add key to keystore")?;
    client.add_account(&foreign_account, false).await?;

    info!(account_id = %foreign_account_id, ?account_type, "Deploying foreign account");

    let tx_id = client
        .submit_new_transaction(
            foreign_account_id,
            TransactionRequestBuilder::new()
                .build()
                .with_context(|| "failed to build transaction request")?,
        )
        .await?;
    wait_for_tx(client, tx_id).await?;

    // NOTE: We get the new account state here since the first transaction updates the nonce from
    // to 1
    let foreign_account: Account = client.try_get_account(foreign_account_id).await?;

    Ok((foreign_account, proc_root))
}
