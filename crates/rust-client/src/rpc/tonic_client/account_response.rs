//! Decodes `GetAccount` responses into domain types.
//!
//! These conversions need the request they answer, so they live with the client that sends it and
//! not with the context-free conversions.

use alloc::collections::BTreeMap;
use alloc::string::ToString;

use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::account::{AccountCode, AccountHeader};

use crate::rpc::RpcError;
use crate::rpc::conversions::MissingFieldHelper;
use crate::rpc::domain::account::{
    AccountDetails,
    AccountProof,
    AccountStorageDetails,
    AccountStorageRequirements,
};
use crate::rpc::generated::{self as proto};

// ACCOUNT DETAILS
// ================================================================================================

impl proto::rpc::account_response::AccountDetails {
    /// Converts the RPC response into `AccountDetails`.
    ///
    /// The RPC response may omit unchanged account codes. If so, this function uses
    /// `known_account_codes` to fill in the missing code. If a required code cannot be found in the
    /// response or `known_account_codes`, an error is returned.
    ///
    /// `storage_requirements` is the request this response answers, used to check that each partial
    /// map covers exactly the keys that were asked for.
    ///
    /// # Errors
    /// - If account code is missing both on `self` and `known_account_codes`
    /// - If data cannot be correctly deserialized
    /// - If a partial map does not cover exactly the keys requested for its slot
    pub fn into_domain(
        self,
        known_account_codes: &BTreeMap<Word, AccountCode>,
        storage_requirements: &AccountStorageRequirements,
    ) -> Result<AccountDetails, crate::rpc::RpcError> {
        let proto::rpc::account_response::AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        } = self;
        let header: AccountHeader = header
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(header)))?
            .decode_and_verify()?;

        let storage_details: AccountStorageDetails = storage_details
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(
                storage_details
            )))?
            .try_into()?;

        storage_details.validate_against_request(storage_requirements)?;

        // If an account code was received, it means the previously known account code is no longer
        // valid. If it was not, it means we sent a code commitment that matched and so our code is
        // still valid
        let code = {
            let received_code: Option<AccountCode> =
                code.map(DecodeMessageExt::decode_and_verify).transpose()?;
            match received_code {
                Some(code) => code,
                None => known_account_codes
                    .get(&header.code_commitment())
                    .ok_or(RpcError::InvalidResponse(
                        "Account code was not provided, but the response did not contain it either"
                            .into(),
                    ))?
                    .clone(),
            }
        };

        let vault_details = vault_details
            .ok_or(proto::rpc::AccountVaultDetails::missing_field(stringify!(vault_details)))?
            .try_into()?;

        Ok(AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        })
    }
}

// ACCOUNT PROOF
// ================================================================================================

impl TryFrom<proto::rpc::AccountResponse> for AccountProof {
    type Error = RpcError;
    fn try_from(account_proof: proto::rpc::AccountResponse) -> Result<Self, Self::Error> {
        let Some(witness) = account_proof.witness else {
            return Err(RpcError::ExpectedDataMissing(
                "GetAccount returned an account without witness".to_string(),
            ));
        };

        let details: Option<AccountDetails> = {
            match account_proof.details {
                None => None,
                Some(details) => Some(
                    details
                        .into_domain(&BTreeMap::new(), &AccountStorageRequirements::default())?,
                ),
            }
        };
        AccountProof::new(witness.decode_and_verify()?, details)
            .map_err(|err| RpcError::InvalidResponse(format!("{err}")))
    }
}
