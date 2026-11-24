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

    /// The data of a validated item that [`validate`](Self::validate) checks the next item against.
    type State;

    /// Sends the request to the node and returns the stream of decoded items.
    fn open(
        &self,
        client: &GrpcClient,
    ) -> impl FutureMaybeSend<Result<RpcStream<Self::Item>, RpcError>>;

    /// Checks one decoded item against the request and the state of the previous item.
    ///
    /// The value of `previous` is `None` for the first item of the stream. The returned state is
    /// the `previous` value for the next item.
    fn validate(
        &self,
        previous: Option<&Self::State>,
        item: &Self::Item,
    ) -> Result<Self::State, RpcError>;
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
    let stream = futures::stream::try_unfold(
        (method, None::<M::State>, items),
        |(method, previous, mut items)| async move {
            let Some(item) = items.next().await.transpose()? else {
                return Ok(None);
            };
            let state = method.validate(previous.as_ref(), &item)?;
            Ok(Some((item, (method, Some(state), items))))
        },
    );

    #[cfg(not(target_arch = "wasm32"))]
    return stream.boxed();
    #[cfg(target_arch = "wasm32")]
    return stream.boxed_local();
}
