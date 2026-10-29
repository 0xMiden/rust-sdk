use std::collections::BTreeMap;

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::account::{
    AccountProof,
    AccountStorageRequirements,
    GetAccountRequest,
    StorageMapFetch,
};
use miden_client_core::rpc::{AccountStateAt, RpcEndpoint};
use miden_objects::DecodeMessageExt;
use miden_protocol::account::{AccountCode, AccountId};
use miden_protocol::block::BlockNumber;
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::{EMPTY_WORD, Word};

use crate::conversions::{storage_map_fetch_to_proto, vault_fetch_to_proto};
use crate::generated::node::v1::get_account_request::AccountDetailRequest;
use crate::validation::verify_block_num;
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the proof of an account and, for accounts with public state, its details.
pub struct GetAccount {
    /// The account to fetch.
    pub account_id: AccountId,
    /// The block and the details to fetch.
    pub request: GetAccountRequest,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetAccount {
    /// The block that the proof is for, and the proof.
    type Output = (BlockNumber, AccountProof);

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let GetAccountRequest { storage, at, known_code, vault } = self.request.clone();

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
        let account_details = if self.account_id.is_public() {
            Some(AccountDetailRequest {
                code_commitment: Some(known_code_commitment.into()),
                asset_vault_commitment: vault_fetch_to_proto(&vault),
                storage_request: storage_map_fetch_to_proto(storage),
            })
        } else {
            None
        };

        let block_num = match at {
            AccountStateAt::Block(number) => Some(number.into()),
            AccountStateAt::ChainTip => None,
        };

        let request = proto::node::v1::GetAccountRequest {
            account_id: Some(self.account_id.into()),
            block_num,
            details: account_details,
        };

        let response = client
            .call_with_retry(RpcEndpoint::GetAccount, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.get_account(request).await })
            })
            .await?
            .into_inner();

        let account_witness: AccountWitness = response
            .witness
            .ok_or(RpcError::ExpectedDataMissing("AccountWitness".to_string()))?
            .decode_and_verify()?;

        let response_block_num: BlockNumber = response
            .block_num
            .ok_or(RpcError::ExpectedDataMissing("response block num".to_string()))?
            .block_num
            .into();

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

    fn validate(&self, (block_num, proof): &Self::Output) -> Result<(), RpcError> {
        let requested = match self.request.at {
            AccountStateAt::Block(number) => Some(number),
            AccountStateAt::ChainTip => None,
        };
        verify_block_num(requested, *block_num)?;

        if proof.account_id() != self.account_id {
            return Err(RpcError::InvalidResponse(format!(
                "node returned proof for account {} but {} was requested",
                proof.account_id(),
                self.account_id,
            )));
        }
        Ok(())
    }
}
