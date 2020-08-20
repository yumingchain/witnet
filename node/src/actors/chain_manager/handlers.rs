use actix::{fut::WrapFuture, prelude::*};
use futures::future::Future;
use std::{
    collections::BTreeMap,
    collections::{HashMap, HashSet},
    convert::TryFrom,
    net::SocketAddr,
};

use witnet_data_structures::{
    chain::{
        get_utxo_info, ChainState, CheckpointBeacon, DataRequestInfo, DataRequestReport, Epoch,
        Hash, Hashable, NodeStats, PublicKeyHash, Reputation, SuperBlockVote, UtxoInfo,
    },
    error::{ChainInfoError, TransactionError::DataRequestNotFound},
    transaction::{DRTransaction, Transaction, VTTransaction},
    types::LastBeacon,
};
use witnet_util::timestamp::get_timestamp;
use witnet_validations::validations::validate_rad_request;

use super::{
    show_sync_progress, transaction_factory, ChainManager, ChainManagerError, StateMachine,
    SyncTarget,
};
use crate::{
    actors::{
        chain_manager::BlockCandidate,
        messages::{
            AddBlocks, AddCandidates, AddCommitReveal, AddSuperBlockVote, AddTransaction,
            Broadcast, BuildDrt, BuildVtt, EpochNotification, GetBalance, GetBlocksEpochRange,
            GetDataRequestReport, GetHighestCheckpointBeacon, GetMemoryTransaction, GetMempool,
            GetMempoolResult, GetNodeStats, GetReputation, GetReputationAll, GetReputationStatus,
            GetReputationStatusResult, GetState, GetSuperBlockVotes, GetUtxoInfo, PeersBeacons,
            SendLastBeacon, SessionUnitResult, SetLastBeacon, TryMineBlock,
        },
        sessions_manager::SessionsManager,
    },
    storage_mngr,
    utils::mode_consensus,
};

pub const SYNCED_BANNER: &str = r"
███████╗██╗   ██╗███╗   ██╗ ██████╗███████╗██████╗ ██╗
██╔════╝╚██╗ ██╔╝████╗  ██║██╔════╝██╔════╝██╔══██╗██║
███████╗ ╚████╔╝ ██╔██╗ ██║██║     █████╗  ██║  ██║██║
╚════██║  ╚██╔╝  ██║╚██╗██║██║     ██╔══╝  ██║  ██║╚═╝
███████║   ██║   ██║ ╚████║╚██████╗███████╗██████╔╝██╗
╚══════╝   ╚═╝   ╚═╝  ╚═══╝ ╚═════╝╚══════╝╚═════╝ ╚═╝
╔════════════════════════════════════════════════════╗
║ This node has finished bootstrapping and is now    ║
║ working at full steam in validating transactions,  ║
║ proposing blocks and resolving data requests.      ║
╟────────────────────────────────────────────────────╢
║ You can now sit back and enjoy Witnet.             ║
╟────────────────────────────────────────────────────╢
║ Wait... Are you still there? You want more fun?    ║
║ Go to https://docs.witnet.io/node-operators/cli/   ║
║ to learn how to monitor the progress of your node  ║
║ (balance, reputation, proposed blocks, etc.)       ║
╚════════════════════════════════════════════════════╝";

////////////////////////////////////////////////////////////////////////////////////////
// ACTOR MESSAGE HANDLERS
////////////////////////////////////////////////////////////////////////////////////////

/// Payload for the notification for all epochs
#[derive(Clone, Debug)]
pub struct EveryEpochPayload;

/// Handler for EpochNotification<EveryEpochPayload>
impl Handler<EpochNotification<EveryEpochPayload>> for ChainManager {
    type Result = ();

    #[allow(clippy::cognitive_complexity)]
    fn handle(&mut self, msg: EpochNotification<EveryEpochPayload>, ctx: &mut Context<Self>) {
        log::debug!("Periodic epoch notification received {:?}", msg.checkpoint);
        let current_timestamp = get_timestamp();
        log::debug!(
            "Timestamp diff: {}, Epoch timestamp: {}. Current timestamp: {}",
            current_timestamp as i64 - msg.timestamp as i64,
            msg.timestamp,
            current_timestamp
        );

        let last_checked_epoch = self.current_epoch;
        let current_epoch = msg.checkpoint;
        self.current_epoch = Some(current_epoch);

        log::debug!(
            "EpochNotification received while StateMachine is in state {:?}",
            self.sm_state
        );
        let chain_beacon = self.get_chain_beacon();
        log::debug!(
            "Chain state ---> checkpoint: {}, hash_prev_block: {}",
            chain_beacon.checkpoint,
            chain_beacon.hash_prev_block
        );

        // Clear pending transactions HashSet
        self.transactions_pool.clear_pending_transactions();

        // Handle case consensus not achieved
        if !self.peers_beacons_received {
            log::warn!("No beacon messages received from peers. Moving to WaitingConsensus state");
            self.sm_state = StateMachine::WaitingConsensus;
            // Clear candidates
            self.candidates.clear();
            self.seen_candidates.clear();
        }

        if let Some(last_checked_epoch) = last_checked_epoch {
            if msg.checkpoint - last_checked_epoch != 1 {
                log::warn!(
                    "Missed epoch notification {}. Moving to WaitingConsensus state",
                    last_checked_epoch + 1
                );
                self.sm_state = StateMachine::WaitingConsensus;
            }
        }

        self.peers_beacons_received = false;
        match self.sm_state {
            StateMachine::WaitingConsensus => {
                if let Some(chain_info) = &self.chain_state.chain_info {
                    // Send last beacon because otherwise the network cannot bootstrap
                    let sessions_manager = SessionsManager::from_registry();
                    let last_beacon = LastBeacon {
                        highest_block_checkpoint: chain_info.highest_block_checkpoint,
                        highest_superblock_checkpoint: self.get_superblock_beacon(),
                    };
                    sessions_manager.do_send(SetLastBeacon {
                        beacon: last_beacon.clone(),
                    });
                    sessions_manager.do_send(Broadcast {
                        command: SendLastBeacon { last_beacon },
                        only_inbound: true,
                    });
                }
            }
            StateMachine::Synchronizing => {}
            StateMachine::AlmostSynced | StateMachine::Synced => {
                match self.chain_state {
                    ChainState {
                        reputation_engine: Some(_),
                        ..
                    } => {
                        if self.epoch_constants.is_none()
                            || self.vrf_ctx.is_none()
                            || self.secp.is_none()
                        {
                            log::error!("{}", ChainManagerError::ChainNotReady);
                            return;
                        }

                        // Consolidate the best candidate
                        if let Some(BlockCandidate {
                            block,
                            utxo_diff,
                            reputation: _,
                            vrf_proof: _,
                        }) = self.best_candidate.take()
                        {
                            // Persist block and update ChainState
                            self.consolidate_block(ctx, block, utxo_diff);
                        } else if msg.checkpoint > 0 {
                            let previous_epoch = msg.checkpoint - 1;
                            log::warn!(
                                "There was no valid block candidate to consolidate for epoch {}",
                                previous_epoch
                            );
                        }

                        // Send last beacon on block consolidation
                        let sessions_manager = SessionsManager::from_registry();
                        let beacon = self
                            .chain_state
                            .chain_info
                            .as_ref()
                            .unwrap()
                            .highest_block_checkpoint;
                        let superblock_beacon = self.get_superblock_beacon();
                        let last_beacon = LastBeacon {
                            highest_block_checkpoint: beacon,
                            highest_superblock_checkpoint: superblock_beacon,
                        };
                        sessions_manager.do_send(SetLastBeacon {
                            beacon: last_beacon.clone(),
                        });
                        sessions_manager.do_send(Broadcast {
                            command: SendLastBeacon { last_beacon },
                            only_inbound: true,
                        });

                        // TODO: Review time since commits are clear and new ones are received before to mining
                        // Remove commits because they expire every epoch
                        self.transactions_pool.clear_commits();

                        // Mining
                        if self.mining_enabled {
                            // Block mining is now triggered by SessionsManager on peers beacon timeout
                            // Data request mining MUST finish BEFORE the block has been mined!!!!
                            // The transactions must be included into this block, both the transactions from
                            // our node and the transactions from other nodes
                            self.try_mine_data_request(ctx);
                        }

                        // Clear candidates
                        self.candidates.clear();
                        self.seen_candidates.clear();

                        log::debug!(
                            "Transactions pool size: {} value transfer, {} data request",
                            self.transactions_pool.vt_len(),
                            self.transactions_pool.dr_len()
                        );
                    }

                    _ => {
                        log::error!("No ChainInfo loaded in ChainManager");
                    }
                }
            }
        }

        self.peers_beacons_received = false;
    }
}

/// Handler for GetHighestBlockCheckpoint message
impl Handler<GetHighestCheckpointBeacon> for ChainManager {
    type Result = Result<CheckpointBeacon, failure::Error>;

    fn handle(
        &mut self,
        _msg: GetHighestCheckpointBeacon,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        if let Some(chain_info) = &self.chain_state.chain_info {
            Ok(chain_info.highest_block_checkpoint)
        } else {
            log::error!("No ChainInfo loaded in ChainManager");

            Err(ChainInfoError::ChainInfoNotFound.into())
        }
    }
}

/// Handler for GetSuperBlockVotes message
impl Handler<GetSuperBlockVotes> for ChainManager {
    type Result = Result<HashSet<SuperBlockVote>, failure::Error>;

    fn handle(&mut self, _msg: GetSuperBlockVotes, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(self
            .chain_state
            .superblock_state
            .get_current_superblock_votes())
    }
}

/// Handler for GetNodeStats message
impl Handler<GetNodeStats> for ChainManager {
    type Result = Result<NodeStats, failure::Error>;

    fn handle(&mut self, _msg: GetNodeStats, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(self.chain_state.node_stats.clone())
    }
}

/// Handler for AddBlocks message
impl Handler<AddBlocks> for ChainManager {
    type Result = SessionUnitResult;

    #[allow(clippy::cognitive_complexity)]
    fn handle(&mut self, msg: AddBlocks, ctx: &mut Context<Self>) {
        log::debug!(
            "AddBlocks received while StateMachine is in state {:?}",
            self.sm_state
        );

        let consensus_constants = self.consensus_constants();

        match self.sm_state {
            StateMachine::WaitingConsensus => {
                // In WaitingConsensus state, only allow AddBlocks when the argument is
                // the genesis block
                if msg.blocks.len() == 1 && msg.blocks[0].hash() == consensus_constants.genesis_hash
                {
                    let block = msg.blocks.into_iter().next().unwrap();
                    match self.process_requested_block(ctx, block) {
                        Ok(()) => {
                            log::debug!("Successfully consolidated genesis block");

                            // Set last beacon because otherwise the network cannot bootstrap
                            let sessions_manager = SessionsManager::from_registry();
                            let last_beacon = LastBeacon {
                                highest_block_checkpoint: self.get_chain_beacon(),
                                highest_superblock_checkpoint: self.get_superblock_beacon(),
                            };
                            sessions_manager.do_send(SetLastBeacon {
                                beacon: last_beacon,
                            });
                        }
                        Err(e) => log::error!("Failed to consolidate genesis block: {}", e),
                    }
                }
            }
            StateMachine::Synchronizing => {
                if let Some(target_beacon) = self.target_beacon {
                    let mut batch_succeeded = true;
                    let chain_beacon = self.get_chain_beacon();
                    let superblock_period = u32::from(consensus_constants.superblock_period);
                    if msg.blocks.is_empty() {
                        batch_succeeded = false;
                        log::debug!("Received an empty AddBlocks message");
                    // FIXME(#684): this condition would be modified when genesis block exist
                    } else if chain_beacon.hash_prev_block != consensus_constants.bootstrap_hash
                        && msg.blocks[0].hash() != chain_beacon.hash_prev_block
                        && msg.blocks[0].block_header.beacon.checkpoint == chain_beacon.checkpoint
                    {
                        // Fork case
                        batch_succeeded = false;
                        log::error!("Mismatching blocks, fork detected");
                        self.initialize_from_storage(ctx);
                        log::info!("Restored chain state from storage");
                    } else {
                        // FIXME(#684): this condition would be deleted when genesis block exist
                        let blocks = if chain_beacon.hash_prev_block
                            == consensus_constants.bootstrap_hash
                            || msg.blocks[0].block_header.beacon.checkpoint
                                > chain_beacon.checkpoint
                        {
                            &msg.blocks[..]
                        } else {
                            &msg.blocks[1..]
                        };

                        for block in blocks.iter() {
                            // Update reputation before checking Proof-of-Eligibility
                            let block_epoch = block.block_header.beacon.checkpoint;

                            // Construct superblock upon consolidation of the first block that took place after
                            // the initial checkpoint of the next superblock
                            let next_superblock_index =
                                self.chain_state.superblock_state.get_beacon().checkpoint + 1;
                            if block_epoch >= next_superblock_index * superblock_period {
                                // Construct superblocks while synchronizing but do not broadcast them
                                // This is needed to ensure that we can validate the received superblocks later on
                                self.construct_superblock(ctx, block_epoch)
                                    .and_then(move |_, _act, _ctx| actix::fut::ok(()))
                                    .wait(ctx);
                            }

                            if let Err(e) = self.process_requested_block(ctx, block.clone()) {
                                log::error!("Error processing block: {}", e);
                                self.initialize_from_storage(ctx);
                                log::info!("Restored chain state from storage");
                                batch_succeeded = false;
                                break;
                            }

                            let beacon = self.get_chain_beacon();
                            show_sync_progress(
                                beacon,
                                target_beacon,
                                self.epoch_constants.unwrap(),
                            );

                            if beacon == target_beacon {
                                break;
                            }
                        }
                    }

                    if batch_succeeded {
                        self.persist_blocks_batch(ctx, msg.blocks, target_beacon);
                        let to_be_stored =
                            self.chain_state.data_request_pool.finished_data_requests();
                        self.persist_data_requests(ctx, to_be_stored);
                        self.persist_chain_state(ctx);

                        let beacon = self.get_chain_beacon();

                        if beacon == target_beacon {
                            // Target achived, go back to state 1
                            self.sm_state = StateMachine::WaitingConsensus;
                        } else {
                            self.request_blocks_batch(ctx);
                        }
                    } else {
                        // This branch will happen if this node has forked, but the network has
                        // a valid consensus. In that case we would want to restore the node to
                        // the state just before the fork, and restart the synchronization.

                        // This branch could also happen when one peer has sent us an invalid block batch.
                        // Ideally we would mark it as a bad peer and restart the
                        // synchronization process, but that's not implemented yet.
                        // Note that in order to correctly restart the synchronization process,
                        // restoring the chain state from storage is not enough,
                        // as that storage was overwritten at the end of the last successful batch.

                        // In any case, the current behavior is to go back to WaitingConsensus
                        // state and restart the synchronization on the next PeersBeacons message.
                        self.sm_state = StateMachine::WaitingConsensus;
                    }
                } else {
                    log::warn!("Target Beacon is None");
                }
            }
            StateMachine::AlmostSynced | StateMachine::Synced => {}
        };

        // If we are not synchronizing, forget about when we started synchronizing
        if self.sm_state != StateMachine::Synchronizing {
            self.sync_waiting_for_add_blocks_since = None;
        }
    }
}

/// Handler for AddCandidates message
impl Handler<AddCandidates> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, msg: AddCandidates, _ctx: &mut Context<Self>) {
        // AddCandidates is needed in all states
        for block in msg.blocks {
            self.process_candidate(block);
        }
    }
}

/// Handler for AddSuperBlockVote message
impl Handler<AddSuperBlockVote> for ChainManager {
    type Result = Result<(), failure::Error>;

    fn handle(
        &mut self,
        AddSuperBlockVote { superblock_vote }: AddSuperBlockVote,
        ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.add_superblock_vote(superblock_vote, ctx)
    }
}

/// Handler for AddTransaction message
impl Handler<AddTransaction> for ChainManager {
    type Result = ResponseActFuture<Self, (), failure::Error>;

    fn handle(&mut self, msg: AddTransaction, _ctx: &mut Context<Self>) -> Self::Result {
        let timestamp_now = get_timestamp();
        self.add_transaction(msg, timestamp_now)
    }
}

/// Handler for GetBlocksEpochRange
impl Handler<GetBlocksEpochRange> for ChainManager {
    type Result = Result<Vec<(Epoch, Hash)>, ChainManagerError>;

    fn handle(
        &mut self,
        GetBlocksEpochRange {
            range,
            limit,
            limit_from_end,
        }: GetBlocksEpochRange,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        log::debug!("GetBlocksEpochRange received {:?}", range);

        // Accept this message in any state
        // TODO: we should only accept this message in Synced state, but that breaks the
        // JSON-RPC getBlockChain method

        // Iterator over all the blocks in the given range
        let block_chain_range = self
            .chain_state
            .block_chain
            .range(range)
            .map(|(k, v)| (*k, *v));

        if limit == 0 {
            // Return all the blocks from this epoch range
            let hashes: Vec<(Epoch, Hash)> = block_chain_range.collect();

            Ok(hashes)
        } else if limit_from_end {
            let mut hashes: Vec<(Epoch, Hash)> = block_chain_range
                // Take the last "limit" blocks
                .rev()
                .take(limit)
                .collect();

            // Reverse again to return them in non-reversed order
            hashes.reverse();

            Ok(hashes)
        } else {
            let hashes: Vec<(Epoch, Hash)> = block_chain_range
                // Take the first "limit" blocks
                .take(limit)
                .collect();

            Ok(hashes)
        }
    }
}

impl PeersBeacons {
    /// Pretty-print a map {beacon: [peers]}
    pub fn pretty_format(&self) -> String {
        let mut beacon_peers_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in self.pb.iter() {
            let v = v
                .map(|x| format!("#{} {}", x.checkpoint, x.hash_prev_block))
                .unwrap_or_else(|| "NO BEACON".to_string());
            beacon_peers_map.entry(v).or_default().push(k.to_string());
        }

        format!("{:?}", beacon_peers_map)
    }

    /// Run the consensus on the beacons, will return the most common beacon.
    /// We do not take into account our beacon to calculate the consensus.
    /// The beacons are `Option<CheckpointBeacon>`, so peers that have not
    /// sent us a beacon are counted as `None`. Keeping that in mind, we
    /// reach consensus as long as consensus_threshold % of peers agree.
    pub fn consensus(&self, consensus_threshold: usize) -> Option<CheckpointBeacon> {
        // We need to add `num_missing_peers` times NO BEACON, to take into account
        // missing outbound peers.
        let num_missing_peers = self.outbound_limit
            .map(|outbound_limit| {
                // TODO: is it possible to receive more than outbound_limit beacons?
                // (it shouldn't be possible)
                assert!(self.pb.len() <= outbound_limit as usize, "Received more beacons than the outbound_limit. Check the code for race conditions.");
                usize::try_from(outbound_limit).unwrap() - self.pb.len()
            })
            // The outbound limit is set when the SessionsManager actor is initialized, so here it
            // cannot be None. But if it is None, set num_missing_peers to 0 in order to calculate
            // consensus with the existing beacons.
            .unwrap_or(0);

        mode_consensus(
            self.pb
                .iter()
                .map(|(_p, b)| b)
                .chain(std::iter::repeat(&None).take(num_missing_peers)),
            consensus_threshold,
        )
        // Flatten result:
        // None (consensus % below threshold) should be treated the same way as
        // Some(None) (most of the peers did not send a beacon)
        .and_then(|x| *x)
    }

    /// Collects the peers to unregister based on the beacon they reported and the beacon to be compared it with
    pub fn decide_peers_to_unregister(&self, beacon: CheckpointBeacon) -> Vec<SocketAddr> {
        // Unregister peers which have a different beacon
        (&self.pb)
            .iter()
            .filter_map(|(p, b)| if *b != Some(beacon) { Some(*p) } else { None })
            .collect()
    }
}

impl Handler<PeersBeacons> for ChainManager {
    type Result = <PeersBeacons as Message>::Result;

    // FIXME(#676): Remove clippy skip error
    #[allow(clippy::cognitive_complexity)]
    fn handle(&mut self, peers_beacons: PeersBeacons, ctx: &mut Context<Self>) -> Self::Result {
        log::debug!(
            "PeersBeacons received while StateMachine is in state {:?}",
            self.sm_state
        );

        log::debug!("Received beacons: {}", peers_beacons.pretty_format());

        // Activate peers beacons index to continue synced
        if !peers_beacons.pb.is_empty() {
            self.peers_beacons_received = true;
        }

        // Calculate the consensus, or None if there is no consensus
        let consensus_threshold = self.consensus_c as usize;
        let consensus = peers_beacons.consensus(consensus_threshold);
        let outbound_limit = peers_beacons.outbound_limit;
        let pb_len = peers_beacons.pb.len();
        let peers_needed_for_consensus = outbound_limit
            .map(|x| {
                // ceil(x * consensus_threshold / 100)
                (usize::from(x) * consensus_threshold + 99) / 100
            })
            .unwrap_or(1);
        let peers_to_unregister = if let Some(consensus_beacon) = consensus {
            peers_beacons.decide_peers_to_unregister(consensus_beacon)
        } else if pb_len < peers_needed_for_consensus {
            // Not enough outbound peers, do not unregister any peers
            log::debug!(
                "Got {} peers but need at least {} to calculate the consensus",
                pb_len,
                peers_needed_for_consensus
            );
            vec![]
        } else {
            // No consensus: if state is AlmostSynced unregister those that are not coincident with ours.
            // Else, unregister all peers
            if self.sm_state == StateMachine::AlmostSynced || self.sm_state == StateMachine::Synced
            {
                log::warn!("Lack of peer consensus while state is `AlmostSynced`: peers that do not coincide with our last beacon will be unregistered");
                peers_beacons.decide_peers_to_unregister(self.get_chain_beacon())
            } else {
                log::warn!("Lack of peer consensus: all peers will be unregistered");
                peers_beacons.pb.into_iter().map(|(p, _b)| p).collect()
            }
        };

        let peers_to_unregister = match self.sm_state {
            StateMachine::WaitingConsensus => {
                // As soon as there is consensus, we set the target beacon to the consensus
                // and set the state to Synchronizing
                if let Some(consensus_beacon) = consensus {
                    self.target_beacon = Some(consensus_beacon);

                    let our_beacon = self.get_chain_beacon();

                    let consensus_constants = self.consensus_constants();

                    // Check if we are already synchronized
                    self.sm_state = if consensus_beacon.hash_prev_block
                        == consensus_constants.bootstrap_hash
                    {
                        log::debug!("The consensus is that there is no genesis block yet");

                        StateMachine::WaitingConsensus
                    } else if our_beacon == consensus_beacon {
                        StateMachine::AlmostSynced
                    } else if our_beacon.checkpoint == consensus_beacon.checkpoint
                        && our_beacon.hash_prev_block != consensus_beacon.hash_prev_block
                    {
                        // Fork case
                        log::warn!(
                            "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                            our_beacon,
                            consensus_beacon
                        );

                        self.initialize_from_storage(ctx);
                        log::info!("Restored chain state from storage");

                        StateMachine::WaitingConsensus
                    } else {
                        // Review candidates
                        let consensus_block_hash = consensus_beacon.hash_prev_block;
                        let candidate = self.candidates.remove(&consensus_block_hash);
                        // Clear candidates, as they are only valid for one epoch
                        self.candidates.clear();
                        self.seen_candidates.clear();
                        // TODO: Be functional my friend
                        if let Some(consensus_block) = candidate {
                            match self.process_requested_block(ctx, consensus_block) {
                                Ok(()) => {
                                    log::info!(
                                        "Consolidate consensus candidate. AlmostSynced state"
                                    );
                                    StateMachine::AlmostSynced
                                }
                                Err(e) => {
                                    log::debug!("Failed to consolidate consensus candidate: {}", e);

                                    self.request_blocks_batch(ctx);

                                    StateMachine::Synchronizing
                                }
                            }
                        } else {
                            self.request_blocks_batch(ctx);

                            StateMachine::Synchronizing
                        }
                    };

                    Ok(peers_to_unregister)
                } else {
                    Ok(peers_to_unregister)
                }
            }
            StateMachine::Synchronizing => {
                if let Some(consensus_beacon) = consensus {
                    self.target_beacon = Some(consensus_beacon);

                    let our_beacon = self.get_chain_beacon();

                    // Check if we are already synchronized
                    self.sm_state = if our_beacon == consensus_beacon {
                        StateMachine::AlmostSynced
                    } else if our_beacon.checkpoint == consensus_beacon.checkpoint
                        && our_beacon.hash_prev_block != consensus_beacon.hash_prev_block
                    {
                        // Fork case
                        log::warn!(
                            "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                            our_beacon,
                            consensus_beacon
                        );

                        self.initialize_from_storage(ctx);
                        log::info!("Restored chain state from storage");

                        StateMachine::WaitingConsensus
                    } else {
                        StateMachine::Synchronizing
                    };

                    Ok(peers_to_unregister)
                } else {
                    // Move to waiting consensus stage
                    self.sm_state = StateMachine::WaitingConsensus;

                    Ok(peers_to_unregister)
                }
            }
            StateMachine::AlmostSynced | StateMachine::Synced => {
                let our_beacon = self.get_chain_beacon();
                match consensus {
                    Some(consensus_beacon) if consensus_beacon == our_beacon => {
                        if self.sm_state == StateMachine::AlmostSynced {
                            // This is the only point in the whole base code for the state
                            // machine to move into `Synced` state.
                            log::debug!("Moving from AlmostSynced to Synced state");
                            log::info!("{}", SYNCED_BANNER);
                            self.sm_state = StateMachine::Synced;
                        }
                        Ok(peers_to_unregister)
                    }
                    Some(_) => {
                        // We are out of consensus!
                        log::warn!(
                            "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                            our_beacon,
                            consensus
                        );

                        self.initialize_from_storage(ctx);
                        log::info!("Restored chain state from storage");

                        self.sm_state = StateMachine::WaitingConsensus;

                        Ok(peers_to_unregister)
                    }
                    None => {
                        // If we are synced and the consensus beacon is not the same as our beacon, then
                        // we need to rewind one epoch
                        if pb_len == 0 {
                            log::warn!(
                                "[CONSENSUS]: We have not received any beacons for this epoch"
                            );
                        } else {
                            // There is no consensus because of a tie, do not rewind?
                            // For example this could happen when each peer reports a different beacon...
                            log::warn!(
                                "[CONSENSUS]: We are on {:?} but the network has no consensus",
                                our_beacon
                            );
                        }
                        self.sm_state = StateMachine::AlmostSynced;

                        Ok(peers_to_unregister)
                    }
                }
            }
        };

        if self.sm_state == StateMachine::Synchronizing {
            if let Some(sync_start_epoch) = self.sync_waiting_for_add_blocks_since {
                let current_epoch = self.current_epoch.unwrap();
                let how_many_epochs_are_we_willing_to_wait_for_one_block_batch = 10;
                if current_epoch - sync_start_epoch
                    >= how_many_epochs_are_we_willing_to_wait_for_one_block_batch
                {
                    log::warn!("Timeout for waiting for blocks achieved. Requesting blocks again.");
                    self.request_blocks_batch(ctx);
                }
            }
        }

        peers_to_unregister
    }
}

impl Handler<BuildVtt> for ChainManager {
    type Result = ResponseActFuture<Self, Hash, failure::Error>;

    fn handle(&mut self, msg: BuildVtt, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Box::new(actix::fut::err(
                ChainManagerError::NotSynced {
                    current_state: self.sm_state,
                }
                .into(),
            ));
        }
        let timestamp = u64::try_from(get_timestamp()).unwrap();
        match transaction_factory::build_vtt(
            msg.vto,
            msg.fee,
            &mut self.chain_state.own_utxos,
            self.own_pkh.unwrap(),
            &self.chain_state.unspent_outputs_pool,
            timestamp,
            self.tx_pending_timeout,
            msg.utxo_strategy,
        ) {
            Err(e) => {
                log::error!("Error when building value transfer transaction: {}", e);
                Box::new(actix::fut::err(e.into()))
            }
            Ok(vtt) => {
                let fut = transaction_factory::sign_transaction(&vtt, vtt.inputs.len())
                    .into_actor(self)
                    .then(|s, act, ctx| match s {
                        Ok(signatures) => {
                            let transaction =
                                Transaction::ValueTransfer(VTTransaction::new(vtt, signatures));
                            let tx_hash = transaction.hash();
                            Box::new(
                                act.handle(AddTransaction { transaction }, ctx)
                                    .map(move |_, _, _| tx_hash),
                            )
                        }
                        Err(e) => {
                            log::error!("Failed to sign value transfer transaction: {}", e);

                            let res: Box<
                                dyn ActorFuture<
                                    Item = Hash,
                                    Error = failure::Error,
                                    Actor = ChainManager,
                                >,
                            > = Box::new(actix::fut::err(e));
                            res
                        }
                    });

                Box::new(fut)
            }
        }
    }
}

impl Handler<BuildDrt> for ChainManager {
    type Result = ResponseActFuture<Self, Hash, failure::Error>;

    fn handle(&mut self, msg: BuildDrt, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Box::new(actix::fut::err(
                ChainManagerError::NotSynced {
                    current_state: self.sm_state,
                }
                .into(),
            ));
        }
        if let Err(e) = validate_rad_request(&msg.dro.data_request) {
            return Box::new(actix::fut::err(e));
        }
        let timestamp = u64::try_from(get_timestamp()).unwrap();
        match transaction_factory::build_drt(
            msg.dro,
            msg.fee,
            &mut self.chain_state.own_utxos,
            self.own_pkh.unwrap(),
            &self.chain_state.unspent_outputs_pool,
            timestamp,
            self.tx_pending_timeout,
        ) {
            Err(e) => {
                log::error!("Error when building data request transaction: {}", e);
                Box::new(actix::fut::err(e.into()))
            }
            Ok(drt) => {
                log::debug!("Created drt:\n{:?}", drt);
                let fut = transaction_factory::sign_transaction(&drt, drt.inputs.len())
                    .into_actor(self)
                    .then(|s, act, ctx| match s {
                        Ok(signatures) => {
                            let transaction =
                                Transaction::DataRequest(DRTransaction::new(drt, signatures));
                            let tx_hash = transaction.hash();
                            Box::new(
                                act.handle(AddTransaction { transaction }, ctx)
                                    .map(move |_, _, _| tx_hash),
                            )
                        }
                        Err(e) => {
                            log::error!("Failed to sign data request transaction: {}", e);

                            let res: Box<
                                dyn ActorFuture<
                                    Item = Hash,
                                    Error = failure::Error,
                                    Actor = ChainManager,
                                >,
                            > = Box::new(actix::fut::err(e));
                            res
                        }
                    });

                Box::new(fut)
            }
        }
    }
}

impl Handler<GetState> for ChainManager {
    type Result = <GetState as Message>::Result;

    fn handle(&mut self, _msg: GetState, _ctx: &mut Self::Context) -> Self::Result {
        Ok(self.sm_state)
    }
}

impl Handler<GetDataRequestReport> for ChainManager {
    type Result = ResponseFuture<DataRequestInfo, failure::Error>;

    fn handle(&mut self, msg: GetDataRequestReport, _ctx: &mut Self::Context) -> Self::Result {
        let dr_pointer = msg.dr_pointer;

        // First, try to get it from memory
        if let Some(dr_info) = self
            .chain_state
            .data_request_pool
            .data_request_pool
            .get(&dr_pointer)
            .map(|dr_state| dr_state.info.clone())
        {
            Box::new(futures::finished(dr_info))
        } else {
            let dr_pointer_string = format!("DR-REPORT-{}", dr_pointer);
            // Otherwise, try to get it from storage
            let fut = storage_mngr::get::<_, DataRequestReport>(&dr_pointer_string).and_then(
                move |dr_report| match dr_report {
                    Some(x) => futures::finished(DataRequestInfo::from(x)),
                    None => futures::failed(DataRequestNotFound { hash: dr_pointer }.into()),
                },
            );

            Box::new(fut)
        }
    }
}

impl Handler<GetBalance> for ChainManager {
    type Result = Result<u64, failure::Error>;

    fn handle(&mut self, GetBalance { pkh }: GetBalance, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        Ok(transaction_factory::get_total_balance(
            &self.chain_state.unspent_outputs_pool,
            pkh,
        ))
    }
}

impl Handler<GetUtxoInfo> for ChainManager {
    type Result = Result<UtxoInfo, failure::Error>;

    fn handle(
        &mut self,
        GetUtxoInfo { pkh }: GetUtxoInfo,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let chain_info = self.chain_state.chain_info.as_ref().unwrap();
        let block_number_limit = self
            .chain_state
            .block_number()
            .saturating_sub(chain_info.consensus_constants.collateral_age);

        let pkh = if self.own_pkh == Some(pkh) {
            None
        } else {
            Some(pkh)
        };

        Ok(get_utxo_info(
            pkh,
            &self.chain_state.own_utxos,
            &self.chain_state.unspent_outputs_pool,
            chain_info.consensus_constants.collateral_minimum,
            block_number_limit,
        ))
    }
}

impl Handler<GetReputation> for ChainManager {
    type Result = Result<(Reputation, bool), failure::Error>;

    fn handle(
        &mut self,
        GetReputation { pkh }: GetReputation,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        Ok((rep_eng.trs().get(&pkh), rep_eng.ars().contains(&pkh)))
    }
}

impl Handler<GetReputationAll> for ChainManager {
    type Result = Result<HashMap<PublicKeyHash, (Reputation, bool)>, failure::Error>;

    fn handle(&mut self, _msg: GetReputationAll, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        Ok(rep_eng
            .trs()
            .identities()
            .map(|(k, v)| (*k, (*v, rep_eng.ars().contains(k))))
            .collect())
    }
}

impl Handler<GetReputationStatus> for ChainManager {
    type Result = Result<GetReputationStatusResult, failure::Error>;

    fn handle(&mut self, _msg: GetReputationStatus, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        let num_active_identities = u32::try_from(rep_eng.ars().active_identities_number())?;
        let total_active_reputation = rep_eng.total_active_reputation();

        Ok(GetReputationStatusResult {
            num_active_identities,
            total_active_reputation,
        })
    }
}

impl Handler<TryMineBlock> for ChainManager {
    type Result = ();

    fn handle(&mut self, _msg: TryMineBlock, ctx: &mut Self::Context) -> Self::Result {
        self.try_mine_block(ctx);
    }
}

impl Handler<AddCommitReveal> for ChainManager {
    type Result = ResponseActFuture<Self, (), failure::Error>;

    fn handle(
        &mut self,
        AddCommitReveal {
            commit_transaction,
            reveal_transaction,
        }: AddCommitReveal,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        let dr_pointer = commit_transaction.body.dr_pointer;
        // Hold reveal transaction under "waiting_for_reveal" field of data requests pool
        self.chain_state
            .data_request_pool
            .insert_reveal(dr_pointer, reveal_transaction);

        // Send AddTransaction message to self
        // And broadcast it to all of peers
        Box::new(
            self.handle(
                AddTransaction {
                    transaction: Transaction::Commit(commit_transaction),
                },
                ctx,
            )
            .map_err(|e, _, _| {
                log::warn!("Failed to add commit transaction: {}", e);
                e
            }),
        )
    }
}

impl Handler<GetMemoryTransaction> for ChainManager {
    type Result = Result<Transaction, ()>;

    fn handle(&mut self, msg: GetMemoryTransaction, _ctx: &mut Self::Context) -> Self::Result {
        self.transactions_pool.get(&msg.hash).ok_or(())
    }
}

impl Handler<GetMempool> for ChainManager {
    type Result = Result<GetMempoolResult, failure::Error>;

    fn handle(&mut self, _msg: GetMempool, _ctx: &mut Self::Context) -> Self::Result {
        let res = GetMempoolResult {
            value_transfer: self.transactions_pool.vt_iter().map(|t| t.hash()).collect(),
            data_request: self.transactions_pool.dr_iter().map(|t| t.hash()).collect(),
        };

        Ok(res)
    }
}

// TODO: USE ME!
#[allow(dead_code)]
#[derive(Debug, Eq, PartialEq)]
pub enum BlockBatches<T> {
    TargetNotReached(Vec<T>),
    SyncWithoutCandidate(Vec<T>, Vec<T>),
    SyncWithCandidate(Vec<T>, Vec<T>, Vec<T>),
}

// TODO: return slices instead of vectors?
// TODO: USE ME!
#[allow(dead_code)]
fn split_blocks_batch_at_target<T, F>(
    key: F,
    blocks: Vec<T>,
    current_epoch: u32,
    sync_target: &SyncTarget,
    superblock_period: u32,
) -> Result<BlockBatches<T>, ChainManagerError>
where
    F: Fn(&T) -> u32 + Copy,
{
    use BlockBatches::*;

    let current_superblock_index = current_epoch / superblock_period;
    assert!(
        current_superblock_index >= sync_target.superblock.checkpoint,
        "Provided a sync target that is in the future"
    );

    // If the chain reverted, this function cannot receive blocks from between the reverted epochs
    let first_valid_block = (current_superblock_index
        - ((current_superblock_index - sync_target.superblock.checkpoint) % 2))
        * superblock_period;

    let wrong_index = blocks.iter().position(|block| {
        key(block) >= sync_target.superblock.checkpoint * superblock_period
            && key(block) < first_valid_block
    });
    if let Some(wrong_index) = wrong_index {
        // We received blocks that do not match the current epoch and the last consolidated superblock.
        // As an example, if the last consolidated superblock has the block 9 inside, and we are in epoch 50,
        // it means we reverted somehow. Thus Blocks between 10 and 49 cannot exist.
        return Err(ChainManagerError::WrongBlocksForSuperblock {
            wrong_index: key(&blocks[wrong_index]),
            consolidated_superblock_index: sync_target.superblock.checkpoint,
            current_superblock_index,
        });
    }

    // The case where blocks is an empty array
    let last_epoch = blocks.last().map(key).unwrap_or(0);

    if last_epoch < ((sync_target.superblock.checkpoint * superblock_period).saturating_sub(1))
        && last_epoch < sync_target.block.checkpoint
    {
        return Ok(TargetNotReached(blocks));
    }

    if (current_superblock_index - sync_target.superblock.checkpoint) % 2 == 0 {
        let consolidated_blocks_target = sync_target.superblock.checkpoint * superblock_period;
        let mut consolidated_blocks = blocks;
        let mut remaining_blocks = vec![];
        let split_position = consolidated_blocks
            .iter()
            .position(|block| key(block) >= consolidated_blocks_target);
        if let Some(split_position) = split_position {
            remaining_blocks = consolidated_blocks.split_off(split_position);
        }

        return Ok(SyncWithoutCandidate(consolidated_blocks, remaining_blocks));
    }

    let (consolidated_blocks_target, candidate_blocks_target) = (
        sync_target.superblock.checkpoint * superblock_period,
        current_superblock_index * superblock_period,
    );
    let mut consolidated_blocks = blocks;
    let candidate_split_position = consolidated_blocks
        .iter()
        .position(|block| key(block) >= consolidated_blocks_target);

    let mut candidate_blocks = vec![];
    let mut remaining_blocks = vec![];

    if let Some(candidate_split_position) = candidate_split_position {
        candidate_blocks = consolidated_blocks.split_off(candidate_split_position);
    }

    let remaining_split_position = candidate_blocks
        .iter()
        .position(|block| key(block) >= candidate_blocks_target);
    if let Some(remaining_split_position) = remaining_split_position {
        remaining_blocks = candidate_blocks.split_off(remaining_split_position);
    }

    Ok(SyncWithCandidate(
        consolidated_blocks,
        candidate_blocks,
        remaining_blocks,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_beacons_consensus_less_peers_than_outbound() {
        let beacon1 = CheckpointBeacon {
            checkpoint: 1,
            hash_prev_block: "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"
                .parse()
                .unwrap(),
        };
        let beacon2 = CheckpointBeacon {
            checkpoint: 1,
            hash_prev_block: "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35"
                .parse()
                .unwrap(),
        };

        // 0 peers
        let peers_beacons = PeersBeacons {
            pb: vec![],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), None);

        // 1 peer
        let peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1))],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), None);

        // 2 peers
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), None);

        // 3 peers and 2 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2)),
            ],
            outbound_limit: Some(4),
        };
        // Note that the consensus % includes the missing peers,
        // so in this case it is 2/4 (50%), not 2/3 (66%), so there is no consensus for 60%
        assert_eq!(peers_beacons.consensus(60), None);

        // 3 peers and 3 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), Some(beacon1));

        // 4 peers and 2 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2)),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), None);

        // 4 peers and 3 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), Some(beacon1));

        // 4 peers and 4 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon1)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.consensus(60), Some(beacon1));
    }

    #[test]
    fn test_unregister_peers() {
        let beacon1 = CheckpointBeacon {
            checkpoint: 1,
            hash_prev_block: "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"
                .parse()
                .unwrap(),
        };
        let beacon2 = CheckpointBeacon {
            checkpoint: 1,
            hash_prev_block: "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35"
                .parse()
                .unwrap(),
        };

        // 0 peers
        let mut peers_beacons = PeersBeacons {
            pb: vec![],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.decide_peers_to_unregister(beacon1), []);

        // 1 peer in consensus
        peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1))],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.decide_peers_to_unregister(beacon1), []);

        // 1 peer out of consensus
        peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1))],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon2),
            ["127.0.0.1:10001".parse().unwrap()]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2)),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon2),
            [
                "127.0.0.1:10001".parse().unwrap(),
                "127.0.0.1:10002".parse().unwrap()
            ]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1)),
                ("127.0.0.1:10003".parse().unwrap(), None),
                ("127.0.0.1:10004".parse().unwrap(), None),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1),
            [
                "127.0.0.1:10003".parse().unwrap(),
                "127.0.0.1:10004".parse().unwrap()
            ]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), None),
                ("127.0.0.1:10002".parse().unwrap(), None),
                ("127.0.0.1:10003".parse().unwrap(), None),
                ("127.0.0.1:10004".parse().unwrap(), None),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1),
            [
                "127.0.0.1:10001".parse().unwrap(),
                "127.0.0.1:10002".parse().unwrap(),
                "127.0.0.1:10003".parse().unwrap(),
                "127.0.0.1:10004".parse().unwrap()
            ]
        );
    }
    #[test]
    fn test_split_blocks_batch() {
        use BlockBatches::*;
        let mut sync_target = SyncTarget {
            block: Default::default(),
            superblock: Default::default(),
        };
        let superblock_period = 10;

        let test_split_batch = |provided_blocks, epoch, sync_target: &SyncTarget| {
            split_blocks_batch_at_target(
                |x| *x,
                provided_blocks,
                epoch,
                &sync_target.clone(),
                superblock_period,
            )
        };

        assert_eq!(
            test_split_batch(vec![], 1, &sync_target),
            Ok(SyncWithoutCandidate(vec![], vec![]))
        );
        assert_eq!(
            test_split_batch(vec![0], 1, &sync_target),
            Ok(SyncWithoutCandidate(vec![], vec![0]))
        );
        assert_eq!(
            test_split_batch(vec![0, 8], 9, &sync_target),
            Ok(SyncWithoutCandidate(vec![], vec![0, 8]))
        );
        assert_eq!(
            test_split_batch(vec![0, 9], 11, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![0, 9], vec![]))
        );

        assert_eq!(
            test_split_batch(vec![0, 10], 11, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![0], vec![10]))
        );

        sync_target.superblock.checkpoint = 1;

        assert_eq!(
            test_split_batch(vec![0, 9], 21, &sync_target),
            Ok(SyncWithCandidate(vec![0, 9], vec![], vec![]))
        );
        assert_eq!(
            test_split_batch(vec![0, 10], 21, &sync_target),
            Ok(SyncWithCandidate(vec![0], vec![10], vec![]))
        );
        assert_eq!(
            test_split_batch(vec![0, 8, 11], 21, &sync_target),
            Ok(SyncWithCandidate(vec![0, 8], vec![11], vec![]))
        );
        assert_eq!(
            test_split_batch(vec![0, 9, 10, 18, 26], 29, &sync_target),
            Ok(SyncWithCandidate(vec![0, 9], vec![10, 18], vec![26]))
        );
        assert_eq!(
            test_split_batch(vec![0, 9, 10, 19], 21, &sync_target,),
            Ok(SyncWithCandidate(vec![0, 9], vec![10, 19], vec![]))
        );
        assert_eq!(
            test_split_batch(vec![0, 10, 20], 21, &sync_target),
            Ok(SyncWithCandidate(vec![0], vec![10], vec![20]))
        );
        assert_eq!(
            test_split_batch(vec![0, 9, 10, 19, 20, 21], 22, &sync_target,),
            Ok(SyncWithCandidate(vec![0, 9], vec![10, 19], vec![20, 21]))
        );

        sync_target.superblock.checkpoint = 2;
        assert_eq!(
            test_split_batch(vec![100], 101, &sync_target),
            Ok(SyncWithoutCandidate(vec![], vec![100]))
        );

        assert_eq!(
            test_split_batch(vec![110], 111, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![], vec![110]))
        );

        assert_eq!(
            test_split_batch(vec![105, 110], 111, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![105], vec![110]))
        );

        assert_eq!(
            test_split_batch(vec![], 111, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![], vec![]))
        );

        assert_eq!(
            test_split_batch(vec![], 111, &sync_target),
            Ok(SyncWithCandidate(vec![], vec![], vec![]))
        );

        assert_eq!(
            test_split_batch(vec![1, 8, 18, 108, 110], 111, &sync_target),
            Ok(SyncWithCandidate(vec![1, 8, 18], vec![108], vec![110]))
        );

        sync_target.superblock.checkpoint = 3;
        assert_eq!(
            test_split_batch(vec![1, 8, 18, 70, 100], 101, &sync_target),
            (Err(ChainManagerError::WrongBlocksForSuperblock {
                wrong_index: 70,
                consolidated_superblock_index: 3,
                current_superblock_index: 10
            }))
        );

        sync_target.superblock.checkpoint = 10;
        sync_target.block.checkpoint = 99;

        assert_eq!(
            test_split_batch(vec![1, 8, 18], 101, &sync_target),
            Ok(TargetNotReached(vec![1, 8, 18]))
        );
    }
}
