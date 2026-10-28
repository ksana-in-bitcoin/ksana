//! Relay probe: checks that a BTC network relays and mines Ksana's two kinds of data transactions (SPEC §7.1,
//! §7.2, D-032, D-081) before a public network is started on it, by sending one of each and following the chain.
//!
//! ```sh
//! cargo run --release -p ksana-btc --example relay_probe -- --key probe.key \
//!     --rpc-url http://127.0.0.1:38332 --rpc-user ksana --rpc-pass ksana
//! ```
//!
//! - The first run creates the key file (64 hex digits, the format of `ksana-keytool` and `--btc-funding-key-file`)
//!   and prints its Taproot address and how many sats to send to it; any wallet or faucet can fund it.
//! - With confirmed funds on that address, it splits them into one output per test and waits for the split to
//!   confirm, so that every test spends a confirmed output of its own and no test waits on another. A split that
//!   waits because its fee rate is too low is replaced by running the probe again with `--replace <its txid>` and a
//!   higher fee rate: the new split spends the same outputs again (every input signals replacement, BIP125).
//! - The fee rate (`--feerate`, default `auto`) is bitcoind's estimate for confirmation within 2 blocks, at least
//!   2 sat/vB, plus 1: a rate the miners do not take right now looks the same as a filter.
//! - Then it builds, checks with the local bitcoind (`testmempoolaccept`) and broadcasts:
//!   - commit / reveal pairs built exactly like settlement transactions, whose reveal carries a KSN1 envelope in its
//!     Taproot witness (`--envelope`, default 600 bytes, about a busy block's settlement, and 81 920 bytes,
//!     `MAX_BLOCK_DATA_BYTES`);
//!   - transactions with a KSN1 `OP_RETURN` data output, the carrier of path C (`--op-return`, default 220 bytes,
//!     about a transfer, 4 096 bytes and 65 536 bytes, `MAX_OP_BYTES`).
//!
//!   The payloads are filler that starts with `0xff`, which is no payload kind: nodes and indexers do not take them
//!   for blocks or ops.
//! - Last, it follows the chain block by block until every transaction is mined or `--wait-blocks` blocks have
//!   passed, and prints for each one whether the local bitcoind accepted it and the block it was mined in. A commit
//!   transaction mined without its reveal means the envelope is filtered on the way to the miners; a data
//!   transaction left in the local mempool means its `OP_RETURN` is.
//!
//! Change goes back to the probe's own address, so the next run spends it again. `--network regtest` runs it against
//! a regtest bitcoind, which mines only when asked (`generatetoaddress`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1, XOnlyPublicKey};
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::{
    absolute, transaction, Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
};
use ksana_btc::client::{BtcClient, RpcClient};
use ksana_btc::data::data_script;
use ksana_btc::settlement::{
    draft, finalize, producer_script, ProducerUtxo, P2A_AMOUNT_SAT, PRODUCER_SEQUENCE,
};
use ksana_btc::spend::bip86_keypair;
use ksana_primitives::{Params, DEVNET, MAINNET, SIGNET};

/// Sats left in each test's change on top of what the test spends, so that the change stays above the dust floor.
const CHANGE_MARGIN_SAT: u64 = 2_000;
/// The smallest change the split transaction keeps; anything smaller goes to its fee.
const MIN_CHANGE_SAT: u64 = 1_000;
/// The first payload byte: no payload kind uses it, so no node or indexer recognizes the probe's data.
const FILLER_TAG: u8 = 0xff;
/// `--feerate auto`: bitcoind's estimate for confirmation within this many blocks...
const AUTO_TARGET: u16 = 2;
/// ...at least this many sat/vB (the lowest rate signet's blocks took while a backlog waited on 2026-10-13, D-107)...
const AUTO_FLOOR: u64 = 2;
/// ...plus this margin, for the blocks the probe waits.
const AUTO_MARGIN: u64 = 1;

const USAGE: &str = "usage: relay_probe --key <file> [--rpc-url <url>] [--rpc-user <user>] [--rpc-pass <pass>]
       [--wallet <name>] [--network signet|regtest|testnet4|bitcoin] [--feerate <sat/vB>|auto] [--replace <txid>]
       [--wait-blocks <n>] [--poll-secs <s>] [--envelope <bytes,...>] [--op-return <bytes,...>]
The password can come from the environment variable KSANA_BTC_RPC_PASS instead. With --wallet, the probe's coins are
listed from that watch-only wallet of bitcoind, which holds raw(<the probe address's locking script>), instead of a
scan of the whole UTXO set (D-109). --feerate auto (the default) pays bitcoind's estimate for 2 blocks, at least
2 sat/vB, plus 1. --replace <txid> spends the inputs of that unconfirmed transaction of the probe's (a split that
waits) again, so that the new split replaces it.";

struct Args {
    key: PathBuf,
    rpc_url: String,
    rpc_user: String,
    rpc_pass: String,
    wallet: Option<String>,
    network: Network,
    /// `None`: `auto`.
    feerate: Option<u64>,
    replace: Option<Txid>,
    wait_blocks: u32,
    poll: Duration,
    envelopes: Vec<usize>,
    op_returns: Vec<usize>,
}

fn parse_sizes(value: &str) -> Result<Vec<usize>, String> {
    value
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .replace('_', "")
                .parse::<usize>()
                .map_err(|e| format!("size {s:?}: {e}"))
                .and_then(|n| {
                    if n == 0 {
                        Err("sizes must be at least 1 byte".to_string())
                    } else {
                        Ok(n)
                    }
                })
        })
        .collect()
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        key: PathBuf::new(),
        rpc_url: "http://127.0.0.1:38332".into(),
        rpc_user: "ksana".into(),
        rpc_pass: std::env::var("KSANA_BTC_RPC_PASS").unwrap_or_else(|_| "ksana".into()),
        wallet: None,
        network: Network::Signet,
        feerate: None,
        replace: None,
        wait_blocks: 6,
        poll: Duration::from_secs(20),
        envelopes: vec![600, 81_920],
        op_returns: vec![220, 4_096, 65_536],
    };
    let mut key = None;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        if flag == "-h" || flag == "--help" {
            return Err(USAGE.into());
        }
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--key" => key = Some(PathBuf::from(value)),
            "--rpc-url" => args.rpc_url = value,
            "--rpc-user" => args.rpc_user = value,
            "--rpc-pass" => args.rpc_pass = value,
            "--wallet" => args.wallet = Some(value),
            "--network" => {
                args.network = match value.as_str() {
                    "signet" => Network::Signet,
                    "regtest" => Network::Regtest,
                    "testnet4" => Network::Testnet4,
                    "bitcoin" | "mainnet" => Network::Bitcoin,
                    other => return Err(format!("unknown network {other:?}\n{USAGE}")),
                }
            }
            "--feerate" => {
                args.feerate = if value == "auto" {
                    None
                } else {
                    Some(value.parse().map_err(|e| format!("--feerate: {e}"))?)
                };
            }
            "--replace" => {
                args.replace = Some(value.parse().map_err(|e| format!("--replace: {e}"))?);
            }
            "--wait-blocks" => {
                args.wait_blocks = value.parse().map_err(|e| format!("--wait-blocks: {e}"))?;
            }
            "--poll-secs" => {
                let secs: u64 = value.parse().map_err(|e| format!("--poll-secs: {e}"))?;
                args.poll = Duration::from_secs(secs.max(1));
            }
            "--envelope" => args.envelopes = parse_sizes(&value)?,
            "--op-return" => args.op_returns = parse_sizes(&value)?,
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    args.key = key.ok_or_else(|| format!("--key is required\n{USAGE}"))?;
    if args.envelopes.is_empty() && args.op_returns.is_empty() {
        return Err("nothing to probe: both --envelope and --op-return are empty".into());
    }
    Ok(args)
}

/// The Ksana parameters whose carriers are probed: the magic, the payload bound and the lowest fee rate are the same
/// on every network, but take the ones of the network that runs on this BTC network.
fn params_for(network: Network) -> &'static Params {
    match network {
        Network::Bitcoin => &MAINNET,
        Network::Regtest => &DEVNET,
        _ => &SIGNET,
    }
}

/// Reads the secret key (64 hex digits) or creates it with permissions 0600, like `ksana-keytool`.
fn load_or_create_key(path: &Path) -> Result<(Keypair, bool), String> {
    let secp = Secp256k1::new();
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut secret = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut secret)
            .map_err(|e| format!("the secret key must be 64 hex digits: {e}"))?;
        let keypair = Keypair::from_seckey_slice(&secp, &secret)
            .map_err(|e| format!("secret key invalid: {e}"))?;
        return Ok((keypair, false));
    }
    let mut urandom =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("opening /dev/urandom: {e}"))?;
    loop {
        let mut secret = [0u8; 32];
        urandom
            .read_exact(&mut secret)
            .map_err(|e| format!("reading /dev/urandom: {e}"))?;
        let Ok(keypair) = Keypair::from_seckey_slice(&secp, &secret) else {
            continue;
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .map_err(|e| format!("creating {}: {e}", path.display()))?;
        file.write_all(hex::encode(secret).as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        return Ok((keypair, true));
    }
}

/// `len` filler bytes: [`FILLER_TAG`] followed by a SHA-256 stream, so the data does not compress.
fn filler(len: usize, seed: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    out.push(FILLER_TAG);
    let mut block = ksana_primitives::hash::sha256(&[seed]);
    while out.len() < len {
        let take = (len - out.len()).min(block.len());
        out.extend_from_slice(&block[..take]);
        block = ksana_primitives::hash::sha256(&block);
    }
    out.truncate(len);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A settlement-shaped commit / reveal pair with the payload in the reveal's envelope.
    Envelope,
    /// A transaction with a KSN1 `OP_RETURN` data output.
    OpReturn,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Envelope => "envelope",
            Kind::OpReturn => "op_return",
        }
    }
}

/// One test: a carrier and a payload size.
struct Probe {
    kind: Kind,
    size: usize,
    payload: Vec<u8>,
    /// What the test spends in all (fees, the P2A output), from a dry run.
    spends: u64,
}

/// The transactions of one test once built and broadcast.
struct Sent {
    /// The transactions in broadcast order; the last one carries the data.
    txids: Vec<Txid>,
    vsize: u64,
    fee: u64,
    /// The height of the tip when they were broadcast.
    from_height: u32,
    /// Where each was mined: block height and hash.
    mined: BTreeMap<Txid, (u32, [u8; 32])>,
    /// `testmempoolaccept` refused the transactions: the reason.
    refused: Option<String>,
}

/// A transaction that spends `utxos` (all of `script`, signed with the BIP86-tweaked `key`, `SIGHASH_DEFAULT`) into
/// `outputs`; the first output whose value is 0 and whose script is `script` receives the change.
fn spend_all(
    utxos: &[ProducerUtxo],
    mut outputs: Vec<TxOut>,
    feerate: u64,
    key: &Keypair,
) -> Result<(Transaction, u64), String> {
    let input: Vec<TxIn> = utxos
        .iter()
        .map(|u| TxIn {
            previous_output: u.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence(PRODUCER_SEQUENCE),
            witness: Witness::from_slice(&[[0u8; 64]]),
        })
        .collect();
    let script = producer_script(&key.x_only_public_key().0);
    let change_index = outputs
        .iter()
        .position(|o| o.value == Amount::ZERO && o.script_pubkey == script)
        .ok_or("no change output")?;
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input,
        output: std::mem::take(&mut outputs),
    };
    let vsize = tx.weight().to_vbytes_ceil();
    let fee = vsize * feerate;
    let total_in: u64 = utxos.iter().map(|u| u.txout.value.to_sat()).sum();
    let total_out: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    let change = total_in
        .checked_sub(total_out + fee)
        .filter(|change| *change >= MIN_CHANGE_SAT)
        .ok_or_else(|| {
            format!(
                "{total_in} sats cannot pay {total_out} sats of outputs, a {fee}-sat fee and {MIN_CHANGE_SAT} sats of change"
            )
        })?;
    tx.output[change_index].value = Amount::from_sat(change);

    let secp = Secp256k1::new();
    let tweaked = bip86_keypair(&secp, key)?;
    let prevouts: Vec<TxOut> = utxos.iter().map(|u| u.txout.clone()).collect();
    let mut signatures = Vec::with_capacity(tx.input.len());
    let mut cache = SighashCache::new(&tx);
    for index in 0..prevouts.len() {
        let sighash = cache
            .taproot_key_spend_signature_hash(
                index,
                &Prevouts::All(&prevouts),
                TapSighashType::Default,
            )
            .map_err(|e| format!("sighash: {e}"))?;
        signatures.push(secp.sign_schnorr_no_aux_rand(&Message::from(sighash), &tweaked));
    }
    for (txin, signature) in tx.input.iter_mut().zip(signatures) {
        txin.witness = Witness::from_slice(&[signature.serialize()]);
    }
    Ok((tx, fee))
}

/// Builds the transactions of a test from its own UTXO: `(transactions in broadcast order, vsize, fee)`.
fn build(
    params: &Params,
    probe: &Probe,
    utxo: &ProducerUtxo,
    feerate: u64,
    key: &Keypair,
) -> Result<(Vec<Transaction>, u64, u64), String> {
    let internal: XOnlyPublicKey = key.x_only_public_key().0;
    let script = producer_script(&internal);
    match probe.kind {
        Kind::Envelope => {
            let draft = draft(
                params,
                std::slice::from_ref(utxo),
                script,
                &internal,
                &probe.payload,
                feerate,
            )
            .map_err(|e| e.to_string())?;
            let fee = draft.fee;
            let pair = finalize(params, draft, &probe.payload, key).map_err(|e| e.to_string())?;
            let vsize =
                pair.commit.weight().to_vbytes_ceil() + pair.reveal.weight().to_vbytes_ceil();
            Ok((vec![pair.commit, pair.reveal], vsize, fee))
        }
        Kind::OpReturn => {
            let data = data_script(&params.magic, &probe.payload).map_err(|e| e.to_string())?;
            let outputs = vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: data,
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: script,
                },
            ];
            let (tx, fee) = spend_all(std::slice::from_ref(utxo), outputs, feerate, key)?;
            let vsize = tx.weight().to_vbytes_ceil();
            Ok((vec![tx], vsize, fee))
        }
    }
}

/// What a test spends in all, from a dry run on a large dummy UTXO.
fn dry_run(params: &Params, probe: &Probe, feerate: u64, key: &Keypair) -> Result<u64, String> {
    let dummy = ProducerUtxo {
        outpoint: OutPoint {
            txid: Txid::all_zeros(),
            vout: 0,
        },
        txout: TxOut {
            value: Amount::from_sat(100_000_000),
            script_pubkey: producer_script(&key.x_only_public_key().0),
        },
    };
    let (_, _, fee) = build(params, probe, &dummy, feerate, key)?;
    Ok(match probe.kind {
        Kind::Envelope => fee + P2A_AMOUNT_SAT,
        Kind::OpReturn => fee,
    })
}

/// The tests at `feerate`, what each needs from its own UTXO, and what all of them need with the split.
fn plan(
    params: &Params,
    probes: &[(Kind, usize, usize)],
    feerate: u64,
    key: &Keypair,
) -> Result<(Vec<Probe>, Vec<u64>, u64), String> {
    let mut tests = Vec::with_capacity(probes.len());
    for (kind, size, seed) in probes {
        let seed = u8::try_from(*seed).map_err(|_| "too many sizes")?;
        let mut probe = Probe {
            kind: *kind,
            size: *size,
            payload: filler(*size, seed),
            spends: 0,
        };
        probe.spends = dry_run(params, &probe, feerate, key)?;
        tests.push(probe);
    }
    let needs: Vec<u64> = tests.iter().map(|t| t.spends + CHANGE_MARGIN_SAT).collect();
    let split_estimate = 200 + 43 * u64::try_from(tests.len()).unwrap_or(u64::MAX);
    let total = needs.iter().sum::<u64>() + split_estimate * feerate + MIN_CHANGE_SAT;
    Ok((tests, needs, total))
}

/// `sats` as BTC with 8 decimals.
fn btc(sats: u64) -> String {
    format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}

/// Waits until output 0 of `txid` is confirmed; an error when the transaction leaves the local mempool unconfirmed.
fn wait_confirmed(client: &RpcClient, txid: Txid, poll: Duration) -> Result<(), String> {
    let first = OutPoint { txid, vout: 0 };
    loop {
        std::thread::sleep(poll);
        let in_mempool = client
            .in_mempool(&txid.to_byte_array())
            .map_err(|e| format!("getmempoolentry: {e}"))?;
        match client.utxo(&first, false) {
            Ok(Some(info)) if info.confirmations > 0 => return Ok(()),
            Ok(_) if !in_mempool => {
                return Err(format!("{txid} left the local mempool unconfirmed"));
            }
            Ok(_) => {}
            Err(e) => eprintln!("gettxout: {e}"),
        }
    }
}

fn run() -> Result<bool, String> {
    let args = parse_args()?;
    let params = params_for(args.network);
    let (key, created) = load_or_create_key(&args.key)?;
    let internal = key.x_only_public_key().0;
    let script = producer_script(&internal);
    let address = Address::from_script(&script, args.network)
        .map_err(|e| format!("cannot derive the address: {e}"))?;

    let mut probes = Vec::new();
    for (i, size) in args.envelopes.iter().enumerate() {
        if *size > params.max_block_data_bytes {
            return Err(format!(
                "an envelope payload of {size} bytes exceeds MAX_BLOCK_DATA_BYTES {}",
                params.max_block_data_bytes
            ));
        }
        probes.push((Kind::Envelope, *size, i));
    }
    for (i, size) in args.op_returns.iter().enumerate() {
        probes.push((Kind::OpReturn, *size, i + 100));
    }

    println!("probe address ({}): {address}", args.network);
    if created {
        let feerate = args.feerate.unwrap_or(AUTO_FLOOR + AUTO_MARGIN);
        let (_, _, total) = plan(params, &probes, feerate, &key)?;
        println!(
            "created {}: send at least {total} sats ({} BTC, at {feerate} sat/vB) to the address above, wait for a \
             confirmation, and run again",
            args.key.display(),
            btc(total)
        );
        return Ok(false);
    }

    let mut client = RpcClient::new(&args.rpc_url, &args.rpc_user, &args.rpc_pass)
        .map_err(|e| format!("bitcoind: {e}"))?;
    if let Some(wallet) = &args.wallet {
        client = client
            .with_wallet(wallet, script.clone())
            .map_err(|e| format!("--wallet: {e}"))?;
        if client.wallet_watches(&address.to_string()) != Ok(Some(true)) {
            return Err(format!(
                "bitcoind's wallet {wallet} does not watch {address}: import raw({}) into it",
                hex::encode(script.as_bytes())
            ));
        }
    }
    let (tip, _) = client.tip().map_err(|e| format!("bitcoind: {e}"))?;
    let feerate = match args.feerate {
        Some(rate) => rate,
        None => {
            let estimate = client
                .estimate_feerate(AUTO_TARGET)
                .map_err(|e| format!("estimatesmartfee: {e}"))?;
            let rate = estimate.unwrap_or(0).max(AUTO_FLOOR) + AUTO_MARGIN;
            println!(
                "fee rate {rate} sat/vB: bitcoind's estimate for {AUTO_TARGET} blocks is {}, at least {AUTO_FLOOR}, \
                 plus {AUTO_MARGIN}",
                estimate.map_or("none".to_string(), |r| format!("{r} sat/vB"))
            );
            rate
        }
    };
    let (tests, needs, total) = plan(params, &probes, feerate, &key)?;
    println!(
        "bitcoind at height {tip}; fee rate {feerate} sat/vB; {} tests",
        tests.len()
    );

    // The inputs of the transaction to replace: bitcoind's wallet no longer lists them while it waits in the
    // mempool, but they are still in the UTXO set. The first split spends them again
    let mut replaced = Vec::new();
    if let Some(txid) = args.replace {
        let info = client
            .transaction(&txid.to_byte_array())
            .map_err(|e| format!("getrawtransaction: {e}"))?
            .ok_or_else(|| format!("--replace {txid}: bitcoind knows no such transaction"))?;
        if info.block.is_some() {
            return Err(format!(
                "--replace {txid}: it is already in a block; run without --replace"
            ));
        }
        for input in &info.tx.input {
            let coin = client
                .utxo(&input.previous_output, false)
                .map_err(|e| format!("gettxout: {e}"))?;
            if let Some(coin) = coin.filter(|c| c.txout.script_pubkey == script && !c.coinbase) {
                replaced.push(ProducerUtxo {
                    outpoint: input.previous_output,
                    txout: coin.txout,
                });
            }
        }
        if replaced.is_empty() {
            return Err(format!(
                "--replace {txid}: none of its inputs is a confirmed unspent output of {address}"
            ));
        }
        println!(
            "replacing {txid}: its {} input(s) of {} sats are spent again",
            replaced.len(),
            replaced.iter().map(|u| u.txout.value.to_sat()).sum::<u64>()
        );
    }

    // Match every test with a confirmed UTXO of its own; split the funds first when they do not match.
    let assigned: Vec<ProducerUtxo>;
    loop {
        let mut utxos: Vec<ProducerUtxo> = client
            .scan_utxos(&script)
            .map_err(|e| format!("scantxoutset: {e}"))?
            .into_iter()
            .filter(|u| !u.coinbase)
            .map(|u| ProducerUtxo {
                outpoint: u.outpoint,
                txout: u.txout,
            })
            .collect();
        for utxo in &replaced {
            if !utxos.iter().any(|u| u.outpoint == utxo.outpoint) {
                utxos.push(utxo.clone());
            }
        }
        let balance: u64 = utxos.iter().map(|u| u.txout.value.to_sat()).sum();
        // A replacement is always a split, which spends every input of the transaction it replaces again
        if replaced.is_empty() {
            if let Some(found) = assign(&mut utxos.clone(), &needs) {
                assigned = found;
                break;
            }
        }
        if balance < total {
            // A descriptor wallet leaves out the outputs that a transaction of its own in the mempool spends
            let hint = if args.wallet.is_some() && replaced.is_empty() {
                " (the wallet does not list outputs that a transaction in the mempool spends: if a split of the \
                 probe's waits there, run again with --replace <its txid>)"
            } else {
                ""
            };
            return Err(format!(
                "the probe address holds {balance} confirmed sats; it needs {total} at {feerate} sat/vB: send the \
                 rest to {address}, or lower --feerate{hint}"
            ));
        }
        utxos.sort_by_key(|u| std::cmp::Reverse(u.txout.value));
        let mut outputs: Vec<TxOut> = needs
            .iter()
            .map(|need| TxOut {
                value: Amount::from_sat(*need),
                script_pubkey: script.clone(),
            })
            .collect();
        outputs.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: script.clone(),
        });
        let (split, fee) = spend_all(&utxos, outputs, feerate, &key)?;
        let txid = split.compute_txid();
        // The same tests at the split's own rate give the same transaction again (the signatures are deterministic),
        // which bitcoind takes as the one already waiting: nothing would be replaced
        if args.replace == Some(txid) {
            return Err(format!(
                "--replace {txid}: at {feerate} sat/vB this is the same split again, which replaces nothing: raise \
                 --feerate"
            ));
        }
        match client.broadcast(&split) {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                let hint = match args.replace {
                    Some(old) if !replaced.is_empty() => format!(
                        " (it replaces {old}, so it must pay a higher fee rate and more fee in all: raise --feerate)"
                    ),
                    _ => String::new(),
                };
                return Err(format!("the split transaction was refused: {reason}{hint}"));
            }
            Err(e) => return Err(format!("sendrawtransaction: {e}")),
        }
        println!(
            "split {balance} sats into {} outputs: {txid} (fee {fee} sats); waiting for it to confirm",
            needs.len()
        );
        replaced.clear();
        wait_confirmed(&client, txid, args.poll)?;
    }

    // Build, check and broadcast every test.
    let (from_height, _) = client.tip().map_err(|e| format!("bitcoind: {e}"))?;
    let mut sent = Vec::with_capacity(tests.len());
    for (probe, utxo) in tests.iter().zip(&assigned) {
        let (txs, vsize, fee) = build(params, probe, utxo, feerate, &key)?;
        let refs: Vec<&Transaction> = txs.iter().collect();
        let txids: Vec<Txid> = txs.iter().map(Transaction::compute_txid).collect();
        let refused = match client.test_accept(&refs) {
            Ok(Ok(())) => match client.broadcast_all(&refs) {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason),
                Err(e) => return Err(format!("sendrawtransaction: {e}")),
            },
            Ok(Err(reason)) => Some(reason),
            Err(e) => return Err(format!("testmempoolaccept: {e}")),
        };
        println!(
            "{} {} B: {} ({vsize} vB, fee {fee} sats){}",
            probe.kind.name(),
            probe.size,
            txids
                .iter()
                .map(Txid::to_string)
                .collect::<Vec<_>>()
                .join(" → "),
            refused
                .as_ref()
                .map(|r| format!(": refused by the local bitcoind: {r}"))
                .unwrap_or_default()
        );
        sent.push(Sent {
            txids,
            vsize,
            fee,
            from_height,
            mined: BTreeMap::new(),
            refused,
        });
    }

    // Follow the chain block by block until everything is mined or the wait is over.
    let mut next = from_height.saturating_add(1);
    let last = from_height.saturating_add(args.wait_blocks);
    let mut warned = BTreeSet::new();
    loop {
        let (tip, _) = client.tip().map_err(|e| format!("bitcoind: {e}"))?;
        while next <= tip.min(last) {
            let Some(hash) = client
                .block_hash(next)
                .map_err(|e| format!("getblockhash: {e}"))?
            else {
                break;
            };
            let Some(block) = client.block(&hash).map_err(|e| format!("getblock: {e}"))? else {
                break;
            };
            for tx in &block.txdata {
                let txid = tx.compute_txid();
                for s in sent.iter_mut() {
                    if s.txids.contains(&txid) {
                        s.mined.insert(txid, (next, hash));
                    }
                }
            }
            println!(
                "block {next}: {} transactions, {} of ours so far",
                block.txdata.len(),
                sent.iter().map(|s| s.mined.len()).sum::<usize>()
            );
            next = next.saturating_add(1);
        }
        let pending = sent
            .iter()
            .any(|s| s.refused.is_none() && s.mined.len() < s.txids.len());
        if !pending || next > last {
            break;
        }
        for s in &sent {
            for txid in &s.txids {
                if !s.mined.contains_key(txid)
                    && s.refused.is_none()
                    && !client.in_mempool(&txid.to_byte_array()).unwrap_or(true)
                    && warned.insert(*txid)
                {
                    eprintln!("{txid} is neither mined nor in the local mempool any more");
                }
            }
        }
        std::thread::sleep(args.poll);
    }

    // A block that a reorg took away no longer counts.
    for s in sent.iter_mut() {
        let mut gone = Vec::new();
        for (txid, (_, hash)) in &s.mined {
            let best = client
                .header(hash)
                .map_err(|e| format!("getblockheader: {e}"))?
                .is_some_and(|h| h.in_best_chain);
            if !best {
                gone.push(*txid);
            }
        }
        for txid in gone {
            s.mined.remove(&txid);
        }
    }
    // A transaction the walk above missed, mined again after a reorg in a block at a height it had already read, is
    // found through txindex
    for s in sent.iter_mut().filter(|s| s.refused.is_none()) {
        for txid in &s.txids {
            if s.mined.contains_key(txid) {
                continue;
            }
            let block = client
                .transaction(&txid.to_byte_array())
                .map_err(|e| format!("getrawtransaction: {e}"))?
                .and_then(|info| info.block);
            if let Some(hash) = block {
                if let Some(header) = client
                    .header(&hash)
                    .map_err(|e| format!("getblockheader: {e}"))?
                    .filter(|h| h.in_best_chain)
                {
                    s.mined.insert(*txid, (header.height, hash));
                }
            }
        }
    }

    println!();
    println!(
        "{:<10} {:>8}  {:>7} {:>7}  {:<64}  result",
        "carrier", "bytes", "vB", "fee", "txid of the data transaction"
    );
    let mut all = true;
    for (probe, s) in tests.iter().zip(&sent) {
        let data = s.txids.last().copied().unwrap_or(Txid::all_zeros());
        let result = if let Some(reason) = &s.refused {
            all = false;
            format!("refused by the local bitcoind: {reason}")
        } else if let Some((height, _)) = s.mined.get(&data) {
            format!(
                "mined at {height}, {} block(s) after broadcast",
                height.saturating_sub(s.from_height)
            )
        } else {
            all = false;
            let first = s.txids.first().and_then(|txid| s.mined.get(txid));
            match (probe.kind, first) {
                (Kind::Envelope, Some((height, _))) => format!(
                    "NOT mined within {} blocks; its commit was mined at {height}: the envelope is filtered",
                    args.wait_blocks
                ),
                _ => format!("NOT mined within {} blocks", args.wait_blocks),
            }
        };
        println!(
            "{:<10} {:>8}  {:>7} {:>7}  {:<64}  {result}",
            probe.kind.name(),
            probe.size,
            s.vsize,
            s.fee,
            data.to_string()
        );
    }
    println!();
    if all {
        println!("every probe transaction was mined");
    } else {
        println!("some probe transactions were not mined: see above");
        // A fee rate the miners do not take right now looks the same as a filter: compare it before concluding
        let waiting = sent
            .iter()
            .flat_map(|s| s.txids.iter().filter(|txid| !s.mined.contains_key(*txid)))
            .filter(|txid| client.in_mempool(&txid.to_byte_array()).unwrap_or(false))
            .count();
        let estimate = |target| match client.estimate_feerate(target) {
            Ok(Some(rate)) => format!("{rate} sat/vB"),
            Ok(None) => "none".to_string(),
            Err(e) => format!("unknown ({e})"),
        };
        println!(
            "{waiting} of them still wait in the local mempool; the probe paid {} sat/vB, bitcoind's estimate is {} \
             for the next block and {} within 6 blocks. While the estimate is above what the probe paid, run it again \
             with a higher --feerate before concluding that a carrier is filtered.",
            feerate,
            estimate(1),
            estimate(6)
        );
    }
    Ok(all)
}

/// Gives each need a distinct UTXO of at least that value (largest needs first, smallest fitting UTXO); `None`
/// when they do not all fit.
fn assign(utxos: &mut Vec<ProducerUtxo>, needs: &[u64]) -> Option<Vec<ProducerUtxo>> {
    let mut order: Vec<usize> = (0..needs.len()).collect();
    order.sort_by_key(|i| std::cmp::Reverse(needs[*i]));
    utxos.sort_by_key(|u| u.txout.value);
    let mut chosen: Vec<Option<ProducerUtxo>> = vec![None; needs.len()];
    for i in order {
        let pos = utxos
            .iter()
            .position(|u| u.txout.value.to_sat() >= needs[i])?;
        chosen[i] = Some(utxos.remove(pos));
    }
    chosen.into_iter().collect()
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("relay_probe: {e}");
            ExitCode::from(2)
        }
    }
}
