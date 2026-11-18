use futures::StreamExt;
use miden_client_core::errors::RpcError;
use miden_protocol::vm::FutureMaybeSend;

use crate::GrpcClient;

// RPC STREAM
// ================================================================================================

/// A stream of decoded items from a server-streaming node RPC endpoint.
///
/// The stream does not borrow the [`GrpcClient`] that opens it.
#[cfg(not(target_arch = "wasm32"))]
pub type RpcStream<T> = futures::stream::BoxStream<'static, Result<T, RpcError>>;

/// A stream of decoded items from a server-streaming node RPC endpoint.
///
/// The stream does not borrow the [`GrpcClient`] that opens it.
#[cfg(target_arch = "wasm32")]
pub type RpcStream<T> = futures::stream::LocalBoxStream<'static, Result<T, RpcError>>;

// RPC STREAM METHOD
// ================================================================================================

/// A call to one server-streaming node RPC endpoint in one API version.
///
/// [`GrpcClient::subscribe`] runs [`open`](Self::open) and then runs [`validate`](Self::validate)
/// on each item of the stream.
pub trait RpcStreamMethod {
    /// A decoded stream item.
    type Item;

    /// The validation state that one item passes to the next item.
    type State;

    /// Sends the request to the node and returns the stream of decoded items.
    fn open(
        &self,
        client: &GrpcClient,
    ) -> impl FutureMaybeSend<Result<RpcStream<Self::Item>, RpcError>>;

    /// Returns the validation state for a new stream.
    fn initial_state(&self) -> Self::State;

    /// Checks one decoded item against the request and the items before it.
    fn validate(&self, state: &mut Self::State, item: &Self::Item) -> Result<(), RpcError>;
}

/// Returns `items` with [`RpcStreamMethod::validate`] applied to each item.
///
/// The returned stream ends after the first error. After the end, each poll returns `None`.
pub(crate) fn validate_items<M>(method: M, items: RpcStream<M::Item>) -> RpcStream<M::Item>
where
    M: RpcStreamMethod + Send + 'static,
    M::Item: Send,
    M::State: Send,
{
    let state = method.initial_state();
    let stream = futures::stream::try_unfold(
        (method, state, items),
        |(method, mut state, mut items)| async move {
            let Some(item) = items.next().await.transpose()? else {
                return Ok(None);
            };
            method.validate(&mut state, &item)?;
            Ok(Some((item, (method, state, items))))
        },
    );

    #[cfg(not(target_arch = "wasm32"))]
    return stream.boxed();
    #[cfg(target_arch = "wasm32")]
    return stream.boxed_local();
}
