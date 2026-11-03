//! Contains structures and functions related to transaction creation.

mod foreign;
pub use miden_client_core::transaction::{
    ForeignAccount,
    InputNoteAuthentication,
    NoteArgs,
    PaymentNoteDescription,
    PswapTransactionData,
    RequestInputNote,
    SwapTransactionData,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionScriptTemplate,
    build_fpi_script,
};
