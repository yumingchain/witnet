//! Witnet <> Ethereum bridge
use async_jsonrpc_client::{
    futures::Stream, transports::tcp::TcpSocket, DuplexTransport, Transport,
};
use ethabi::{Bytes, Token};
use futures::future::Either;
use futures::sink::Sink;
use log::*;
use rand::{thread_rng, Rng};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process;
use std::time::{Duration, Instant};
use std::{sync::Arc, time};
use tokio::{prelude::FutureExt, sync::mpsc, timer::Interval};
use web3::{
    contract,
    futures::{future, Future},
    types::{FilterBuilder, TransactionReceipt, U256},
};
use witnet_crypto::hash::{calculate_sha256, Sha256};
use witnet_data_structures::{
    chain::{Block, DataRequestOutput, Hash, Hashable},
    proto::ProtobufConvert,
};
use witnet_ethereum_bridge::{
    config::{read_config, Config},
    eth::{read_u256_from_event_log, EthState, WbiEvent},
};

fn handle_receipt(receipt: TransactionReceipt) -> impl Future<Item = (), Error = ()> {
    match receipt.status {
        Some(x) if x == 1.into() => {
            //debug!("Success!");
            // Success
            futures::finished(())
        }
        Some(x) if x == 0.into() => {
            error!("Error :(");
            // Fail
            // TODO: Reason?
            futures::failed(())
        }
        x => {
            error!("Wtf is a {:?} status", x);
            futures::failed(())
        }
    }
}

fn convert_json_array_to_eth_bytes(value: Value) -> Result<Bytes, serde_json::Error> {
    // Convert json values such as [1, 2, 3] into bytes
    serde_json::from_value(value)
}

fn wait_for_witnet_block_in_block_relay(
    config: Arc<Config>,
    eth_state: Arc<EthState>,
    block_hash: U256,
) -> impl Future<Item = (), Error = ()> {
    // TODO: this function is not async
    // Because for some reason timers aren't working
    loop {
        debug!("Waiting for block {:x} in block relay", block_hash);

        let res = eth_state
            .block_relay_contract
            .query(
                "readDrMerkleRoot",
                (block_hash,),
                config.eth_account,
                contract::Options::default(),
                None,
            )
            .map(move |_res: U256| {
                debug!(
                    "Block {:x} was included in BlockRelay contract!",
                    block_hash
                );
            })
            .wait();

        if res.is_ok() {
            break;
        }

        std::thread::sleep(Duration::from_millis(10_000));
    }

    futures::finished(())

    // This does not work
    /*
    Interval::new(Instant::now(), Duration::from_millis(30_000))
        .map_err(|e| error!("Error creating interval: {:?}", e))
        .for_each(move |_instant| {
            // This line is never printed
            error!("BlockRelay tick");
            eth_state
                .block_relay_contract
                .query(
                    "readDrMerkleRoot",
                    (block_hash,),
                    config.eth_account,
                    contract::Options::default(),
                    None,
                )
                .map(move |_res: U256| {
                    debug!("Block {} was included in BlockRelay contract!", block_hash);
                })
                .map_err(move |e| {
                    debug!(
                        "Block {} not yet included in BlockRelay contract: {:?}",
                        block_hash, e
                    )
                })
                .then(|res| {
                    // We want to exit this function when the future returns Ok,
                    // but streams only abort if the future returns Err.
                    match res {
                        Ok(()) => Err(()),
                        Err(()) => Ok(()),
                    }
                })
        })
    */

    // This does not work, and may result in a stack overflow
    /*
    Box::new(
        eth_state
            .block_relay_contract
            .query(
                "readDrMerkleRoot",
                (block_hash,),
                config.eth_account,
                contract::Options::default(),
                None,
            )
            .map(move |_res: U256| {
                debug!("Block {} was included in BlockRelay contract!", block_hash);
            })
            .or_else(move |e| {
                /*
                debug!("Block {} not found in BlockRelay, retrying in 30 seconds: {:?}", block_hash, e);
                // Retry in 30 seconds
                let when = Instant::now() + Duration::from_millis(30_000);

                Delay::new(when)
                    .map_err(|e| panic!("delay errored; err={:?}", e))
                    .and_then(move |_| {
                        wait_for_witnet_block_in_block_relay(config, eth_state, block_hash)
                    })
                */
                wait_for_witnet_block_in_block_relay(config, eth_state, block_hash)
            }),
    )
    */
}

fn eth_event_stream(
    config: Arc<Config>,
    eth_state: Arc<EthState>,
    _tx: mpsc::Sender<ActorMessage>,
    tx4: mpsc::Sender<PostActorMessage>,
) -> impl Future<Item = (), Error = ()> {
    let web3 = &eth_state.web3;
    let accounts = eth_state.accounts.clone();
    if !accounts.contains(&config.eth_account) {
        error!(
            "Account does not exists: {}\nAvailable accounts:\n{:#?}",
            config.eth_account, accounts
        );
        process::exit(1);
    }

    let contract_address = config.wbi_contract_addr;

    let post_dr_event_sig = eth_state.post_dr_event_sig;
    let inclusion_dr_event_sig = eth_state.inclusion_dr_event_sig;
    let post_tally_event_sig = eth_state.post_tally_event_sig;

    debug!(
        "Subscribing to contract {:?} topic {:?}",
        contract_address, post_dr_event_sig
    );
    let post_dr_filter = FilterBuilder::default()
        .from_block(0.into())
        .address(vec![contract_address])
        .topics(
            Some(vec![
                post_dr_event_sig,
                inclusion_dr_event_sig,
                post_tally_event_sig,
            ]),
            None,
            None,
            None,
        )
        .build();

    // Helper function to parse an ethereum event log as one of the possible WBI events
    let parse_as_wbi_event = move |value: &web3::types::Log| -> Result<WbiEvent, ()> {
        match &value.topics[0] {
            x if x == &post_dr_event_sig => {
                Ok(WbiEvent::PostedRequest(read_u256_from_event_log(&value)?))
            }
            x if x == &inclusion_dr_event_sig => {
                Ok(WbiEvent::IncludedRequest(read_u256_from_event_log(&value)?))
            }
            x if x == &post_tally_event_sig => {
                Ok(WbiEvent::PostedResult(read_u256_from_event_log(&value)?))
            }
            _ => Err(()),
        }
    };

    web3.eth_filter()
        .create_logs_filter(post_dr_filter)
        .map_err(|e| {
            error!("Failed to create logs filter: {}", e);
            process::exit(1);
        })
        .and_then(move |filter| {
            debug!("Created filter: {:?}", filter);
            info!("Subscribed to ethereum events");
            filter
                // This poll interval was set to 0 in the example, which resulted in the
                // bridge having 100% cpu usage...
                .stream(time::Duration::from_secs(1))
                .map_err(|e| error!("ethereum event error = {:?}", e))
                .map(move |value| {
                    debug!("Got ethereum event: {:?}", value);

                    parse_as_wbi_event(&value)
                })
                .for_each(move |value| {
                    let tx4 = tx4.clone();
                    let eth_state2 = eth_state.clone();
                    let fut: Box<dyn Future<Item = (), Error = ()> + Send> =
                        match value {
                            Ok(WbiEvent::PostedRequest(dr_id)) => {
                                info!("[{}] New data request posted to WBI", dr_id);

                                Box::new(
                                    eth_state.wbi_requests.write().map(move |mut wbi_requests| {
                                        wbi_requests.insert_posted(dr_id);
                                    }).and_then(move |()| {
                                        tx4.send(PostActorMessage::NewDr(dr_id))
                                            .map(|_| ())
                                            .map_err(|e| error!("Error sending message to PostActorMessage channel: {:?}", e))
                                    })
                                )
                            }
                            Ok(WbiEvent::IncludedRequest(dr_id)) => {
                                let contract = &eth_state.wbi_contract;
                                debug!("[{}] Reading dr_tx_hash for id", dr_id);
                                Box::new(
                                    contract
                                        .query(
                                            "readDrHash",
                                            (dr_id,),
                                            config.eth_account,
                                            contract::Options::default(),
                                            None,
                                        )
                                        .map_err(|e| error!("{:?}", e))
                                        .and_then(move |dr_tx_hash: U256| {
                                            let dr_tx_hash = Hash::SHA256(dr_tx_hash.into());
                                            info!(
                                                "[{}] Data request included in witnet with dr_tx_hash: {}",
                                                dr_id, dr_tx_hash
                                            );

                                            eth_state2.wbi_requests.write().map(move |mut wbi_requests| {
                                                wbi_requests.insert_included(dr_id, dr_tx_hash);
                                            })
                                        })
                                )
                            }
                            Ok(WbiEvent::PostedResult(dr_id)) => {
                                info!("[{}] Data request has been resolved!", dr_id);

                                // TODO: actually get result?
                                let result = vec![];
                                Box::new(eth_state.wbi_requests.write().map(move |mut wbi_requests| {
                                    wbi_requests.insert_result(dr_id, result);
                                }))
                            }
                            _ => {
                                warn!("Received unknown ethereum event");
                                Box::new(futures::finished(()))
                            }
                        };

                    fut
                })
                // Without this line the stream will stop on the first failure
                .then(|_| Ok(()))
        })
}

fn witnet_block_stream(
    config: Arc<Config>,
    tx: mpsc::Sender<ActorMessage>,
) -> (
    async_jsonrpc_client::transports::shared::EventLoopHandle,
    impl Future<Item = (), Error = ()>,
) {
    let witnet_addr = config.witnet_jsonrpc_addr.to_string();
    let witnet_addr1 = witnet_addr.clone();
    let witnet_addr2 = witnet_addr.clone();
    // Important: the handle cannot be dropped, otherwise the client stops
    // processing events
    let (handle, witnet_client) =
        async_jsonrpc_client::transports::tcp::TcpSocket::new(&witnet_addr).unwrap();
    let witnet_client1 = witnet_client.clone();

    let fut = witnet_client
        .execute("witnet_subscribe", json!(["newBlocks"]))
        .timeout(Duration::from_secs(1))
        .map_err(move |e| {
            if e.is_elapsed() {
                error!(
                    "Timeout when trying to connect to witnet node at {}",
                    witnet_addr2
                );
                error!("Is the witnet node running?");
            } else if e.is_inner() {
                error!(
                    "Error connecting to witnet node at {}: {:?}",
                    witnet_addr1,
                    e.into_inner()
                );
            } else {
                error!("{:?}", e);
            }
        })
        .then(|witnet_subscription_id_value| {
            // Panic if the subscription wasn't successful
            let witnet_subscription_id = match witnet_subscription_id_value {
                Ok(serde_json::Value::String(s)) => s,
                Ok(x) => {
                    error!("Witnet subscription id must be a string, is {:?}", x);
                    process::exit(1);
                }
                Err(_) => {
                    error!("Failed to subscribe to newBlocks from witnet node");
                    process::exit(1);
                }
            };
            info!(
                "Subscribed to witnet newBlocks with subscription id \"{}\"",
                witnet_subscription_id
            );

            let witnet_client = witnet_client1;

            witnet_client
                .subscribe(&witnet_subscription_id.into())
                .map_err(|e| error!("witnet notification error = {:?}", e))
                .and_then(move |value| {
                    let tx1 = tx.clone();
                    // TODO: get current epoch to distinguish between old blocks that are sent
                    // to us while synchronizing and new blocks
                    match serde_json::from_value::<Block>(value) {
                        Ok(block) => {
                            debug!("Got witnet block: {:?}", block);
                            Either::A(
                                tx1.send(ActorMessage::NewWitnetBlock(Box::new(block)))
                                    .map_err(|_| ())
                                    .map(|_| ()),
                            )
                        }
                        Err(e) => {
                            error!("Error parsing witnet block: {:?}", e);
                            Either::B(futures::finished(()))
                        }
                    }
                })
                .for_each(|_| Ok(()))
        });

    (handle, fut)
}

fn init_logger() {
    // Info log level by default
    let mut log_level = log::LevelFilter::Info;
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        if rust_log.contains("witnet") {
            log_level = env_logger::Logger::from_default_env().filter();
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default())
        .filter_module("witnet_ethereum_bridge", log_level)
        .init();
}

fn postdr(
    config: Arc<Config>,
    eth_state: Arc<EthState>,
    witnet_client: Arc<TcpSocket>,
    dr_id: U256,
) -> impl Future<Item = (), Error = ()> {
    let wbi_contract = eth_state.wbi_contract.clone();
    let eth_account = config.eth_account;

    let wbi_contract = wbi_contract.clone();
    let wbi_contract2 = wbi_contract.clone();
    let wbi_contract3 = wbi_contract.clone();
    let wbi_contract4 = wbi_contract.clone();
    let wbi_contract5 = wbi_contract.clone();
    let wbi_contract6 = wbi_contract.clone();
    let wbi_contract7 = wbi_contract.clone();
    let witnet_client = Arc::clone(&witnet_client);
    let witnet_client2 = Arc::clone(&witnet_client);
    let witnet_client3 = Arc::clone(&witnet_client);
    let witnet_client4 = Arc::clone(&witnet_client);

    wbi_contract
        .query(
            "readDataRequest",
            (dr_id,),
            eth_account,
            contract::Options::default(),
            None,
        )
        .map_err(|e| error!("{:?}", e))
        .and_then(move |dr_bytes: Bytes| {
            let dr_output: DataRequestOutput =
                match ProtobufConvert::from_pb_bytes(&dr_bytes) {
                    Ok(x) => x,
                    Err(e) => {
                        warn!(
                            "[{}] uses an invalid serialization, will be ignored: {:?}",
                            dr_id, e
                        );
                        let fut: Box<dyn Future<Item = (_, _), Error = ()> + Send> =
                            Box::new(futures::failed(()));
                        return fut;
                    }
                };

            Box::new(
                wbi_contract2
                    .query(
                        "getLastBeacon",
                        (),
                        eth_account,
                        contract::Options::default(),
                        None,
                    )
                    .map(|x: Bytes| (x, dr_output))
                    .map_err(|e| error!("{:?}", e)),
            )
        })
        .and_then(move |(vrf_message, dr_output)| {
            let last_beacon = vrf_message.clone();

            witnet_client2
                .execute("createVRF", vrf_message.into())
                .map_err(|e| error!("createVRF: {:?}", e))
                .map(move |vrf| {
                    trace!("createVRF: {:?}", vrf);

                    (vrf, dr_output, last_beacon)
                })
        })
        .and_then(move |(vrf, dr_output, last_beacon)| {
            // Sign the ethereum account address with the witnet node private key
            let Sha256(hash) = calculate_sha256(eth_account.as_bytes());

            witnet_client3
                .execute("sign", hash.to_vec().into())
                .map_err(|e| error!("sign: {:?}", e))
                .map(|sign_addr| {
                    trace!("sign: {:?}", sign_addr);

                    (vrf, sign_addr, dr_output, last_beacon)
                })
        })
        .and_then(move |(vrf, sign_addr, dr_output, last_beacon)| {
            // Get the public key of the witnet node

            witnet_client4
                .execute("getPublicKey", json!(null))
                .map_err(|e| error!("getPublicKey: {:?}", e))
                .map(move |witnet_pk| {
                    trace!("getPublicKey: {:?}", witnet_pk);

                    (vrf, sign_addr, witnet_pk, dr_output, last_beacon)
                })
        })
        .and_then(move |(vrf, sign_addr, witnet_pk, dr_output, last_beacon)| {

            // Locallty execute POE verification to check for eligibility
            // without spending any gas
            // TODO: this assumes that the vrf, witnet_pk and sign_addr are returned
            // as an array of bytes: [1, 2, 3].
            let poe = convert_json_array_to_eth_bytes(vrf);
            let witnet_pk = convert_json_array_to_eth_bytes(witnet_pk);
            let sign_addr = convert_json_array_to_eth_bytes(sign_addr);

            let (poe, witnet_pk, sign_addr) = match (poe, witnet_pk, sign_addr) {
                (Ok(poe), Ok(witnet_pk), Ok(sign_addr)) => {
                    (poe, witnet_pk, sign_addr)
                }
                e => {
                    error!(
                        "Error deserializing value from witnet JSONRPC: {:?}",
                        e
                    );
                    let fut: Box<
                        dyn Future<Item = (_, _, _, _, _), Error = ()> + Send,
                    > = Box::new(futures::failed(()));
                    return fut;
                }
            };

            debug!(
                "\nPoE: {:?}\nWitnet Public Key: {:?}\nSignature Address: {:?}",
                poe, witnet_pk.clone(), sign_addr
            );
            info!("[{}] Checking eligibility for claiming dr", dr_id);

            Box::new(
                wbi_contract5
                    .query(
                        "decodePoint",
                        witnet_pk,
                        eth_account,
                        contract::Options::default(),
                        None,
                    )
                    .map_err(move |e| {
                        warn!(
                            "[{}] Error decoding public Key:  {}",
                            dr_id, e);
                    })
                    .map(move |pk: Token| {
                        debug!("Received public key decode Point: {:?}", pk);

                        (poe, sign_addr, pk, dr_output, last_beacon)
                    }),
            )
        })
        .and_then(move |(poe, sign_addr, witnet_pk, dr_output, last_beacon)| {

            Box::new(
                wbi_contract6
                    .query(
                        "decodeProof",
                        poe,
                        eth_account,
                        contract::Options::default(),
                        None,
                    )
                    .map_err(move |e| {
                        warn!(
                            "[{}] Error decoding proof:  {}",
                            dr_id, e);
                    })
                    .map(move |proof: Token| {
                        debug!("Received proof decode Point: {:?}", proof);

                        (proof, sign_addr, witnet_pk, dr_output, last_beacon)
                    }),
            )
        })
        .and_then(move |(poe, sign_addr, witnet_pk, dr_output, last_beacon)| {

            Box::new(
                wbi_contract7
                    .query(
                        "computeFastVerifyParams",
                        (witnet_pk.clone(), poe.clone(), last_beacon),
                        eth_account,
                        contract::Options::default(),
                        None,
                    )
                    .map_err(move |e| {
                        warn!(
                            "[{}] Error in params reception:  {}",
                            dr_id, e);
                    })
                    .map(move |(u_point, v_point): (Token, Token)| {
                        debug!("Received fast verify params: ({:?}, {:?})", u_point, v_point);

                        (poe, sign_addr, witnet_pk, dr_output, u_point , v_point)
                    }),
            )
        })
        .and_then(move |(poe, sign_addr, witnet_pk, dr_output, u_point , v_point)| {
            let mut sign_addr2 = sign_addr.clone();
            let fut1 = wbi_contract3
                .query(
                    "claimDataRequests",
                    (vec![dr_id], poe.clone(), witnet_pk.clone(), u_point.clone(), v_point.clone(), sign_addr.clone()),
                    eth_account,
                    contract::Options::default(),
                    None,
                )
                .map(|_: Token| sign_addr);
            // If the query fails, we want to retry it with the signature "v" value flipped.
            *sign_addr2.last_mut().unwrap() ^= 0x01;
            let fut2 = wbi_contract3
                .query(
                    "claimDataRequests",
                    (vec![dr_id], poe.clone(), witnet_pk.clone(), u_point.clone(), v_point.clone(), sign_addr2.clone()),
                    eth_account,
                    contract::Options::default(),
                    None,
                )
                .map(|_: Token| sign_addr2);

            Box::new(
                fut1
                    .or_else(move |e| {
                        debug!("claimDataRequests failed, retrying with different signature sign (v): {:?}", e);
                        Box::new(fut2)
                    })
                    .map_err(move |e| {
                        warn!(
                            "[{}] the POE is invalid, no eligibility for this epoch, or the data request has already been claimed :( {:?}",
                            dr_id, e);
                    })
                    .map(move |sign_addr| {
                        (poe, sign_addr, witnet_pk, dr_output, u_point, v_point)
                    }),
            )

        })
        .and_then(move |(poe, sign_addr, witnet_pk, dr_output, u_point, v_point)| {
            // Claim dr
            info!("[{}] Claiming dr", dr_id);
            let dr_output_hash = dr_output.hash();

            // Mark the data request as claimed to prevent double claims by other threads
            eth_state.wbi_requests.write().then(move |wbi_requests| {
                match wbi_requests {
                    Ok(mut wbi_requests) => {
                        if wbi_requests.posted().contains(&dr_id) {
                            wbi_requests.set_claiming(dr_id);
                            Ok(())
                        } else {
                            // This data request is not available, abort.
                            warn!("[{}] is already marked as claimed, we will not claim it again", dr_id);
                            Err(())
                        }
                    }
                    Err(e) => {
                        // According to the documentation of the futures-locks crate,
                        // this error cannot happen
                        error!("Failed to acquire RwLock: {:?}", e);
                        Err(())
                    }
                }
            })
                .and_then(move |()| {
                    let eth_state2 = eth_state.clone();

                    wbi_contract4
                        .call_with_confirmations(
                            "claimDataRequests",
                            (vec![dr_id], poe, witnet_pk, u_point, v_point, sign_addr),
                            eth_account,
                            contract::Options::with(|opt| {
                                opt.gas = Some(500_000.into());
                            }),
                            1,
                        )
                        .map_err(|e| {
                            error!("claimDataRequests: {:?}", e);
                        })
                        .and_then(move |tx| {
                            debug!("claimDataRequests: {:?}", tx);
                            handle_receipt(tx).map_err(move |_| {
                                // Or the PoE became invalid because a new witnet block was
                                // just relayed
                                // In this case we should save this data request to retry later
                                warn!(
                                    "[{}] has been claimed by another bridge node, or the PoE expired",
                                    dr_id
                                );
                            })
                        })
                        .and_then(move |()| {
                            eth_state.wbi_requests.write().map(move |mut wbi_requests| {
                                wbi_requests.confirm_claim(dr_id, dr_output_hash);
                            })
                        })
                        .or_else(move |()| {
                            // Undo the claim
                            eth_state2.wbi_requests.write().map(move |mut wbi_requests| {
                                wbi_requests.undo_claim(dr_id);
                            }).then(|_| {
                                // Short-circuit the and_then cascade
                                Err(())
                            })
                        })
                })
                .and_then(move |_traces| {
                    // Post dr in witnet
                    info!("[{}] Claimed dr, posting to witnet", dr_id);

                    let bdr_params = json!({"dro": dr_output, "fee": 0});

                    witnet_client
                        .execute("buildDataRequest", bdr_params)
                        .map_err(|e| error!("{:?}", e))
                        .map(move |bdr_res| {
                            debug!("buildDataRequest: {:?}", bdr_res);
                        })
                })
                .map(|_| ())
        })
}

fn post_actor(
    config: Arc<Config>,
    eth_state: Arc<EthState>,
    rx: mpsc::Receiver<PostActorMessage>,
) -> (
    async_jsonrpc_client::transports::shared::EventLoopHandle,
    impl Future<Item = (), Error = ()>,
) {
    // Important: the handle cannot be dropped, otherwise the client stops
    // processing events
    let witnet_addr = config.witnet_jsonrpc_addr.to_string();
    let (handle, witnet_client) =
        async_jsonrpc_client::transports::tcp::TcpSocket::new(&witnet_addr).unwrap();
    let witnet_client = Arc::new(witnet_client);

    (
        handle,
        rx.map_err(|_| ()).for_each(move |msg| {
            debug!("Got PostActorMessage: {:?}", msg);

            let config2 = Arc::clone(&config);
            let eth_state2 = Arc::clone(&eth_state);
            let witnet_client2 = Arc::clone(&witnet_client);

            let fut = match msg {
                PostActorMessage::NewDr(dr_id) => Either::A(postdr(
                    config.clone(),
                    eth_state.clone(),
                    witnet_client.clone(),
                    dr_id,
                )),
                PostActorMessage::Tick => {
                    Either::B(eth_state.wbi_requests.read().and_then(move |known_dr_ids| {
                        let known_dr_ids_posted = known_dr_ids.posted();
                        let known_dr_ids_claimed = known_dr_ids.claimed();
                        debug!(
                            "Known data requests in WBI: {:?}{:?}",
                            known_dr_ids_posted, known_dr_ids_claimed
                        );

                        // Chose a random data request and try to claim and post it.
                        // Gives preference to newly posted data requests
                        match (
                            known_dr_ids_posted.is_empty(),
                            known_dr_ids_claimed.is_empty(),
                        ) {
                            (true, true) => Either::B(futures::finished(())),
                            (false, _) => {
                                let i = thread_rng().gen_range(0, known_dr_ids_posted.len());
                                let dr_id = *known_dr_ids_posted.iter().nth(i).unwrap();
                                std::mem::drop(known_dr_ids);

                                Either::A(postdr(
                                    config2.clone(),
                                    eth_state2.clone(),
                                    witnet_client2.clone(),
                                    dr_id,
                                ))
                            }
                            _ => {
                                // Should we claim an already claimed data request?
                                // This can be useful in the following scenarios:
                                // * The data request is posted to Witnet, but it
                                //   is not accepted into a Witnet block
                                //   (or is invalid because of double-spending).
                                //   In this case probably there is no need to claim
                                //   again, just build another transaction in Witnet.
                                // * The data request is accepted into a Witnet
                                //   block, but the reportInclusion transaction
                                //   is not accepted into an Ethereum block.
                                //   In this case the data request will be in
                                //   "Including" state, and we do not handle that here.
                                let i = thread_rng().gen_range(0, known_dr_ids_claimed.len());
                                let dr_id = *known_dr_ids_claimed.iter().nth(i).unwrap().0;
                                std::mem::drop(known_dr_ids);

                                Either::A(postdr(
                                    config2.clone(),
                                    eth_state2.clone(),
                                    witnet_client2.clone(),
                                    dr_id,
                                ))
                            }
                        }
                    }))
                }
            };

            // Start the claim as a separate task, to avoid blocking this receiver
            tokio::spawn(fut);

            Ok(())
        }),
    )
}

#[derive(Debug)]
enum PostActorMessage {
    NewDr(U256),
    Tick,
}

#[derive(Debug)]
enum ActorMessage {
    NewWitnetBlock(Box<Block>),
}

fn main_actor(
    config: Arc<Config>,
    eth_state: Arc<EthState>,
    rx: mpsc::Receiver<ActorMessage>,
) -> impl Future<Item = (), Error = ()> {
    // A list of all the tallies from all the blocks since we started listening
    // This is a workaround around a race condition with ethereum events:
    // If the reportDataRequestInclusion event is emitted after the data request
    // has been resolved in witnet, we will never see the tally transaction
    // and the result will not be reported to the WBI.
    // By storing all the tallies we avoid this problem, but obviously this
    // does not scale.
    // When the dataRequestReport method will be implemented on the witnet
    // node JSON-RPC, we will be able to query data request status, and use
    // that information to prove inclusion of any data request.
    let mut all_seen_tallies = HashMap::new();

    let wbi_contract = eth_state.wbi_contract.clone();
    let block_relay_contract = eth_state.block_relay_contract.clone();

    rx.map_err(|_| ())
        .for_each(move |msg| {
            debug!("Got ActorMessage: {:?}", msg);

            debug!("all_seen_tallies.len() == {}", all_seen_tallies.len());
            match msg {
                ActorMessage::NewWitnetBlock(block) => {
                    // Optimization: do not process empty blocks
                    let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".parse().unwrap();
                    if block.block_header.merkle_roots.dr_hash_merkle_root == empty_hash && block.block_header.merkle_roots.tally_hash_merkle_root == empty_hash {
                        debug!("Skipping empty block");
                        return futures::finished(());
                    }

                    let block_hash: U256 = match block.hash() {
                        Hash::SHA256(x) => x.into(),
                    };

                    // Enable block relay?
                    if config.enable_block_relay {
                        let block_epoch: U256 = block.block_header.beacon.checkpoint.into();
                        let dr_merkle_root: U256 =
                            match block.block_header.merkle_roots.dr_hash_merkle_root {
                                Hash::SHA256(x) => x.into(),
                            };
                        let tally_merkle_root: U256 =
                            match block.block_header.merkle_roots.tally_hash_merkle_root {
                                Hash::SHA256(x) => x.into(),
                            };

                        debug!("Trying to relay block {:x}", block_hash);

                        // Post witnet block to BlockRelay wbi_contract
                        tokio::spawn(
                            block_relay_contract
                                .call_with_confirmations(
                                    "postNewBlock",
                                    (block_hash, block_epoch, dr_merkle_root, tally_merkle_root),
                                    config.eth_account,
                                    contract::Options::with(|opt| {
                                        opt.gas = Some(100_000.into());
                                    }),
                                    1,
                                )
                                .map_err(|e| error!("postNewBlock: {:?}", e))
                                .and_then(|tx| {
                                    debug!("postNewBlock: {:?}", tx);
                                    handle_receipt(tx)
                                })
                                .map(move |()| {
                                    info!("Posted block {:x} to block relay", block_hash);
                                })
                        );
                    }

                    // Wait for someone else to publish the witnet block to ethereum
                    wait_for_witnet_block_in_block_relay(
                        Arc::clone(&config),
                        Arc::clone(&eth_state),
                        block_hash,
                    ).and_then(|()| {
                        eth_state.wbi_requests.read()
                    })
                    .and_then(|wbi_requests| {
                        let block_hash: U256 = match block.hash() {
                            Hash::SHA256(x) => x.into(),
                        };

                        let mut including = vec![];
                        let mut resolving = vec![];

                        let claimed_drs = wbi_requests.claimed();
                        let waiting_for_tally = wbi_requests.included();
                        /*
                        // Verify that the block was posted correctly
                        block_relay_contract.query(
                            "readDrMerkleRoot",
                            (block_hash,),
                            config.eth_account,
                            contract::Options::default(),
                            None,
                        ).then(|tx: Result<U256, _>| {
                            debug!("readDrMerkleRoot: {:?}", tx);
                            Result::<(), ()>::Ok(())
                        }).wait().unwrap();
                        */
                        // The futures executed after this point should be executed *after* the
                        // postNewBlock transaction has been confirmed
                        // TODO: double check that the bridge contains this block?

                        for dr in &block.txns.data_request_txns {
                            if let Some(dr_id) =
                                claimed_drs.get_by_right(&dr.body.dr_output.hash())
                            {
                                let dr_inclusion_proof = match dr.data_proof_of_inclusion(&block) {
                                    Some(x) => x,
                                    None => {
                                        error!("Error creating data request proof of inclusion");
                                        continue;
                                    }
                                };

                                let dr_bytes = match dr.body.dr_output.to_pb_bytes() {
                                    Ok(x) => x,
                                    Err(e) => {
                                        error!("Error serializing data request output to Protocol Buffers: {:?}", e);
                                        continue;
                                    }
                                };

                                debug!(
                                    "Proof of inclusion for data request {}:\nData: {:?}\n{:?}",
                                    dr.hash(),
                                    dr_bytes,
                                    dr_inclusion_proof
                                );
                                info!("[{}] Claimed dr got included in witnet block!", dr_id);
                                info!("[{}] Sending proof of inclusion to WBI wbi_contract", dr_id);

                                let poi: Vec<U256> = dr_inclusion_proof
                                    .lemma
                                    .iter()
                                    .map(|x| match x {
                                        Hash::SHA256(x) => x.into(),
                                    })
                                    .collect();
                                let poi_index = U256::from(dr_inclusion_proof.index);
                                including.push((*dr_id, poi.clone(), poi_index, block_hash));
                                tokio::spawn(
                                    wbi_contract
                                        .call_with_confirmations(
                                            "reportDataRequestInclusion",
                                            (*dr_id, poi, poi_index, block_hash),
                                            config.eth_account,
                                            contract::Options::default(),
                                            1,
                                        )
                                        .map_err(|e| error!("reportDataRequestInclusion: {:?}", e))
                                        .and_then(move |tx| {
                                            debug!("reportDataRequestInclusion: {:?}", tx);
                                            handle_receipt(tx)
                                        }),
                                );
                            }
                        }

                        all_seen_tallies.extend(block.txns.tally_txns.iter().filter_map(|tally| {
                            let tally_inclusion_proof = match tally.data_proof_of_inclusion(&block) {
                                Some(x) => x,
                                None => {
                                    error!("Error creating tally data proof of inclusion");
                                    return None;
                                }
                            };

                            Some((tally.dr_pointer, (tally.clone(), tally_inclusion_proof, block_hash)))
                        }));

                        for (dr_id, dr_tx_hash) in waiting_for_tally {
                            if let Some((tally, tally_inclusion_proof, tally_block_hash)) = all_seen_tallies.get(&dr_tx_hash)
                            {
                                // Call report_result method of the WBI
                                let Hash::SHA256(dr_pointer_bytes) = tally.dr_pointer;
                                info!("[{}] Found tally for data request, posting to WBI", dr_id);
                                debug!(
                                    "Proof of inclusion for tally        {}:\nData: {:?}\n{:?}",
                                    tally.hash(),
                                    [&dr_pointer_bytes[..], &tally.tally].concat(),
                                    tally_inclusion_proof
                                );

                                // Call report_result
                                let poi: Vec<U256> = tally_inclusion_proof
                                    .lemma
                                    .iter()
                                    .map(|x| match x {
                                        Hash::SHA256(x) => x.into(),
                                    })
                                    .collect();
                                let poi_index = U256::from(tally_inclusion_proof.index);
                                let result: Bytes = tally.tally.clone();
                                resolving.push((*dr_id, poi.clone(), poi_index, *tally_block_hash, result.clone()));
                                tokio::spawn(
                                    wbi_contract
                                        .call_with_confirmations(
                                            "reportResult",
                                            (*dr_id, poi, poi_index, *tally_block_hash, result),
                                            config.eth_account,
                                            contract::Options::default(),
                                            1,
                                        )
                                        .map_err(|e| error!("reportResult: {:?}", e))
                                        .and_then(|tx| {
                                            debug!("reportResult: {:?}", tx);
                                            handle_receipt(tx)
                                        }),
                                );
                            }
                        }

                        // Update the wbi_requests map
                        //std::mem::drop(wbi_requests);
                        // Check if we need to acquire a write lock
                        if !including.is_empty() || !resolving.is_empty() {
                            Either::A(eth_state.wbi_requests.write().map(|mut wbi_requests| {
                                for (dr_id, poi, poi_index, block_hash) in including {
                                    wbi_requests.set_including(dr_id, poi, poi_index, block_hash);
                                }
                                for (dr_id, poi, poi_index, block_hash, result) in resolving {
                                    wbi_requests.set_resolving(dr_id, poi, poi_index, block_hash, result);
                                }
                            }))
                        } else {
                            Either::B(futures::finished(()))
                        }
                    })
                    // Without this line the actor will panic on the first failure
                    .then(|_| Result::<(), ()>::Ok(()))
                    // Synchronously wait for the future because we do not want to be processing
                    // multiple blocks in parallel
                    .wait().unwrap();

                    futures::finished(())
                }
            }
        })
        .map(|_| ())
}

fn main() {
    init_logger();
    let config = Arc::new(match read_config() {
        Ok(x) => x,
        Err(e) => {
            error!("Error reading configuration file: {}", e);
            process::exit(1);
        }
    });
    let eth_state = Arc::new(match EthState::create(&config) {
        Ok(x) => x,
        Err(()) => {
            error!("Error when trying to initialize ethereum related stuff");
            error!("Is the ethereum node running at {}?", config.eth_client_url);
            process::exit(1);
        }
    });

    // FIXME(#772): Channel closes in case of future errors and bridge fails
    // TODO: prefer bounded or unbounded channels?
    let (tx1, rx) = mpsc::channel(16);
    let (ptx, prx) = mpsc::channel(16);
    let tx2 = tx1.clone();
    let ptx2 = ptx.clone();

    let ees = eth_event_stream(Arc::clone(&config), Arc::clone(&eth_state), tx1, ptx);
    let (_handle, wbs) = witnet_block_stream(Arc::clone(&config), tx2);
    let (_handle, pct) = post_actor(Arc::clone(&config), Arc::clone(&eth_state), prx);
    let act = main_actor(Arc::clone(&config), Arc::clone(&eth_state), rx);

    // Every 30 seconds, try to claim a random data request
    // This has a problem with race conditions: the same data request can be
    // claimed twice (leading to an invalid transaction).
    // Also, when the PostActor is busy posting a different data request,
    // all the Tick messages get queued and then processed at once.
    let post_ticker = Interval::new(Instant::now(), Duration::from_millis(30_000))
        .map_err(|e| error!("Error creating interval: {:?}", e))
        .and_then(move |_instant| {
            ptx2.clone()
                .send(PostActorMessage::Tick)
                .map_err(|e| error!("Error sending tick to PostActor: {:?}", e))
        })
        .for_each(|_| Ok(()));

    tokio::run(future::ok(()).map(move |_| {
        tokio::spawn(wbs);
        tokio::spawn(ees);
        tokio::spawn(pct);
        tokio::spawn(act);
        tokio::spawn(post_ticker);
    }));
}
