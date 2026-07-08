use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};
use std::vec;

use anyhow::{Context, Result, anyhow, ensure};
use miden_client::account::component::{
    AccessControl,
    AccountComponent,
    AccountComponentMetadata,
    AuthNetworkAccount,
    BurnPolicy,
    FungibleFaucet,
    MintPolicy,
    PausableManager,
    TokenName,
    TokenPolicyManager,
};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
    StorageSlot,
    StorageSlotName,
};
use miden_client::assembly::{CodeBuilder, Library, Module, ModuleKind, Path, SourceManagerSync};
use miden_client::asset::{AssetAmount, FungibleAsset, TokenSymbol};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::crypto::FeltRng;
use miden_client::note::{
    MintNote,
    MintNoteStorage,
    NetworkAccountTarget,
    Note,
    NoteAssets,
    NoteAttachment,
    NoteAttachments,
    NoteDetailsCommitment,
    NoteExecutionHint,
    NoteRecipient,
    NoteScript,
    NoteScriptRoot,
    NoteStorage,
    NoteTag,
    NoteType,
    P2idNoteStorage,
    PartialNoteMetadata,
    StandardNote,
};
use miden_client::store::{InputNoteState, NoteFilter};
use miden_client::sync::NoteTagSource;
use miden_client::testing::common::{
    TestClient,
    assert_account_has_single_asset,
    consume_notes,
    execute_tx_and_sync,
    insert_new_wallet,
    wait_for_blocks,
    wait_for_tx,
};
use miden_client::transaction::{TransactionKernel, TransactionRequestBuilder};
use miden_client::{Felt, Word, ZERO};
use rand::{Rng, RngCore};

use crate::tests::config::ClientConfig;

// HELPERS
// ================================================================================================

pub(crate) static COUNTER_SLOT_NAME: LazyLock<StorageSlotName> = LazyLock::new(|| {
    StorageSlotName::new("miden::testing::counter_contract::counter").expect("slot name is valid")
});

const COUNTER_CONTRACT: &str = r#"
        use miden::protocol::active_account
        use miden::protocol::native_account
        use miden::core::word
        use miden::core::sys

        const COUNTER_SLOT = word("miden::testing::counter_contract::counter")

        # => []
        pub proc get_count
            push.COUNTER_SLOT[0..2] exec.active_account::get_item
            exec.sys::truncate_stack
        end

        # => []
        pub proc increment_count
            push.COUNTER_SLOT[0..2] exec.active_account::get_item
            # => [count]
            push.1 add
            # => [count+1]
            push.COUNTER_SLOT[0..2] exec.native_account::set_item
            # => []
            exec.sys::truncate_stack
            # => []
        end"#;

const INCR_NONCE_AUTH_CODE: &str = "
    use miden::protocol::native_account

    @auth_script
    pub proc auth_basic
        exec.native_account::incr_nonce
        drop
    end
";

const INCR_NOTE_SCRIPT_CODE: &str = "
    use external_contract::counter_contract
    @note_script
    pub proc main
        call.counter_contract::increment_count
    end
";

// Minimal no-op tx script: the faucet's `INCR_NONCE_AUTH_CODE` auth
// procedure already increments the nonce, so the script itself needs
// only to satisfy the builder's requirement that _some_ user code runs.
const NOOP_TX_SCRIPT: &str = "
    begin
        push.0 drop
    end
";

// A non-standard "claim to target" note script: it asserts the consuming account is the note's
// target (read from the note's storage) and then moves all of the note's assets into that
// account's vault. It is functionally similar to P2ID but hand-written, so its MAST root differs
// from every standard note script — exactly the case the node's NTX builder cannot resolve without
// the script being pre-registered. The `{nonce}` placeholder is replaced with a per-test value so
// the compiled root is unique per run and can never collide with a previously registered script on
// a shared node.
const NON_STANDARD_CLAIM_NOTE_SCRIPT: &str = r#"
    use miden::protocol::active_account
    use miden::protocol::account_id
    use miden::protocol::active_note
    use miden::standards::wallets::basic->basic_wallet

    @note_script
    pub proc main
        # drop the note arguments
        dropw

        # mix in a per-test nonce so this script's MAST root is unique per run
        push.{nonce} drop

        # write the note storage to memory starting at address 0
        push.0 exec.active_note::get_storage
        # => [num_storage_items]

        # this script expects exactly the 2 storage items of an account id (suffix, prefix)
        eq.2 assert.err="non-standard claim note expects exactly 2 storage items"
        # => []

        # read the target account id from note storage: address 0 holds the suffix and address 1
        # holds the prefix, so load prefix first to leave [suffix, prefix] on the stack
        mem_load.1 mem_load.0
        # => [target_account_id_suffix, target_account_id_prefix]

        exec.active_account::get_id
        # => [account_id_suffix, account_id_prefix, target_account_id_suffix, target_account_id_prefix]

        exec.account_id::is_equal assert.err="consumer is not the note's target account"
        # => []

        # move all of the note's assets into the consuming account's vault
        exec.basic_wallet::add_assets_to_account
        # => []
    end
"#;

/// Deploys a counter contract as a network account that allowlists `allowed_note_script_roots`.
///
/// The standardized allowlist slot (carried by [`AuthNetworkAccount`]) is what makes the node treat
/// the account as a network account and route matching notes to it.
pub(crate) async fn deploy_network_counter_contract(
    client: &mut TestClient,
    allowed_note_script_roots: &[NoteScriptRoot],
) -> Result<Account> {
    let roots = allowed_note_script_roots.iter().copied().collect::<BTreeSet<NoteScriptRoot>>();
    let auth = AuthNetworkAccount::with_allowed_notes(roots)
        .map_err(|err| anyhow::anyhow!(err))
        .context("failed to build network account auth component")?;
    deploy_counter_with_auth(client, auth).await
}

/// Deploys a counter contract as an ordinary public account that consumes notes via user
/// transactions (the node rejects user transactions against network accounts).
pub(crate) async fn deploy_counter_contract(client: &mut TestClient) -> Result<Account> {
    let incr_nonce_auth_code = CodeBuilder::default()
        .compile_component_code("miden::testing::incr_nonce_auth", INCR_NONCE_AUTH_CODE)
        .context("failed to compile increment nonce auth component code")?;
    let incr_nonce_auth = AccountComponent::new(
        incr_nonce_auth_code,
        vec![],
        AccountComponentMetadata::new("miden::testing::incr_nonce_auth"),
    )
    .map_err(|err| anyhow::anyhow!(err))
    .context("failed to create increment nonce auth component")?;
    deploy_counter_with_auth(client, incr_nonce_auth).await
}

/// Builds a public counter contract account with the given auth component and deploys it with an
/// empty transaction; the auth component should bump the nonce from 0 to 1, which makes the account
/// update valid.
async fn deploy_counter_with_auth(
    client: &mut TestClient,
    auth: impl Into<AccountComponent>,
) -> Result<Account> {
    let counter_slot = StorageSlot::with_empty_value(COUNTER_SLOT_NAME.clone());
    let counter_code = CodeBuilder::default()
        .compile_component_code("miden::testing::counter_contract", COUNTER_CONTRACT)
        .context("failed to compile counter contract component code")?;
    let counter_component = AccountComponent::new(
        counter_code,
        vec![counter_slot],
        AccountComponentMetadata::new("miden::testing::counter_component"),
    )
    .map_err(|err| anyhow::anyhow!(err))
    .context("failed to create counter contract component")?;

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let acc = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_component(counter_component)
        .with_auth_component(auth)
        .build_with_schema_commitment()
        .context("failed to build counter contract account")?;

    client.add_account(&acc, false).await?;
    let tx_id = client
        .submit_new_transaction(acc.id(), TransactionRequestBuilder::new().build()?)
        .await?;
    wait_for_tx(client, tx_id).await?;
    Ok(acc)
}

/// Deploys a network fungible faucet owned by `owner_id` and commits its initial state on-chain.
///
/// The faucet authenticates with `AuthNetworkAccount`, which carries the standardized allowlist
/// slot the node's NTX builder uses to route MINT notes to it. Minting is additionally gated
/// note-side: the `mint_and_send` procedure checks that the MINT note sender matches the
/// `Ownable2Step` owner.
async fn deploy_network_fungible_faucet(
    client: &mut TestClient,
    owner_id: AccountId,
) -> Result<Account> {
    // The faucet is a network account: `AuthNetworkAccount` carries the standardized allowlist slot
    // the node uses to route MINT notes to it and enforces that only allowlisted notes are consumed
    // with no tx script. The scriptless deploy transaction below is authorized by this same auth.
    let allowed_roots = [MintNote::script_root()].into_iter().collect::<BTreeSet<_>>();
    let network_auth = AuthNetworkAccount::with_allowed_notes(allowed_roots)
        .map_err(|err| anyhow!("failed to build faucet network-account auth: {err}"))?;

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let symbol = TokenSymbol::new("MNT")?;
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let faucet_component = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(10)
        .max_supply(AssetAmount::new(9_999_999)?)
        .build()
        .map_err(|e| anyhow!("failed to build fungible faucet component: {e}"))?;
    let policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::owner_only())
        .active_burn_policy(BurnPolicy::allow_all())
        .build();
    let faucet = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(network_auth)
        .with_component(faucet_component)
        .with_components(AccessControl::Ownable2Step { owner: owner_id })
        .with_components(policy_manager)
        .with_component(PausableManager)
        .build_with_schema_commitment()
        .map_err(|e| anyhow!("failed to build network faucet: {e}"))?;
    client.add_account(&faucet, false).await?;

    // Scriptless deploy: `AuthNetworkAccount` forbids tx scripts and bumps the nonce on its own, so
    // an empty transaction is enough to register the faucet on-chain.
    let deploy_tx = TransactionRequestBuilder::new().build()?;
    let deploy_tx_id = client.submit_new_transaction(faucet.id(), deploy_tx).await?;
    wait_for_tx(client, deploy_tx_id).await?;

    Ok(faucet)
}

/// Waits for a public note to be observed as `Committed` on `observer`.
///
/// Advances up to `max_blocks` blocks on `block_client`, syncing `observer` after each block and
/// looking the note up by its details commitment. Returns `true` as soon as the note is observed
/// as `Committed` and `false` if the window elapses first.
async fn wait_for_committed_note(
    block_client: &mut TestClient,
    observer: &mut TestClient,
    details_commitment: NoteDetailsCommitment,
    max_blocks: u32,
) -> Result<bool> {
    for _ in 0..max_blocks {
        wait_for_blocks(block_client, 1).await;
        observer.sync_state().await?;
        if let Some(rec) = observer
            .get_input_notes(NoteFilter::DetailsCommitments(vec![details_commitment]))
            .await?
            .pop()
            && matches!(rec.state(), InputNoteState::Committed { .. })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Compiles the [`NON_STANDARD_CLAIM_NOTE_SCRIPT`] with a unique `nonce` and returns it together
/// with the note storage encoding `target`'s account id (the script asserts the consumer matches
/// it). The compiled script is guaranteed non-standard; callers assert that before relying on the
/// script-registration path.
fn build_non_standard_claim_note(
    client: &TestClient,
    target: AccountId,
    nonce: u32,
) -> Result<(NoteScript, NoteStorage)> {
    let script_src = NON_STANDARD_CLAIM_NOTE_SCRIPT.replace("{nonce}", &nonce.to_string());
    let script = client
        .code_builder()
        .compile_note_script(script_src.as_str())
        .context("failed to compile non-standard claim note script")?;
    let storage = NoteStorage::new(vec![target.suffix(), target.prefix().as_felt()])
        .context("failed to build non-standard claim note storage")?;
    Ok((script, storage))
}

/// Builds a MINT note routed to `faucet` whose public output recipient uses the non-standard claim
/// script for `nonce` and targets `target`. Returns the compiled (guaranteed non-standard) script,
/// the MINT note, and the `NoteId` the network transaction is expected to emit for it.
fn build_non_standard_mint(
    client: &mut TestClient,
    faucet: &Account,
    faucet_owner: AccountId,
    target: AccountId,
    amount: Felt,
    nonce: u32,
) -> Result<(NoteScript, Note, NoteDetailsCommitment)> {
    let serial_num = client.rng().draw_word();
    let (custom_script, custom_storage) = build_non_standard_claim_note(client, target, nonce)?;
    assert!(
        StandardNote::from_script(&custom_script).is_none(),
        "the claim script must be non-standard for this test to exercise script pre-registration",
    );
    let recipient = NoteRecipient::new(serial_num, custom_script.clone(), custom_storage);

    let expected_asset = FungibleAsset::new(faucet.id(), amount.as_canonical_u64())?;
    let expected_assets = NoteAssets::new(vec![expected_asset.into()])?;
    let expected_output_commitment = Note::with_attachments(
        expected_assets,
        PartialNoteMetadata::new(faucet.id(), NoteType::Public)
            .with_tag(NoteTag::with_account_target(target)),
        recipient.clone(),
        NoteAttachments::default(),
    )
    .details_commitment();

    let mint_storage = MintNoteStorage::new_public(
        recipient,
        expected_asset,
        NoteTag::with_account_target(target).into(),
    )?;
    let target_ntx = NetworkAccountTarget::new(faucet.id(), NoteExecutionHint::Always)?;
    let mint_note: Note = MintNote::builder()
        .sender(faucet_owner) // must equal the faucet owner, checked by mint_and_send
        .mint_storage(mint_storage)
        .attachment(target_ntx)
        .generate_serial_number(client.rng())
        .build()?
        .into();

    Ok((custom_script, mint_note, expected_output_commitment))
}

// TESTS
// ================================================================================================

/// Deploys a counter contract as a network account, emits bump notes, and verifies the network
/// account consumes them and the counter is bumped.
pub async fn test_counter_contract_ntx(client_config: ClientConfig) -> Result<()> {
    const BUMP_NOTE_NUMBER: u64 = 5;
    let (mut client, keystore) = client_config.into_client().await?;
    client.sync_state().await?;

    let incr_note_root = note_script_root(INCR_NOTE_SCRIPT_CODE, client.source_manager())?;
    let network_account = deploy_network_counter_contract(&mut client, &[incr_note_root]).await?;

    let counter_value = client
        .account_reader(network_account.id())
        .get_storage_item(COUNTER_SLOT_NAME.clone())
        .await
        .context("failed to find network account after deployment")?;
    assert_eq!(counter_value, Word::from([ZERO, ZERO, ZERO, ZERO]));

    let (native_account, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    let mut network_notes = vec![];

    let source_manager = client.source_manager();
    for _ in 0..BUMP_NOTE_NUMBER {
        let network_note = get_network_note(
            native_account.id(),
            network_account.id(),
            source_manager.clone(),
            &mut client.rng(),
        )?;
        network_notes.push(network_note);
    }

    let tx_request = TransactionRequestBuilder::new().own_output_notes(network_notes).build()?;

    execute_tx_and_sync(&mut client, native_account.id(), tx_request).await?;

    // Wait for the node to consume the network notes in subsequent blocks
    let expected_counter = Word::from([Felt::new_unchecked(BUMP_NOTE_NUMBER), ZERO, ZERO, ZERO]);
    for _ in 0..10 {
        let a = client
            .test_rpc_api()
            .get_account_details(network_account.id())
            .await?
            .with_context(|| "account details not available")?;

        if a.storage().get_item(&COUNTER_SLOT_NAME)? == expected_counter {
            return Ok(());
        }

        wait_for_blocks(&mut client, 1).await;
    }

    let a = client
        .test_rpc_api()
        .get_account_details(network_account.id())
        .await?
        .with_context(|| "account details not available")?;

    assert_eq!(a.storage().get_item(&COUNTER_SLOT_NAME)?, expected_counter);
    Ok(())
}

pub async fn test_recall_note_before_ntx_consumes_it(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;
    client.sync_state().await?;

    let incr_note_root = note_script_root(INCR_NOTE_SCRIPT_CODE, client.source_manager())?;
    let network_account = deploy_network_counter_contract(&mut client, &[incr_note_root]).await?;
    // The native account consumes the note via a user-submitted transaction, so it must stay an
    // ordinary public account: the node rejects user transactions against network accounts.
    let native_account = deploy_counter_contract(&mut client).await?;

    let wallet =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?
            .0;

    let network_note = get_network_note(
        wallet.id(),
        network_account.id(),
        client.source_manager(),
        &mut client.rng(),
    )?;
    // Prepare both transactions
    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![network_note.clone()])
        .build()?;

    let bump_result = client.execute_transaction(wallet.id(), tx_request).await?;
    let current_height = client.get_sync_height().await?;
    client.apply_transaction(&bump_result, current_height).await?;

    let tx_request = TransactionRequestBuilder::new()
        .input_notes(vec![(network_note, None)])
        .build()?;

    let consume_result = client.execute_transaction(native_account.id(), tx_request).await?;
    let bump_proven = client.prove_transaction(&bump_result).await?;
    let consume_proven = client.prove_transaction(&consume_result).await?;

    // Submit both transactions
    let _bump_submission_height =
        client.submit_proven_transaction(bump_proven, &bump_result).await?;

    let consume_submission_height =
        client.submit_proven_transaction(consume_proven, &consume_result).await?;
    client.apply_transaction(&consume_result, consume_submission_height).await?;

    wait_for_blocks(&mut client, 2).await;

    // The network account should have original value
    let network_counter = client
        .account_reader(network_account.id())
        .get_storage_item(COUNTER_SLOT_NAME.clone())
        .await
        .context("failed to find network account after recall test")?;
    assert_eq!(network_counter, Word::from([ZERO, ZERO, ZERO, ZERO]));

    // The native account should have the incremented value
    let native_counter = client
        .account_reader(native_account.id())
        .get_storage_item(COUNTER_SLOT_NAME.clone())
        .await
        .context("failed to find native account after recall test")?;
    assert_eq!(native_counter, Word::from([Felt::from(1u32), ZERO, ZERO, ZERO]));
    Ok(())
}

/// After a network account consumes a note (potentially in the same batch it was created),
/// the receiver's `InputNoteReader` should find it as consumed by that account. Validates
/// the erased-notes detection flow end-to-end against a real node.
pub async fn test_note_reader_finds_note_consumed_by_ntx(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;
    client.sync_state().await?;

    let incr_note_root = note_script_root(INCR_NOTE_SCRIPT_CODE, client.source_manager())?;
    let network_account = deploy_network_counter_contract(&mut client, &[incr_note_root]).await?;
    let network_account_id = network_account.id();

    let (sender_account, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    let network_note = get_network_note(
        sender_account.id(),
        network_account_id,
        client.source_manager(),
        &mut client.rng(),
    )?;
    // Captured before `network_note` is moved into the request below, so the consumed note can be
    // matched later by its stable details commitment.
    let details_commitment = network_note.details_commitment();

    let tx_request =
        TransactionRequestBuilder::new().own_output_notes(vec![network_note]).build()?;
    execute_tx_and_sync(&mut client, sender_account.id(), tx_request).await?;

    // Wait for the network account to consume the note (check counter increment).
    let expected_counter = Word::from([Felt::from(2u32), ZERO, ZERO, ZERO]);
    for _ in 0..15 {
        client.sync_state().await?;
        let account_details = client
            .test_rpc_api()
            .get_account_details(network_account_id)
            .await?
            .with_context(|| "account details not available")?;

        if account_details.storage().get_item(&COUNTER_SLOT_NAME)? == expected_counter {
            break;
        }
        wait_for_blocks(&mut client, 1).await;
    }

    client.sync_state().await?;

    let mut reader = client.input_note_reader(network_account_id);
    let mut found = false;
    while let Some(note) = reader.next().await? {
        if note.details_commitment() == details_commitment {
            assert_eq!(
                note.consumer_account(),
                Some(network_account_id),
                "consumer should be the network account"
            );
            found = true;
            break;
        }
    }

    assert!(found, "NoteReader should find the note consumed by the network account");

    Ok(())
}

/// Validates end-to-end against a real node that a note created for a network account is consumed
/// by that account and the client records the consumption.
///
/// The network account consumes the note via same-batch erasure, whose RPC stream carries only the
/// `NoteHeader`. The network-account target lives in the note attachment (not delivered by that
/// stream), so the consumer is not derivable: the note is recorded as consumed with an unknown
/// consumer rather than attributed to the network account. The test therefore asserts the note
/// reaches a consumed state, not the consumer identity.
pub async fn test_network_note_consumed_by_ntx(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;
    client.sync_state().await?;

    let incr_note_root = note_script_root(INCR_NOTE_SCRIPT_CODE, client.source_manager())?;
    let network_account = deploy_network_counter_contract(&mut client, &[incr_note_root]).await?;
    let network_account_id = network_account.id();

    let (sender_account, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    let network_note = get_network_note(
        sender_account.id(),
        network_account_id,
        client.source_manager(),
        &mut client.rng(),
    )?;
    // Captured before `network_note` is moved into the request below, so the consumed note can be
    // matched later by its stable details commitment.
    let details_commitment = network_note.details_commitment();

    let tx_request =
        TransactionRequestBuilder::new().own_output_notes(vec![network_note]).build()?;
    execute_tx_and_sync(&mut client, sender_account.id(), tx_request).await?;

    // Wait for the network account to consume the note (check counter increment).
    let expected_counter = Word::from([Felt::from(2u32), ZERO, ZERO, ZERO]);
    for _ in 0..15 {
        client.sync_state().await?;
        let account_details = client
            .test_rpc_api()
            .get_account_details(network_account_id)
            .await?
            .with_context(|| "account details not available")?;

        if account_details.storage().get_item(&COUNTER_SLOT_NAME)? == expected_counter {
            break;
        }
        wait_for_blocks(&mut client, 1).await;
    }

    // The note is consumed via same-batch erasure, so the consumer is not derivable and the note
    // is recorded as consumed with an unknown consumer. Poll until the client records it consumed.
    let mut consumed = false;
    for _ in 0..10 {
        client.sync_state().await?;
        if let Some(record) = client
            .get_input_notes(NoteFilter::DetailsCommitments(vec![details_commitment]))
            .await?
            .pop()
            && record.is_consumed()
        {
            consumed = true;
            break;
        }
        wait_for_blocks(&mut client, 1).await;
    }

    assert!(
        consumed,
        "network note should be marked consumed after the network account consumes it"
    );

    Ok(())
}

/// End-to-end integration test for the standard MINT note -> network faucet -> public P2ID output
/// note flow.
pub async fn test_ntx_mint_produces_public_p2id(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    let (alice, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;
    let (bob, ..) =
        insert_new_wallet(&mut client_2, AccountType::Public, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;

    let faucet = deploy_network_fungible_faucet(&mut client, alice.id()).await?;

    // Build the standard MINT note. Precompute Bob's P2ID recipient and details commitment so we
    // can poll for the emitted public note on client_2.
    let serial_num = client.rng().draw_word();
    let bob_recipient = P2idNoteStorage::new(bob.id()).into_recipient(serial_num);
    let expected_asset = FungibleAsset::new(faucet.id(), 100)?;
    let expected_assets = NoteAssets::new(vec![expected_asset.into()])?;
    let expected_output_commitment = Note::with_attachments(
        expected_assets,
        PartialNoteMetadata::new(faucet.id(), NoteType::Public)
            .with_tag(NoteTag::with_account_target(bob.id())),
        bob_recipient.clone(),
        NoteAttachments::default(),
    )
    .details_commitment();

    let mint_storage = MintNoteStorage::new_public(
        bob_recipient,
        expected_asset,
        NoteTag::with_account_target(bob.id()).into(),
    )?;

    let target_ntx = NetworkAccountTarget::new(faucet.id(), NoteExecutionHint::Always)?;
    let mint_note: Note = MintNote::builder()
        .sender(alice.id())
        .mint_storage(mint_storage)
        .attachment(target_ntx)
        .generate_serial_number(client.rng())
        .build()?
        .into();

    let mint_tx = TransactionRequestBuilder::new().own_output_notes(vec![mint_note]).build()?;
    execute_tx_and_sync(&mut client, alice.id(), mint_tx).await?;

    ensure!(
        wait_for_committed_note(&mut client, &mut client_2, expected_output_commitment, 15).await?,
        "timed out waiting for committed P2ID note {expected_output_commitment:?} emitted by network faucet"
    );

    Ok(())
}

/// End-to-end NTX integration test for public output notes carrying NON-STANDARD note scripts,
/// covering both the registered (built) and the unregistered (never built) case.
///
/// This is the general case of [`test_ntx_mint_produces_public_p2id`]. There the output is a
/// standard P2ID note, which the node's NTX builder resolves directly, so no script
/// pre-registration is needed. Here an output note uses a custom (non-standard) script, which the
/// NTX builder must resolve from its registry when it builds the public output note, so the script
/// has to be registered on the node first.
///
/// Both cases share one network faucet and one pair of wallets, and each uses its own MINT note
/// with a distinct per-run script nonce so the registered case cannot accidentally satisfy the
/// unregistered one. A network transaction that fails because its output script is missing is not
/// reprocessed once that script is registered later, so the registered case uses a fresh note
/// rather than reusing the unregistered one.
///
/// Flow:
///   1. Alice owns a network `NetworkFungibleFaucet`.
///   2. Registered case: Alice pre-registers a non-standard script via
///      [`TransactionRequestBuilder::expected_ntx_scripts`], waits for it to commit, then mints a
///      note whose public output uses that script. The NTX builder resolves it and emits the public
///      note. Bob imports the expected `NoteId`, observes it `Committed`, consumes it, and ends up
///      holding the minted asset.
///   3. Unregistered case: Alice mints a note whose public output uses a different,
///      never-registered non-standard script. The NTX builder cannot build the public output note,
///      so it never reaches `Committed` on Bob's client over a bounded window.
pub async fn test_ntx_mint_produces_public_note_with_non_standard_script(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    let (alice, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;
    let (bob, ..) =
        insert_new_wallet(&mut client_2, AccountType::Public, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;

    let faucet = deploy_network_fungible_faucet(&mut client, alice.id()).await?;
    let amount = Felt::new_unchecked(100);

    // Registered case: pre-register a non-standard output script via `expected_ntx_scripts` on a
    // trivial no-op tx, then wait for the registration to commit. `execute_tx_and_sync` waits
    // for the no-op tx (committed alongside the registration note) and the extra block adds
    // an indexing margin, so the script is resolvable before the MINT's network transaction
    // runs.
    let registered_nonce: u32 = client.rng().random();
    let (registered_script, registered_mint, registered_output_commitment) =
        build_non_standard_mint(
            &mut client,
            &faucet,
            alice.id(),
            bob.id(),
            amount,
            registered_nonce,
        )?;

    let noop_script = client
        .code_builder()
        .compile_tx_script(NOOP_TX_SCRIPT)
        .context("failed to compile no-op registration tx script")?;
    let register_tx = TransactionRequestBuilder::new()
        .custom_script(noop_script)
        .expected_ntx_scripts(vec![registered_script])
        .build()?;
    execute_tx_and_sync(&mut client, alice.id(), register_tx).await?;
    wait_for_blocks(&mut client, 1).await;

    let registered_mint_tx = TransactionRequestBuilder::new()
        .own_output_notes(vec![registered_mint])
        .build()?;
    execute_tx_and_sync(&mut client, alice.id(), registered_mint_tx).await?;

    // The NTX builder resolves the registered script and emits the public note. Observe it
    // `Committed` on Bob's client.
    ensure!(
        wait_for_committed_note(&mut client, &mut client_2, registered_output_commitment, 15)
            .await?,
        "timed out waiting for committed public note {registered_output_commitment:?} with a non-standard script"
    );

    // Bob consumes the public note; the custom script moves the minted asset into his vault.
    let note: Note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![registered_output_commitment]))
        .await?
        .pop()
        .context("expected the committed public note to be present on Bob's client")?
        .try_into()?;
    let consume_tx_id = consume_notes(&mut client_2, bob.id(), &[note]).await;
    wait_for_tx(&mut client_2, consume_tx_id).await?;

    assert_account_has_single_asset(&client_2, bob.id(), faucet.id(), amount.as_canonical_u64())
        .await;

    // Unregistered case: mint a note whose public output uses a different non-standard script
    // that is never registered. The NTX builder cannot build the public output note, so it
    // never reaches `Committed` over a bounded window.
    let unregistered_nonce: u32 = client.rng().random();
    let (_unregistered_script, unregistered_mint, unregistered_output_id) =
        build_non_standard_mint(
            &mut client,
            &faucet,
            alice.id(),
            bob.id(),
            amount,
            unregistered_nonce,
        )?;
    let unregistered_mint_tx = TransactionRequestBuilder::new()
        .own_output_notes(vec![unregistered_mint])
        .build()?;
    execute_tx_and_sync(&mut client, alice.id(), unregistered_mint_tx).await?;

    ensure!(
        !wait_for_committed_note(&mut client, &mut client_2, unregistered_output_id, 10).await?,
        "NTX must not produce a committed public note for an unregistered non-standard script"
    );

    Ok(())
}

/// Compiles the counter contract library using the provided source manager so that all source
/// spans are registered in the same manager used by the client's executor.
pub(crate) fn counter_contract_library(source_manager: Arc<dyn SourceManagerSync>) -> Arc<Library> {
    let assembler = TransactionKernel::assembler_with_source_manager(source_manager.clone());
    let module = Module::parser(ModuleKind::Library)
        .parse_str(
            Path::new("external_contract::counter_contract"),
            COUNTER_CONTRACT,
            source_manager.clone(),
        )
        .map_err(|err| anyhow!(err))
        .unwrap();
    assembler
        .clone()
        .assemble_library([module])
        .map_err(|err| anyhow!(err))
        .unwrap()
}

/// Compiles a note script (linked against the counter contract library) and returns its script
/// root, used to populate a network account's note-script allowlist. The root must match the note
/// the account is expected to consume, so this compiles the script exactly as
/// [`get_network_note_with_script`] does.
pub(crate) fn note_script_root(
    script: &str,
    source_manager: Arc<dyn SourceManagerSync>,
) -> Result<NoteScriptRoot> {
    let script = CodeBuilder::with_source_manager(source_manager.clone())
        .with_dynamically_linked_library(counter_contract_library(source_manager))?
        .compile_note_script(script)?;
    Ok(script.root())
}

fn get_network_note<T: Rng>(
    sender: AccountId,
    network_account: AccountId,
    source_manager: Arc<dyn SourceManagerSync>,
    rng: &mut T,
) -> Result<Note> {
    get_network_note_with_script(
        sender,
        network_account,
        INCR_NOTE_SCRIPT_CODE,
        source_manager,
        rng,
    )
}

pub(crate) fn get_network_note_with_script<T: Rng>(
    sender: AccountId,
    network_account: AccountId,
    script: &str,
    source_manager: Arc<dyn SourceManagerSync>,
    rng: &mut T,
) -> Result<Note> {
    let target = NetworkAccountTarget::new(network_account, NoteExecutionHint::Always)?;
    let attachment: NoteAttachment = target.into();
    let attachments = NoteAttachments::new(vec![attachment])?;
    let partial_metadata = PartialNoteMetadata::new(sender, NoteType::Public)
        .with_tag(NoteTag::with_account_target(network_account));

    let script = CodeBuilder::with_source_manager(source_manager.clone())
        .with_dynamically_linked_library(counter_contract_library(source_manager))?
        .compile_note_script(script)?;
    let recipient = NoteRecipient::new(
        Word::new([
            Felt::new_unchecked(rng.random()),
            Felt::new_unchecked(rng.random()),
            Felt::new_unchecked(rng.random()),
            Felt::new_unchecked(rng.random()),
        ]),
        script,
        NoteStorage::new(vec![])?,
    );

    let network_note =
        Note::with_attachments(NoteAssets::new(vec![])?, partial_metadata, recipient, attachments);
    Ok(network_note)
}

/// Watched-account flow against a network account:
///   - `client_1` deploys the counter as a network account and emits bump notes.
///   - `client_2` watches the network account via `import_watched_account_by_id` (no note tag).
///   - The node-driven counter increments are observed by `client_2` after `sync_state`.
pub async fn test_watch_network_account(client_config: ClientConfig) -> Result<()> {
    const BUMP_NOTE_NUMBER: u64 = 3;

    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, _keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    client_1.sync_state().await?;

    let incr_note_root = note_script_root(INCR_NOTE_SCRIPT_CODE, client_1.source_manager())?;
    let network_account = deploy_network_counter_contract(&mut client_1, &[incr_note_root]).await?;
    let network_account_id = network_account.id();

    // Sanity: counter is 0 after deployment (the deploy transaction carries no script).
    let counter_value = client_1
        .account_reader(network_account_id)
        .get_storage_item(COUNTER_SLOT_NAME.clone())
        .await
        .context("failed to find network account after deployment")?;
    assert_eq!(counter_value, Word::from([ZERO, ZERO, ZERO, ZERO]));

    // client_2 starts watching the network account.
    client_2.import_watched_account_by_id(network_account_id).await?;

    let watched_record = client_2
        .test_store()
        .get_account(network_account_id)
        .await?
        .context("watched network account should be tracked in client_2's store")?;
    assert!(watched_record.is_watched(), "watched network account must be marked as watched");

    let tags = client_2.test_store().get_note_tags().await?;
    assert!(
        !tags
            .iter()
            .any(|t| matches!(t.source, NoteTagSource::Account(id) if id == network_account_id)),
        "watched network account must not register a per-account note tag",
    );

    let initial_watched_commitment =
        client_2.account_reader(network_account_id).commitment().await?;

    // client_1 emits BUMP_NOTE_NUMBER network notes targeted at the counter; the node will
    // consume them in subsequent blocks and bump the counter to BUMP_NOTE_NUMBER.
    let (native_account, ..) =
        insert_new_wallet(&mut client_1, AccountType::Public, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;

    let source_manager = client_1.source_manager();
    let mut network_notes = vec![];
    for _ in 0..BUMP_NOTE_NUMBER {
        let network_note = get_network_note(
            native_account.id(),
            network_account_id,
            source_manager.clone(),
            &mut client_1.rng(),
        )?;
        network_notes.push(network_note);
    }

    let tx_request = TransactionRequestBuilder::new().own_output_notes(network_notes).build()?;
    execute_tx_and_sync(&mut client_1, native_account.id(), tx_request).await?;

    // Poll the watched client until it observes the bumped counter.
    let expected_counter = Word::from([Felt::new_unchecked(BUMP_NOTE_NUMBER), ZERO, ZERO, ZERO]);
    let mut observed = false;
    for _ in 0..10 {
        wait_for_blocks(&mut client_1, 1).await;
        client_2.sync_state().await?;
        let counter = client_2
            .account_reader(network_account_id)
            .get_storage_item(COUNTER_SLOT_NAME.clone())
            .await?;
        if counter == expected_counter {
            observed = true;
            break;
        }
    }
    assert!(
        observed,
        "client_2 should observe the network account state advance via sync_state"
    );

    let source_commitment = client_1.account_reader(network_account_id).commitment().await?;
    let watched_commitment = client_2.account_reader(network_account_id).commitment().await?;
    assert_eq!(
        watched_commitment, source_commitment,
        "watched commitment should track source after node-driven bumps",
    );
    assert_ne!(
        watched_commitment, initial_watched_commitment,
        "watched commitment should have advanced",
    );

    Ok(())
}
