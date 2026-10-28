//! `ksana-cli`: the Ksana command-line tool (SPEC §11.4): keys, op submission over path B / C (path A was deleted,
//! D-078), top-up (the lock flow of D-085), contract deployment and calls, state queries; mainly for the integration
//! test scripts and the devnet.
//!
//! ```sh
//! ksana-cli key alice.key  # generate or read a secret key, print the account (i.e. the Taproot address, D-092)
//! ksana-cli --rpc http://127.0.0.1:18545 account <account>  # query an account (Taproot address or 64 hex digits)
//! ksana-cli transfer --key alice.key --to <account> --amount 5 --fee-token 1 --wait          # path B
//! ksana-cli transfer --key alice.key --to <account> --amount 5 --native --btc-wallet w     # path C
//! ksana-cli exchange lock --key seller.key --fee-token 1 --exchange <contract> --ksa 3000000 --price 100 \
//!     --script <payee script hex> --beneficiary <buyer account> --expiry <BTC height> --fund 3000000 --wait   # lock (seller, path B)
//! ksana-cli top-up --exchange <contract> --lock 0 --beneficiary <buyer account> --sats 30000 --btc-wallet w --wait   # top-up (buyer, path C)
//! ksana-cli exchange release --key seller.key --fee-token 1 --exchange <contract> --lock 0  # release an expired, unpaid lock
//! ksana-cli exchange show --exchange <contract> --lock 0                                     # show a lock
//! ksana-cli token <contract>                # a KS-20 token or a KS-721 collection by the node's index (D-120)
//! ksana-cli approvals <account>             # the allowances and operators an account gave (D-120)
//! ```
//!
//! The secret key file is the same as `ksana-keytool`'s: 64 hex digits. The account is that key's Taproot address
//! (BIP86 output key, D-092): every argument taking an account accepts a Taproot address of this chain's network
//! (`bcrt1p…` / `tb1p…`) or 64 hex digits (a contract ID only has the hex form). Query results and submission results
//! are both printed to standard output as JSON.
//!
//! The network is the node's: every command but `key` first reads the node's chain ID (`ksana_getChainInfo`) and signs
//! for that network; `--chain` only states the network expected and refuses a node on another one. Path C and
//! top-up also refuse a bitcoind on another BTC network (A2-11, D-106).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::consensus::encode::{deserialize_hex, serialize, serialize_hex};
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{absolute, transaction, Address, Amount, Network, Transaction, TxOut, Txid};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use corepc_client::client_sync::v28::Client as BtcRpc;
use corepc_client::client_sync::Auth;
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::data::data_script;
use ksana_primitives::{
    check_funds, Action, FeeMode, Funds, OutPoint, Params, Payload, SigScheme, VmOp, KSA_ASSET_ID,
};
use ksana_rpc::{codes, AccountAssetJson, CallRequestJson, KsanaApiClient, OpState, MAX_OPS_LIMIT};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "ksana-cli",
    version,
    about = "Ksana command-line tool (SPEC §11.4)"
)]
struct Cli {
    /// Fast-lane network expected: `devnet`, `signet` (D-055) or `mainnet` (D-096). The VmOp chain ID and the BTC
    /// network of account addresses are always the node's (its `ksana_getChainInfo` chain ID); with `--chain`, a node on
    /// another network is refused before anything is signed or sent (A2-11, D-106). `key` does not talk to the node:
    /// there it only picks the address network (devnet by default).
    #[arg(long, global = true, value_enum)]
    chain: Option<ChainArg>,
    /// JSON-RPC address of the fast-lane node.
    #[arg(
        long,
        global = true,
        env = "KSANA_RPC",
        default_value = "http://127.0.0.1:8545"
    )]
    rpc: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate (when the file does not exist, mode 0600) or read a secret key; print the account (Taproot output key)
    /// and its address (the same bytes, D-092).
    Key {
        /// Secret key file (64 hex digits).
        file: PathBuf,
        /// Network of the address; defaults according to `--chain` (regtest for devnet, the default, signet for
        /// signet, BTC mainnet for mainnet).
        #[arg(long, value_enum)]
        network: Option<Net>,
    },
    #[command(flatten)]
    Node(Box<NodeCommand>),
}

/// The commands that talk to the node: each first reads the node's chain ID (A2-11, D-106).
#[derive(Subcommand)]
enum NodeCommand {
    /// Query an account: nonce, balance of each asset, sponsored fees and block rewards not yet unlocked, and the KS-20
    /// tokens (`tokens`) and KS-721 NFTs (`nfts`, a count by collection) it holds by the node's index of the standards'
    /// events (D-120; left out with a node without the index).
    Account {
        /// Account (Taproot address or 64 hex digits), or `@<secret key file>`.
        account: String,
    },
    /// Query a contract: the hash of its code, its deployer and the standards it declared (`ksana_getContract`, D-120).
    Contract {
        /// Contract ID (64 hex digits).
        contract: String,
    },
    /// Query a KS-20 token or a KS-721 collection by the node's index: standard and extensions, metadata, supply and
    /// number of holders (`ksana_getToken`, D-120).
    Token {
        /// Contract ID (64 hex digits).
        contract: String,
    },
    /// A page of a token's or a collection's holders, the largest balance first (`ksana_getTokenHolders`, D-120).
    Holders {
        /// Contract ID (64 hex digits).
        contract: String,
        #[command(flatten)]
        page: PageArgs,
    },
    /// A page of the NFTs an account holds, by collection and token ID (`ksana_getNfts`, D-120).
    Nfts {
        /// Account (Taproot address or 64 hex digits), or `@<secret key file>`.
        account: String,
        /// Only the NFTs of this collection (64 hex digits).
        #[arg(long)]
        contract: Option<String>,
        #[command(flatten)]
        page: PageArgs,
    },
    /// A page of the approvals an account gave: KS-20 allowances, KS-721 operators and the approved accounts of its
    /// NFTs, as their latest events set them, expired or not (`ksana_getApprovals`, D-120). What is left of an
    /// allowance is the token's `allowance` view (`query`).
    Approvals {
        /// Account (Taproot address or 64 hex digits), or `@<secret key file>`.
        account: String,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Query an asset: name, symbol, decimals, total supply, issuing contract, the capabilities it holds and the pause
    /// (`ksana_getAsset`, D-074, D-121; defaults to KSA).
    Asset {
        /// Asset ID (64 hex digits, defaults to KSA).
        #[arg(default_value = "0000000000000000000000000000000000000000000000000000000000000000")]
        asset: String,
    },
    /// Query an account's standing in an asset: its status flags and whether it may send and receive the asset now
    /// (`ksana_getAssetStatus`, D-121).
    AssetStatus {
        /// Asset ID (64 hex digits).
        asset: String,
        /// Account (Taproot address or 64 hex digits), or `@<secret key file>`.
        account: String,
    },
    /// Query an op's status.
    Op {
        /// Op ID (64 hex digits).
        op_id: String,
    },
    /// Query a block: `latest`, a decimal height or a 64-hex-digit hash.
    Block { id: String },
    /// Query the settlement transaction a block carries and its BTC status.
    Settlement { id: String },
    /// Suggested token amount for path B and the node's BTC fee rate (`ksana_estimateFee`, D-054).
    EstimateFee,
    /// Read-only contract call (`ksana_call`, not submitted to the chain).
    Query {
        /// Contract ID (64 hex digits).
        #[arg(long)]
        contract: String,
        /// Method name.
        #[arg(long)]
        method: String,
        /// Borsh-encoded arguments (hex).
        #[arg(long, default_value = "")]
        args: String,
        /// Simulated caller (Taproot address or 64 hex digits).
        #[arg(long)]
        caller: Option<String>,
        /// Attached funds (D-073, repeatable): `<amount>` for KSA, `<asset ID>:<amount>` for other assets.
        #[arg(long = "fund")]
        funds: Vec<String>,
    },
    /// Transfer an asset (KSA by default).
    Transfer {
        #[command(flatten)]
        tx: TxArgs,
        /// Receiving account (Taproot address or 64 hex digits), or `@<secret key file>`.
        #[arg(long)]
        to: String,
        /// Amount.
        #[arg(long)]
        amount: u128,
        /// Asset ID (64 hex digits, KSA by default).
        #[arg(long)]
        asset: Option<String>,
    },
    /// Deploy a contract.
    Deploy {
        #[command(flatten)]
        tx: TxArgs,
        /// WASM file.
        #[arg(long)]
        code: PathBuf,
        /// Arguments of `init` (hex).
        #[arg(long, default_value = "")]
        init_args: String,
        /// Attached funds (D-073, repeatable): `<amount>` for KSA, `<asset ID>:<amount>` for other assets.
        #[arg(long = "fund")]
        funds: Vec<String>,
    },
    /// Top-up (D-078, D-085): one BTC transaction pays a lock of the exchange contract and carries the claim op at the
    /// same time (path C, signed with a one-time key, nonce 0), funded and broadcast by the bitcoind wallet; once BTC
    /// confirms it, the KSA is credited to the beneficiary account fixed when locking. The lock is read first: refuses
    /// when the beneficiary is not `--beneficiary`, the lock is not Open, fewer than `--expiry-margin` BTC blocks are
    /// left before expiry, or the payment differs from the sats needed to get all the KSA still locked (overpaying buys no
    /// more, `--allow-overpay` allows it; underpaying buys only a part and the rest stays locked until another payment
    /// buys it or the seller releases it after the expiry, `--allow-partial` allows it).
    TopUp {
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// Lock number.
        #[arg(long)]
        lock: u64,
        /// Buyer account (Taproot address or 64 hex digits): must equal the lock's beneficiary, otherwise the KSA is
        /// credited to someone else.
        #[arg(long)]
        beneficiary: String,
        /// Amount paid to the lock's payee script (sats).
        #[arg(long)]
        sats: u64,
        /// Allow paying more than needed to get all the KSA (the excess buys no more).
        #[arg(long)]
        allow_overpay: bool,
        /// Allow paying less than needed to get all the KSA still locked (buys only a part; the rest stays locked for the
        /// beneficiary until another payment buys it or the seller releases it after the expiry).
        #[arg(long)]
        allow_partial: bool,
        /// Expiry safety margin (BTC blocks): the payment is refused when the current BTC height plus it reaches the
        /// lock's expiry height.
        #[arg(long, default_value_t = 3)]
        expiry_margin: u32,
        #[command(flatten)]
        btc: BtcArgs,
        /// Wait for the claim op to be executed (needs a BTC block and confirmation).
        #[arg(long)]
        wait: bool,
        /// Timeout of `--wait` (seconds).
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Seller operations and queries of the exchange contract (example `ksa-exchange`, D-085).
    Exchange {
        #[command(subcommand)]
        command: ExchangeCommand,
    },
    /// Call a contract.
    Call {
        #[command(flatten)]
        tx: TxArgs,
        /// Contract ID (64 hex digits).
        #[arg(long)]
        contract: String,
        /// Method name.
        #[arg(long)]
        method: String,
        /// Borsh-encoded arguments (hex).
        #[arg(long, default_value = "")]
        args: String,
        /// Attached funds (D-073, repeatable): `<amount>` for KSA, `<asset ID>:<amount>` for other assets.
        #[arg(long = "fund")]
        funds: Vec<String>,
    },
}

/// Seller operations (path B) and read-only queries of the exchange contract.
#[derive(Subcommand)]
enum ExchangeCommand {
    /// Deposit KSA into the seller's available balance in the contract (`deposit`, with attached KSA).
    Deposit {
        #[command(flatten)]
        tx: TxArgs,
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// KSA to deposit (base units).
        #[arg(long)]
        amount: u128,
    },
    /// Withdraw KSA from the available balance (`withdraw`, does not affect any lock).
    Withdraw {
        #[command(flatten)]
        tx: TxArgs,
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// KSA to withdraw (base units).
        #[arg(long)]
        amount: u128,
    },
    /// Lock (`lock`): locks KSA from the available balance (the part attached with `--fund` is credited first) for the
    /// buyer, fixing the price, a payee script used only for this lock and the expiry BTC height; with `--wait` and a
    /// successful execution, `lock` in the result is the lock number.
    Lock {
        #[command(flatten)]
        tx: TxArgs,
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// KSA to lock (base units).
        #[arg(long)]
        ksa: u128,
        /// Price: how many KSA base units per sat.
        #[arg(long)]
        price: u128,
        /// Payee script (hex); either this or `--address`.
        #[arg(long)]
        script: Option<String>,
        /// Payee address (resolved to a script according to `--network`, any standard address type); either this or
        /// `--script`.
        #[arg(long)]
        address: Option<String>,
        /// Network of `--address`; defaults to the BTC network of the node's chain.
        #[arg(long, value_enum)]
        network: Option<Net>,
        /// Beneficiary account (the buyer, Taproot address or 64 hex digits), or `@<secret key file>`.
        #[arg(long)]
        beneficiary: String,
        /// Expiry BTC height (must be greater than the BTC height anchored by the current block).
        #[arg(long)]
        expiry: u32,
        /// KSA attached to the call (goes into the available balance first).
        #[arg(long)]
        fund: Option<u128>,
    },
    /// Release (`release`): a lock that expired, passed the release delay and is still unclaimed; the KSA returns to
    /// the seller's available balance.
    Release {
        #[command(flatten)]
        tx: TxArgs,
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// Lock number.
        #[arg(long)]
        lock: u64,
    },
    /// Show a lock (read-only call of `get_lock`).
    Show {
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// Lock number.
        #[arg(long)]
        lock: u64,
    },
    /// Show the seller's available balance in the contract (read-only call of `balance`).
    Balance {
        /// Exchange contract ID (64 hex digits).
        #[arg(long)]
        exchange: String,
        /// Seller account (Taproot address or 64 hex digits), or `@<secret key file>`.
        #[arg(long)]
        seller: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ChainArg {
    Devnet,
    Signet,
    Mainnet,
}

impl ChainArg {
    fn network(self) -> ksana_primitives::Network {
        match self {
            ChainArg::Devnet => ksana_primitives::Network::Devnet,
            ChainArg::Signet => ksana_primitives::Network::Signet,
            ChainArg::Mainnet => ksana_primitives::Network::Mainnet,
        }
    }
}

/// The node at `--rpc` and the network it is on.
struct Node {
    client: HttpClient,
    network: ksana_primitives::Network,
}

impl Node {
    /// Connects to the node at `url` and takes its network from its chain ID (`ksana_getChainInfo`), refusing a node on
    /// another network than `chain` (A2-11, D-106). Commands call this before they read, sign or send anything.
    async fn connect(url: &str, chain: Option<ChainArg>) -> Result<Node> {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(url)
            .with_context(|| format!("connecting to {url}"))?;
        let info = client
            .get_chain_info()
            .await
            .with_context(|| format!("reading the chain ID of the node at {url}"))?;
        let network = node_network(url, info.chain_id, chain)?;
        Ok(Node { client, network })
    }

    fn params(&self) -> &'static Params {
        self.network.params()
    }

    /// BTC network of account addresses (D-092) and of path C transactions.
    fn btc_net(&self) -> Network {
        ksana_crypto::btc_network(self.network)
    }
}

/// The network of the node at `url`, which reports `chain_id`: one of the three networks, and the one `--chain` names
/// when given. An op signed with another network's chain ID is still included by the node when it comes as path C,
/// and fails there while its BTC payment confirms (A2-11).
fn node_network(
    url: &str,
    chain_id: u32,
    chain: Option<ChainArg>,
) -> Result<ksana_primitives::Network> {
    let node = ksana_primitives::Network::from_chain_id(chain_id).with_context(|| {
        let known: Vec<String> = ksana_primitives::Network::ALL
            .iter()
            .map(|n| format!("{} {}", n.name(), n.params().chain_id))
            .collect();
        format!(
            "the node at {url} reports chain ID {chain_id}, which belongs to no Ksana network ({})",
            known.join(", ")
        )
    })?;
    if let Some(chain) = chain {
        let expected = chain.network();
        ensure!(
            node == expected,
            "the node at {url} is on {} (chain ID {chain_id}), not {} (--chain): refusing to sign for one network and \
             send to another; drop --chain to use the node's network, or point --rpc at a {} node",
            node.name(),
            expected.name(),
            expected.name()
        );
    }
    Ok(node)
}

#[derive(Clone, Copy, ValueEnum)]
enum SigKind {
    Bip340,
    Bip322,
}

#[derive(Clone, Copy, ValueEnum)]
enum Net {
    Regtest,
    Signet,
    Testnet,
    Mainnet,
}

impl From<Net> for Network {
    fn from(n: Net) -> Network {
        match n {
            Net::Regtest => Network::Regtest,
            Net::Signet => Network::Signet,
            Net::Testnet => Network::Testnet,
            Net::Mainnet => Network::Bitcoin,
        }
    }
}

/// A page of a listing of the node's index of the standards' events (D-120).
#[derive(Args)]
struct PageArgs {
    /// Continue after an earlier page: its `next`.
    #[arg(long)]
    after: Option<String>,
    /// At most this many items, 1 to 500 (the node's default is 50).
    #[arg(long)]
    limit: Option<u32>,
}

/// Common arguments for submitting an op. Exactly one fee mode: `--fee-token` (path B) or `--native` (path C).
#[derive(Args)]
#[command(group(ArgGroup::new("fee").required(true).args(["fee_token", "native"])))]
struct TxArgs {
    /// Signing secret key file (64 hex digits, the same as ksana-keytool).
    #[arg(long)]
    key: PathBuf,
    /// nonce; defaults to the account's next nonce on the node.
    #[arg(long)]
    nonce: Option<u64>,
    /// Execution limit (gas).
    #[arg(long, default_value_t = 1_000_000)]
    max_gas: u64,
    /// Path B: sponsored fee, the amount of KSA paid to the producer.
    #[arg(long)]
    fee_token: Option<u128>,
    /// Signature scheme of the VmOp: `bip340` (default) or `bip322` (the browser wallets' scheme: a BIP322 simple
    /// signature over the account's Taproot address, D-052, D-092).
    #[arg(long, value_enum, default_value_t = SigKind::Bip340)]
    sig: SigKind,
    /// Path C: a native transaction, funded and broadcast by the bitcoind wallet.
    #[arg(long)]
    native: bool,
    #[command(flatten)]
    btc: BtcArgs,
    /// Wait for the op to be executed (path C also needs a BTC block and confirmation; an op skipped by a merged block
    /// keeps waiting until it is included again).
    #[arg(long)]
    wait: bool,
    /// Timeout of `--wait` (seconds).
    #[arg(long, default_value_t = 120)]
    timeout: u64,
}

/// bitcoind (funding and broadcasting for path C and top-up).
#[derive(Args)]
struct BtcArgs {
    /// bitcoind RPC.
    #[arg(
        long,
        env = "KSANA_BTC_RPC_URL",
        default_value = "http://127.0.0.1:18443"
    )]
    btc_rpc_url: String,
    /// bitcoind RPC user name.
    #[arg(long, env = "KSANA_BTC_RPC_USER", default_value = "ksana")]
    btc_rpc_user: String,
    /// bitcoind RPC password.
    #[arg(long, env = "KSANA_BTC_RPC_PASS", default_value = "ksana")]
    btc_rpc_pass: String,
    /// bitcoind wallet that funds (defaults to the default wallet).
    #[arg(long)]
    btc_wallet: Option<String>,
    /// BTC fee rate (sat/vB).
    #[arg(long, default_value_t = 2)]
    btc_fee_rate: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ksana-cli: failed to start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ksana-cli: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<Value> {
    match cli.command {
        // `key` does not talk to the node: `--chain` (devnet by default) only picks the address network
        Command::Key { file, network } => {
            let chain = cli
                .chain
                .map_or(ksana_primitives::Network::Devnet, ChainArg::network);
            key_info(
                &file,
                network.map_or(ksana_crypto::btc_network(chain), Into::into),
            )
        }
        Command::Node(command) => {
            node_command(&Node::connect(&cli.rpc, cli.chain).await?, *command).await
        }
    }
}

/// Runs a command against the node, signing for the node's network.
async fn node_command(node: &Node, command: NodeCommand) -> Result<Value> {
    let (client, btc_net) = (&node.client, node.btc_net());
    match command {
        NodeCommand::Account { account } => {
            let account = parse_account(&account, btc_net)?;
            let mut info = json!(client.get_account(hex::encode(account)).await?);
            if let Ok(address) = ksana_crypto::account_address(&account, btc_net) {
                info["address"] = json!(address.to_string());
            }
            if let Some((tokens, nfts)) = contract_assets(client, &account).await? {
                info["tokens"] = json!(tokens);
                info["nfts"] = json!(nfts);
            }
            Ok(info)
        }
        NodeCommand::Contract { contract } => {
            let contract = parse_id(&contract, "contract ID")?;
            Ok(json!(client.get_contract(hex::encode(contract)).await?))
        }
        NodeCommand::Token { contract } => {
            let contract = parse_id(&contract, "contract ID")?;
            Ok(json!(client.get_token(hex::encode(contract)).await?))
        }
        NodeCommand::Holders { contract, page } => {
            let contract = parse_id(&contract, "contract ID")?;
            Ok(json!(
                client
                    .get_token_holders(hex::encode(contract), page.after, page.limit)
                    .await?
            ))
        }
        NodeCommand::Nfts {
            account,
            contract,
            page,
        } => {
            let account = parse_account(&account, btc_net)?;
            let contract = contract
                .map(|c| parse_id(&c, "contract ID").map(hex::encode))
                .transpose()?;
            Ok(json!(
                client
                    .get_nfts(hex::encode(account), contract, page.after, page.limit)
                    .await?
            ))
        }
        NodeCommand::Approvals { account, page } => {
            let account = parse_account(&account, btc_net)?;
            Ok(json!(
                client
                    .get_approvals(hex::encode(account), page.after, page.limit)
                    .await?
            ))
        }
        NodeCommand::Asset { asset } => Ok(json!(client.get_asset(asset).await?)),
        NodeCommand::AssetStatus { asset, account } => {
            let account = hex::encode(parse_account(&account, btc_net)?);
            Ok(json!(client.get_asset_status(asset, account).await?))
        }
        NodeCommand::Op { op_id } => Ok(json!(client.get_op_status(op_id).await?)),
        NodeCommand::Block { id } => Ok(json!(client.get_block(id).await?)),
        NodeCommand::Settlement { id } => Ok(json!(client.get_settlement(id).await?)),
        NodeCommand::EstimateFee => Ok(json!(client.estimate_fee().await?)),
        NodeCommand::Query {
            contract,
            method,
            args,
            caller,
            funds,
        } => {
            let request = CallRequestJson {
                contract: parse_id(&contract, "contract ID")?,
                method,
                args: parse_hex(&args, "arguments")?,
                funds: parse_funds(&funds)?,
                caller: caller.map(|c| parse_account(&c, btc_net)).transpose()?,
                max_gas: None,
            };
            Ok(json!(client.call(request).await?))
        }
        NodeCommand::Transfer {
            tx,
            to,
            amount,
            asset,
        } => {
            let action = Action::AssetTransfer {
                asset: asset
                    .map(|a| parse_id(&a, "asset ID"))
                    .transpose()?
                    .unwrap_or(KSA_ASSET_ID),
                to: parse_account(&to, btc_net)?,
                amount,
            };
            submit(node, tx, action).await
        }
        NodeCommand::Deploy {
            tx,
            code,
            init_args,
            funds,
        } => {
            let code =
                std::fs::read(&code).with_context(|| format!("reading {}", code.display()))?;
            let action = Action::Deploy {
                code,
                init_args: parse_hex(&init_args, "init arguments")?,
                funds: parse_funds(&funds)?,
            };
            submit(node, tx, action).await
        }
        NodeCommand::TopUp {
            exchange,
            lock,
            beneficiary,
            sats,
            allow_overpay,
            allow_partial,
            expiry_margin,
            btc,
            wait,
            timeout,
        } => {
            let exchange = parse_id(&exchange, "exchange contract ID")?;
            let beneficiary = parse_account(&beneficiary, btc_net)?;
            top_up(
                node,
                TopUp {
                    exchange,
                    lock,
                    beneficiary,
                    sats,
                    allow_overpay,
                    allow_partial,
                    expiry_margin,
                },
                &btc,
                wait.then_some(Duration::from_secs(timeout)),
            )
            .await
        }
        NodeCommand::Exchange { command } => exchange(node, command).await,
        NodeCommand::Call {
            tx,
            contract,
            method,
            args,
            funds,
        } => {
            let action = Action::Call {
                contract: parse_id(&contract, "contract ID")?,
                method,
                args: parse_hex(&args, "arguments")?,
                funds: parse_funds(&funds)?,
            };
            submit(node, tx, action).await
        }
    }
}

/// Signs and submits an op; with `--wait`, waits until it is included.
async fn submit(node: &Node, args: TxArgs, action: Action) -> Result<Value> {
    let (client, params) = (&node.client, node.params());
    let keypair = load_keypair(&args.key)?;
    let account = ksana_crypto::account_of(&keypair);
    let nonce = match args.nonce {
        Some(n) => n,
        None => client.get_account(hex::encode(account)).await?.nonce,
    };
    let fee = match args.fee_token {
        Some(amount) => FeeMode::Token { amount },
        None => FeeMode::BtcNative,
    };
    let mut op = VmOp {
        version: ksana_primitives::VMOP_VERSION,
        chain_id: params.chain_id,
        account,
        nonce,
        fee,
        max_gas: args.max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    let aux = random32()?;
    // a path C signature binds the carrying transaction's first input (D-086): signs over it when `first_input` is
    // `Some`
    let sign = |op: &VmOp, first_input: Option<&OutPoint>| -> Result<SigScheme> {
        match (args.sig, first_input) {
            (SigKind::Bip340, None) => ksana_crypto::sign_vmop(op, &keypair, &aux),
            (SigKind::Bip340, Some(input)) => {
                ksana_crypto::sign_native_vmop(op, input, &keypair, &aux)
            }
            (SigKind::Bip322, None) => ksana_crypto::sign_vmop_bip322_p2tr(op, &keypair, &aux),
            (SigKind::Bip322, Some(input)) => {
                ksana_crypto::sign_native_vmop_bip322_p2tr(op, input, &keypair, &aux)
            }
        }
        .map_err(|e| anyhow!("signing failed: {e}"))
    };

    let mut result = json!({ "account": hex::encode(account), "nonce": nonce.to_string() });
    match &op.fee {
        FeeMode::Token { .. } => {
            op.sig = sign(&op, None)?;
            let encoded = op.encode().map_err(|e| anyhow!("{e}"))?;
            client.submit_op(hex::encode(&encoded)).await?;
        }
        FeeMode::BtcNative => {
            let unsigned = op.clone();
            let (tx, signed, _) = send_native(
                node,
                &args.btc,
                &|first_input| {
                    let mut op = unsigned.clone();
                    op.sig = sign(&op, Some(first_input))?;
                    Ok(op)
                },
                &[],
            )?;
            op = signed;
            result["btc_txid"] = json!(tx.compute_txid().to_string());
        }
    }
    let op_id = op.op_id().map_err(|e| anyhow!("{e}"))?;
    result["op_id"] = json!(hex::encode(op_id));
    if args.wait {
        result["status"] = wait_included(client, &op_id, Duration::from_secs(args.timeout)).await?;
    }
    Ok(result)
}

/// State of a lock (`LockState` of `contracts/examples/ksa-exchange`).
#[derive(borsh::BorshDeserialize, PartialEq, Eq)]
enum LockState {
    Open,
    Claimed { paid_sats: u64, ksa_paid: u128 },
    Released,
}

impl LockState {
    fn json(&self) -> Value {
        match self {
            LockState::Open => json!("open"),
            LockState::Claimed {
                paid_sats,
                ksa_paid,
            } => json!({
                "claimed": { "paid_sats": paid_sats.to_string(), "ksa_paid": ksa_paid.to_string() }
            }),
            LockState::Released => json!("released"),
        }
    }
}

/// A lock of the exchange contract (the return value of the contract method `get_lock`, `Lock` of
/// `contracts/examples/ksa-exchange`).
#[derive(borsh::BorshDeserialize)]
struct ExchangeLock {
    seller: [u8; 32],
    beneficiary: [u8; 32],
    script: Vec<u8>,
    price: u128,
    ksa: u128,
    expiry: u32,
    state: LockState,
}

/// Reads a lock (read-only call of `get_lock`).
async fn read_lock(client: &HttpClient, exchange: [u8; 32], lock: u64) -> Result<ExchangeLock> {
    let receipt = client
        .call(CallRequestJson {
            contract: exchange,
            method: "get_lock".into(),
            args: lock.to_le_bytes().to_vec(),
            funds: Vec::new(),
            caller: None,
            max_gas: None,
        })
        .await?;
    ensure!(
        receipt.success,
        "reading lock {lock} failed: {}",
        receipt.error.unwrap_or_default()
    );
    let lock: ExchangeLock = borsh::from_slice(&receipt.output)
        .context("the lock encoding does not match the exchange contract")?;
    ensure!(
        lock.price > 0,
        "the lock's price is 0, contrary to the exchange contract's rules"
    );
    Ok(lock)
}

/// The minimum sats needed to get all the locked KSA: `ceil(ksa / price)` (`price > 0`).
fn sats_for_all(lock: &ExchangeLock) -> u128 {
    lock.ksa.div_ceil(lock.price)
}

fn lock_json(id: u64, lock: &ExchangeLock) -> Value {
    json!({
        "lock": id.to_string(),
        "seller": hex::encode(lock.seller),
        "beneficiary": hex::encode(lock.beneficiary),
        "script": hex::encode(&lock.script),
        "price": lock.price.to_string(),
        "ksa": lock.ksa.to_string(),
        "expiry": lock.expiry,
        "state": lock.state.json(),
        "sats_for_all": sats_for_all(lock).to_string(),
    })
}

/// Seller operations and queries of the exchange contract (D-085).
async fn exchange(node: &Node, command: ExchangeCommand) -> Result<Value> {
    let (client, btc_net) = (&node.client, node.btc_net());
    match command {
        ExchangeCommand::Deposit {
            tx,
            exchange,
            amount,
        } => {
            ensure!(amount > 0, "the KSA to deposit must be greater than 0");
            let action = Action::Call {
                contract: parse_id(&exchange, "exchange contract ID")?,
                method: "deposit".into(),
                args: Vec::new(),
                funds: vec![Funds {
                    asset: KSA_ASSET_ID,
                    amount,
                }],
            };
            submit(node, tx, action).await
        }
        ExchangeCommand::Withdraw {
            tx,
            exchange,
            amount,
        } => {
            let action = Action::Call {
                contract: parse_id(&exchange, "exchange contract ID")?,
                method: "withdraw".into(),
                args: borsh::to_vec(&amount)?,
                funds: Vec::new(),
            };
            submit(node, tx, action).await
        }
        ExchangeCommand::Lock {
            tx,
            exchange,
            ksa,
            price,
            script,
            address,
            network,
            beneficiary,
            expiry,
            fund,
        } => {
            let script = match (script, address) {
                (Some(script), None) => parse_hex(&script, "payee script")?,
                (None, Some(address)) => {
                    let address: Address<NetworkUnchecked> =
                        address.parse().context("parsing the payee address")?;
                    address
                        .require_network(network.map_or(btc_net, Into::into))
                        .context("the payee address does not match --network")?
                        .script_pubkey()
                        .into_bytes()
                }
                _ => bail!("exactly one of --script and --address must be given"),
            };
            let beneficiary = parse_account(&beneficiary, btc_net)?;
            let funds = match fund {
                Some(amount) => {
                    ensure!(amount > 0, "the attached KSA must be greater than 0");
                    vec![Funds {
                        asset: KSA_ASSET_ID,
                        amount,
                    }]
                }
                None => Vec::new(),
            };
            let action = Action::Call {
                contract: parse_id(&exchange, "exchange contract ID")?,
                method: "lock".into(),
                args: borsh::to_vec(&(ksa, price, script, beneficiary, expiry))?,
                funds,
            };
            let mut result = submit(node, tx, action).await?;
            // with `--wait` and a successful execution, the receipt's output is the lock number
            let receipt = &result["status"]["receipt"];
            let lock = match (receipt["success"].as_bool(), receipt["output"].as_str()) {
                (Some(true), Some(output)) => Some(
                    borsh::from_slice::<u64>(&parse_hex(output, "lock number")?)
                        .context("the lock number encoding does not match the exchange contract")?,
                ),
                _ => None,
            };
            if let Some(lock) = lock {
                result["lock"] = json!(lock.to_string());
            }
            Ok(result)
        }
        ExchangeCommand::Release { tx, exchange, lock } => {
            let action = Action::Call {
                contract: parse_id(&exchange, "exchange contract ID")?,
                method: "release".into(),
                args: borsh::to_vec(&lock)?,
                funds: Vec::new(),
            };
            submit(node, tx, action).await
        }
        ExchangeCommand::Show { exchange, lock } => {
            let exchange = parse_id(&exchange, "exchange contract ID")?;
            Ok(lock_json(lock, &read_lock(client, exchange, lock).await?))
        }
        ExchangeCommand::Balance { exchange, seller } => {
            let seller = parse_account(&seller, btc_net)?;
            let receipt = client
                .call(CallRequestJson {
                    contract: parse_id(&exchange, "exchange contract ID")?,
                    method: "balance".into(),
                    args: seller.to_vec(),
                    funds: Vec::new(),
                    caller: None,
                    max_gas: None,
                })
                .await?;
            ensure!(
                receipt.success,
                "reading the available balance failed: {}",
                receipt.error.unwrap_or_default()
            );
            let balance: u128 = borsh::from_slice(&receipt.output)
                .context("the available balance encoding does not match the exchange contract")?;
            Ok(json!({ "seller": hex::encode(seller), "balance": balance.to_string() }))
        }
    }
}

/// Arguments of one top-up.
struct TopUp {
    exchange: [u8; 32],
    lock: u64,
    beneficiary: [u8; 32],
    sats: u64,
    allow_overpay: bool,
    allow_partial: bool,
    expiry_margin: u32,
}

/// Top-up (D-078, D-085): reads and checks the lock (beneficiary, Open, margin before expiry, over- or underpayment),
/// signs the claim op with a one-time key (nonce 0, carrying only the lock number), builds the payment + claim native
/// transaction, funded and broadcast by the bitcoind wallet; with `wait`, waits until the claim op is executed.
async fn top_up(node: &Node, req: TopUp, btc: &BtcArgs, wait: Option<Duration>) -> Result<Value> {
    let (client, params) = (&node.client, node.params());
    let lock = read_lock(client, req.exchange, req.lock).await?;
    ensure!(
        lock.beneficiary == req.beneficiary,
        "lock {}: its beneficiary {} is not the account given by --beneficiary: the payment would credit the KSA to someone else",
        req.lock,
        hex::encode(lock.beneficiary)
    );
    ensure!(
        lock.state == LockState::Open,
        "lock {} is not Open: the payment would buy no KSA",
        req.lock
    );
    // the expiry check uses bitcoind's tip height
    let rpc = btc_rpc(btc, node.btc_net())?;
    let current = rpc
        .call::<Value>("getblockcount", &[])
        .context("bitcoind getblockcount")?;
    let current = u32::try_from(
        current
            .as_u64()
            .context("getblockcount returned no height")?,
    )
    .context("BTC height exceeds u32")?;
    ensure!(
        u64::from(current) + u64::from(req.expiry_margin) < u64::from(lock.expiry),
        "lock {} expires at BTC height {} (current {current}, safety margin {} blocks): the payment might not be included before the seller releases",
        req.lock,
        lock.expiry,
        req.expiry_margin
    );
    let ksa = u128::from(req.sats)
        .saturating_mul(lock.price)
        .min(lock.ksa);
    let sats_for_all = sats_for_all(&lock);
    if !req.allow_overpay {
        ensure!(
            u128::from(req.sats) <= sats_for_all,
            "paying {} sats exceeds the {sats_for_all} sats needed to get all {} KSA base units: the excess buys no more (add --allow-overpay if this is really intended)",
            req.sats,
            lock.ksa
        );
    }
    if !req.allow_partial {
        ensure!(
            u128::from(req.sats) >= sats_for_all,
            "paying {} sats is underpaying: getting all {} KSA base units takes {sats_for_all} sats, and underpaying buys only {ksa} (the rest stays locked until another payment buys it or the seller releases it after the expiry; add --allow-partial if this is really intended)",
            req.sats,
            lock.ksa
        );
    }
    // one-time key: signs only this claim op, the nonce is always 0 (D-018: a path C op with a wrong nonce is void
    // while the BTC has already been paid)
    let one_time = loop {
        if let Ok(secret) = SecretKey::from_slice(&random32()?) {
            break Keypair::from_secret_key(&Secp256k1::new(), &secret);
        }
    };
    let unsigned = VmOp {
        version: ksana_primitives::VMOP_VERSION,
        chain_id: params.chain_id,
        account: ksana_crypto::account_of(&one_time),
        nonce: 0,
        fee: FeeMode::BtcNative,
        max_gas: params.native_max_gas,
        action: Action::Call {
            contract: req.exchange,
            method: "claim".into(),
            args: req.lock.to_le_bytes().to_vec(),
            funds: Vec::new(),
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    let aux = random32()?;
    let payment = TxOut {
        value: Amount::from_sat(req.sats),
        script_pubkey: bitcoin::ScriptBuf::from_bytes(lock.script.clone()),
    };
    // the signature binds the payment transaction's first input (D-086): the seller cannot copy the claim op into a
    // transaction of their own to claim fraudulently
    let (tx, op, fee) = send_native(
        node,
        btc,
        &|first_input| {
            let mut op = unsigned.clone();
            op.sig = ksana_crypto::sign_native_vmop(&op, first_input, &one_time, &aux)
                .map_err(|e| anyhow!("signing failed: {e}"))?;
            Ok(op)
        },
        &[payment],
    )?;
    let op_id = op.op_id().map_err(|e| anyhow!("{e}"))?;
    let mut result = json!({
        "op_id": hex::encode(op_id),
        "btc_txid": tx.compute_txid().to_string(),
        "vsize": tx.vsize(),
        "btc_fee": fee,
        "lock": req.lock.to_string(),
        "ksa": ksa.to_string(),
        "sats_for_all": sats_for_all.to_string(),
        "one_time_account": hex::encode(op.account),
        "beneficiary": hex::encode(lock.beneficiary),
        "seller": hex::encode(lock.seller),
        "expiry": lock.expiry,
        "current_btc_height": current,
    });
    if let Some(timeout) = wait {
        result["status"] = wait_included(client, &op_id, timeout).await?;
    }
    Ok(result)
}

/// Connects to bitcoind (to the given wallet when `--btc-wallet` is set) and checks that it is on `network`, the BTC
/// network of the node's chain: a transaction paid on another BTC network never reaches the node (A2-11, D-106).
fn btc_rpc(args: &BtcArgs, network: Network) -> Result<BtcRpc> {
    let url = match &args.btc_wallet {
        Some(wallet) => format!("{}/wallet/{wallet}", args.btc_rpc_url.trim_end_matches('/')),
        None => args.btc_rpc_url.clone(),
    };
    let rpc = BtcRpc::new_with_auth(
        &url,
        Auth::UserPass(args.btc_rpc_user.clone(), args.btc_rpc_pass.clone()),
    )
    .with_context(|| format!("connecting to {url}"))?;
    let info = rpc
        .call::<Value>("getblockchaininfo", &[])
        .context("bitcoind getblockchaininfo")?;
    let chain = info["chain"]
        .as_str()
        .context("getblockchaininfo returned no chain")?;
    check_btc_chain(&url, chain, network)?;
    Ok(rpc)
}

/// Refuses a bitcoind whose `getblockchaininfo` chain (`main`, `test`, `testnet4`, `signet` or `regtest`) is not
/// `network`.
fn check_btc_chain(url: &str, chain: &str, network: Network) -> Result<()> {
    ensure!(
        chain == network.to_core_arg(),
        "bitcoind at {url} is on BTC {chain}, but the node's chain runs on BTC {}: refusing to pay on another BTC \
         network; point --btc-rpc-url at a bitcoind on {}",
        network.to_core_arg(),
        network.to_core_arg()
    );
    Ok(())
}

/// Path C: a BTC transaction with a KSN1 native-transaction payload, followed after the data output by `payments` (for
/// example the amount paid to the lock's payee script when topping up), funded, signed and broadcast by the bitcoind
/// wallet; returns the broadcast transaction, the op it carries and the fee (sats).
///
/// The op's signature binds the carrying transaction's first input (D-086), hence two steps: first a placeholder op
/// (`sign` signs over an all-zero outpoint, with the same encoded length as the real op; BIP322's ECDSA signature has
/// variable length, so the placeholder payload leaves 2 extra bytes) lets the wallet fund the transaction and fix the
/// inputs, then the op is signed over the first input and the data output replaced, and finally the wallet signs the
/// inputs (so the input signatures cover the final data output).
fn send_native(
    node: &Node,
    args: &BtcArgs,
    sign: &dyn Fn(&OutPoint) -> Result<VmOp>,
    payments: &[TxOut],
) -> Result<(Transaction, VmOp, u64)> {
    let params = node.params();
    let rpc = btc_rpc(args, node.btc_net())?;
    let placeholder = sign(&OutPoint {
        txid: [0; 32],
        vout: 0,
    })?;
    let mut payload = Payload::NativeOp { op: placeholder }
        .encode(params)
        .map_err(|e| anyhow!("{e}"))?;
    payload.extend([0u8; 2]);
    let script = data_script(&params.magic, &payload).map_err(|e| anyhow!("{e}"))?;
    let call = |method: &str, params: &[Value]| -> Result<Value> {
        rpc.call::<Value>(method, params)
            .with_context(|| format!("bitcoind {method}"))
    };
    // a zero-input transaction is serialized in the legacy format: rust-bitcoin writes the segwit marker for a
    // zero-input transaction, which Core cannot parse.
    let mut unsigned = Vec::new();
    unsigned.extend(transaction::Version::TWO.0.to_le_bytes());
    unsigned.push(0);
    let mut outputs = vec![TxOut {
        value: Amount::ZERO,
        script_pubkey: script,
    }];
    outputs.extend(payments.iter().cloned());
    unsigned.extend(serialize(&outputs));
    unsigned.extend(absolute::LockTime::ZERO.to_consensus_u32().to_le_bytes());
    let funded = call(
        "fundrawtransaction",
        &[
            json!(hex::encode(unsigned)),
            json!({ "fee_rate": args.btc_fee_rate }),
            json!(false),
        ],
    )?;
    let funded_tx: Transaction = deserialize_hex(
        funded["hex"]
            .as_str()
            .context("fundrawtransaction returned no hex")?,
    )
    .context("parsing the funded transaction")?;
    let first_input = ksana_btc::native::first_input(&funded_tx)
        .context("the funded transaction has no inputs")?;
    let op = sign(&first_input)?;
    let bound = ksana_btc::native::with_native_op(&funded_tx, params, &op)
        .map_err(|e| anyhow!("replacing the data output: {e}"))?;
    let signed = call(
        "signrawtransactionwithwallet",
        &[json!(serialize_hex(&bound))],
    )?;
    ensure!(
        signed["complete"] == true,
        "the wallet could not sign all inputs: {signed}"
    );
    let hex_tx = signed["hex"]
        .as_str()
        .context("signrawtransactionwithwallet returned no hex")?;
    let tx: Transaction = deserialize_hex(hex_tx).context("parsing the signed transaction")?;
    ensure!(
        ksana_btc::native::native_op(params, &tx).as_ref() == Some(&op),
        "the built transaction is not recognized as a native transaction"
    );
    ensure!(
        ksana_btc::native::first_input(&tx) == Some(first_input),
        "the wallet's signing changed the transaction's first input"
    );
    for payment in payments {
        ensure!(
            tx.output.contains(payment),
            "the wallet-funded transaction lacks the payment output"
        );
    }
    let fee = funded["fee"]
        .as_f64()
        .map(|btc| Amount::from_btc(btc).map(Amount::to_sat))
        .transpose()
        .context("fundrawtransaction returned an invalid fee")?
        .unwrap_or(0);
    let txid = call("sendrawtransaction", &[json!(hex_tx)])?;
    let txid: Txid = txid
        .as_str()
        .context("sendrawtransaction returned no txid")?
        .parse()
        .context("parsing the txid")?;
    ensure!(
        txid == tx.compute_txid(),
        "the txid returned by bitcoind does not match the transaction"
    );
    Ok((tx, op, fee))
}

/// Polls the op's status until it is executed (success or failure). The node does not know a native transaction before
/// it is included, so the query reports it as an unknown op; `skipped` (included only by a merged block and not
/// executed when merging) keeps waiting too: an op that is still valid returns to the entry pool and is included again
/// later (D-063). On timeout, returns the last status seen if it is `skipped`, otherwise errors. Queries every 500 ms,
/// the number of queries derived from the timeout (the system time is not read).
async fn wait_included(client: &HttpClient, op_id: &[u8; 32], timeout: Duration) -> Result<Value> {
    const POLL: Duration = Duration::from_millis(500);
    let polls = timeout.as_millis() / POLL.as_millis();
    let mut skipped = None;
    for _ in 0..=polls {
        if let Ok(status) = client.get_op_status(hex::encode(op_id)).await {
            match status.status {
                OpState::Pending => skipped = None,
                OpState::Skipped => skipped = Some(status),
                _ => return Ok(json!(status)),
            }
        }
        tokio::time::sleep(POLL).await;
    }
    match skipped {
        Some(status) => Ok(json!(status)),
        None => bail!(
            "timed out waiting for the op to be included ({} seconds)",
            timeout.as_secs()
        ),
    }
}

fn key_info(path: &Path, network: Network) -> Result<Value> {
    let keypair = load_or_create(path)?;
    let account = ksana_crypto::account_of(&keypair);
    let address = ksana_crypto::account_address(&account, network).map_err(|e| anyhow!("{e}"))?;
    let (internal, _) = keypair.x_only_public_key();
    // the account is the Taproot address's output key (D-092); the internal key is only used when signing PSBTs
    // (tap_internal_key)
    Ok(json!({
        "account": hex::encode(account),
        "address": address.to_string(),
        "script": hex::encode(address.script_pubkey().as_bytes()),
        "internal_key": hex::encode(internal.serialize()),
    }))
}

fn load_secret(path: &Path) -> Result<SecretKey> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = parse_id(text.trim(), "secret key")?;
    SecretKey::from_slice(&bytes).context("invalid secret key")
}

fn load_keypair(path: &Path) -> Result<Keypair> {
    Ok(Keypair::from_secret_key(
        &Secp256k1::new(),
        &load_secret(path)?,
    ))
}

/// Reads the file when it exists; otherwise generates a random secret key and saves it with mode 0600.
fn load_or_create(path: &Path) -> Result<Keypair> {
    if path.exists() {
        return load_keypair(path);
    }
    let keypair = loop {
        // the probability of falling outside the curve order is negligible; redraw when it happens
        if let Ok(secret) = SecretKey::from_slice(&random32()?) {
            break Keypair::from_secret_key(&Secp256k1::new(), &secret);
        }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(hex::encode(keypair.secret_bytes()).as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(keypair)
}

fn random32() -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes)
}

fn parse_hex(s: &str, what: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).with_context(|| format!("{what} is not hex"))
}

/// The KS-20 tokens and the KS-721 collections `account` holds by the node's index (D-120), from every page of
/// `ksana_getAccountAssets`: each token's contract and balance, each collection's contract and the number of its NFTs
/// held. `None` from a node without the index (a version before it, or one that failed to build it), which answers
/// as a node without the method.
async fn contract_assets(
    client: &HttpClient,
    account: &[u8; 32],
) -> Result<Option<(Vec<Value>, Vec<Value>)>> {
    let (mut tokens, mut nfts) = (Vec::new(), Vec::new());
    let mut after = None;
    loop {
        let page = match client
            .get_account_assets(hex::encode(account), after.take(), Some(MAX_OPS_LIMIT))
            .await
        {
            Ok(page) => page,
            Err(ClientError::Call(e)) if e.code() == codes::METHOD_NOT_FOUND => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        for asset in page.assets {
            match asset {
                AccountAssetJson::Asset { .. } => {}
                AccountAssetJson::Token { contract, balance } => tokens.push(json!({
                    "contract": hex::encode(contract),
                    "balance": balance.to_string(),
                })),
                AccountAssetJson::Nft { contract, count } => nfts.push(json!({
                    "contract": hex::encode(contract),
                    "count": count.to_string(),
                })),
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(Some((tokens, nfts))),
        }
    }
}

fn parse_id(s: &str, what: &str) -> Result<[u8; 32]> {
    let bytes = parse_hex(s, what)?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| anyhow!("{what} should be 32 bytes, got {} bytes", b.len()))
}

/// Values of `--fund`: `<amount>` (KSA) or `<asset ID>:<amount>`; sorted by asset ID ascending, checking the count,
/// the amounts and duplicates (D-073).
fn parse_funds(values: &[String]) -> Result<Vec<Funds>> {
    let mut funds = Vec::with_capacity(values.len());
    for v in values {
        let (asset, amount) = match v.split_once(':') {
            Some((asset, amount)) => (parse_id(asset, "asset ID")?, amount),
            None => (KSA_ASSET_ID, v.as_str()),
        };
        let amount: u128 = amount
            .parse()
            .with_context(|| format!("invalid attached fund amount: {v}"))?;
        funds.push(Funds { asset, amount });
    }
    funds.sort_by_key(|f| f.asset);
    check_funds(&funds).map_err(|e| anyhow!("invalid attached funds: {e}"))?;
    Ok(funds)
}

/// An account: a Taproot address of this chain's network, 64 hex digits, or `@<secret key file>` (D-092).
fn parse_account(s: &str, network: Network) -> Result<[u8; 32]> {
    match s.strip_prefix('@') {
        Some(path) => Ok(ksana_crypto::account_of(&load_keypair(Path::new(path))?)),
        None => {
            ksana_crypto::parse_account(s, network).map_err(|e| anyhow!("invalid account: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fund_arguments_are_sorted_and_checked() {
        let asset = "11".repeat(32);
        let funds = parse_funds(&[format!("{asset}:5"), "7".into()]).unwrap();
        assert_eq!(
            funds,
            vec![
                Funds {
                    asset: KSA_ASSET_ID,
                    amount: 7
                },
                Funds {
                    asset: [0x11; 32],
                    amount: 5
                },
            ]
        );
        assert!(parse_funds(&[]).unwrap().is_empty());
        // duplicate asset, amount 0, invalid amount or asset ID
        for bad in [
            vec!["1".to_string(), "2".to_string()],
            vec!["0".to_string()],
            vec!["x".to_string()],
            vec!["abcd:1".to_string()],
            vec![format!("{asset}:-1")],
        ] {
            assert!(parse_funds(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_network_is_the_nodes() {
        use ksana_primitives::Network as Chain;
        let url = "http://node";
        for chain in Chain::ALL {
            let id = chain.params().chain_id;
            assert_eq!(node_network(url, id, None).unwrap(), chain);
            for expected in [ChainArg::Devnet, ChainArg::Signet, ChainArg::Mainnet] {
                let checked = node_network(url, id, Some(expected));
                if expected.network() == chain {
                    assert_eq!(checked.unwrap(), chain);
                } else {
                    let message = format!("{:#}", checked.unwrap_err());
                    assert!(message.contains(chain.name()), "{message}");
                    assert!(message.contains("--chain"), "{message}");
                }
            }
        }
        // a chain ID of no network: refused, with or without --chain
        for expected in [None, Some(ChainArg::Devnet)] {
            let message = format!("{:#}", node_network(url, 4242, expected).unwrap_err());
            assert!(message.contains("4242"), "{message}");
        }
    }

    #[test]
    fn bitcoind_on_another_btc_network_is_refused() {
        use ksana_primitives::Network as Chain;
        let pairs = [
            (Chain::Devnet, "regtest"),
            (Chain::Signet, "signet"),
            (Chain::Mainnet, "main"),
        ];
        for (chain, core) in pairs {
            let network = ksana_crypto::btc_network(chain);
            for (_, other) in pairs {
                let checked = check_btc_chain("http://btc", other, network);
                assert_eq!(
                    checked.is_ok(),
                    other == core,
                    "{} on {other}",
                    chain.name()
                );
            }
            for other in ["test", "testnet4", ""] {
                assert!(check_btc_chain("http://btc", other, network).is_err());
            }
        }
    }

    #[test]
    fn lock_decoding_matches_the_contract_layout() {
        // Lock { seller, beneficiary, script, price, ksa, expiry, state: Claimed { paid_sats, ksa_paid } }
        let mut bytes = vec![0x5e; 32];
        bytes.extend([0xb1; 32]);
        bytes.extend(2u32.to_le_bytes());
        bytes.extend([0x51, 0x20]);
        bytes.extend(3u128.to_le_bytes());
        bytes.extend(1_000u128.to_le_bytes());
        bytes.extend(500u32.to_le_bytes());
        bytes.push(1);
        bytes.extend(334u64.to_le_bytes());
        bytes.extend(1_000u128.to_le_bytes());
        let lock: ExchangeLock = borsh::from_slice(&bytes).unwrap();
        assert_eq!(lock.seller, [0x5e; 32]);
        assert_eq!(lock.beneficiary, [0xb1; 32]);
        assert_eq!(lock.script, vec![0x51, 0x20]);
        assert_eq!((lock.price, lock.ksa, lock.expiry), (3, 1_000, 500));
        assert!(matches!(
            lock.state,
            LockState::Claimed {
                paid_sats: 334,
                ksa_paid: 1_000
            }
        ));
        assert_eq!(sats_for_all(&lock), 334, "1 000 / 3 rounded up");
        let json = lock_json(7, &lock);
        assert_eq!(json["lock"], "7");
        assert_eq!(json["state"]["claimed"]["paid_sats"], "334");
        // trailing bytes: rejected
        bytes.push(0);
        assert!(borsh::from_slice::<ExchangeLock>(&bytes).is_err());
    }
}
