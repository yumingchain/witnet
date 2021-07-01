use std::sync::Arc;

use actix::utils::TimerFunc;
use futures::FutureExt;

use witnet_crypto::mnemonic;
use witnet_data_structures::{
    chain::{Block, InventoryItem, RADRequest, StateMachine, SyncStatus},
    transaction::Transaction,
};
use witnet_rad::RADRequestExecutionReport;

use crate::{
    actors::{
        worker::{HandleBlockRequest, HandleSuperBlockRequest, NodeStatusRequest, NotifyStatus},
        *,
    },
    crypto, model,
};

use super::*;
use witnet_futures_utils::TryFutureExt2;

pub struct Validated {
    pub description: Option<String>,
    pub name: Option<String>,
    pub overwrite: bool,
    pub password: types::Password,
    pub seed_source: types::SeedSource,
    pub birth_date: Option<types::BirthDate>,
}

impl App {
    /// Start the actor App with the provided parameters
    pub fn start(params: Params) -> Addr<Self> {
        let actor = Self {
            server: None,
            params,
            state: Default::default(),
        };

        actor.start()
    }

    /// Stop the wallet application completely
    /// Note: if `rpc.on` subscriptions are closed before shutting down, stop() works correctly.
    pub fn stop(&mut self, ctx: &mut <Self as Actor>::Context) {
        log::debug!("Stopping application...");
        let s = self.server.take();
        // Potentially leak memory because we never join the thread, but that's fine because we are stopping the application
        std::thread::spawn(move || {
            drop(s);
        });
        self.stop_worker()
            .map(|res| match res {
                Ok(_) => {
                    log::info!("Application stopped. Shutting down system!");
                    System::current().stop();
                }
                Err(_) => {
                    log::error!("Couldn't stop application!");
                }
            })
            .into_actor(self)
            .spawn(ctx);
    }

    /// Return a new subscription id for a session.
    pub fn next_subscription_id(
        &mut self,
        session_id: &types::SessionId,
    ) -> Result<jsonrpc_pubsub::SubscriptionId> {
        if self.state.is_session_active(session_id) {
            // We are re-using the session id as the subscription id, this is because using a number
            // can let any client call the unsubscribe method for any other session.
            Ok(jsonrpc_pubsub::SubscriptionId::from(session_id))
        } else {
            Err(Error::SessionNotFound)
        }
    }

    /// Try to create a subscription and store it in the session. After subscribing, events related
    /// to wallets unlocked by this session will be sent to the client.
    pub fn subscribe(
        &mut self,
        session_id: types::SessionId,
        _subscription_id: jsonrpc_pubsub::SubscriptionId,
        sink: jsonrpc_pubsub::Sink,
    ) -> Result<()> {
        self.state.subscribe(&session_id, sink).map(|dyn_sink| {
            // If the subscription was successful, notify subscriber about initial status for all
            // wallets that belong to this session.
            let wallets = self.state.get_wallets_by_session(&session_id);
            if let Ok(wallets) = wallets {
                for (_, wallet) in wallets.iter() {
                    self.params.worker.do_send(NotifyStatus(
                        wallet.clone(),
                        dyn_sink.clone(),
                        None,
                    ));
                }
            }
        })
    }

    /// Remove a subscription.
    pub fn unsubscribe(&mut self, id: &jsonrpc_pubsub::SubscriptionId) -> Result<()> {
        // Session id and subscription id are currently the same thing. See comment in
        // next_subscription_id method.
        self.state.unsubscribe(id).map(|_| ())
    }

    /// Generate a receive address for the wallet's current account.
    pub fn generate_address(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
        external: bool,
        label: Option<String>,
    ) -> ResponseActFuture<model::Address> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::GenAddress {
                    wallet,
                    external,
                    label,
                })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Get a list of addresses generated by a wallet.
    pub fn get_addresses(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
        offset: u32,
        limit: u32,
        external: bool,
    ) -> ResponseActFuture<model::Addresses> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::GetAddresses {
                    wallet,
                    offset,
                    limit,
                    external,
                })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Get a list of addresses generated by a wallet.
    pub fn get_balance(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
    ) -> ResponseActFuture<model::WalletBalance> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::GetBalance { wallet })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Get a list of transactions associated to a wallet account.
    pub fn get_transactions(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
        offset: u32,
        limit: u32,
    ) -> ResponseActFuture<model::WalletTransactions> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::GetTransactions {
                    wallet,
                    offset,
                    limit,
                })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Run a RADRequest and return the computed result.
    pub fn run_rad_request(
        &self,
        request: RADRequest,
    ) -> ResponseFuture<RADRequestExecutionReport> {
        let f = self
            .params
            .worker
            .send(worker::RunRadRequest { request })
            .flatten_err();

        Box::pin(f)
    }

    /// Generate a random BIP39 mnemonics sentence
    pub fn generate_mnemonics(&self, length: mnemonic::Length) -> ResponseFuture<String> {
        let f = self
            .params
            .worker
            .send(worker::GenMnemonic { length })
            .map(|res| match res {
                Ok(x) => Ok(x),
                Err(e) => Err(e.into()),
            });

        Box::pin(f)
    }

    /// Forward a Json-RPC call to the node.
    pub fn forward(
        &mut self,
        method: String,
        params: jsonrpc_core::Params,
    ) -> ResponseFuture<serde_json::Value> {
        let req = jsonrpc::Request::method(method)
            .timeout(self.params.requests_timeout)
            .params(params)
            .expect("params failed serialization");
        let f = self.get_client().actor.send(req).flatten_err();

        Box::pin(f)
    }

    /// Get public info of all the wallets stored in the database.
    pub fn wallet_infos(&self) -> ResponseFuture<Vec<model::Wallet>> {
        let f = self.params.worker.send(worker::WalletInfos).flatten_err();

        Box::pin(f)
    }

    /// Create an empty HD Wallet.
    pub fn create_wallet(
        &self,
        password: types::Password,
        seed_source: types::SeedSource,
        name: Option<String>,
        description: Option<String>,
        overwrite: bool,
        birth_date: Option<types::BirthDate>,
    ) -> ResponseFuture<String> {
        let f = self
            .params
            .worker
            .send(worker::CreateWallet {
                name,
                description,
                password,
                seed_source,
                overwrite,
                birth_date,
            })
            .flatten_err();

        Box::pin(f)
    }

    /// Update a wallet details
    pub fn update_wallet(
        &self,
        session_id: types::SessionId,
        wallet_id: String,
        name: Option<String>,
        description: Option<String>,
    ) -> ResponseActFuture<()> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            let wallet_update = slf
                .params
                .worker
                .send(worker::UpdateWallet(
                    wallet,
                    name.clone(),
                    description.clone(),
                ))
                .flatten_err();

            let info_update = slf
                .params
                .worker
                .send(worker::UpdateWalletInfo { wallet_id, name })
                .flatten_err();

            futures::future::try_join(wallet_update, info_update)
                .map(|res| res.map(|_| ()))
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Lock a wallet, that is, remove its encryption/decryption key from the list of known keys and
    /// close the session.
    ///
    /// This means the state of this wallet won't be updated with information received from the
    /// node.
    pub fn lock_wallet(&mut self, session_id: types::SessionId, wallet_id: String) -> Result<()> {
        self.state.remove_wallet(&session_id, &wallet_id)
    }

    /// Load a wallet's private information and keys in memory.
    pub fn unlock_wallet(
        &mut self,
        wallet_id: String,
        password: types::Password,
    ) -> ResponseActFuture<types::UnlockedWallet> {
        // If a synchronization from a previous session is still running, set `stop_syncing` to
        // `true` so as to signal that it must stop as soon as possible
        if let Some(wallet) = self.state.get_current_wallet_session(wallet_id.clone()) {
            wallet.set_stop_syncing().expect("Lock error")
        }
        let id = wallet_id.clone();
        let f = self
            .params
            .worker
            .send(worker::UnlockWallet { id, password })
            .flatten_err()
            .into_actor(self)
            .and_then(move |res, slf: &mut Self, ctx| {
                let types::UnlockedSessionWallet {
                    wallet,
                    session_id,
                    data,
                } = res;

                slf.state
                    .create_session(session_id.clone(), wallet_id.clone(), wallet.clone());

                // If the node is synced start synchronization for this wallet
                if slf.state.node_state == Some(StateMachine::Synced)
                    || slf.state.node_state == None
                {
                    let sink = slf.state.get_sink(&session_id);
                    slf.params
                        .worker
                        .send(worker::SyncRequest {
                            wallet_id,
                            wallet,
                            sink,
                        })
                        .flatten_err()
                        .into_actor(slf)
                        .map(|res: Result<()>, act: &mut Self, _ctx| {
                            if let Err(e) = res {
                                act.handle_sync_error(&e);
                            }
                        })
                        .spawn(ctx)
                };

                fut::ok(types::UnlockedWallet { data, session_id })
            });

        Box::pin(f)
    }

    pub fn create_vtt(
        &self,
        session_id: &types::SessionId,
        wallet_id: &str,
        params: types::VttParams,
    ) -> ResponseActFuture<Transaction> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::CreateVtt { wallet, params })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    pub fn create_data_req(
        &self,
        session_id: &types::SessionId,
        wallet_id: &str,
        params: types::DataReqParams,
    ) -> ResponseActFuture<Transaction> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::CreateDataReq { wallet, params })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Perform all the tasks needed to properly stop the application.
    pub fn stop_worker(&self) -> ResponseFuture<()> {
        let fut = self
            .params
            .worker
            .send(worker::FlushDb)
            .map(|res| res.map_err(internal_error))
            .map(|res| match res {
                Ok(result) => result.map_err(internal_error),
                Err(e) => Err(e),
            });

        Box::pin(fut)
    }

    /// Return a timer function that can be scheduled to expire the session after the configured time.
    pub fn set_session_to_expire(&self, session_id: types::SessionId) -> Result<TimerFunc<Self>> {
        if !self.state.sessions.contains_key(&session_id) {
            log::error!("Session {} does not exist.", &session_id,);

            return Err(app::Error::SessionNotFound);
        }

        log::debug!(
            "Session {} will expire in {} seconds.",
            &session_id,
            self.params.session_expires_in.as_secs()
        );

        Ok(TimerFunc::new(
            self.params.session_expires_in,
            move |slf: &mut Self, _ctx| {
                if let Some(session) = slf.state.sessions.get_mut(&session_id) {
                    if !session.session_extended {
                        match slf.close_session(session_id.clone()) {
                            Ok(_) => log::info!("Session {} expired", session_id),
                            Err(err) => {
                                log::error!("Session {} couldn't be closed: {}", session_id, err)
                            }
                        }
                    } else {
                        session.session_extended = false;
                        log::debug!("Session {} expiration time has been extended", session_id)
                    }
                } else {
                    log::debug!(
                        "Session {} cannot be closed because it already expired",
                        session_id
                    )
                }
            },
        ))
    }

    /// Remove a session from the list of active sessions.
    pub fn close_session(&mut self, session_id: types::SessionId) -> Result<()> {
        self.state.remove_session(&session_id)
    }

    /// Get a client's previously stored value in the db (set method) with the given key.
    pub fn get(
        &self,
        session_id: types::SessionId,
        wallet_id: String,
        key: String,
    ) -> ResponseActFuture<Option<jsonrpc_core::Value>> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(|wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::Get { wallet, key })
                .flatten_err()
                .map(|res| match res {
                    Ok(Some(value)) => serde_json::from_str(&value)
                        .map_err(internal_error)
                        .map(Some),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                })
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Store a client's value in the db, associated to the given key.
    pub fn set(
        &self,
        session_id: types::SessionId,
        wallet_id: String,
        key: String,
        value: jsonrpc_core::Params,
    ) -> ResponseActFuture<()> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, _, _| {
            fut::result(serde_json::to_string(&value).map_err(internal_error)).and_then(
                move |value, slf: &mut Self, _| {
                    slf.params
                        .worker
                        .send(worker::Set { wallet, key, value })
                        .flatten_err()
                        .into_actor(slf)
                },
            )
        });

        Box::pin(f)
    }

    /// Handle any kind of notifications received from a Witnet node.
    pub fn handle_notification(&mut self, topic: String, value: serde_json::Value) -> Result<()> {
        match topic.as_str() {
            "blocks" => self.handle_block_notification(value),
            "superblocks" => self.handle_superblock_notification(value),
            _ => {
                log::debug!("Unhandled `{}` notification", topic);
                log::trace!("Payload is {:?}", value);

                Ok(())
            }
        }
    }

    /// Handle new block notifications received from a Witnet node.
    pub fn handle_block_notification(&mut self, value: serde_json::Value) -> Result<()> {
        let block = Arc::new(serde_json::from_value::<Block>(value).map_err(node_error)?);

        // This iterator is collected early so as to free the immutable reference to `self`.
        let wallets: Vec<types::SessionWallet> = self
            .state
            .wallets
            .iter()
            .map(|(_, wallet)| wallet.clone())
            .collect();

        for wallet in &wallets {
            let sink = self.state.get_sink(&wallet.session_id);
            self.handle_block_in_worker(block.clone(), &wallet, sink.clone());
        }

        Ok(())
    }

    /// Handle superblock notifications received from a Witnet node.
    pub fn handle_superblock_notification(&mut self, value: serde_json::Value) -> Result<()> {
        let superblock_notification =
            serde_json::from_value::<types::SuperBlockNotification>(value).map_err(node_error)?;

        // This iterator is collected early so as to free the immutable reference to `self`.
        let wallets: Vec<types::SessionWallet> = self
            .state
            .wallets
            .iter()
            .map(|(_, wallet)| wallet.clone())
            .collect();

        for wallet in &wallets {
            let sink = self.state.get_sink(&wallet.session_id);
            self.handle_superblock_in_worker(
                superblock_notification.clone(),
                wallet.clone(),
                sink.clone(),
            );
        }

        Ok(())
    }

    /// Offload block processing into a worker that operates on a different Arbiter than the main
    /// server thread, so as not to lock the rest of the application.
    pub fn handle_block_in_worker(
        &self,
        block: Arc<Block>,
        wallet: &types::SessionWallet,
        sink: types::DynamicSink,
    ) {
        self.params.worker.do_send(HandleBlockRequest {
            block,
            wallet: wallet.clone(),
            sink,
        });
    }

    /// Offload superblock processing into a worker that operates on a different Arbiter than the main
    /// server thread, so as not to lock the rest of the application.
    pub fn handle_superblock_in_worker(
        &self,
        superblock_notification: types::SuperBlockNotification,
        wallet: types::SessionWallet,
        sink: types::DynamicSink,
    ) {
        self.params.worker.do_send(HandleSuperBlockRequest {
            superblock_notification,
            wallet,
            sink,
        });
    }

    /// Send a transaction to witnet network using the Inventory method
    fn send_inventory_transaction(&self, txn: Transaction) -> ResponseActFuture<serde_json::Value> {
        let method = "inventory".to_string();
        let params = InventoryItem::Transaction(txn);

        let req = jsonrpc::Request::method(method)
            .timeout(self.params.requests_timeout)
            .params(params)
            .expect("params failed serialization");
        let f = self
            .get_client()
            .actor
            .send(req)
            .flatten_err()
            .map(|res| {
                match &res {
                    Ok(res) => log::debug!("Inventory request result: {:?}", res),
                    Err(err) => log::warn!("Inventory request failed: {}", &err),
                }

                res
            })
            .into_actor(self);

        Box::pin(f)
    }

    /// Send a transaction to the node as inventory item broadcast
    /// and add a local pending balance movement to the wallet state.
    pub fn send_transaction(
        &self,
        session_id: types::SessionId,
        wallet_id: String,
        transaction: Transaction,
    ) -> ResponseActFuture<SendTransactionResponse> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.send_inventory_transaction(transaction.clone())
                .and_then(move |jsonrpc_result, act, _ctx| {
                    match wallet.add_local_movement(&model::ExtendedTransaction {
                        transaction,
                        metadata: None,
                    }) {
                        Ok(balance_movement) => {
                            let sink = act.state.get_sink(&session_id);
                            if let Some(balance_movement) = balance_movement.clone() {
                                // We send a notification to the client
                                let events = Some(vec![types::Event::Movement(balance_movement)]);
                                act.params
                                    .worker
                                    .do_send(NotifyStatus(wallet, sink, events));
                            }
                            actix::fut::ok(SendTransactionResponse {
                                jsonrpc_result,
                                balance_movement,
                            })
                        }
                        Err(e) => {
                            log::error!("Error while adding local pending movement: {}", e);

                            actix::fut::err(Error::Internal(failure::Error::from(e)))
                        }
                    }
                })
        });

        Box::pin(f)
    }

    /// Use wallet's master key to sign message data
    pub fn sign_data(
        &self,
        session_id: &types::SessionId,
        wallet_id: &str,
        data: String,
        extended_pk: bool,
    ) -> ResponseActFuture<model::ExtendedKeyedSignature> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::SignData {
                    wallet,
                    data,
                    extended_pk,
                })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Shutdown system if session id is valid or there are no open sessions
    pub fn shutdown_request(
        &mut self,
        session_id: Option<types::SessionId>,
        ctx: &mut <Self as Actor>::Context,
    ) -> Result<()> {
        // Check if valid id or no open session(s)
        if let Some(session_id) = session_id {
            self.state.get_wallets_by_session(&session_id)?;
        } else if !self.state.sessions.is_empty() {
            return Err(app::Error::SessionsStillOpen);
        }
        self.stop(ctx);

        Ok(())
    }

    /// Get the URL and address of an existing JsonRpcClient actor.
    ///
    /// This method exists for convenience in case that at some point we decide to allow changing
    /// the `JsonRpcClient` address by putting `NodeClient` inside an `Arc<RwLock<_>>` or similar.
    #[inline(always)]
    pub fn get_client(&self) -> Arc<NodeClient> {
        self.params.client.clone()
    }

    /// Subscribe to receiving real time notifications of a specific type from a Witnet node.
    pub fn node_subscribe(&self, method: &str, ctx: &mut <Self as Actor>::Context) {
        let recipient = ctx.address().recipient();

        let request = jsonrpc::Request::method("witnet_subscribe")
            .timeout(self.params.requests_timeout)
            .value(serde_json::to_value([method]).expect(
                "Any JSON-RPC method name should be serializable using `serde_json::to_value`",
            ));

        log::debug!("Subscribing to {} notifications: {:?}", method, request);

        self.get_client()
            .actor
            .do_send(jsonrpc::Subscribe(request, recipient));
    }

    /// Send syncStatus request to the node every 10 seconds and send
    /// NodeDisconnected event if error in the response
    pub fn periodic_node_request(&self, ctx: &mut <Self as Actor>::Context) {
        let wallets: Vec<types::SessionWallet> = self
            .state
            .wallets
            .iter()
            .map(|(_, wallet)| wallet.clone())
            .collect();

        let wallets2 = wallets.clone();

        let events = Some(vec![types::Event::NodeDisconnected]);

        let req = jsonrpc::Request::method("syncStatus".to_string())
            .timeout(self.params.requests_timeout)
            .params(())
            .expect("params failed serialization");

        log::debug!("Sending periodic request: {:?}", req);

        let f = self
            .get_client()
            .actor
            .send(req)
            .flatten_err()
            .map(|res: Result<_>| {
                if let Ok(res) = &res {
                    log::debug!("Periodic request result: {:?}", res);
                    let status = serde_json::from_value::<SyncStatus>(res.clone());
                    log::debug!("The result of the node status is {:?}", status);
                }

                res
            })
            .into_actor(self)
            .map_err(move |err, act, _ctx| {
                log::warn!("Periodic request failed: {}", &err);
                log::error!("The node is disconnected");
                // Update node_state
                act.state.node_state = None;
                // Notify that the node is disconnected
                for wallet in &wallets2 {
                    let sink = act.state.get_sink(&wallet.session_id);
                    act.params
                        .worker
                        .do_send(NotifyStatus(wallet.clone(), sink, events.clone()))
                }
            })
            .map_ok(move |res, act, ctx| {
                let status = serde_json::from_value::<SyncStatus>(res);
                // Notify if the node status is changed
                if let Ok(status) = status {
                    if Some(status.node_state) != act.state.node_state {
                        // Update node_state
                        act.state.node_state = Some(status.node_state);
                        for wallet in &wallets {
                            let sink = act.state.get_sink(&wallet.session_id);
                            act.params
                                .worker
                                .send(NodeStatusRequest {
                                    status: act.state.node_state.unwrap(),
                                    wallet: wallet.clone(),
                                    sink,
                                })
                                .flatten_err()
                                .into_actor(act)
                                .map(|res: Result<()>, act: &mut Self, _ctx| {
                                    if let Err(e) = res {
                                        act.handle_sync_error(&e);
                                    }
                                })
                                .spawn(ctx);
                        }
                    }
                } else {
                    log::error!("Periodic request result serialization failed");
                }
            })
            .map(|_res: std::result::Result<(), ()>, _act, _ctx| ());
        ctx.spawn(f);

        // Try to contact the node once every 15 seconds
        let duration = std::time::Duration::from_secs(15);
        ctx.run_later(duration, |act, ctx| act.periodic_node_request(ctx));
    }

    /// Validate seed (mnemonics or xprv):
    ///  - check if seed data is valid
    ///  - check if there is already a wallet created with same seed
    ///  - return wallet id deterministically derived from seed data
    pub fn validate_seed(
        &self,
        seed_source: String,
        seed_data: types::Password,
        backup_password: Option<types::Password>,
    ) -> ResponseActFuture<ValidateMnemonicsResponse> {
        // Validate mnemonics source and data
        let f = fut::result(match seed_source.as_ref() {
            "xprv" => validate_xprv(seed_data, backup_password),
            "mnemonics" => mnemonic::Mnemonic::from_phrase(seed_data)
                .map_err(|err| Error::Validation(app::field_error("seed_data", format!("{}", err))))
                .map(types::SeedSource::Mnemonics),
            _ => Err(Error::Validation(app::field_error(
                "seed_source",
                "Seed source has to be mnemonics|xprv.",
            ))),
        })
        // Check if seed was already used in wallet
        .and_then(|seed, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::CheckWalletSeedRequest { seed })
                .flatten_err()
                .map(|res| {
                    res.map(|(exist, wallet_id)| ValidateMnemonicsResponse { exist, wallet_id })
                })
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Clear all chain data for a wallet state.
    ///
    /// Proceed with caution, as this wipes the following data entirely:
    /// - Synchronization status
    /// - Balances
    /// - Movements
    /// - Addresses and their metadata
    ///
    /// In order to prevent data race conditions, resyncing is not allowed while a sync or resync
    /// process is already in progress. Accordingly, this function returns whether chain data has
    /// been cleared or not.
    pub fn clear_chain_data_and_resync(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
    ) -> ResponseActFuture<bool> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            let sink = slf.state.get_sink(&session_id);

            // Send `Resync` message to worker
            slf.params
                .worker
                .send(worker::Resync {
                    wallet_id,
                    wallet,
                    sink,
                })
                .flatten_err()
                .into_actor(slf)
                .map_err(|e: Error, slf: &mut Self, _| {
                    slf.handle_sync_error(&e);
                    e
                })
        });

        Box::pin(f)
    }

    /// Export wallet master key, encrypted with password
    pub fn export_master_key(
        &mut self,
        session_id: types::SessionId,
        wallet_id: String,
        password: types::Password,
    ) -> ResponseActFuture<String> {
        let f = fut::result(
            self.state
                .get_wallet_by_session_and_id(&session_id, &wallet_id),
        )
        .and_then(move |wallet, slf: &mut Self, _| {
            slf.params
                .worker
                .send(worker::ExportMasterKey { wallet, password })
                .flatten_err()
                .into_actor(slf)
        });

        Box::pin(f)
    }

    /// Handle status from sync error
    pub fn handle_sync_error(&mut self, e: &Error) {
        if let Error::JsonRpcTimeoutError = e {
            log::error!(
                "Detected timeout while syncing, waiting until next periodic sync to connect"
            );
            self.state.node_state = None
        }
    }
}

// Validate `CreateWalletRequest`.
///
/// To be valid it must pass these checks:
/// - password is at least 8 characters
/// - seed_sources has to be `mnemonics | xprv`
#[allow(clippy::too_many_arguments)]
pub fn validate(
    password: types::Password,
    seed_data: types::Password,
    seed_source: String,
    name: Option<String>,
    description: Option<String>,
    overwrite: Option<bool>,
    backup_password: Option<types::Password>,
    birth_date: Option<types::BirthDate>,
) -> Result<Validated> {
    let source = match seed_source.as_ref() {
        "xprv" => validate_xprv(seed_data, backup_password)
            .map_err(|e| app::field_error("seed_data", e.to_string())),
        "mnemonics" => mnemonic::Mnemonic::from_phrase(seed_data)
            .map_err(|err| app::field_error("seed_data", format!("{}", err)))
            .map(types::SeedSource::Mnemonics),
        _ => Err(app::field_error(
            "seed_source",
            "Seed source has to be mnemonics|xprv",
        )),
    };
    let password = if <str>::len(password.as_ref()) < 8 {
        Err(app::field_error(
            "password",
            "Password must be at least 8 characters",
        ))
    } else {
        Ok(password)
    };
    let overwrite = overwrite.unwrap_or(false);
    app::combine_field_errors(source, password, move |seed_source, password| Validated {
        description,
        name,
        overwrite,
        password,
        seed_source,
        birth_date,
    })
    .map_err(validation_error)
}

/// Validate an encrypted XPRV file, first decrypting it and then checking the key format
/// The seed data contains the hrp||iv||salt||ciphertext
/// hrp can be either 'xprv' or 'xprvoduble'
pub fn validate_xprv(
    seed_data: types::Password,
    backup_password: Option<types::Password>,
) -> Result<types::SeedSource> {
    let backup_password = backup_password.ok_or_else(|| {
        validation_error(app::field_error(
            "backup_password",
            "Backup password not found for XPRV key",
        ))
    })?;
    let seed_data_string = seed_data.as_ref();
    let (hrp, ciphertext) = bech32::decode(seed_data_string).map_err(|_| {
        validation_error(app::field_error("seed_data", "Could not decode bech32 key"))
    })?;

    if hrp.as_str() != "xprv" && hrp.as_str() != "xprvdouble" {
        return Err(validation_error(app::field_error(
            "seed_data",
            "Invalid seed data prefix",
        )));
    }
    let decrypted_key_string = bech32::FromBase32::from_base32(&ciphertext)
        .map_err(|_| {
            validation_error(app::field_error(
                "seed_data",
                "Could not convert bech 32 decoded key to u8 array",
            ))
        })
        .and_then(|res: Vec<u8>| {
            crypto::decrypt_cbc(&res, backup_password.as_ref()).map_err(|_| {
                validation_error(app::field_error("seed_data", "Could not decrypt seed data"))
            })
        })
        .and_then(|decrypted: Vec<u8>| {
            std::str::from_utf8(&decrypted)
                .map(|str| str.to_string())
                .map_err(|_| {
                    validation_error(app::field_error("seed_data", "Could not decrypt seed data"))
                })
        })?;

    if hrp.as_str() == "xprv" {
        Ok(types::SeedSource::Xprv(decrypted_key_string.into()))
    } else {
        let (internal, external) = split_xprv_double(decrypted_key_string)?;

        Ok(types::SeedSource::XprvDouble((internal, external)))
    }
}

/// Split a double XPRV string into internal and external keys
pub fn split_xprv_double(xprv_double_key: String) -> Result<(types::Password, types::Password)> {
    let ocurrences: Vec<(usize, &str)> = xprv_double_key.match_indices("xprv").collect();
    // xprvDouble should only have 2 ocurrences
    if ocurrences.len() != 2 {
        return Err(validation_error(app::field_error(
            "seed_data",
            "Invalid number of XPRV keys found for xprvDouble type",
        )));
    }
    let (internal, external) = xprv_double_key.split_at(ocurrences[1].0);
    Ok((internal.into(), external.into()))
}
