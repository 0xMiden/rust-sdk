# RPC method proposal

**Recommendation:** try a small internal `GrpcMethod` trait. Keep `Arc<dyn NodeRpcClient>`, generated tonic calls, and the existing retry helper. The main gains are code organization and testing. These examples are design sketches, not compiled changes.

**Current versus proposed**

Today, [`GrpcClient`](crates/rust-client/src/rpc/tonic_client/mod.rs) builds requests, retries calls, and converts responses. [`VerifyingRpcClient`](crates/rust-client/src/rpc/verifying_client/mod.rs) checks selected request/response relationships. [`NodeRpcClient`](crates/rust-client/src/rpc/mod.rs) already supports dynamic dispatch.

| Aspect | Status quo | Proposed |
| --- | --- | --- |
| Endpoint logic | Conversion and matching checks are split | Request context and response handling live together |
| Execution | Each endpoint calls the retry helper | One executor runs `request → retry(send) → finish` |
| Testing | Domain-level mocks and conversion helpers | Also test `finish` with request context and protobuf responses |
| Custom clients | Wrapper checks different implementations | Still need the wrapper or equivalent shared checks |
| Complexity | Large, direct async methods | More types and bounds; public forwarding methods remain |
| Performance | Existing transport and retries | No inherent speedup; watch extra clones and duplicate checks |

The trait enforces a sequence, not complete validation. Separate endpoint modules with response helpers are a simpler alternative if organization is the only problem.

**Execution contract**

`Request` and `Response` are protobuf types; `Output` is a domain type. `send` selects the generated endpoint. `finish` converts and validates using the retained request context. Tonic/prost handle wire encoding.

```rust
trait GrpcMethod: Send + Sync {
    type Request: Clone + Send + Sync + 'static;
    type Response: Send + 'static;
    type Output;

    const ENDPOINT: RpcEndpoint;

    fn request(&self) -> Result<Self::Request, RpcError>;
    fn send(
        client: ApiClient,
        request: Self::Request,
    ) -> RpcFuture<Result<tonic::Response<Self::Response>, tonic::Status>>;
    fn finish(&self, response: Self::Response) -> Result<Self::Output, RpcError>;
}

impl GrpcClient {
    async fn execute<M: GrpcMethod>(&self, method: M) -> Result<M::Output, RpcError> {
        let request = method.request()?;
        let response = self
            .call_with_retry(M::ENDPOINT, |client| M::send(client, request.clone()))
            .await?
            .into_inner();
        method.finish(response)
    }
}
```

`ApiClient` and `RpcFuture` are existing internal types. Preserve native/WASM future behavior. Keep `execute` outside the trait so methods cannot override its sequence. `finish` can call separate conversion and validation helpers without more trait hooks.

**Example: one header, without an MMR proof**

This covers a fixed-height request. The public endpoint would also support latest-header requests and optional proofs.

```rust
struct GetHeader(BlockNumber);

impl GrpcMethod for GetHeader {
    type Request = proto::rpc::BlockHeaderByNumberRequest;
    type Response = proto::rpc::BlockHeaderByNumberResponse;
    type Output = BlockHeader;
    const ENDPOINT: RpcEndpoint = RpcEndpoint::GetBlockHeaderByNumber;

    fn request(&self) -> Result<Self::Request, RpcError> {
        Ok(Self::Request {
            block_num: Some(self.0.as_u32()),
            include_mmr_proof: Some(false),
        })
    }

    fn send(
        mut client: ApiClient,
        request: Self::Request,
    ) -> RpcFuture<Result<tonic::Response<Self::Response>, tonic::Status>> {
        Box::pin(async move { client.get_block_header_by_number(request).await })
    }

    fn finish(&self, response: Self::Response) -> Result<BlockHeader, RpcError> {
        let header: BlockHeader = response.block_header
            .ok_or_else(|| RpcError::ExpectedDataMissing("BlockHeader".into()))?
            .try_into()?;
        if header.block_num() != self.0 {
            return Err(RpcError::InvalidResponse("block number mismatch".into()));
        }
        Ok(header)
    }
}
```

`grpc.execute(GetHeader(42.into())).await?` rejects a response for block 43. Public trait methods delegate to the concrete executor, preserving `dyn NodeRpcClient`. Matching the block number does not authenticate the header against trusted chain state.

**Boundaries to preserve**

- **Retries:** reuse endpoint-specific policy. Clone the prepared request per transport attempt; do not retry conversion or validation failures. Stale-key refresh belongs to submission orchestration.
- **Pagination:** an outer operation handles limits, chunks, cursors, and merging. Validate each page against its exact chunk before merging; check progress and cross-page consistency outside the method.
- **Validation:** retain the wrapper during migration and share helpers. Moving checks into gRPC does not protect custom clients. Proof verification stays with the component holding trusted state.
- **Transport:** retain connection management, headers, timeouts, and tracing in shared infrastructure.

**What the node does**

Checked at the client-pinned node revision `0d51bb31` (2026-09-07).

| Area | Technique | Lesson |
| --- | --- | --- |
| [Server generator][node-gen] | Per-endpoint traits; default `full` runs `decode → handle → encode`, records sizes, and bridges to tonic | Direct precedent for shared execution; generated forwarding reduces boilerplate |
| [RPC account handler][node-account] / [validator][node-validator] | Structural conversion and contextual validation happen in different stages | Put checks where their context is available |
| [NTX server][node-ntx-server] | Direct malformed-request decoder tests | Test endpoint stages without a server |
| [Remote prover][node-prover] | `ProveRequest` shares decoding, proving, encoding, and instrumentation | Small execution traits also work without code generation |
| [Outbound clients][node-clients] / [RPC middleware][node-middleware] | Generated tonic wrappers, shared builder/interceptor, Tower layers | Keep transport separate; these clients do not need a universal method executor |
| [Retry helpers][node-retry] / [NTX client][node-ntx-client] | Shared backoff, operation-specific closures, separate reconnection and stale-key recovery | Retry policy needs operation context |

Fixed hooks have costs: the [header handler][node-header] uses identity conversions; validator [status][node-status] overrides `full` and leaves unused hooks as `unimplemented!()`. Avoid unnecessary stages.

The node generates **one trait per endpoint, implemented by a service**. This proposal uses **one trait implemented by request values**. The shared-sequence idea transfers; the exact architecture differs.

Outside Miden, [tonic](https://docs.rs/tonic/latest/tonic/client/) supplies generated clients over shared transport; the [AWS Rust SDK](https://docs.rs/aws-sdk-s3/latest/src/aws_sdk_s3/operation/create_session.rs.html) combines typed operations with a shared runtime. Both are established patterns.

**Why keep `dyn NodeRpcClient`?**

A generic `execute<M>` cannot be dynamically dispatched, even with `async_trait`. Keep it on concrete `GrpcClient`. [Rust rules](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility).

Alternatively, a concrete facade could expose generic `execute<M>` over `Arc<dyn RpcTransport>`. The transport needs a non-generic boundary such as bytes or enums, plus adaptation to tonic's typed calls. Consider this only if sharing conversion/validation across backends or replacing the endpoint trait justifies the API change.

**First step:** prototype `GetBlockHeader`, `GetAccount`, and `SyncNotesPage`. Check response mismatches, missing proof fields, chunk membership, pagination progress, retries, and native/WASM compilation. Keep the trait only if these cases become easier to maintain.

[node-gen]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/proto/build.rs
[node-account]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/rpc/src/server/api/get_account.rs
[node-validator]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/bin/validator/src/server/validator_service/submit_proven_transaction.rs
[node-ntx-server]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/bin/ntx-builder/src/server/get_network_note_status.rs
[node-prover]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/bin/remote-prover/src/server/prover.rs
[node-clients]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/proto/src/clients/mod.rs
[node-middleware]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/rpc/src/server/mod.rs
[node-retry]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/utils/src/retry.rs
[node-ntx-client]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/bin/ntx-builder/src/clients/rpc.rs
[node-header]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/crates/rpc/src/server/api/get_block_header_by_number.rs
[node-status]: https://github.com/0xMiden/node/blob/0d51bb31795d1d2d48eda49f9a6b711f13fc37df/bin/validator/src/server/validator_service/status.rs
