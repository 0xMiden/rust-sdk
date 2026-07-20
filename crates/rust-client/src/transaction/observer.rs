//! Side-effect-only observer trait for committed transactions.
//!
//! Analogous to [`crate::sync::NoteObserver`] but scoped to
//! `Client::apply_transaction`. Lets feature subsystems (e.g. PSWAP
//! chain tracking) hook into the post-apply pipeline without
//! `apply_transaction` knowing about them by name.

use alloc::boxed::Box;

use async_trait::async_trait;

use crate::ClientError;
use crate::transaction::TransactionResult;

/// Side-effect-only observer of committed transactions. `apply()` runs
/// once per `apply_transaction` AFTER the standard updates land. Errors
/// are logged (tagged with [`Self::name`]) and never abort sync.
#[async_trait(?Send)]
pub trait TransactionObserver: Send + Sync {
    /// Short identifier for `tracing::warn!` events on `apply()` errors.
    fn name(&self) -> &'static str;

    /// Return `Ok(())` for "not interested"; reserve `Err(_)` for genuine
    /// internal failures.
    async fn apply(&self, tx_result: &TransactionResult) -> Result<(), ClientError>;
}
