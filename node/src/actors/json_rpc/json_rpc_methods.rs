#[cfg(test)]
use self::mock_actix::System;
use crate::actors::{
    chain_manager::{ChainManager, ChainManagerError},
    epoch_manager::EpochManager,
    messages::{AddCandidates, AddTransaction, GetBlocksEpochRange, GetEpoch},
};
#[cfg(not(test))]
use actix::System;
use actix::{MailboxError, SystemService};
use jsonrpc_core::{futures, futures::Future, IoHandler, Params, Value};
use log::{debug, info};
use serde::{Deserialize, Serialize};
//use std::str::FromStr;
use witnet_data_structures::chain::{Block, InventoryEntry, Transaction};

type JsonRpcResult = Result<Value, jsonrpc_core::Error>;
type JsonRpcResultAsync = Box<dyn Future<Item = Value, Error = jsonrpc_core::Error> + Send>;

/// Define the JSON-RPC interface:
/// All the methods available through JSON-RPC
pub fn jsonrpc_io_handler() -> IoHandler<()> {
    let mut io = IoHandler::new();

    io.add_method("inventory", |params: Params| inventory(params.parse()?));
    io.add_method("getBlockChain", |params: Params| {
        get_block_chain(params.parse())
    });
    //io.add_method("getOutput", |params: Params| get_output(params.parse()));

    io
}

fn internal_error<T: std::fmt::Debug>(e: T) -> jsonrpc_core::Error {
    jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::InternalError,
        message: format!("{:?}", e),
        data: None,
    }
}

/// Inventory element: block, transaction, etc
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum InventoryItem {
    /// Error
    #[serde(rename = "error")]
    Error,
    /// Transaction
    #[serde(rename = "transaction")]
    Transaction(Transaction),
    /// Block
    #[serde(rename = "block")]
    Block(Block),
}

/// Make the node process, validate and potentially broadcast a new inventory entry.
///
/// Input: the JSON serialization of a well-formed inventory entry
///
/// Returns a boolean indicating success.
/* Test string:
{"jsonrpc": "2.0","method": "inventory","params": {"block": {"block_header":{"version":1,"beacon":{"checkpoint":2,"hash_prev_block": {"SHA256": [4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4]}},"hash_merkle_root":{"SHA256":[3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3]}},"proof":{"block_sig": null,"influence":99999}"txns":[null]}},"id": 1}
*/
pub fn inventory(inv_elem: InventoryItem) -> JsonRpcResult {
    match inv_elem {
        InventoryItem::Block(block) => {
            debug!("Got block from JSON-RPC. Sending AnnounceItems message.");

            // Get SessionsManager's address
            let chain_manager_addr = System::current().registry().get::<ChainManager>();
            // If this function was called asynchronously, it could wait for the result
            // But it's not so we just assume success
            chain_manager_addr.do_send(AddCandidates {
                blocks: vec![block],
            });

            // Returns a boolean indicating success
            Ok(Value::Bool(true))
        }

        InventoryItem::Transaction(transaction) => {
            debug!("Got transaction from JSON-RPC. Sending AnnounceItems message.");

            // Get SessionsManager's address
            let chain_manager_addr = System::current().registry().get::<ChainManager>();
            // If this function was called asynchronously, it could wait for the result
            // But it's not so we just assume success
            chain_manager_addr.do_send(AddTransaction { transaction });

            // Returns a boolean indicating success
            Ok(Value::Bool(true))
        }

        inv_elem => {
            info!(
                "Invalid type of inventory item from JSON-RPC: {:?}",
                inv_elem
            );
            Err(jsonrpc_core::Error::invalid_params(
                "Item type not implemented",
            ))
        }
    }
}

/// Params of getBlockChain method
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GetBlockChainParams {
    /// TODO
    #[serde(default)] // default to 0
    pub epoch: i64,
    /// TODO
    #[serde(default)] // default to 0
    pub limit: u32,
}

/// Get the list of all the known block hashes.
///
/// Returns a list of `(epoch, block_hash)` pairs.
/* test
{"jsonrpc": "2.0","method": "getBlockChain", "id": 1}
*/
pub fn get_block_chain(
    params: Result<Option<GetBlockChainParams>, jsonrpc_core::Error>,
) -> JsonRpcResultAsync {
    // Helper function to convert the result of GetBlockEpochRange to a JSON value, or a JSON-RPC error
    fn process_get_block_chain(
        res: Result<Result<Vec<(u32, InventoryEntry)>, ChainManagerError>, MailboxError>,
    ) -> impl Future<Item = Value, Error = jsonrpc_core::Error> {
        match res {
            Ok(Ok(vec_inv_entry)) => {
                let epoch_and_hash: Vec<_> = vec_inv_entry
                    .into_iter()
                    .map(|(epoch, inv_entry)| {
                        let hash = match inv_entry {
                            InventoryEntry::Block(hash) => hash,
                            x => panic!("{:?} is not a block", x),
                        };
                        let hash_string = format!("{}", hash);
                        (epoch, hash_string)
                    })
                    .collect();
                let value = match serde_json::to_value(epoch_and_hash) {
                    Ok(x) => x,
                    Err(e) => {
                        let err = internal_error(e);
                        return futures::failed(err);
                    }
                };
                futures::finished(value)
            }
            Ok(Err(e)) => {
                let err = internal_error(e);
                futures::failed(err)
            }
            Err(e) => {
                let err = internal_error(e);
                futures::failed(err)
            }
        }
    }

    let GetBlockChainParams { epoch, limit } = match params {
        Ok(x) => x.unwrap_or_default(),
        Err(e) => return Box::new(futures::failed(e)),
    };

    let limit = limit as usize;
    let chain_manager_addr = ChainManager::from_registry();
    if epoch >= 0 {
        let epoch = epoch as u32;
        let fut = chain_manager_addr
            .send(GetBlocksEpochRange::new_with_limit(epoch.., limit))
            .then(process_get_block_chain);
        Box::new(fut)
    } else {
        // On negative epoch, get blocks from last n epochs
        // But, what is the current epoch?
        let fut = EpochManager::from_registry()
            .send(GetEpoch)
            .then(move |res| match res {
                Ok(Ok(current_epoch)) => {
                    let epoch = (i64::from(current_epoch) + epoch) as u32;

                    futures::finished(epoch)
                }
                Ok(Err(e)) => {
                    let err = internal_error(e);
                    futures::failed(err)
                }
                Err(e) => {
                    let err = internal_error(e);
                    futures::failed(err)
                }
            })
            .and_then(move |epoch| {
                chain_manager_addr
                    .send(GetBlocksEpochRange::new_with_limit(epoch.., limit))
                    .then(process_get_block_chain)
            });
        Box::new(fut)
    }
}
/*
/// get output
pub fn get_output(output_pointer: Result<(String,), jsonrpc_core::Error>) -> JsonRpcResultAsync {
    let output_pointer = match output_pointer {
        Ok(x) => match OutputPointer::from_str(&x.0) {
            Ok(x) => x,
            Err(e) => {
                let err = internal_error(e);
                return Box::new(futures::failed(err));
            }
        },
        Err(e) => {
            return Box::new(futures::failed(e));
        }
    };
    let chain_manager_addr = ChainManager::from_registry();
    Box::new(
        chain_manager_addr
            .send(GetOutput { output_pointer })
            .then(|res| match res {
                Ok(Ok(output)) => {
                    let value = match serde_json::to_value(output) {
                        Ok(x) => x,
                        Err(e) => {
                            let err = internal_error(e);
                            return futures::failed(err);
                        }
                    };
                    futures::finished(value)
                }
                Ok(Err(e)) => {
                    let err = internal_error(e);
                    futures::failed(err)
                }
                Err(e) => {
                    let err = internal_error(e);
                    futures::failed(err)
                }
            }),
    )
}
*/

#[cfg(test)]
mod mock_actix {
    pub struct System;

    pub struct SystemRegistry;

    pub struct Addr;

    impl System {
        pub fn current() -> Self {
            System
        }
        pub fn registry(&self) -> &SystemRegistry {
            &SystemRegistry
        }
    }

    impl SystemRegistry {
        pub fn get<T>(&self) -> Addr {
            Addr
        }
    }

    impl Addr {
        pub fn do_send<T>(&self, _msg: T) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_parse_error() {
        // An empty message should return a parse error
        let empty_string = "";
        let parse_error =
            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}"#
                .to_string();
        let io = jsonrpc_io_handler();
        let response = io.handle_request_sync(empty_string);
        assert_eq!(response, Some(parse_error));
    }

    #[test]
    fn inventory_method() {
        // The expected behaviour of the inventory method
        use witnet_data_structures::chain::*;
        let signature = Signature::Secp256k1(Secp256k1Signature {
            r: [0; 32],
            s: [0; 32],
            v: 0,
        });
        let keyed_signatures = vec![KeyedSignature {
            public_key: [0; 32],
            signature,
        }];

        let reveal_input = Input::Reveal(RevealInput {
            output_index: 0,
            transaction_id: Hash::SHA256([0; 32]),
        });

        let commit_input = Input::Commit(CommitInput {
            nonce: 0,
            output_index: 0,
            reveal: [0; 32].to_vec(),
            transaction_id: Hash::SHA256([0; 32]),
        });
        let data_request_input = Input::DataRequest(DataRequestInput {
            output_index: 0,
            poe: [0; 32],
            transaction_id: Hash::SHA256([0; 32]),
        });

        let rad_aggregate = RADAggregate { script: vec![0] };

        let rad_retrieve_1 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_retrieve_2 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_consensus = RADConsensus { script: vec![0] };

        let rad_deliver_1 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l2awcd/".to_string(),
        };

        let rad_deliver_2 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l1awcw/".to_string(),
        };

        let rad_request = RADRequest {
            aggregate: rad_aggregate,
            not_before: 0,
            retrieve: vec![rad_retrieve_1, rad_retrieve_2],
            consensus: rad_consensus,
            deliver: vec![rad_deliver_1, rad_deliver_2],
        };
        let data_request_output = Output::DataRequest(DataRequestOutput {
            backup_witnesses: 0,
            commit_fee: 0,
            data_request: rad_request,
            pkh: [0; 20],
            reveal_fee: 0,
            tally_fee: 0,
            time_lock: 0,
            value: 0,
            witnesses: 0,
        });
        let commit_output = Output::Commit(CommitOutput {
            commitment: Hash::SHA256([0; 32]),
            value: 0,
        });
        let reveal_output = Output::Reveal(RevealOutput {
            pkh: [0; 20],
            reveal: [0; 32].to_vec(),
            value: 0,
        });
        let consensus_output = Output::Tally(TallyOutput {
            pkh: [0; 20],
            result: [0; 32].to_vec(),
            value: 0,
        });
        let value_transfer_output = Output::ValueTransfer(ValueTransferOutput {
            pkh: [0; 20],
            value: 0,
        });
        let inputs = vec![reveal_input, data_request_input, commit_input];
        let outputs = vec![
            value_transfer_output,
            data_request_output,
            commit_output,
            reveal_output,
            consensus_output,
        ];
        let txns: Vec<Transaction> = vec![Transaction {
            inputs,
            signatures: keyed_signatures,
            outputs,
            version: 0,
        }];
        let block = Block {
            block_header: BlockHeader {
                version: 1,
                beacon: CheckpointBeacon {
                    checkpoint: 2,
                    hash_prev_block: Hash::SHA256([4; 32]),
                },
                hash_merkle_root: Hash::SHA256([3; 32]),
            },
            proof: LeadershipProof {
                block_sig: None,
                influence: 99999,
            },
            txns,
        };

        let inv_elem = InventoryItem::Block(block);
        let s = serde_json::to_string(&inv_elem).unwrap();
        let msg = format!(
            r#"{{"jsonrpc":"2.0","method":"inventory","params":{},"id":1}}"#,
            s
        );

        // Expected result: true
        let expected = r#"{"jsonrpc":"2.0","result":true,"id":1}"#.to_string();
        let io = jsonrpc_io_handler();
        let response = io.handle_request_sync(&msg);
        assert_eq!(response, Some(expected));
    }

    #[test]
    fn inventory_invalid_params() {
        // What happens when the inventory method is called with an invalid parameter?
        let msg = r#"{"jsonrpc":"2.0","method":"inventory","params":{ "header": 0 },"id":1}"#;
        let expected = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params: unknown variant `header`, expected one of"#.to_string();
        let io = jsonrpc_io_handler();
        let response = io.handle_request_sync(&msg);
        // Compare only the first N characters
        let response =
            response.map(|s| s.chars().take(expected.chars().count()).collect::<String>());
        assert_eq!(response, Some(expected));
    }

    #[test]
    fn inventory_unimplemented_type() {
        // What happens when the inventory method is called with an unimplemented type?
        let msg = r#"{"jsonrpc":"2.0","method":"inventory","params":{ "error": null },"id":1}"#;
        let expected =
            r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Item type not implemented"#
                .to_string();
        let io = jsonrpc_io_handler();
        let response = io.handle_request_sync(&msg);
        // Compare only the first N characters
        let response =
            response.map(|s| s.chars().take(expected.chars().count()).collect::<String>());
        assert_eq!(response, Some(expected));
    }

    #[test]
    fn serialize_block() {
        // Check that the serialization of `Block` doesn't change
        use witnet_data_structures::chain::*;
        let signature = Signature::Secp256k1(Secp256k1Signature {
            r: [0; 32],
            s: [0; 32],
            v: 0,
        });
        let keyed_signatures = vec![KeyedSignature {
            public_key: [0; 32],
            signature,
        }];
        let reveal_input = Input::Reveal(RevealInput {
            output_index: 0,
            transaction_id: Hash::SHA256([0; 32]),
        });
        let commit_input = Input::Commit(CommitInput {
            nonce: 0,
            output_index: 0,
            reveal: [0; 32].to_vec(),
            transaction_id: Hash::SHA256([0; 32]),
        });
        let data_request_input = Input::DataRequest(DataRequestInput {
            output_index: 0,
            poe: [0; 32],
            transaction_id: Hash::SHA256([0; 32]),
        });
        let value_transfer_output = Output::ValueTransfer(ValueTransferOutput {
            pkh: [0; 20],
            value: 0,
        });

        let rad_aggregate = RADAggregate { script: vec![0] };

        let rad_retrieve_1 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_retrieve_2 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_consensus = RADConsensus { script: vec![0] };

        let rad_deliver_1 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l2awcd/".to_string(),
        };

        let rad_deliver_2 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l1awcw/".to_string(),
        };

        let rad_request = RADRequest {
            aggregate: rad_aggregate,
            not_before: 0,
            retrieve: vec![rad_retrieve_1, rad_retrieve_2],
            consensus: rad_consensus,
            deliver: vec![rad_deliver_1, rad_deliver_2],
        };
        let data_request_output = Output::DataRequest(DataRequestOutput {
            backup_witnesses: 0,
            commit_fee: 0,
            data_request: rad_request,
            pkh: [0; 20],
            reveal_fee: 0,
            tally_fee: 0,
            time_lock: 0,
            value: 0,
            witnesses: 0,
        });
        let commit_output = Output::Commit(CommitOutput {
            commitment: Hash::SHA256([0; 32]),
            value: 0,
        });
        let reveal_output = Output::Reveal(RevealOutput {
            pkh: [0; 20],
            reveal: [0; 32].to_vec(),
            value: 0,
        });
        let consensus_output = Output::Tally(TallyOutput {
            pkh: [0; 20],
            result: [0; 32].to_vec(),
            value: 0,
        });

        let inputs = vec![commit_input, data_request_input, reveal_input];
        let outputs = vec![
            value_transfer_output,
            data_request_output,
            commit_output,
            reveal_output,
            consensus_output,
        ];
        let txns: Vec<Transaction> = vec![Transaction {
            inputs,
            signatures: keyed_signatures,
            outputs,
            version: 0,
        }];
        let block = Block {
            block_header: BlockHeader {
                version: 1,
                beacon: CheckpointBeacon {
                    checkpoint: 2,
                    hash_prev_block: Hash::SHA256([4; 32]),
                },
                hash_merkle_root: Hash::SHA256([3; 32]),
            },
            proof: LeadershipProof {
                block_sig: None,
                influence: 99999,
            },
            txns,
        };
        let inv_elem = InventoryItem::Block(block);
        let s = serde_json::to_string(&inv_elem);
        let expected = r#"{"block":{"block_header":{"version":1,"beacon":{"checkpoint":2,"hash_prev_block":{"SHA256":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4]}},"hash_merkle_root":{"SHA256":[3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3]}},"proof":{"block_sig":null,"influence":99999},"txns":[{"version":0,"inputs":[{"Commit":{"transaction_id":{"SHA256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"output_index":0,"reveal":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"nonce":0}},{"DataRequest":{"transaction_id":{"SHA256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"output_index":0,"poe":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}},{"Reveal":{"transaction_id":{"SHA256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"output_index":0}}],"outputs":[{"ValueTransfer":{"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"value":0}},{"DataRequest":{"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"data_request":{"not_before":0,"retrieve":[{"kind":"HTTP-GET","url":"https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22","script":[0]},{"kind":"HTTP-GET","url":"https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22","script":[0]}],"aggregate":{"script":[0]},"consensus":{"script":[0]},"deliver":[{"kind":"HTTP-GET","url":"https://hooks.zapier.com/hooks/catch/3860543/l2awcd/"},{"kind":"HTTP-GET","url":"https://hooks.zapier.com/hooks/catch/3860543/l1awcw/"}]},"value":0,"witnesses":0,"backup_witnesses":0,"commit_fee":0,"reveal_fee":0,"tally_fee":0,"time_lock":0}},{"Commit":{"commitment":{"SHA256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"value":0}},{"Reveal":{"reveal":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"value":0}},{"Tally":{"result":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"value":0}}],"signatures":[{"signature":{"Secp256k1":{"r":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"s":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"v":0}},"public_key":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}]}]}}"#;
        assert_eq!(s.unwrap(), expected);
    }

    #[test]
    fn serialize_transaction() {
        use witnet_data_structures::chain::*;
        let rad_aggregate = RADAggregate { script: vec![0] };

        let rad_retrieve_1 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_retrieve_2 = RADRetrieve {
            kind: RADType::HttpGet,
            url: "https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22".to_string(),
            script: vec![0],
        };

        let rad_consensus = RADConsensus { script: vec![0] };

        let rad_deliver_1 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l2awcd/".to_string(),
        };

        let rad_deliver_2 = RADDeliver {
            kind: RADType::HttpGet,
            url: "https://hooks.zapier.com/hooks/catch/3860543/l1awcw/".to_string(),
        };

        let rad_request = RADRequest {
            aggregate: rad_aggregate,
            not_before: 0,
            retrieve: vec![rad_retrieve_1, rad_retrieve_2],
            consensus: rad_consensus,
            deliver: vec![rad_deliver_1, rad_deliver_2],
        };
        // Check that the serialization of `Transaction` doesn't change
        let transaction = build_hardcoded_transaction(rad_request);

        let inv_elem = InventoryItem::Transaction(transaction);
        let s = serde_json::to_string(&inv_elem);
        let expected = r#"{"transaction":{"version":0,"inputs":[{"ValueTransfer":{"transaction_id":{"SHA256":[9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9]},"output_index":0}}],"outputs":[{"DataRequest":{"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"data_request":{"not_before":0,"retrieve":[{"kind":"HTTP-GET","url":"https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22","script":[0]},{"kind":"HTTP-GET","url":"https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22","script":[0]}],"aggregate":{"script":[0]},"consensus":{"script":[0]},"deliver":[{"kind":"HTTP-GET","url":"https://hooks.zapier.com/hooks/catch/3860543/l2awcd/"},{"kind":"HTTP-GET","url":"https://hooks.zapier.com/hooks/catch/3860543/l1awcw/"}]},"value":0,"witnesses":0,"backup_witnesses":0,"commit_fee":0,"reveal_fee":0,"tally_fee":0,"time_lock":0}}],"signatures":[{"signature":{"Secp256k1":{"r":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"s":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"v":0}},"public_key":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}]}}"#;
        assert_eq!(s.unwrap(), expected);
    }

    #[cfg(test)]
    use witnet_data_structures::chain::RADRequest;
    fn build_hardcoded_transaction(data_request: RADRequest) -> Transaction {
        use witnet_data_structures::chain::*;
        let signature = Signature::Secp256k1(Secp256k1Signature {
            r: [0; 32],
            s: [0; 32],
            v: 0,
        });
        let keyed_signatures = vec![KeyedSignature {
            public_key: [0; 32],
            signature,
        }];

        let value_transfer_input = Input::ValueTransfer(ValueTransferInput {
            transaction_id: Hash::SHA256([9; 32]),
            output_index: 0,
        });

        let data_request_output = Output::DataRequest(DataRequestOutput {
            backup_witnesses: 0,
            commit_fee: 0,
            data_request,
            pkh: [0; 20],
            reveal_fee: 0,
            tally_fee: 0,
            time_lock: 0,
            value: 0,
            witnesses: 0,
        });

        let inputs = vec![value_transfer_input];
        let outputs = vec![data_request_output];
        Transaction {
            inputs,
            signatures: keyed_signatures,
            outputs,
            version: 0,
        }
    }

    #[test]
    fn hash_str_format() {
        use witnet_data_structures::chain::Hash;
        let h = Hash::SHA256([0; 32]);
        let hash_array = serde_json::to_string(&h).unwrap();
        let h2 = serde_json::from_str(&hash_array).unwrap();
        assert_eq!(h, h2);
        let hash_str = r#""0000000000000000000000000000000000000000000000000000000000000000""#;
        let h3 = serde_json::from_str(hash_str).unwrap();
        assert_eq!(h, h3);
    }
}
