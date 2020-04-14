use actix::prelude::*;

use super::{handlers::EveryEpochPayload, ChainManager};
use crate::{
    actors::{
        epoch_manager::{EpochManager, EpochManagerError::CheckpointZeroInTheFuture},
        messages::{AddBlocks, GetEpoch, GetEpochConstants, Subscribe},
        storage_keys,
    },
    config_mngr, signature_mngr, storage_mngr,
};
use witnet_data_structures::{
    chain::{ChainInfo, ChainState, CheckpointBeacon, GenesisBlockInfo, ReputationEngine},
    vrf::VrfCtx,
};

use witnet_util::timestamp::pretty_print;

use log::{debug, error, info, warn};
use std::time::Duration;
use witnet_crypto::key::CryptoEngine;

/// Implement Actor trait for `ChainManager`
impl Actor for ChainManager {
    /// Every actor has to provide execution `Context` in which it can run
    type Context = Context<Self>;

    /// Method to be executed when the actor is started
    fn started(&mut self, ctx: &mut Self::Context) {
        debug!("ChainManager actor has been started!");

        self.initialize_from_storage(ctx);

        self.subscribe_to_epoch_manager(ctx);

        self.get_pkh(ctx);

        self.vrf_ctx = VrfCtx::secp256k1()
            .map_err(|e| {
                error!("Failed to create VRF context: {}", e);
                // Stop the node
                ctx.stop();
            })
            .ok();

        self.secp = Some(CryptoEngine::new());
    }
}

impl ChainManager {
    /// Get configuration from ConfigManager and try to initialize ChainManager state from Storage
    /// (initialize to Default values if empty)
    pub fn initialize_from_storage(&mut self, ctx: &mut Context<ChainManager>) {
        config_mngr::get()
            .into_actor(self)
            .map_err(|err, _act, _ctx| {
                log::error!("Couldn't get config: {}", err);
            })
            .and_then(|config, act, _ctx| {
                let consensus_constants = config.consensus_constants.clone();

                if config.mining.data_request_timeout == Duration::new(0, 0) {
                    act.data_request_timeout = None;
                } else {
                    act.data_request_timeout = Some(config.mining.data_request_timeout);
                }

                // Set the retrievals limit per epoch, as read from the configuration
                act.data_request_max_retrievals_per_epoch = config.mining.data_request_max_retrievals_per_epoch;

                act.tx_pending_timeout = config.mempool.tx_pending_timeout;

                let magic = consensus_constants.get_magic();
                act.set_magic(magic);

                // Do not start the MiningManager if the configuration disables it
                act.mining_enabled = config.mining.enabled;

                // Get consensus parameter from config
                act.consensus_c = config.connections.consensus_c;

                storage_mngr::get::<_, ChainState>(&storage_keys::chain_state_key(magic))
                    .into_actor(act)
                    .then(|chain_state_from_storage, _, _| {
                        let result = match chain_state_from_storage {
                            Ok(x) => (x, config),
                            Err(e) => {
                                error!("Error while getting chain state from storage: {}", e);
                                (None, config)
                            }
                        };

                        actix::fut::ok(result)
                    })
            })
            .map(move |(chain_state_from_storage, config), act, ctx| {
                // Get environment and consensus_constants parameters from config
                let environment = config.environment;
                let consensus_constants = &config.consensus_constants;
                // chain_info_from_storage can be None if the storage does not contain that key

                let chain_state = match chain_state_from_storage {
                    Some(
                        chain_state_from_storage @ ChainState {
                            chain_info: Some(..),
                            reputation_engine: Some(..),
                            ..
                        }
                    ) => {
                        let chain_info_from_storage =
                            chain_state_from_storage.chain_info.as_ref().unwrap();

                        if environment == chain_info_from_storage.environment {
                            if consensus_constants == &chain_info_from_storage.consensus_constants {
                                debug!("ChainInfo successfully obtained from storage");

                                chain_state_from_storage
                            } else {
                                // Mismatching consensus constants between config and storage
                                panic!(
                                    "Mismatching consensus constants: tried to run a node using \
                                     different consensus constants than the ones that were used when \
                                     the local chain was initialized.\nNode constants: {:#?}\nChain \
                                     constants: {:#?}",
                                    consensus_constants, chain_info_from_storage.consensus_constants
                                );
                            }
                        } else {
                            // Mismatching environment names between config and storage
                            panic!(
                                "Mismatching environments: tried to run a node on environment \
                                \"{:?}\" with a chain that was initialized with environment \
                                \"{:?}\".",
                                environment, chain_info_from_storage.environment
                            );
                        }
                    }
                    x => {
                        if x.is_some() {
                            debug!(
                                "Uninitialized local chain the ChainInfo in storage is incomplete. Proceeding to \
                                 initialize and store a new chain."
                            );
                        } else {
                            debug!(
                                "Uninitialized local chain (no ChainInfo in storage). Proceeding to \
                                 initialize and store a new chain."
                            );
                        }
                        // Create a new ChainInfo
                        let bootstrap_hash = consensus_constants.bootstrap_hash;
                        let reputation_engine = ReputationEngine::new(consensus_constants.activity_period as usize);
                        let hash_prev_block = bootstrap_hash;

                        let chain_info = ChainInfo {
                            environment,
                            consensus_constants: consensus_constants.clone(),
                            highest_block_checkpoint: CheckpointBeacon {
                                checkpoint: 0,
                                hash_prev_block,
                            },
                            highest_vrf_output: CheckpointBeacon {
                                checkpoint: 0,
                                hash_prev_block,
                            },
                        };

                        ChainState {
                            chain_info: Some(chain_info),
                            reputation_engine: Some(reputation_engine),
                            ..ChainState::default()
                        }
                    }
                };

                let chain_info = chain_state.chain_info.as_ref().unwrap();
                info!(
                    "Actual ChainState CheckpointBeacon: epoch ({}), hash_block ({})",
                    chain_info.highest_block_checkpoint.checkpoint,
                    chain_info.highest_block_checkpoint.hash_prev_block
                );

                // If hash_prev_block is the bootstrap hash, create and consolidate genesis block
                if chain_info.highest_block_checkpoint.hash_prev_block == consensus_constants.bootstrap_hash {
                    // Create genesis block
                    let info_genesis =
                        GenesisBlockInfo::from_path(&config.mining.genesis_path, consensus_constants.bootstrap_hash, consensus_constants.genesis_hash)
                            .map_err(|e| {
                                log::error!("Failed to create genesis block: {}", e);
                                log::error!("Genesis block could be downloaded in: https://github.com/witnet/genesis_block");
                                System::current().stop_with_code(1);
                            }).ok();

                    if let Some(ig) = info_genesis {
                        log::info!("Genesis block successfully created. Hash: {}", consensus_constants.genesis_hash);

                        let genesis_block = ig.build_genesis_block(consensus_constants.bootstrap_hash);
                        ctx.notify(AddBlocks {
                            blocks: vec![genesis_block],
                        });
                    }
                }

                act.chain_state = chain_state;
                act.last_chain_state = act.chain_state.clone();

                act.persist_chain_state(ctx);
            }).wait(ctx);
    }

    /// Get epoch constants and current epoch from EpochManager, and subscribe to future epochs
    fn subscribe_to_epoch_manager(&mut self, ctx: &mut Context<ChainManager>) {
        // Get EpochManager address from registry
        let epoch_manager_addr = EpochManager::from_registry();
        let epoch_manager_addr2 = epoch_manager_addr.clone();

        // Get epoch constants
        epoch_manager_addr.send(GetEpochConstants).into_actor(self).then(move |res, act, _ctx| {
            match res {
                Ok(f) => act.epoch_constants = f,
                error => error!("Failed to get epoch constants: {:?}", error),
            }

            epoch_manager_addr2
                // Send GetEpoch message to epoch manager actor
                // This returns a RequestFuture, representing an asynchronous message sending process
                .send(GetEpoch)
                // Convert a normal future into an ActorFuture
                .into_actor(act)
        })
            // Process the response from the EpochManager
            // This returns a FutureResult containing the socket address if present
            .then(move |res, act, ctx| {
                // Get ChainManager address
                let chain_manager_addr = ctx.address();

                // Check GetEpoch result
                match res {
                    Ok(Ok(epoch)) => {
                        // Subscribe to all epochs with an EveryEpochPayload
                        epoch_manager_addr
                            .do_send(Subscribe::to_all(chain_manager_addr, EveryEpochPayload));

                        // Set current_epoch
                        act.current_epoch = Some(epoch);
                    }
                    Ok(Err(CheckpointZeroInTheFuture(zero))) => {
                        let date = pretty_print(zero, 0);
                        warn!("Checkpoint zero is in the future ({:?}). Delaying chain bootstrapping until then.", date);

                        // Subscribe to all epochs with an EveryEpochPayload
                        epoch_manager_addr
                            .do_send(Subscribe::to_all(chain_manager_addr, EveryEpochPayload));
                    }
                    error => {
                        error!("Current epoch could not be retrieved from EpochManager: {:?}", error);
                    }
                }

                actix::fut::ok(())
            })
            .wait(ctx);
    }

    /// Load public key hash from signature manager
    fn get_pkh(&mut self, ctx: &mut Context<Self>) {
        signature_mngr::pkh()
            .into_actor(self)
            .map_err(|e, _act, _ctx| {
                error!(
                    "Error while getting public key hash from signature manager: {}",
                    e
                );
            })
            .and_then(|res, act, _ctx| {
                act.own_pkh = Some(res);
                debug!("Public key hash successfully loaded from signature manager");
                info!("PublicKeyHash: {}", res);
                actix::fut::ok(())
            })
            .wait(ctx);
    }
}
