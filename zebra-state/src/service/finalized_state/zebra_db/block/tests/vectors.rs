//! Fixed database test vectors for blocks and transactions.
//!
//! These tests check that the database correctly serializes
//! and deserializes large heights, blocks and transactions.
//!
//! # TODO
//!
//! Test large blocks and transactions with shielded data,
//! including data activated in Overwinter and later network upgrades.
//!
//! Check transparent address indexes, UTXOs, etc.

use std::{iter, sync::Arc};

use zebra_chain::{
    block::{
        tests::generate::{
            large_multi_transaction_block, large_single_transaction_block_many_inputs,
            large_single_transaction_block_many_outputs,
        },
        Block, Height,
    },
    parameters::Network::{self, *},
    serialization::{ZcashDeserializeInto, ZcashSerialize},
};
use zebra_test::vectors::{MAINNET_BLOCKS, TESTNET_BLOCKS};

use crate::{
    service::finalized_state::{disk_db::DiskWriteBatch, FinalizedState},
    Config, FinalizedBlock,
};

/// Storage round-trip test for block and transaction data in the finalized state database.
#[test]
fn test_block_db_round_trip() {
    let mainnet_test_cases = MAINNET_BLOCKS
        .iter()
        .map(|(_height, block)| block.zcash_deserialize_into().unwrap());
    let testnet_test_cases = TESTNET_BLOCKS
        .iter()
        .map(|(_height, block)| block.zcash_deserialize_into().unwrap());

    test_block_db_round_trip_with(Mainnet, mainnet_test_cases);
    test_block_db_round_trip_with(Testnet, testnet_test_cases);

    // It doesn't matter if these blocks are mainnet or testnet,
    // because there is no validation at this level of the database.
    //
    // These blocks have the same height and header hash, so they each need a new state.
    test_block_db_round_trip_with(Mainnet, iter::once(large_multi_transaction_block()));

    // These blocks are unstable under serialization, so we apply a round-trip first.
    //
    // TODO: fix the bug in the generated test vectors.
    let block = large_single_transaction_block_many_inputs();
    let block_data = block
        .zcash_serialize_to_vec()
        .expect("serialization to vec never fails");
    let block: Block = block_data
        .zcash_deserialize_into()
        .expect("deserialization of valid serialized block never fails");
    test_block_db_round_trip_with(Mainnet, iter::once(block));

    let block = large_single_transaction_block_many_outputs();
    let block_data = block
        .zcash_serialize_to_vec()
        .expect("serialization to vec never fails");
    let block: Block = block_data
        .zcash_deserialize_into()
        .expect("deserialization of valid serialized block never fails");
    test_block_db_round_trip_with(Mainnet, iter::once(block));
}

fn test_block_db_round_trip_with(
    network: Network,
    block_test_cases: impl IntoIterator<Item = Block>,
) {
    let _init_guard = zebra_test::init();

    let state = FinalizedState::new(&Config::ephemeral(), network);

    // Check that each block round-trips to the database
    for original_block in block_test_cases.into_iter() {
        // First, check that the block round-trips without using the database
        let block_data = original_block
            .zcash_serialize_to_vec()
            .expect("serialization to vec never fails");
        let round_trip_block: Block = block_data
            .zcash_deserialize_into()
            .expect("deserialization of valid serialized block never fails");
        let round_trip_data = round_trip_block
            .zcash_serialize_to_vec()
            .expect("serialization to vec never fails");

        assert_eq!(
            original_block, round_trip_block,
            "test block structure must round-trip",
        );
        assert_eq!(
            block_data, round_trip_data,
            "test block data must round-trip",
        );

        // Now, use the database
        let original_block = Arc::new(original_block);
        let finalized = if original_block.coinbase_height().is_some() {
            original_block.clone().into()
        } else {
            // Fake a zero height
            FinalizedBlock::with_hash_and_height(
                original_block.clone(),
                original_block.hash(),
                Height(0),
            )
        };

        // Skip validation by writing the block directly to the database
        let mut batch = DiskWriteBatch::new(Mainnet);
        batch
            .prepare_block_header_and_transaction_data_batch(&state.db, &finalized)
            .expect("block is valid for batch");
        state.db.write(batch).expect("block is valid for writing");

        // Now read it back from the state
        let stored_block = state
            .block(finalized.height.into())
            .expect("block was stored at height");

        if stored_block != original_block {
            error!(
                "
                detailed block mismatch report:
                original: {:?}\n\
                original data: {:?}\n\
                stored: {:?}\n\
                stored data: {:?}\n\
                ",
                original_block,
                hex::encode(original_block.zcash_serialize_to_vec().unwrap()),
                stored_block,
                hex::encode(stored_block.zcash_serialize_to_vec().unwrap()),
            );
        }

        assert_eq!(stored_block, original_block);
    }
}
