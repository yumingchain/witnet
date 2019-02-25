use actix::{Actor, AsyncContext, Context, Handler};
use ansi_term::Color::{Purple, White, Yellow};
use log::{debug, error, info, warn};

use witnet_data_structures::{
    chain::{
        ActiveDataRequestPool, CheckpointBeacon, Epoch, Hashable, InventoryEntry, InventoryItem,
        Output, OutputPointer,
    },
    error::{ChainInfoError, ChainInfoErrorKind, ChainInfoResult},
    serializers::decoders::TryFrom,
};
use witnet_rad::types::RadonTypes;
use witnet_util::error::WitnetError;

use super::{data_request::DataRequestPool, ChainManager, ChainManagerError};
use crate::actors::messages::{
    AddNewBlock, AddTransaction, DiscardExistingInventoryEntries, EpochNotification,
    GetBlocksEpochRange, GetHighestCheckpointBeacon, GetOutput, GetOutputResult,
    InventoryEntriesResult, PeerLastEpoch, SessionUnitResult, SetNetworkReady,
};

////////////////////////////////////////////////////////////////////////////////////////
// ACTOR MESSAGE HANDLERS
////////////////////////////////////////////////////////////////////////////////////////
/// Payload for the notification for a specific epoch
#[derive(Debug)]
pub struct EpochPayload;

/// Payload for the notification for all epochs
#[derive(Clone, Debug)]
pub struct EveryEpochPayload;

/// Handler for SetNetworkReady message
impl Handler<SetNetworkReady> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, msg: SetNetworkReady, _ctx: &mut Context<Self>) {
        self.network_ready = msg.network_ready;
    }
}

/// Handler for EpochNotification<EpochPayload>
impl Handler<EpochNotification<EpochPayload>> for ChainManager {
    type Result = ();

    fn handle(&mut self, msg: EpochNotification<EpochPayload>, ctx: &mut Context<Self>) {
        debug!("Epoch notification received {:?}", msg.checkpoint);

        // Genesis checkpoint notification. We need to start building the chain.
        if msg.checkpoint == 0 {
            warn!("Genesis checkpoint is here! Starting to bootstrap the chain...");
            self.started(ctx);
        }
    }
}

/// Handler for EpochNotification<EveryEpochPayload>
impl Handler<EpochNotification<EveryEpochPayload>> for ChainManager {
    type Result = ();

    fn handle(&mut self, msg: EpochNotification<EveryEpochPayload>, ctx: &mut Context<Self>) {
        debug!("Periodic epoch notification received {:?}", msg.checkpoint);
        self.current_epoch = Some(msg.checkpoint);

        if !self.network_ready {
            warn!(
                "Node is not connected to enough peers. Delaying chain bootstrapping until then."
            );

            return;
        }

        // Consolidate the best known block candidate
        if let Some(candidate) = self.best_candidate.take() {
            // Update chain_info
            match self.chain_state.chain_info.as_mut() {
                Some(chain_info) => {
                    // Update `highest_block_checkpoint`
                    let block_hash = candidate.block.hash();
                    let block_epoch = candidate.block.block_header.beacon.checkpoint;
                    let beacon = CheckpointBeacon {
                        checkpoint: block_epoch,
                        hash_prev_block: block_hash,
                    };
                    chain_info.highest_block_checkpoint = beacon;

                    // Insert candidate block into `block_chain`
                    self.chain_state.block_chain.insert(block_epoch, block_hash);

                    // Update utxo_set, transactions_pool and data_request_pool with block_candidate transactions
                    self.chain_state.unspent_outputs_pool = candidate.utxo_set;
                    self.update_transaction_pool(candidate.block.txns.as_ref());
                    self.data_request_pool = candidate.data_request_pool;

                    let reveals = self.data_request_pool.update_data_request_stages();

                    for reveal in reveals {
                        // Send AddTransaction message to self
                        // And broadcast it to all of peers
                        ctx.address().do_send(AddTransaction {
                            transaction: reveal,
                        })
                    }

                    // Persist finished data requests into storage
                    let to_be_stored = self.data_request_pool.finished_data_requests();
                    to_be_stored.into_iter().for_each(|dr| {
                        self.persist_data_request(ctx, &dr);

                        let tally_output_pointer = dr.1.tally;
                        let tr = self
                            .chain_state
                            .unspent_outputs_pool
                            .get(&tally_output_pointer);
                        if let Some(Output::Tally(tally_output)) = tr {
                            let result = RadonTypes::try_from(tally_output.result.as_slice())
                                .map(|x| x.to_string())
                                .unwrap_or_else(|_| "RADError".to_string());
                            info!(
                                "{} {} completed at epoch #{} with result: {}",
                                Yellow.bold().paint("[Data Request]"),
                                Yellow.bold().paint(&dr.0.to_string()),
                                Yellow.bold().paint(block_epoch.to_string()),
                                Yellow.bold().paint(result),
                            );
                        }
                    });

                    // FIXME: Revisit to avoid data redundancies
                    // Store active data requests
                    self.chain_state.data_request_pool = ActiveDataRequestPool {
                        waiting_for_reveal: self.data_request_pool.waiting_for_reveal.clone(),
                        data_requests_by_epoch: self
                            .data_request_pool
                            .data_requests_by_epoch
                            .clone(),
                        data_request_pool: self.data_request_pool.data_request_pool.clone(),
                        to_be_stored: self.data_request_pool.to_be_stored.clone(),
                        dr_pointer_cache: self.data_request_pool.dr_pointer_cache.clone(),
                    };

                    {
                        let info = self.data_request_pool.data_request_pool.iter().fold(
                            String::new(),
                            |acc, (k, v)| {
                                format!(
                                    "{}\n\t* {} Stage: {}, Commits: {}, Reveals: {}",
                                    acc,
                                    White.bold().paint(k.to_string()),
                                    White.bold().paint(format!("{:?}", v.stage)),
                                    v.info.commits.len(),
                                    v.info.reveals.len()
                                )
                            },
                        );

                        if info.is_empty() {
                            info!(
                                "{} Block {} consolidated for epoch #{} {}",
                                Purple.bold().paint("[Chain]"),
                                Purple.bold().paint(candidate.block.hash().to_string()),
                                Purple.bold().paint(block_epoch.to_string()),
                                White.paint("with no data requests".to_string()),
                            );
                        } else {
                            info!(
                                "{} Block {} consolidated for epoch #{}\n{}{}",
                                Purple.bold().paint("[Chain]"),
                                Purple.bold().paint(candidate.block.hash().to_string()),
                                Purple.bold().paint(block_epoch.to_string()),
                                White.bold().paint("Data Requests: "),
                                White.bold().paint(info),
                            );
                        }
                    }

                    debug!("{:?}", candidate.block);
                    debug!(
                        "Mint transaction hash: {:?}",
                        candidate.block.txns[0].hash()
                    );

                    // Send block to Inventory Manager
                    self.persist_item(ctx, InventoryItem::Block(candidate.block));

                    // Persist chain_info into storage
                    self.persist_chain_state(ctx);
                }
                None => {
                    error!("No ChainInfo loaded in ChainManager");
                }
            }
        }

        if self.mining_enabled && self.mine {
            // Data race: the data requests should be sent after mining the block, otherwise
            // it takes 2 epochs to move from one stage to the next one
            self.try_mine_block(ctx);
        }

        // Data request mining MUST finish BEFORE the block has been mined!!!!
        // (The transactions must be included into this block, both the transactions from
        // our node and the transactions from other nodes
        self.try_mine_data_request(ctx);
    }
}

/// Handler for GetHighestBlockCheckpoint message
impl Handler<GetHighestCheckpointBeacon> for ChainManager {
    type Result = ChainInfoResult<CheckpointBeacon>;

    fn handle(
        &mut self,
        _msg: GetHighestCheckpointBeacon,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        if let Some(chain_info) = &self.chain_state.chain_info {
            Ok(chain_info.highest_block_checkpoint)
        } else {
            error!("No ChainInfo loaded in ChainManager");
            Err(WitnetError::from(ChainInfoError::new(
                ChainInfoErrorKind::ChainInfoNotFound,
                "No ChainInfo loaded in ChainManager".to_string(),
            )))
        }
    }
}

/// Handler for AddNewBlock message
impl Handler<AddNewBlock> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, msg: AddNewBlock, ctx: &mut Context<Self>) {
        self.process_block(ctx, msg.block)
    }
}

/// Handler for AddTransaction message
impl Handler<AddTransaction> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, msg: AddTransaction, _ctx: &mut Context<Self>) {
        let transaction_hash = &msg.transaction.hash();
        if self.transactions_pool.contains(transaction_hash) {
            debug!("Transaction is already in the pool: {}", transaction_hash);
            return;
        }

        debug!("Adding transaction: {:?}", msg.transaction);
        // FIXME: transaction validation is broken
        //let outputs = &msg.transaction.outputs;
        let inputs = &msg.transaction.inputs;

        match self.chain_state.get_outputs_from_inputs(inputs) {
            Ok(_outputs_from_inputs) => {
                //let outputs_from_inputs = &outputs_from_inputs;

                // Check that all inputs point to unspent outputs
                if self
                    .chain_state
                    .find_unspent_outputs(&msg.transaction.inputs)
                {
                    /*
                    use crate::utils::{
                        count_tally_outputs, is_commit_input, is_commit_output, is_data_request_input,
                        is_reveal_output, is_tally_output, is_value_transfer_output, validate_tally_output_uniqueness,
                        validate_value_transfer_output_position,
                    };
                                        // Validate transaction
                                        // DRO RULE 1. Multiple data request outputs can be included into a single transaction.
                                        // As long as the inputs are greater than the outputs, the rule still hold true. The difference
                                        // with VTOs is that the total output value for data request outputs also includes the
                                        // commit fee, reveal fee and tally fee.
                                        let inputs_sum = outputs_from_inputs.iter().map(Output::value).sum();
                                        let outputs_sum = msg.transaction.outputs_sum();

                                        if outputs_sum < inputs_sum {
                                            let mut is_valid_input = true;
                                            let mut is_valid_output = true;
                                            let iter = outputs.iter().zip(inputs.iter());

                                            // VTO RULE 2. The number of VTOs in a single transaction is virtually unlimited as
                                            // long as the VTOs are all contiguous and located at the end of the outputs list.
                                            let is_valid_vto_position =
                                                validate_value_transfer_output_position(outputs);

                                            // TO RULE 1. Any transaction can contain at most one tally output.
                                            let consensus_output_overflow = count_tally_outputs(outputs) > 1;

                                            let is_valid_transaction = iter
                                                .take_while(|(output, input)| {
                                                    // Validate input
                                                    is_valid_input =
                                                        match &self.chain_state.get_output_from_input(input) {
                                                            // VTO RULE 4. The value brought into a transaction by an input pointing
                                                            // to a VTO can be freely assigned to any output of any type unless otherwise
                                                            // restricted by the specific validation rules for such output type.
                                                            Some(Output::ValueTransfer(_)) => true,

                                                            // DRO RULE 2. The value brought into a transaction by an input pointing
                                                            // to a data request output can only be spent by commit outputs.
                                                            Some(Output::DataRequest(_)) => is_commit_output(output),

                                                            // CO RULE 3. The value brought into a transaction by an input pointing
                                                            // to a commit output can only be spent by reveal or tally outputs.
                                                            Some(Output::Commit(_)) => {
                                                                is_reveal_output(output) || is_tally_output(output)
                                                            }

                                                            // RO 3. The value brought into a transaction by an input pointing to a
                                                            // reveal output can only be spent by value transfer outputs.
                                                            Some(Output::Reveal(_)) => match output {
                                                                Output::ValueTransfer(_) => true,
                                                                _ => false,
                                                            },
                                                            // TO RULE 4. The value brought into a transaction by an input pointing
                                                            // to a tally output can be freely assigned to any output of any type
                                                            // unless otherwise restricted by the specific validation rules for such
                                                            // output type.
                                                            Some(Output::Tally(_)) => true,

                                                            None => false,
                                                        };

                                                    is_valid_output = match output {
                                                        Output::ValueTransfer(_) => true,

                                                        Output::DataRequest(_) => true,
                                                        // CO RULE 1. Commit outputs can only take value from data request inputs
                                                        // whose index in the inputs list is the same as their own index in the outputs list.
                                                        // CO RULE 2. Multiple commit outputs can exist in a single transaction,
                                                        // but each of them needs to be coupled with a data request input occupying
                                                        // the same index in the inputs list as their own in the outputs list.
                                                        // Predictably, as a result of the previous rule, each of the multiple
                                                        // commit outputs only takes value from the data request input with the same index.
                                                        Output::Commit(_) => is_data_request_input(input),

                                                        Output::Reveal(_) => {
                                                            // RO RULE 3. The value brought into a transaction by an input pointing
                                                            // to a reveal output can only be spent by value transfer outputs.
                                                            is_value_transfer_output(output)
                                                            // RO RULE 1. Reveal outputs can only take value from commit inputs
                                                            // whose index in the inputs list is the same as their own index in the outputs list.
                                                            // RO RULE 2. Multiple reveal outputs can exist in a single transaction,
                                                            // but each of them needs to be coupled with a commit input occupying
                                                            // the same index in the inputs list as their own in the outputs list.
                                                            // Predictably, as a result of the previous rule, each of the multiple
                                                            // reveal outputs only takes value from the commit input with the same index.
                                                            && is_commit_input(input)
                                                            // TODO: validate only once
                                                            // RO RULE 4. Any transaction including an input pointing to a
                                                            // reveal output must also include exactly only one tally output.
                                                            && validate_tally_output_uniqueness(outputs)
                                                        }
                                                        Output::Tally(_) => true,
                                                    };

                                                    is_valid_input && is_valid_output
                                                })
                                                .last();

                                            if is_valid_transaction.is_some()
                                                && is_valid_vto_position
                                                && !consensus_output_overflow
                                            {*/
                    debug!("Transaction added successfully");
                    // Broadcast valid transaction
                    self.broadcast_item(InventoryItem::Transaction(msg.transaction.clone()));

                    // Add valid transaction to transactions_pool
                    self.transactions_pool
                        .insert(*transaction_hash, msg.transaction);
                } else {
                    warn!("Input OutputPointer not in pool");
                }
            }
            Err(_) => {
                //TODO Show input information
                warn!("Input with no output");
            }
        }
    }
}

/// Handler for GetBlocksEpochRange
impl Handler<GetBlocksEpochRange> for ChainManager {
    type Result = Result<Vec<(Epoch, InventoryEntry)>, ChainManagerError>;

    fn handle(
        &mut self,
        GetBlocksEpochRange { range, limit }: GetBlocksEpochRange,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        debug!("GetBlocksEpochRange received {:?}", range);
        let mut hashes: Vec<(Epoch, InventoryEntry)> = self
            .chain_state
            .block_chain
            .range(range)
            .map(|(k, v)| (*k, InventoryEntry::Block(*v)))
            .collect();

        // Hashes Vec has not to be bigger than MAX_BLOCKS_SYNC
        if limit != 0 {
            hashes.truncate(limit);
        }

        Ok(hashes)
    }
}

/// Handler for DiscardExistingInvVectors message
impl Handler<DiscardExistingInventoryEntries> for ChainManager {
    type Result = InventoryEntriesResult;

    fn handle(
        &mut self,
        msg: DiscardExistingInventoryEntries,
        _ctx: &mut Context<Self>,
    ) -> InventoryEntriesResult {
        // Discard existing inventory vectors
        self.discard_existing_inventory_entries(msg.inv_entries)
    }
}

impl Handler<GetOutput> for ChainManager {
    type Result = GetOutputResult;

    fn handle(
        &mut self,
        GetOutput { output_pointer }: GetOutput,
        _ctx: &mut Context<Self>,
    ) -> GetOutputResult {
        find_output_from_pointer(&self.data_request_pool, &output_pointer)
    }
}

fn find_output_from_pointer(
    d: &DataRequestPool,
    pointer: &OutputPointer,
) -> Result<Output, ChainManagerError> {
    if let Some(dr) = d.data_request_state(pointer) {
        // This pointer is already in our DataRequestPool
        Ok(Output::DataRequest(dr.data_request.clone()))
    } else {
        // FIXME: retrieve Output from Storage
        Err(ChainManagerError::BlockDoesNotExist)
    }
}

impl Handler<PeerLastEpoch> for ChainManager {
    type Result = ();

    fn handle(&mut self, msg: PeerLastEpoch, _ctx: &mut Context<Self>) {
        if msg.epoch
            == self
                .chain_state
                .chain_info
                .as_ref()
                .map(|x| x.highest_block_checkpoint.checkpoint)
                .unwrap_or(0)
        {
            self.synced = true;
        } else {
            warn!("Mine flag disabled");
            self.synced = false;
            self.mine = false;
            self.best_candidate = None;
        }
    }
}
