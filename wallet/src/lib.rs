//! Wallet implementation for Witnet
//!
//! The way a client interacts with the Wallet is through a Websockets server. After running it you
//! can interact with it from the web-browser's javascript console:
//! ```js
//! var sock= (() => { let s = new WebSocket('ws://localhost:3030');s.addEventListener('message', (e) => {  console.log('Rcv =>', e.data) });return s; })();
//! sock.send('{"jsonrpc":"2.0","method":"getBlockChain","id":"1"}');
//! ```

#![deny(rust_2018_idioms)]
#![deny(non_upper_case_globals)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![deny(missing_docs)]
use std::time::Duration;

use actix::prelude::*;
use failure::Error;
use jsonrpc_core as rpc;
use jsonrpc_pubsub as pubsub;

use witnet_config::config::Config;
use witnet_net::{client::tcp::JsonRpcClient, server::ws::Server};

mod actors;
mod model;
mod rocksdb;
mod signal;
mod types;

/// Run the Witnet wallet application.
pub fn run(conf: Config) -> Result<(), Error> {
    let session_expires_in = Duration::from_secs(conf.wallet.session_expires_in);
    let requests_timeout = Duration::from_millis(conf.wallet.requests_timeout);
    let server_addr = conf.wallet.server_addr;
    let db_path = conf.wallet.db_path;
    let db_file_name = conf.wallet.db_file_name;
    let node_url = conf.wallet.node_url;
    let mut rocksdb_opts = conf.rocksdb.to_rocksdb_options();
    // https://github.com/facebook/rocksdb/wiki/Merge-Operator
    rocksdb_opts.set_merge_operator("wallet merge operator", rocksdb::merge_operator, None);

    // Db-encryption params
    let db_hash_iterations = conf.wallet.db_encrypt_hash_iterations;
    let db_iv_length = conf.wallet.db_encrypt_iv_length;
    let db_salt_length = conf.wallet.db_encrypt_salt_length;

    // Whether wallet is in testnet mode or not
    let testnet = conf.wallet.testnet;

    // Master-key generation params
    let seed_password = conf.wallet.seed_password;
    let master_key_salt = conf.wallet.master_key_salt;
    let id_hash_iterations = conf.wallet.id_hash_iterations;
    let id_hash_function = conf.wallet.id_hash_function;

    let system = System::new("witnet-wallet");

    let client = node_url.clone().map_or_else(
        || Ok(None),
        |url| JsonRpcClient::start(url.as_ref()).map(Some),
    )?;

    let db = ::rocksdb::DB::open(&rocksdb_opts, db_path.join(db_file_name))
        .map_err(|e| failure::format_err!("{}", e))?;

    let worker = actors::Worker::start(actors::worker::Params {
        testnet,
        seed_password,
        master_key_salt,
        id_hash_iterations,
        id_hash_function,
        db_hash_iterations,
        db_iv_length,
        db_salt_length,
    });

    let app = actors::App::start(
        db,
        actors::app::Params {
            worker,
            client,
            session_expires_in,
            requests_timeout,
        },
    );
    let mut handler = pubsub::PubSubHandler::new(rpc::MetaIoHandler::default());

    actors::app::connect_routes(&mut handler, app.clone(), Arbiter::current());

    let server = Server::build().handler(handler).addr(server_addr).start()?;
    let controller = actors::Controller::start(server, app);

    signal::ctrl_c(move || {
        controller.do_send(actors::controller::Shutdown);
    });

    system.run()?;

    Ok(())
}
