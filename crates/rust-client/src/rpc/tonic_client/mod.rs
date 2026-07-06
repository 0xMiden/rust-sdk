use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error;
use core::pin::Pin;

use miden_protocol::vm::FutureMaybeSend;

type RpcFuture<T> = Pin<Box<dyn FutureMaybeSend<T>>>;

use miden_protocol::account::{AccountCode, AccountId};
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::crypto::merkle::mmr::{Forest, MmrPath, MmrProof};
use miden_protocol::note::{NoteId, NoteScript, NoteTag};
use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};
use miden_protocol::utils::serde::Deserializable;
use miden_protocol::{EMPTY_WORD, Word};
use miden_tx::utils::serde::Serializable;
use miden_tx::utils::sync::RwLock;
use tonic::Status;
use tracing::info;

use super::domain::account::{
    AccountProof,
    AccountStorageRequirements,
    GetAccountRequest,
    StorageMapFetch,
};
use super::domain::note::{CommittedNote, FetchedNote, NoteSyncBlock};
use super::domain::nullifier::NullifierUpdate;
use super::generated::rpc::AccountRequest;
use super::generated::rpc::account_request::AccountDetailRequest;
use super::{Endpoint, NodeRpcClient, RpcEndpoint, RpcError, RpcStatusInfo};
use crate::rpc::domain::account_vault::{AccountVaultInfo, AccountVaultUpdate};
use crate::rpc::domain::limits::RpcLimits;
use crate::rpc::domain::status::NetworkNoteStatusInfo;
use crate::rpc::domain::storage_map::{StorageMapInfo, StorageMapUpdate};
use crate::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use crate::rpc::domain::transaction::TransactionRecord;
use crate::rpc::errors::node::parse_node_error;
use crate::rpc::errors::{AcceptHeaderContext, AcceptHeaderError, GrpcError, RpcConversionError};
use crate::rpc::generated::rpc::BlockRange;
use crate::rpc::{AccountStateAt, generated as proto};

mod api_client;
mod retry;

use api_client::api_client_wrapper::ApiClient;

/// Tracks the pagination state for block-driven endpoints.
struct BlockPagination {
    current_block_from: BlockNumber,
    block_to: BlockNumber,
    iterations: u32,
}

enum PaginationResult {
    Continue,
    Done {
        chain_tip: BlockNumber,
        block_num: BlockNumber,
    },
}

impl BlockPagination {
    /// Maximum number of pagination iterations for a single request.
    ///
    /// Protects against nodes returning inconsistent pagination data that could otherwise
    /// trigger an infinite loop.
    const MAX_ITERATIONS: u32 = 1000;

    fn new(block_from: BlockNumber, block_to: BlockNumber) -> Self {
        Self {
            current_block_from: block_from,
            block_to,
            iterations: 0,
        }
    }

    fn current_block_from(&self) -> BlockNumber {
        self.current_block_from
    }

    fn block_to(&self) -> BlockNumber {
        self.block_to
    }

    fn advance(
        &mut self,
        block_num: BlockNumber,
        chain_tip: BlockNumber,
    ) -> Result<PaginationResult, RpcError> {
        if self.iterations >= Self::MAX_ITERATIONS {
            return Err(RpcError::PaginationError(
                "too many pagination iterations, possible infinite loop".to_owned(),
            ));
        }
        self.iterations += 1;

        if block_num < self.current_block_from {
            return Err(RpcError::PaginationError(
                "invalid pagination: block_num went backwards".to_owned(),
            ));
        }

        let target_block = self.block_to.min(chain_tip);

        if block_num >= target_block {
            return Ok(PaginationResult::Done { chain_tip, block_num });
        }

        self.current_block_from = BlockNumber::from(block_num.as_u32().saturating_add(1));

        Ok(PaginationResult::Continue)
    }
}

/// Returns [`RpcError::InvalidResponse`] if any update in the `sync_nullifiers` batch carries a
/// nullifier whose prefix was not requested.
fn ensure_requested_nullifiers(
    requested_prefixes: &BTreeSet<u16>,
    batch: &[NullifierUpdate],
) -> Result<(), RpcError> {
    for update in batch {
        let prefix = update.nullifier.prefix();
        if !requested_prefixes.contains(&prefix) {
            let requested = requested_prefixes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned nullifier with prefix {prefix} but [{requested}] were requested"
            )));
        }
    }
    Ok(())
}

/// Returns an error if any note in a `sync_notes` response carries a tag that was not requested.
fn ensure_requested_tags(
    requested: &BTreeSet<NoteTag>,
    returned: impl IntoIterator<Item = NoteTag>,
) -> Result<(), RpcError> {
    for tag in returned {
        if !requested.contains(&tag) {
            let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned note with tag {tag} but [{list}] were requested"
            )));
        }
    }
    Ok(())
}

/// Returns an error if any note in a `GetNotesById` response has an ID that was not requested.
fn ensure_requested_note_ids(
    requested: &BTreeSet<NoteId>,
    returned: impl IntoIterator<Item = NoteId>,
) -> Result<(), RpcError> {
    for id in returned {
        if !requested.contains(&id) {
            let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned note {id} but [{list}] were requested"
            )));
        }
    }
    Ok(())
}

// GRPC CLIENT
// ================================================================================================

/// Client for the Node RPC API using gRPC.
///
/// If the `tonic` feature is enabled, this client will use a `tonic::transport::Channel` to
/// communicate with the node. In this case the connection will be established lazily when the
/// first request is made.
/// If the `web-tonic` feature is enabled, this client will use a `tonic_web_wasm_client::Client`
/// to communicate with the node.
///
/// In both cases, the [`GrpcClient`] depends on the types inside the `generated` module, which
/// are generated by the build script and also depend on the target architecture.
pub struct GrpcClient {
    /// The underlying gRPC client, lazily initialized on first request.
    client: RwLock<Option<ApiClient>>,
    /// The node endpoint URL to connect to.
    endpoint: String,
    /// Request timeout in milliseconds.
    timeout_ms: u64,
    /// The genesis block commitment, used for request validation by the node.
    genesis_commitment: RwLock<Option<Word>>,
    /// Cached RPC limits fetched from the node.
    limits: RwLock<Option<RpcLimits>>,
    /// Maximum number of retry attempts for rate-limited or transiently unavailable requests.
    max_retries: u32,
    /// Fallback retry interval in milliseconds when no `retry-after` header is present.
    retry_interval_ms: u64,
    /// Optional bearer token injected as `authorization: Bearer <token>` on every outbound
    /// gRPC call, alongside the standard `accept` header. Used when talking to an
    /// authenticating gateway in front of the node.
    bearer_token: Option<String>,
}

impl GrpcClient {
    /// Returns a new instance of [`GrpcClient`] that'll do calls to the provided [`Endpoint`]
    /// with the given timeout in milliseconds.
    pub fn new(endpoint: &Endpoint, timeout_ms: u64) -> GrpcClient {
        GrpcClient {
            client: RwLock::new(None),
            endpoint: endpoint.to_string(),
            timeout_ms,
            genesis_commitment: RwLock::new(None),
            limits: RwLock::new(None),
            max_retries: retry::DEFAULT_MAX_RETRIES,
            retry_interval_ms: retry::DEFAULT_RETRY_INTERVAL_MS,
            bearer_token: None,
        }
    }

    /// Sets the maximum number of retry attempts for rate-limited or transiently unavailable
    /// requests. Defaults to `4`.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Sets the fallback retry interval in milliseconds, used when the server does not provide
    /// a `retry-after` header. Defaults to `100` ms.
    #[must_use]
    pub fn with_retry_interval_ms(mut self, retry_interval_ms: u64) -> Self {
        self.retry_interval_ms = retry_interval_ms;
        self
    }

    /// Attaches a `authorization: Bearer <token>` header to every outbound gRPC call made
    /// by this client, alongside the standard `accept` header.
    ///
    /// Intended for connecting to a Miden node through an authenticating gateway
    /// (e.g. `miden-testnet.eu-central-8.gateway.fm`) that rate-limits unauthenticated
    /// traffic. Without an auth mechanism on the client side, callers would have no way
    /// to supply the token the gateway requires.
    ///
    /// Calling this method twice overwrites the earlier token.
    ///
    /// Validation of the token against
    /// [`AsciiMetadataValue`](tonic::metadata::AsciiMetadataValue) is deferred to
    /// connection time (printable ASCII plus tab only — `HeaderValue::from_str` semantics):
    /// invalid tokens surface as
    /// [`RpcError::ConnectionError`](crate::rpc::RpcError::ConnectionError) on the first
    /// request, so CR/LF header-injection attempts are rejected.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use miden_client::rpc::{Endpoint, GrpcClient};
    /// let endpoint = Endpoint::new("https".into(), "node.example".into(), Some(443));
    /// let client = GrpcClient::new(&endpoint, 10_000).with_bearer_auth("<api-key>".into());
    /// ```
    #[must_use]
    pub fn with_bearer_auth(mut self, token: String) -> Self {
        self.bearer_token = Some(token);
        self
    }

    /// Takes care of establishing the RPC connection if not connected yet. It ensures that the
    /// `rpc_api` field is initialized and returns a write guard to it.
    async fn ensure_connected(&self) -> Result<ApiClient, RpcError> {
        if self.client.read().is_none() {
            self.connect().await?;
        }

        Ok(self.client.read().as_ref().expect("rpc_api should be initialized").clone())
    }

    /// Connects to the Miden node, setting the client API with the provided URL, timeout and
    /// genesis commitment.
    async fn connect(&self) -> Result<(), RpcError> {
        let genesis_commitment = *self.genesis_commitment.read();
        let new_client = ApiClient::new_client(
            self.endpoint.clone(),
            self.timeout_ms,
            genesis_commitment,
            self.bearer_token.clone(),
        )
        .await?;
        let mut client = self.client.write();
        client.replace(new_client);

        Ok(())
    }

    fn rpc_error_from_status(&self, endpoint: RpcEndpoint, status: Status) -> RpcError {
        let genesis_commitment = self
            .genesis_commitment
            .read()
            .as_ref()
            .map_or_else(|| "none".to_string(), Word::to_hex);
        let context = AcceptHeaderContext {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            genesis_commitment,
        };
        RpcError::from_grpc_error_with_context(endpoint, status, context)
    }

    /// Executes an RPC call and automatically retries transient failures.
    ///
    /// The provided closure is invoked with a freshly connected [`ApiClient`] on each attempt.
    /// Retries are delegated to [`retry::RetryState`], which currently handles gRPC
    /// [`tonic::Code::ResourceExhausted`] and [`tonic::Code::Unavailable`] responses, including
    /// honoring cooldown delays when the node provides them.
    ///
    /// Returns the first successful gRPC response. If the call keeps failing after retries are
    /// exhausted, or if the error is not retryable, this returns the corresponding [`RpcError`]
    /// for the provided [`RpcEndpoint`].
    async fn call_with_retry<T: Send + 'static>(
        &self,
        endpoint: RpcEndpoint,
        mut call: impl FnMut(ApiClient) -> RpcFuture<Result<tonic::Response<T>, Status>>,
    ) -> Result<tonic::Response<T>, RpcError> {
        let mut retry_state = retry::RetryState::new(self.max_retries, self.retry_interval_ms);

        loop {
            let rpc_api = self.ensure_connected().await?;

            match call(rpc_api).await {
                Ok(response) => return Ok(response),
                Err(status) if retry_state.should_retry(&status).await => {},
                Err(status) => return Err(self.rpc_error_from_status(endpoint, status)),
            }
        }
    }

    /// Fetches RPC status without injecting an Accept header.
    ///
    /// This instantiates a separate API client without the Accept interceptor, so it does not
    /// reuse the primary gRPC client. Any caller-supplied
    /// [`with_bearer_auth`](Self::with_bearer_auth) token is still forwarded so gateway
    /// authentication keeps working.
    pub async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        let mut rpc_api = ApiClient::new_client_without_accept_header(
            self.endpoint.clone(),
            self.timeout_ms,
            self.bearer_token.clone(),
        )
        .await?;
        rpc_api
            .status(())
            .await
            .map_err(|status| self.rpc_error_from_status(RpcEndpoint::Status, status))
            .map(tonic::Response::into_inner)
            .and_then(RpcStatusInfo::try_from)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NodeRpcClient for GrpcClient {
    /// Sets the genesis commitment for the client. If the client is already connected, it will be
    /// updated to use the new commitment on subsequent requests. If the client is not connected,
    /// the commitment will be stored and used when the client connects. If the genesis commitment
    /// is already set, this method does nothing.
    fn has_genesis_commitment(&self) -> Option<Word> {
        *self.genesis_commitment.read()
    }

    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError> {
        // Check if already set before doing anything else
        if self.genesis_commitment.read().is_some() {
            // Genesis commitment is already set, ignoring the new value.
            return Ok(());
        }

        // Store the commitment for future connections
        self.genesis_commitment.write().replace(commitment);

        // If a client is already connected, update it to use the new genesis commitment.
        // If not connected, the commitment will be used when connect() is called.
        let mut client_guard = self.client.write();
        if let Some(client) = client_guard.as_mut() {
            client.set_genesis_commitment(commitment);
        }

        Ok(())
    }

    async fn submit_proven_transaction(
        &self,
        proven_transaction: ProvenTransaction,
        transaction_inputs: TransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        let request = proto::transaction::ProvenTransaction {
            transaction: proven_transaction.to_bytes(),
            transaction_inputs: Some(transaction_inputs.to_bytes()),
        };

        let api_response = self
            .call_with_retry(RpcEndpoint::SubmitProvenTx, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.submit_proven_tx(request).await })
            })
            .await?;

        Ok(BlockNumber::from(api_response.into_inner().block_num))
    }

    async fn submit_proven_batch(
        &self,
        proven_batch: ProvenBatch,
        proposed_batch: ProposedBatch,
        transaction_inputs: Vec<TransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        let request = proto::transaction::TransactionBatch {
            batch_proof: proven_batch.to_bytes(),
            proposed_batch: Some(proposed_batch.to_bytes()),
            transaction_inputs: transaction_inputs.iter().map(Serializable::to_bytes).collect(),
        };

        let api_response = self
            .call_with_retry(RpcEndpoint::SubmitProvenBatch, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.submit_proven_tx_batch(request).await })
            })
            .await?;

        Ok(BlockNumber::from(api_response.into_inner().block_num))
    }

    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        let request = proto::rpc::BlockHeaderByNumberRequest {
            block_num: block_num.as_ref().map(BlockNumber::as_u32),
            include_mmr_proof: Some(include_mmr_proof),
        };

        info!("Calling GetBlockHeaderByNumber: {:?}", request);

        let api_response = self
            .call_with_retry(RpcEndpoint::GetBlockHeaderByNumber, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_block_header_by_number(request).await })
            })
            .await?;

        let response = api_response.into_inner();

        let block_header: BlockHeader = response
            .block_header
            .ok_or(RpcError::ExpectedDataMissing("BlockHeader".into()))?
            .try_into()?;

        if let Some(requested) = block_num
            && block_header.block_num() != requested
        {
            return Err(RpcError::InvalidResponse(format!(
                "node returned header for block {} but block {requested} was requested",
                block_header.block_num(),
            )));
        }

        let mmr_proof = if include_mmr_proof {
            let forest = response
                .chain_length
                .ok_or(RpcError::ExpectedDataMissing("ChainLength".into()))?;
            let merkle_path: MerklePath = response
                .mmr_path
                .ok_or(RpcError::ExpectedDataMissing("MmrPath".into()))?
                .try_into()?;

            let forest_size = usize::try_from(forest).expect("u64 should fit in usize");
            let forest = Forest::new(forest_size).map_err(|_| {
                RpcError::InvalidResponse(format!("invalid forest size: {forest_size}"))
            })?;
            Some(MmrProof::new(
                MmrPath::new(forest, block_header.block_num().as_usize(), merkle_path),
                block_header.commitment(),
            ))
        } else {
            None
        };

        Ok((block_header, mmr_proof))
    }

    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        let limits = self.get_rpc_limits().await?;
        let requested_ids: BTreeSet<NoteId> = note_ids.iter().copied().collect();
        let mut notes = Vec::with_capacity(note_ids.len());
        for chunk in note_ids.chunks(limits.note_ids_limit as usize) {
            let request = proto::note::NoteIdList {
                ids: chunk.iter().map(|id| (*id).into()).collect(),
            };

            let api_response = self
                .call_with_retry(RpcEndpoint::GetNotesById, |mut rpc_api| {
                    let request = request.clone();
                    Box::pin(async move { rpc_api.get_notes_by_id(request).await })
                })
                .await?;

            let response_notes = api_response
                .into_inner()
                .notes
                .into_iter()
                .map(FetchedNote::try_from)
                .collect::<Result<Vec<FetchedNote>, RpcConversionError>>()?;

            ensure_requested_note_ids(&requested_ids, response_notes.iter().map(FetchedNote::id))?;

            notes.extend(response_notes);
        }
        Ok(notes)
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        let finality_level: proto::rpc::FinalityLevel = upper_bound.into();

        let request = proto::rpc::SyncChainMmrRequest {
            current_client_block_height: current_block_height.as_u32(),
            finality_level: finality_level.into(),
        };

        let response = self
            .call_with_retry(RpcEndpoint::SyncChainMmr, |mut rpc_api| {
                Box::pin(async move { rpc_api.sync_chain_mmr(request).await })
            })
            .await?;

        response.into_inner().try_into()
    }

    /// Sends a `GetAccount` request to the Miden node, and extracts the [`AccountProof`]
    /// from the response, as well as the block number that it was retrieved for.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    ///
    /// - The requested Account isn't returned by the node.
    /// - The block number of the requested Account doesn't match the response block number.
    /// - There was an error sending the request to the node.
    /// - The answer had a `None` for one of the expected fields.
    /// - There is an error during storage deserialization.
    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        let GetAccountRequest { storage, at, known_code, vault } = request;

        let known_code_commitment = known_code.as_ref().map_or(EMPTY_WORD, AccountCode::commitment);
        let mut known_codes_by_commitment: BTreeMap<Word, AccountCode> = BTreeMap::new();
        if let Some(account_code) = known_code {
            known_codes_by_commitment.insert(account_code.commitment(), account_code);
        }

        // We need the requested slots to interpret the node response.
        let requirements = match storage.clone() {
            StorageMapFetch::Slots(reqs) => reqs,
            StorageMapFetch::Skip | StorageMapFetch::All => AccountStorageRequirements::default(),
        };

        // Only request details for accounts with public state (Public or Network), passing the
        // known code commitment so the node can skip re-sending code we already hold.
        let account_details = if account_id.is_public() {
            Some(AccountDetailRequest {
                code_commitment: Some(known_code_commitment.into()),
                asset_vault_commitment: vault.into(),
                storage_request: storage.into(),
            })
        } else {
            None
        };

        let block_num = match at {
            AccountStateAt::Block(number) => Some(number.into()),
            AccountStateAt::ChainTip => None,
        };

        let proto_request = AccountRequest {
            account_id: Some(account_id.into()),
            block_num,
            details: account_details,
        };

        let response = self
            .call_with_retry(RpcEndpoint::GetAccount, |mut rpc_api| {
                let request = proto_request.clone();
                Box::pin(async move { rpc_api.get_account(request).await })
            })
            .await?
            .into_inner();

        let account_witness: AccountWitness = response
            .witness
            .ok_or(RpcError::ExpectedDataMissing("AccountWitness".to_string()))?
            .try_into()?;

        let response_block_num: BlockNumber = response
            .block_num
            .ok_or(RpcError::ExpectedDataMissing("response block num".to_string()))?
            .block_num
            .into();

        if let Some(requested) = block_num
            && requested.block_num != response_block_num.as_u32()
        {
            return Err(RpcError::InvalidResponse(format!(
                "node returned header for block {} but block {} was requested",
                response_block_num.as_u32(),
                requested.block_num
            )));
        }

        // For accounts with public state, details should be present when requested
        let headers = if account_witness.id().is_public() {
            let details = response
                .details
                .ok_or(RpcError::ExpectedDataMissing("Account.Details".to_string()))?
                .into_domain(&known_codes_by_commitment, &requirements)?;

            Some(details)
        } else {
            None
        };

        let proof = AccountProof::new(account_witness, headers)
            .map_err(|err| RpcError::InvalidResponse(err.to_string()))?;

        Ok((response_block_num, proof))
    }

    /// Sends one or more `SyncNoteRequest`s to the node and merges the responses into a list of
    /// [`NoteSyncBlock`]s.
    ///
    /// Chunks `note_tags` by [`RpcLimits::note_tags_limit`] and paginates each chunk across the
    /// requested block range.
    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<NoteSyncBlock>, RpcError> {
        if note_tags.is_empty() {
            return Ok(Vec::new());
        }

        let limits = self.get_rpc_limits().await?;
        let tags: Vec<NoteTag> = note_tags.iter().copied().collect();

        // Merge blocks across tag-chunks: a single block can hold notes whose tags fall into
        // different chunks, so the same block can appear in multiple chunks' responses.
        let mut merged_blocks: BTreeMap<BlockNumber, NoteSyncBlock> = BTreeMap::new();

        for chunk in tags.chunks(limits.note_tags_limit as usize) {
            let proto_tags: Vec<u32> = chunk.iter().map(|&t| t.into()).collect();
            let requested_tags: BTreeSet<NoteTag> = chunk.iter().copied().collect();
            let mut pagination = BlockPagination::new(block_from, block_to);

            loop {
                let request = proto::rpc::SyncNotesRequest {
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: block_to.as_u32(),
                    }),
                    note_tags: proto_tags.clone(),
                };

                let response = self
                    .call_with_retry(RpcEndpoint::SyncNotes, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_notes(request).await })
                    })
                    .await?
                    .into_inner();

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncNotesResponse.pagination_info".to_owned(),
                ))?;
                let page_chain_tip = BlockNumber::from(page.chain_tip);
                let page_block_to = BlockNumber::from(page.block_num);

                for proto_block in response.blocks {
                    let block: NoteSyncBlock = proto_block.try_into()?;
                    ensure_requested_tags(
                        &requested_tags,
                        block.notes.values().map(CommittedNote::tag),
                    )?;
                    let bn = block.block_header.block_num();
                    if let Some(existing) = merged_blocks.get_mut(&bn) {
                        for (id, note) in block.notes {
                            existing.notes.entry(id).or_insert(note);
                        }
                    } else {
                        merged_blocks.insert(bn, block);
                    }
                }

                match pagination.advance(page_block_to, page_chain_tip)? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }

        Ok(merged_blocks.into_values().collect())
    }

    async fn sync_nullifiers(
        &self,
        prefixes: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        let limits = self.get_rpc_limits().await?;
        let mut all_nullifiers = BTreeSet::new();

        // If the prefixes are too many, we need to chunk them into smaller groups to avoid
        // violating the RPC limit.
        for chunk in prefixes.chunks(limits.nullifiers_limit as usize) {
            let proto_prefixes: Vec<u32> = chunk.iter().map(|&x| u32::from(x)).collect();
            let requested_prefixes: BTreeSet<u16> = chunk.iter().copied().collect();
            let mut pagination = BlockPagination::new(block_from, block_to);

            loop {
                let request = proto::rpc::SyncNullifiersRequest {
                    nullifiers: proto_prefixes.clone(),
                    prefix_len: 16,
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: pagination.block_to().as_u32(),
                    }),
                };

                let response = self
                    .call_with_retry(RpcEndpoint::SyncNullifiers, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_nullifiers(request).await })
                    })
                    .await?
                    .into_inner();

                let batch_nullifiers = response
                    .nullifiers
                    .iter()
                    .map(TryFrom::try_from)
                    .collect::<Result<Vec<NullifierUpdate>, _>>()
                    .map_err(|err| RpcError::InvalidResponse(err.to_string()))?;

                ensure_requested_nullifiers(&requested_prefixes, &batch_nullifiers)?;
                all_nullifiers.extend(batch_nullifiers);

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncNullifiersResponse.pagination_info".to_owned(),
                ))?;

                match pagination.advance(page.block_num.into(), page.chain_tip.into())? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }
        Ok(all_nullifiers.into_iter().collect::<Vec<_>>())
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<ProvenBlock, RpcError> {
        let request = proto::blockchain::BlockRequest {
            block_num: block_num.as_u32(),
            include_proof: Some(include_proof),
        };

        let response = self
            .call_with_retry(RpcEndpoint::GetBlockByNumber, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_block_by_number(request).await })
            })
            .await?;

        let response = response.into_inner();
        let block =
            ProvenBlock::read_from_bytes(&response.block.ok_or(RpcError::ExpectedDataMissing(
                "GetBlockByNumberResponse.block".to_string(),
            ))?)?;

        if block.header().block_num() != block_num {
            return Err(RpcError::InvalidResponse(format!(
                "node returned header for block {} but block {block_num} was requested",
                block.header().block_num(),
            )));
        }

        Ok(block)
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        let request = proto::note::NoteScriptRoot { root: Some(root.into()) };

        let response = self
            .call_with_retry(RpcEndpoint::GetNoteScriptByRoot, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_note_script_by_root(request).await })
            })
            .await?;

        // The node returns an empty payload when it has no script registered for the root.
        let Some(script) = response.into_inner().script else {
            return Ok(None);
        };
        let note_script = NoteScript::try_from(script)?;

        let fetched_root = note_script.root();
        if Word::from(fetched_root) != root {
            return Err(RpcError::InvalidResponse(format!(
                "node returned note script with root {fetched_root} for requested root {root}",
            )));
        }

        Ok(Some(note_script))
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        let mut pagination = BlockPagination::new(block_from, block_to);
        let mut updates = Vec::new();

        let (chain_tip, block_number) = loop {
            let request = proto::rpc::SyncAccountStorageMapsRequest {
                block_range: Some(BlockRange {
                    block_from: pagination.current_block_from().as_u32(),
                    block_to: block_to.as_u32(),
                }),
                account_id: Some(account_id.into()),
            };
            let response = self
                .call_with_retry(RpcEndpoint::SyncStorageMaps, |mut rpc_api| {
                    let request = request.clone();
                    Box::pin(async move { rpc_api.sync_account_storage_maps(request).await })
                })
                .await?;
            let response = response.into_inner();
            let page = response
                .pagination_info
                .ok_or(RpcError::ExpectedDataMissing("pagination_info".to_owned()))?;
            let page_block_num = BlockNumber::from(page.block_num);
            let page_chain_tip = BlockNumber::from(page.chain_tip);
            let batch = response
                .updates
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<StorageMapUpdate>, _>>()?;
            updates.extend(batch);

            match pagination.advance(page_block_num, page_chain_tip)? {
                PaginationResult::Continue => {},
                PaginationResult::Done {
                    chain_tip: final_chain_tip,
                    block_num: final_block_num,
                } => break (final_chain_tip, final_block_num),
            }
        };

        Ok(StorageMapInfo { chain_tip, block_number, updates })
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        let mut pagination = BlockPagination::new(block_from, block_to);
        let mut updates = Vec::new();

        let (chain_tip, block_number) = loop {
            let request = proto::rpc::SyncAccountVaultRequest {
                block_range: Some(BlockRange {
                    block_from: pagination.current_block_from().as_u32(),
                    block_to: block_to.as_u32(),
                }),
                account_id: Some(account_id.into()),
            };
            let response = self
                .call_with_retry(RpcEndpoint::SyncAccountVault, |mut rpc_api| {
                    let request = request.clone();
                    Box::pin(async move { rpc_api.sync_account_vault(request).await })
                })
                .await?;
            let response = response.into_inner();
            let page = response
                .pagination_info
                .ok_or(RpcError::ExpectedDataMissing("pagination_info".to_owned()))?;
            let page_block_num = BlockNumber::from(page.block_num);
            let page_chain_tip = BlockNumber::from(page.chain_tip);
            let batch = response
                .updates
                .iter()
                .map(|u| (*u).try_into())
                .collect::<Result<Vec<AccountVaultUpdate>, _>>()?;
            updates.extend(batch);

            match pagination.advance(page_block_num, page_chain_tip)? {
                PaginationResult::Continue => {},
                PaginationResult::Done {
                    chain_tip: final_chain_tip,
                    block_num: final_block_num,
                } => break (final_chain_tip, final_block_num),
            }
        };

        Ok(AccountVaultInfo { chain_tip, block_number, updates })
    }

    /// Sends one or more `SyncTransactions` requests to the node and concatenates the responses
    /// into a flat list of [`TransactionRecord`]s.
    ///
    /// Chunks `account_ids` by [`RpcLimits::account_ids_limit`] and paginates each chunk across the
    /// requested block range.
    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let limits = self.get_rpc_limits().await?;
        let mut transactions: Vec<TransactionRecord> = Vec::new();

        for chunk in account_ids.chunks(limits.account_ids_limit as usize) {
            let proto_account_ids: Vec<_> = chunk.iter().map(|acc_id| (*acc_id).into()).collect();
            let mut pagination = BlockPagination::new(block_from, block_to);

            loop {
                let request = proto::rpc::SyncTransactionsRequest {
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: block_to.as_u32(),
                    }),
                    account_ids: proto_account_ids.clone(),
                };

                let response = self
                    .call_with_retry(RpcEndpoint::SyncTransactions, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_transactions(request).await })
                    })
                    .await?
                    .into_inner();

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncTransactionsResponse.pagination_info".to_owned(),
                ))?;
                let page_chain_tip = BlockNumber::from(page.chain_tip);
                let page_block_to = BlockNumber::from(page.block_num);

                for proto_tx in response.transactions {
                    transactions.push(TransactionRecord::try_from(proto_tx)?);
                }

                match pagination.advance(page_block_to, page_chain_tip)? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }

        Ok(transactions)
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        let endpoint: Endpoint =
            Endpoint::try_from(self.endpoint.as_str()).map_err(RpcError::InvalidNodeEndpoint)?;
        Ok(endpoint.to_network_id())
    }

    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
        // Return cached limits if available
        if let Some(limits) = *self.limits.read() {
            return Ok(limits);
        }

        // Fetch limits from the node
        let response = self
            .call_with_retry(RpcEndpoint::GetLimits, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_limits(()).await })
            })
            .await?;
        let limits = RpcLimits::try_from(response.into_inner()).map_err(RpcError::from)?;

        // Cache fetched values
        self.limits.write().replace(limits);
        Ok(limits)
    }

    fn has_rpc_limits(&self) -> Option<RpcLimits> {
        *self.limits.read()
    }

    async fn set_rpc_limits(&self, limits: RpcLimits) {
        self.limits.write().replace(limits);
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        GrpcClient::get_status_unversioned(self).await
    }

    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        let request = proto::note::NoteId { id: Some(note_id.into()) };

        let response = self
            .call_with_retry(RpcEndpoint::GetNetworkNoteStatus, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_network_note_status(request).await })
            })
            .await?;

        response.into_inner().try_into()
    }
}

// ERRORS
// ================================================================================================

impl RpcError {
    pub fn from_grpc_error_with_context(
        endpoint: RpcEndpoint,
        status: Status,
        context: AcceptHeaderContext,
    ) -> Self {
        if let Some(accept_error) =
            AcceptHeaderError::try_from_message_with_context(status.message(), context)
        {
            return Self::AcceptHeaderError(accept_error);
        }

        // Parse application-level error from status details
        let endpoint_error = parse_node_error(&endpoint, status.details(), status.message());

        let error_kind = GrpcError::from(&status);
        let source = Box::new(status) as Box<dyn Error + Send + Sync + 'static>;

        Self::RequestError {
            endpoint,
            error_kind,
            endpoint_error,
            source: Some(source),
        }
    }
}

impl From<&Status> for GrpcError {
    fn from(status: &Status) -> Self {
        GrpcError::from_code(status.code() as i32, Some(status.message().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use core::slice;
    use std::boxed::Box;
    use std::collections::BTreeSet;

    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{NoteId, NoteTag, Nullifier};
    use miden_protocol::{Felt, Word};

    use super::{
        BlockPagination,
        GrpcClient,
        NullifierUpdate,
        PaginationResult,
        ensure_requested_note_ids,
        ensure_requested_nullifiers,
        ensure_requested_tags,
    };
    use crate::alloc::string::ToString;
    use crate::rpc::{Endpoint, NodeRpcClient, RpcError};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn is_send_sync() {
        assert_send_sync::<GrpcClient>();
        assert_send_sync::<Box<dyn NodeRpcClient>>();
    }

    #[test]
    fn block_pagination_errors_when_block_num_goes_backwards() {
        let mut pagination = BlockPagination::new(10_u32.into(), 20_u32.into());

        let res = pagination.advance(9_u32.into(), 20_u32.into());
        assert!(matches!(res, Err(RpcError::PaginationError(_))));
    }

    #[test]
    fn block_pagination_errors_after_max_iterations() {
        let mut pagination = BlockPagination::new(0_u32.into(), 10_000_u32.into());
        let chain_tip: BlockNumber = 10_000_u32.into();

        for _ in 0..BlockPagination::MAX_ITERATIONS {
            let current = pagination.current_block_from();
            let res = pagination
                .advance(current, chain_tip)
                .expect("expected pagination to continue within iteration limit");
            assert!(matches!(res, PaginationResult::Continue));
        }

        let res = pagination.advance(pagination.current_block_from(), chain_tip);
        assert!(matches!(res, Err(RpcError::PaginationError(_))));
    }

    #[test]
    fn block_pagination_stops_at_min_of_block_to_and_chain_tip() {
        // block_to is beyond chain tip, so target should be chain_tip.
        let mut pagination = BlockPagination::new(0_u32.into(), 50_u32.into());

        let res = pagination
            .advance(30_u32.into(), 30_u32.into())
            .expect("expected pagination to succeed");

        assert!(matches!(
            res,
            PaginationResult::Done {
                chain_tip,
                block_num
            } if chain_tip.as_u32() == 30 && block_num.as_u32() == 30
        ));
    }

    #[test]
    fn block_pagination_advances_cursor_by_one() {
        let mut pagination = BlockPagination::new(5_u32.into(), 100_u32.into());

        let res = pagination
            .advance(5_u32.into(), 100_u32.into())
            .expect("expected pagination to succeed");
        assert!(matches!(res, PaginationResult::Continue));
        assert_eq!(pagination.current_block_from().as_u32(), 6);
    }

    // Function that returns a `Send` future from a dynamic trait that must be `Sync`.
    async fn dyn_trait_send_fut(client: Box<dyn NodeRpcClient>) {
        // This won't compile if `get_block_header_by_number` doesn't return a `Send+Sync` future.
        let res = client.get_block_header_by_number(None, false).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn future_is_send() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000);
        let client: Box<GrpcClient> = client.into();
        tokio::task::spawn(async move { dyn_trait_send_fut(client).await });
    }

    #[tokio::test]
    async fn set_genesis_commitment_sets_the_commitment_when_its_not_already_set() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000);

        assert!(client.genesis_commitment.read().is_none());

        let commitment = Word::default();
        client.set_genesis_commitment(commitment).await.unwrap();

        assert_eq!(client.genesis_commitment.read().unwrap(), commitment);
    }

    #[tokio::test]
    async fn set_genesis_commitment_does_nothing_if_the_commitment_is_already_set() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000);

        let initial_commitment = Word::default();
        client.set_genesis_commitment(initial_commitment).await.unwrap();

        let new_commitment = Word::from([1u32, 2, 3, 4]);
        client.set_genesis_commitment(new_commitment).await.unwrap();

        assert_eq!(client.genesis_commitment.read().unwrap(), initial_commitment);
    }

    #[tokio::test]
    async fn set_genesis_commitment_updates_the_client_if_already_connected() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000);

        // "Connect" the client
        client.connect().await.unwrap();

        let commitment = Word::default();
        client.set_genesis_commitment(commitment).await.unwrap();

        assert_eq!(client.genesis_commitment.read().unwrap(), commitment);
        assert!(client.client.read().as_ref().is_some());
    }

    #[test]
    fn with_bearer_auth_stores_token() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000).with_bearer_auth("token-one".to_string());

        assert_eq!(client.bearer_token.as_deref(), Some("token-one"));
    }

    #[test]
    fn with_bearer_auth_overwrites_on_repeat_call() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000)
            .with_bearer_auth("token-one".to_string())
            .with_bearer_auth("token-two".to_string());

        // Second call replaces the first.
        assert_eq!(client.bearer_token.as_deref(), Some("token-two"));
    }

    #[tokio::test]
    async fn with_bearer_auth_surfaces_invalid_ascii_value_at_connect_time() {
        // Tokens containing control characters are rejected by `AsciiMetadataValue`. The
        // fluent builder defers the check to connection time, so the error must surface as
        // a `ConnectionError` on the first request — preventing CR/LF header-injection.
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000).with_bearer_auth("bad\nvalue".to_string());

        let err = client.connect().await.expect_err("expected invalid token to fail connect");
        assert!(
            matches!(err, RpcError::ConnectionError(_)),
            "expected ConnectionError, got {err:?}",
        );
    }

    #[tokio::test]
    async fn with_bearer_auth_is_preserved_across_set_genesis_commitment() {
        let endpoint = &Endpoint::devnet();
        let client = GrpcClient::new(endpoint, 10000).with_bearer_auth("token".to_string());
        client.connect().await.unwrap();

        client.set_genesis_commitment(Word::default()).await.unwrap();

        // Rebuilding the interceptor after a genesis update must not drop the caller token.
        assert_eq!(client.bearer_token.as_deref(), Some("token"));
        assert!(client.client.read().as_ref().is_some());
    }

    /// Real-network smoke test: hitting the public testnet with a caller-supplied bearer
    /// token must return a real [`RpcStatusInfo`], proving the header is a valid
    /// [`AsciiMetadataValue`](tonic::metadata::AsciiMetadataValue) on the wire and that an
    /// unauthenticated node ignores it cleanly.
    ///
    /// `#[ignore]`d by default so offline CI doesn't fail; run with
    /// `cargo test -- --ignored with_bearer_auth_does_not_break_real_rpc_against_testnet`
    /// when validating against the real network. The interceptor-level test
    /// (`api_client::tests::interceptor_injects_bearer_token_onto_request`)
    /// already proves the header reaches outbound request metadata without needing the
    /// network.
    #[tokio::test]
    #[ignore = "requires network access to public testnet"]
    async fn with_bearer_auth_does_not_break_real_rpc_against_testnet() {
        let endpoint = &Endpoint::testnet();
        let client = GrpcClient::new(endpoint, 10_000).with_bearer_auth("smoke-test".to_string());

        let status = client
            .get_status_unversioned()
            .await
            .expect("testnet status with caller auth header must succeed");
        assert!(!status.version.is_empty(), "status must include a server version");
    }

    fn nullifier_with_prefix(prefix: u16) -> Nullifier {
        Nullifier::from_raw(Word::new([
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::new_unchecked(u64::from(prefix) << 48),
        ]))
    }

    #[test]
    fn verify_requested_nullifiers_rejects_unrequested_prefix() {
        let requested = NullifierUpdate {
            nullifier: nullifier_with_prefix(0x1234),
            block_num: 1u32.into(),
        };
        let unrequested = NullifierUpdate {
            nullifier: nullifier_with_prefix(0xabcd),
            block_num: 2u32.into(),
        };

        let requested_prefixes: BTreeSet<u16> = BTreeSet::from([0x1234]);

        ensure_requested_nullifiers(&requested_prefixes, slice::from_ref(&requested))
            .expect("requested prefix must be accepted");

        let err = ensure_requested_nullifiers(&requested_prefixes, &[requested, unrequested])
            .expect_err("unrequested prefix must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[test]
    fn ensure_requested_tags_rejects_unrequested() {
        let requested = NoteTag::new(1);
        let other = NoteTag::new(2);
        let requested_set = BTreeSet::from([requested]);

        ensure_requested_tags(&requested_set, [requested]).expect("requested tag must be accepted");

        let err = ensure_requested_tags(&requested_set, [other])
            .expect_err("unrequested tag must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    fn note_id(n: u32) -> NoteId {
        NoteId::from_raw(Word::from([n, 0, 0, 0]))
    }

    #[test]
    fn ensure_requested_note_ids_rejects_unrequested() {
        let requested = note_id(1);
        let other = note_id(2);
        let requested_set = BTreeSet::from([requested]);

        ensure_requested_note_ids(&requested_set, [requested])
            .expect("requested note id must be accepted");

        let err = ensure_requested_note_ids(&requested_set, [other])
            .expect_err("unrequested note id must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }
}
