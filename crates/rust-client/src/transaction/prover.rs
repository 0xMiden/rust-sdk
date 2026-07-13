use alloc::boxed::Box;

use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};
use miden_tx::{LocalTransactionProver, TransactionProverError};

#[cfg(feature = "tonic")]
use crate::remote_prover::RemoteTransactionProver;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait TransactionProver {
    async fn prove(
        &self,
        tx_result: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TransactionProver for LocalTransactionProver {
    async fn prove(
        &self,
        witness: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        LocalTransactionProver::prove(self, witness).await
    }
}

#[cfg(feature = "tonic")]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TransactionProver for RemoteTransactionProver {
    async fn prove(
        &self,
        witness: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        self.prove(&witness).await
    }
}
