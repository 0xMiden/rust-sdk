use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{
    NoteAcquisition,
    NoteAcquisitionError,
    NoteAcquisitionGate,
    NoteAcquisitionLimits,
    NoteAcquisitionSource,
    NoteAcquisitionVariant,
    NoteFile,
    NoteId,
    NoteSyncHint,
    NoteTag,
    NoteType,
};
use miden_client::note_transport::NOTE_TRANSPORT_CURSORS_KEY;
use miden_client::store::{NoteFilter, SettingScope, Store};
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::MockRpcApi;
use miden_client::testing::note_transport::{MockNoteTransportApi, MockNoteTransportNode};
use miden_client::utils::RwLock;
use miden_client::{ClientError, async_trait};
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::EMPTY_WORD;
use miden_protocol::block::BlockNumber;
use miden_protocol::utils::serde::Serializable;
use miden_standards::testing::note::NoteBuilder;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use tokio::sync::Notify;

struct RejectGate {
    calls: AtomicUsize,
}

#[async_trait]
impl NoteAcquisitionGate for RejectGate {
    async fn before_import(&self, _item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(NoteAcquisitionError::RetentionFailed)
    }
}

fn limits() -> NoteAcquisitionLimits {
    NoteAcquisitionLimits {
        max_notes: 2,
        max_note_file_bytes: 4096,
        max_total_note_file_bytes: 8192,
    }
}

#[tokio::test]
async fn acquisition_rejection_precedes_genesis_initialization() {
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(RejectGate { calls: AtomicUsize::new(0) });
    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(Arc::new(MockRpcApi::default()))
        .store(store.clone())
        .note_acquisition_gate(gate.clone(), limits())
        .build()
        .await
        .unwrap();
    let files = [NoteFile::NoteId(NoteId::from_raw(EMPTY_WORD))];

    let result = client.import_notes(&files).await;

    assert!(matches!(
        result,
        Err(ClientError::NoteAcquisition(NoteAcquisitionError::RetentionFailed))
    ));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
}

struct PendingGate {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl NoteAcquisitionGate for PendingGate {
    async fn before_import(&self, _item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(NoteAcquisitionError::Rejected)
    }
}

#[tokio::test]
async fn acquisition_acknowledgement_is_awaited_before_genesis_initialization() {
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(PendingGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(Arc::new(MockRpcApi::default()))
        .store(store.clone())
        .note_acquisition_gate(gate.clone(), limits())
        .build()
        .await
        .unwrap();
    let files = [NoteFile::NoteId(NoteId::from_raw(EMPTY_WORD))];
    let mut import = Box::pin(client.import_notes(&files));

    tokio::select! {
        result = &mut import => panic!("import returned before retention acknowledgement: {result:?}"),
        () = gate.entered.notified() => {},
    }
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
    gate.release.notify_one();
    assert!(matches!(
        import.await,
        Err(ClientError::NoteAcquisition(NoteAcquisitionError::Rejected))
    ));
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
}

struct RejectSecondGate {
    calls: AtomicUsize,
}

#[async_trait]
impl NoteAcquisitionGate for RejectSecondGate {
    async fn before_import(&self, _item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(NoteAcquisitionError::RetentionFailed)
        }
    }
}

#[tokio::test]
async fn acquisition_rejection_of_second_item_precedes_all_import_writes() {
    let rpc = Arc::new(MockRpcApi::new(Box::pin(super::create_prebuilt_mock_chain()).await));
    let files: Vec<_> = rpc
        .get_public_available_notes()
        .iter()
        .take(2)
        .map(|record| NoteFile::NoteId(record.note().unwrap().id()))
        .collect();
    assert_eq!(files.len(), 2);
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(RejectSecondGate { calls: AtomicUsize::new(0) });
    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(rpc)
        .store(store.clone())
        .note_acquisition_gate(gate.clone(), limits())
        .build()
        .await
        .unwrap();

    let result = client.import_notes(&files).await;

    assert!(matches!(
        result,
        Err(ClientError::NoteAcquisition(NoteAcquisitionError::RetentionFailed))
    ));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
    assert!(store.get_input_notes(NoteFilter::All).await.unwrap().is_empty());
    assert!(store.get_note_tags().await.unwrap().is_empty());
    assert!(
        store
            .get_setting(SettingScope::Client, NOTE_TRANSPORT_CURSORS_KEY.to_string())
            .await
            .unwrap()
            .is_none()
    );
}

#[derive(Default)]
struct CaptureGate {
    files: Mutex<Vec<NoteFile>>,
}

#[async_trait]
impl NoteAcquisitionGate for CaptureGate {
    async fn before_import(&self, item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        assert_eq!(item.source(), NoteAcquisitionSource::Import);
        assert_eq!(item.batch_count(), 3);
        let decoded = NoteFile::try_from_bytes(item.note_file_bytes()).unwrap();
        match &decoded {
            NoteFile::NoteId(id) => {
                assert_eq!(item.variant(), NoteAcquisitionVariant::NoteId);
                assert_eq!(item.note_id(), Some(*id));
                assert!(item.details_commitment().is_none());
                assert!(item.nullifier().is_none());
                assert!(item.metadata().is_none());
                assert!(item.tag().is_none());
                assert!(item.after_block_num().is_none());
            },
            NoteFile::ExpectedNote { details, sync_hint } => {
                assert_eq!(item.variant(), NoteAcquisitionVariant::ExpectedNote);
                assert_eq!(item.details_commitment(), Some(details.commitment()));
                assert_eq!(item.tag(), Some(sync_hint.tag()));
                assert_eq!(item.after_block_num(), Some(sync_hint.after_block_num()));
                assert!(item.note_id().is_none());
                assert!(item.nullifier().is_none());
                assert!(item.metadata().is_none());
            },
            NoteFile::Committed { note, .. } => {
                assert_eq!(item.variant(), NoteAcquisitionVariant::Committed);
                assert_eq!(item.details_commitment(), Some(note.details_commitment()));
                assert_eq!(item.note_id(), Some(note.id()));
                assert_eq!(item.nullifier(), Some(note.nullifier()));
                assert_eq!(item.metadata(), Some(*note.metadata()));
                assert_eq!(item.tag(), Some(note.metadata().tag()));
                assert!(item.after_block_num().is_none());
            },
        }
        let mut files = self.files.lock().unwrap();
        assert_eq!(item.batch_index(), files.len());
        files.push(decoded);
        let count = files.len();
        drop(files);
        if count == 3 {
            Err(NoteAcquisitionError::Rejected)
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn acquisition_preserves_candidates_and_does_not_invent_expected_note_metadata() {
    let rpc = Arc::new(MockRpcApi::new(Box::pin(super::create_prebuilt_mock_chain()).await));
    let available = rpc.get_public_available_notes();
    let note = available[0].note().unwrap().clone();
    let files = vec![
        NoteFile::NoteId(note.id()),
        NoteFile::ExpectedNote {
            details: note.details().clone(),
            sync_hint: NoteSyncHint::new(37.into(), NoteTag::new(41)),
        },
        NoteFile::Committed {
            note,
            proof: available[0].inclusion_proof().clone(),
        },
    ];
    let lengths: Vec<_> = files.iter().map(|file| file.to_bytes().len()).collect();
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(CaptureGate::default());
    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(rpc)
        .store(store.clone())
        .note_acquisition_gate(
            gate.clone(),
            NoteAcquisitionLimits {
                max_notes: files.len(),
                max_note_file_bytes: *lengths.iter().max().unwrap(),
                max_total_note_file_bytes: lengths.iter().sum(),
            },
        )
        .build()
        .await
        .unwrap();

    let result = client.import_notes(&files).await;

    assert!(matches!(
        result,
        Err(ClientError::NoteAcquisition(NoteAcquisitionError::Rejected))
    ));
    assert_eq!(*gate.files.lock().unwrap(), files);
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
}

#[tokio::test]
async fn acquisition_count_and_item_limits_refuse_before_callback() {
    let files = [NoteFile::NoteId(NoteId::from_raw(EMPTY_WORD))];
    let encoded_len = files[0].to_bytes().len();
    assert!(encoded_len > 0);
    for configured in [
        NoteAcquisitionLimits { max_notes: 0, ..limits() },
        NoteAcquisitionLimits {
            max_note_file_bytes: encoded_len - 1,
            ..limits()
        },
        NoteAcquisitionLimits { max_total_note_file_bytes: 0, ..limits() },
    ] {
        let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
        let gate = Arc::new(RejectGate { calls: AtomicUsize::new(0) });
        let mut client = ClientBuilder::<FilesystemKeyStore>::new()
            .rpc(Arc::new(MockRpcApi::default()))
            .store(store.clone())
            .note_acquisition_gate(gate.clone(), configured)
            .build()
            .await
            .unwrap();

        let result = client.import_notes(&files).await;

        assert!(matches!(
            result,
            Err(ClientError::NoteAcquisition(NoteAcquisitionError::LimitExceeded))
        ));
        assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
        assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
    }
}

#[derive(Default)]
struct AcceptGate {
    calls: AtomicUsize,
}

#[async_trait]
impl NoteAcquisitionGate for AcceptGate {
    async fn before_import(&self, _item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn acquisition_aggregate_limit_preserves_import_state_after_an_earlier_acknowledgement() {
    let files = [
        NoteFile::NoteId(NoteId::from_raw(EMPTY_WORD)),
        NoteFile::NoteId(
            NoteId::try_from_hex(
                "0x0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap(),
        ),
    ];
    let lengths: Vec<_> = files.iter().map(|file| file.to_bytes().len()).collect();
    let configured = NoteAcquisitionLimits {
        max_notes: files.len(),
        max_note_file_bytes: *lengths.iter().max().unwrap(),
        max_total_note_file_bytes: lengths[0],
    };
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(AcceptGate::default());
    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(Arc::new(MockRpcApi::default()))
        .store(store.clone())
        .note_acquisition_gate(gate.clone(), configured)
        .build()
        .await
        .unwrap();

    let result = client.import_notes(&files).await;

    assert!(matches!(
        result,
        Err(ClientError::NoteAcquisition(NoteAcquisitionError::LimitExceeded))
    ));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
    assert!(store.get_input_notes(NoteFilter::All).await.unwrap().is_empty());
    assert!(store.get_note_tags().await.unwrap().is_empty());
}

#[tokio::test]
async fn acquisition_client_opt_in_imports_after_successful_acknowledgement() {
    let rpc = Arc::new(MockRpcApi::new(Box::pin(super::create_prebuilt_mock_chain()).await));
    let available = rpc.get_public_available_notes();
    let files = [NoteFile::Committed {
        note: available[0].note().unwrap().clone(),
        proof: available[0].inclusion_proof().clone(),
    }];
    let encoded_len = files[0].to_bytes().len();
    let configured = NoteAcquisitionLimits {
        max_notes: 1,
        max_note_file_bytes: encoded_len,
        max_total_note_file_bytes: encoded_len,
    };
    let store = Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap());
    let gate = Arc::new(AcceptGate::default());
    let client = ClientBuilder::<FilesystemKeyStore>::new()
        .rpc(rpc)
        .store(store.clone())
        .build()
        .await
        .unwrap();
    let mut client = client.with_note_acquisition_gate(gate.clone(), configured);

    let imported = client.import_notes(&files).await.unwrap();

    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(imported.len(), 1);
    assert_eq!(store.get_input_notes(NoteFilter::All).await.unwrap().len(), 1);
}

struct ToggleGate {
    allow: AtomicBool,
    sources: Mutex<Vec<NoteAcquisitionSource>>,
}

#[async_trait]
impl NoteAcquisitionGate for ToggleGate {
    async fn before_import(&self, item: &NoteAcquisition) -> Result<(), NoteAcquisitionError> {
        self.sources.lock().unwrap().push(item.source());
        if self.allow.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(NoteAcquisitionError::RetentionFailed)
        }
    }
}

#[tokio::test]
async fn acquisition_transport_failure_preserves_cursors_in_direct_and_combined_sync() {
    for combined_sync in [false, true] {
        let (builder, rpc_api) = Box::pin(super::create_test_client_builder()).await;
        let rpc = Arc::new(rpc_api);
        let sender = rpc.get_public_available_notes()[0].note().unwrap().metadata().sender();
        let tag = NoteTag::new(1948);
        let note = NoteBuilder::new(sender, ChaCha20Rng::seed_from_u64(1948))
            .note_type(NoteType::Private)
            .tag(tag.into())
            .build()
            .unwrap();
        let transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
        transport_node.write().add_note(*note.header(), note.details().to_bytes());
        let gate = Arc::new(ToggleGate {
            allow: AtomicBool::new(false),
            sources: Mutex::new(Vec::new()),
        });
        let mut client = builder
            .rpc(rpc)
            .note_transport(Arc::new(MockNoteTransportApi::new(transport_node)))
            .note_acquisition_gate(
                gate.clone(),
                NoteAcquisitionLimits {
                    max_notes: 4,
                    max_note_file_bytes: 1024 * 1024,
                    max_total_note_file_bytes: 2 * 1024 * 1024,
                },
            )
            .build()
            .await
            .unwrap();
        client.add_note_tag(tag).await.unwrap();
        let store = client.test_store().clone();
        let cursors_before = store
            .get_setting(SettingScope::Client, NOTE_TRANSPORT_CURSORS_KEY.to_string())
            .await
            .unwrap();
        let notes_before = store.get_input_notes(NoteFilter::All).await.unwrap();

        let result = if combined_sync {
            client.sync_state().await.map(|_| ())
        } else {
            client.fetch_private_notes().await.map(|_| ())
        };

        assert!(matches!(
            result,
            Err(ClientError::NoteAcquisition(NoteAcquisitionError::RetentionFailed))
        ));
        assert_eq!(
            store
                .get_setting(SettingScope::Client, NOTE_TRANSPORT_CURSORS_KEY.to_string())
                .await
                .unwrap(),
            cursors_before
        );
        assert_eq!(store.get_input_notes(NoteFilter::All).await.unwrap(), notes_before);
        assert_eq!(*gate.sources.lock().unwrap(), [NoteAcquisitionSource::NoteTransport]);

        gate.allow.store(true, Ordering::SeqCst);
        if combined_sync {
            client.sync_state().await.unwrap();
        } else {
            client.fetch_private_notes().await.unwrap();
        }
        assert_ne!(
            store
                .get_setting(SettingScope::Client, NOTE_TRANSPORT_CURSORS_KEY.to_string())
                .await
                .unwrap(),
            cursors_before
        );
        assert_eq!(
            store
                .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            gate.sources
                .lock()
                .unwrap()
                .iter()
                .all(|source| *source == NoteAcquisitionSource::NoteTransport)
        );
    }
}
