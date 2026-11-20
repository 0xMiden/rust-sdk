use alloc::boxed::Box;

use miden_protocol::transaction::{ProvenTransaction, TransactionWitness};
use miden_tx::{LocalTransactionProver, TransactionProverError};

#[cfg(feature = "tonic")]
use crate::remote_prover::RemoteTransactionProver;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait TransactionProver {
    async fn prove(
        &self,
        witness: TransactionWitness,
    ) -> Result<ProvenTransaction, TransactionProverError>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TransactionProver for LocalTransactionProver {
    async fn prove(
        &self,
        witness: TransactionWitness,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        LocalTransactionProver::prove(self, witness)
    }
}

#[cfg(feature = "tonic")]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TransactionProver for RemoteTransactionProver {
    async fn prove(
        &self,
        witness: TransactionWitness,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        self.prove(&witness).await
    }
}
