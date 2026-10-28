//! The CLI's queries of the node's index of the standards' events (D-120): `account` adds the tokens and NFTs of every
//! page of `ksana_getAccountAssets` and leaves them out with a node without the index; `contract`, `token`, `holders`,
//! `nfts` and `approvals` print the node's answers, passing on the cursor and the size of a page.

use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use jsonrpsee::core::RpcResult;
use jsonrpsee::server::ServerHandle;
use ksana_primitives::{DEVNET, KSA_ASSET_ID};
use ksana_rpc::{
    codes, error, index_not_built, start_server, AccountAssetJson, AccountAssetsJson, AccountJson,
    ApprovalJson, ApprovalsJson, AssetJson, AssetStatusJson, Backend, BalanceJson, BlockId,
    BlockJson, BlockRef, BlockSummary, CallRequestJson, ChainInfoJson, ContractJson, Cors,
    EstimateFeeJson, HolderJson, HoldersJson, NftJson, NftsJson, OpListJson, OpPosition,
    OpStatusJson, ReceiptJson, SettlementJson, SubmitResult, TokenJson,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

const ACCOUNT: [u8; 32] = [0x11; 32];
const TOKEN: [u8; 32] = [0x22; 32];
const COLLECTION: [u8; 32] = [0x33; 32];

/// What a listing was asked: its method, the cursor and the size of the page.
type Asked = (&'static str, Option<Vec<u8>>, usize);

/// A devnet node. With `indexed`, it has the index: [`ACCOUNT`] holds KSA, the token [`TOKEN`] and two NFTs of
/// [`COLLECTION`], listed two at a most whatever the size asked; without, its index methods answer as a node without
/// them.
struct Node {
    indexed: bool,
    asked: Mutex<Vec<Asked>>,
    blocks: broadcast::Sender<BlockSummary>,
}

fn absent<T>() -> RpcResult<T> {
    Err(error(codes::NOT_FOUND, "not found"))
}

fn at() -> BlockRef {
    BlockRef {
        height: 7,
        hash: [0x7e; 32],
    }
}

/// A listing's cursor of an asset (`ksana_getAccountAssets`): 0 and an asset ID, 1 and a contract ID.
fn cursor(asset: &AccountAssetJson) -> Vec<u8> {
    let (kind, id) = match asset {
        AccountAssetJson::Asset { asset, .. } => (0, asset),
        AccountAssetJson::Token { contract, .. } | AccountAssetJson::Nft { contract, .. } => {
            (1, contract)
        }
    };
    [[kind].as_slice(), id.as_slice()].concat()
}

impl Node {
    fn index(&self, method: &'static str, after: Option<Vec<u8>>, limit: usize) -> RpcResult<()> {
        if !self.indexed {
            return Err(index_not_built());
        }
        self.asked.lock().unwrap().push((method, after, limit));
        Ok(())
    }
}

impl Backend for Node {
    fn submit_op(&self, _: Vec<u8>) -> RpcResult<SubmitResult> {
        absent()
    }
    fn op_status(&self, _: [u8; 32]) -> RpcResult<OpStatusJson> {
        absent()
    }
    fn block(&self, _: BlockId) -> RpcResult<BlockJson> {
        absent()
    }
    fn account(&self, account: [u8; 32]) -> RpcResult<AccountJson> {
        Ok(AccountJson {
            account,
            nonce: 3,
            balances: vec![BalanceJson {
                asset: KSA_ASSET_ID,
                amount: 7,
            }],
            locked_fees: Vec::new(),
            at: at(),
        })
    }
    fn asset(&self, _: [u8; 32]) -> RpcResult<AssetJson> {
        absent()
    }
    fn asset_status(&self, _: [u8; 32], _: [u8; 32]) -> RpcResult<AssetStatusJson> {
        absent()
    }
    fn call(&self, _: CallRequestJson) -> RpcResult<ReceiptJson> {
        absent()
    }
    fn settlement(&self, _: BlockId) -> RpcResult<SettlementJson> {
        absent()
    }
    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson> {
        absent()
    }
    fn chain_info(&self) -> RpcResult<ChainInfoJson> {
        Ok(ChainInfoJson {
            chain_id: DEVNET.chain_id,
            tip: at(),
            daa_score: 0,
            protocol_version: 2,
            next_upgrade: None,
            max_known_version: 2,
            finalized: at(),
        })
    }
    fn account_ops(&self, _: [u8; 32], _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn latest_ops(&self, _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn subscribe_blocks(&self) -> broadcast::Receiver<BlockSummary> {
        self.blocks.subscribe()
    }

    fn contract(&self, contract: [u8; 32]) -> RpcResult<ContractJson> {
        self.index("contract", None, 0)?;
        Ok(ContractJson {
            contract,
            code_hash: [0xc0; 32],
            deployer: ACCOUNT,
            standards: vec!["KS-20".into(), "KS-20-burnable".into()],
            declared: Some(OpPosition {
                height: 2,
                slot: 0,
                entry: 1,
            }),
            at: at(),
        })
    }

    fn account_assets(
        &self,
        account: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<AccountAssetsJson> {
        self.index("assets", after.clone(), limit)?;
        let all = if account == ACCOUNT {
            vec![
                AccountAssetJson::Asset {
                    asset: KSA_ASSET_ID,
                    balance: 7,
                },
                AccountAssetJson::Token {
                    contract: TOKEN,
                    balance: 1_000,
                },
                AccountAssetJson::Nft {
                    contract: COLLECTION,
                    count: 2,
                },
            ]
        } else {
            Vec::new()
        };
        let start = match after {
            None => 0,
            Some(after) => all
                .iter()
                .position(|a| cursor(a) == after)
                .map_or(all.len(), |i| i + 1),
        };
        let assets: Vec<AccountAssetJson> =
            all.iter().skip(start).take(limit.min(2)).cloned().collect();
        let next = (start + assets.len() < all.len())
            .then(|| assets.last().map(|a| hex::encode(cursor(a))))
            .flatten();
        Ok(AccountAssetsJson {
            assets,
            next,
            at: at(),
        })
    }

    fn token(&self, contract: [u8; 32]) -> RpcResult<TokenJson> {
        self.index("token", None, 0)?;
        Ok(TokenJson {
            contract,
            standard: "KS-20".into(),
            extensions: vec!["KS-20-burnable".into()],
            name: Some("Ksana Coin".into()),
            symbol: Some("KCN".into()),
            decimals: Some(8),
            supply: 1_000,
            holders: 1,
            declared: OpPosition {
                height: 2,
                slot: 0,
                entry: 1,
            },
            irregular: false,
            at: at(),
        })
    }

    fn token_holders(
        &self,
        _: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<HoldersJson> {
        self.index("holders", after, limit)?;
        Ok(HoldersJson {
            holders: vec![HolderJson {
                account: ACCOUNT,
                balance: 1_000,
            }],
            next: None,
            at: at(),
        })
    }

    fn nfts(
        &self,
        _: [u8; 32],
        contract: Option<[u8; 32]>,
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<NftsJson> {
        self.index("nfts", after, limit)?;
        Ok(NftsJson {
            nfts: vec![NftJson {
                contract: contract.unwrap_or(COLLECTION),
                token_id: [0x01; 32],
            }],
            next: None,
            at: at(),
        })
    }

    fn approvals(
        &self,
        _: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<ApprovalsJson> {
        self.index("approvals", after, limit)?;
        Ok(ApprovalsJson {
            approvals: vec![ApprovalJson::Allowance {
                contract: TOKEN,
                spender: [0x44; 32],
                amount: 50,
                expires_at: Some(1_900_000_000),
            }],
            next: None,
            at: at(),
        })
    }
}

/// The node of one case, served until dropped.
struct Services {
    node: Arc<Node>,
    url: String,
    handle: Option<ServerHandle>,
    runtime: tokio::runtime::Runtime,
}

impl Drop for Services {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.stop();
            self.runtime.block_on(handle.stopped());
        }
    }
}

fn start(indexed: bool) -> Services {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let node = Arc::new(Node {
        indexed,
        asked: Mutex::new(Vec::new()),
        blocks: broadcast::channel(1).0,
    });
    let backend: Arc<dyn Backend> = node.clone();
    let (addr, handle) = runtime
        .block_on(start_server(
            "127.0.0.1:0".parse().unwrap(),
            backend,
            &Cors::Disabled,
        ))
        .unwrap();
    Services {
        node,
        url: format!("http://{addr}"),
        handle: Some(handle),
        runtime,
    }
}

impl Services {
    /// Runs `ksana-cli --rpc <node> <args>`.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ksana-cli"))
            .arg("--rpc")
            .arg(&self.url)
            .args(args)
            .env_remove("KSANA_RPC")
            .output()
            .unwrap()
    }

    fn asked(&self) -> Vec<Asked> {
        std::mem::take(&mut *self.node.asked.lock().unwrap())
    }
}

fn succeeded(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "ksana-cli failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn refused(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected a refusal, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn account_adds_the_tokens_and_nfts_of_every_page() {
    let services = start(true);
    let info = succeeded(&services.cli(&["account", &hex::encode(ACCOUNT)]));
    assert_eq!(info["nonce"], "3");
    assert_eq!(
        info["tokens"],
        json!([{ "contract": hex::encode(TOKEN), "balance": "1000" }])
    );
    assert_eq!(
        info["nfts"],
        json!([{ "contract": hex::encode(COLLECTION), "count": "2" }])
    );
    // KSA is listed once, among the balances
    assert_eq!(info["balances"].as_array().unwrap().len(), 1);
    // Two pages: the second continues after the token
    assert_eq!(
        services.asked(),
        [
            ("assets", None, 500),
            ("assets", Some([[1].as_slice(), &TOKEN].concat()), 500),
        ]
    );

    // An account holding none of them
    let info = succeeded(&services.cli(&["account", &hex::encode([0x55; 32])]));
    assert_eq!(info["tokens"], json!([]));
    assert_eq!(info["nfts"], json!([]));
}

#[test]
fn account_leaves_them_out_with_a_node_without_the_index() {
    let services = start(false);
    let info = succeeded(&services.cli(&["account", &hex::encode(ACCOUNT)]));
    assert_eq!(info["nonce"], "3");
    assert_eq!(info.get("tokens"), None);
    assert_eq!(info.get("nfts"), None);
}

#[test]
fn the_index_queries_print_the_nodes_answers() {
    let services = start(true);
    let (account, token, collection) = (
        hex::encode(ACCOUNT),
        hex::encode(TOKEN),
        hex::encode(COLLECTION),
    );
    let answer = succeeded(&services.cli(&["token", &token]));
    assert_eq!(answer["standard"], "KS-20");
    assert_eq!(answer["decimals"], json!(8));
    assert_eq!(answer["supply"], "1000");
    assert_eq!(answer["declared"], "2:0:1");
    let answer = succeeded(&services.cli(&["contract", &token]));
    assert_eq!(answer["standards"], json!(["KS-20", "KS-20-burnable"]));
    assert_eq!(answer["deployer"], account);

    // The page's cursor and size go to the node as given; without them, its default
    let after = "ab".repeat(48);
    let answer = succeeded(&services.cli(&["holders", &token, "--after", &after, "--limit", "3"]));
    assert_eq!(answer["holders"][0]["balance"], "1000");
    assert_eq!(answer["next"], Value::Null);
    let answer = succeeded(&services.cli(&["nfts", &account, "--contract", &collection]));
    assert_eq!(answer["nfts"][0]["contract"], collection);
    let answer = succeeded(&services.cli(&["approvals", &account, "--limit", "1"]));
    assert_eq!(
        answer["approvals"],
        json!([{
            "kind": "allowance",
            "contract": token,
            "spender": "44".repeat(32),
            "amount": "50",
            "expires_at": "1900000000",
        }])
    );
    assert_eq!(
        services.asked(),
        [
            ("token", None, 0),
            ("contract", None, 0),
            ("holders", Some(vec![0xab; 48]), 3),
            ("nfts", None, 50),
            ("approvals", None, 1),
        ]
    );

    // Arguments the node refuses: a cursor of another length, a page of 0
    for args in [
        ["holders", token.as_str(), "--after", "abcd"],
        ["approvals", account.as_str(), "--limit", "0"],
    ] {
        let stderr = refused(&services.cli(&args));
        assert!(
            stderr.contains("after") || stderr.contains("limit"),
            "{stderr}"
        );
    }
    assert!(refused(&services.cli(&["token", "zz"])).contains("contract ID"));

    // Without the index, each fails with the node's word
    let services = start(false);
    for args in [
        vec!["contract", token.as_str()],
        vec!["token", token.as_str()],
        vec!["holders", token.as_str()],
        vec!["nfts", account.as_str()],
        vec!["approvals", account.as_str()],
    ] {
        let stderr = refused(&services.cli(&args));
        assert!(stderr.contains("index is not built"), "{args:?}: {stderr}");
    }
}
