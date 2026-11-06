//! Contains structures and functions related to transaction creation.

mod foreign;
pub use miden_client_core::transaction::{
    ForeignAccount,
    InputNoteAuthentication,
    NoteArgs,
    PaymentNoteDescription,
    PswapTransactionData,
    RequestAuthArgs,
    RequestInputNote,
    RequestScript,
    SwapTransactionData,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    build_fpi_script,
};
