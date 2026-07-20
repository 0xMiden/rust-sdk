use anyhow::{Context, Result};
use miden_client::account::{AccountId, AccountType};
use miden_client::asset::FungibleAsset;
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::crypto::{FeltRng, MerkleStore, MerkleTree, NodeIndex, Poseidon2, RandomCoin};
use miden_client::note::{
    Note,
    NoteAssets,
    NoteRecipient,
    NoteStorage,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
};
use miden_client::store::{NoteFilter, TransactionFilter};
use miden_client::testing::common::*;
use miden_client::transaction::{
    AdviceMap,
    InputNote,
    TransactionRequest,
    TransactionRequestBuilder,
};
use miden_client::utils::{Deserializable, Serializable};
use miden_client::{Felt, Word, ZERO};

use crate::tests::config::ClientConfig;

// CUSTOM TRANSACTION REQUEST
// ================================================================================================
//
// The following functions are for testing custom transaction code. What the test does is:
//
// - Create a custom tx that mints a custom note which checks that the note args are as expected
//   (ie, a word of 8 felts that represent [9, 12, 18, 3, 3, 18, 12, 9])
//      - The args will be provided via the advice map
//
// - Create another transaction that consumes this note with custom code. This custom code only
//   asserts that the {asserted_value} parameter is 0. To test this we first execute with an
//   incorrect value passed in, and after that we try again with the correct value.
//
// Because it's currently not possible to create/consume notes without assets, the P2ID code
// is used as the base for the note code.

const NOTE_ARGS: [Felt; 8] = [
    Felt::new_unchecked(9),
    Felt::new_unchecked(12),
    Felt::new_unchecked(18),
    Felt::new_unchecked(3),
    Felt::new_unchecked(3),
    Felt::new_unchecked(18),
    Felt::new_unchecked(12),
    Felt::new_unchecked(9),
];

pub async fn test_transaction_request(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    client.sync_state().await?;
    // Insert Account
    let (regular_account, _) =
        insert_new_wallet(&mut client, AccountType::Private, &authenticator, RPO_FALCON_SCHEME_ID)
            .await?;

    let (fungible_faucet, _) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // Execute mint transaction in order to create custom note
    let note = mint_custom_note(&mut client, fungible_faucet.id(), regular_account.id()).await?;
    client.sync_state().await?;

    // Prepare transaction

    // If these args were to be modified, the transaction would fail because the note code expects
    // these exact arguments
    let note_args_commitment = Poseidon2::hash_elements(&NOTE_ARGS);

    let note_args_map = vec![(note.clone(), Some(note_args_commitment))];
    let mut advice_map = AdviceMap::default();
    advice_map.insert(note_args_commitment, NOTE_ARGS.to_vec());

    let code = "
        @transaction_script
        pub proc main
            # We use the script argument to store the expected value to be compared
            push.1.2.3.4
            # => [[1,2,3,4], TX_SCRIPT_ARG]
            assert_eqw
        end
        ";
    let tx_script = client.code_builder().compile_tx_script(code)?;

    // FAILURE ATTEMPT
    let transaction_request = TransactionRequestBuilder::new()
        .input_notes(note_args_map.clone())
        .trusted_input_note_script_roots([Word::from(note.script().root())])
        .custom_script(tx_script.clone())
        .script_arg(Word::empty())
        .extend_advice_map(advice_map.clone())
        .build()?;

    // This fails because of {asserted_value} having the incorrect number passed in
    assert!(
        client
            .execute_transaction(regular_account.id(), transaction_request)
            .await
            .is_err()
    );

    // SUCCESS EXECUTION
    let transaction_request = TransactionRequestBuilder::new()
        .input_notes(note_args_map)
        .trusted_input_note_script_roots([Word::from(note.script().root())])
        .custom_script(tx_script)
        .script_arg([Felt::from(4u32), Felt::from(3u32), Felt::from(2u32), Felt::from(1u32)].into())
        .extend_advice_map(advice_map)
        .build()?;

    // TEST CUSTOM SCRIPT SERIALIZATION
    let mut buffer = Vec::new();
    transaction_request.write_into(&mut buffer);

    let deserialized_transaction_request = TransactionRequest::read_from_bytes(&buffer)?;
    assert_eq!(transaction_request, deserialized_transaction_request);

    let tx_id = client.submit_new_transaction(regular_account.id(), transaction_request).await?;
    let transaction = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .with_context(|| "failed to find transaction after submission")?;

    // Assert that the custom note was used in the transaction
    assert!(
        transaction
            .details
            .input_note_nullifiers
            .into_iter()
            .any(|nullifier| nullifier == note.nullifier().as_word())
    );

    wait_for_tx(&mut client, tx_id).await?;

    // Assert that the note was consumed on chain
    let input_note = client
        .get_input_note(note.id())
        .await?
        .context("failed to find input note after consume transaction execution")?;
    assert!(input_note.is_consumed());
    Ok(())
}

pub async fn test_merkle_store(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    client.sync_state().await?;
    // Insert Account
    let (regular_account, _) =
        insert_new_wallet(&mut client, AccountType::Private, &authenticator, RPO_FALCON_SCHEME_ID)
            .await?;

    let (fungible_faucet, _) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    // Execute mint transaction in order to increase nonce
    let note = mint_custom_note(&mut client, fungible_faucet.id(), regular_account.id()).await?;
    client.sync_state().await?;

    // Prepare custom merkle store transaction

    // If these args were to be modified, the transaction would fail because the note code expects
    // these exact arguments
    let note_args_commitment = Poseidon2::hash_elements(&NOTE_ARGS);

    let trusted_script_root = Word::from(note.script().root());
    let note_args_map = vec![(note, Some(note_args_commitment))];
    let mut advice_map = AdviceMap::default();
    advice_map.insert(note_args_commitment, NOTE_ARGS.to_vec());

    // Build merkle store and advice stack with merkle root
    let leaves: Vec<Word> = [1, 2, 3, 4]
        .iter()
        .map(|&v| [Felt::new_unchecked(v), ZERO, ZERO, ZERO].into())
        .collect();
    let num_leaves = leaves.len();
    let merkle_tree = MerkleTree::new(leaves)?;
    let merkle_root = merkle_tree.root();
    let merkle_store: MerkleStore = MerkleStore::from(&merkle_tree);

    let mut code = format!(
        "
         @transaction_script
         pub proc main
             # leaf count -> mem[4000][0]
             push.{num_leaves} push.4000 mem_store

             # merkle root -> mem[4004]
             push.{} push.4004 mem_storew_le dropw
        ",
        merkle_root.to_hex()
    );

    for pos in 0..(num_leaves as u64) {
        let expected_element =
            merkle_store.get_node(merkle_root, NodeIndex::new(2u8, pos)?)?.to_hex();
        code += format!(
            "
            # get element at index `pos` from the merkle store in mem[1000] and push it to stack
            push.4000 push.{pos} exec.::miden::core::collections::mmr::get

            # check the element matches what was inserted at `pos`
            push.{expected_element} assert_eqw.err=\"element in merkle store didn't match expected\"
        "
        )
        .as_str();
    }
    code += "end";
    // Build the transaction
    let tx_script = client.code_builder().compile_tx_script(&code)?;

    let transaction_request = TransactionRequestBuilder::new()
        .input_notes(note_args_map)
        .trusted_input_note_script_roots([trusted_script_root])
        .custom_script(tx_script)
        .extend_advice_map(advice_map)
        .extend_merkle_store(merkle_store.inner_nodes())
        .build()?;

    execute_tx_and_sync(&mut client, regular_account.id(), transaction_request).await?;

    client.sync_state().await?;
    Ok(())
}

pub async fn test_onchain_notes_sync_with_tag(client_config: ClientConfig) -> Result<()> {
    // Client 1 has an private faucet which will mint an onchain note for client 2
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    // Client 2 will be used to sync and check that by adding the tag we can still fetch notes
    // whose tag doesn't necessarily match any of its accounts
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    // Client 3 will be the control client. We won't add any tags and expect the note not to be
    // fetched
    let (mut client_3, ..) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_3).await;

    // Create accounts
    let (basic_account_1, ..) =
        insert_new_wallet(&mut client_1, AccountType::Private, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;
    insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
        .await?;

    client_1.sync_state().await?;
    client_2.sync_state().await?;
    client_3.sync_state().await?;

    // Create the custom note
    let note_script = "
            @note_script
            pub proc main
                push.1 push.1
                assert_eq
            end
            ";
    let note_script = client_1.code_builder().compile_note_script(note_script)?;
    let inputs = NoteStorage::new(vec![])?;
    let serial_num = client_1.rng().draw_word();
    let note_metadata = PartialNoteMetadata::new(basic_account_1.id(), NoteType::Public)
        .with_tag(NoteTag::with_account_target(basic_account_1.id()));
    let note_assets = NoteAssets::new(vec![])?;
    let note_recipient = NoteRecipient::new(serial_num, note_script, inputs);
    let note = Note::new(note_assets, note_metadata, note_recipient);

    // Send transaction and wait for it to be committed
    let tx_request =
        TransactionRequestBuilder::new().own_output_notes(vec![note.clone()]).build()?;

    let note = tx_request
        .expected_output_own_notes()
        .pop()
        .with_context(|| "no expected output notes found in transaction request")?
        .clone();
    execute_tx_and_sync(&mut client_1, basic_account_1.id(), tx_request).await?;

    // Load tag into client 2
    client_2
        .add_note_tag(NoteTag::with_account_target(basic_account_1.id()))
        .await?;

    // Client 2's account should receive the note here:
    client_2.sync_state().await?;
    client_3.sync_state().await?;

    // Assert that the note is the same
    let received_note: InputNote = client_2
        .get_input_note(note.id())
        .await?
        .context("failed to find input note in client 2 after sync")?
        .try_into()?;
    assert_eq!(received_note.note().id(), note.id());
    assert!(client_3.get_input_notes(NoteFilter::All).await?.is_empty());
    Ok(())
}

async fn mint_custom_note(
    client: &mut TestClient,
    faucet_account_id: AccountId,
    target_account_id: AccountId,
) -> Result<Note> {
    // Prepare transaction
    let mut random_coin = RandomCoin::new(Default::default());
    let note = create_custom_note(client, faucet_account_id, target_account_id, &mut random_coin)?;

    let transaction_request =
        TransactionRequestBuilder::new().own_output_notes(vec![note.clone()]).build()?;

    execute_tx_and_sync(client, faucet_account_id, transaction_request).await?;
    Ok(note)
}

// HELPERS
// ================================================================================================

fn create_custom_note(
    client: &TestClient,
    faucet_account_id: AccountId,
    target_account_id: AccountId,
    rng: &mut RandomCoin,
) -> Result<Note> {
    let mem_addr: u32 = 1000;

    let word_1: Word = NOTE_ARGS[0..4].try_into().unwrap();
    let word_2: Word = NOTE_ARGS[4..8].try_into().unwrap();
    let note_script = include_str!("../asm/custom_p2id.masm")
        .replace("{expected_note_arg_1}", &word_1.to_hex())
        .replace("{expected_note_arg_2}", &word_2.to_hex())
        .replace("{mem_address}", &mem_addr.to_string())
        .replace("{mem_address_2}", &(mem_addr + 4).to_string());
    let note_script = client
        .code_builder()
        .compile_note_script(&note_script)
        .context("failed to compile custom note script")?;

    let inputs =
        NoteStorage::new(vec![target_account_id.suffix(), target_account_id.prefix().as_felt()])
            .context("failed to create note inputs")?;
    let serial_num = rng.draw_word();
    let note_metadata = PartialNoteMetadata::new(faucet_account_id, NoteType::Private)
        .with_tag(NoteTag::with_account_target(target_account_id));
    let note_assets = NoteAssets::new(vec![
        FungibleAsset::new(faucet_account_id, 10)
            .context("failed to create fungible asset")?
            .into(),
    ])
    .context("failed to create note assets")?;
    let note_recipient = NoteRecipient::new(serial_num, note_script, inputs);
    Ok(Note::new(note_assets, note_metadata, note_recipient))
}
