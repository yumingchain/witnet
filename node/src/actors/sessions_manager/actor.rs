use super::SessionsManager;
use crate::config_mngr;
use actix::prelude::*;
use log;
use witnet_crypto::hash::calculate_sha256;
use witnet_data_structures::proto::ProtobufConvert;

/// Make actor from `SessionsManager`
impl Actor for SessionsManager {
    /// Every actor has to provide execution `Context` in which it can run
    type Context = Context<Self>;

    /// Method to be executed when the actor is started
    fn started(&mut self, ctx: &mut Self::Context) {
        log::debug!("Sessions Manager actor has been started!");

        // Send message to config manager and process its response
        config_mngr::get()
            .into_actor(self)
            .and_then(|config, act, ctx| {
                // Get periods for peers bootstrapping and discovery tasks
                let bootstrap_peers_period = config.connections.bootstrap_peers_period;
                let discovery_peers_period = config.connections.discovery_peers_period;
                let consensus_constants = config.consensus_constants.clone();

                // Set server address, connections limits and handshake timeout
                act.sessions
                    .set_server_address(config.connections.server_addr);
                act.sessions.set_limits(
                    config.connections.inbound_limit,
                    config.connections.outbound_limit,
                );
                act.sessions
                    .set_handshake_timeout(config.connections.handshake_timeout);
                act.sessions
                    .set_blocks_timeout(config.connections.blocks_timeout);

                let magic = calculate_sha256(&consensus_constants.to_pb_bytes().unwrap());
                let magic = u16::from(magic.0[0]) << 8 | (u16::from(magic.0[1]));
                act.sessions.set_magic_number(magic);

                // The peers bootstrapping process begins upon SessionsManager's start
                act.bootstrap_peers(ctx, bootstrap_peers_period);

                // The peers discovery process begins upon SessionsManager's start
                act.discovery_peers(ctx, discovery_peers_period);

                fut::ok(())
            })
            .map_err(|err, _, _| log::error!("Sessions manager startup error: {}", err))
            .wait(ctx);
    }
}
