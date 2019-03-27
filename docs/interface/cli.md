# Command Line Interface (CLI)

The cli subcommand provides a human friendly command-line interface to the [JSON-RPC API][jsonrpc].

## Usage

See all the available options by running the help command.
`cargo run --` can be used to replace `witnet` in a development environment.

```sh
$ witnet cli --help
$ cargo run -- cli --help
```

The JSON-RPC server address is obtained from the [configuration file][configuration].
The path of this file can be set using the `-c` or `--config` flag.
This flag must appear after `cli`.

```sh
$ witnet cli -c witnet.toml getBlockChain
```

```text
$ witnet cli getBlockChain
Block for epoch #46924 had digest e706995269bfc4fb5f4ab9082765a1bdb48fc6e58cdf5f95621c9e3f849301ed
Block for epoch #46925 had digest 2dc469691916a862154eb92473278ea8591ace910ec7ecb560797cbb91fdc01e
```

If there is any error, the process will return a non-zero exit code.

```text
$ witnet cli getBlockChain
ERROR 2019-01-03T12:01:51Z: witnet: Error: Connection refused (os error 111)
```

The executable implements the usual logging API, which can be enabled using `RUST_LOG=witnet=debug`:

```text
$ RUST_LOG=witnet=debug witnet cli getBlockChain
 INFO 2019-01-03T12:04:43Z: witnet::json_rpc_client: Connecting to JSON-RPC server at 127.0.0.1:21338
ERROR 2019-01-03T12:04:43Z: witnet: Error: Connection refused (os error 111)
```

### Commands

#### raw

The `raw` command allows sending raw JSON-RPC requests from the command line.
It can be used in an interactive way: each line of user input will be sent
to the JSON-RPC server without any modifications:

```sh
$ witnet cli -c witnet.toml raw
```

Each block represents a method call:
the first line is a request, the second line is a response.

```js
hi
{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}
```
```js
{"jsonrpc": "2.0","method": "getBlockChain", "id": 1}
{"jsonrpc":"2.0","result":[[242037,"3f8c9ed0fa721e39de9483f61f290f76a541757a828e54a8d951101b1940c59a"]],"id":1}
```
```js
{"jsonrpc": "2.0","method": "someInvalidMethod", "id": 2}
{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":2}
```
```js
bye
{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}
```


Alternatively, the input can be read from a file using pipes, as is usual in Unix-like environments:

```text
$ cat get_block_chain.txt | witnet cli raw
{"jsonrpc":"2.0","result":[[242037,"3f8c9ed0fa721e39de9483f61f290f76a541757a828e54a8d951101b1940c59a"]],"id":1}
```

#### getBlockChain

Returns the hashes of all the blocks in the blockchain, one per line:

```text
$ witnet cli getBlockChain -c witnet_01.toml
Block for epoch #46924 had digest e706995269bfc4fb5f4ab9082765a1bdb48fc6e58cdf5f95621c9e3f849301ed
Block for epoch #46925 had digest 2dc469691916a862154eb92473278ea8591ace910ec7ecb560797cbb91fdc01e
```

There are two optional arguments: `epoch` and `limit`. For example, to get the
blocks for epochs `100-104`, use `epoch 100` and `limit 5`:

```sh
$ witnet cli getBlockChain -c witnet_01.toml 100 5
```

If a negative epoch is supplied, it is interpreted as "the last N epochs".
For instance, to get the block for the last epoch:

```sh
$ witnet cli getBlockChain -c witnet_01.toml -1
```

#### getBlock

Returns the block that matches the provided hash.

```sh
$ witnet cli getBlock <hash>
```

The hash of the block should be provided as a hexadecimal string.

##### Example

###### Request

```sh
$ witnet cli getBlock 2dca073973d87dba3beec0ac5a4aeef22ae97d310cbcd6694d0197b292347d71
```

###### Response
```js
{"jsonrpc":"2.0","result":{"block_header":{"beacon":{"checkpoint":279313,"hash_prev_block":{"SHA256":[72,57,249,156,218,72,75,103,227,231,101,175,220,170,167,221,26,113,75,32,38,46,116,180,119,254,66,83,239,73,45,186]}},"hash_merkle_root":{"SHA256":[213,120,146,54,165,218,119,82,142,198,232,156,45,174,34,203,107,87,171,204,108,233,223,198,186,218,93,102,190,186,216,27]},"version":0},"proof":{"block_sig":{"Secp256k1":{"r":[110,242,206,28,113,89,70,255,14,223,109,187,94,13,137,221,79,193,56,184,116,142,84,146,185,143,5,66,145,26,126,58],"s":[110,242,206,28,113,89,70,255,14,223,109,187,94,13,137,221,79,193,56,184,116,142,84,146,185,143,5,66,145,26,126,58],"v":0}},"influence":0},"txns":[{"inputs":[],"outputs":[{"ValueTransfer":{"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"value":50000000000}}],"signatures":[],"version":0}]},"id":"1"}
```

#### getOutput

Returns the output of the transaction that matches the provided output pointer.

```sh
$ witnet cli getOutput <output pointer>
```

The format of the `<output pointer>` argument is `<transaction id>:<output index>`, where:

- `transaction id`: Is the 32 hex digits of the transaction id
- `output index`: Is a number identifying the index of the output in the transaction

##### Example

###### Request

```sh
$ witnet cli getOutput 1234567890abcdef111111111111111111111111111111111111111111111111:1
```

###### Response

```js
{"jsonrpc":"2.0","result":{"DataRequest":{"backup_witnesses":0,"commit_fee":0,"data_request":{"aggregate":{"script":[0]},"consensus":{"script":[0]},"deliver":[{"kind":"HTTP-GET","url":"https://hooks.zapier.com/hooks/catch/3860543/l2awcd/"}],"not_before":0,"retrieve":[{"kind":"HTTP-GET","script":[0],"url":"https://openweathermap.org/data/2.5/weather?id=2950159&appid=b6907d289e10d714a6e88b30761fae22"}]},"pkh":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"reveal_fee":0,"tally_fee":0,"time_lock":0,"value":0,"witnesses":0}},"id":"1"}
```

[jsonrpc]: json-rpc/
[configuration]: ../configuration/toml-file/
