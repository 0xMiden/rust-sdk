use miden_client_core::errors::RpcError;
use miden_protocol::block::BlockNumber;

/// Returns [`RpcError::InvalidResponse`] if `requested` is `Some` and `returned` does not equal it.
pub(crate) fn verify_block_num(
    requested: Option<BlockNumber>,
    returned: BlockNumber,
) -> Result<(), RpcError> {
    if let Some(requested) = requested
        && returned != requested
    {
        return Err(RpcError::InvalidResponse(format!(
            "node returned block {returned} but block {requested} was requested"
        )));
    }
    Ok(())
}
