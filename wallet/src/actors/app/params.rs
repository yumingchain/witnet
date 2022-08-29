use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use witnet_net::client::tcp::jsonrpc::{JsonRpcClient, Request};

use crate::actors;

use super::*;

pub struct Params {
    pub testnet: bool,
    pub worker: Addr<actors::Worker>,
    pub client: Arc<NodeClient>,
    pub server_addr: SocketAddr,
    pub session_expires_in: Duration,
    pub requests_timeout: Duration,
}

pub struct NodeClient {
    pub actor: Addr<JsonRpcClient>,
    pub url: Arc<Mutex<String>>,
}

impl NodeClient {
    /// Get the URL that the current client is connecting to.
    pub fn current_url(&self) -> String {
        self.url.lock().unwrap().to_string()
    }

    /// Verifies the existing connection by issuing a `syncStatus` command with a low timeout.
    pub async fn valid_connection(&self) -> bool {
        let url = self.current_url();

        log::debug!("Validating connection to {}", url);

        let request = Request::method("syncStatus").timeout(Duration::from_secs(2));
        let response = self.actor.send(request).await;

        matches!(response, Ok(Ok(_)))
    }
}
