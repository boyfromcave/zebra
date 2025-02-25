//! Provides high-level access to database [`Block`]s and [`Transaction`]s.
//!
//! This module makes sure that:
//! - all disk writes happen inside a RocksDB transaction, and
//! - format-specific invariants are maintained.
//!
//! # Correctness
//!
//! The [`crate::constants::DATABASE_FORMAT_VERSION`] constant must
//! be incremented each time the database format (column, serialization, etc) changes.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use itertools::Itertools;

use zebra_chain::{
    amount::NonNegative,
    block::{self, Block, Height},
    history_tree::HistoryTree,
    orchard,
    parallel::tree::NoteCommitmentTrees,
    parameters::{Network, GENESIS_PREVIOUS_BLOCK_HASH},
    sapling,
    serialization::TrustedPreallocate,
    transaction::{self, Transaction},
    transparent,
    value_balance::ValueBalance,
};

use crate::{
    service::finalized_state::{
        disk_db::{DiskDb, DiskWriteBatch, ReadDisk, WriteDisk},
        disk_format::{
            block::TransactionLocation,
            transparent::{AddressBalanceLocation, OutputLocation},
        },
        zebra_db::{metrics::block_precommit_metrics, ZebraDb},
        FinalizedBlock,
    },
    BoxError, HashOrHeight,
};

#[cfg(test)]
mod tests;

impl ZebraDb {
    // Read block methods

    /// Returns true if the database is empty.
    //
    // TODO: move this method to the tip section
    pub fn is_empty(&self) -> bool {
        let hash_by_height = self.db.cf_handle("hash_by_height").unwrap();
        self.db.zs_is_empty(&hash_by_height)
    }

    /// Returns the tip height and hash, if there is one.
    //
    // TODO: rename to finalized_tip()
    //       move this method to the tip section
    #[allow(clippy::unwrap_in_result)]
    pub fn tip(&self) -> Option<(block::Height, block::Hash)> {
        let hash_by_height = self.db.cf_handle("hash_by_height").unwrap();
        self.db.zs_last_key_value(&hash_by_height)
    }

    /// Returns `true` if `height` is present in the finalized state.
    #[allow(clippy::unwrap_in_result)]
    pub fn contains_height(&self, height: block::Height) -> bool {
        let hash_by_height = self.db.cf_handle("hash_by_height").unwrap();

        self.db.zs_contains(&hash_by_height, &height)
    }

    /// Returns the finalized hash for a given `block::Height` if it is present.
    #[allow(clippy::unwrap_in_result)]
    pub fn hash(&self, height: block::Height) -> Option<block::Hash> {
        let hash_by_height = self.db.cf_handle("hash_by_height").unwrap();
        self.db.zs_get(&hash_by_height, &height)
    }

    /// Returns `true` if `hash` is present in the finalized state.
    #[allow(clippy::unwrap_in_result)]
    pub fn contains_hash(&self, hash: block::Hash) -> bool {
        let height_by_hash = self.db.cf_handle("height_by_hash").unwrap();

        self.db.zs_contains(&height_by_hash, &hash)
    }

    /// Returns the height of the given block if it exists.
    #[allow(clippy::unwrap_in_result)]
    pub fn height(&self, hash: block::Hash) -> Option<block::Height> {
        let height_by_hash = self.db.cf_handle("height_by_hash").unwrap();
        self.db.zs_get(&height_by_hash, &hash)
    }

    /// Returns the [`block::Header`](zebra_chain::block::Header) with [`block::Hash`](zebra_chain::block::Hash)
    /// or [`Height`](zebra_chain::block::Height), if it exists in the finalized chain.
    //
    // TODO: move this method to the start of the section
    #[allow(clippy::unwrap_in_result)]
    pub fn block_header(&self, hash_or_height: HashOrHeight) -> Option<Arc<block::Header>> {
        // Block Header
        let block_header_by_height = self.db.cf_handle("block_header_by_height").unwrap();

        let height = hash_or_height.height_or_else(|hash| self.height(hash))?;
        let header = self.db.zs_get(&block_header_by_height, &height)?;

        Some(header)
    }

    /// Returns the [`Block`] with [`block::Hash`](zebra_chain::block::Hash) or
    /// [`Height`](zebra_chain::block::Height), if it exists in the finalized chain.
    //
    // TODO: move this method to the start of the section
    #[allow(clippy::unwrap_in_result)]
    pub fn block(&self, hash_or_height: HashOrHeight) -> Option<Arc<Block>> {
        // Block
        let height = hash_or_height.height_or_else(|hash| self.height(hash))?;
        let header = self.block_header(height.into())?;

        // Transactions
        let tx_by_loc = self.db.cf_handle("tx_by_loc").unwrap();

        // Manually fetch the entire block's transactions
        let mut transactions = Vec::new();

        // TODO:
        // - split disk reads from deserialization, and run deserialization in parallel,
        //   this improves performance for blocks with multiple large shielded transactions
        // - is this loop more efficient if we store the number of transactions?
        // - is the difference large enough to matter?
        for tx_index in 0..=Transaction::max_allocation() {
            let tx_loc = TransactionLocation::from_u64(height, tx_index);

            if let Some(tx) = self.db.zs_get(&tx_by_loc, &tx_loc) {
                transactions.push(tx);
            } else {
                break;
            }
        }

        Some(Arc::new(Block {
            header,
            transactions,
        }))
    }

    /// Returns the Sapling
    /// [`NoteCommitmentTree`](sapling::tree::NoteCommitmentTree) specified by a
    /// hash or height, if it exists in the finalized `db`.
    #[allow(clippy::unwrap_in_result)]
    pub fn sapling_tree(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<sapling::tree::NoteCommitmentTree>> {
        let height = hash_or_height.height_or_else(|hash| self.height(hash))?;

        let sapling_tree_handle = self.db.cf_handle("sapling_note_commitment_tree").unwrap();

        self.db.zs_get(&sapling_tree_handle, &height)
    }

    /// Returns the Orchard
    /// [`NoteCommitmentTree`](orchard::tree::NoteCommitmentTree) specified by a
    /// hash or height, if it exists in the finalized `db`.
    #[allow(clippy::unwrap_in_result)]
    pub fn orchard_tree(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<orchard::tree::NoteCommitmentTree>> {
        let height = hash_or_height.height_or_else(|hash| self.height(hash))?;

        let orchard_tree_handle = self.db.cf_handle("orchard_note_commitment_tree").unwrap();

        self.db.zs_get(&orchard_tree_handle, &height)
    }

    // Read tip block methods

    /// Returns the hash of the current finalized tip block.
    pub fn finalized_tip_hash(&self) -> block::Hash {
        self.tip()
            .map(|(_, hash)| hash)
            // if the state is empty, return the genesis previous block hash
            .unwrap_or(GENESIS_PREVIOUS_BLOCK_HASH)
    }

    /// Returns the height of the current finalized tip block.
    pub fn finalized_tip_height(&self) -> Option<block::Height> {
        self.tip().map(|(height, _)| height)
    }

    /// Returns the tip block, if there is one.
    pub fn tip_block(&self) -> Option<Arc<Block>> {
        let (height, _hash) = self.tip()?;
        self.block(height.into())
    }

    // Read transaction methods

    /// Returns the [`TransactionLocation`] for [`transaction::Hash`],
    /// if it exists in the finalized chain.
    #[allow(clippy::unwrap_in_result)]
    pub fn transaction_location(&self, hash: transaction::Hash) -> Option<TransactionLocation> {
        let tx_loc_by_hash = self.db.cf_handle("tx_loc_by_hash").unwrap();
        self.db.zs_get(&tx_loc_by_hash, &hash)
    }

    /// Returns the [`transaction::Hash`] for [`TransactionLocation`],
    /// if it exists in the finalized chain.
    #[allow(clippy::unwrap_in_result)]
    #[allow(dead_code)]
    pub fn transaction_hash(&self, location: TransactionLocation) -> Option<transaction::Hash> {
        let hash_by_tx_loc = self.db.cf_handle("hash_by_tx_loc").unwrap();
        self.db.zs_get(&hash_by_tx_loc, &location)
    }

    /// Returns the [`Transaction`] with [`transaction::Hash`], and its [`Height`],
    /// if a transaction with that hash exists in the finalized chain.
    //
    // TODO: move this method to the start of the section
    #[allow(clippy::unwrap_in_result)]
    pub fn transaction(&self, hash: transaction::Hash) -> Option<(Arc<Transaction>, Height)> {
        let tx_by_loc = self.db.cf_handle("tx_by_loc").unwrap();

        let transaction_location = self.transaction_location(hash)?;

        self.db
            .zs_get(&tx_by_loc, &transaction_location)
            .map(|tx| (tx, transaction_location.height))
    }

    /// Returns the [`transaction::Hash`]es in the block with `hash_or_height`,
    /// if it exists in this chain.
    ///
    /// Hashes are returned in block order.
    ///
    /// Returns `None` if the block is not found.
    #[allow(clippy::unwrap_in_result)]
    pub fn transaction_hashes_for_block(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Option<Arc<[transaction::Hash]>> {
        // Block
        let height = hash_or_height.height_or_else(|hash| self.height(hash))?;

        // Transaction hashes
        let hash_by_tx_loc = self.db.cf_handle("hash_by_tx_loc").unwrap();

        // Manually fetch the entire block's transaction hashes
        let mut transaction_hashes = Vec::new();

        for tx_index in 0..=Transaction::max_allocation() {
            let tx_loc = TransactionLocation::from_u64(height, tx_index);

            if let Some(tx_hash) = self.db.zs_get(&hash_by_tx_loc, &tx_loc) {
                transaction_hashes.push(tx_hash);
            } else {
                break;
            }
        }

        Some(transaction_hashes.into())
    }

    // Write block methods

    /// Write `finalized` to the finalized state.
    ///
    /// Uses:
    /// - `history_tree`: the current tip's history tree
    /// - `network`: the configured network
    /// - `source`: the source of the block in log messages
    ///
    /// # Errors
    ///
    /// - Propagates any errors from writing to the DB
    /// - Propagates any errors from updating history and note commitment trees
    pub(in super::super) fn write_block(
        &mut self,
        finalized: FinalizedBlock,
        history_tree: Arc<HistoryTree>,
        note_commitment_trees: NoteCommitmentTrees,
        network: Network,
        source: &str,
    ) -> Result<block::Hash, BoxError> {
        let finalized_hash = finalized.hash;

        let tx_hash_indexes: HashMap<transaction::Hash, usize> = finalized
            .transaction_hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| (*hash, index))
            .collect();

        // Get a list of the new UTXOs in the format we need for database updates.
        //
        // TODO: index new_outputs by TransactionLocation,
        //       simplify the spent_utxos location lookup code,
        //       and remove the extra new_outputs_by_out_loc argument
        let new_outputs_by_out_loc: BTreeMap<OutputLocation, transparent::Utxo> = finalized
            .new_outputs
            .iter()
            .map(|(outpoint, utxo)| {
                (
                    lookup_out_loc(finalized.height, outpoint, &tx_hash_indexes),
                    utxo.clone(),
                )
            })
            .collect();

        // Get a list of the spent UTXOs, before we delete any from the database
        let spent_utxos: Vec<(transparent::OutPoint, OutputLocation, transparent::Utxo)> =
            finalized
                .block
                .transactions
                .iter()
                .flat_map(|tx| tx.inputs().iter())
                .flat_map(|input| input.outpoint())
                .map(|outpoint| {
                    (
                        outpoint,
                        // Some utxos are spent in the same block, so they will be in
                        // `tx_hash_indexes` and `new_outputs`
                        self.output_location(&outpoint).unwrap_or_else(|| {
                            lookup_out_loc(finalized.height, &outpoint, &tx_hash_indexes)
                        }),
                        self.utxo(&outpoint)
                            .map(|ordered_utxo| ordered_utxo.utxo)
                            .or_else(|| finalized.new_outputs.get(&outpoint).cloned())
                            .expect("already checked UTXO was in state or block"),
                    )
                })
                .collect();

        let spent_utxos_by_outpoint: HashMap<transparent::OutPoint, transparent::Utxo> =
            spent_utxos
                .iter()
                .map(|(outpoint, _output_loc, utxo)| (*outpoint, utxo.clone()))
                .collect();
        let spent_utxos_by_out_loc: BTreeMap<OutputLocation, transparent::Utxo> = spent_utxos
            .into_iter()
            .map(|(_outpoint, out_loc, utxo)| (out_loc, utxo))
            .collect();

        // Get the transparent addresses with changed balances/UTXOs
        let changed_addresses: HashSet<transparent::Address> = spent_utxos_by_out_loc
            .values()
            .chain(finalized.new_outputs.values())
            .filter_map(|utxo| utxo.output.address(network))
            .unique()
            .collect();

        // Get the current address balances, before the transactions in this block
        let address_balances: HashMap<transparent::Address, AddressBalanceLocation> =
            changed_addresses
                .into_iter()
                .filter_map(|address| Some((address, self.address_balance_location(&address)?)))
                .collect();

        let mut batch = DiskWriteBatch::new(network);

        // In case of errors, propagate and do not write the batch.
        batch.prepare_block_batch(
            &self.db,
            finalized,
            new_outputs_by_out_loc,
            spent_utxos_by_outpoint,
            spent_utxos_by_out_loc,
            address_balances,
            history_tree,
            note_commitment_trees,
            self.finalized_value_pool(),
        )?;

        self.db.write(batch)?;

        tracing::trace!(?source, "committed block from");

        Ok(finalized_hash)
    }
}

/// Lookup the output location for an outpoint.
///
/// `tx_hash_indexes` must contain `outpoint.hash` and that transaction's index in its block.
fn lookup_out_loc(
    height: Height,
    outpoint: &transparent::OutPoint,
    tx_hash_indexes: &HashMap<transaction::Hash, usize>,
) -> OutputLocation {
    let tx_index = tx_hash_indexes
        .get(&outpoint.hash)
        .expect("already checked UTXO was in state or block");

    let tx_loc = TransactionLocation::from_usize(height, *tx_index);

    OutputLocation::from_outpoint(tx_loc, outpoint)
}

impl DiskWriteBatch {
    // Write block methods

    /// Prepare a database batch containing `finalized.block`,
    /// and return it (without actually writing anything).
    ///
    /// If this method returns an error, it will be propagated,
    /// and the batch should not be written to the database.
    ///
    /// # Errors
    ///
    /// - Propagates any errors from updating history tree, note commitment trees, or value pools
    //
    // TODO: move db, finalized, and maybe other arguments into DiskWriteBatch
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_block_batch(
        &mut self,
        db: &DiskDb,
        finalized: FinalizedBlock,
        new_outputs_by_out_loc: BTreeMap<OutputLocation, transparent::Utxo>,
        spent_utxos_by_outpoint: HashMap<transparent::OutPoint, transparent::Utxo>,
        spent_utxos_by_out_loc: BTreeMap<OutputLocation, transparent::Utxo>,
        address_balances: HashMap<transparent::Address, AddressBalanceLocation>,
        history_tree: Arc<HistoryTree>,
        note_commitment_trees: NoteCommitmentTrees,
        value_pool: ValueBalance<NonNegative>,
    ) -> Result<(), BoxError> {
        let FinalizedBlock {
            block,
            hash,
            height,
            ..
        } = &finalized;

        // Commit block and transaction data.
        // (Transaction indexes, note commitments, and UTXOs are committed later.)
        self.prepare_block_header_and_transaction_data_batch(db, &finalized)?;

        // # Consensus
        //
        // > A transaction MUST NOT spend an output of the genesis block coinbase transaction.
        // > (There is one such zero-valued output, on each of Testnet and Mainnet.)
        //
        // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
        //
        // By returning early, Zebra commits the genesis block and transaction data,
        // but it ignores the genesis UTXO and value pool updates.
        if self.prepare_genesis_batch(db, &finalized) {
            return Ok(());
        }

        // Commit transaction indexes
        self.prepare_transparent_transaction_batch(
            db,
            &finalized,
            &new_outputs_by_out_loc,
            &spent_utxos_by_outpoint,
            &spent_utxos_by_out_loc,
            address_balances,
        )?;
        self.prepare_shielded_transaction_batch(db, &finalized)?;

        self.prepare_note_commitment_batch(db, &finalized, note_commitment_trees, history_tree)?;

        // Commit UTXOs and value pools
        self.prepare_chain_value_pools_batch(db, &finalized, spent_utxos_by_outpoint, value_pool)?;

        // The block has passed contextual validation, so update the metrics
        block_precommit_metrics(block, *hash, *height);

        Ok(())
    }

    /// Prepare a database batch containing the block header and transaction data
    /// from `finalized.block`, and return it (without actually writing anything).
    ///
    /// # Errors
    ///
    /// - This method does not currently return any errors.
    #[allow(clippy::unwrap_in_result)]
    pub fn prepare_block_header_and_transaction_data_batch(
        &mut self,
        db: &DiskDb,
        finalized: &FinalizedBlock,
    ) -> Result<(), BoxError> {
        // Blocks
        let block_header_by_height = db.cf_handle("block_header_by_height").unwrap();
        let hash_by_height = db.cf_handle("hash_by_height").unwrap();
        let height_by_hash = db.cf_handle("height_by_hash").unwrap();

        // Transactions
        let tx_by_loc = db.cf_handle("tx_by_loc").unwrap();
        let hash_by_tx_loc = db.cf_handle("hash_by_tx_loc").unwrap();
        let tx_loc_by_hash = db.cf_handle("tx_loc_by_hash").unwrap();

        let FinalizedBlock {
            block,
            hash,
            height,
            transaction_hashes,
            ..
        } = finalized;

        // Commit block header data
        self.zs_insert(&block_header_by_height, height, &block.header);

        // Index the block hash and height
        self.zs_insert(&hash_by_height, height, hash);
        self.zs_insert(&height_by_hash, hash, height);

        for (transaction_index, (transaction, transaction_hash)) in block
            .transactions
            .iter()
            .zip(transaction_hashes.iter())
            .enumerate()
        {
            let transaction_location = TransactionLocation::from_usize(*height, transaction_index);

            // Commit each transaction's data
            self.zs_insert(&tx_by_loc, transaction_location, transaction);

            // Index each transaction hash and location
            self.zs_insert(&hash_by_tx_loc, transaction_location, transaction_hash);
            self.zs_insert(&tx_loc_by_hash, transaction_hash, transaction_location);
        }

        Ok(())
    }

    /// If `finalized.block` is a genesis block,
    /// prepare a database batch that finishes initializing the database,
    /// and return `true` (without actually writing anything).
    ///
    /// Since the genesis block's transactions are skipped,
    /// the returned genesis batch should be written to the database immediately.
    ///
    /// If `finalized.block` is not a genesis block, does nothing.
    ///
    /// This method never returns an error.
    pub fn prepare_genesis_batch(&mut self, db: &DiskDb, finalized: &FinalizedBlock) -> bool {
        let FinalizedBlock { block, .. } = finalized;

        if block.header.previous_block_hash == GENESIS_PREVIOUS_BLOCK_HASH {
            self.prepare_genesis_note_commitment_tree_batch(db, finalized);

            return true;
        }

        false
    }
}
