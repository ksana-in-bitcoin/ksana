//! `ksana-node`: the fast-lane node binary.
//!
//! ```sh
//! ksana-node --data-dir ./data --genesis devnet/genesis.json --rpc-addr 127.0.0.1:8545 \
//!     --btc-rpc-url http://127.0.0.1:18443 --btc-rpc-user ksana --btc-rpc-pass ksana \
//!     --producer <producer x-only public key (64 hex digits)> --btc-funding-key-file funding.key
//! ```
//!
//! `funding.key` holds the 64 hex digits of the settlement transaction funding secret key; the funding address (P2TR
//! key-path, BIP86) is printed to standard error at startup, and only after confirmed UTXOs are transferred to it can
//! the producer include path-B entries (D-037). `ksana-keytool` can generate a secret key and print the address.
//!
//! Multi-node (M4): `--p2p-listen /ip4/0.0.0.0/tcp/9000` enables the network layer, `--peer <multiaddr>` (repeatable,
//! `/p2p/<peer id>` may be omitted) gives static bootstrap nodes; the node identity key is kept in `p2p.key` in the
//! data directory (D-040).
//!
//! After a successful start, prints one line `ksana-node rpc=<address>` on standard output; logs go to standard error.
//! Exits normally on SIGINT / SIGTERM.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1};
use clap::Parser;
use ksana_btc::client::RpcClient;
use ksana_mempool::Policy;
use ksana_node::chain::Pruning;
use ksana_node::genesis::GenesisFile;
use ksana_node::node::{NetConfig, Node, NodeConfig};
use ksana_node::producer::{FeePolicy, Funding, Revenue};
use ksana_node::NodeError;
use ksana_p2p::Multiaddr;
use ksana_primitives::Network;

/// On Linux every allocation of the node, RocksDB's included, goes to jemalloc (D-103): glibc's allocator keeps the
/// memory that the node's many threads free in its arenas, so the resident memory kept rising after pruning while the
/// memory in use stayed flat.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Ksana fast-lane node (devnet).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Data directory.
    #[arg(long)]
    data_dir: PathBuf,
    /// Genesis config file (JSON).
    #[arg(long)]
    genesis: PathBuf,
    /// Fast-lane network: `devnet` (BTC regtest), `signet` (D-055) or `mainnet` (D-096); determines the protocol
    /// params (chain ID etc.).
    #[arg(long, default_value = "devnet")]
    chain: String,
    /// JSON-RPC listen address (HTTP and WebSocket).
    #[arg(long, default_value = "127.0.0.1:8545")]
    rpc_addr: SocketAddr,
    /// Origins allowed to call the RPC cross-origin (browser pages, D-055): `*` for any origin, or a comma-separated
    /// list; none allowed by default.
    #[arg(long, default_value = "")]
    rpc_cors: String,
    /// Produce blocks, and receive the token fees at this account (a Taproot address of this chain's network or 64
    /// hex digits, D-092); without it the node only serves queries.
    #[arg(long)]
    producer: Option<String>,
    /// Minimum token amount for packing path-B ops (local policy): when this node produces blocks, ops whose sponsored
    /// fee is below it are not included.
    #[arg(long, default_value_t = 1)]
    min_token_fee: u128,
    /// Minimum token amount for admitting (relaying) path-B ops (local policy): ops below it neither enter the entry
    /// pool nor get forwarded. Defaults to --min-token-fee; when given and lower, ops that reach it but not
    /// --min-token-fee enter the pool and are forwarded, to be packed by other producers; when higher than
    /// --min-token-fee, the latter applies.
    #[arg(long)]
    min_relay_token_fee: Option<u128>,
    /// Minimum fee rate for packing path-B ops (local policy): token amount per KiB of cost, where the cost is the
    /// larger of the encoded byte count and the bytes converted from max_gas (a block's gas limit corresponds to a
    /// block's data limit); 0 means no limit.
    #[arg(long, default_value_t = 0)]
    min_fee_per_kib: u128,
    /// Payee account of republish payloads (D-095; a Taproot address of this chain's network or 64 hex digits): when
    /// a republish lands some block's data on BTC for the first time, and that data has waited more than
    /// CARRIER_REPOST_DELAY BTC blocks, it takes that block's carrier reward. Defaults to --producer; when neither is
    /// given, republishing earns no carrier reward.
    #[arg(long)]
    carrier_payee: Option<String>,
    /// bitcoind RPC address (requires `-txindex=1`).
    #[arg(long, default_value = "http://127.0.0.1:18443")]
    btc_rpc_url: String,
    /// bitcoind RPC user name.
    #[arg(long, default_value = "ksana")]
    btc_rpc_user: String,
    /// bitcoind RPC password (or the environment variable KSANA_BTC_RPC_PASS, which keeps it off the command line).
    #[arg(
        long,
        env = "KSANA_BTC_RPC_PASS",
        hide_env_values = true,
        default_value = "ksana"
    )]
    btc_rpc_pass: String,
    /// Settlement transaction funding secret key file (64 hex digits); when producing blocks without it, blocks only
    /// contain forced-inclusion and unlock entries.
    #[arg(long)]
    btc_funding_key_file: Option<PathBuf>,
    /// A wallet of the same bitcoind that watches the funding address (D-109): the funding outputs are listed from it
    /// instead of a scan of the whole UTXO set (scantxoutset), which takes as long as the set is large. Required with
    /// --btc-funding-key-file on signet and mainnet, and only taken with it. A watch-only wallet loaded in bitcoind
    /// that holds the descriptor raw(<locking script>) of the funding address (`ksana-keytool <key file>` prints the
    /// address and its `funding_script`).
    #[arg(long)]
    btc_funding_wallet: Option<String>,
    /// Lowest fee rate of this node's BTC transactions (settlements, rescues, republishes; sat/vB, not below
    /// MIN_SETTLEMENT_FEERATE): paid when bitcoind has no fee estimate (D-102), and outside a race with a competing
    /// chain while bitcoind's mempool holds at most half a block (D-117).
    #[arg(long, default_value_t = 2)]
    settlement_feerate: u64,
    /// Highest fee rate of this node's BTC transactions (sat/vB, D-102): bitcoind's estimate is paid up to this rate,
    /// also when bumping a waiting settlement by CPFP and while racing a competing chain for BTC finality (D-091).
    #[arg(long, default_value_t = ksana_node::producer::DEFAULT_MAX_FEERATE)]
    max_settlement_feerate: u64,
    /// Confirmation target of this node's BTC transactions (BTC blocks, D-102): the fee rate is bitcoind's estimate
    /// (estimatesmartfee) for confirming within this many blocks, unless bitcoind's mempool has room (D-117); within
    /// the next block while a lighter competing chain is on BTC (D-091).
    #[arg(
        long,
        default_value_t = ksana_node::producer::DEFAULT_FEE_TARGET,
        value_parser = clap::value_parser!(u16).range(1..=1008)
    )]
    settlement_conf_target: u16,
    /// Revenue address (an address of this chain's BTC network, D-060): when the hot wallet (funding address) minus
    /// this funding still stays at or above --hot-wallet-cap-sat, the change of the settlement transaction is paid
    /// here; without it the change always returns to the hot wallet.
    #[arg(long)]
    revenue_address: Option<String>,
    /// Hot wallet cap (sats, D-060), only effective when --revenue-address is given.
    #[arg(long, default_value_t = 1_000_000)]
    hot_wallet_cap_sat: u64,
    /// Target size of the funding UTXO pool (D-075): when the hot wallet has fewer usable confirmed UTXOs than this,
    /// the largest one is split into several pieces (each at least 100 000 sats); 0 means no splitting.
    #[arg(long, default_value_t = 64)]
    funding_pool_target: usize,
    /// Disable settlement batching (D-077): every block that needs a body carries its own settlement transaction (BTC
    /// finality is faster, more settlement transactions and fees). Batching is on by default: while a settlement
    /// transaction is in flight, new blocks' bodies are backlogged first and carried all at once by the next block
    /// after it confirms.
    #[arg(long)]
    no_settlement_batching: bool,
    /// Disable pruning (D-097): keep the whole block DAG, the main chain's summaries and the BTC snapshot in memory
    /// (memory grows with the chain's history). Pruning is on by default: memory holds only what the chains this node
    /// can still switch to need, and each pruning writes a checkpoint the next start continues from.
    #[arg(long)]
    no_pruning: bool,
    /// Interval of syncing with bitcoind (milliseconds).
    #[arg(long, default_value_t = 500)]
    btc_poll_ms: u64,
    /// Minimum interval between two blocks (milliseconds, local policy; low-difficulty test networks use it to limit
    /// the block rate).
    #[arg(long, default_value_t = 0)]
    min_block_interval_ms: u64,
    /// P2P listen address (multiaddr, repeatable, e.g. /ip4/0.0.0.0/tcp/9000); without it the network layer is not
    /// enabled.
    #[arg(long = "p2p-listen")]
    p2p_listen: Vec<Multiaddr>,
    /// Static bootstrap nodes (multiaddr, repeatable; /p2p/<peer id> may be omitted).
    #[arg(long = "peer")]
    peers: Vec<Multiaddr>,
}

fn load_funding(path: &std::path::Path, fees: FeePolicy) -> Result<Funding, NodeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        NodeError::Config(format!(
            "reading funding secret key {}: {e}",
            path.display()
        ))
    })?;
    let mut secret = [0u8; 32];
    hex::decode_to_slice(text.trim(), &mut secret)
        .map_err(|e| NodeError::Config(format!("funding secret key must be 64 hex digits: {e}")))?;
    let keypair = Keypair::from_seckey_slice(&Secp256k1::new(), &secret)
        .map_err(|e| NodeError::Config(format!("funding secret key invalid: {e}")))?;
    Ok(Funding::new(keypair, fees.floor).with_fees(fees))
}

/// Locking script of the revenue address (D-060): must belong to this chain's BTC network (regtest for devnet), be a
/// standard output script, and not be the funding address itself.
fn revenue_script(
    address: &str,
    network: Network,
    funding: &bitcoin::ScriptBuf,
) -> Result<bitcoin::ScriptBuf, NodeError> {
    let btc_network = match network {
        Network::Devnet => bitcoin::Network::Regtest,
        Network::Signet => bitcoin::Network::Signet,
        Network::Mainnet => bitcoin::Network::Bitcoin,
    };
    let script = address
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .map_err(|e| NodeError::Config(format!("--revenue-address cannot be parsed: {e}")))?
        .require_network(btc_network)
        .map_err(|e| {
            NodeError::Config(format!(
                "--revenue-address does not belong to {btc_network}: {e}"
            ))
        })?
        .script_pubkey();
    if !ksana_btc::spend::is_standard_output_script(&script) {
        return Err(NodeError::Config(
            "--revenue-address must be a P2PKH, P2SH, P2WPKH, P2WSH or P2TR address".into(),
        ));
    }
    if script == *funding {
        return Err(NodeError::Config(
            "--revenue-address cannot be the funding address itself".into(),
        ));
    }
    Ok(script)
}

fn run(cli: Cli) -> Result<(), NodeError> {
    let network = Network::from_name(&cli.chain).ok_or_else(|| {
        NodeError::Config(format!(
            "unknown network {} (devnet, signet or mainnet)",
            cli.chain
        ))
    })?;
    let producer = cli
        .producer
        .map(|p| {
            ksana_crypto::parse_account(&p, ksana_crypto::btc_network(network)).map_err(|e| {
                NodeError::Config(format!(
                    "--producer must be a Taproot address of this chain's network or 64 hex digits: {e}"
                ))
            })
        })
        .transpose()?;
    let carrier_payee = cli
        .carrier_payee
        .map(|p| {
            ksana_crypto::parse_account(&p, ksana_crypto::btc_network(network)).map_err(|e| {
                NodeError::Config(format!(
                    "--carrier-payee must be a Taproot address of this chain's network or 64 hex digits: {e}"
                ))
            })
        })
        .transpose()?;
    eprintln!(
        "[ksana-node] upgrade table (D-076): {}",
        network.params().upgrade_schedule()
    );
    check_funding_listing(
        network,
        cli.btc_funding_key_file.is_some(),
        cli.btc_funding_wallet.is_some(),
    )?;
    let mut funding = cli
        .btc_funding_key_file
        .as_deref()
        .map(|p| {
            load_funding(
                p,
                FeePolicy {
                    floor: cli.settlement_feerate,
                    cap: cli.max_settlement_feerate,
                    target: cli.settlement_conf_target,
                },
            )
        })
        .transpose()?
        .map(|f| {
            f.with_pool_target(cli.funding_pool_target)
                .with_batching(!cli.no_settlement_batching)
        });
    if let Some(f) = &funding {
        eprintln!(
            "[ksana-node] locking script of the settlement funding address: {}",
            hex::encode(f.script.as_bytes())
        );
    }
    if let Some(address) = &cli.revenue_address {
        let Some(f) = funding.take() else {
            return Err(NodeError::Config(
                "--revenue-address requires --btc-funding-key-file as well".into(),
            ));
        };
        let revenue = Revenue {
            script: revenue_script(address, network, &f.script)?,
            hot_cap_sat: cli.hot_wallet_cap_sat,
        };
        eprintln!(
            "[ksana-node] hot wallet cap {} sats: when the wallet minus this funding still stays at or above the cap, the settlement change is paid to the revenue address {address} (D-060)",
            revenue.hot_cap_sat
        );
        funding = Some(f.with_revenue(revenue));
    }
    let mut btc = RpcClient::new(&cli.btc_rpc_url, &cli.btc_rpc_user, &cli.btc_rpc_pass)
        .map_err(|e| NodeError::Config(format!("connecting to bitcoind: {e}")))?;
    if let Some(wallet) = &cli.btc_funding_wallet {
        let Some(f) = &funding else {
            return Err(NodeError::Config(
                "--btc-funding-wallet requires --btc-funding-key-file as well".into(),
            ));
        };
        let address = bitcoin::Address::from_script(&f.script, ksana_crypto::btc_network(network))
            .map_err(|e| NodeError::Config(format!("the funding address: {e}")))?;
        btc = btc
            .with_wallet(wallet, f.script.clone())
            .map_err(|e| NodeError::Config(format!("--btc-funding-wallet: {e}")))?;
        // A wallet that does not watch the funding address would list nothing, and the node would never settle
        match btc.wallet_watches(&address.to_string()) {
            Ok(Some(true)) => eprintln!(
                "[ksana-node] the funding outputs are listed from bitcoind's wallet {wallet} (D-109)"
            ),
            Ok(_) => {
                return Err(NodeError::Config(format!(
                    "bitcoind's wallet {wallet} does not watch the funding address {address}: import the descriptor \
                     raw({}) into it",
                    hex::encode(f.script.as_bytes())
                )))
            }
            Err(e) => eprintln!(
                "[ksana-node] warning: cannot check that bitcoind's wallet {wallet} watches the funding address \
                 {address}: {e}"
            ),
        }
    }
    let net = if cli.p2p_listen.is_empty() {
        if !cli.peers.is_empty() {
            return Err(NodeError::Config(
                "--peer requires --p2p-listen as well".into(),
            ));
        }
        None
    } else {
        std::fs::create_dir_all(&cli.data_dir)?;
        let keypair = ksana_p2p::load_or_create_key(&cli.data_dir.join("p2p.key"))
            .map_err(|e| NodeError::Config(format!("node key: {e}")))?;
        Some(NetConfig {
            keypair,
            listen: cli.p2p_listen,
            peers: cli.peers,
            tuning: ksana_p2p::Tuning::default(),
        })
    };
    let pruning = if cli.no_pruning {
        Pruning::disabled()
    } else {
        Pruning::default()
    };
    let config = NodeConfig {
        data_dir: cli.data_dir,
        genesis: GenesisFile::load(&cli.genesis)?,
        params: network.params().clone(),
        rpc_addr: cli.rpc_addr,
        rpc_cors: ksana_rpc::Cors::parse(&cli.rpc_cors).map_err(NodeError::Config)?,
        producer,
        carrier_payee,
        funding,
        policy: Policy {
            min_token_fee: cli.min_token_fee,
            min_relay_token_fee: cli.min_relay_token_fee.unwrap_or(cli.min_token_fee),
            min_fee_per_kib: cli.min_fee_per_kib,
            ..Policy::default()
        },
        btc: Arc::new(btc),
        btc_poll: Duration::from_millis(cli.btc_poll_ms),
        min_block_interval: Duration::from_millis(cli.min_block_interval_ms),
        net,
        pow_algorithm: None,
        pruning,
    };
    let node = Node::start(config)?;
    let tip = node.chain().tip()?;
    eprintln!(
        "[ksana-node] started: main-chain height {}, tip {}",
        tip.header.height,
        hex::encode(tip.hash)
    );
    println!("ksana-node rpc={}", node.rpc_addr());
    node.wait_for_signal()?;
    eprintln!("[ksana-node] stopping");
    node.stop()
}

/// A node with a funding key on signet or mainnet lists its funding outputs from a watch-only wallet (D-109): a scan
/// of the UTXO set takes 30 to 50 seconds on signet and longer on mainnet, and the producer waits for it on every new
/// BTC tip.
fn check_funding_listing(
    network: Network,
    funding_key: bool,
    wallet: bool,
) -> Result<(), NodeError> {
    if funding_key && !wallet && network != Network::Devnet {
        return Err(NodeError::Config(format!(
            "--btc-funding-key-file on {} needs --btc-funding-wallet as well: a scan of the UTXO set takes 30 seconds \
             or more there (D-109; docs/SIGNET.md shows how to make the wallet)",
            network.name()
        )));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ksana-node] error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::SecretKey;

    fn funding_script() -> bitcoin::ScriptBuf {
        let secret = SecretKey::from_slice(&[7; 32]).unwrap();
        Funding::new(Keypair::from_secret_key(&Secp256k1::new(), &secret), 2).script
    }

    fn address_of(script: &bitcoin::ScriptBuf, network: bitcoin::Network) -> String {
        bitcoin::Address::from_script(script, network)
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_funding_key_on_signet_or_mainnet_needs_a_wallet() {
        for network in [Network::Signet, Network::Mainnet] {
            let refused = check_funding_listing(network, true, false).unwrap_err();
            assert!(
                refused.to_string().contains("--btc-funding-wallet"),
                "{refused}"
            );
            assert!(check_funding_listing(network, true, true).is_ok());
            assert!(check_funding_listing(network, false, false).is_ok());
        }
        // devnet's regtest UTXO set is small: scanning stays allowed
        assert!(check_funding_listing(Network::Devnet, true, false).is_ok());
    }

    #[test]
    fn revenue_address_must_belong_to_the_chain_and_differ_from_the_funding_address() {
        let funding = funding_script();
        let wpkh = bitcoin::ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([9; 20]));
        // devnet uses regtest addresses, signet uses signet addresses
        let regtest = address_of(&wpkh, bitcoin::Network::Regtest);
        assert_eq!(
            revenue_script(&regtest, Network::Devnet, &funding).unwrap(),
            wpkh
        );
        let signet = address_of(&wpkh, bitcoin::Network::Signet);
        assert_eq!(
            revenue_script(&signet, Network::Signet, &funding).unwrap(),
            wpkh
        );
        let mainnet = address_of(&wpkh, bitcoin::Network::Bitcoin);
        assert_eq!(
            revenue_script(&mainnet, Network::Mainnet, &funding).unwrap(),
            wpkh
        );
        assert!(revenue_script(&mainnet, Network::Devnet, &funding).is_err());
        assert!(revenue_script(&regtest, Network::Signet, &funding).is_err());
        assert!(revenue_script(&signet, Network::Mainnet, &funding).is_err());
        assert!(revenue_script("not an address", Network::Devnet, &funding).is_err());
        // Cannot be the funding address itself
        let own = address_of(&funding, bitcoin::Network::Regtest);
        assert!(revenue_script(&own, Network::Devnet, &funding).is_err());
    }
}
