use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail, ensure};
use miden_client::account::component::{
    AccessControl,
    AccountComponent,
    AccountComponentMetadata,
    AuthNetworkAccount,
    Authority,
    BasicWallet,
    UpgradeManager,
};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountCode,
    AccountId,
    AccountType,
    StorageSlot,
};
use miden_client::assembly::CodeBuilder;
use miden_client::auth::{Approver, AuthSingleSig, RPO_FALCON_SCHEME_ID};
use miden_client::note::{AccountCodeUpgradeAttachment, NoteScriptRoot, P2idNote, UpgradeNote};
use miden_client::testing::common::{AccountSetup, TestClient, auth_component};
use miden_client::testing::standards::account_component::MockProceduresComponent;
use miden_client::transaction::{TransactionRequest, TransactionRequestBuilder};
use miden_client::{Felt, Word, ZERO};
use rand::Rng;

use super::network_transaction::{COUNTER_CONTRACT, COUNTER_SLOT_NAME, zero_fee_policy_manager};
use crate::ClientConfig;
use crate::fee_funding::fee_faucet_id;

// HELPERS
// ================================================================================================

/// Returns a component without storage whose procedure no other component has. Code with this
/// component has a different commitment, and the same storage layout.
fn upgrade_marker_component() -> Result<AccountComponent> {
    let code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::upgrade_marker",
            "@account_procedure\npub proc marker push.7 drop end",
        )
        .context("failed to compile the upgrade marker component")?;

    AccountComponent::new(
        code,
        vec![],
        AccountComponentMetadata::new("miden::testing::upgrade_marker"),
    )
    .map_err(|err| anyhow!(err))
    .context("failed to create the upgrade marker component")
}

/// Returns `components` plus [`upgrade_marker_component`] as account code.
fn upgraded_code(components: Vec<AccountComponent>) -> Result<AccountCode> {
    let mut components = components;
    components.push(upgrade_marker_component()?);
    AccountCode::from_components(&components).context("failed to build the upgraded account code")
}

/// Returns the counter component, whose `increment_count` procedure changes its storage slot.
fn counter_component() -> Result<AccountComponent> {
    let code = CodeBuilder::default()
        .compile_component_code("miden::testing::counter_contract", COUNTER_CONTRACT)
        .context("failed to compile the counter component")?;

    AccountComponent::new(
        code,
        vec![StorageSlot::with_empty_value(COUNTER_SLOT_NAME.clone())],
        AccountComponentMetadata::new("miden::testing::counter_component"),
    )
    .map_err(|err| anyhow!(err))
    .context("failed to create the counter component")
}

/// Returns a request that upgrades the code of the executing account to `code`. If
/// `increment_counter` is set, the same transaction also increments the counter in storage, so the
/// request needs a custom script.
fn upgrade_request(
    client: &TestClient,
    code: &AccountCode,
    increment_counter: bool,
) -> Result<TransactionRequest> {
    if !increment_counter {
        return TransactionRequestBuilder::new()
            .build_account_code_upgrade(code.clone())
            .context("failed to build the upgrade transaction request");
    }

    let new_code_commitment = code.commitment();
    let tx_script = client
        .code_builder()
        .with_linked_module("external_contract::counter_contract", COUNTER_CONTRACT)?
        .compile_tx_script(format!(
            "use miden::standards::account_upgrade
            use external_contract::counter_contract

            @transaction_script
            pub proc main
                padw push.{new_code_commitment}
                # => [NEW_CODE_COMMITMENT, STORAGE_UPGRADE_COMMITMENT]

                call.account_upgrade::upgrade
                dropw dropw

                call.counter_contract::increment_count
            end"
        ))
        .context("failed to compile the upgrade transaction script")?;

    TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .account_code_upgrade(code.clone())
        .build()
        .context("failed to build the upgrade transaction request")
}

/// Returns the account as the client store has it.
async fn stored_account(client: &mut TestClient, account_id: AccountId) -> Result<Account> {
    client
        .test_store()
        .get_account(account_id)
        .await?
        .with_context(|| format!("account {account_id} is not tracked"))?
        .try_into()
        .context("failed to read the stored account")
}

/// Returns the components of an upgradeable network account owned by `owner`.
///
/// The account allowlists the upgrade note, and P2ID so that its deploy can consume a funding note.
/// The `Ownable2Step` owner is the only sender whose upgrade notes the account accepts.
async fn upgradeable_network_components(
    client: &TestClient,
    owner: AccountId,
) -> Result<Vec<AccountComponent>> {
    let roots: BTreeSet<NoteScriptRoot> =
        [UpgradeNote::script_root(), P2idNote::script_root()].into_iter().collect();
    let fee_policy_manager =
        zero_fee_policy_manager(fee_faucet_id(client).await?, roots.iter().copied());
    let auth = AuthNetworkAccount::new(roots, fee_policy_manager)
        .map_err(|err| anyhow!(err))
        .context("failed to build the network account auth component")?;

    Ok(auth
        .into_iter()
        .chain(AccessControl::Ownable2Step { owner })
        .chain([UpgradeManager.into(), BasicWallet.into()])
        .collect())
}

/// Deploys a network account that a new wallet owns. The owner sends an upgrade note whose new code
/// adds `extra_components`. The note must carry the code in `expected_num_chunks` attachments. The
/// network transaction builder consumes the note and upgrades the code. The client syncs the
/// account and stores the new code.
async fn network_account_code_upgrade_via_upgrade_note(
    client_config: ClientConfig,
    extra_components: Vec<AccountComponent>,
    expected_num_chunks: usize,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.sync_state().await?;

    let owner = client.insert_wallet(AccountType::Public).await?;

    let components = upgradeable_network_components(&client, owner.id()).await?;
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let network_account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_components(components.clone())
        .build_with_schema_commitment()
        .context("failed to build the network account")?;
    client.add_account(&network_account, false).await?;
    client.deploy_account(network_account.id()).await?;

    let upgraded_code = upgraded_code(components.into_iter().chain(extra_components).collect())?;
    ensure!(
        upgraded_code.commitment() != network_account.code().commitment(),
        "the upgraded code must be different from the current code"
    );

    let request = TransactionRequestBuilder::new()
        .build_upgrade_note(owner.id(), network_account.id(), upgraded_code.clone(), client.rng())
        .context("failed to build the upgrade note request")?;
    let notes = request.expected_output_own_notes();
    let [note] = notes.as_slice() else {
        bail!("the request should create exactly one upgrade note");
    };
    let num_chunks = note
        .attachments()
        .iter()
        .filter(|attachment| {
            attachment.attachment_scheme() == AccountCodeUpgradeAttachment::ATTACHMENT_SCHEME
        })
        .count();
    ensure!(
        num_chunks == expected_num_chunks,
        "the upgrade note should carry the code in {expected_num_chunks} attachments, not {num_chunks}"
    );
    client.execute_tx_and_sync(owner.id(), request).await?;

    // Wait until the network transaction builder consumes the upgrade note.
    let mut upgraded = false;
    for _ in 0..15 {
        client.sync_state().await?;
        if client.account_reader(network_account.id()).code_commitment().await?
            == upgraded_code.commitment()
        {
            upgraded = true;
            break;
        }
        client.wait_for_blocks(1).await?;
    }
    ensure!(upgraded, "the network account should have the upgraded code after sync");

    let stored = stored_account(&mut client, network_account.id()).await?;
    assert_eq!(stored.code(), &upgraded_code);

    let node_account = client
        .test_rpc_api()
        .get_account_details(network_account.id())
        .await?
        .context("the node should return the details of the public account")?;
    assert_eq!(node_account.code().commitment(), upgraded_code.commitment());
    assert_eq!(stored.to_commitment(), node_account.to_commitment());

    Ok(())
}

// TESTS
// ================================================================================================

/// The owner of a network account sends an upgrade note to it. The network transaction builder
/// consumes the note and upgrades the code. The client syncs the account and stores the new code.
pub async fn test_network_account_code_upgrade_via_upgrade_note(
    client_config: ClientConfig,
) -> Result<()> {
    network_account_code_upgrade_via_upgrade_note(client_config, vec![], 1).await
}

/// The owner of a network account sends an upgrade note with new code that does not fit into one
/// note attachment. The note carries the code in two attachments. The network transaction builder
/// consumes the note and upgrades the code. The client syncs the account and stores the new code.
pub async fn test_network_account_code_upgrade_via_two_chunk_upgrade_note(
    client_config: ClientConfig,
) -> Result<()> {
    network_account_code_upgrade_via_upgrade_note(
        client_config,
        vec![MockProceduresComponent::new(150).into()],
        2,
    )
    .await
}

/// A public account upgrades its code, and then upgrades back to the original code and increments a
/// counter in the same transaction. The client that executes the upgrades stores each new code and
/// storage. A second client that tracks the account gets each new code and storage during sync.
///
/// On a chain that charges fees, each transaction also pays its fee from the vault, so the patch of
/// each upgrade also changes the vault.
pub async fn test_public_account_code_upgrade_syncs_to_other_client(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client_1 = client_config.clone().into_client().await?;
    let mut client_2 = client_config.into_client().await?;
    client_1.sync_state().await?;

    let (auth, key) = auth_component(RPO_FALCON_SCHEME_ID)?;
    let public_key_commitment = key.public_key().to_commitment();
    let components = || -> Result<Vec<AccountComponent>> {
        Ok(vec![
            AuthSingleSig::new(Approver::new(public_key_commitment, RPO_FALCON_SCHEME_ID)).into(),
            BasicWallet.into(),
            Authority::AuthControlled.into(),
            UpgradeManager.into(),
            counter_component()?,
        ])
    };

    let mut init_seed = [0u8; 32];
    client_1.rng().fill_bytes(&mut init_seed);
    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(auth)
        .with_components(components()?.into_iter().skip(1))
        .build_with_schema_commitment()
        .context("failed to build the upgradeable account")?;
    let original_code = account.code().clone();
    let upgraded_code = upgraded_code(components()?)?;

    let (account, _) = client_1.insert_account(AccountSetup::prebuilt(account, key)).await?;
    client_1.deploy_account(account.id()).await?;
    client_2.import_account_by_id(account.id()).await?;

    let steps = [(&upgraded_code, false, 0u32), (&original_code, true, 1u32)];
    for (new_code, increment_counter, expected_count) in steps {
        let request = upgrade_request(&client_1, new_code, increment_counter)?;
        client_1.execute_tx_and_sync(account.id(), request).await?;
        let expected_counter = Word::from([Felt::from(expected_count), ZERO, ZERO, ZERO]);

        let stored_1 = stored_account(&mut client_1, account.id()).await?;
        assert_eq!(stored_1.code(), new_code);
        assert_eq!(stored_1.storage().get_item(&COUNTER_SLOT_NAME)?, expected_counter);

        client_2.sync_state().await?;
        let stored_2 = stored_account(&mut client_2, account.id()).await?;
        assert_eq!(stored_2.code(), new_code);
        assert_eq!(stored_2.storage().get_item(&COUNTER_SLOT_NAME)?, expected_counter);
        assert_eq!(stored_2.to_commitment(), stored_1.to_commitment());

        let node_account = client_1
            .test_rpc_api()
            .get_account_details(account.id())
            .await?
            .context("the node should return the details of the public account")?;
        assert_eq!(node_account.to_commitment(), stored_1.to_commitment());
    }

    Ok(())
}
