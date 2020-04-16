use actix::{Context, Handler};
use log;

use super::PeersManager;
use crate::actors::messages::{
    AddConsolidatedPeer, AddPeers, GetKnownPeers, GetRandomPeers, PeersNewTried,
    PeersSocketAddrResult, PeersSocketAddrsResult, RemovePeers, RequestPeers,
};
use witnet_util::timestamp::get_timestamp;

/// Handler for AddPeers message
impl Handler<AddPeers> for PeersManager {
    type Result = PeersSocketAddrsResult;

    fn handle(&mut self, msg: AddPeers, _: &mut Context<Self>) -> Self::Result {
        // Insert address
        log::debug!("Adding the following peer addresses: {:?}", msg.addresses);
        self.peers.add_to_new(msg.addresses, msg.src_address)
    }
}

/// Handler for AddPeers message
impl Handler<AddConsolidatedPeer> for PeersManager {
    type Result = PeersSocketAddrResult;

    fn handle(&mut self, msg: AddConsolidatedPeer, _: &mut Context<Self>) -> Self::Result {
        // Insert address
        log::debug!(
            "Adding the following consolidated peer address: {:?}",
            msg.address
        );
        let current_ts = get_timestamp();

        let index = self.peers.tried_bucket_index(&msg.address);
        match self.peers.tried_bucket_get_timestamp(index) {
            Some(ts) if current_ts - ts < self.bucketing_update_period => {
                // It is recently updated
                Ok(None)
            }
            _ => self.peers.add_to_tried(msg.address),
        }
    }
}

/// Handler for RemovePeers message
impl Handler<RemovePeers> for PeersManager {
    type Result = PeersSocketAddrsResult;

    fn handle(&mut self, msg: RemovePeers, _: &mut Context<Self>) -> Self::Result {
        // Find index of element with address
        log::debug!("Removing the following addresses: {:?}", msg.addresses);
        Ok(self.peers.remove_from_tried(&msg.addresses))
    }
}

/// Handler for GetRandomPeer message
impl Handler<GetRandomPeers> for PeersManager {
    type Result = PeersSocketAddrsResult;

    fn handle(&mut self, msg: GetRandomPeers, _: &mut Context<Self>) -> Self::Result {
        let result = self.peers.get_random_peers(msg.n);

        match result {
            Ok(peers) => {
                log::debug!("Selected random peer addresses: {:?}", peers);
                Ok(peers)
            }
            error => {
                log::error!("Error selecting a random peer address: {:?}", error);
                error
            }
        }
    }
}

/// Handler for RequestPeers message
impl Handler<RequestPeers> for PeersManager {
    type Result = PeersSocketAddrsResult;

    fn handle(&mut self, _msg: RequestPeers, _: &mut Context<Self>) -> Self::Result {
        log::debug!("Get all peers");
        self.peers.get_all_from_tried()
    }
}

/// Handler for RequestPeers message
impl Handler<GetKnownPeers> for PeersManager {
    type Result = Result<PeersNewTried, failure::Error>;

    fn handle(&mut self, _msg: GetKnownPeers, _: &mut Context<Self>) -> Self::Result {
        Ok(PeersNewTried {
            new: self.peers.get_all_from_new()?,
            tried: self.peers.get_all_from_tried()?,
        })
    }
}
