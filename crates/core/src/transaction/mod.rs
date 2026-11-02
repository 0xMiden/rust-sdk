//! Defines the transaction types shared by the client and the store implementations.

mod chain_anchor;
pub use chain_anchor::{ChainAnchor, ChainAnchorError};

mod record;
pub use record::{
    DiscardCause,
    TransactionDetails,
    TransactionRecord,
    TransactionStatus,
    TransactionStatusVariant,
};

mod result;
pub use result::TransactionResult;

mod store_update;
pub use store_update::{BatchStoreUpdate, TransactionStoreUpdate};

mod request;
pub use request::{
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
    collect_assets,
};
