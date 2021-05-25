use crate::{
    actors::dr_database::{DrDatabase, DrInfoBridge, DrState, GetLastDrId, SetDrInfoBridge},
    config::Config,
    create_wrb_contract,
};
use actix::prelude::*;
use std::{convert::TryFrom, time::Duration};
use ethabi::Bytes;
use web3::{
    contract::{self, Contract},
    types::{H160, U256},
};
use witnet_data_structures::chain::Hash;

/// EthPoller actor reads periodically new requests from the WRB Contract and includes them
/// in the DrDatabase
#[derive(Default)]
pub struct EthPoller {
    /// WRB contract
    pub wrb_contract: Option<Contract<web3::transports::Http>>,
    /// Period to check for new requests in the WRB
    pub eth_new_dr_polling_rate_ms: u64,
    /// eth_account
    pub eth_account: H160,
}

/// Make actor from EthPoller
impl Actor for EthPoller {
    /// Every actor has to provide execution Context in which it can run.
    type Context = Context<Self>;

    /// Method to be executed when the actor is started
    fn started(&mut self, ctx: &mut Self::Context) {
        log::debug!("EthPoller actor has been started!");

        self.check_new_requests_from_ethereum(
            ctx,
            Duration::from_millis(self.eth_new_dr_polling_rate_ms),
        );
    }
}

/// Required trait for being able to retrieve EthPoller address from system registry
impl actix::Supervised for EthPoller {}

/// Required trait for being able to retrieve EthPoller address from system registry
impl SystemService for EthPoller {}

impl EthPoller {
    /// Initialize `PeersManager` taking the configuration from a `Config` structure
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let wrb_contract = create_wrb_contract(config);

        Ok(Self {
            wrb_contract: Some(wrb_contract),
            eth_new_dr_polling_rate_ms: config.eth_new_dr_polling_rate_ms,
            eth_account: config.eth_account,
        })
    }

    fn check_new_requests_from_ethereum(&self, ctx: &mut Context<Self>, period: Duration) {
        let wrb_contract = self.wrb_contract.clone().unwrap();
        let eth_account = self.eth_account;
        // Check requests
        let fut = async move {
            let total_requests_count: Result<U256, web3::contract::Error> = wrb_contract
                .query(
                    "requestsCount",
                    (),
                    eth_account,
                    contract::Options::default(),
                    None,
                )
                .await;

            let dr_database_addr = DrDatabase::from_registry();
            let db_request_count = dr_database_addr.send(GetLastDrId).await;

            if let (Ok(total_requests_count), Ok(Ok(db_request_count))) =
                (total_requests_count, db_request_count)
            {
                if db_request_count < total_requests_count {
                    let init_index = usize::try_from(db_request_count + 1).unwrap();
                    let last_index = usize::try_from(total_requests_count).unwrap();

                    for i in init_index..last_index {
                        log::debug!("[{}] checking dr in wrb", i);
                        let dr_bytes: Result<Bytes, web3::contract::Error> = wrb_contract
                            .query(
                                "readDataRequest",
                                (U256::from(i),),
                                eth_account,
                                contract::Options::default(),
                                None,
                            )
                            .await;

                        if let Ok(dr_bytes) = dr_bytes {
                            let dr_result: Result<Bytes, web3::contract::Error> = wrb_contract
                                .query(
                                    "readResult",
                                    (U256::from(i),),
                                    eth_account,
                                    contract::Options::default(),
                                    None,
                                )
                                .await;

                            if let Ok(dr_result) = dr_result {
                                // Non-empty result: this data request is already "Finished"
                                if !dr_result.is_empty() {
                                    let dr_tx_hash: Result<U256, web3::contract::Error> =
                                        wrb_contract
                                            .query(
                                                "readDrHash",
                                                (U256::from(i),),
                                                eth_account,
                                                contract::Options::default(),
                                                None,
                                            )
                                            .await;

                                    if let Ok(dr_tx_hash) = dr_tx_hash {
                                        log::debug!("[{}] already finished", i);
                                        dr_database_addr.do_send(SetDrInfoBridge(
                                            U256::from(i),
                                            DrInfoBridge {
                                                dr_bytes,
                                                dr_state: DrState::Finished,
                                                dr_tx_hash: Some(Hash::SHA256(dr_tx_hash.into())),
                                            },
                                        ));
                                    } else {
                                        break;
                                    }
                                } else {
                                    log::info!("[{}] new dr in wrb", i);
                                    dr_database_addr.do_send(SetDrInfoBridge(
                                        U256::from(i),
                                        DrInfoBridge {
                                            dr_bytes,
                                            dr_state: DrState::New,
                                            dr_tx_hash: None,
                                        },
                                    ));
                                }
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
        };

        ctx.spawn(fut.into_actor(self).then(move |(), _act, ctx| {
            // Wait until the function finished to schedule next call.
            // This avoids tasks running in parallel.
            ctx.run_later(period, move |act, ctx| {
                // Reschedule check_new_requests_from_ethereum
                act.check_new_requests_from_ethereum(ctx, period);
            });

            actix::fut::ready(())
        }));
    }
}
