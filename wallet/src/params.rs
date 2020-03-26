use std::time::Duration;

use actix::Addr;

use witnet_data_structures::chain::EpochConstants;
use witnet_net::client::tcp::JsonRpcClient;

use crate::types;

/// Initialization parameters that can be specific for each wallet.
#[derive(Clone)]
pub struct Params {
    pub testnet: bool,
    pub seed_password: types::Password,
    pub master_key_salt: Vec<u8>,
    pub id_hash_iterations: u32,
    pub id_hash_function: types::HashFunction,
    pub db_hash_iterations: u32,
    pub db_iv_length: usize,
    pub db_salt_length: usize,
    pub epoch_constants: EpochConstants,
    pub last_sync: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            testnet: false,
            seed_password: "".into(),
            master_key_salt: b"Bitcoin seed".to_vec(),
            id_hash_iterations: 4096,
            id_hash_function: types::HashFunction::Sha256,
            db_hash_iterations: 10_000,
            db_iv_length: 16,
            db_salt_length: 32,
            epoch_constants: EpochConstants::default(),
            last_sync: 0,
        }
    }
}

#[derive(Clone)]
pub struct NodeParams {
    pub address: Addr<JsonRpcClient>,
    pub requests_timeout: Duration,
}
