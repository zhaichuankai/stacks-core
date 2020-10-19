#![allow(unused_imports)]
// Copyright (C) 2013-2020 Blocstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

extern crate blockstack_lib;

use blockstack_lib::address::AddressHashMode;
use blockstack_lib::burnchains::Address;
use blockstack_lib::chainstate::stacks::{
    StacksAddress, StacksBlock, StacksPrivateKey, StacksPublicKey, StacksTransaction, StacksTransactionSigner,
    TokenTransferMemo, TransactionAuth, TransactionContractCall, TransactionPayload,
    TransactionSmartContract, TransactionSpendingCondition, TransactionVersion,
    C32_ADDRESS_VERSION_MAINNET_SINGLESIG, C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
};
use blockstack_lib::net::{Error as NetError, StacksMessageCodec};
use blockstack_lib::util::{hash::hex_bytes, hash::to_hex, log, strings::StacksString, retry::LogReader};
use blockstack_lib::vm;
use blockstack_lib::vm::{
    errors::{Error as ClarityError, RuntimeErrorType},
    types::PrincipalData,
    ClarityName, ContractName, Value,
};
use std::convert::TryFrom;
use std::io::prelude::*;
use std::io::Read;
use std::{env, fs, io};

use blockstack_lib::address::b58;
use blockstack_lib::burnchains::bitcoin::address::{
    ADDRESS_VERSION_MAINNET_SINGLESIG, ADDRESS_VERSION_TESTNET_SINGLESIG,
};

const TESTNET_CHAIN_ID: u32 = 0x80000000;
const MAINNET_CHAIN_ID: u32 = 0x00000001;

const USAGE: &str = "blockstack-cli (options) [method] [args...]

This CLI allows you to generate simple signed transactions for blockstack-core
to process.

This CLI has these methods:

  publish          used to generate and sign a contract publish transaction
  contract-call    used to generate and sign a contract-call transaction
  generate-sk      used to generate a secret key for transaction signing
  token-transfer   used to generate and sign a transfer transaction
  addresses        used to get both Bitcoin and Stacks addresses from a private key

For usage information on those methods, call `blockstack-cli [method] -h`

`blockstack-cli` accepts flag options as well:

   --testnet       instruct the transaction generator to use a testnet version byte instead of MAINNET (default)

";

const PUBLISH_USAGE: &str = "blockstack-cli (options) publish [publisher-secret-key-hex] [fee-rate] [nonce] [contract-name] [file-name.clar]

The publish command generates and signs a contract publish transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0";

const CALL_USAGE: &str = "blockstack-cli (options) contract-call [origin-secret-key-hex] [fee-rate] [nonce] [contract-publisher-address] [contract-name] [function-name] [args...]

The contract-call command generates and signs a contract-call transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0

Arguments are supplied in one of two ways: through script evaluation or via hex encoding
of the value serialization format. The method for supplying arguments is chosen by
prefacing each argument with a flag:

  -e  indicates the argument should be _evaluated_
  -x  indicates the argument that a serialized Clarity value is being passed (hex-serialized)

e.g.,

   blockstack-cli contract-call $secret_key 10 0 SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4 foo-contract \\
      transfer-fookens -e \\'SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4 \\
                       -e \"(+ 1 2)\" \\
                       -x 0000000000000000000000000000000001 \\
                       -x 050011deadbeef11ababffff11deadbeef11ababffff
";

const TOKEN_TRANSFER_USAGE: &str = "blockstack-cli (options) token-transfer [origin-secret-key-hex] [fee-rate] [nonce] [recipient-address] [amount] [memo] [args...]

The transfer command generates and signs a STX transfer transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0";

const GENERATE_USAGE: &str = "blockstack-cli (options) generate-sk

This method generates a secret key, outputting the hex encoding of the
secret key, the corresponding public key, and the corresponding P2PKH Stacks address.";

const ADDRESSES_USAGE: &str = "blockstack-cli (options) addresses [secret-key-hex]

The addresses command calculates both the Bitcoin and Stacks addresses from a secret key.
If successful, this command outputs both the Bitcoin and Stacks addresses to stdout, formatted
as JSON, and exits with code 0";

const DECODE_TRANSACTION_USAGE: &str = "blockstack-cli (options) decode-tx [transaction-hex-or-stdin]

The decode-tx command decodes a serialized Stacks transaction and prints it to stdout as JSON.
The transaction, if given, must be a hex string.  Alternatively, you may pass - instead, and the
raw binary transaction will be read from stdin";

const DECODE_BLOCK_USAGE: &str = "blockstack-cli (options) decode-block [block-path-or-stdin]

The decode-tx command decodes a serialized Stacks block and prints it to stdout as JSON.
The block, if given, must be a hex string.  Alternatively, you may pass - instead, and the
raw binary block will be read from stdin";

#[derive(Debug)]
enum CliError {
    ClarityRuntimeError(RuntimeErrorType),
    ClarityGeneralError(ClarityError),
    Message(String),
    Usage,
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::ClarityRuntimeError(e) => Some(e),
            CliError::ClarityGeneralError(e) => Some(e),
            _ => None,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::ClarityRuntimeError(e) => write!(f, "Clarity error: {:?}", e),
            CliError::ClarityGeneralError(e) => write!(f, "Clarity error: {}", e),
            CliError::Message(e) => write!(f, "{}", e),
            CliError::Usage => write!(f, "{}", USAGE),
        }
    }
}

impl From<&str> for CliError {
    fn from(value: &str) -> Self {
        CliError::Message(value.into())
    }
}

impl From<RuntimeErrorType> for CliError {
    fn from(value: RuntimeErrorType) -> Self {
        CliError::ClarityRuntimeError(value)
    }
}

impl From<ClarityError> for CliError {
    fn from(value: ClarityError) -> Self {
        CliError::ClarityGeneralError(value)
    }
}

impl From<NetError> for CliError {
    fn from(value: NetError) -> Self {
        CliError::Message(format!("Stacks NetError: {}", value))
    }
}

impl From<std::num::ParseIntError> for CliError {
    fn from(value: std::num::ParseIntError) -> Self {
        CliError::Message(format!("Failed to parse integer: {}", value))
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        CliError::Message(format!("IO error reading CLI input: {}", value))
    }
}

impl From<blockstack_lib::util::HexError> for CliError {
    fn from(value: blockstack_lib::util::HexError) -> Self {
        CliError::Message(format!("Bad hex string supplied: {}", value))
    }
}

impl From<blockstack_lib::vm::types::serialization::SerializationError> for CliError {
    fn from(value: blockstack_lib::vm::types::serialization::SerializationError) -> Self {
        CliError::Message(format!("Failed to deserialize: {}", value))
    }
}

fn make_contract_publish(
    contract_name: String,
    contract_content: String,
) -> Result<TransactionSmartContract, CliError> {
    let name = ContractName::try_from(contract_name)?;
    let code_body = StacksString::from_string(&contract_content)
        .ok_or("Non-legal characters in contract-content")?;
    Ok(TransactionSmartContract { name, code_body })
}

fn make_contract_call(
    contract_address: String,
    contract_name: String,
    function_name: String,
    function_args: Vec<Value>,
) -> Result<TransactionContractCall, CliError> {
    let address =
        StacksAddress::from_string(&contract_address).ok_or("Failed to parse contract address")?;
    let contract_name = ContractName::try_from(contract_name)?;
    let function_name = ClarityName::try_from(function_name)?;

    Ok(TransactionContractCall {
        address,
        contract_name,
        function_name,
        function_args,
    })
}

fn make_standard_single_sig_tx(
    version: TransactionVersion,
    chain_id: u32,
    payload: TransactionPayload,
    publicKey: &StacksPublicKey,
    nonce: u64,
    fee_rate: u64,
) -> StacksTransaction {
    let mut spending_condition =
        TransactionSpendingCondition::new_singlesig_p2pkh(publicKey.clone())
            .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(nonce);
    spending_condition.set_fee_rate(fee_rate);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut tx = StacksTransaction::new(version, auth, payload);
    tx.chain_id = chain_id;
    tx
}

fn sign_transaction_single_sig_standard(
    transaction: &str,
    secret_key: &StacksPrivateKey,
) -> Result<StacksTransaction, CliError> {
    let transaction =
        StacksTransaction::consensus_deserialize(&mut io::Cursor::new(&hex_bytes(transaction)?))?;

    let mut tx_signer = StacksTransactionSigner::new(&transaction);
    tx_signer.sign_origin(secret_key)?;

    Ok(tx_signer
        .get_tx()
        .ok_or("TX did not finish signing -- was this a standard single signature transaction?")?)
}

fn handle_contract_publish(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", PUBLISH_USAGE)));
    }
    if args.len() != 5 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            PUBLISH_USAGE
        )));
    }
    let sk_publisher = &args[0];
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let contract_name = &args[3];
    let contract_file = &args[4];

    let contract_contents = if contract_file == "-" {
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer)?;
        buffer
    } else {
        fs::read_to_string(contract_file)?
    };

    let sk_publisher = StacksPrivateKey::from_hex(sk_publisher)?;

    let payload = make_contract_publish(contract_name.clone(), contract_contents)?;
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload.into(),
        &StacksPublicKey::from_private(&sk_publisher),
        nonce,
        fee_rate,
    );
    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx =
        sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_publisher)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn handle_contract_call(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", CALL_USAGE)));
    }
    if args.len() < 6 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            CALL_USAGE
        )));
    }
    let sk_origin = &args[0];
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let contract_address = &args[3];
    let contract_name = &args[4];
    let function_name = &args[5];

    let val_args = &args[6..];

    if val_args.len() % 2 != 0 {
        return Err(
            "contract-call arguments must be supplied as a list of `-e ...` or `-x 0000...` pairs"
                .into(),
        );
    }

    let mut arg_iterator = 0;
    let mut values = Vec::new();
    while arg_iterator < val_args.len() {
        let eval_method = &val_args[arg_iterator];
        let input = &val_args[arg_iterator + 1];
        let value = match eval_method.as_str() {
            "-x" => {
                Value::try_deserialize_hex_untyped(input)?
            },
            "-e" => {
                vm::execute(input)?
                    .ok_or("Supplied argument did not evaluate to a Value")?
            },
            _ => {
                return Err("contract-call arguments must be supplied as a list of `-e ...` or `-x 0000...` pairs".into())
            }
        };

        values.push(value);
        arg_iterator += 2;
    }

    let sk_origin = StacksPrivateKey::from_hex(sk_origin)?;

    let payload = make_contract_call(
        contract_address.clone(),
        contract_name.clone(),
        function_name.clone(),
        values,
    )?;
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload.into(),
        &StacksPublicKey::from_private(&sk_origin),
        nonce,
        fee_rate,
    );

    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx = sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_origin)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn handle_token_transfer(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!(
            "USAGE:\n {}",
            TOKEN_TRANSFER_USAGE
        )));
    }
    if args.len() < 5 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            TOKEN_TRANSFER_USAGE
        )));
    }
    let sk_origin = StacksPrivateKey::from_hex(&args[0])?;
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let recipient_address =
        PrincipalData::parse(&args[3]).map_err(|_e| "Failed to parse recipient")?;
    let amount = &args[4].parse()?;
    let memo = {
        let mut memo = [0; 34];
        let mut bytes = if args.len() == 6 {
            args[5].as_bytes().to_vec()
        } else {
            vec![]
        };
        bytes.resize(34, 0);
        memo.copy_from_slice(&bytes);
        TokenTransferMemo(memo)
    };

    let payload = TransactionPayload::TokenTransfer(recipient_address, *amount, memo);
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload,
        &StacksPublicKey::from_private(&sk_origin),
        nonce,
        fee_rate,
    );
    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx = sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_origin)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn generate_secret_key(args: &[String], version: TransactionVersion) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", GENERATE_USAGE)));
    }

    let sk = StacksPrivateKey::new();
    let pk = StacksPublicKey::from_private(&sk);
    let version = match version {
        TransactionVersion::Mainnet => C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
        TransactionVersion::Testnet => C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
    };

    let address = StacksAddress::from_public_keys(
        version,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![pk.clone()],
    )
    .expect("Failed to generate address from public key");
    Ok(format!(
        "{{ 
  \"secretKey\": \"{}\",
  \"publicKey\": \"{}\",
  \"stacksAddress\": \"{}\"
}}",
        sk.to_hex(),
        pk.to_hex(),
        address.to_string()
    ))
}

fn get_addresses(args: &[String], version: TransactionVersion) -> Result<String, CliError> {
    if (args.len() >= 1 && args[0] == "-h") || args.len() != 1 {
        return Err(CliError::Message(format!("USAGE:\n {}", ADDRESSES_USAGE)));
    }

    let sk = StacksPrivateKey::from_hex(&args[0]).expect("Failed to load private key");

    let pk = StacksPublicKey::from_private(&sk);
    let c32_version = match version {
        TransactionVersion::Mainnet => C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
        TransactionVersion::Testnet => C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
    };

    let b58_version = match version {
        TransactionVersion::Mainnet => ADDRESS_VERSION_MAINNET_SINGLESIG,
        TransactionVersion::Testnet => ADDRESS_VERSION_TESTNET_SINGLESIG,
    };

    let stx_address = StacksAddress::from_public_keys(
        c32_version,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![pk.clone()],
    )
    .expect("Failed to generate address from public key");

    let mut b58_addr_slice = [0u8; 21];
    b58_addr_slice[0] = b58_version;
    b58_addr_slice[1..].copy_from_slice(&stx_address.bytes.0);
    let b58_address_string = b58::check_encode_slice(&b58_addr_slice);
    Ok(format!(
        "{{
    \"STX\": \"{}\",
    \"BTC\": \"{}\"
}}",
        &stx_address, &b58_address_string
    ))
}

fn decode_transaction(args: &[String], _version: TransactionVersion) -> Result<String, CliError> {
    if (args.len() >= 1 && args[0] == "-h") || args.len() != 1 {
        return Err(CliError::Message(format!("Usage: {}\n", DECODE_TRANSACTION_USAGE)));
    }

    let tx_str =
        if args[0] == "-" {
            // read from stdin
            let mut tx_str = Vec::new();
            io::stdin().read_to_end(&mut tx_str).expect("Failed to read transaction from stdin");
            tx_str
        }
        else {
            // given as a command-line arg
            hex_bytes(&args[0].clone())
                .expect("Failed to decode transaction: must be a hex string")
        };

    let mut cursor = io::Cursor::new(&tx_str);
    let mut debug_cursor = LogReader::from_reader(&mut cursor);

    match StacksTransaction::consensus_deserialize(&mut debug_cursor) {
        Ok(tx) => {
            Ok(serde_json::to_string(&tx).expect("Failed to serialize transaction to JSON"))
        },
        Err(e) => {
            let mut ret = String::new();
            ret.push_str(&format!("Failed to decode transaction: {:?}\n", &e));
            ret.push_str("Bytes consumed:\n");
            for buf in debug_cursor.log().iter() {
                ret.push_str(&format!("   {}", to_hex(buf)));
            }
            ret.push_str("\n");
            Ok(ret)
        }
    }
}

fn decode_block(args: &[String], _version: TransactionVersion) -> Result<String, CliError> {
    if (args.len() >= 1 && args[0] == "-h") || args.len() != 1 {
        return Err(CliError::Message(format!("Usage: {}\n", DECODE_BLOCK_USAGE)));
    }
    let block_data =
        if args[0] == "-" {
            // read from stdin
            let mut block_str = Vec::new();
            io::stdin().read_to_end(&mut block_str).expect("Failed to read block from stdin");
            block_str
        }
        else {
            // given as a command-line arg
            hex_bytes(&args[0].clone())
                .expect("Failed to decode block: must be a hex string")
        };

    let mut cursor = io::Cursor::new(&block_data);
    let mut debug_cursor = LogReader::from_reader(&mut cursor);

    match StacksBlock::consensus_deserialize(&mut debug_cursor) {
        Ok(block) => {
            Ok(serde_json::to_string(&block).expect("Failed to serialize block to JSON"))
        },
        Err(e) => {
            let mut ret = String::new();
            ret.push_str(&format!("Failed to decode block: {:?}\n", &e));
            ret.push_str("Bytes consumed:\n");
            for buf in debug_cursor.log().iter() {
                ret.push_str(&format!("   {}", to_hex(buf)));
            }
            ret.push_str("\n");
            Ok(ret)
        }
    }
}

fn main() {
    log::set_loglevel(log::LOG_DEBUG).unwrap();
    let mut argv: Vec<String> = env::args().collect();

    argv.remove(0);

    match main_handler(argv) {
        Ok(s) => {
            println!("{}", s);
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn main_handler(mut argv: Vec<String>) -> Result<String, CliError> {
    let tx_version = if let Some(ix) = argv.iter().position(|x| x == "--testnet") {
        argv.remove(ix);
        TransactionVersion::Testnet
    } else {
        TransactionVersion::Mainnet
    };

    let chain_id = if tx_version == TransactionVersion::Testnet {
        TESTNET_CHAIN_ID
    } else {
        MAINNET_CHAIN_ID
    };

    if let Some((method, args)) = argv.split_first() {
        match method.as_str() {
            "contract-call" => handle_contract_call(args, tx_version, chain_id),
            "publish" => handle_contract_publish(args, tx_version, chain_id),
            "token-transfer" => handle_token_transfer(args, tx_version, chain_id),
            "generate-sk" => generate_secret_key(args, tx_version),
            "addresses" => get_addresses(args, tx_version),
            "decode-tx" => decode_transaction(args, tx_version),
            "decode-block" => decode_block(args, tx_version),
            _ => Err(CliError::Usage),
        }
    } else {
        Err(CliError::Usage)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn generate_should_work() {
        assert!(main_handler(vec!["generate-sk".into(), "--testnet".into()]).is_ok());
        assert!(main_handler(vec!["generate-sk".into()]).is_ok());
        assert!(generate_secret_key(&vec!["-h".into()], TransactionVersion::Mainnet).is_err());
    }

    fn to_string_vec(x: &[&str]) -> Vec<String> {
        x.iter().map(|&x| x.into()).collect()
    }

    #[test]
    fn simple_publish() {
        let publish_args = [
            "publish",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "foo-contract",
            "./sample-contracts/tokens.clar",
        ];

        assert!(main_handler(to_string_vec(&publish_args)).is_ok());

        let publish_args = [
            "publish",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "foo-contract",
            "./sample-contracts/non-existent-tokens.clar",
        ];

        assert!(format!(
            "{}",
            main_handler(to_string_vec(&publish_args)).unwrap_err()
        )
        .contains("IO error"));
    }

    #[test]
    fn simple_token_transfer() {
        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
        ];

        assert!(main_handler(to_string_vec(&tt_args)).is_ok());

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
            "Memo",
        ];

        assert!(main_handler(to_string_vec(&tt_args)).is_ok());

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "-1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&tt_args)).unwrap_err())
                .contains("Failed to parse integer")
        );

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SX1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&tt_args)).unwrap_err())
                .contains("Failed to parse recipient")
        );
    }

    #[test]
    fn simple_cc() {
        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 1 0)",
            "-e",
            "2",
        ];

        let exec_1 = main_handler(to_string_vec(&cc_args)).unwrap();

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 0 1)",
            "-e",
            "(+ 1 1)",
        ];

        let exec_2 = main_handler(to_string_vec(&cc_args)).unwrap();

        assert_eq!(exec_1, exec_2);

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-x",
            "0000000000000000000000000000000001",
            "-x",
            "0000000000000000000000000000000002",
        ];

        let exec_3 = main_handler(to_string_vec(&cc_args)).unwrap();

        assert_eq!(exec_2, exec_3);

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 0 1)",
            "-e",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("arguments must be supplied as")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(/ 1 0)",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("Clarity error")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "quryey",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("parse integer")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000fz",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("Failed to decode hex")
        );

        let sk = StacksPrivateKey::new();
        let s = format!(
            "{}",
            sign_transaction_single_sig_standard("01zz", &sk).unwrap_err()
        );
        println!("{}", s);
        assert!(s.contains("Bad hex string"));

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-x",
            "1010",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("deserialize")
        );
    }

    #[test]
    fn simple_addresses() {
        let addr_args = [
            "addresses",
            "2945c6be8758994652a498f0445d534d0fadb0b2025b37c72297b059ebf887ed01",
        ];

        let result = main_handler(to_string_vec(&addr_args)).unwrap();
        assert!(result.contains("SP36T883PDD2EK4PHVTA5GFHC8NQW6558XG7YX1GD"));
        assert!(result.contains("1KkL94EPD3mz7RFCZPmRBy3KjbWZ4qo58E"));

        let addr_args = [
            "--testnet",
            "addresses",
            "2945c6be8758994652a498f0445d534d0fadb0b2025b37c72297b059ebf887ed01",
        ];

        let result = main_handler(to_string_vec(&addr_args)).unwrap();
        assert!(result.contains("mzGHS7KN25DEtXipGxjo1tFebb7Fw5aAkp"));
        assert!(result.contains("ST36T883PDD2EK4PHVTA5GFHC8NQW6558XJQX6Q3K"));
    }

    #[test]
    fn simple_decode_tx() {
        let tx_args = [
            "decode-tx",
            "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe4000000000000000100000000000000000100c90ae0235365f3a73c595f8c6ab3c529807feb3cb269247329c9a24218d50d3f34c7eef5d28ba26831affa652a73ec32f098fec4bf1decd1ceb3fde4b8ce216b030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265096765742d76616c7565000000010d00000003666f6f"
        ];

        let result = main_handler(to_string_vec(&tx_args)).unwrap();
        eprintln!("result:\n{}", result);
    }

    #[test]
    fn simple_decode_block() {
        let block_args = [
            "decode-block",
            "000000000000395f800000000000000179cb51f6bbd6d90cb257616e77a495919667c3772dd08ea7c4f5c372739490bc91da6609c5c95c96f612dbc8cab2f7a0d8bfb83abdb630167579ccc36b66c03c1d0d250cd3b3615c03afcdaef313dbd30d3d5b0fd10ed5acbc35d042abfba66cdfc32881c5a665ad9685a2eb6e0c131fb400000000000000000000000000000000000000000000000000000000000000000000e87f28593f66d77ae3c57abd4e5ae0e632b837b2596be14c2b2572cd4d0015229976eb5c4a5b08816b31f485513d2e6501f6cd29ee240a2c4056b1f7cc32c2e118ef6499e0fcc575da75fca8cc409e5c884eb3450000000180800000000400403e2ff80a8a8ecacfb827dcf6adddd21fdd4c3c000000000000017800000000000000000000f3f497268f8a12e318f96ba4f1ad3ed2485e87cefe75b88bf735bb1bbb7db754746e6a244ba869183a2ab73002c6465936b7d9b059ffc5a94488bee7b5afb33c010200000000040000000000000000000000000000000000000000000000000000000000000000",
        ];

        let result = main_handler(to_string_vec(&block_args)).unwrap();
        eprintln!("result:\n{}", result);
    }
}
