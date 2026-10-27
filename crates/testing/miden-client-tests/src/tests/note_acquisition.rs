use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use miden_client::keystore::FilesystemKeyStore;
use miden_client::builder::ClientBuilder;
use miden_client::note::{
    NoteAcquisition, NoteAcquisitionError, NoteAcquisitionGate, NoteAcquisitionLimits, NoteFile,
    NoteId,
};
use miden_client::store::{NoteFilter, Store};
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::MockRpcApi;
use miden_client::{ClientError, async_trait};
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::EMPTY_WORD;
use miden_protocol::block::BlockNumber;
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

    assert!(matches!(result, Err(ClientError::NoteAcquisition(NoteAcquisitionError::RetentionFailed))));
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
    let gate = Arc::new(PendingGate { entered: Notify::new(), release: Notify::new() });
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
    assert!(matches!(import.await, Err(ClientError::NoteAcquisition(NoteAcquisitionError::Rejected))));
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
    let rpc = Arc::new(super::prebuilt_rpc_api().await);
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

    assert!(matches!(result, Err(ClientError::NoteAcquisition(NoteAcquisitionError::RetentionFailed))));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    assert!(store.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().is_none());
    assert!(store.get_input_notes(NoteFilter::All).await.unwrap().is_empty());
    assert!(store.get_note_tags().await.unwrap().is_empty());
    assert!(store.get_note_transport_cursors().await.unwrap().is_empty());
}
