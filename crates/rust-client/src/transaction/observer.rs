//! Side-effect-only observer trait for committed transactions.
//!
//! Analogous to [`crate::sync::NoteObserver`] but scoped to the transaction store updates. Lets
//! feature subsystems (e.g. PSWAP chain tracking) hook into the post-apply pipeline without the
//! apply methods knowing about them by name.

use alloc::boxed::Box;

use async_trait::async_trait;

use crate::ClientError;
use crate::transaction::TransactionOutcome;

/// Side-effect-only observer of committed transactions.
///
/// `apply()` runs once for each transaction, after the store applies the transaction. The client
/// runs it from `Client::apply_transaction_update` and `Client::apply_batch_update`, so every apply
/// path reaches it. Errors are logged (tagged with [`Self::name`]) and never abort the apply.
#[async_trait(?Send)]
pub trait TransactionObserver: Send + Sync {
    /// Short identifier for `tracing::warn!` events on `apply()` errors.
    fn name(&self) -> &'static str;

    /// Return `Ok(())` for "not interested"; reserve `Err(_)` for genuine internal failures.
    async fn apply(&self, transaction: &TransactionOutcome) -> Result<(), ClientError>;
}
