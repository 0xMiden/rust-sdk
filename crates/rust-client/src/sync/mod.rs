//! Provides the client APIs for synchronizing the client's local state with the Miden
//! network. It ensures that the client maintains a valid, up-to-date view of the chain.
//!
//! ## Overview
//!
//! This module handles the synchronization process between the local client and the Miden network.
//! The sync operation involves:
//!
//! - Querying the Miden node for state updates using tracked account IDs, note tags, and nullifier
//!   prefixes.
//! - Processing the received data to update note inclusion proofs, reconcile note state (new,
//!   committed, or consumed), and update account states.
//! - Incorporating new block headers and updating the local Merkle Mountain Range (MMR) with new
//!   peaks and authentication nodes.
//! - Aggregating transaction updates to determine which transactions have been committed or
//!   discarded.
//!
//! The result of the synchronization process is captured in a [`SyncSummary`], which provides
//! a summary of the new block number along with lists of received, committed, and consumed note
//! IDs, updated account IDs, locked accounts, and committed transaction IDs.
//!
//! Once the data is requested and retrieved, updates are persisted in the client's store.
//!
//! ## Examples
//!
//! The following example shows how to initiate a state sync and handle the resulting summary:
//!
//! ```rust
//! # use miden_client::auth::TransactionAuthenticator;
//! # use miden_client::sync::SyncSummary;
//! # use miden_client::{Client, ClientError};
//! # use miden_protocol::{block::BlockHeader, Felt, Word};
//! # use miden_protocol::crypto::rand::FeltRng;
//! # async fn run_sync<AUTH: TransactionAuthenticator + Sync + 'static>(client: &mut Client<AUTH>) -> Result<(), ClientError> {
//! // Attempt to synchronize the client's state with the Miden network.
//! // The requested data is based on the client's state: it gets updates for accounts, relevant
//! // notes, etc. For more information on the data that gets requested, see the doc comments for
//! // `sync_state()`.
//! let sync_summary: SyncSummary = client.sync_state().await?;
//!
//! println!("Synced up to block number: {}", sync_summary.block_num);
//! println!("New private notes: {}", sync_summary.new_private_notes.len());
//! println!("Committed notes: {}", sync_summary.committed_notes.len());
//! println!("Consumed notes: {}", sync_summary.consumed_notes.len());
//! println!("Updated accounts: {}", sync_summary.updated_accounts.len());
//! println!("Locked accounts: {}", sync_summary.locked_accounts.len());
//! println!("Committed transactions: {}", sync_summary.committed_transactions.len());
//!
//! Ok(())
//! # }
//! ```
//!
//! The `sync_state` method loops internally until the client is fully synced to the network tip.
//!
//! For more advanced usage, refer to the individual functions (such as
//! `committed_note_updates` and `consumed_note_updates`) to understand how the sync data is
//! processed and applied to the local store.

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::max;

use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, PartialMmr};
use miden_protocol::note::NoteId;
use miden_protocol::transaction::TransactionId;
use miden_tx::auth::TransactionAuthenticator;
use miden_tx::utils::serde::{Deserializable, DeserializationError, Serializable};
use tracing::{debug, info};

use crate::pswap::PswapChainObserver;
use crate::store::{NoteFilter, TransactionFilter};
use crate::{Client, ClientError};
mod block_header;

mod tag;
pub use tag::{NoteTagRecord, NoteTagSource};

mod note_observer;
pub use note_observer::NoteObserver;

mod state_sync;
pub use state_sync::{NoteUpdateAction, OnNoteReceived, StateSync, StateSyncInput};

mod state_sync_update;
pub use state_sync_update::{
    AccountUpdates,
    PartialBlockchainUpdates,
    PublicAccountUpdate,
    StateSyncUpdate,
    TransactionUpdateTracker,
};

/// Untracks the given block leaves from `partial_mmr`, returning the authentication-node indices
/// that are no longer needed by any remaining tracked leaf.
///
/// Untracking a leaf frees an inner node only once no other tracked leaf still needs it, so the
/// returned indices are exactly the nodes that became removable.
fn untrack_blocks(
    partial_mmr: &mut PartialMmr,
    block_positions: impl IntoIterator<Item = usize>,
) -> Vec<InOrderIndex> {
    block_positions
        .into_iter()
        .flat_map(|block_pos| partial_mmr.untrack(block_pos))
        .map(|(index, _)| index)
        .collect()
}

/// Client synchronization methods.
impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    // SYNC STATE
    // --------------------------------------------------------------------------------------------

    /// Returns the block number of the last state sync block.
    pub async fn get_sync_height(&self) -> Result<BlockNumber, ClientError> {
        self.store.get_sync_height().await.map_err(Into::into)
    }

    /// Syncs the client's on-chain state with the current state of the Miden network and returns
    /// a [`SyncSummary`] corresponding to the local state update.
    ///
    /// Does **not** fetch private notes from the Note Transport Layer. Use
    /// [`Client::sync_state`] for the combined sync, or call [`Client::sync_note_transport`]
    /// separately.
    ///
    /// Builds the default sync input, runs [`StateSync::sync_state`] (see that method for the
    /// detailed pipeline), applies the resulting update to the store, caches the partial MMR, and
    /// prunes irrelevant blocks according to the configured cadence.
    pub async fn sync_chain(&mut self) -> Result<SyncSummary, ClientError> {
        self.ensure_genesis_in_place().await?;
        self.ensure_rpc_limits_in_place().await?;

        let mut partial_mmr = self.get_current_partial_mmr().await?;
        let update = self.fetch_chain_update(&mut partial_mmr).await?;

        self.apply_chain_update(update, partial_mmr).await
    }

    /// Fetches the on-chain state update from the node, without writing anything to the store.
    ///
    /// Split out of [`Client::sync_chain`] so it can run concurrently with
    /// [`Client::sync_note_transport`]: it takes `&self`, and `partial_mmr` is owned by the
    /// caller rather than read from the client's cache.
    ///
    /// Assumes the genesis block and the RPC limits are already in place; [`Client::sync_chain`]
    /// and [`Client::sync_state`] both ensure that before calling this.
    async fn fetch_chain_update(
        &self,
        partial_mmr: &mut PartialMmr,
    ) -> Result<StateSyncUpdate, ClientError> {
        // Each `NoteObserver` owns its own per-sync state; `with_note_observer` just attaches.
        let note_screener = self.note_screener();
        let state_sync =
            StateSync::new(self.rpc_api.clone(), Arc::new(note_screener), self.tx_discard_delta)
                .with_note_observer(Arc::new(PswapChainObserver::new(self.store.clone())));
        let input = self.build_sync_input().await?;

        // Get the sync update from the network
        let state_sync_update = state_sync.sync_state(partial_mmr, input).await?;

        // Post-sync observer hooks; run before persisting. Per-observer errors are logged, not
        // propagated.
        state_sync.run_apply_hooks(&state_sync_update).await?;

        Ok(state_sync_update)
    }

    /// Writes a fetched state update to the store, refreshes the cached partial MMR, and prunes
    /// irrelevant blocks according to the configured cadence.
    ///
    /// `partial_mmr` must be the one [`Client::fetch_chain_update`] advanced while producing
    /// `update`.
    async fn apply_chain_update(
        &mut self,
        update: StateSyncUpdate,
        partial_mmr: PartialMmr,
    ) -> Result<SyncSummary, ClientError> {
        let sync_summary: SyncSummary = (&update).into();
        debug!(sync_summary = ?sync_summary, "Sync summary computed");

        info!("Applying changes to the store.");

        // Apply received and computed updates to the store
        self.store.apply_state_sync(update).await.map_err(ClientError::StoreError)?;

        // Cache MMR so pruning can reuse in-memory MMR.
        self.cache_partial_mmr(partial_mmr).await?;

        self.maybe_untrack_and_prune_irrelevant_blocks().await?;

        Ok(sync_summary)
    }

    /// Fetches private notes from the Note Transport Layer for the tracked note tags.
    ///
    /// Returns the IDs of notes imported in this call. No-op (returns an empty vec) if note
    /// transport is disabled.
    pub async fn sync_note_transport(&self) -> Result<Vec<NoteId>, ClientError> {
        if !self.is_note_transport_enabled() {
            debug!("Note transport disabled, skipping transport sync.");
            return Ok(Vec::new());
        }

        // Drain any private notes whose previous relay attempt failed. A flush
        // error is logged, not propagated: a failing relay must not block the
        // sync, and the entries stay durable for the next attempt.
        if let Err(err) = self.flush_relay_outbox().await {
            tracing::warn!(?err, "relay outbox flush failed during sync; entries retained");
        }

        // Recover historical private notes for any tag added after the global cursor advanced.
        // This drains each newly tracked tag from the start, fetching only that tag's own history.
        let mut imported_ids = self.backfill_new_tags().await?;

        let backfilled = imported_ids.len();

        let cursor = self.store.get_note_transport_cursor().await?;
        let note_tags: Vec<_> = self.store.get_unique_note_tags().await?.into_iter().collect();
        let (ids, new_cursor) = self.fetch_transport_notes(cursor, &note_tags).await?;
        self.store.update_note_transport_cursor(new_cursor).await?;
        let streamed = ids.len();
        imported_ids.extend(ids);

        imported_ids.sort_unstable();
        imported_ids.dedup();

        debug!(
            backfilled,
            streamed,
            total = imported_ids.len(),
            tags = note_tags.len(),
            cursor_from = cursor.value(),
            cursor_to = new_cursor.value(),
            "Transport sync finished."
        );

        Ok(imported_ids)
    }

    /// Runs the full client sync.
    ///
    /// Fetches private notes from the Note Transport Layer (see
    /// [`Client::sync_note_transport`]) concurrently with the on-chain state fetch from the Miden
    /// node (see [`Client::sync_chain`]), then applies the chain update. The two halves talk to
    /// different endpoints, so overlapping them saves a round trip; they are polled on this task
    /// rather than spawned, so nothing here requires the futures to be `Send`. If note transport
    /// is disabled, this is equivalent to [`Client::sync_chain`].
    ///
    /// Fails fast on the first error.
    ///
    /// A private note delivered over NTL does not depend on this call's chain sync to learn that
    /// it was already consumed: [`Client::import_notes`] checks the nullifiers of the notes it
    /// imports. The chain sync reads its input set concurrently with that import, so a note
    /// imported here is picked up by the *next* chain sync.
    pub async fn sync_state(&mut self) -> Result<SyncSummary, ClientError> {
        // Hoisted out of the join: both are read-then-write-if-absent against shared state, and
        // `import_notes` needs the genesis block in place as well.
        self.ensure_genesis_in_place().await?;
        self.ensure_rpc_limits_in_place().await?;

        let mut partial_mmr = self.get_current_partial_mmr().await?;

        // Both halves borrow `&self`; `partial_mmr` is a local, so advancing it does not conflict.
        let (new_private_notes, update) = futures::try_join!(
            self.sync_note_transport(),
            self.fetch_chain_update(&mut partial_mmr),
        )?;

        // The store write is kept out of the join: `apply_state_sync` opens an `Immediate`
        // transaction and holds the write lock for its whole duration.
        let mut summary = self.apply_chain_update(update, partial_mmr).await?;
        summary.new_private_notes = new_private_notes;
        Ok(summary)
    }

    /// Builds a default [`StateSyncInput`] from the current client state.
    ///
    /// This includes all tracked account headers, all unique note tags, all unspent input and
    /// output notes, and all uncommitted transactions.
    pub async fn build_sync_input(&self) -> Result<StateSyncInput, ClientError> {
        let accounts = self
            .store
            .get_account_headers()
            .await?
            .into_iter()
            .map(|(header, _status)| header)
            .collect();

        let note_tags = self.store.get_unique_note_tags().await?;

        let input_notes = self.store.get_input_notes(NoteFilter::Unspent).await?;
        let output_notes = self.store.get_output_notes(NoteFilter::Unspent).await?;

        let uncommitted_transactions =
            self.store.get_transactions(TransactionFilter::Uncommitted).await?;

        Ok(StateSyncInput {
            accounts,
            note_tags,
            input_notes,
            output_notes,
            uncommitted_transactions,
        })
    }

    /// Applies the state sync update to the store and prunes irrelevant blocks according to the
    /// configured cadence.
    ///
    /// See [`crate::Store::apply_state_sync()`] for what the update implies.
    pub async fn apply_state_sync(&mut self, update: StateSyncUpdate) -> Result<(), ClientError> {
        self.store.apply_state_sync(update).await?;

        self.maybe_untrack_and_prune_irrelevant_blocks().await?;

        Ok(())
    }

    /// Prunes irrelevant blocks and their MMR authentication nodes according to the configured
    /// cadence.
    async fn maybe_untrack_and_prune_irrelevant_blocks(&mut self) -> Result<(), ClientError> {
        let Some(interval) = self.irrelevant_block_prune_interval else {
            return Ok(());
        };

        let sync_height = self.store.get_sync_height().await?;

        if let Some(last_prune_height) = self.last_irrelevant_block_prune_sync_height
            && sync_height < last_prune_height + interval
        {
            return Ok(());
        }

        self.untrack_and_prune_irrelevant_blocks().await?;
        self.last_irrelevant_block_prune_sync_height = Some(sync_height);

        Ok(())
    }

    /// Prunes irrelevant block data from the store.
    ///
    /// Identifies tracked blocks whose input notes have all been consumed, untracks them from the
    /// `PartialMmr` to determine which authentication nodes are no longer needed, then delegates
    /// to [`Store::untrack_and_prune_irrelevant_blocks`] to atomically remove the stale nodes,
    /// mark the blocks as irrelevant, and delete irrelevant block headers.
    /// Any caller of this function should've cached the `PartialMmr` beforehand.
    async fn untrack_and_prune_irrelevant_blocks(&mut self) -> Result<(), ClientError> {
        let tracked_blocks = self.store.get_tracked_block_header_numbers().await?;
        let to_untrack: Vec<usize> = if tracked_blocks.is_empty() {
            // Do not early-return: even without blocks to untrack, old irrelevant tip headers may
            // need pruning.
            Vec::new()
        } else {
            // Blocks that still have at least one unspent note need to stay tracked.
            let unspent_notes = self.store.get_input_notes(NoteFilter::Unspent).await?;
            let live_blocks: BTreeSet<usize> = unspent_notes
                .iter()
                .filter_map(|n| n.inclusion_proof().map(|p| p.location().block_num().as_usize()))
                .collect();

            tracked_blocks.difference(&live_blocks).copied().collect()
        };

        let mut blocks_to_untrack = Vec::new();
        let mut nodes_to_remove = Vec::new();
        let mut updated_partial_mmr = None;

        if !to_untrack.is_empty() {
            // Rebuild the PartialMmr and untrack each block to collect the authentication node
            // indices that are no longer needed by any remaining tracked leaf.
            let mut partial_mmr = self.get_current_partial_mmr().await?;
            nodes_to_remove = untrack_blocks(&mut partial_mmr, to_untrack.iter().copied());

            blocks_to_untrack = to_untrack
                .iter()
                .map(|&b| BlockNumber::from(u32::try_from(b).expect("block number fits in u32")))
                .collect();
            updated_partial_mmr = Some(partial_mmr);
        }

        // Store deletes stale auth nodes, marks blocks as irrelevant, and removes irrelevant
        // block headers. Old irrelevant tip headers may still need pruning.
        self.store
            .untrack_and_prune_irrelevant_blocks(&blocks_to_untrack, &nodes_to_remove)
            .await?;

        if let Some(partial_mmr) = updated_partial_mmr {
            self.cache_partial_mmr(partial_mmr).await?;
        }

        Ok(())
    }

    /// Ensures that the RPC limits are set in the RPC client. If not already cached,
    /// fetches them from the node and persists them in the store.
    pub async fn ensure_rpc_limits_in_place(&self) -> Result<(), ClientError> {
        if self.rpc_api.has_rpc_limits().is_some() {
            return Ok(());
        }

        let limits = self.rpc_api.get_rpc_limits().await?;
        self.store.set_rpc_limits(limits).await?;
        Ok(())
    }
}

// SYNC SUMMARY
// ================================================================================================

/// Contains stats about the sync operation.
#[derive(Debug, PartialEq)]
pub struct SyncSummary {
    /// Block number up to which the client has been synced.
    pub block_num: BlockNumber,
    /// IDs of new public notes that the client has received.
    pub new_public_notes: Vec<NoteId>,
    /// IDs of private notes imported from the Note Transport Layer in this sync. They are still
    /// `Expected` until observed on-chain.
    ///
    /// Only populated by [`Client::sync_state`]; [`Client::sync_chain`] always leaves this empty
    /// because it does not touch the Note Transport Layer.
    pub new_private_notes: Vec<NoteId>,
    /// IDs of tracked notes that have been committed.
    pub committed_notes: Vec<NoteId>,
    /// IDs of notes that have been consumed.
    pub consumed_notes: Vec<NoteId>,
    /// IDs of on-chain accounts that have been updated.
    pub updated_accounts: Vec<AccountId>,
    /// IDs of private accounts that have been locked.
    pub locked_accounts: Vec<AccountId>,
    /// IDs of committed transactions.
    pub committed_transactions: Vec<TransactionId>,
}

impl SyncSummary {
    pub fn new(
        block_num: BlockNumber,
        new_public_notes: Vec<NoteId>,
        new_private_notes: Vec<NoteId>,
        committed_notes: Vec<NoteId>,
        consumed_notes: Vec<NoteId>,
        updated_accounts: Vec<AccountId>,
        locked_accounts: Vec<AccountId>,
        committed_transactions: Vec<TransactionId>,
    ) -> Self {
        Self {
            block_num,
            new_public_notes,
            new_private_notes,
            committed_notes,
            consumed_notes,
            updated_accounts,
            locked_accounts,
            committed_transactions,
        }
    }

    pub fn new_empty(block_num: BlockNumber) -> Self {
        Self {
            block_num,
            new_public_notes: vec![],
            new_private_notes: vec![],
            committed_notes: vec![],
            consumed_notes: vec![],
            updated_accounts: vec![],
            locked_accounts: vec![],
            committed_transactions: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.new_public_notes.is_empty()
            && self.new_private_notes.is_empty()
            && self.committed_notes.is_empty()
            && self.consumed_notes.is_empty()
            && self.updated_accounts.is_empty()
            && self.locked_accounts.is_empty()
            && self.committed_transactions.is_empty()
    }

    pub fn combine_with(&mut self, mut other: Self) {
        self.block_num = max(self.block_num, other.block_num);
        self.new_public_notes.append(&mut other.new_public_notes);
        self.new_private_notes.append(&mut other.new_private_notes);
        self.committed_notes.append(&mut other.committed_notes);
        self.consumed_notes.append(&mut other.consumed_notes);
        self.updated_accounts.append(&mut other.updated_accounts);
        self.locked_accounts.append(&mut other.locked_accounts);
        self.committed_transactions.append(&mut other.committed_transactions);
    }
}

impl Serializable for SyncSummary {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.block_num.write_into(target);
        self.new_public_notes.write_into(target);
        self.new_private_notes.write_into(target);
        self.committed_notes.write_into(target);
        self.consumed_notes.write_into(target);
        self.updated_accounts.write_into(target);
        self.locked_accounts.write_into(target);
        self.committed_transactions.write_into(target);
    }
}

impl Deserializable for SyncSummary {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, DeserializationError> {
        let block_num = BlockNumber::read_from(source)?;
        let new_public_notes = Vec::<NoteId>::read_from(source)?;
        let new_private_notes = Vec::<NoteId>::read_from(source)?;
        let committed_notes = Vec::<NoteId>::read_from(source)?;
        let consumed_notes = Vec::<NoteId>::read_from(source)?;
        let updated_accounts = Vec::<AccountId>::read_from(source)?;
        let locked_accounts = Vec::<AccountId>::read_from(source)?;
        let committed_transactions = Vec::<TransactionId>::read_from(source)?;

        Ok(Self {
            block_num,
            new_public_notes,
            new_private_notes,
            committed_notes,
            consumed_notes,
            updated_accounts,
            locked_accounts,
            committed_transactions,
        })
    }
}
