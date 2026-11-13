use miden_client_core::errors::node::GetTransactionLogsError;
use miden_client_core::errors::{EndpointError, RpcError};
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::domain::transaction::TransactionLogsInfo;
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the logs of a committed transaction.
pub struct GetTransactionLogs {
    /// The account that executed the transaction.
    pub account_id: AccountId,
    /// The transaction that emitted the logs.
    pub transaction_id: TransactionId,
}

impl RpcMethod for GetTransactionLogs {
    /// The logs, or `None` if the node has no committed transaction with the ID for the account.
    type Output = Option<TransactionLogsInfo>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::GetTransactionLogsRequest {
            native_account_id: Some(self.account_id.into()),
            transaction_id: Some(self.transaction_id.into()),
        };

        let response = client
            .send_with_retry(RpcEndpoint::GetTransactionLogs, |mut rpc_api| {
                let request = request.clone();
                async move { rpc_api.get_transaction_logs(request).await }
            })
            .await;

        match response {
            Ok(response) => Ok(Some(response.into_inner().try_into()?)),
            Err(err) if is_transaction_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let Some(info) = output else {
            return Ok(());
        };

        if info.transaction_id != self.transaction_id {
            return Err(RpcError::InvalidResponse(format!(
                "node returned logs of transaction {} for requested transaction {}",
                info.transaction_id, self.transaction_id
            )));
        }
        if info.account_id != self.account_id {
            return Err(RpcError::InvalidResponse(format!(
                "node returned logs of account {} for requested account {}",
                info.account_id, self.account_id
            )));
        }
        // A public account submits its logs to the node. A private account submits only their
        // commitment.
        if info.logs().is_some() != self.account_id.is_public() {
            return Err(RpcError::InvalidResponse(format!(
                "node returned logs with the wrong visibility for account {}",
                self.account_id
            )));
        }

        Ok(())
    }
}

/// Returns whether the node reported that it has no such transaction for the account.
fn is_transaction_not_found(err: &RpcError) -> bool {
    matches!(
        err.endpoint_error(),
        Some(EndpointError::GetTransactionLogs(GetTransactionLogsError::TransactionNotFound))
    )
}
