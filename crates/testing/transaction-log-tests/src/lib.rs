#![cfg(test)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::domain::AccountLogQuery;
use miden_client::rpc::encryption::seal_transaction_inputs;
use miden_client::rpc::{Endpoint, GrpcClient as SdkRpc, NodeRpcClient};
use miden_client::store::{Store, TransactionFilter};
use miden_client::transaction::{ForeignAccount, TransactionRequestBuilder};
use miden_client_sqlite_store::SqliteStore;
use miden_node_block_producer::{BlockProducerApi, Sequencer, SequencerHandle};
use miden_node_proto::clients::{Builder, ValidatorClient};
use miden_node_proto::generated as proto;
use miden_node_rpc::{AccountAdmission, Rpc, RpcMode, ValidatorClients};
use miden_node_store::DataDirectory;
use miden_node_store::allowlist::AccountAllowlist;
use miden_node_store::state::{State, WriterTask};
use miden_node_utils::clap::{GrpcOptions, StorageOptions};
use miden_node_utils::genesis::GenesisBlock;
use miden_node_utils::shutdown::CancellationToken;
use miden_protocol::Word;
use miden_protocol::account::component::AccountComponentMetadata;
use miden_protocol::account::{Account, AccountBuilder, AccountComponent, AccountType};
use miden_protocol::block::{BlockSignatures, SignedBlock};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SigningKey;
use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
use miden_protocol::transaction::{ProvenTransaction, TransactionLogData, TransactionVerifier};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_standards::account::wallets::BasicWallet;
use miden_standards::code_builder::CodeBuilder;
use miden_testing::{Auth, MockChainBuilder};
use miden_validator::{
    EncodedGoldenOperatorKey,
    LocalX25519TransactionInputDecrypter,
    PrivateRecordSealer,
    StorageKeyEpoch,
    ValidatorServer,
    ValidatorSigner,
};
use tokio::net::TcpListener;
use url::Url;

struct LogNode {
    state: Arc<State>,
    sequencer: SequencerHandle,
    writer_task: WriterTask,
    producer: BlockProducerApi,
    address: std::net::SocketAddr,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl LogNode {
    async fn start(path: &std::path::Path, validator: ValidatorClient, validator_url: Url) -> Self {
        let shutdown = CancellationToken::new();
        let (state, writer, proof_writer, writer_task) =
            State::load(path, StorageOptions::default())
                .await
                .unwrap()
                .start(shutdown.clone());
        let sequencer = Sequencer {
            state: Arc::clone(&state),
            block_writer: writer,
            proof_writer,
            validator_urls: vec![validator_url],
            validator_timeout: Duration::from_secs(30),
            batch_prover_url: None,
            block_prover_url: None,
            batch_interval: Duration::from_millis(100),
            block_interval: Duration::from_secs(5),
            max_txs_per_batch: NonZeroUsize::new(8).unwrap(),
            max_batches_per_block: NonZeroUsize::new(8).unwrap(),
            max_concurrent_proofs: NonZeroUsize::new(1).unwrap(),
            mempool_tx_capacity: NonZeroUsize::new(32).unwrap(),
            batch_workers: NonZeroUsize::new(1).unwrap(),
        }
        .spawn(shutdown.clone())
        .unwrap();
        let producer = sequencer.api();
        let allowlist_path =
            DataDirectory::load(path.to_path_buf()).unwrap().allowlist_database_path();
        let allowlist = Arc::new(AccountAllowlist::load(allowlist_path).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let rpc = Rpc {
            listener,
            state: Arc::clone(&state),
            mode: RpcMode::Sequencer {
                block_producer: Box::new(producer.clone()),
                validators: ValidatorClients::new(vec![validator]).unwrap(),
                account_admission: AccountAdmission::disabled(allowlist),
            },
            ntx_builder: None,
            grpc_options: GrpcOptions {
                request_timeout: Duration::from_secs(180),
            },
            network_tx_auth: None,
        };
        let server = tokio::spawn(rpc.serve(shutdown.clone()));
        Self {
            state,
            sequencer,
            writer_task,
            producer,
            address,
            shutdown,
            server,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.server.await.unwrap().unwrap();
        drop(self.producer);
        self.sequencer.wait().await.unwrap();
        self.writer_task.await.unwrap();
        drop(self.state);
    }
}

struct LogValidator {
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    reader: miden_validator::db::ValidatorDbReader,
}

impl LogValidator {
    async fn stop(self) {
        self.shutdown.cancel();
        self.server.await.unwrap().unwrap();
    }
}

async fn log_validator(
    chain: &miden_testing::MockChain,
    path: &std::path::Path,
) -> (ValidatorClient, Url, LogValidator) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = Url::parse(&format!("http://{address}")).unwrap();
    let data_directory = miden_validator::DataDirectory::load(path.to_path_buf()).unwrap();
    let block = chain.latest_block();
    let genesis = GenesisBlock::new(
        SignedBlock::new(
            block.header().clone(),
            block.body().clone(),
            BlockSignatures::new(vec![]).unwrap(),
        )
        .unwrap(),
        chain.protocol_config().clone(),
    )
    .unwrap();
    miden_node_store::BlockStore::bootstrap(data_directory.block_store_dir(), &genesis).unwrap();
    miden_validator::db::bootstrap(
        data_directory.database_path(),
        NonZeroUsize::new(2).unwrap(),
        genesis.inner().header().clone(),
        chain.protocol_config().clone(),
    )
    .await
    .unwrap();
    let storage_key = EncodedGoldenOperatorKey::new(
        StorageKeyEpoch::new([9; 32]),
        include_bytes!("../fixtures/setup-context.wire").to_vec(),
        include_bytes!("../fixtures/public-key-set.wire").to_vec(),
        include_bytes!("../fixtures/secret-share.wire").to_vec(),
    )
    .decode()
    .unwrap();
    let db = miden_validator::db::load(data_directory.database_path()).await.unwrap();
    let reader = db.reader();
    let server = ValidatorServer {
        address,
        grpc_options: GrpcOptions::default(),
        signer: ValidatorSigner::new_local(SigningKey::read_from_bytes(&[7; 32]).unwrap()),
        decrypter: Arc::new(LocalX25519TransactionInputDecrypter::new(
            KeyExchangeKey::read_from_bytes(&[3; 32]).unwrap(),
        )),
        private_record_sealer: PrivateRecordSealer::from_operator_key(&storage_key),
        db,
        data_directory,
    };
    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let server = tokio::spawn(server.serve_on(listener, token));
    let client = Builder::new(url.clone())
        .without_tls()
        .without_timeout()
        .without_metadata_version()
        .without_metadata_genesis()
        .without_otel_context_injection()
        .connect_lazy::<ValidatorClient>();
    (client, url, LogValidator { shutdown, server, reader })
}

fn logging_account(account_type: AccountType, seed: u8, commitment: Word) -> Account {
    let source = format!(
        r"
        use miden::protocol::tx
        use miden::protocol::native_account

        @auth_script
        pub proc auth
            repeat.2
                push.{commitment} push.29.17 exec.tx::add_log
            end
            padw push.31.17 exec.tx::add_log
            exec.native_account::incr_nonce drop
        end
        "
    );
    let component = AccountComponent::new(
        CodeBuilder::default()
            .compile_component_code("test::native_logs", source)
            .unwrap(),
        vec![],
        AccountComponentMetadata::mock("test::native_logs"),
    )
    .unwrap();
    AccountBuilder::new([seed; 32])
        .account_type(account_type)
        .with_component(component)
        .with_component(BasicWallet)
        .build_existing()
        .unwrap()
}

#[expect(
    clippy::too_many_lines,
    reason = "covers one transaction lifecycle across node and SDK restarts"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transaction_logs_sdk_node_e2e() {
    let payload = vec![Word::from([123u32, 456, 789, 1234])];
    let commitment = miden_protocol::Hasher::hash_elements(Word::words_as_elements(&payload));
    let public = logging_account(AccountType::Public, 61, commitment);
    let private = logging_account(AccountType::Private, 62, commitment);
    let foreign_component = AccountComponent::new(
        CodeBuilder::default()
            .compile_component_code(
                "test::foreign_logs",
                r"
            use miden::protocol::tx

            @account_procedure
            pub proc emit_log
                padw push.29.17 exec.tx::add_log
            end
            ",
            )
            .unwrap(),
        vec![],
        AccountComponentMetadata::mock("test::foreign_logs"),
    )
    .unwrap();
    let foreign_root = foreign_component
        .get_procedure_root_by_path("test::foreign_logs::emit_log")
        .unwrap();
    let foreign = AccountBuilder::new([63; 32])
        .account_type(AccountType::Public)
        .with_components(Auth::IncrNonce)
        .with_component(foreign_component)
        .build_existing()
        .unwrap();
    let chain = MockChainBuilder::with_accounts([public.clone(), private.clone(), foreign.clone()])
        .unwrap()
        .verification_base_fee(0)
        .validator_signing_keys(vec![SigningKey::read_from_bytes(&[7; 32]).unwrap()])
        .build()
        .unwrap();
    let node_dir = tempfile::tempdir().unwrap();
    let node_path = node_dir.path().to_path_buf();
    let block = chain.latest_block();
    let genesis = GenesisBlock::new(
        SignedBlock::new(
            block.header().clone(),
            block.body().clone(),
            BlockSignatures::new(vec![]).unwrap(),
        )
        .unwrap(),
        chain.protocol_config().clone(),
    )
    .unwrap();
    State::bootstrap(genesis, &node_path).unwrap();
    AccountAllowlist::bootstrap(
        DataDirectory::load(node_path.clone()).unwrap().allowlist_database_path(),
    )
    .unwrap();
    let validator_dir = tempfile::tempdir().unwrap();
    let (validator, validator_url, validator_server) =
        log_validator(&chain, validator_dir.path()).await;
    let mut node = LogNode::start(&node_path, validator.clone(), validator_url.clone()).await;

    let client_dir = tempfile::tempdir().unwrap();
    let client_path = client_dir.path().join("client.sqlite");
    let store = Arc::new(SqliteStore::new(client_path.clone()).await.unwrap());
    let rpc = Arc::new(SdkRpc::new(
        &Endpoint::try_from(format!("http://{}", node.address).as_str()).unwrap(),
        180_000,
    ));
    let mut client = ClientBuilder::new()
        .protocol_config(chain.protocol_config().clone())
        .rpc(rpc.clone())
        .store(store.clone())
        .authenticator(Arc::new(FilesystemKeyStore::new(client_dir.path().join("keys")).unwrap()))
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    client.add_account(&public, false).await.unwrap();
    client.add_account(&private, false).await.unwrap();

    let fpi_script = miden_client::transaction::build_fpi_script(
        CodeBuilder::default(),
        foreign.id(),
        foreign_root.into(),
        &[],
    )
    .unwrap();
    let request = TransactionRequestBuilder::new()
        .custom_script(fpi_script)
        .foreign_accounts([ForeignAccount::public(
            foreign.id(),
            miden_client::rpc::domain::account::AccountStorageRequirements::default(),
        )
        .unwrap()])
        .log_payload(&payload)
        .unwrap()
        .log_payload([])
        .unwrap()
        .build()
        .unwrap();
    let key = rpc
        .get_transaction_encryption_key()
        .await
        .unwrap()
        .verify(
            chain.genesis_block_header().commitment(),
            chain.genesis_block_header().validator_config(),
        )
        .unwrap();
    let mut proven = Vec::new();
    let mut private_result = None;
    for account in [&public, &private] {
        let result = client.execute_transaction(account.id(), request.clone()).await.unwrap();
        assert_eq!(result.logs().num_logs(), 4);
        assert_eq!(result.logs().iter().next().unwrap().emitter(), foreign.id());
        if account.id().is_private() {
            let repeated = client.execute_transaction(account.id(), request.clone()).await.unwrap();
            assert_eq!(repeated.logs(), result.logs());
            assert_ne!(
                repeated.id(),
                result.id(),
                "fresh secret openings must hide repeated private logs"
            );
            private_result = Some(result.clone());
        }
        let transaction = client.prove_transaction(&result).await.unwrap();
        assert!(
            TransactionVerifier::new(miden_protocol::MIN_PROOF_SECURITY_LEVEL)
                .verify(&transaction)
                .unwrap()
                .is_complete()
        );
        assert_eq!(
            matches!(transaction.log_data(), TransactionLogData::Public(_)),
            account.id().is_public()
        );
        if account.id().is_private() {
            for salt in [Word::empty(), Word::from([99u32; 4])] {
                let inputs = result
                    .tx_inputs()
                    .clone()
                    .with_tx_args(result.tx_inputs().tx_args().clone().with_log_salt(salt));
                let sealed =
                    seal_transaction_inputs(client.rng(), &key, transaction.id(), &inputs).unwrap();
                let status = validator
                    .clone()
                    .submit_proven_transaction(proto::submission::ProvenTransactionSubmission {
                        transaction: Some((&transaction).into()),
                        sealed_transaction_inputs: Some(
                            proto::submission::SealedTransactionInputs {
                                key_id: sealed.key_id().to_vec(),
                                ciphertext: sealed.ciphertext().to_vec(),
                            },
                        ),
                    })
                    .await
                    .unwrap_err();
                assert_eq!(status.code(), tonic::Code::InvalidArgument);
                assert!(
                    validator_server
                        .reader
                        .load_private_record(transaction.id())
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        }
        let sealed =
            seal_transaction_inputs(client.rng(), &key, transaction.id(), result.tx_inputs())
                .unwrap();
        rpc.submit_proven_transaction(transaction.clone(), sealed).await.unwrap();
        assert!(
            validator_server
                .reader
                .load_private_record(transaction.id())
                .await
                .unwrap()
                .is_some()
        );
        client.apply_transaction(&result, 0.into()).await.unwrap();
        proven.push(transaction);
    }
    let expected_ids: std::collections::BTreeSet<_> =
        proven.iter().map(ProvenTransaction::id).collect();
    let end = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let tip = node.state.committed_tip();
            let records = rpc
                .sync_transactions(0.into(), tip, vec![public.id(), private.id()])
                .await
                .unwrap();
            let ids: std::collections::BTreeSet<_> =
                records.iter().map(|record| record.transaction_header.id()).collect();
            if ids == expected_ids {
                break tip;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the sequencer must commit both transactions");

    let mut query = AccountLogQuery::new(public.id(), 0.into(), end);
    query.page_size = 1;
    assert_eq!(client.sync_account_logs(query.clone()).await.unwrap(), 3);
    assert_eq!(client.sync_account_logs(query.clone()).await.unwrap(), 3);
    let mut cached = query.clone();
    cached.page_size = 128;
    let records = client.get_cached_account_logs(cached.clone()).await.unwrap().records;
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].log, records[1].log, "duplicate occurrences must be preserved");
    assert!(records[2].log.payload().is_empty());
    assert!(
        client
            .get_account_logs(AccountLogQuery::new(private.id(), 0.into(), end))
            .await
            .unwrap()
            .records
            .is_empty()
    );
    let foreign_records = client
        .get_account_logs(AccountLogQuery::new(foreign.id(), 0.into(), end))
        .await
        .unwrap()
        .records;
    assert_eq!(
        foreign_records.len(),
        1,
        "private FPI must not publish records for a public emitter"
    );
    assert_eq!(foreign_records[0].native_account_id, public.id());
    let mut by_topic = cached.clone();
    by_topic.topic = Some(records[2].log.topic());
    assert_eq!(client.get_account_logs(by_topic).await.unwrap().records.len(), 1);
    client.sync_state().await.unwrap();

    drop(client);
    drop(store);
    node.stop().await;
    node = LogNode::start(&node_path, validator, validator_url).await;
    let reopened = Arc::new(SqliteStore::new(client_path).await.unwrap());
    let rpc = Arc::new(SdkRpc::new(
        &Endpoint::try_from(format!("http://{}", node.address).as_str()).unwrap(),
        180_000,
    ));
    let client = ClientBuilder::new()
        .rpc(rpc)
        .store(reopened.clone())
        .authenticator(Arc::new(FilesystemKeyStore::new(client_dir.path().join("keys")).unwrap()))
        .build()
        .await
        .unwrap();
    assert_eq!(client.get_cached_account_logs(cached.clone()).await.unwrap().records, records);
    assert_eq!(client.get_account_logs(cached).await.unwrap().records, records);
    let private_result = private_result.unwrap();
    let transactions = reopened.get_transactions(TransactionFilter::All).await.unwrap();
    let private_record =
        transactions.iter().find(|record| record.id == private_result.id()).unwrap();
    assert_eq!(&private_record.details.logs, private_result.logs());
    assert_eq!(private_record.details.log_salt, private_result.tx_inputs().tx_args().log_salt());
    drop(client);
    drop(reopened);
    node.stop().await;
    validator_server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_log_blocks_exceeding_default_grpc_limit_reach_validation() {
    let chain = MockChainBuilder::new()
        .validator_signing_keys(vec![SigningKey::read_from_bytes(&[7; 32]).unwrap()])
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _, validator) = log_validator(&chain, dir.path()).await;
    let log = miden_protocol::transaction::TransactionLog::new(
        ACCOUNT_ID_SENDER.try_into().unwrap(),
        miden_protocol::transaction::LogTopic::from_name("test::large_block"),
        vec![Word::from([17u32; 4]); miden_protocol::MAX_LOG_PAYLOAD_WORDS],
    )
    .unwrap();
    let data = TransactionLogData::Public(
        miden_protocol::transaction::TransactionLogs::new(vec![log; 2]).unwrap(),
    );
    let batch = miden_protocol::transaction::TransactionLogDataCollection::new(vec![
            data;
            miden_protocol::MAX_PUBLIC_LOG_PAYLOAD_WORDS_PER_BATCH
                / miden_protocol::MAX_LOG_PAYLOAD_WORDS_PER_TX
        ])
    .unwrap();
    let batches = miden_protocol::MAX_PUBLIC_LOG_PAYLOAD_WORDS_PER_BLOCK
        / miden_protocol::MAX_PUBLIC_LOG_PAYLOAD_WORDS_PER_BATCH;
    let request = proto::validator::SignBlockRequest {
        batches: vec![
            miden_objects::proto::transaction::ProvenBatch {
                log_data: batch.to_bytes(),
                ..Default::default()
            };
            batches
        ],
        ..Default::default()
    };
    let bytes = miden_node_proto::prost::Message::encoded_len(&request);
    assert!(bytes > 4 * 1024 * 1024);
    assert!(bytes < miden_node_proto::MAX_BLOCK_MESSAGE_SIZE);
    let status = client.sign_block(request).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let oversized = proto::validator::SignBlockRequest {
        batches: vec![miden_objects::proto::transaction::ProvenBatch {
            log_data: vec![0; miden_node_proto::MAX_BLOCK_MESSAGE_SIZE],
            ..Default::default()
        }],
        ..Default::default()
    };
    let status = client.sign_block(oversized).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::OutOfRange);
    validator.stop().await;
}
