//! [`Chain`] implements a single non-finalized blockchain,
//! starting at the finalized tip.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::{Deref, RangeInclusive},
    sync::Arc,
    time::Instant,
};

use mset::MultiSet;
use tracing::instrument;

use zebra_chain::{
    amount::{Amount, NegativeAllowed, NonNegative},
    block::{self, Height},
    fmt::humantime_milliseconds,
    history_tree::HistoryTree,
    orchard,
    parallel::tree::NoteCommitmentTrees,
    parameters::Network,
    primitives::Groth16Proof,
    sapling, sprout,
    transaction::Transaction::*,
    transaction::{self, Transaction},
    transparent,
    value_balance::ValueBalance,
    work::difficulty::PartialCumulativeWork,
};

use crate::{
    request::Treestate, service::check, ContextuallyValidBlock, HashOrHeight, OutputLocation,
    TransactionLocation, ValidateContextError,
};

use self::index::TransparentTransfers;

pub mod index;

#[derive(Debug, Clone)]
pub struct Chain {
    // The function `eq_internal_state` must be updated every time a field is added to [`Chain`].
    /// The configured network for this chain.
    network: Network,

    /// The contextually valid blocks which form this non-finalized partial chain, in height order.
    pub(crate) blocks: BTreeMap<block::Height, ContextuallyValidBlock>,

    /// An index of block heights for each block hash in `blocks`.
    pub height_by_hash: HashMap<block::Hash, block::Height>,

    /// An index of [`TransactionLocation`]s for each transaction hash in `blocks`.
    pub tx_loc_by_hash: HashMap<transaction::Hash, TransactionLocation>,

    /// The [`transparent::Utxo`]s created by `blocks`.
    ///
    /// Note that these UTXOs may not be unspent.
    /// Outputs can be spent by later transactions or blocks in the chain.
    //
    // TODO: replace OutPoint with OutputLocation?
    pub(crate) created_utxos: HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
    /// The [`transparent::OutPoint`]s spent by `blocks`,
    /// including those created by earlier transactions or blocks in the chain.
    pub(crate) spent_utxos: HashSet<transparent::OutPoint>,

    /// The Sprout note commitment tree of the tip of this [`Chain`],
    /// including all finalized notes, and the non-finalized notes in this chain.
    pub(super) sprout_note_commitment_tree: Arc<sprout::tree::NoteCommitmentTree>,
    /// The Sprout note commitment tree for each anchor.
    /// This is required for interstitial states.
    pub(crate) sprout_trees_by_anchor:
        HashMap<sprout::tree::Root, Arc<sprout::tree::NoteCommitmentTree>>,
    /// The Sprout note commitment tree for each height.
    pub(crate) sprout_trees_by_height:
        BTreeMap<block::Height, Arc<sprout::tree::NoteCommitmentTree>>,
    /// The Sapling note commitment tree of the tip of this [`Chain`],
    /// including all finalized notes, and the non-finalized notes in this chain.
    pub(super) sapling_note_commitment_tree: Arc<sapling::tree::NoteCommitmentTree>,
    /// The Sapling note commitment tree for each height.
    pub(crate) sapling_trees_by_height:
        BTreeMap<block::Height, Arc<sapling::tree::NoteCommitmentTree>>,
    /// The Orchard note commitment tree of the tip of this [`Chain`],
    /// including all finalized notes, and the non-finalized notes in this chain.
    pub(super) orchard_note_commitment_tree: Arc<orchard::tree::NoteCommitmentTree>,
    /// The Orchard note commitment tree for each height.
    pub(crate) orchard_trees_by_height:
        BTreeMap<block::Height, Arc<orchard::tree::NoteCommitmentTree>>,
    /// The ZIP-221 history tree of the tip of this [`Chain`],
    /// including all finalized blocks, and the non-finalized `blocks` in this chain.
    pub(crate) history_tree: Arc<HistoryTree>,
    pub(crate) history_trees_by_height: BTreeMap<block::Height, Arc<HistoryTree>>,

    /// The Sprout anchors created by `blocks`.
    pub(crate) sprout_anchors: MultiSet<sprout::tree::Root>,
    /// The Sprout anchors created by each block in `blocks`.
    pub(crate) sprout_anchors_by_height: BTreeMap<block::Height, sprout::tree::Root>,
    /// The Sapling anchors created by `blocks`.
    pub(crate) sapling_anchors: MultiSet<sapling::tree::Root>,
    /// The Sapling anchors created by each block in `blocks`.
    pub(crate) sapling_anchors_by_height: BTreeMap<block::Height, sapling::tree::Root>,
    /// The Orchard anchors created by `blocks`.
    pub(crate) orchard_anchors: MultiSet<orchard::tree::Root>,
    /// The Orchard anchors created by each block in `blocks`.
    pub(crate) orchard_anchors_by_height: BTreeMap<block::Height, orchard::tree::Root>,

    /// The Sprout nullifiers revealed by `blocks`.
    pub(crate) sprout_nullifiers: HashSet<sprout::Nullifier>,
    /// The Sapling nullifiers revealed by `blocks`.
    pub(crate) sapling_nullifiers: HashSet<sapling::Nullifier>,
    /// The Orchard nullifiers revealed by `blocks`.
    pub(crate) orchard_nullifiers: HashSet<orchard::Nullifier>,

    /// Partial transparent address index data from `blocks`.
    pub(super) partial_transparent_transfers: HashMap<transparent::Address, TransparentTransfers>,

    /// The cumulative work represented by `blocks`.
    ///
    /// Since the best chain is determined by the largest cumulative work,
    /// the work represented by finalized blocks can be ignored,
    /// because they are common to all non-finalized chains.
    pub(super) partial_cumulative_work: PartialCumulativeWork,

    /// The chain value pool balances of the tip of this [`Chain`],
    /// including the block value pool changes from all finalized blocks,
    /// and the non-finalized blocks in this chain.
    ///
    /// When a new chain is created from the finalized tip,
    /// it is initialized with the finalized tip chain value pool balances.
    pub(crate) chain_value_pools: ValueBalance<NonNegative>,
}

impl Chain {
    /// Create a new Chain with the given trees and network.
    pub(crate) fn new(
        network: Network,
        sprout_note_commitment_tree: Arc<sprout::tree::NoteCommitmentTree>,
        sapling_note_commitment_tree: Arc<sapling::tree::NoteCommitmentTree>,
        orchard_note_commitment_tree: Arc<orchard::tree::NoteCommitmentTree>,
        history_tree: Arc<HistoryTree>,
        finalized_tip_chain_value_pools: ValueBalance<NonNegative>,
    ) -> Self {
        Self {
            network,
            blocks: Default::default(),
            height_by_hash: Default::default(),
            tx_loc_by_hash: Default::default(),
            created_utxos: Default::default(),
            sprout_note_commitment_tree,
            sapling_note_commitment_tree,
            orchard_note_commitment_tree,
            spent_utxos: Default::default(),
            sprout_anchors: MultiSet::new(),
            sprout_anchors_by_height: Default::default(),
            sprout_trees_by_anchor: Default::default(),
            sprout_trees_by_height: Default::default(),
            sapling_anchors: MultiSet::new(),
            sapling_anchors_by_height: Default::default(),
            sapling_trees_by_height: Default::default(),
            orchard_anchors: MultiSet::new(),
            orchard_anchors_by_height: Default::default(),
            orchard_trees_by_height: Default::default(),
            sprout_nullifiers: Default::default(),
            sapling_nullifiers: Default::default(),
            orchard_nullifiers: Default::default(),
            partial_transparent_transfers: Default::default(),
            partial_cumulative_work: Default::default(),
            history_tree,
            history_trees_by_height: Default::default(),
            chain_value_pools: finalized_tip_chain_value_pools,
        }
    }

    /// Is the internal state of `self` the same as `other`?
    ///
    /// [`Chain`] has custom [`Eq`] and [`Ord`] implementations based on proof of work,
    /// which are used to select the best chain. So we can't derive [`Eq`] for [`Chain`].
    ///
    /// Unlike the custom trait impls, this method returns `true` if the entire internal state
    /// of two chains is equal.
    ///
    /// If the internal states are different, it returns `false`,
    /// even if the blocks in the two chains are equal.
    #[cfg(test)]
    pub(crate) fn eq_internal_state(&self, other: &Chain) -> bool {
        // blocks, heights, hashes
        self.blocks == other.blocks &&
            self.height_by_hash == other.height_by_hash &&
            self.tx_loc_by_hash == other.tx_loc_by_hash &&

            // transparent UTXOs
            self.created_utxos == other.created_utxos &&
            self.spent_utxos == other.spent_utxos &&

            // note commitment trees
            self.sprout_note_commitment_tree.root() == other.sprout_note_commitment_tree.root() &&
            self.sprout_trees_by_anchor == other.sprout_trees_by_anchor &&
            self.sprout_trees_by_height == other.sprout_trees_by_height &&
            self.sapling_note_commitment_tree.root() == other.sapling_note_commitment_tree.root() &&
            self.sapling_trees_by_height == other.sapling_trees_by_height &&
            self.orchard_note_commitment_tree.root() == other.orchard_note_commitment_tree.root() &&
            self.orchard_trees_by_height == other.orchard_trees_by_height &&

            // history trees
            self.history_tree == other.history_tree &&
            self.history_trees_by_height == other.history_trees_by_height &&

            // anchors
            self.sprout_anchors == other.sprout_anchors &&
            self.sprout_anchors_by_height == other.sprout_anchors_by_height &&
            self.sapling_anchors == other.sapling_anchors &&
            self.sapling_anchors_by_height == other.sapling_anchors_by_height &&
            self.orchard_anchors == other.orchard_anchors &&
            self.orchard_anchors_by_height == other.orchard_anchors_by_height &&

            // nullifiers
            self.sprout_nullifiers == other.sprout_nullifiers &&
            self.sapling_nullifiers == other.sapling_nullifiers &&
            self.orchard_nullifiers == other.orchard_nullifiers &&

            // transparent address indexes
            self.partial_transparent_transfers == other.partial_transparent_transfers &&

            // proof of work
            self.partial_cumulative_work == other.partial_cumulative_work &&

            // chain value pool balances
            self.chain_value_pools == other.chain_value_pools
    }

    /// Push a contextually valid non-finalized block into this chain as the new tip.
    ///
    /// If the block is invalid, drops this chain, and returns an error.
    ///
    /// Note: a [`ContextuallyValidBlock`] isn't actually contextually valid until
    /// [`Self::update_chain_tip_with`] returns success.
    #[instrument(level = "debug", skip(self, block), fields(block = %block.block))]
    pub fn push(mut self, block: ContextuallyValidBlock) -> Result<Chain, ValidateContextError> {
        // update cumulative data members
        self.update_chain_tip_with(&block)?;

        tracing::debug!(block = %block.block, "adding block to chain");
        self.blocks.insert(block.height, block);

        Ok(self)
    }

    /// Pops the lowest height block of the non-finalized portion of a chain,
    /// and returns it with its associated treestate.
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn pop_root(&mut self) -> (ContextuallyValidBlock, Treestate) {
        // Obtain the lowest height.
        let block_height = self.non_finalized_root_height();

        // Obtain the treestate associated with the block being finalized.
        let treestate = self
            .treestate(block_height.into())
            .expect("The treestate must be present for the root height.");

        // Remove the lowest height block from `self.blocks`.
        let block = self
            .blocks
            .remove(&block_height)
            .expect("only called while blocks is populated");

        // Update cumulative data members.
        self.revert_chain_with(&block, RevertPosition::Root);

        (block, treestate)
    }

    /// Returns the height of the chain root.
    pub fn non_finalized_root_height(&self) -> block::Height {
        self.blocks
            .keys()
            .next()
            .cloned()
            .expect("only called while blocks is populated")
    }

    /// Fork a chain at the block with the given hash, if it is part of this
    /// chain.
    ///
    /// The passed trees must match the trees of the finalized tip. They are
    /// extended by the commitments from the newly forked chain up to the passed
    /// `fork_tip`.
    #[allow(clippy::unwrap_in_result)]
    pub fn fork(
        &self,
        fork_tip: block::Hash,
        sprout_note_commitment_tree: Arc<sprout::tree::NoteCommitmentTree>,
        sapling_note_commitment_tree: Arc<sapling::tree::NoteCommitmentTree>,
        orchard_note_commitment_tree: Arc<orchard::tree::NoteCommitmentTree>,
        history_tree: Arc<HistoryTree>,
    ) -> Result<Option<Self>, ValidateContextError> {
        if !self.height_by_hash.contains_key(&fork_tip) {
            return Ok(None);
        }

        let mut forked = self.with_trees(
            sprout_note_commitment_tree,
            sapling_note_commitment_tree,
            orchard_note_commitment_tree,
            history_tree,
        );

        // Revert blocks above the fork
        while forked.non_finalized_tip_hash() != fork_tip {
            forked.pop_tip();
        }

        // Rebuild trees from the finalized tip, because we haven't implemented reverts yet.
        forked.rebuild_trees_parallel()?;

        Ok(Some(forked))
    }

    /// Rebuild the note commitment and history trees after a chain fork,
    /// starting from the finalized tip trees, using parallel `rayon` threads.
    ///
    /// Note commitments and history trees are not removed from the Chain during a fork,
    /// because we don't support that operation yet. Instead, we recreate the tree
    /// from the finalized tip.
    ///
    /// TODO: remove trees and anchors above the fork, to save CPU time (#4794)
    #[allow(clippy::unwrap_in_result)]
    pub fn rebuild_trees_parallel(&mut self) -> Result<(), ValidateContextError> {
        let start_time = Instant::now();
        let rebuilt_block_count = self.blocks.len();
        let fork_height = self.non_finalized_tip_height();
        let fork_tip = self.non_finalized_tip_hash();

        info!(
            ?rebuilt_block_count,
            ?fork_height,
            ?fork_tip,
            "starting to rebuild note commitment trees after a non-finalized chain fork",
        );

        // Prepare data for parallel execution
        let block_list = self
            .blocks
            .iter()
            .map(|(height, block)| (*height, block.block.clone()))
            .collect();

        // TODO: use NoteCommitmentTrees to store the trees as well?
        let mut nct = NoteCommitmentTrees {
            sprout: self.sprout_note_commitment_tree.clone(),
            sapling: self.sapling_note_commitment_tree.clone(),
            orchard: self.orchard_note_commitment_tree.clone(),
        };

        let mut note_result = None;
        let mut history_result = None;

        // Run 4 tasks in parallel:
        // - sprout, sapling, and orchard tree updates and (redundant) root calculations
        // - history tree updates
        rayon::in_place_scope_fifo(|scope| {
            // Spawns a separate rayon task for each note commitment tree.
            //
            // TODO: skip the unused root calculations? (redundant after #4794)
            note_result = Some(nct.update_trees_parallel_list(block_list));

            scope.spawn_fifo(|_scope| {
                history_result = Some(self.rebuild_history_tree());
            });
        });

        note_result.expect("scope has already finished")?;
        history_result.expect("scope has already finished")?;

        // Update the note commitment trees in the chain.
        self.sprout_note_commitment_tree = nct.sprout;
        self.sapling_note_commitment_tree = nct.sapling;
        self.orchard_note_commitment_tree = nct.orchard;

        let rebuild_time = start_time.elapsed();
        let rebuild_time_per_block =
            rebuild_time / rebuilt_block_count.try_into().expect("fits in u32");
        info!(
            rebuild_time = ?humantime_milliseconds(rebuild_time),
            rebuild_time_per_block = ?humantime_milliseconds(rebuild_time_per_block),
            ?rebuilt_block_count,
            ?fork_height,
            ?fork_tip,
            "finished rebuilding note commitment trees after a non-finalized chain fork",
        );

        Ok(())
    }

    /// Rebuild the history tree after a chain fork.
    ///
    /// TODO: remove trees and anchors above the fork, to save CPU time (#4794)
    #[allow(clippy::unwrap_in_result)]
    pub fn rebuild_history_tree(&mut self) -> Result<(), ValidateContextError> {
        for block in self.blocks.values() {
            // Note that anchors don't need to be recreated since they are already
            // handled in revert_chain_state_with.
            let sapling_root = self
                .sapling_anchors_by_height
                .get(&block.height)
                .expect("Sapling anchors must exist for pre-fork blocks");

            let orchard_root = self
                .orchard_anchors_by_height
                .get(&block.height)
                .expect("Orchard anchors must exist for pre-fork blocks");

            let history_tree_mut = Arc::make_mut(&mut self.history_tree);
            history_tree_mut
                .push(
                    self.network,
                    block.block.clone(),
                    *sapling_root,
                    *orchard_root,
                )
                .map_err(Arc::new)?;
        }

        Ok(())
    }

    /// Returns the [`Network`] for this chain.
    pub fn network(&self) -> Network {
        self.network
    }

    /// Returns the [`ContextuallyValidBlock`] with [`block::Hash`] or
    /// [`Height`](zebra_chain::block::Height), if it exists in this chain.
    pub fn block(&self, hash_or_height: HashOrHeight) -> Option<&ContextuallyValidBlock> {
        let height =
            hash_or_height.height_or_else(|hash| self.height_by_hash.get(&hash).cloned())?;

        self.blocks.get(&height)
    }

    /// Returns the [`Transaction`] with [`transaction::Hash`], if it exists in this chain.
    pub fn transaction(
        &self,
        hash: transaction::Hash,
    ) -> Option<(&Arc<Transaction>, block::Height)> {
        self.tx_loc_by_hash.get(&hash).map(|tx_loc| {
            (
                &self.blocks[&tx_loc.height].block.transactions[tx_loc.index.as_usize()],
                tx_loc.height,
            )
        })
    }

    /// Returns the [`Transaction`] at [`TransactionLocation`], if it exists in this chain.
    #[allow(dead_code)]
    pub fn transaction_by_loc(&self, tx_loc: TransactionLocation) -> Option<&Arc<Transaction>> {
        self.blocks
            .get(&tx_loc.height)?
            .block
            .transactions
            .get(tx_loc.index.as_usize())
    }

    /// Returns the [`transaction::Hash`] for the transaction at [`TransactionLocation`],
    /// if it exists in this chain.
    #[allow(dead_code)]
    pub fn transaction_hash_by_loc(
        &self,
        tx_loc: TransactionLocation,
    ) -> Option<&transaction::Hash> {
        self.blocks
            .get(&tx_loc.height)?
            .transaction_hashes
            .get(tx_loc.index.as_usize())
    }

    /// Returns the [`transaction::Hash`]es in the block with `hash_or_height`,
    /// if it exists in this chain.
    ///
    /// Hashes are returned in block order.
    ///
    /// Returns `None` if the block is not found.
    pub fn transaction_hashes_for_block(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<[transaction::Hash]>> {
        let transaction_hashes = self.block(hash_or_height)?.transaction_hashes.clone();

        Some(transaction_hashes)
    }

    /// Returns the [`block::Hash`] for `height`, if it exists in this chain.
    pub fn hash_by_height(&self, height: Height) -> Option<block::Hash> {
        let hash = self.blocks.get(&height)?.hash;

        Some(hash)
    }

    /// Returns the [`Height`] for `hash`, if it exists in this chain.
    pub fn height_by_hash(&self, hash: block::Hash) -> Option<Height> {
        self.height_by_hash.get(&hash).cloned()
    }

    /// Returns the non-finalized tip block height and hash.
    pub fn non_finalized_tip(&self) -> (Height, block::Hash) {
        (
            self.non_finalized_tip_height(),
            self.non_finalized_tip_hash(),
        )
    }

    /// Returns the Sprout
    /// [`NoteCommitmentTree`](sprout::tree::NoteCommitmentTree) specified by a
    /// [`HashOrHeight`], if it exists in the non-finalized [`Chain`].
    pub fn sprout_tree(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<sprout::tree::NoteCommitmentTree>> {
        let height =
            hash_or_height.height_or_else(|hash| self.height_by_hash.get(&hash).cloned())?;

        self.sprout_trees_by_height.get(&height).cloned()
    }

    /// Returns the Sapling
    /// [`NoteCommitmentTree`](sapling::tree::NoteCommitmentTree) specified by a
    /// [`HashOrHeight`], if it exists in the non-finalized [`Chain`].
    pub fn sapling_tree(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<sapling::tree::NoteCommitmentTree>> {
        let height =
            hash_or_height.height_or_else(|hash| self.height_by_hash.get(&hash).cloned())?;

        self.sapling_trees_by_height.get(&height).cloned()
    }

    /// Returns the Orchard
    /// [`NoteCommitmentTree`](orchard::tree::NoteCommitmentTree) specified by a
    /// [`HashOrHeight`], if it exists in the non-finalized [`Chain`].
    pub fn orchard_tree(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<orchard::tree::NoteCommitmentTree>> {
        let height =
            hash_or_height.height_or_else(|hash| self.height_by_hash.get(&hash).cloned())?;

        self.orchard_trees_by_height.get(&height).cloned()
    }

    /// Returns the [`HistoryTree`] specified by a [`HashOrHeight`], if it
    /// exists in the non-finalized [`Chain`].
    pub fn history_tree(&self, hash_or_height: HashOrHeight) -> Option<Arc<HistoryTree>> {
        let height =
            hash_or_height.height_or_else(|hash| self.height_by_hash.get(&hash).cloned())?;

        self.history_trees_by_height.get(&height).cloned()
    }

    fn treestate(&self, hash_or_height: HashOrHeight) -> Option<Treestate> {
        let sprout_tree = self.sprout_tree(hash_or_height)?;
        let sapling_tree = self.sapling_tree(hash_or_height)?;
        let orchard_tree = self.orchard_tree(hash_or_height)?;
        let history_tree = self.history_tree(hash_or_height)?;

        Some(Treestate::new(
            sprout_tree,
            sapling_tree,
            orchard_tree,
            history_tree,
        ))
    }

    /// Returns the block hash of the tip block.
    pub fn non_finalized_tip_hash(&self) -> block::Hash {
        self.blocks
            .values()
            .next_back()
            .expect("only called while blocks is populated")
            .hash
    }

    /// Returns the non-finalized root block hash and height.
    #[allow(dead_code)]
    pub fn non_finalized_root(&self) -> (block::Hash, block::Height) {
        (
            self.non_finalized_root_hash(),
            self.non_finalized_root_height(),
        )
    }

    /// Returns the block hash of the non-finalized root block.
    pub fn non_finalized_root_hash(&self) -> block::Hash {
        self.blocks
            .values()
            .next()
            .expect("only called while blocks is populated")
            .hash
    }

    /// Returns the block hash of the `n`th block from the non-finalized root.
    ///
    /// This is the block at `non_finalized_root_height() + n`.
    #[allow(dead_code)]
    pub fn non_finalized_nth_hash(&self, n: usize) -> Option<block::Hash> {
        self.blocks.values().nth(n).map(|block| block.hash)
    }

    /// Remove the highest height block of the non-finalized portion of a chain.
    fn pop_tip(&mut self) {
        let block_height = self.non_finalized_tip_height();

        let block = self
            .blocks
            .remove(&block_height)
            .expect("only called while blocks is populated");

        assert!(
            !self.blocks.is_empty(),
            "Non-finalized chains must have at least one block to be valid"
        );

        self.revert_chain_with(&block, RevertPosition::Tip);
    }

    /// Return the non-finalized tip height for this chain.
    ///
    /// # Panics
    ///
    /// Panics if called while the chain is empty,
    /// or while the chain is updating its internal state with the first block.
    pub fn non_finalized_tip_height(&self) -> block::Height {
        self.max_block_height()
            .expect("only called while blocks is populated")
    }

    /// Return the non-finalized tip height for this chain,
    /// or `None` if `self.blocks` is empty.
    fn max_block_height(&self) -> Option<block::Height> {
        self.blocks.keys().next_back().cloned()
    }

    /// Return the non-finalized tip block for this chain,
    /// or `None` if `self.blocks` is empty.
    pub fn tip_block(&self) -> Option<&ContextuallyValidBlock> {
        self.blocks.values().next_back()
    }

    /// Returns true if the non-finalized part of this chain is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Returns the non-finalized length of this chain.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns the unspent transaction outputs (UTXOs) in this non-finalized chain.
    ///
    /// Callers should also check the finalized state for available UTXOs.
    /// If UTXOs remain unspent when a block is finalized, they are stored in the finalized state,
    /// and removed from the relevant chain(s).
    pub fn unspent_utxos(&self) -> HashMap<transparent::OutPoint, transparent::OrderedUtxo> {
        let mut unspent_utxos = self.created_utxos.clone();
        unspent_utxos.retain(|outpoint, _utxo| !self.spent_utxos.contains(outpoint));

        unspent_utxos
    }

    /// Returns the [`transparent::Utxo`] pointed to by the given
    /// [`transparent::OutPoint`] if it was created by this chain.
    ///
    /// UTXOs are returned regardless of whether they have been spent.
    pub fn created_utxo(&self, outpoint: &transparent::OutPoint) -> Option<transparent::Utxo> {
        if let Some(utxo) = self.created_utxos.get(outpoint) {
            return Some(utxo.utxo.clone());
        }

        None
    }

    // Address index queries

    /// Returns the transparent transfers for `addresses` in this non-finalized chain.
    ///
    /// If none of the addresses have an address index, returns an empty iterator.
    ///
    /// # Correctness
    ///
    /// Callers should apply the returned indexes to the corresponding finalized state indexes.
    ///
    /// The combined result will only be correct if the chains match.
    /// The exact type of match varies by query.
    pub fn partial_transparent_indexes<'a>(
        &'a self,
        addresses: &'a HashSet<transparent::Address>,
    ) -> impl Iterator<Item = &TransparentTransfers> {
        addresses
            .iter()
            .copied()
            .flat_map(|address| self.partial_transparent_transfers.get(&address))
    }

    /// Returns the transparent balance change for `addresses` in this non-finalized chain.
    ///
    /// If the balance doesn't change for any of the addresses, returns zero.
    ///
    /// # Correctness
    ///
    /// Callers should apply this balance change to the finalized state balance for `addresses`.
    ///
    /// The total balance will only be correct if this partial chain matches the finalized state.
    /// Specifically, the root of this partial chain must be a child block of the finalized tip.
    pub fn partial_transparent_balance_change(
        &self,
        addresses: &HashSet<transparent::Address>,
    ) -> Amount<NegativeAllowed> {
        let balance_change: Result<Amount<NegativeAllowed>, _> = self
            .partial_transparent_indexes(addresses)
            .map(|transfers| transfers.balance())
            .sum();

        balance_change.expect(
            "unexpected amount overflow: value balances are valid, so partial sum should be valid",
        )
    }

    /// Returns the transparent UTXO changes for `addresses` in this non-finalized chain.
    ///
    /// If the UTXOs don't change for any of the addresses, returns empty lists.
    ///
    /// # Correctness
    ///
    /// Callers should apply these non-finalized UTXO changes to the finalized state UTXOs.
    ///
    /// The UTXOs will only be correct if the non-finalized chain matches or overlaps with
    /// the finalized state.
    ///
    /// Specifically, a block in the partial chain must be a child block of the finalized tip.
    /// (But the child block does not have to be the partial chain root.)
    pub fn partial_transparent_utxo_changes(
        &self,
        addresses: &HashSet<transparent::Address>,
    ) -> (
        BTreeMap<OutputLocation, transparent::Output>,
        BTreeSet<OutputLocation>,
    ) {
        let created_utxos = self
            .partial_transparent_indexes(addresses)
            .flat_map(|transfers| transfers.created_utxos())
            .map(|(out_loc, output)| (*out_loc, output.clone()))
            .collect();

        let spent_utxos = self
            .partial_transparent_indexes(addresses)
            .flat_map(|transfers| transfers.spent_utxos())
            .cloned()
            .collect();

        (created_utxos, spent_utxos)
    }

    /// Returns the [`transaction::Hash`]es used by `addresses` to receive or spend funds,
    /// in the non-finalized chain, filtered using the `query_height_range`.
    ///
    /// If none of the addresses receive or spend funds in this partial chain, returns an empty list.
    ///
    /// # Correctness
    ///
    /// Callers should combine these non-finalized transactions with the finalized state transactions.
    ///
    /// The transaction IDs will only be correct if the non-finalized chain matches or overlaps with
    /// the finalized state.
    ///
    /// Specifically, a block in the partial chain must be a child block of the finalized tip.
    /// (But the child block does not have to be the partial chain root.)
    ///
    /// This condition does not apply if there is only one address.
    /// Since address transactions are only appended by blocks,
    /// and the finalized state query reads them in order,
    /// it is impossible to get inconsistent transactions for a single address.
    pub fn partial_transparent_tx_ids(
        &self,
        addresses: &HashSet<transparent::Address>,
        query_height_range: RangeInclusive<Height>,
    ) -> BTreeMap<TransactionLocation, transaction::Hash> {
        self.partial_transparent_indexes(addresses)
            .flat_map(|transfers| {
                transfers.tx_ids(&self.tx_loc_by_hash, query_height_range.clone())
            })
            .collect()
    }

    // Cloning

    /// Clone the Chain but not the history and note commitment trees, using
    /// the specified trees instead.
    ///
    /// Useful when forking, where the trees are rebuilt anyway.
    fn with_trees(
        &self,
        sprout_note_commitment_tree: Arc<sprout::tree::NoteCommitmentTree>,
        sapling_note_commitment_tree: Arc<sapling::tree::NoteCommitmentTree>,
        orchard_note_commitment_tree: Arc<orchard::tree::NoteCommitmentTree>,
        history_tree: Arc<HistoryTree>,
    ) -> Self {
        Chain {
            network: self.network,
            blocks: self.blocks.clone(),
            height_by_hash: self.height_by_hash.clone(),
            tx_loc_by_hash: self.tx_loc_by_hash.clone(),
            created_utxos: self.created_utxos.clone(),
            spent_utxos: self.spent_utxos.clone(),
            sprout_note_commitment_tree,
            sprout_trees_by_anchor: self.sprout_trees_by_anchor.clone(),
            sprout_trees_by_height: self.sprout_trees_by_height.clone(),
            sapling_note_commitment_tree,
            sapling_trees_by_height: self.sapling_trees_by_height.clone(),
            orchard_note_commitment_tree,
            orchard_trees_by_height: self.orchard_trees_by_height.clone(),
            sprout_anchors: self.sprout_anchors.clone(),
            sapling_anchors: self.sapling_anchors.clone(),
            orchard_anchors: self.orchard_anchors.clone(),
            sprout_anchors_by_height: self.sprout_anchors_by_height.clone(),
            sapling_anchors_by_height: self.sapling_anchors_by_height.clone(),
            orchard_anchors_by_height: self.orchard_anchors_by_height.clone(),
            sprout_nullifiers: self.sprout_nullifiers.clone(),
            sapling_nullifiers: self.sapling_nullifiers.clone(),
            orchard_nullifiers: self.orchard_nullifiers.clone(),
            partial_transparent_transfers: self.partial_transparent_transfers.clone(),
            partial_cumulative_work: self.partial_cumulative_work,
            history_tree,
            history_trees_by_height: self.history_trees_by_height.clone(),
            chain_value_pools: self.chain_value_pools,
        }
    }

    /// Update the chain tip with the `contextually_valid` block,
    /// running note commitment tree updates in parallel with other updates.
    ///
    /// Used to implement `update_chain_tip_with::<ContextuallyValidBlock>`.
    #[instrument(skip(self, contextually_valid), fields(block = %contextually_valid.block))]
    #[allow(clippy::unwrap_in_result)]
    fn update_chain_tip_with_block_parallel(
        &mut self,
        contextually_valid: &ContextuallyValidBlock,
    ) -> Result<(), ValidateContextError> {
        let height = contextually_valid.height;

        // Prepare data for parallel execution
        //
        // TODO: use NoteCommitmentTrees to store the trees as well?
        let mut nct = NoteCommitmentTrees {
            sprout: self.sprout_note_commitment_tree.clone(),
            sapling: self.sapling_note_commitment_tree.clone(),
            orchard: self.orchard_note_commitment_tree.clone(),
        };

        let mut tree_result = None;
        let mut partial_result = None;

        // Run 4 tasks in parallel:
        // - sprout, sapling, and orchard tree updates and root calculations
        // - the rest of the Chain updates
        rayon::in_place_scope_fifo(|scope| {
            // Spawns a separate rayon task for each note commitment tree
            tree_result = Some(nct.update_trees_parallel(&contextually_valid.block.clone()));

            scope.spawn_fifo(|_scope| {
                partial_result =
                    Some(self.update_chain_tip_with_block_except_trees(contextually_valid));
            });
        });

        tree_result.expect("scope has already finished")?;
        partial_result.expect("scope has already finished")?;

        // Update the note commitment trees in the chain.
        self.sprout_note_commitment_tree = nct.sprout;
        self.sapling_note_commitment_tree = nct.sapling;
        self.orchard_note_commitment_tree = nct.orchard;

        // Do the Chain updates with data dependencies on note commitment tree updates

        // Update the note commitment trees indexed by height.
        self.sprout_trees_by_height
            .insert(height, self.sprout_note_commitment_tree.clone());
        self.sapling_trees_by_height
            .insert(height, self.sapling_note_commitment_tree.clone());
        self.orchard_trees_by_height
            .insert(height, self.orchard_note_commitment_tree.clone());

        // Having updated all the note commitment trees and nullifier sets in
        // this block, the roots of the note commitment trees as of the last
        // transaction are the treestates of this block.
        //
        // Use the previously cached roots, which were calculated in parallel.
        let sprout_root = self.sprout_note_commitment_tree.root();
        self.sprout_anchors.insert(sprout_root);
        self.sprout_anchors_by_height.insert(height, sprout_root);
        self.sprout_trees_by_anchor
            .insert(sprout_root, self.sprout_note_commitment_tree.clone());
        let sapling_root = self.sapling_note_commitment_tree.root();
        self.sapling_anchors.insert(sapling_root);
        self.sapling_anchors_by_height.insert(height, sapling_root);

        let orchard_root = self.orchard_note_commitment_tree.root();
        self.orchard_anchors.insert(orchard_root);
        self.orchard_anchors_by_height.insert(height, orchard_root);

        // TODO: update the history trees in a rayon thread, if they show up in CPU profiles
        let history_tree_mut = Arc::make_mut(&mut self.history_tree);
        history_tree_mut
            .push(
                self.network,
                contextually_valid.block.clone(),
                sapling_root,
                orchard_root,
            )
            .map_err(Arc::new)?;

        self.history_trees_by_height
            .insert(height, self.history_tree.clone());

        Ok(())
    }

    /// Update the chain tip with the `contextually_valid` block,
    /// except for the note commitment and history tree updates.
    ///
    /// Used to implement `update_chain_tip_with::<ContextuallyValidBlock>`.
    #[instrument(skip(self, contextually_valid), fields(block = %contextually_valid.block))]
    #[allow(clippy::unwrap_in_result)]
    fn update_chain_tip_with_block_except_trees(
        &mut self,
        contextually_valid: &ContextuallyValidBlock,
    ) -> Result<(), ValidateContextError> {
        let (
            block,
            hash,
            height,
            new_outputs,
            spent_outputs,
            transaction_hashes,
            chain_value_pool_change,
        ) = (
            contextually_valid.block.as_ref(),
            contextually_valid.hash,
            contextually_valid.height,
            &contextually_valid.new_outputs,
            &contextually_valid.spent_outputs,
            &contextually_valid.transaction_hashes,
            &contextually_valid.chain_value_pool_change,
        );

        // add hash to height_by_hash
        let prior_height = self.height_by_hash.insert(hash, height);
        assert!(
            prior_height.is_none(),
            "block heights must be unique within a single chain"
        );

        // add work to partial cumulative work
        let block_work = block
            .header
            .difficulty_threshold
            .to_work()
            .expect("work has already been validated");
        self.partial_cumulative_work += block_work;

        // for each transaction in block
        for (transaction_index, (transaction, transaction_hash)) in block
            .transactions
            .iter()
            .zip(transaction_hashes.iter().cloned())
            .enumerate()
        {
            let (
                inputs,
                outputs,
                joinsplit_data,
                sapling_shielded_data_per_spend_anchor,
                sapling_shielded_data_shared_anchor,
                orchard_shielded_data,
            ) = match transaction.deref() {
                V4 {
                    inputs,
                    outputs,
                    joinsplit_data,
                    sapling_shielded_data,
                    ..
                } => (inputs, outputs, joinsplit_data, sapling_shielded_data, &None, &None),
                V5 {
                    inputs,
                    outputs,
                    sapling_shielded_data,
                    orchard_shielded_data,
                    ..
                } => (
                    inputs,
                    outputs,
                    &None,
                    &None,
                    sapling_shielded_data,
                    orchard_shielded_data,
                ),
                V1 { .. } | V2 { .. } | V3 { .. } => unreachable!(
                    "older transaction versions only exist in finalized blocks, because of the mandatory canopy checkpoint",
                ),
            };

            // add key `transaction.hash` and value `(height, tx_index)` to `tx_loc_by_hash`
            let transaction_location = TransactionLocation::from_usize(height, transaction_index);
            let prior_pair = self
                .tx_loc_by_hash
                .insert(transaction_hash, transaction_location);
            assert_eq!(
                prior_pair, None,
                "transactions must be unique within a single chain"
            );

            // add the utxos this produced
            self.update_chain_tip_with(&(outputs, &transaction_hash, new_outputs))?;
            // delete the utxos this consumed
            self.update_chain_tip_with(&(inputs, &transaction_hash, spent_outputs))?;

            // add the shielded data
            self.update_chain_tip_with(joinsplit_data)?;
            self.update_chain_tip_with(sapling_shielded_data_per_spend_anchor)?;
            self.update_chain_tip_with(sapling_shielded_data_shared_anchor)?;
            self.update_chain_tip_with(orchard_shielded_data)?;
        }

        // update the chain value pool balances
        self.update_chain_tip_with(chain_value_pool_change)?;

        Ok(())
    }
}

/// The revert position being performed on a chain.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum RevertPosition {
    /// The chain root is being reverted via [`Chain::pop_root`], when a block
    /// is finalized.
    Root,

    /// The chain tip is being reverted via [`Chain::pop_tip`],
    /// when a chain is forked.
    Tip,
}

/// Helper trait to organize inverse operations done on the [`Chain`] type.
///
/// Used to overload update and revert methods, based on the type of the argument,
/// and the position of the removed block in the chain.
///
/// This trait was motivated by the length of the `push`, [`Chain::pop_root`],
/// and [`Chain::pop_tip`] functions, and fear that it would be easy to
/// introduce bugs when updating them, unless the code was reorganized to keep
/// related operations adjacent to each other.
trait UpdateWith<T> {
    /// When `T` is added to the chain tip,
    /// update [`Chain`] cumulative data members to add data that are derived from `T`.
    fn update_chain_tip_with(&mut self, _: &T) -> Result<(), ValidateContextError>;

    /// When `T` is removed from `position` in the chain,
    /// revert [`Chain`] cumulative data members to remove data that are derived from `T`.
    fn revert_chain_with(&mut self, _: &T, position: RevertPosition);
}

impl UpdateWith<ContextuallyValidBlock> for Chain {
    #[instrument(skip(self, contextually_valid), fields(block = %contextually_valid.block))]
    #[allow(clippy::unwrap_in_result)]
    fn update_chain_tip_with(
        &mut self,
        contextually_valid: &ContextuallyValidBlock,
    ) -> Result<(), ValidateContextError> {
        self.update_chain_tip_with_block_parallel(contextually_valid)
    }

    #[instrument(skip(self, contextually_valid), fields(block = %contextually_valid.block))]
    fn revert_chain_with(
        &mut self,
        contextually_valid: &ContextuallyValidBlock,
        position: RevertPosition,
    ) {
        let (
            block,
            hash,
            height,
            new_outputs,
            spent_outputs,
            transaction_hashes,
            chain_value_pool_change,
        ) = (
            contextually_valid.block.as_ref(),
            contextually_valid.hash,
            contextually_valid.height,
            &contextually_valid.new_outputs,
            &contextually_valid.spent_outputs,
            &contextually_valid.transaction_hashes,
            &contextually_valid.chain_value_pool_change,
        );

        // remove the blocks hash from `height_by_hash`
        assert!(
            self.height_by_hash.remove(&hash).is_some(),
            "hash must be present if block was added to chain"
        );

        // remove work from partial_cumulative_work
        let block_work = block
            .header
            .difficulty_threshold
            .to_work()
            .expect("work has already been validated");
        self.partial_cumulative_work -= block_work;

        // Note: the history tree is not modified in this method.
        // This method is called on two scenarios:
        // - When popping the root: the history tree does not change.
        // - When popping the tip: the history tree is rebuilt in fork().
        //
        // However, `history_trees_by_height` is reverted.
        self.history_trees_by_height
            .remove(&height)
            .expect("History tree must be present if block was added to chain");

        // for each transaction in block
        for (transaction, transaction_hash) in
            block.transactions.iter().zip(transaction_hashes.iter())
        {
            let (
                inputs,
                outputs,
                joinsplit_data,
                sapling_shielded_data_per_spend_anchor,
                sapling_shielded_data_shared_anchor,
                orchard_shielded_data,
            ) = match transaction.deref() {
                V4 {
                    inputs,
                    outputs,
                    joinsplit_data,
                    sapling_shielded_data,
                    ..
                } => (inputs, outputs, joinsplit_data, sapling_shielded_data, &None, &None),
                V5 {
                    inputs,
                    outputs,
                    sapling_shielded_data,
                    orchard_shielded_data,
                    ..
                } => (
                    inputs,
                    outputs,
                    &None,
                    &None,
                    sapling_shielded_data,
                    orchard_shielded_data,
                ),
                V1 { .. } | V2 { .. } | V3 { .. } => unreachable!(
                    "older transaction versions only exist in finalized blocks, because of the mandatory canopy checkpoint",
                ),
            };

            // remove the utxos this produced
            self.revert_chain_with(&(outputs, transaction_hash, new_outputs), position);
            // reset the utxos this consumed
            self.revert_chain_with(&(inputs, transaction_hash, spent_outputs), position);

            // remove `transaction.hash` from `tx_loc_by_hash`
            assert!(
                self.tx_loc_by_hash.remove(transaction_hash).is_some(),
                "transactions must be present if block was added to chain"
            );

            // remove the shielded data
            self.revert_chain_with(joinsplit_data, position);
            self.revert_chain_with(sapling_shielded_data_per_spend_anchor, position);
            self.revert_chain_with(sapling_shielded_data_shared_anchor, position);
            self.revert_chain_with(orchard_shielded_data, position);
        }

        let anchor = self
            .sprout_anchors_by_height
            .remove(&height)
            .expect("Sprout anchor must be present if block was added to chain");
        assert!(
            self.sprout_anchors.remove(&anchor),
            "Sprout anchor must be present if block was added to chain"
        );
        if !self.sprout_anchors.contains(&anchor) {
            self.sprout_trees_by_anchor.remove(&anchor);
        }
        self.sprout_trees_by_height
            .remove(&height)
            .expect("Sprout note commitment tree must be present if block was added to chain");

        let anchor = self
            .sapling_anchors_by_height
            .remove(&height)
            .expect("Sapling anchor must be present if block was added to chain");
        assert!(
            self.sapling_anchors.remove(&anchor),
            "Sapling anchor must be present if block was added to chain"
        );
        self.sapling_trees_by_height
            .remove(&height)
            .expect("Sapling note commitment tree must be present if block was added to chain");

        let anchor = self
            .orchard_anchors_by_height
            .remove(&height)
            .expect("Orchard anchor must be present if block was added to chain");
        assert!(
            self.orchard_anchors.remove(&anchor),
            "Orchard anchor must be present if block was added to chain"
        );
        self.orchard_trees_by_height
            .remove(&height)
            .expect("Orchard note commitment tree must be present if block was added to chain");

        // revert the chain value pool balances, if needed
        self.revert_chain_with(chain_value_pool_change, position);
    }
}

// Created UTXOs
//
// TODO: replace arguments with a struct
impl
    UpdateWith<(
        // The outputs from a transaction in this block
        &Vec<transparent::Output>,
        // The hash of the transaction that the outputs are from
        &transaction::Hash,
        // The UTXOs for all outputs created by this transaction (or block)
        &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
    )> for Chain
{
    #[allow(clippy::unwrap_in_result)]
    fn update_chain_tip_with(
        &mut self,
        &(created_outputs, creating_tx_hash, block_created_outputs): &(
            &Vec<transparent::Output>,
            &transaction::Hash,
            &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
        ),
    ) -> Result<(), ValidateContextError> {
        for output_index in 0..created_outputs.len() {
            let outpoint = transparent::OutPoint {
                hash: *creating_tx_hash,
                index: output_index.try_into().expect("valid indexes fit in u32"),
            };
            let created_utxo = block_created_outputs
                .get(&outpoint)
                .expect("new_outputs contains all created UTXOs");

            // Update the chain's created UTXOs
            let previous_entry = self.created_utxos.insert(outpoint, created_utxo.clone());
            assert_eq!(
                previous_entry, None,
                "unexpected created output: duplicate update or duplicate UTXO",
            );

            // Update the address index with this UTXO
            if let Some(receiving_address) = created_utxo.utxo.output.address(self.network) {
                let address_transfers = self
                    .partial_transparent_transfers
                    .entry(receiving_address)
                    .or_default();

                address_transfers.update_chain_tip_with(&(&outpoint, created_utxo))?;
            }
        }

        Ok(())
    }

    fn revert_chain_with(
        &mut self,
        &(created_outputs, creating_tx_hash, block_created_outputs): &(
            &Vec<transparent::Output>,
            &transaction::Hash,
            &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
        ),
        position: RevertPosition,
    ) {
        for output_index in 0..created_outputs.len() {
            let outpoint = transparent::OutPoint {
                hash: *creating_tx_hash,
                index: output_index.try_into().expect("valid indexes fit in u32"),
            };
            let created_utxo = block_created_outputs
                .get(&outpoint)
                .expect("new_outputs contains all created UTXOs");

            // Revert the chain's created UTXOs
            let removed_entry = self.created_utxos.remove(&outpoint);
            assert!(
                removed_entry.is_some(),
                "unexpected revert of created output: duplicate revert or duplicate UTXO",
            );

            // Revert the address index for this UTXO
            if let Some(receiving_address) = created_utxo.utxo.output.address(self.network) {
                let address_transfers = self
                    .partial_transparent_transfers
                    .get_mut(&receiving_address)
                    .expect("block has previously been applied to the chain");

                address_transfers.revert_chain_with(&(&outpoint, created_utxo), position);

                // Remove this transfer if it is now empty
                if address_transfers.is_empty() {
                    self.partial_transparent_transfers
                        .remove(&receiving_address);
                }
            }
        }
    }
}

// Transparent inputs
//
// TODO: replace arguments with a struct
impl
    UpdateWith<(
        // The inputs from a transaction in this block
        &Vec<transparent::Input>,
        // The hash of the transaction that the inputs are from
        // (not the transaction the spent output was created by)
        &transaction::Hash,
        // The outputs for all inputs spent in this transaction (or block)
        &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
    )> for Chain
{
    fn update_chain_tip_with(
        &mut self,
        &(spending_inputs, spending_tx_hash, spent_outputs): &(
            &Vec<transparent::Input>,
            &transaction::Hash,
            &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
        ),
    ) -> Result<(), ValidateContextError> {
        for spending_input in spending_inputs.iter() {
            let spent_outpoint = if let Some(spent_outpoint) = spending_input.outpoint() {
                spent_outpoint
            } else {
                continue;
            };

            // Index the spent outpoint in the chain
            let first_spend = self.spent_utxos.insert(spent_outpoint);
            assert!(
                first_spend,
                "unexpected duplicate spent output: should be checked earlier"
            );

            // TODO: fix tests to supply correct spent outputs, then turn this into an expect()
            let spent_output = if let Some(spent_output) = spent_outputs.get(&spent_outpoint) {
                spent_output
            } else if !cfg!(test) {
                panic!("unexpected missing spent output: all spent outputs must be indexed");
            } else {
                continue;
            };

            // Index the spent output for the address
            if let Some(spending_address) = spent_output.utxo.output.address(self.network) {
                let address_transfers = self
                    .partial_transparent_transfers
                    .entry(spending_address)
                    .or_default();

                address_transfers.update_chain_tip_with(&(
                    spending_input,
                    spending_tx_hash,
                    spent_output,
                ))?;
            }
        }

        Ok(())
    }

    fn revert_chain_with(
        &mut self,
        &(spending_inputs, spending_tx_hash, spent_outputs): &(
            &Vec<transparent::Input>,
            &transaction::Hash,
            &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
        ),
        position: RevertPosition,
    ) {
        for spending_input in spending_inputs.iter() {
            let spent_outpoint = if let Some(spent_outpoint) = spending_input.outpoint() {
                spent_outpoint
            } else {
                continue;
            };

            // Revert the spent outpoint in the chain
            let spent_outpoint_was_removed = self.spent_utxos.remove(&spent_outpoint);
            assert!(
                spent_outpoint_was_removed,
                "spent_utxos must be present if block was added to chain"
            );

            // TODO: fix tests to supply correct spent outputs, then turn this into an expect()
            let spent_output = if let Some(spent_output) = spent_outputs.get(&spent_outpoint) {
                spent_output
            } else if !cfg!(test) {
                panic!(
                    "unexpected missing reverted spent output: all spent outputs must be indexed"
                );
            } else {
                continue;
            };

            // Revert the spent output for the address
            if let Some(receiving_address) = spent_output.utxo.output.address(self.network) {
                let address_transfers = self
                    .partial_transparent_transfers
                    .get_mut(&receiving_address)
                    .expect("block has previously been applied to the chain");

                address_transfers
                    .revert_chain_with(&(spending_input, spending_tx_hash, spent_output), position);

                // Remove this transfer if it is now empty
                if address_transfers.is_empty() {
                    self.partial_transparent_transfers
                        .remove(&receiving_address);
                }
            }
        }
    }
}

impl UpdateWith<Option<transaction::JoinSplitData<Groth16Proof>>> for Chain {
    #[instrument(skip(self, joinsplit_data))]
    fn update_chain_tip_with(
        &mut self,
        joinsplit_data: &Option<transaction::JoinSplitData<Groth16Proof>>,
    ) -> Result<(), ValidateContextError> {
        if let Some(joinsplit_data) = joinsplit_data {
            // We do note commitment tree updates in parallel rayon threads.

            check::nullifier::add_to_non_finalized_chain_unique(
                &mut self.sprout_nullifiers,
                joinsplit_data.nullifiers(),
            )?;
        }
        Ok(())
    }

    /// # Panics
    ///
    /// Panics if any nullifier is missing from the chain when we try to remove it.
    ///
    /// See [`check::nullifier::remove_from_non_finalized_chain`] for details.
    #[instrument(skip(self, joinsplit_data))]
    fn revert_chain_with(
        &mut self,
        joinsplit_data: &Option<transaction::JoinSplitData<Groth16Proof>>,
        _position: RevertPosition,
    ) {
        if let Some(joinsplit_data) = joinsplit_data {
            // Note commitments are not removed from the Chain during a fork,
            // because we don't support that operation yet. Instead, we
            // recreate the tree from the finalized tip in Chain::fork().
            //
            // TODO: remove trees and anchors above the fork, to save CPU time (#4794)

            check::nullifier::remove_from_non_finalized_chain(
                &mut self.sprout_nullifiers,
                joinsplit_data.nullifiers(),
            );
        }
    }
}

impl<AnchorV> UpdateWith<Option<sapling::ShieldedData<AnchorV>>> for Chain
where
    AnchorV: sapling::AnchorVariant + Clone,
{
    #[instrument(skip(self, sapling_shielded_data))]
    fn update_chain_tip_with(
        &mut self,
        sapling_shielded_data: &Option<sapling::ShieldedData<AnchorV>>,
    ) -> Result<(), ValidateContextError> {
        if let Some(sapling_shielded_data) = sapling_shielded_data {
            // We do note commitment tree updates in parallel rayon threads.

            check::nullifier::add_to_non_finalized_chain_unique(
                &mut self.sapling_nullifiers,
                sapling_shielded_data.nullifiers(),
            )?;
        }
        Ok(())
    }

    /// # Panics
    ///
    /// Panics if any nullifier is missing from the chain when we try to remove it.
    ///
    /// See [`check::nullifier::remove_from_non_finalized_chain`] for details.
    #[instrument(skip(self, sapling_shielded_data))]
    fn revert_chain_with(
        &mut self,
        sapling_shielded_data: &Option<sapling::ShieldedData<AnchorV>>,
        _position: RevertPosition,
    ) {
        if let Some(sapling_shielded_data) = sapling_shielded_data {
            // Note commitments are not removed from the Chain during a fork,
            // because we don't support that operation yet. Instead, we
            // recreate the tree from the finalized tip in Chain::fork().
            //
            // TODO: remove trees and anchors above the fork, to save CPU time (#4794)

            check::nullifier::remove_from_non_finalized_chain(
                &mut self.sapling_nullifiers,
                sapling_shielded_data.nullifiers(),
            );
        }
    }
}

impl UpdateWith<Option<orchard::ShieldedData>> for Chain {
    #[instrument(skip(self, orchard_shielded_data))]
    fn update_chain_tip_with(
        &mut self,
        orchard_shielded_data: &Option<orchard::ShieldedData>,
    ) -> Result<(), ValidateContextError> {
        if let Some(orchard_shielded_data) = orchard_shielded_data {
            // We do note commitment tree updates in parallel rayon threads.

            check::nullifier::add_to_non_finalized_chain_unique(
                &mut self.orchard_nullifiers,
                orchard_shielded_data.nullifiers(),
            )?;
        }
        Ok(())
    }

    /// # Panics
    ///
    /// Panics if any nullifier is missing from the chain when we try to remove it.
    ///
    /// See [`check::nullifier::remove_from_non_finalized_chain`] for details.
    #[instrument(skip(self, orchard_shielded_data))]
    fn revert_chain_with(
        &mut self,
        orchard_shielded_data: &Option<orchard::ShieldedData>,
        _position: RevertPosition,
    ) {
        if let Some(orchard_shielded_data) = orchard_shielded_data {
            // Note commitments are not removed from the Chain during a fork,
            // because we don't support that operation yet. Instead, we
            // recreate the tree from the finalized tip in Chain::fork().
            //
            // TODO: remove trees and anchors above the fork, to save CPU time (#4794)

            check::nullifier::remove_from_non_finalized_chain(
                &mut self.orchard_nullifiers,
                orchard_shielded_data.nullifiers(),
            );
        }
    }
}

impl UpdateWith<ValueBalance<NegativeAllowed>> for Chain {
    fn update_chain_tip_with(
        &mut self,
        block_value_pool_change: &ValueBalance<NegativeAllowed>,
    ) -> Result<(), ValidateContextError> {
        match self
            .chain_value_pools
            .add_chain_value_pool_change(*block_value_pool_change)
        {
            Ok(chain_value_pools) => self.chain_value_pools = chain_value_pools,
            Err(value_balance_error) => Err(ValidateContextError::AddValuePool {
                value_balance_error,
                chain_value_pools: self.chain_value_pools,
                block_value_pool_change: *block_value_pool_change,
                // assume that the current block is added to `blocks` after `update_chain_tip_with`
                height: self.max_block_height().and_then(|height| height + 1),
            })?,
        };

        Ok(())
    }

    /// Revert the chain state using a block chain value pool change.
    ///
    /// When forking from the tip, subtract the block's chain value pool change.
    ///
    /// When finalizing the root, leave the chain value pool balances unchanged.
    /// [`Self::chain_value_pools`] tracks the chain value pools for all
    /// finalized blocks, and the non-finalized blocks in this chain. So
    /// finalizing the root doesn't change the set of blocks it tracks.
    ///
    /// # Panics
    ///
    /// Panics if the chain pool value balance is invalid
    /// after we subtract the block value pool change.
    fn revert_chain_with(
        &mut self,
        block_value_pool_change: &ValueBalance<NegativeAllowed>,
        position: RevertPosition,
    ) {
        use std::ops::Neg;

        if position == RevertPosition::Tip {
            self.chain_value_pools = self
                .chain_value_pools
                .add_chain_value_pool_change(block_value_pool_change.neg())
                .expect("reverting the tip will leave the pools in a previously valid state");
        }
    }
}

impl Ord for Chain {
    /// Chain order for the [`NonFinalizedState`][1]'s `chain_set`.
    ///
    /// Chains with higher cumulative Proof of Work are [`Ordering::Greater`],
    /// breaking ties using the tip block hash.
    ///
    /// Despite the consensus rules, Zebra uses the tip block hash as a
    /// tie-breaker. Zebra blocks are downloaded in parallel, so download
    /// timestamps may not be unique. (And Zebra currently doesn't track
    /// download times, because [`Block`](block::Block)s are immutable.)
    ///
    /// This departure from the consensus rules may delay network convergence,
    /// for as long as the greater hash belongs to the later mined block.
    /// But Zebra nodes should converge as soon as the tied work is broken.
    ///
    /// "At a given point in time, each full validator is aware of a set of candidate blocks.
    /// These form a tree rooted at the genesis block, where each node in the tree
    /// refers to its parent via the hashPrevBlock block header field.
    ///
    /// A path from the root toward the leaves of the tree consisting of a sequence
    /// of one or more valid blocks consistent with consensus rules,
    /// is called a valid block chain.
    ///
    /// In order to choose the best valid block chain in its view of the overall block tree,
    /// a node sums the work ... of all blocks in each valid block chain,
    /// and considers the valid block chain with greatest total work to be best.
    ///
    /// To break ties between leaf blocks, a node will prefer the block that it received first.
    ///
    /// The consensus protocol is designed to ensure that for any given block height,
    /// the vast majority of nodes should eventually agree on their best valid block chain
    /// up to that height."
    ///
    /// <https://zips.z.cash/protocol/protocol.pdf#blockchain>
    ///
    /// # Correctness
    ///
    /// `Chain::cmp` is used in a `BTreeSet`, so the fields accessed by `cmp` must not have
    /// interior mutability.
    ///
    /// # Panics
    ///
    /// If two chains compare equal.
    ///
    /// This panic enforces the [`NonFinalizedState::chain_set`][2] unique chain invariant.
    ///
    /// If the chain set contains duplicate chains, the non-finalized state might
    /// handle new blocks or block finalization incorrectly.
    ///
    /// [1]: super::NonFinalizedState
    /// [2]: super::NonFinalizedState::chain_set
    fn cmp(&self, other: &Self) -> Ordering {
        if self.partial_cumulative_work != other.partial_cumulative_work {
            self.partial_cumulative_work
                .cmp(&other.partial_cumulative_work)
        } else {
            let self_hash = self
                .blocks
                .values()
                .last()
                .expect("always at least 1 element")
                .hash;

            let other_hash = other
                .blocks
                .values()
                .last()
                .expect("always at least 1 element")
                .hash;

            // This comparison is a tie-breaker within the local node, so it does not need to
            // be consistent with the ordering on `ExpandedDifficulty` and `block::Hash`.
            match self_hash.0.cmp(&other_hash.0) {
                Ordering::Equal => unreachable!("Chain tip block hashes are always unique"),
                ordering => ordering,
            }
        }
    }
}

impl PartialOrd for Chain {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Chain {
    /// Chain equality for [`NonFinalizedState::chain_set`][1], using proof of
    /// work, then the tip block hash as a tie-breaker.
    ///
    /// # Panics
    ///
    /// If two chains compare equal.
    ///
    /// See [`Chain::cmp`] for details.
    ///
    /// [1]: super::NonFinalizedState::chain_set
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl Eq for Chain {}
