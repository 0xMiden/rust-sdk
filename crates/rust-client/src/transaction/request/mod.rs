//! Contains structures and functions related to transaction creation.

mod foreign;
pub(crate) use foreign::account_proof_into_inputs;
pub use miden_client_core::transaction::{
    ForeignAccount,
    NoteArgs,
    PaymentNoteDescription,
    PswapTransactionData,
    SwapTransactionData,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionScriptTemplate,
    build_fpi_script,
};
