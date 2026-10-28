//! Node end-to-end test tools: starting the `ksana-node` child process, a JSON-RPC client, keys and genesis config, and
//! a fake bitcoind (in-memory mock chain).

#![allow(dead_code)]

pub mod fake_bitcoind;
pub mod mock_node;
pub mod net;
pub mod side;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::secp256k1::{Keypair as BtcKeypair, Secp256k1};
use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;

use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_crypto::{account_of, keypair_from_secret, sign_vmop, Keypair};
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BlockHeader, BtcAnchor, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{AccountJson, BlockJson, KsanaApiClient, OpState, OpStatusJson};

pub struct User {
    pub kp: Keypair,
    pub account: [u8; 32],
}

pub fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/node-test/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

pub fn transfer_op(u: &User, nonce: u64, fee: u128, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: fee },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to,
            amount,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[1; 32]).unwrap();
    op
}

/// BTC for tests: a fake bitcoind (in-memory mock chain) and the producer's funding secret key file; the funding
/// address already has 20 confirmed UTXOs.
pub struct TestBtc {
    pub fake: fake_bitcoind::FakeBitcoind,
    pub funding_key: PathBuf,
}

impl TestBtc {
    pub fn new(dir: &Path) -> TestBtc {
        let mock = Arc::new(MockBtc::new());
        let secret = sha256(b"ksana/node-test/funding");
        let keypair = BtcKeypair::from_seckey_slice(&Secp256k1::new(), &secret).unwrap();
        let script = ksana_btc::settlement::producer_script(&keypair.x_only_public_key().0);
        mock.fund(&script, &[1_000_000; 20]);
        let funding_key = dir.join("funding.key");
        std::fs::write(&funding_key, hex::encode(secret)).unwrap();
        TestBtc {
            fake: fake_bitcoind::start(mock),
            funding_key,
        }
    }

    /// The current tip of the mock chain (for the genesis anchor).
    pub fn anchor(&self) -> BtcAnchor {
        let (height, hash) = self.fake.mock.tip().unwrap();
        BtcAnchor { height, hash }
    }

    /// The in-process client (sees the same mock chain as the child-process node).
    pub fn client(&self) -> Arc<dyn BtcClient> {
        self.fake.mock.clone()
    }
}

/// Genesis timestamp of the in-process test chains (mock_node, net, side): 30 days early. The weight of an empty block
/// is 12-120 by streak (D-084, D-090), and each block produced advances the ASERT schedule by 12-120 block intervals,
/// so a test chain producing blocks at millisecond intervals quickly runs far ahead of schedule and the difficulty
/// doubles about every 12 empty blocks; with the genesis timestamp moved early the chain always lags the schedule
/// (enough for 4,000-odd empty blocks of weight 120) and the difficulty stays at the PoW limit. `write_genesis` does
/// not move it early: it calibrates the genesis difficulty to the local PoW speed, and the difficulty adjustment has to
/// start from the genesis difficulty.
pub fn genesis_timestamp() -> u64 {
    ksana_node::unix_now() - 30 * 24 * 3600
}

/// Write a genesis config: anchored at `anchor`, with the given pre-allocations; `bits` of `None` uses the PoW limit.
pub fn write_genesis(
    dir: &Path,
    anchor: BtcAnchor,
    allocations: &[(&User, u128)],
    bits: Option<u32>,
) -> PathBuf {
    // The genesis timestamp is the current time: ASERT is anchored at the genesis block, and a timestamp too far behind
    // the current time would keep the difficulty at the lower limit forever
    let genesis = GenesisFile {
        timestamp: ksana_node::unix_now(),
        btc_anchor: anchor,
        bits,
        block_share_account: None,
        allocations: allocations
            .iter()
            .map(|(u, amount)| Allocation {
                account: u.account,
                amount: *amount,
            })
            .collect(),
    };
    let path = dir.join("genesis.json");
    std::fs::write(&path, serde_json::to_string_pretty(&genesis).unwrap()).unwrap();
    path
}

/// The PoW speed of this process (attempts per second; the network algorithm Argon2id does about one per millisecond,
/// D-094): timed for 2 seconds on the hardest target (where a solution is almost impossible to find).
pub fn pow_rate() -> f64 {
    use ksana_consensus::{Consensus, PowConsensus};
    let genesis = ksana_consensus::genesis_header(
        &DEVNET,
        1_790_000_000,
        BtcAnchor {
            height: 0,
            hash: [0; 32],
        },
        [0; 32],
    )
    .unwrap();
    let consensus = PowConsensus::new(&DEVNET, &genesis).unwrap();
    let mut header = genesis.clone();
    // A non-empty block (weight 1): the effective target of an empty block is the bits target divided by the weight
    // (D-084), and target 1 divided by 12 is 0, so solving would fail outright
    header.entries_root = [1; 32];
    header.bits = 0x0300_0001;
    let chunk = consensus.solve_chunk();
    let started = ksana_node::monotonic_now();
    let mut tried = 0u64;
    while started.elapsed() < Duration::from_secs(2) {
        assert!(!consensus.solve(&mut header, 1, tried, chunk).unwrap());
        tried += chunk;
    }
    (tried as f64 / started.elapsed().as_secs_f64()).max(1.0)
}

/// The sum of the weights (number of reward slots) of the blocks at heights 1-`tip` on a single producer's linear
/// chain: the weight of an empty block grows with the streak (12, 24, ..., 120, D-084, D-090), the streak is passed
/// along the selected parent (the previous block on a linear chain), and the streak of the genesis block is 0.
pub fn chain_slots(tip: u64, header_at: impl Fn(u64) -> BlockHeader) -> u128 {
    let mut streak = 0;
    let mut slots = 0u128;
    for height in 1..=tip {
        let header = header_at(height);
        slots += u128::from(DEVNET.block_weight(&header, streak));
        streak = DEVNET.block_streak(&header, streak);
    }
    slots
}

/// The genesis difficulty expected to give about one block per `secs` seconds at PoW speed `rate`.
pub fn bits_for_seconds(rate: f64, secs: f64) -> u32 {
    let work = (rate * secs).max(2.0);
    eprintln!("PoW speed about {rate:.0} attempts/s, the genesis target corresponds to about {work:.0} attempts per block");
    compact_for_log2_target(256.0 - work.log2())
}

/// The average number of attempts corresponding to `bits` (work ⌊2^256 / (target + 1)⌋).
pub fn expected_attempts(bits: u32) -> f64 {
    ksana_consensus::work_from_bits(bits, DEVNET.pow_limit_bits)
        .unwrap()
        .to_limbs()
        .iter()
        .rev()
        .fold(0.0, |acc, &limb| acc * 2f64.powi(64) + limb as f64)
}

/// The compact representation of a target of about 2^lg (Bitcoin nBits: 3-byte mantissa, sign bit not set).
pub fn compact_for_log2_target(lg: f64) -> u32 {
    let mut exponent = (lg / 8.0).ceil() as i32;
    let mut mantissa = 2f64.powf(lg - 8.0 * f64::from(exponent - 3));
    if mantissa >= f64::from(0x0080_0000) {
        exponent += 1;
        mantissa /= 256.0;
    }
    ((exponent as u32) << 24) | (mantissa as u32 & 0x007f_ffff)
}

/// A running node child process.
pub struct NodeProcess {
    pub child: Child,
    pub rpc_url: String,
    pub data_dir: PathBuf,
}

impl NodeProcess {
    /// Start the node (connected to the fake bitcoind of `btc`); when `producer` is `Some` it produces blocks and funds
    /// the settlement transactions with the test funding secret key.
    pub fn start(
        data_dir: &Path,
        genesis: &Path,
        producer: Option<&User>,
        min_fee: u128,
        btc: &TestBtc,
    ) -> Self {
        Self::try_start(data_dir, genesis, producer, min_fee, btc).unwrap()
    }

    /// Start the node; on failure, return its standard error output.
    pub fn try_start(
        data_dir: &Path,
        genesis: &Path,
        producer: Option<&User>,
        min_fee: u128,
        btc: &TestBtc,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ksana-node"));
        cmd.arg("--data-dir")
            .arg(data_dir)
            .arg("--genesis")
            .arg(genesis)
            .arg("--rpc-addr")
            .arg("127.0.0.1:0")
            .arg("--min-token-fee")
            .arg(min_fee.to_string())
            .arg("--btc-rpc-url")
            .arg(&btc.fake.url)
            .arg("--btc-poll-ms")
            .arg("100")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = producer {
            cmd.arg("--producer")
                .arg(hex::encode(p.account))
                .arg("--btc-funding-key-file")
                .arg(&btc.funding_key);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let Some(addr) = line.trim().strip_prefix("ksana-node rpc=") else {
            let output = child.wait_with_output().unwrap();
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        };
        let rpc_url = format!("http://{addr}");
        // The node's log is forwarded straight to the test's standard error
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("  | {line}");
            }
        });
        Ok(NodeProcess {
            child,
            rpc_url,
            data_dir: data_dir.to_path_buf(),
        })
    }

    pub fn client(&self) -> HttpClient {
        HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(&self.rpc_url)
            .unwrap()
    }

    /// SIGTERM, then wait for a normal exit.
    pub fn stop(mut self) {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
        let exit = self.child.wait().unwrap();
        assert!(exit.success(), "the node did not exit normally: {exit}");
    }

    /// SIGKILL (simulates a crash).
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub fn latest(rt: &tokio::runtime::Runtime, c: &HttpClient) -> BlockJson {
    rt.block_on(c.get_block("latest".into())).unwrap()
}

pub fn block_at(rt: &tokio::runtime::Runtime, c: &HttpClient, height: u64) -> BlockJson {
    rt.block_on(c.get_block(height.to_string())).unwrap()
}

pub fn account(rt: &tokio::runtime::Runtime, c: &HttpClient, u: &User) -> AccountJson {
    rt.block_on(c.get_account(hex::encode(u.account))).unwrap()
}

pub fn ksa(acc: &AccountJson) -> u128 {
    acc.balances
        .iter()
        .find(|b| b.asset == KSA_ASSET_ID)
        .map(|b| b.amount)
        .unwrap_or(0)
}

/// Wait until the main-chain height is at least `height`.
pub fn wait_height(
    rt: &tokio::runtime::Runtime,
    c: &HttpClient,
    height: u64,
    timeout: Duration,
) -> BlockJson {
    let started = ksana_node::monotonic_now();
    loop {
        let tip = latest(rt, c);
        if tip.header.height >= height {
            return tip;
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for height {height} (currently {})",
            tip.header.height
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait until the op is no longer pending.
pub fn wait_included(
    rt: &tokio::runtime::Runtime,
    c: &HttpClient,
    op_id: [u8; 32],
    timeout: Duration,
) -> OpStatusJson {
    let started = ksana_node::monotonic_now();
    loop {
        let status = rt.block_on(c.get_op_status(hex::encode(op_id))).unwrap();
        if status.status != OpState::Pending {
            return status;
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for the op to be included"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn submit(
    rt: &tokio::runtime::Runtime,
    c: &HttpClient,
    op: &VmOp,
) -> Result<[u8; 32], jsonrpsee::core::ClientError> {
    rt.block_on(c.submit_op(hex::encode(op.encode().unwrap())))
        .map(|r| r.op_id)
}

/// The JSON-RPC error code in a client error.
pub fn error_code(e: &jsonrpsee::core::ClientError) -> i32 {
    match e {
        jsonrpsee::core::ClientError::Call(obj) => obj.code(),
        other => panic!("not a JSON-RPC error: {other}"),
    }
}
