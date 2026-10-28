# Ksana: Technical Specification of the BTC Fast-Lane VM Chain

v2 final · 2026-10-05, amended by D-097 and D-100 on 2026-10-06, by D-101 and D-104 on 2026-10-08 and by D-105 on 2026-10-12 (v2: 2026-10-03; v1: 2026-09-30)

This version (v2 final) is the frozen consensus: v2 (2026-10-03) merged the decisions made during implementation after v1 and confirmed by the project owner (`docs/DECISIONS.md`,
D-001 to D-086) back into the specification text; this version additionally merges in D-087 to D-096, and closes out all the decisions in DECISIONS that were originally marked "pending confirmation".
The project owner signed off on 2026-10-05: apart from the receiving wallet of the block share, the design is concluded (D-096).
This document is the single source of requirements for the protocol. In the body text, a parenthesized
"(D-0xx)" points to the corresponding entry in DECISIONS, which only records the rationale, trade-offs and process; the rules themselves are as stated here, and this document plus `docs/ABI.md` (contract interface and
gas table) and `docs/RPC.md` (JSON-RPC) are enough to build an implementation compatible with the existing node and indexer, without reading DECISIONS.

Main changes compared with v1: path A (merged payment) and fee debt are deleted, and paying with BTC becomes "top up for KSA + path B" (§5); the signature of a path C op is bound to the first input of the
BTC transaction that carries it (§4.2); the fast-lane consensus becomes a GHOSTDAG block DAG, block header v2 carries multiple parents, difficulty is adjusted by DAA score and distinguishes empty blocks from non-empty blocks (two-speed difficulty:
the difficulty of consecutive empty blocks doubles block by block up to a cap), and timestamps are constrained by the time of the anchored BTC block (§6); finality determined by BTC chain data is added (§6.5); settlement and republish payloads are placed in an envelope in the Taproot witness, block headers and entries use compact encoding, and one settlement transaction can carry the data of several blocks (§7);
KSA has a max supply and block rewards, and sponsored fees and rewards are unlocked cumulatively by acceptance position (§8); the contract model is aligned with the EVM (attached funds, catchable calls, delegate calls, contracts creating
contracts, §9); there is an upgrade mechanism activated by DAA score (§6.9).

Version history:

- **2026-10-28, the source published without mainnet's block share (D-134)**: no protocol change. The repository is published with mainnet's block share (its fraction, its end and its
  account) left out until the project owner announces it: `MAINNET` holds placeholders for `BLOCK_SHARE_BPS` and `BLOCK_SHARE_END`, a mainnet node or indexer does not start until they are
  filled in (§6.1), and §8.6, §13, §13.1 and §17 leave the values out. devnet and signet are unchanged.
- **2026-10-28, limits of the P2P layer and the RPC servers (M49, D-132)**: no consensus change, the first internal audit's F-28 and F-29, which the second audit's report still listed as
  open. Inbound connections are capped in all, in the handshake, per address and per peer, and a peer banned by score bans its address too; D-059's quotas belong to the address a peer
  connects from and outlive its connections; the gossip awaiting validation is held within a byte budget per topic and per source, a gossip op longer than `MAX_OP_BYTES` is rejected before it
  is decoded, and inbound sync requests are served a few at a time within a byte budget, with lower limits for requests and for the responses to `Status` and `GetMempool` (§11.1). The node's
  and the indexer's RPC servers take batches of at most 64 calls, bodies of at most 1 MiB and 16 subscriptions per connection, and `ksana_getAccount` no longer scans every balance nor walks the
  main chain once for every block waiting to unlock (§11.2). Local policy only: block validity, the encodings, the shared vectors and the golden state roots are unchanged; no restart is needed.
- **2026-10-28, a pinned pair goes out again (M48, D-131)**: no consensus change, the open part of the second internal audit's A2-09. Anyone can attach a large child at a low rate to a
  reveal's P2A output first, and a CPFP child of the node's would then have to pay more than that child's whole fee, so a lone producer's settlement, a rescue or a republish could wait for as
  long as the pin held. When bitcoind refuses the node's first child while another party's transaction spends the P2A output, the pair's payload now goes out again in a new pair of the node's
  at the bump's rate, which spends nothing of the old pair's, and that pair takes the old one's place; nothing more is spent on a pair whose block's data has landed (§7.2, §7.3, §7.7, §14, §15).
- **2026-10-28, entry-pool churn (M47, D-130)**: no consensus change, from the second internal audit's A2-08. Every op admitted to the entry pool made the producer throw its template
  away and build another, ops behind a nonce gap included, which no block takes and which could be replaced for one more unit of fee over and over. A replacement now raises the sponsored
  fee by at least a tenth and by at least the relay floor (§8.2); only a change that can alter the packing candidates makes the template stale, and only once the template has been solved for
  four times as long as building it took, while a new main-chain tip, a change of the block DAG and a new BTC snapshot still rebuild it at once (§6.2); the producer reads each account's nonce
  once per main-chain tip. Local policy only: block validity, the encodings, the shared vectors and the golden state roots are unchanged; no restart is needed.
- **2026-10-18, the floor fee rate while the next BTC block has room (M32, D-117)**: no consensus change, from the project owner's report of the signet's settlement fees. Outside a race
  with a competing chain, the node's new BTC transactions pay the floor, or bitcoind's mempool minimum when that is higher, while bitcoind's mempool holds at most half a block, instead of
  `estimatesmartfee`; on signet the estimate stayed at the seed nodes' own rate of 100 sat/vB while signet's blocks took 1. A pair left waiting for 2 BTC blocks is still bumped to the estimate, and after
  2 BTC blocks with room to at least twice its own rate, doubling for every further one, so that the bump does not depend on the estimate (§7.2, §7.7); what the review of D-117 left for mainnet is in §14.
- **2026-10-14, BTC-final waits for the BTC block of the determination (M38, D-114)**: no consensus change, from the review of 2026-10-14 of how deep a BTC reorg must go to take
  back a BTC-final block. A determination rests on the data of the block's closure, `BTC_FINALITY_CONFS` BTC blocks deep, and on no heavier candidate having connected by the BTC height
  determined, which is only as deep as the BTC block that made it: right after a block was determined final, a reorg of that one BTC block whose new branch carried a heavier chain forking below
  the block took it back, while §6.5, §10.4, §13.1 and §16 said that took at least `BTC_FINALITY_CONFS` blocks. Nodes and indexers now report a block BTC-final (`finalized` of
  `ksana_getChainInfo` and `indexer_status`, `btc_final` of ops, settlements and the op listings) only once the BTC block of its determination has `BTC_FINALITY_CONFS` confirmations, so taking a
  reported block back takes a BTC reorg of at least `BTC_FINALITY_CONFS` blocks, and it leaves the finalized chain only when that reorg's new branch carries a heavier chain or when a reorg of
  at least `2 × BTC_FINALITY_CONFS − 1` blocks removes its data; BTC-final comes after about 110 minutes instead of 60 (§6.5, §10.4, §11.2, §13.1, §15, §16). The determination itself, the floor of the main chain, the verdicts and pruning do not wait. One fix
  of the determination's code goes with it, where the code departed from §6.5 item 4 (the rule is unchanged): what was determined at a BTC height the determination paused at is determined again
  from the start when it resumes, so that a reorg replacing that BTC block alone revokes it as the item requires. The encodings, the shared vectors and the golden state roots are unchanged; no restart is needed.
- **2026-10-13, the public signet (M32, D-107)**: no consensus change. Ksana's public test network runs on Bitcoin's public signet, hosted on one server (`signet/`, deployed over ssh) with a
  seed node, an indexer, the web wallet, a BTC proxy for the wallet's path C and a KSA faucet; before it starts, a relay probe checks that signet's miners mine both data carriers (§7.1, §7.2).
  `docs/SIGNET.md` becomes the guide for joining it (§12, §14).
- **2026-10-13, clients take the network from the node (M31, D-106)**: no consensus change, from the second internal audit's A2-11. The CLI reads the node's chain ID before every command that
  talks to the node and signs for that network, `--chain` only stating the network expected; path C and top-up also refuse a bitcoind on another BTC network (§11.4). The TS SDK has no default network:
  what signs, builds or decodes takes the parameters of the node, picked by its chain ID (§11.3). The web page checks the node's chain ID when it connects and before anything is signed, and refuses
  mainnet for now; its Bitcoin RPC must be on the network's BTC network too before coins are listed, a transaction is broadcast or anything is signed, and so must the wallet, as far as it says which
  network it is set to (a `tb1` address does not tell signet from testnet3 or testnet4). An op signed for another network's chain ID is still force-included when it comes as path C, and fails while its BTC payment confirms (§16).
- **2026-10-12, halving periods that double (M30, D-105)**: a consensus change for mainnet only, decided by the project owner: the halving periods of the block reward double in length twice and then
  stay, 12 500 000, 25 000 000 and then 50 000 000 DAA score (about 2, 4 and then 8 years), at 4.2 KSA per slot, so that the subsidy paying for landing data on BTC lasts decades longer; the max supply stays
  210 million and the block share is unchanged (§8.6, §13, §13.1). The new parameter `REWARD_INTERVAL_DOUBLINGS` is 0 on devnet and signet, whose issuance is unchanged; no
  restart is needed for this change. The encodings, the shared vectors and the golden state roots are unchanged.
- **2026-10-08, a deep BTC reorg takes back the fast-lane blocks anchored on the BTC blocks it orphans (M29, D-104)**: a change of the fork choice decided by the project owner the same day, which
  removes D-101's known limitation; no block becomes valid or invalid. Every main-chain block above the finalized tip must be eligible: when a BTC reorg of at least `ANCHOR_DEPTH` + 1 blocks takes the anchor
  of one of them, or of a block it merges, off the BTC best chain, the main chain retreats below the lowest such block and is chosen again, on the nodes and in the indexer's header DAG alike, instead of
  stopping finality for good (§6.4, §6.5, §10.3). The ops of the blocks taken back are included again on the new branch, together with the native transactions they included (in the new BTC order
  where the reorg moved them), so the limitations §17.3 listed for deep reorgs are gone; what remains is that a native transaction included again runs at the start of its block, where its op can fail
  out of turn (§6.8, §7.4, §10.4, §17.3).
  The local policy that goes with it: a competing chain counts only below its first block anchored off the BTC chain (§7.6), the funding inputs of the transactions of blocks taken back stay
  reserved while those transactions are in the BTC mempool (§7.3, §7.7), and the entry pool counts the nonces authenticated path C ops waiting to be included again take in turn from the account's
  nonce on as filled, so the path B ops taken back above them are not behind a nonce gap (§10.4), and `Status` leaves out the DAG tips anchored, or merging a block anchored, off the BTC best
  chain, and a block built on a block refused for now is refused with it rather than kept as an orphan (§11.1); a parallel block a retreat leaves behind, no tip any more, is merged again by the
  next block and carried in `Status` (§6.2, §11.1). `ANCHOR_DEPTH` stays 2 on mainnet. The encodings, the finality determination,
  the shared vectors and the golden state roots are unchanged; devnet and signet restart from genesis once, together with M28.
- **2026-10-08, bounded node memory (D-103)**: no consensus change. The node's and the indexer's databases are opened with fixed bounds on RocksDB's write buffers and one block cache shared by the databases of a
  process, and on Linux both allocate with jemalloc (§10.2, §10.3, §16): with RocksDB's defaults the write buffers kept every write for days, and glibc's allocator kept much of what the threads freed, so a node's
  memory still grew with the chain after pruning.
- **2026-10-08, only valid blocks become BTC-final (M28, D-101, D-102)**: a consensus change, from finding 1 of the path assessment of 2026-10-07. Once a candidate's data has landed with
  `BTC_FINALITY_CONFS` confirmations, the finality determination asks for the verdict of executing it on top of the finalized tip, and an invalid one is excluded together with the blocks built on it, so an invalid
  block landed on BTC can no longer become final and halt every indexer (§6.5); a block anchored, or merging a block anchored, off the BTC chain is excluded before anything else is asked about it, so that
  an anchor naming no BTC block cannot stop the determination either (corrected after the review of D-101); the block template never merges such a block, the main-chain selection never takes one, and a
  historical block taken from a peer is bound by the same anchor height limit as a new block, so that a block handed over through sync cannot stop it either (the second review of D-101, §6.2, §6.3, §6.4);
  nodes and indexers keep the states from the finalized tip on (§10.2, §10.3). The local policy that goes with it (D-102): the node's BTC
  fee rate follows `estimatesmartfee` within a floor and a cap, its waiting settlements, rescues and republishes are bumped through the P2A output, a block's own settlement waiting in the mempool holds a rescue back
  only as a claim does, and the next-block rate is paid while a competing chain is on BTC (§7.2, §7.3, §7.5–§7.7). The encodings, the shared vectors and the golden state roots are unchanged; devnet and signet
  restart from genesis.
- **2026-10-06, consensus fixes (M27, D-100)**: a consensus change, adopted by the project owner from the fixes the security audit proposed: the fork choice counts a tip's own work, `chain_work = blue_work + work`
  (§6.4, §6.5, §6.6); a block's body must fit in a republish payload, measured with every key written in full (§6.3); the forced-inclusion entries of an anchor-invalid block are checked against the current BTC best chain
  from its window start, a merged block's forced-inclusion entry naming no native transaction carrying its op on the anchor chain makes the merging block invalid, merged unlock entries are skipped, and re-executing an
  anchor-invalid block never judges it invalid (§6.3, §6.8, §7.4, §9.6); a payload counts only when its headers anchor below its own BTC height, and `MAX_SETTLEMENT_HEADERS` is checked when decoding (§7.1); the settlement
  payload a block carries has no header anchored above the block, and settlement transactions use the producer's `nSequence` (§7.2); `GHOSTDAG_K` is raised from 8 to 16 against withholding a heavy empty block (§13, §16).
  A second internal audit's fixes are included, with the local policy that goes with them (§7.3, §11.1). The encodings, the shared vectors and the golden state roots are unchanged; devnet and signet restart from genesis.
- **2026-10-06, mainnet default values confirmed (D-096)**: no consensus change, no code change. The project owner confirmed the six values D-096 had marked "default value, pending the owner's
  confirmation" (`CHAIN_ID`, `NATIVE_CONFIRMATIONS`, `ANCHOR_DEPTH`, `POW_LIMIT_BITS`, the Argon2id parameters, `REORG_WINDOW`); §13.1 records them as settled and §17.2 records the confirmation.
- **2026-10-06, security audit (M26, D-098)**: no consensus change. The confirmation-wait table in §10.4, the indexer no longer halting on a non-final invalid body (§10.3), the exchange's partial claims (§5.3), rescue cost and
  mempool claims (§7.3), the audit's threats in §15 / §16, the forged-anchor note in §6.8, and §17.4 listing the consensus fixes the audit proposes, which await the project owner.
- **D-097 (2026-10-06)**: the first consensus change after the freeze, by the "new network" route (mainnet has not launched; devnet and signet restart from genesis): the project owner
  chose to add the merge-depth root rule (§6.3, §6.6), which makes pruning exact; node and indexer prune their memory below a BTC-final pruning point and keep the history on disk
  (§10.2, §10.3); the encodings, the shared vectors and the golden roots are unchanged.
- **v2 final (2026-10-05, D-096)**: consensus freeze. Signed off by the project owner; the 29 decisions in DECISIONS originally marked "pending confirmation", together with the pending-confirmation points in D-068, D-074 to D-077 and D-089,
  are all closed out (each one: settled / superseded / deferred to after mainnet); the decisions after 2026-10-03, D-087 to D-095, are merged in (PoW fixed,
  no merged mining, block share, empty-block streak and competing-chain republishing, accounts are Taproot addresses, rebuilding missing blocks from BTC payloads, Argon2id, carrier reward); §13 becomes
  three columns devnet / signet / mainnet and §13.1 "Parameter rationale" is added (each parameter's mainnet value, rationale and status); the code adds the mainnet network `MAINNET` and the mainnet genesis config
  template; §14 adds M16 to M24 and the work items before mainnet; §17 retains just the only open item, the default values pending confirmation and the matters after mainnet. Any consensus change after this version goes through the §6.9 upgrade
  mechanism (or starts a new network), regenerates the shared vectors, and records a separate decision in DECISIONS.
- **v2 (2026-10-03)**: D-001 to D-086 merged back into the body text (after the M15 convergence).
- **v1 (2026-09-30)**: the original specification.

## 0. Implementation notes

This document is the single source of requirements for this project. Implementation proceeds in the order of the milestones in section 14, moving on to the next milestone only after all acceptance criteria of the current one pass; the milestones up to M24 are
complete (§14), and the consensus is frozen (v2 final, D-096): any consensus change after that must go through the upgrade mechanism of §6.9 (or start a new network), regenerate the shared vectors, and be
recorded in DECISIONS. What remains before mainnet is the non-consensus work items (end of §14) and the only open item (§17.1).

Working rules:

- The protocol rules are as stated in this document. When a rule turns out to be contradictory, flawed or impossible to implement, do not change it on your own; record the problem and a proposed solution in `docs/DECISIONS.md`, and adopt the most conservative implementation first.
- The remaining matters of section 17 (the only open item, the matters deferred to after mainnet) are always implemented according to the default option given there until settled, as configurable parameters or replaceable modules.
- All logic that affects consensus must be deterministic: no floating-point arithmetic, no dependence on system time (time is passed in by the caller), no dependence on the iteration order of HashMap / HashSet;
  consensus crates forbid `unwrap` / `expect` / `panic!` / out-of-bounds indexing / unchecked integer overflow; errors are always returned and the block or op is judged invalid (§12).
- Write interfaces and tests first, then the implementation. Each commit corresponds to one verifiable feature.
- Every crate must have unit tests, and every milestone must have integration tests that run on bitcoind regtest.
- The development and CI environment is Ubuntu 22.04; the core code is in Rust, the SDK in TypeScript (Node.js).
- This document distinguishes three kinds of rules: **consensus rules** (all nodes and indexers must agree; a violation means the block is invalid, cannot land on BTC, or the op fails), **reception rules** (checked against the local view only when receiving a new block,
  not when syncing historical blocks; a violation only rejects this one copy) and **local policy** (block production, entry pool, rescue, networking and so on, which may differ between nodes).
  Any rule in the text without such a qualifier is a consensus rule.

| Term | Meaning |
| --- | --- |
| fast lane | This project's chain: a PoW block DAG producing a block about every 5 seconds, carrying VM assets and contracts |
| slow lane | The BTC main chain itself, where operations on native BTC still take place |
| VmOp | One VM operation signed by a user, such as deploying a contract, calling a contract or transferring an asset |
| producer | The node that mined a fast-lane block through PoW; it is also responsible for building the settlement transaction |
| settlement transaction | The pair of BTC transactions (commit / reveal) that the producer builds for a fast-lane block, writing the block header and the entry data onto BTC |
| block body | All entries of a block in payload form; for a block that contains entries other than forced ones, the body must land on BTC |
| forced inclusion | A VmOp transaction sent by a user directly to BTC (path C); once confirmed, the fast lane must include and execute it |
| indexer | A program that reads only BTC data and independently recomputes the whole state; it is the source of final truth |
| KSA | The project token, a built-in VM asset whose asset ID is 32 zero bytes. Path B sponsored fees and block rewards are both KSA |
| selected parent / main chain | The parent with the largest `(chain_work, hash)` (its blue work plus its own work, §6.6); the chain from the heaviest tip that is not disqualified along selected parents down to the genesis block (the virtual selected chain), along which the state is executed |
| merge set / blue block / red block | The blocks in a block's past that are not in the past of its selected parent; GHOSTDAG colors them blue or red by k-cluster |
| DAA score | The score obtained by accumulating block weights along the selected chain: the time scale of difficulty adjustment, protocol version and block rewards |
| empty-block streak | The number of consecutive empty blocks on the selected chain up to a given block: 0 for a non-empty block, the selected parent's streak plus one for an empty block; it determines the weight of an empty block |
| weight | 1 for a non-empty block; for an empty block it doubles block by block with the streak from `EMPTY_BLOCK_DIFFICULTY` (12) up to `MAX_EMPTY_BLOCK_WEIGHT` (120): the difficulty multiplier, the DAA score increment and the number of reward slots |
| acceptance position | The `(height, index)` at which a block is accepted by the main chain: a main-chain block first accepts the blocks it merges and accepts itself last |
| settled closure | The longest prefix of the main chain, up to some height, in which each block together with its entire past has all of its data on BTC |
| BTC-final | The finalized chain determined from BTC chain data, as nodes and indexers report it: its data had `BTC_FINALITY_CONFS` confirmations on BTC while no heavier chain had landed on BTC in time, and the BTC block of that determination has `BTC_FINALITY_CONFS` confirmations too (§6.5 item 7) |

## 1. Project overview

Ksana is a BTC-native programmable VM chain: users top up with BTC for KSA and then pay gas in KSA while the producer sponsors the BTC fee, or submit operations directly with their own
BTC transactions; the fast lane confirms in seconds, every operation is eventually written to BTC, and finality is determined by BTC chain data. Existing projects are either fast but dependent on a single operator (such as Arkade),
or decentralized but slow (such as OP_NET); Ksana achieves both with a permissionless PoW fast lane.

The name is taken from the Sanskrit kṣaṇa, the smallest unit of time in Buddhist scripture: the fast lane confirms within an instant, and the result is finally written into the eternity of Bitcoin.

Goals:

- VM operations take effect on the fast lane within about 5 seconds (devnet target).
- Users can do everything with native BTC alone: path C submits directly with a BTC transaction; for confirmation in seconds, one BTC transaction tops up an exchange contract for KSA, after which sponsorship is used.
  Sponsorship accepts only the project token, and the protocol sets no exchange rate.
- The data of every fast-lane operation is eventually written to BTC, and anyone can recompute the whole VM state from BTC data alone; a block whose data is deep enough on BTC is never reorged again.
- Block production is permissionless, and anyone can take part through PoW; blocks produced in parallel are all merged, so no hashrate is wasted.
- Compatible with existing BTC wallets: wallets that support BIP322 message signing can sign operations directly, and wallets that support PSBT signing can send path C transactions.

Non-goals (not in this phase):

- No custody of users' BTC and no BTC cross-chain bridge. Transfers of native BTC still go through the BTC main chain.
- No EVM compatibility and no Ethereum integration (left for a later phase).
- No changes to the BTC protocol and no dependence on any soft fork.
- No mainnet launch in this phase: what is delivered is a devnet that runs on regtest and signet, plus the frozen mainnet parameters (§13) and the mainnet genesis config template (§6.1, `mainnet/`); before the mainnet
  launch there remain the non-consensus work items at the end of §14 and the only open item (the receiving wallet of the block share, §17.1).

## 2. Core design principles

The following decisions are settled and must not be changed during implementation; if a conflict is found while implementing, record it according to the rules of section 0.

1. **Two-lane structure**. The fast lane carries VM assets and contracts and produces its own blocks; operations on native BTC still follow BTC blocks. The two ecosystems are independent of each other, and the fast lane does not custody BTC.
2. **Every operation is BTC data**. A VmOp is either written to BTC with the producer's settlement transaction or carried by the user's own BTC transaction. Operating the VM only requires carrying data.
3. **Fast-lane order is authoritative, BTC decides finality**. The VM state follows the fast lane's block order and takes effect as soon as the fast-lane block is produced, without waiting for BTC confirmation. BTC is responsible for data availability and finality:
   the settlement situation never causes a finalized block to roll back (§6.5); a node that chose another chain before a block was finalized is forced to switch to the finalized chain.
4. **Batching is allowed**. One settlement transaction can carry all entries of a block, and also the block headers and bodies of other blocks in its past (§7.5).
5. **Merging and sponsorship are bound together**. Only the producer who solved the PoW block can build the settlement transaction for that block: it writes the block's VmOps, advances the BTC fee for path B, and collects the project token
   and the block reward. The right to merge rotates with hashrate, and anyone can take part in block production. The consensus code must be a replaceable module: the `Consensus` trait and the replaceable mining algorithm
   `PowAlgorithm` (D-062); the default implementation is GHOSTDAG on Argon2id PoW (CPU-friendly, D-094).
6. **The VM recognizes signatures, not UTXOs**. A VmOp is authorized by a BIP340 Schnorr signature or a BIP322 simple signature, and the account is the user's Taproot address (its 32-byte output
   key, D-092). The sponsored and native paths look the same to the VM.
7. **The BTC path uses only BTC, sponsorship accepts only the token**. Path C pays transaction fees only to BTC miners; path B is priced and paid in the project token, each producer quotes its own price, and the protocol sets no
   exchange rate. Exchange between BTC and KSA is done by VM contracts (the example contract `ksa-exchange`, §5.3) and is not part of the core protocol. Users who hold no token can still use
   every feature through path C.
8. **Censorship-resistance baseline**. Users can bypass the fast lane and send a transaction carrying a VmOp directly on BTC, and the fast lane must include it by force. Beyond that, producers are free to choose which
   fast-path entries to include; this is market behavior, and the protocol sets no other forced-inclusion rule. Forced inclusion executes an op; it does not make an asset movable that its
   issuing contract has frozen, excluded or paused under the capabilities the asset declared when it was created (§9.5, D-121). KSA and assets without them are never stopped.
9. **Self-certifying data**. A block header certifies itself by PoW and parent hashes, and a block body by the `entries_root` in the header: a payload sent to BTC by anyone can make a block's data
   land on BTC, the indexer accepts payloads from any source, and duplicates are ignored (D-063, D-077).

## 3. System architecture overview

The system consists of four parts: user wallets and the SDK, fast-lane nodes, the Bitcoin main chain, and the standalone indexer. Fast-lane nodes execute immediately, the Bitcoin main chain stores the data and decides finality,
and the indexer recomputes the whole state from BTC data alone.

![The fast lane executes first, BTC stores the data, and the indexer can recompute independently. Solid lines are the normal data flow, dashed lines the native-transaction channel that bypasses the fast lane.](images/architecture.png)

System architecture · fast-lane nodes, the Bitcoin main chain and the standalone indexer

A user's VmOp first enters a fast-lane node for execution, then the producer writes it into a settlement transaction that lands on BTC; a native transaction can bypass the fast lane and go onto BTC directly, and is force-included once confirmed.

- **Fast-lane node**: entry pool → VM execution → PoW block production → settlement construction and delayed broadcast → rescue and republishing. Multiple nodes sync blocks and operations over P2P.
- **Bitcoin main chain**: carries settlement transactions, republish transactions and native transactions; it is the data availability layer and the source of finality.
- **Standalone indexer**: connects only to bitcoind, rebuilds the fast-lane block DAG and verifies the `state_root` block by block.
- **User wallet and SDK**: signs VmOps (BIP340 or BIP322) and builds path C native transactions and top-up transactions (handed to the wallet for PSBT signing).

## 4. Accounts, signatures and VmOp

An account is the 32-byte output key of the user's Taproot address (BIP86 key-path, no script tree: Q = P + H_TapTweak(P)·G, D-092). The address is the bech32m
spelling of the same bytes (devnet / regtest `bcrt1p…`, signet `tb1p…`, mainnet `bc1p…`): the address you give someone for receiving BTC and for receiving KSA is the same one, pasting it in the wrong place still lands on the same key,
and account and address convert to each other both ways. Wherever an account is accepted (CLI, SDK, the node's `--producer`), either an address or 64 hex chars is accepted. The VM identifies accounts only through signatures and does not care who spent
which UTXO. A contract ID (§9.2) is also 32 bytes and shares the balance table with user accounts; it is not a key, has no address form, and is written only as 64 hex chars (clients do not parse non-Taproot
addresses into accounts, and 32 bytes off the curve cannot be turned into an address). Before D-092 the account was the untweaked internal key P, and the public key of a P2WPKH address could also serve as an account (D-052); both have been withdrawn.

### 4.1 VmOp structure

All consensus data is Borsh-encoded: structs are the concatenation of their fields in declaration order, enums are prefixed with a 1-byte variant index (from 0 in declaration order), `Vec` / `String` are prefixed with a u32
little-endian length, and `Option` with a 1-byte tag. Field order and variant order are the encoding format; changing them is a consensus change (regenerate `tests/vectors/` and record it in DECISIONS).
The TypeScript SDK must produce byte-for-byte identical encodings. All BTC transaction IDs and block hashes are stored in Bitcoin's internal byte order (the raw sha256d output, the reverse of the hex shown by RPC and
block explorers, D-001).

```rust
pub const VMOP_VERSION: u8 = 2;      // from 2 on, Call / Deploy carry funds (D-073)
pub const MAX_CALL_FUNDS: usize = 16;
pub const KSA_ASSET_ID: [u8; 32] = [0; 32];

pub struct VmOp {
    pub version: u8,          // currently 2; the accepted version range is given by the rules table (§6.9)
    pub chain_id: u32,        // distinguishes devnet / signet / mainnet, prevents cross-network replay
    pub account: [u8; 32],    // account: the output key of the Taproot address (x-only, D-092)
    pub nonce: u64,           // strictly increasing from 0 within the account
    pub fee: FeeMode,         // see section 8
    pub max_gas: u64,         // execution limit; exceeding it fails the op, but the fee is still charged
    pub action: Action,
    pub sig: SigScheme,       // not part of the sighash
}

pub enum FeeMode {
    Token { amount: u128 },   // path B: sponsored, pays KSA (variant 0)
    BtcNative,                // path C: native transaction (variant 1)
}

pub enum Action {
    Deploy { code: Vec<u8>, init_args: Vec<u8>, funds: Vec<Funds> },
    Call { contract: [u8; 32], method: String, args: Vec<u8>, funds: Vec<Funds> },
    AssetTransfer { asset: [u8; 32], to: [u8; 32], amount: u128 },
}

pub struct Funds { pub asset: [u8; 32], pub amount: u128 }   // built-in asset attached to the call (D-073)

pub enum SigScheme {
    Bip340([u8; 64]),          // used by the SDK and CLI
    Bip322Simple(Vec<u8>),     // used by browser wallets (D-052)
}
```

- `funds` (D-073): built-in assets transferred to the contract with `Deploy` / `Call`, the multi-asset counterpart of `msg.value`. Strictly ascending by asset ID (no duplicates), each amount
  greater than 0, at most `MAX_CALL_FUNDS` = 16 items; the op fails if this does not hold or the balance is insufficient (checked at execution time, not an encoding rule). Before the called contract is entered, the assets are transferred from the signing
  account to the contract account; on deployment they are transferred to the new contract before `init` runs; when the op fails they are rolled back together with the other state changes.
- Path A's `FeeMode::BtcMerged` and `Entry::Merged` are deleted (D-078); the variant indices above are the current encoding.

### 4.2 Signature rules

- The sighash is `sighash = tagged_hash("Ksana/VmOp/v1", message)` (BIP340 tagged hash). For path B (`fee` is `Token`) the message is the Borsh encoding of the VmOp without the `sig`
  field; for path C (`fee` is `BtcNative`) the message is followed by **the output spent by the first input** of the BTC transaction that carries it: `txid` (32 bytes,
  internal byte order) ‖ `vout` (u32 little-endian), 36 bytes in total (D-086). The two kinds of message differ at byte 46 (the variant index of `fee`), so they cannot be confused. The tagged hash guarantees
  that a VmOp signature can never be used as a BTC transaction signature, and vice versa.
- A path C signature is therefore valid only for this one BTC transaction: an op copied into another transaction does not pass authentication (§7.4). The outpoint is bound rather than the transaction ID because the data output itself
  contains this signature; an output can be spent only once on the whole BTC chain, so the first input is unique on-chain (a coinbase has no real input and is not a native transaction). Only the first
  input is bound so that the signature can be fixed before the wallet's coin selection (which appends inputs); the cost is that the first input has to be decided first when building the transaction (§11.3).
- `Bip340(sig)`: `sig` is the BIP340 Schnorr signature over `sighash`, and the verification key is `account` itself (that is, the Taproot output key; verification applies no tweak);
  a local signer therefore signs with the BIP86-tweaked private key (D-092).
- `Bip322Simple(bytes)` (D-052, D-092): a BIP322 simple signature over the same sighash; verifying it is equivalent to `Bip340` -- both prove that the caller controls the account
  (the Taproot address):
  - **Message**: the lowercase hex string of `sighash` (64 ASCII bytes). Wallets' BIP322 interfaces accept only text; BIP322's
    `message_hash = tagged_hash("BIP0322-signed-message", these 64 bytes)`.
  - **Signature bytes**: the BIP322 simple signature itself, that is, the Bitcoin consensus encoding of the witness stack of the `to_sign` input (the base64-decoded bytes returned by the wallet, without the `smp`
    prefix), at most 1 024 bytes long and fully consumed.
  - **Address** (`message_challenge`) is the account's Taproot address `OP_1 <account>` (D-092). The witness must have exactly 1 item -- a P2TR key-path signature,
    64 bytes (`SIGHASH_DEFAULT`) or 65 bytes with last byte `0x01` (`SIGHASH_ALL`), verified against the BIP341 sighash with `account` as the output key. Any other form
    (including the two-item P2WPKH witness that D-052 originally accepted) always fails verification; it also fails when `account` is not a valid x-only public key.
  - `to_spend` / `to_sign` follow BIP322 strictly: version 0, `nLockTime` 0, `nSequence` 0; the only input of `to_spend` spends `000…000:0xFFFFFFFF`,
    its scriptSig is `OP_0 PUSH32[message_hash]`, and its only output has amount 0 and locking script `message_challenge`; `to_sign` has a single `OP_RETURN` output.
  - The official BIP322 test vectors are stored in `crates/ksana-crypto/tests/data/bip322/` (the TS SDK keeps another copy) and are checked one by one.

### 4.3 nonce and replay

- Each account has one nonce, strictly increasing. The nonce of a path B entry must equal the account's nonce at execution time: when a main-chain block's own entry does not satisfy this, the block cannot land on BTC; when an entry of a merged
  block does not, it is skipped (§9.6). A producer must not include entries whose nonces are not consecutive.
- When the nonce of a path C entry does not match, it is recorded as a failed op, the state is unchanged, and the block remains valid (§7.4).
- `chain_id` must equal the chain ID of the network parameters; `version` must be within the VmOp version range accepted by the current rules (§6.9, currently only 2).
- For the same account, the entry pool accepts only a consecutive run of operations starting from the current nonce (local policy, §8.2).

### 4.4 Identifiers and limits

- `op_id = sha256(full VmOp encoding, including sig)`, used for indexing and queries.
- The encoded length of a single VmOp must not exceed `MAX_OP_BYTES` (section 13); anything longer is rejected outright; external input is checked against the length limit before decoding, and the input must be fully consumed.
- The asset ID of the project token KSA is 32 zero bytes; the asset ID of an asset issued by a contract is `sha256(issuing contract ID ‖ sub_id)` (`docs/ABI.md` §6).
- JSON representation of the types (RPC, shared vectors, D-007): byte strings are lowercase hex, `u64` / `u128` are decimal strings, `u8` / `u32` are numbers, enums are
  externally tagged, and unknown fields are rejected.

## 5. The two submission paths

Users have two paths to submit a VmOp: path B takes effect as soon as a fast-lane block is produced, and path C is the censorship-resistant channel that takes effect only after BTC confirmation. Path A (merging the input/output pair that the user signed with
`SIGHASH_SINGLE|ANYONECANPAY` into the settlement transaction) is deleted (D-078): that pair's signature had to be public over P2P, anyone could put it into their own
transaction and intercept the contribution, and on-chain a third party could not be told apart from the user; each one also needed a confirmed UTXO, a special sighash and a separate settlement transaction. Paying with BTC becomes topping up
for KSA first (§5.3).

| Path | Who pays the BTC fee | When it takes effect | Use |
| --- | --- | --- | --- |
| B: sponsored | The producer advances it, the user pays KSA | When the fast-lane block is produced | Default path; seconds, can be packed |
| C: native | The user's own complete BTC transaction | Force-included after BTC confirmation | Censorship-resistant channel; the claim op of a top-up |

### 5.1 Path B: sponsored

1. The user signs the VmOp with `fee = Token { amount }`, needs no BTC input, and submits it through any node's `ksana_submitOp`; the node validates it and broadcasts it over P2P.
2. After a producer includes it, the block's entries (the block body) are written to BTC by its own settlement transaction or by some later settlement / republish transaction (§7.5), and the producer pays the BTC fee.
3. Before the VM executes the op, `amount` KSA is deducted from the user's account and recorded in the locked record at the block's acceptance position (§8.5), owed to `header.producer`; it can be unlocked only after the data lands on BTC and the block enters
   the settled closure.
4. Producers decide for themselves which sponsorship requests to accept, by a locally configured minimum token amount; there is no network-wide pricing.

### 5.2 Path C: native

1. The user builds a complete BTC transaction of their own that carries the VmOp in an OP_RETURN (format in §7.1) with `fee = BtcNative`, pays the BTC fee themselves, and broadcasts it directly to the BTC network.
2. Fast-lane nodes do not include it before BTC confirmation, so this path has no double-spend problem.
3. Once the transaction has `NATIVE_CONFIRMATIONS` BTC confirmations, the fast lane must force-include and execute it according to the rules of §7.4.
4. Even if every producer refuses a user, their operations still take effect, only after waiting for BTC confirmation. Path C pays nothing to the producer, and a single op's `max_gas` must not exceed
   `NATIVE_MAX_GAS` (exceeding it fails the op).
5. A path C op can read the BTC transaction that carries it (transaction ID and all outputs) through the host function `native_tx`, which contracts use to verify payments (§9.4).

### 5.3 Paying with BTC: top-up

Exchanging BTC for KSA is done by VM contracts; the core protocol provides only path C and `native_tx`. The example contract `contracts/examples/ksa-exchange` implements the flow "order → lock →
confirm → redeem" (D-078, D-085); it is an application-level reference implementation, not a protocol rule, and anyone can deploy a different exchange contract:

1. **Order** (off-chain): buyer and seller agree on the amount, the price (how many KSA base units per sat) and the buyer's Ksana account.
2. **Lock** (seller, path B): the seller deposits KSA with `deposit` (or attaches it to `lock`), and `lock(ksa, price, script, beneficiary, expiry)` moves an amount of KSA from
   the available balance into a lock, stating the price, the BTC payment script used only by this lock (1 to 64 bytes, not `OP_RETURN`, never used by any lock before), the beneficiary
   (which cannot be the contract itself) and the expiry BTC height (which must be greater than the BTC height anchored by the current block). The terms of a lock cannot be changed, and the seller cannot take back KSA while it is locked.
3. **Confirm** (buyer, path C): the buyer sends an ordinary BTC transaction that has both an output paying the payment script and, carrying the claim op `claim(lock)`, a KSN1 data
   output. The claim op is signed with a **one-time key** (nonce 0): a path C nonce must equal the account's current nonce, so if it were signed with the buyer's account, any other op of the buyer before BTC confirmation,
   or several top-ups confirming in a different order, would void the claim. The signature is bound to the first input of this payment transaction (§4.2), so someone who copies the claim op into their own transaction
   cannot consume this nonce. After BTC confirmation the op is force-included, the contract reads the transaction with `native_tx` and converts the amount paid to the payment script into KSA at the price.
4. **Redeem**: `ksa_paid = min(paid_sats × price (saturating multiplication), KSA still locked)` is credited to the beneficiary stated at lock time (the claim op has no beneficiary parameter, so the destination
   cannot be changed). Once the whole lock is bought it enters `Claimed`; a payment that buys only part of it leaves the rest locked for the same beneficiary, and a later payment to the same script buys more
   (each BTC transaction can claim a lock once). Overpaying does not buy more KSA. Nothing returns to the seller at claim time, so a dust payment the seller makes to their own lock cannot close it in front of the
   buyer's payment (D-098). The buyer then uses KSA on path B.
5. **Release**: `RELEASE_DELAY` = 6 BTC blocks after expiry, a lock still in `Open` can be taken back by the seller with `release` (what is still locked: all of it, or the rest after partial payments). Claiming does not check expiry: a payment confirmed after expiry but before release
   is still valid; the 6 blocks give a buyer who pays at the last moment time to be force-included.

Before paying, the wallet must check that the lock's beneficiary is the buyer themselves, that the lock is still `Open`, that expiry is far enough away, and that the payment is neither more nor less than due (the SDK and CLI reject overpayment and underpayment by default; this can be explicitly allowed;
the expiry margin defaults to 3 BTC blocks). The payment must be confirmed before BTC height `expiry + RELEASE_DELAY − NATIVE_CONFIRMATIONS − 1` (the SDK's `payBy`):
the entries of merged blocks execute before the block's own forced-inclusion entries (§9.6), so if the block whose anchor jumps straight to `expiry + RELEASE_DELAY` merges a side-chain block carrying `release`, the release executes first.
Paying for a lock that is not `Open` fails the claim while the BTC has already been spent. Measured, one top-up transaction is about 381 vB; for 60 KSA transfers, the BTC bytes of "top-up + path B" are about 47% of the former path A's (before compression, D-078).

## 6. Fast-lane blocks, consensus and fork choice

The fast lane produces blocks with PoW, and the blocks form a GHOSTDAG block DAG (Kaspa's definitions and algorithms, D-063): blocks produced in parallel are all merged, and there are no more orphans; the devnet target interval is
5 seconds. Consensus is implemented as a replaceable `Consensus` trait, and the mining algorithm as a replaceable `PowAlgorithm` (D-062); the network's algorithm is Argon2id (RFC 9106,
2 MiB of memory, D-094): aimed at ordinary CPUs, and a browser can also verify a single block header.

### 6.1 Block structure

```rust
pub const BLOCK_VERSION: u32 = 2;    // base protocol version (§6.9)

pub struct BlockHeader {
    pub version: u32,            // protocol version: the version at this block's DAA score, 2 when there is no upgrade
    pub height: u64,             // selected-chain depth: parents[0].height + 1, 0 for genesis
    pub parents: Vec<[u8; 32]>,  // parents[0] is the selected parent, the rest in strictly ascending byte order; 1 to MAX_BLOCK_PARENTS of them, empty for genesis
    pub btc_anchor: BtcAnchor,   // the BTC block seen at production time (§6.8)
    pub entries_root: [u8; 32],  // Merkle root of all entries of this block
    pub state_root: [u8; 32],    // state root after execution
    pub producer: [u8; 32],      // producer account (Taproot output key, D-092), receives sponsored fees and block rewards
    pub timestamp: u64,          // seconds, used only for difficulty adjustment and the timestamp rules, readable by contracts via block_timestamp
    pub bits: u32,               // difficulty target (compact encoding)
    pub nonce: u64,
}

pub struct BtcAnchor { pub height: u32, pub hash: [u8; 32] }

pub struct Block {
    pub header: BlockHeader,
    pub entries: Vec<Entry>,
    pub settlement: Option<SettlementTxs>,   // the settlement transactions carried by the block (may be absent, §7.2, §7.5)
}

pub struct SettlementTxs { pub commit: Vec<u8>, pub reveal: Vec<u8> }   // Bitcoin network serialization (D-081)

pub enum Entry {
    Forced { btc_txid: [u8; 32], op: VmOp },   // path C (variant 0)
    Sponsored { op: VmOp },                     // path B (variant 1)
    Unlock { height: u64, index: u32 },         // cumulative unlock up to and including this acceptance position, see §8.5 (variant 2)
}
```

- The block hash is `sha256d(borsh(header))` (the header is 168 + 32 × parent-count bytes). The header does not reference the settlement transactions, because they are constructed only after the PoW is solved,
  and they in turn contain the header.
- `entries_root` (D-004): the RFC 6962 Merkle tree hash with SHA-256: `MTH({}) = sha256("")`, leaves `sha256(0x00 ‖ borsh(Entry))`, internal nodes
  `sha256(0x01 ‖ left ‖ right)`, and for n > 1, k is the largest power of 2 less than n. The root of the empty entry list, `sha256("")`, identifies an empty block.
- **Genesis block** (D-027): `version = 2`, height 0, empty `parents`, `btc_anchor` from the network config, no entries, `state_root` the root of the genesis state (genesis allocation),
  `producer` all 0, timestamp from the network config, `bits` defaulting to `pow_limit_bits` (configurable to be harder; it is the ASERT anchor target), `nonce = 0`; the genesis block's
  PoW is not checked. Genesis config file (`--genesis`, the same for node and indexer):

  ```json
  { "timestamp": "1790000000",
    "btc_anchor": { "height": 0, "hash": "<64 hex chars, internal byte order>" },
    "bits": 545259519,
    "allocations": [ { "account": "<64 hex chars>", "amount": "1000000000" } ],
    "block_share_account": "<64 hex chars>" }
  ```

  `bits` may be omitted; `block_share_account` (the block share account, §8.6) may be omitted, in which case the whole block reward goes to the producer; `timestamp` should be the time the network starts (ASERT
  uses the genesis block as its anchor, and a timestamp far earlier than the actual block production time keeps the difficulty at the floor for a long time); the total genesis allocation must not exceed `KSA_MAX_SUPPLY − upper bound of total rewards` (§8.6). The data directory
  records the config fingerprint, and opening it with a different config is refused. **The mainnet template** is `mainnet/genesis.json` (D-096, explained in `mainnet/README.md`): `block_share_account`
  is the placeholder `KSANA_BLOCK_SHARE_ACCOUNT_TBD___` (32 ASCII bytes), the timestamp and anchor are zero values, and there is no genesis allocation. A genesis config of any network that carries the placeholder cannot
  start; a mainnet config (by `CHAIN_ID`) must additionally give the block share account, have no genesis allocation (on mainnet the upper bound of total rewards is exactly the max supply), and have the anchor and timestamp filled in
  (`GenesisFile::check_network`, checked by node and indexer when building the genesis block, and checked once more by the node before it writes any data). Mainnet does not start at all yet:
  its block share is not published (§8.6, D-134), and a mainnet config that passes these checks is refused as well.

### 6.2 Block production flow

1. Pick the anchor: the BTC block at `max(parent anchor height, local BTC tip − ANCHOR_DEPTH)` (§6.8). Take the main-chain tip as the selected parent, and try adding the other DAG tips one by one
   in descending `(chain_work, hash)` order, skipping any whose GHOSTDAG computation fails (merge set or merge depth over the limit) or that would change the selected parent, up to
   `MAX_BLOCK_PARENTS` of them (local policy). A tip is also skipped when it would bring into the merge set a block anchored off the local BTC best chain (the BTC block at its anchor height is
   another one): no block merging it could become BTC-final (§6.5 item 2, D-101). Below such tips, the first blocks that are not, neither anchored nor merging a block anchored off that chain, are tried
   too, among the tips, when all their children are such blocks and the main-chain tip has not merged them: a parallel block that a block a retreat took back had merged (§6.4) is no tip any more, and is
   merged again, its ops ahead of the new block's own entries as before the reorg (local policy, D-104 item k).
2. Arrange the entries by the rules: forced-inclusion entries first (§7.4), then the unlock entry (at most one per block, §8.5); for the remaining path B entries the producer decides on its own whether to include them and
   how to order them (by fee rate from high to low, §8.2). When there are entries to include, prepare a non-empty block (weight 1), otherwise an empty block (weight by the empty-block streak at the main-chain tip, §6.6); if including at weight 1
   still yields no entries, redo it once as an empty block (the rules may differ, §6.7).
3. First execute the entries of the other blocks in the merge set in lenient mode, then this block's entries in strict mode (§9.6), obtaining `state_root` and `entries_root`.
4. The timestamp is `min(max(local time, median of the previous 11 timestamps on the selected chain + 1), anchor bound)` (D-030, D-083, §6.8); when the bound is already no higher than the median, do not produce a block,
   wait for BTC to advance. `bits` follows ASERT (§6.7), the version follows the DAA score (§6.9).
5. Decide whether to carry settlement transactions and assemble them (the batching policy of §7.5): the settlement payload also carries the headers and bodies in the past that have not yet landed on chain.
6. Solve the PoW (against the effective target for this block's weight, §6.6). The template is rebuilt whenever the main-chain tip, the block DAG or the BTC snapshot changes, and when a change
   of the entry pool can alter the packing candidates (§8.2) once the template has been solved for four times as long as building it took (local policy, D-130), so a new op that can be packed
   replaces an empty-block template with one that carries it; ops that no block can take yet (behind a nonce gap, or after an op below the packing floor) do not count.
7. Once a solution is found, write the final payload and sign the settlement transactions (§7.2), run the same checks on the settlement transactions as at reception (including `testmempoolaccept`); on failure discard the block and evict
   the offending ops; on success broadcast them together with the block. At this point they are broadcast only to the fast-lane network, not to BTC (see §7.2 for delayed broadcast).

### 6.3 Block validity rules

Header rules (depend only on the header and the DAG; a violation makes the block invalid, together with its descendants):

- GHOSTDAG (§6.6): 1 to `MAX_BLOCK_PARENTS` parents, pairwise distinct, none an ancestor of another; `parents[0]` is the selected parent computed by `(chain_work, hash)` (§6.6), the rest
  in strictly ascending byte order and not equal to `parents[0]`; the merge set (including the selected parent) does not exceed `MAX_MERGESET_SIZE`; for every block in the merge set other than the selected parent,
  `blue_score + MERGE_DEPTH ≥ blue_score(selected parent)`, and its selected chain passes through the **merge-depth root** `root(B)`: the highest block on the selected parent's selected chain
  (the selected parent included) whose blue score is at most `blue_score(selected parent) − MERGE_DEPTH`, the genesis block when there is none (D-097).
- `height = parents[0].height + 1`; `version` equals the protocol version at this block's DAA score (§6.9).
- `bits` is bit-for-bit identical to the canonical value computed by the difficulty adjustment (§6.7); the PoW meets the effective target for this block's weight (§6.6).
- The height of `btc_anchor` is not lower than the anchor height of any parent.
- The timestamp does not exceed the header time of the anchored BTC block plus `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` (§6.8, D-083).

Body rules (depend on the entries and on the BTC data of the anchor chain):

- The Merkle root of the entries equals `entries_root`. A mismatch only means this copy is wrong and does not mark the block hash invalid (the block hash does not cover the entries or the settlement transactions, D-041).
- State-independent entry rules (by the rules at the block's own DAA score, §6.9; block invalid): the entry order is forced → unlock → path B; at most one unlock entry per block;
  total gas (§8.3); for path B entries, the VmOp version, the chain ID, an encoded length not exceeding `MAX_OP_BYTES`, fee mode `Token`, and a valid signature.
- The body fits in a payload (D-100; block invalid): when the block needs a body (it has entries other than forced inclusions, §6.4), a republish payload (§7.1) carrying only its header and its entries, with a 32-byte payee
  and every key of the entries written in full (none taken from the payload's key dictionary), is no longer than `MAX_BLOCK_DATA_BYTES`. That is the bound every node measures a body by when it lands someone else's (§7.5,
  §7.6), so anyone can land its data on BTC. A block with only forced-inclusion entries needs no body and is not bound by it (§7.4).
- Forced-inclusion entries must be complete and in the correct order (§7.4; block invalid). In anchor-invalid mode (§6.8) the window cannot be derived from the anchor chain, and the entries are checked against the current BTC best
  chain instead (D-100): in a block that needs a body, each forced-inclusion entry must name a native transaction on the current best chain that carries the same op, confirmed above the block's window start `h(parent anchor)`
  and at or below `h(tip − ANCHOR_DEPTH)`, the window end of a block anchored now -- one carrying another op makes the block invalid, a transaction the current best chain does not have as a native transaction in that range
  only rejects this copy (it may gain the confirmations, or be confirmed again); a block with only forced-inclusion entries must have exactly the entries derived from the current best chain over its window (block
  invalid). So the indexer can determine the body of every block the nodes accept (§10.3), and the verdict does not depend on how much a node has pruned (its parents anchor at or above the pruning point's anchor, D-097).
- When settlement transactions are carried, they must pass all the checks of §7.2 (every header of the payload anchors at or below this block's anchor height, D-100), and the other bodies carried by the payload must
  belong to blocks in this block's past that are `root(B)` or in its future (D-097) and equal all the entries of that block; a mismatch only rejects this copy.
- When the settlement backlog limit (§6.4) is triggered, this block may contain only forced-inclusion entries, otherwise it is disqualified.
- Execution (§9.6): a main-chain candidate block executes its own entries in strict mode; if any entry fails a state-dependent rule (nonce, balance, unlock conditions) or the `state_root` after re-execution
  does not match, the block is **disqualified** from the main chain (and so is every block whose selected chain passes through it), but it remains a member of the DAG and can be merged by other blocks.

Reception rules (checked only against the local view when receiving a new block, not when syncing historical blocks; a violation only rejects this copy and does not penalize the peer that relayed it):

- The timestamp is greater than the median of the timestamps of the previous 11 blocks on the selected chain (from `parents[0]` along selected parents) (index `len / 2` after sorting), and does not exceed local time by more than 15 seconds.
- `btc_anchor` is on the local BTC best chain, with height not exceeding `max(parent anchor height, local tip − ANCHOR_DEPTH)` (otherwise local BTC is considered behind, retry later),
  and not more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the local tip. The height bound also applies when accepting historical blocks from peers or from BTC (D-101): a BTC reorg of at most
  `ANCHOR_DEPTH` blocks must not be able to orphan the anchor of a block that may still join the main chain (§6.4, §6.8).
- When settlement transactions are carried, all inputs of the commit transaction are unspent in bitcoind's UTXO set, and the two transactions pass `testmempoolaccept` as one package.

Classification of outcomes: **invalid block** (the rules above that mark a block invalid; also a wrong header version), **disqualified** (strict-mode failure, `state_root` mismatch, backlog limit),
**rejected** (reception rules, the anchor-invalid mode threshold, a forced-inclusion entry of an anchor-invalid block naming a transaction the current BTC best chain lacks, local BTC behind), **local fault** (storage, bitcoind, data-source failure during execution: retry later, never judge the block invalid).
A block whose header version is higher than the highest version this software knows is not judged invalid and the peer is not penalized; it is only rejected with an alert "node software needs upgrading" (one alert per version, §6.9).

### 6.4 Fork choice

- **Main chain (virtual selected chain)**: the tip of the local DAG with the largest `(chain_work, hash)` (§6.6: its blue work plus its own work, D-100) and no block ineligible for the main chain on its selected chain; the path from it along selected parents to the genesis block
  is the main chain; when a tip's selected chain contains ineligible blocks, the candidate is the selected parent of the ineligible block among them closest to the main chain. A block is ineligible when it is
  disqualified, or when the finality determination would exclude it under the local BTC best chain: its anchor, or the anchor of a block it merges, is off that chain (§6.5 item 2, D-101), so neither it nor
  any block built on it could become BTC-final. Nodes on the same BTC chain judge alike. The main chain itself has no ineligible block above the finalized tip (D-104): when a BTC reorg (of at least
  `ANCHOR_DEPTH` + 1 blocks) takes the anchor of a main-chain block above the finalized tip, or of a block it merges, off the local BTC best chain, the main chain first retreats to the selected parent of the
  lowest such block, whatever the work and not subject to `REORG_WINDOW`, then switches as usual; when BTC reorganizes back, those blocks are candidates again. An anchor above the local BTC tip is not off
  the chain. So, `REORG_WINDOW` aside, the main chain depends only on the DAG, the local BTC best chain, the finalized tip and the disqualified blocks, not on the order in which a node learned them.
  Switch only when the candidate is strictly better than the current tip;
  with equal chain work the larger hash wins, and there is no "first received wins". The main chain must pass through the finalized blocks (§6.5).
- **Reorg**: when the main chain changes (the new tip's selected chain diverges from the old main chain at some block), the state is rolled back to the fork point, then each block along the new selected chain is fully verified and executed; if the new chain contains a disqualified block,
  the old main chain is restored, that block is recorded as disqualified (in memory; re-evaluated after restart) and the next-best tip is chosen instead. Reorged blocks are handled as "syncing historical blocks" (reception rules are not checked). The depth of a work-based reorg
  does not exceed `REORG_WINDOW`; beyond that the switch is refused with an alert (a forced switch to the finalized chain is not subject to this limit, §6.5); when the depth exceeds `SETTLEMENT_DEPTH`, a highest-severity
  alert is logged (already-broadcast settlement transactions may be abandoned). For blocks removed from the main chain (also by a retreat after a BTC reorg): their settlement records are no longer broadcast, the path B ops in them that are still valid are returned to the entry pool, and
  the funding inputs of their transactions stay reserved while those are in the BTC mempool (§7.3).
- **Settled closure** (D-038, D-063, D-077): the **data landing height** `data(b)` of block b: for a block that needs a body (one containing entries other than forced-inclusion entries), it is the confirmation height, on the current BTC best chain,
  of the first KSN1 payload (§7.5) that carries its header together with the same body; for an empty block or a block containing only forced-inclusion entries, it is the earliest height at which its header appears in any KSN1 payload.
  The settled prefix at main-chain height h is `prefix(h) = max(data of each accepted block)`, the maximum over main-chain blocks 1 to h and the blocks they merge (the main-chain blocks
  together with the blocks they merge cover exactly the whole past of the tip, equivalently `prefix(b) = max(data(b), max prefix(each parent))`); it is undefined when any block has no data.
  A block is settled at BTC height H if and only if `prefix ≤ H`. It determines the unlocking of sponsored fees and rewards (§8.5), and it is also the prefix the indexer can process (§10.3).
- **Settlement backlog limit** (D-046, D-063, D-077): for a new block N with anchor height A, let B be a block in N's past (the main-chain blocks and the blocks they merge, plus the blocks N itself merges)
  that needs a body, whose data has still not landed on chain as of anchor-chain height A, and with `B.anchor.height + SETTLEMENT_TIMEOUT ≤ A`. When such a B exists, N itself may contain only forced-inclusion entries
  (otherwise N is disqualified), and the path B entries in the blocks N merges are all skipped; N may still carry settlement transactions, carrying the backlogged bodies and thereby clearing the backlog. It depends only on the anchor chain;
  it is not checked in anchor-invalid mode. It is not triggered under normal conditions; it only prevents the fast lane from continuing to accumulate ops on an unverifiable state when data never lands on BTC; when triggered, the node
  produces only such blocks and logs a highest-severity alert, recovers automatically once the backlogged data lands on chain, and the fast lane does not roll back.
- The node keeps rollback-capable state for the most recent `REORG_WINDOW` blocks (§10.2).

### 6.5 BTC-anchored finality

A finality rule determined only by BTC chain data (a checkpoint rule, D-082): all nodes and indexers decide by the same function, and the main chain must pass through the finalized blocks. To reorg a finalized block,
an attacker must reorg BTC rather than rely on fast-lane hashrate alone (item 4), and a block is reported BTC-final only once that takes a BTC reorg of at least `BTC_FINALITY_CONFS` blocks
(item 7, D-114); only a chain that lands its data on chain in time can become final, and a heavier chain that is slow to settle
is "finalized out" by a lighter chain that settles on time. Only a block whose execution is valid, and whose anchor and the anchors of the blocks it merges are on the BTC chain, becomes final (D-101): any
other block that lands first is excluded, not finalized.

1. **Header DAG**: the headers that have appeared on BTC (in settlement and republish payloads that count, §7.1) form a header DAG (shared by node and indexer), with the same GHOSTDAG rules as §6.6, additionally
   checking the header rules (the first group in §6.3), the anchor not lower than the parents' and the timestamp's anchor bound; a header whose parents have not yet appeared is held, and connected in order of appearance once the parents arrive; invalid ones are discarded;
   accepted headers are not deleted on a BTC reorg. The **connection height** of block B is `conn(B) = max(seen(B), max conn(parents))`, where `seen` is the earliest height at which the header appears
   on the current best chain (the genesis block is considered to have always been on chain).
2. **Determination per BTC height**: the finalized tip F starts at the genesis block. For each BTC height H (in order, incrementally): the candidates are the blocks with `conn ≤ H` whose selected chain passes through F
   and through no excluded block, and `best_H` is the one with the largest `(chain_work, hash)`; walk `best_H`'s selected chain block by block after F. Block C is first **excluded** at H, before anything else
   is asked about it, when the anchor of C or of a block C merges is not on the BTC best chain: the BTC block at that anchor height is another one (these anchors lie below the height at which C's header first
   appeared, §7.1, so below H). Otherwise, when, for C's closure (C and the blocks it merges, together with F's closure), the data landing height `landed(C)` (by `data` of §6.4) satisfies
   `landed(C) + BTC_FINALITY_CONFS − 1 ≤ H`, the verdict on C is asked (item 6): valid, C is final and F advances to C; invalid, C is **excluded** at H. An excluded C goes together with every block whose
   selected chain passes through it, `best_H` is chosen again among the other candidates and the walk goes on at H (a block that merges C without passing through it on its selected chain stays a candidate,
   D-101). When the landing condition does not hold, stop. At each step, record the BTC block (height and hash) at which the determination (or the exclusion) was made.
3. **Pause when it cannot be determined**: when the data landing status of some block cannot yet be determined (the node does not have the block and the data on BTC cannot rule out that its data has landed -- it is requested from peers and
   rebuilt from the data on BTC, see item 5 -- or its payload has not yet been parsed against it; or the indexer cannot yet determine its body), or when no verdict on C can be given yet (item 6), the determination
   stops at that height until it is determined, and when it resumes that height is determined again from the start (item 4); later BTC blocks do not change earlier determinations. A block the node does not have is answered from the data on BTC, with the same criterion as the indexer (§10.3 item 3): when the header DAG and §7.4 establish that it needs a body and the current BTC
   best chain has no payload carrying the same body, its data has **not yet landed**, and the determination does not pause -- someone who puts only the header on BTC and withholds the body cannot make the node's determination stop there
   (D-093). A block whose header appears only after `H − BTC_FINALITY_CONFS + 1` cannot be final at H anyway, so no pause is needed when its data status cannot be determined. Nor is the data status of a
   block anchored off the BTC chain ever asked (item 2 excludes it first): its body could never be determined if its anchor names no BTC block (§10.3 item 3), and waiting for it would be a pause for good.
4. **BTC reorg**: determinations, exclusions and connections made at reorged-out BTC blocks are revoked and redone along the new branch, those made at a height where the determination paused
   included (D-114); the finalized tip may therefore move back or even switch to another chain. Fast-lane finality is bounded by BTC's own finality. The determination of C at H rests on two things:
   the data of C's closure, `BTC_FINALITY_CONFS` blocks deep at H, and no heavier candidate having connected by H, which is only as deep as the BTC block at H. A reorg replacing the BTC block at H
   whose new branch carries, at or below H, the headers of a heavier chain forking below C therefore takes C off the finalized chain: one BTC block right after the determination, one more for each
   BTC block since. Without such a chain the determination redone is the same unless the reorg removes C's data, at least `BTC_FINALITY_CONFS` blocks right after the determination; this is why a
   block is reported BTC-final only later (item 7, D-114). A reorg of more than `ANCHOR_DEPTH` blocks also takes the
   main-chain blocks anchored on the BTC blocks it orphans off the main chain (§6.4, D-104); one of at most `BTC_FINALITY_CONFS` blocks never reaches the anchor of a final block, whose data landed above
   its anchor and then gained `BTC_FINALITY_CONFS` confirmations, so this retreat never takes back a final block. After pruning (§10.2), a BTC reorg that would
   revoke the determination of the pruning root (deeper than the pruning margin) cannot be followed: the node stops for a resync and the indexer starts over from the genesis anchor (D-097).
5. **The main chain passes through the finalized blocks**: when choosing the main chain, tips that fork from the main chain below a finalized block do not take part; if this node chose another, heavier chain before the block became final, it switches to the best tip that passes through the finalized
   block (regardless of work, and not subject to `REORG_WINDOW`; every state version from the finalized tip on is kept (D-101), so the state at the fork point is gone only after a BTC reorg deeper than
   `BTC_FINALITY_CONFS` revoked determinations: then a highest-severity alert is logged, the state store is cleared and the old main chain is re-executed from the genesis block up to the fork point before attaching
   the new chain, with no new blocks processed in the meantime, D-093; a pruning node keeps every state version from the pruning root on, so it never needs that, D-097); when it lacks a finalized block, it requests it from peers and **rebuilds it from the data on BTC** (D-093): the header is taken from the header DAG,
   the body is determined by the indexer's rules (§10.3 item 3: a block that needs no body needs only its header, one that does takes it from the payload carrying it, and forced-inclusion entries are derived per §7.4), the rebuilt block
   carries no settlement transactions and is accepted after verification under all the rules for syncing historical blocks (missing parents of orphans are rebuilt the same way). So a node with no peers, or whose peers are all on another chain, can still follow
   the finalized chain on BTC. The block template merges only tips no heavier than the main-chain tip, so block production never leaves the finalized chain. The indexer takes the finalized tip as the lower bound of its main chain, and executes finalized blocks without waiting for
   `SETTLEMENT_DEPTH` confirmations (§10.3).
6. **Verdict** (D-101): the verdict on C is the result of executing C as a main-chain block whose selected parent is F, exactly as the indexer executes a main-chain block (§10.3 item 5): the body rules of each
   block C merges (in consensus order) and of C, the settlement backlog limit, the merged blocks in lenient mode and C in strict mode, and C's `state_root`. It is asked only at that point, when the data of C's
   closure has landed with `BTC_FINALITY_CONFS` confirmations and C's selected parent is F, so it depends only on the BTC chain up to C's anchor height: every node and indexer on the same BTC chain gets the same
   verdict, however early it learned that a block is invalid; a recorded verdict is used again only while the BTC block at C's anchor height is unchanged. It is never asked about a block anchored, or merging a
   block anchored, off the BTC best chain (anchor-invalid, §6.8): item 2 excludes such a block first. Its execution could be reproduced only on its anchor chain, which the BTC chain up to H does not contain;
   executed again on the current chain (D-100) it gives an answer that depends on the BTC tip, or none, and pausing for one stopped the determination for good when the anchor names no BTC block (D-101). So an
   honest block anchored on a BTC block that a reorg of `ANCHOR_DEPTH + 1` blocks or more orphaned is never final either. Nodes take such blocks from peers (anchor-invalid mode, §6.8), so the block
   template never merges one (§6.2) and the main-chain selection never takes one (§6.4); a block already on the main chain when a later reorg orphans its anchor leaves it (§6.4, D-104). Nodes and indexers keep
   the state of F to execute C on it (§10.2, §10.3); a node judges a block it lacks or refused with its body determined from the data on BTC (§10.3 item 3), excludes an invalid one without ever having it, and decides
   a valid one final once it has the block (item 5). A **finalized block that is invalid** can then only come from a non-determinism bug: the main chain cannot be switched; the indexer stops processing and alerts,
   and the node keeps its old main chain and alerts; manual handling is required.
7. **BTC-final, as reported** (D-114): nodes and indexers report a finalized block as BTC-final only once the BTC block of its determination has `BTC_FINALITY_CONFS` confirmations. The **BTC-final tip**
   is the highest finalized block whose determination was made at a BTC block on the best chain with `BTC_FINALITY_CONFS` confirmations, or the BTC-final tip reported before when it is higher and its
   determination still stands (made at the same BTC block, still on the best chain), so that a BTC chain shorter for a while during a reorg takes nothing back; the genesis block and the pruning root
   (§10.2) are always BTC-final. A block reported BTC-final stops being reported only after a BTC reorg replacing the BTC block of its determination, at least `BTC_FINALITY_CONFS` blocks; it leaves the
   finalized chain only when the new branch carries a heavier chain forking below it, or when the reorg removes its data, at least `2 × BTC_FINALITY_CONFS − 1` blocks (item 4). Nodes and indexers raise
   an alert when a block they reported is taken back (§16). `ksana_getChainInfo` and `indexer_status` report the BTC-final tip as `finalized`; `btc_final` of an op or a block means "the main-chain block
   that accepted it is on the finalized chain at or below the BTC-final tip" (§10.4). Reporting is local and waits only for what is reported: the finalized tip itself does not wait, and the floor of the
   main chain (item 5), the verdicts (item 6) and pruning (§10.2) follow it as before.

### 6.6 GHOSTDAG and work

Definitions (D-063, following Kaspa):

- `work(B)`: the work contributed by the header, `⌊2^256 / (effective target + 1)⌋` (see two-speed difficulty below).
- `selected_parent(B)`: the parent with the largest `(chain_work, hash)` (below), hashes compared byte by byte over 32 bytes.
- `mergeset(B) = past(B) \ past(SP)`, including SP itself; apart from SP it is ordered by ascending `(blue_work, hash)` (which is a topological order: `blue_work` strictly increases along every parent-child edge),
  and this order is the execution order and acceptance order of the merged blocks.
- Coloring (k-cluster greedy, K = `GHOSTDAG_K`): SP is blue; the rest are judged one by one in the order above, and candidate c is blue if and only if the merge set has fewer than K + 1 blue blocks so far,
  c's anticone has at most K blue blocks, and no blue block in c's anticone already has K blue blocks in its own anticone; otherwise it is red.
- `blue_score(B) = blue_score(SP) + |mergeset_blues|`, `blue_work(B) = blue_work(SP) + Σ work(mergeset_blues)`, both 0 for the genesis block.
- `chain_work(B) = blue_work(B) + work(B)`: the work of B's blue past and of B itself, the key of the fork choice (selected parent, main chain §6.4, finality candidate §6.5); it too strictly increases along every
  parent-child edge. Counting a block's own work (D-100): with `blue_work` alone a block's work counted only once a block was built on it, so on an idle chain, where the honest work comes in heavy empty blocks minutes apart, a
  miner could withhold more than `GHOSTDAG_K` cheap weight-1 blocks while an honest heavy block was being found and turn it red by publishing them, and a payment was protected by little more than its own block (§10.4, §16).
- `daa_score(B) = daa_score(SP) + Σ_{m ∈ mergeset(B) \ {SP}} w(m) + w(B)` (blue and red blocks both count, D-084), 0 for the genesis block; when all weights are 1 it equals
  `|past(B)|`. Difficulty adjustment (§6.7), the protocol version (§6.9) and block rewards (§8.6) all follow it.
- An ancestor query (`x ∈ past(y)`) walks y's selected chain checking each merge set, stopping when the chain block's `blue_work` is no greater than x's; the merge limits guarantee that the queries during block verification all stay nearby:
  the check of the merge-depth root (§6.3) walks a merged block's selected chain only until it meets the selected parent's chain above the root, or falls to the root's blue work, so a block's own computation never reaches below its root.
- **Pruning** (D-097): `min_pruning_depth = 3 × (MERGE_DEPTH + GHOSTDAG_K) + 1` (2 209). A DAG pruned at a point P on a block F's selected chain, at least this far below F in blue score, after everything in F's past
  was inserted, computes every block a later chain through F needs exactly as the unpruned DAG: such a block's chain passes through F or through the merge-depth root of a chain block above F (at most
  `MERGE_DEPTH + K` below F), and its computation reaches at most `2 × (MERGE_DEPTH + K)` further down. A computation that would need a pruned block is not carried out, which never judges the block invalid. Node and indexer
  keep only the pruning point and its future in memory, F being a BTC-final block (§10.2, §10.3).

Two-speed difficulty (D-084; empty-block difficulty increases with the streak, D-090. N = `EMPTY_BLOCK_DIFFICULTY`, 0 treated as 1; N_MAX = `MAX_EMPTY_BLOCK_WEIGHT`, values below N treated as N):

- **Empty block**: a block whose `B.entries_root` equals the root of the empty entry list (no entries). **Empty-block streak** `streak(B)`: 0 for a non-empty block, `streak(SP) + 1` for an empty block, counted as 0 for the genesis block.
  The streak propagates only along selected parents: merged blocks that are not the selected parent do not affect it.
- **Weight** `w(B) = 1` when B is non-empty; otherwise `w(B) = min(N × 2^(streak(B) − 1), N_MAX)`: the empty blocks after a non-empty block are N, 2N, 4N, … in turn, up to N_MAX
  (devnet: 12, 24, 48, 96, 120, 120…).
- **Effective target** = `target(bits) / w(B)` (integer division); the header's PoW is `pow_value(header) ≤ effective target`; when the effective target is 0 (`bits` too small) the header
  is invalid. Empty and non-empty blocks with the same parents have the same `bits` (§6.7), so an empty block has to spend about w times the work: the idle header interval doubles block by block from about N block intervals to
  N_MAX (devnet: from about 60 seconds to about 10 minutes); when there are transactions, the producer immediately switches to producing at weight 1 (§6.2), and the first empty block after that starts again from N.
- `work(B)` is computed from the effective target, and blue work, chain work and main-chain selection all follow it. The genesis block's work is likewise computed from the effective target (streak 1, weight N) (it is never in any merge set and does not affect consensus).
- Block rewards are paid as w slots (§8.6), so empty and non-empty blocks have the same expected reward per unit time, which encourages neither churning out empty blocks nor deliberately breaking the streak.

Numeric conventions of the PoW (D-025, all following Bitcoin):

- The mining algorithm is Argon2id (RFC 9106, version 0x13, D-094): `pow_value(header)` is the output of
  `Argon2id(message = borsh(header) (including nonce), salt = "ksana-pow" ‖ CHAIN_ID (4 bytes little-endian), memory POW_ARGON2_MEMORY, passes POW_ARGON2_ITERATIONS, parallelism POW_ARGON2_LANES, no key or associated data, 32-byte output)`
  interpreted as a 256-bit unsigned integer in **little-endian** (the same byte-order convention as the block hash: equivalent to treating the hex displayed in reverse as a big-endian number). The block hash (the block identifier)
  is still `sha256d(borsh(header))` and does not take part in the PoW comparison. The parameters are fixed per network (§13; devnet and signet: 2 MiB, 1 pass, 1 lane, single-threaded on an ordinary CPU
  about 1 millisecond per header); changing the algorithm or the parameters is a consensus change; the block hash, `bits`, work and ASERT are all defined in terms of the "target" and do not depend on the algorithm.
  Implementation: `Argon2id` in `ksana-consensus` (RustCrypto `argon2`) and `pow.ts` in the TS SDK (`@noble/hashes`), checked byte for byte against the shared vectors
  `tests/vectors/pow.json`. A computation failure (for example out of memory) is a local fault and does not judge the block invalid.
- `bits` is Bitcoin's compact encoding: exponent = `bits >> 24`, mantissa = `bits & 0x007f_ffff`, target = mantissa × 256^(exponent − 3). A set sign bit, a target of 0,
  a target ≥ 2^256, or a target above the PoW limit are all invalid. Converting a target to compact uses the canonical form of Bitcoin's `GetCompact` (take the top 3 significant bytes; when the mantissa's high bit is 1,
  shift right one byte and add 1 to the exponent), and a block's `bits` must be bit-for-bit identical to the value computed by the difficulty adjustment.
- The PoW limit (minimum difficulty) is `pow_limit_bits` (devnet `0x207f_ffff`, about 2^255, letting devnet produce blocks every 5 seconds on an ordinary CPU), which is also the genesis block's default `bits`.

### 6.7 Difficulty adjustment (ASERT)

Adjusted every block, an integer implementation of aserti3-2d anchored at the genesis block (D-026): anchor target = the target of the genesis block's `bits`, "anchor parent time" = genesis timestamp − T.
The `bits` of block B are determined by its selected parent SP and B's DAA score (D-063, D-084):

```text
T = TARGET_BLOCK_TIME, τ = ASERT_HALF_LIFE
time_diff   = SP.timestamp − anchor parent time
height_diff = daa_score(B) − w(B)          (= SP's DAA score + the weights of the other blocks in the mergeset)
exponent = trunc((time_diff − T·(height_diff + 1)) · 65536 / τ)       truncated toward zero; |exponent| clamped to 2^40 (does not change the result)
shifts   = floor(exponent / 65536)
frac     = exponent − shifts · 65536                                   ∈ [0, 65535]
factor   = 65536 + ((195766423245049·frac + 971821376·frac² + 5127·frac³ + 2^47) >> 48)
next     = floor(anchor target · factor · 2^(shifts − 16)), 1 if it is 0, the PoW limit if it exceeds the PoW limit
bits(B)  = compact(next)
```

Computed exactly with sufficiently wide integers (bit-for-bit identical to Bitcoin Cash Node's `CalculateASERT` under the reference implementation's premise PoW limit < 2^224; the official vectors
`run01`–`run12` all pass). `height_diff` excludes this block's weight, so empty and non-empty blocks with the same parents have the same `bits`; an empty block's weight w makes the schedule advance w intervals per block,
so when only empty blocks are produced (N_MAX·T per block once the streak reaches the cap) or production is mixed, the time per unit of DAA score still converges to T. The half-life is chosen by the cap on empty-block weight (devnet 7 200 seconds = 12 × N_MAX·T,
D-090): at the cap, the ratio of single-block time to half-life is unchanged, and idle difficulty jitter is about ±20%; the cost is a correspondingly slower response to hashrate changes (a 10× hashrate step converges in about 10 000 blocks).

### 6.8 Timestamps and BTC anchoring

Timestamps:

- Consensus rule (D-083): `header.timestamp ≤ time(the header at height btc_anchor.height on the current BTC best chain) + MAX_TIMESTAMP_AHEAD_OF_ANCHOR`
  (14 400 seconds). When the anchor block is on the best chain, that is the anchor block itself (the anchor's hash commits to it); when the anchor block has been reorged out of BTC, the block at the same height on the current chain answers (consistent with anchor-invalid
  mode). The time is taken only from the BTC tracker's snapshot, so this rule is a deterministic function of BTC chain data, and node and indexer reach the same conclusion for the same BTC chain. It is checked when a node receives a new block,
  syncs historical blocks, when a producer submits its own block, and in the header DAG (§6.5); a block that fails is recorded as invalid. It is not checked when the local BTC snapshot does not yet have the anchor height (local BTC behind the anchor):
  such a block's forced inclusion window cannot be derived anyway, and the anchor check handles it. The reason for 4 hours: BTC block intervals have a long tail, and a BTC header time is allowed to be about
  2 hours behind real time, so an honest producer's timestamp normally does not exceed the anchor time by more than 3 hours; the smaller the bound, the more dangerous for honest nodes, and the larger, the more difficulty discount an attacker can borrow, and it cannot accumulate across anchors.
  Consequence: when the BTC chain stalls for longer than this bound, no timestamp can satisfy both the median rule and the bound, and the fast lane pauses block production until a new BTC block appears.
- Reception rule: greater than the median of the timestamps of the previous `MEDIAN_TIME_SPAN` (11) blocks on the selected chain (near the genesis block, when there are fewer than 11 ancestors, all of them are used), and not more than
  `MAX_FUTURE_BLOCK_TIME` (15 seconds) ahead of local time; "ahead of local time" does not mean the block is permanently invalid.
- The producer uses `min(max(local time, median + 1), anchor bound)`: when the local clock is ahead or BTC has had no new block for a long time, it is clamped to the bound; when the bound is already no higher than the median, no block is produced,
  waiting for BTC to advance (after BTC has stalled for more than about 4 hours, the fast lane pauses with it instead of producing a block that everyone else rejects).

BTC anchoring (D-035, D-056):

- Consensus rule: the height of `btc_anchor` is not lower than the anchor height of any parent.
- The producer anchors to the block at `max(parent anchor height, local BTC tip − ANCHOR_DEPTH)` (on the chain fully scanned locally). The deeper the anchored block, the less likely it is to be reorged out of BTC;
  the cost is that the effective number of confirmations for forced inclusion is `ANCHOR_DEPTH + NATIVE_CONFIRMATIONS`.
- Reception rules: see §6.3. An anchor block higher than the locally synced tip means the local state is behind (retry later) and does not make the block invalid.
- **The execution layer's BTC view** (`BtcView`): contracts reading headers, unlock entries querying the settled closure, and path C ops reading native transactions all get answers relative to this block's `btc_anchor`, depending only on
  the part of the anchor chain at heights not exceeding the anchor height, so the results are unchanged when syncing historical blocks; a data-source failure returns an error, which the execution layer treats as a local fault, never judging the block invalid on that basis.
  On the anchor chain the view also answers that a forced-inclusion entry's transaction ID names no native transaction carrying the entry's op: that is not a data-source failure, and the entry stands for no path C op,
  which makes the block invalid, and a block merging it too (§7.4, §9.6, D-100).
- **Anchor-invalid mode** (historical blocks: P2P sync responses, orphans connected along with their parents, side chains connected during a reorg, crash recovery; and the indexer): a block whose anchor block is not on the current best chain but whose height does not exceed
  the local tip is processed as usual, except for the rules that can only be reproduced on the anchor chain: the forced-inclusion entries are checked against the current best chain instead of the window (§6.3, D-100), the settlement backlog limit is
  not checked, and the BTC view answers from the current best chain instead (headers are taken from the current chain by height; native transactions and the settled closure are looked up on the current chain by transaction ID / block hash,
  at any height; not finding them is a local fault). Contracts can only read headers below anchor height −
  `BTC_QUERY_DEPTH`, and only a BTC reorg deeper than that can give a wrong answer, so a `state_root` mismatch when re-executing such a block is treated only as a local fault, and so is a verdict of its execution that the
  block is invalid (D-100). A node enters this mode only when
  bitcoind knows the anchor block (same height, not on the best chain, i.e. a genuinely reorged-out block) or the anchor is more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the local tip (the anchor's hash is then not
  looked at), and otherwise rejects the block (preventing a forged anchor hash from bypassing the checks for new blocks); the anchor height is bound by `max(parent anchor height, local tip − ANCHOR_DEPTH)` as for a new
  block (§6.3, D-101); the indexer processes only the main chain selected by work whose data is already on BTC, with no additional threshold. A block taken in this mode stays a side block: the finality
  determination excludes it and every block merging it or built on it (§6.5 item 2), so no template merges it and the main-chain selection never takes it (§6.2, §6.4, D-101; the second review of D-101 showed
  that one such block handed over through sync was enough to stop finality otherwise).
  This threshold alone does not stop a forged anchor (audit of 2026-10-06): a new block may name an old parent, whose anchor lags enough, and a made-up anchor hash at that height. What keeps such a block from stopping block production or
  the indexer is the forced-inclusion rule of §6.3 (D-100): the nodes accept it only when its forced-inclusion entries name native transactions of the current best chain, so the indexer can determine its body, and a merged block's entry that
  names no native transaction of the merging block's anchor chain makes the merging block invalid (§9.6), so a block merging it finds the same transactions when it is first executed and when it is executed again in this mode;
  unlock entries of merged blocks are skipped (§9.6). Producers also leave a block whose merge would fail out of their templates (D-098). A BTC reorg of `ANCHOR_DEPTH + 1` blocks or more after a block was
  accepted that drops or moves what its execution relied on orphans its anchor, since everything the execution reads lies at or below the anchor height: the block then leaves the main chain (§6.4, D-104)
  instead of being executed again (before D-104 a node or indexer executing it again stopped on a local fault).
- An accepted fast-lane block is not invalidated by a later BTC reorg. A reorg of at most `ANCHOR_DEPTH` blocks orphans no anchor and leaves the fast lane unchanged; a deeper one takes the blocks anchored on
  the BTC blocks it orphaned, and the blocks built on them, off the main chain while that BTC chain holds (§6.4, D-104). A native transaction the reorg moves was included only by such blocks (they anchor at
  or above its old height, above the fork point), so after the retreat the window of §7.4 starts below its new height and it is included again, in the new branch's order; one the new branch no longer has
  is gone, as on BTC, when double-spent, and included once it confirms again when it was only dropped from the mempool.

### 6.9 Protocol versions and upgrades

Protocol version = block header version, activated by DAA score (D-076). Each network's parameters carry an upgrade schedule:

```rust
pub struct Upgrade {
    pub version: u32,                  // the new protocol version, greater than the base version 2 and strictly increasing
    pub daa_score: u64,                // activation point: blocks whose DAA score is not below it use the new version; ≥ 1 and strictly increasing
    pub block_gas_limit: Option<u64>,  // the following are the rules that change with the version; None means unchanged
    pub native_max_gas: Option<u64>,
    pub min_vmop_version: Option<u8>,
    pub max_vmop_version: Option<u8>,
    pub block_share_bps: Option<u32>,  // ratio and end point of the block share (D-089)
    pub block_share_end: Option<u64>,
    pub carrier_bps: Option<u32>,      // the two ratios of the carrier reward (D-095)
    pub carrier_fee_bps: Option<u32>,
}

pub struct Rules {                     // the rules at DAA score d: the base values with every upgrade whose daa_score ≤ d applied in order
    pub version: u32,                  // the block header's version must equal it
    pub block_gas_limit: u64,          // BLOCK_GAS_LIMIT
    pub native_max_gas: u64,           // NATIVE_MAX_GAS (must not exceed block_gas_limit)
    pub min_vmop_version: u8,          // accepted VmOp version range (inclusive), base value 2 to 2
    pub max_vmop_version: u8,
    pub block_share_bps: u32,          // BLOCK_SHARE_BPS: the block share's portion of each reward slot in basis points (must not exceed 10 000, §8.6)
    pub block_share_end: u64,          // BLOCK_SHARE_END: only reward slots whose DAA score is below it have a block share
    pub carrier_bps: u32,              // CARRIER_BPS: the carrier's portion of each reward slot in basis points (its sum with block_share_bps must not exceed 10 000, §8.6, D-095)
    pub carrier_fee_bps: u32,          // CARRIER_FEE_BPS: the carrier's portion of path B sponsored fees in basis points (must not exceed 10 000)
}
```

- Block B's protocol version = the version of the last upgrade whose DAA score does not exceed `daa_score(B)` (including this block's own weight), or 2 when there is none; the header's `version` must equal it exactly:
  the old version after activation and the new version before activation are both invalid. devnet and signet currently have no scheduled upgrades.
- Entry rules that do not depend on state (§6.3) are checked under the block's own rules, merged blocks under their own rules too; execution (state transition) follows the main-chain block's rules -- a merged block's entries execute in the context of the
  main-chain block that merges it, and the whole main-chain block uses a single set of rules. Entry pool admission follows the rules of the next block after the tip. An upgrade that only changes values in `Rules` only needs an item added to the parameters;
  other changes (new execution rules, encodings, VM features) branch on `rules.version` in the code and activate with the same upgrade item.
- When a node or indexer that has not upgraded receives a block whose version is higher than the highest it knows, it does not judge it invalid and does not penalize the peer; it only refuses it and warns (once per version), and the indexer stops before
  the fork and warns; after the software is upgraded and restarted, it syncs as usual.
- Upgrade procedure: implement the rule changes under the new version number (the old version's behavior is unchanged, the determinism test's golden roots are unchanged); add an item to the upgrade schedule in each network's parameters (the DAA score leaves time for release and
  deployment); keep the shared vectors and the TS SDK in step; run the node and indexer tests across the activation point once with test parameters; all nodes and indexers finish upgrading before activation. When a state migration is
  needed, it is done before the first main-chain block of that version executes and is defined with that upgrade.

## 7. Settlement transactions and forced inclusion

For blocks that need a body (blocks containing entries other than forced-inclusion entries), the header and the entries must be written to BTC; empty blocks and blocks containing only forced-inclusion entries only need the header on-chain (their entries can be derived from
BTC). The data is carried by the producer's settlement transaction, by the settlement transaction of some later block, or by a republish transaction sent by anyone (§7.5).

### 7.1 Data format

Two data carriers, both identified by the 4-byte magic `KSN1`:

- **KSN1 output** (carries only path C's `NativeOp`, D-032): the locking script starts with `OP_RETURN` immediately followed by a push whose data is exactly the magic (the push's encoding is not restricted);
  the rest may only be data pushes (`OP_PUSHBYTES_0..75`, `OP_PUSHDATA1/2/4`, non-minimal encodings accepted), each at most 520 bytes; any other opcode
  (including `OP_1`–`OP_16`, `OP_1NEGATE`) or a truncated push is a format error. The payload is the data chunks concatenated in order. When constructing, the push opcode is chosen by length, 520 bytes per chunk.
- **KSN1 envelope** (carries only `Settlement` / `Headers`, D-081): the leaf script of a Taproot script-path spend
  `<32-byte x-only public key> OP_CHECKSIG OP_FALSE OP_IF <magic> <data chunk>… OP_ENDIF`, each data chunk at most 520 bytes. Recognition looks only at the syntax: when an input's witness, after the annex is removed per BIP341
  (at least 2 elements and the last one starts with `0x50`), still has at least 2 elements, the second to last is the leaf script; a leaf starting with `OP_PUSHBYTES_32 <32 bytes> OP_CHECKSIG
  OP_FALSE OP_IF` immediately followed by a push whose data is exactly the magic carries a KSN1 envelope; after that only data pushes may follow (same rules as above), and the format is correct only when the last element is exactly `OP_ENDIF` and the script
  ends there. Each witness byte counts as only 1 weight unit (0.25 vB); in an OP_RETURN it is 1 vB.
- The payload is taken only when a transaction's KSN1 outputs and inputs carrying a KSN1 envelope **together number exactly one** and that one is well-formed; the payload's length is checked against `MAX_BLOCK_DATA_BYTES` before it is fully
  decoded (complete consumption required), and the data carrier must match the payload kind; otherwise it is not recognized (neither a native transaction nor a settlement / republish transaction).
- A settlement or republish payload confirmed at BTC height h **counts** only when every header it carries anchors below h (`btc_anchor.height < h`, D-100); one that does not count is ignored as a whole: none of its headers has appeared on BTC
  through it (§6.5), it lands no body (§6.4) and its sender carries nothing (§8.6). An honest payload always counts: a block anchors to a BTC block already on the chain when it is produced, and a payload carrying it is broadcast later, so it confirms higher up. So every header in the header DAG anchors to
  a BTC block already scanned when the header appears, its timestamp bound (§6.8) is checked right then, and node and indexer reach the same conclusion whatever order they learn the blocks in.
- bitcoind is started with `-txindex=1 -datacarriersize=100000` (regtest additionally with `-acceptnonstdtxn=0`); from Bitcoin Core 30 on, large OP_RETURN
  data outputs are relayed by default; earlier versions by default relay only those of at most 83 bytes, which affects path C transactions.

```rust
pub enum Payload {
    Settlement {                       // variant 0: settlement payload
        headers: Vec<BlockHeader>,     // at most MAX_SETTLEMENT_HEADERS; the last is this block's header, the rest are headers not yet on-chain that the producer chose to carry
        entries: Vec<EntryData>,       // all entries of this block; Forced entries write only btc_txid
        bodies: Vec<Body>,             // bodies of other blocks being carried (D-077); may not point at the last header
    },
    NativeOp { op: VmOp },             // variant 1: path C, plain Borsh encoding (with chain ID)
    Headers {                          // variant 2: header and body republishing; anyone may send it
        headers: Vec<BlockHeader>,     // any headers (self-certified by PoW), at most MAX_SETTLEMENT_HEADERS
        bodies: Vec<Body>,             // bodies of some of those blocks (self-certified by entries_root)
        payee: [u8; 32],               // payee (D-095): takes a block's carrier reward (§8.6) when the republish lands that block's data on-chain for the first time and the data has waited more than CARRIER_REPOST_DELAY BTC blocks; all zeros means not collecting
    },
}

pub struct Body { pub header: u32, pub entries: Vec<EntryData> }   // header is the index of the header in the same payload

pub enum EntryData {                   // the payload form of Entry; variant indices are the same as Entry's
    Forced { btc_txid: [u8; 32] },
    Sponsored { op: VmOp },
    Unlock { height: u64, index: u32 },
}
```

The payload's wire encoding (`Payload::encode(params)` / `decode(bytes, params)`) is not plain Borsh: `Settlement` = variant 0, the compact header sequence, the body
list, this block's entry list (**bodies first**, unlike the field order of the type); `Headers` = variant 2, the compact header sequence, the body list, the 32-byte payee (D-095); `NativeOp` = variant 1 plus
the Borsh encoding of the VmOp. The headers and entries themselves, block hashes, signatures, `op_id` and `entries_root` are all unchanged and are restored byte for byte after decoding.

**Compact header sequence** (D-075, D-079): a `u32` count (little-endian), then each header encoded relative to the headers before it in the sequence:

| Field | Encoding |
| --- | --- |
| `flags: u8` | bit 0 version same as the previous, bit 1 BTC anchor same as the previous, bit 2 `bits` same as the previous, bit 3 `entries_root` is the root of the empty entry list, bit 4 `state_root` same as the previous; the other bits are 0 |
| Version | `u32` when bit 0 is not set |
| Height | the first as a varint, then the difference from the previous (interpreted as `i64` modulo 2^64) as a zigzag varint |
| Parents | varint count, each parent as a varint `r`: `r ≥ 1` refers to the hash of the `r`-th header before it in the sequence, `r = 0` is followed by a 32-byte hash |
| BTC anchor | `u32` height + 32-byte hash when bit 1 is not set |
| `entries_root` | 32 bytes when bit 3 is not set |
| `state_root` | 32 bytes when bit 4 is not set |
| Producer | varint `r`: `r ≥ 1` is the producer of the `r`-th header before it, `r = 0` is followed by 32 bytes |
| Timestamp | same as height |
| `bits` | `u32` when bit 2 is not set |
| `nonce` | `u64` |

varint is LEB128 (unsigned, minimal encoding). The encoding is canonical, checked when decoding: a field equal to the previous one must have its bit set, the first header does not set bits 0 to 2 and 4, the empty root must use
bit 3, a parent that appeared in the sequence must be referenced (taking the nearest one), the producer likewise references the nearest identical producer, and varints must be minimal; the parent count is limited to 255 in encoding and decoding.
In a run of empty blocks only the first header writes out the state root (empty blocks do not change the state root, §8.5); each one after it is 14 bytes.

**Compact entries** (D-080): the bodies and this block's entries in the same payload share one dictionary of 32-byte keys (accounts, contract IDs, asset IDs, recipients), ordered by first occurrence,
preloaded with KSA's asset ID (the 1st):

| Element | Encoding |
| --- | --- |
| Entry | `u8` variant (0 forced, 1 path B, 2 unlock), followed by that variant's fields |
| Forced | 32-byte BTC transaction ID |
| Unlock | varint height, varint acceptance index |
| Path B | VmOp: `version: u8`, key `account`, varint `nonce`, fee, varint `max_gas`, action, signature; `chain_id` is not written and is taken from the network parameters when decoding |
| Fee | `u8` variant: 0 `Token` followed by a varint amount; 1 `BtcNative` |
| Action | `u8` variant: 0 `Deploy` (byte string `code`, byte string `init_args`, asset list); 1 `Call` (key `contract`, string `method`, byte string `args`, asset list); 2 `AssetTransfer` (key `asset`, key `to`, varint `amount`) |
| Asset list | varint count, each item a key `asset` and a varint `amount` |
| Signature | `u8` variant: 0 `Bip340` followed by 64 bytes; 1 `Bip322Simple` followed by a byte string |
| Key | varint `r`: `r ≥ 1` is the `r`-th key in the dictionary; `r = 0` is followed by 32 bytes, which are appended to the end of the dictionary |
| Byte string, string | varint length and the content; a string must be valid UTF-8 |
| Entry list | varint count and the entries |
| Body list | varint count, each body a varint header index and an entry list |

All integers are LEB128 varints (unsigned, minimal encoding, not exceeding the field's type; `u128` at most 19 bytes). The encoding is canonical: a key already in the dictionary must be referenced, a reference may not be out of range,
and varints must be minimal, otherwise decoding fails. When encoding, a VmOp whose chain ID differs from the network's is an error. A path B KSA transfer (BIP340 signature) is about 143 bytes when both keys occur for the first time,
about 79 bytes when both are already in the dictionary (Borsh is 217 bytes); a forced-inclusion entry is 33 bytes.

**Canonical form of bodies** (D-077, checked when decoding): `header` strictly increasing and less than the header count, and in `Settlement` it may not point at the last header (this block's entries are in
`entries`); each body has at least one entry that is not `Forced`. The whole payload does not exceed `MAX_BLOCK_DATA_BYTES`, with at most `MAX_SETTLEMENT_HEADERS` headers (checked on the count before any header is decoded, D-100; a payload with
more is not recognized).

### 7.2 Settlement transaction structure

The settlement a block carries consists of two transactions, a commit transaction and a reveal transaction (D-081), with the payload in the reveal transaction's witness. With a single transaction the payload could only go in a witness stack element not covered by the signature,
which any relayer could replace without changing the transaction ID, so the payload must go in a leaf script committed to by an output, and the commit transaction first creates the commitment output. Both are v2 with `nLockTime = 0`,
and do not use TRUC (v3).

Commit transaction:

| Position | Content |
| --- | --- |
| Inputs | at least one producer input: P2TR key-path (witness is a 64-byte signature, or 65 bytes with last byte `0x01`) or P2WPKH (`[strict DER low-S signature ‖ 0x01, 33-byte compressed public key]`), empty scriptSig, the signature covers all inputs and outputs and is verified against all prevouts; the prevouts are confirmed in blocks at height ≤ `anchor.height` on the anchor chain; all inputs are distinct |
| Output 0 | producer change: P2PKH, P2SH, P2WPKH, P2WSH or P2TR, not below that script's dust limit, not a KSN1 output |
| Output 1 | commitment output: a P2TR output containing only the envelope leaf, the internal key is the public key in the leaf, the control block is 33 bytes; amount = the reveal transaction's fee + 240 sat |

Reveal transaction:

| Position | Content |
| --- | --- |
| Input | exactly one: output 1 of the commit transaction, empty scriptSig, the witness is exactly `[signature, envelope leaf, control block]`, the signature is `SIGHASH_DEFAULT` or `SIGHASH_ALL`, verified by the script path |
| Output 0 | a 240 sat P2A anchor output (`OP_1 <0x4e73>`, Bitcoin Core 28's dust limit): anyone can use it for CPFP, accelerating both transactions at once |
| Output 1 | an `OP_RETURN` with amount 0 and no data: with only one input and the P2A output, the non-witness part is 64 bytes, below Bitcoin Core's minimum of 65 bytes |

Validation rules (the settlement transactions a block carries, D-034, D-081):

- The serialized length and the weight of both transactions do not exceed 400 000 (BTC's standard transaction limit); when decoding, the length is checked first and complete consumption is required.
- Every input of both transactions has `nSequence = 0xFFFFFFFD` (D-100): with another value BIP68 could put a relative time lock on an input, and the block's settlement could not land on BTC until it expired.
- The envelope leaf must be the canonical construction of the payload generated from the block's contents (compared byte for byte); the control block and the commitment output are both recomputed from the public key in the leaf and the leaf, and compared byte for byte.
- Payload (§7.1): the last header is this block and `entries` is all of this block's entries; the header each item in `bodies` points at must be a block in this block's past, and its `entries` must equal
  all entries of that block; the remaining headers may be any headers (self-certified by PoW); every header anchors at or below this block's anchor height (D-100): the reveal transaction commits to this block's anchor
  and so confirms above it, and the payload then always counts (§7.1).
- Fees: each transaction's own fee (sum of prevout amounts − sum of output amounts) ≥ `vsize × MIN_SETTLEMENT_FEERATE`, `vsize = ⌈weight / 4⌉`; without relying on package relay
  each can still enter the mempool on its own; above that, the node pays the rate of its fee policy (§7.7: the floor while the next BTC block has room, otherwise `estimatesmartfee` within a floor and a cap,
  D-117) and bumps a pair left waiting in the mempool through the P2A output (D-102), or sends its payload again in a new pair when another party's transaction holds that output (D-131).
- **Settlement transactions may not spend one another**: guaranteed by "producer inputs must be confirmed before the anchor height"; settlement transactions in the mempool have no parent-child relationships, which avoids the BTC mempool's
  limit of 25 unconfirmed ancestors. The producer's change can be used for funding again once the settlement it belongs to is confirmed.
- When receiving a new block (and before the producer submits its own block), additionally check: all inputs of the commit transaction are unspent in bitcoind's UTXO set, and the two transactions, as one package, pass
  `testmempoolaccept` (only the reveal transaction is submitted when the commit transaction is already in the mempool or confirmed).
- Construction: producer inputs are always the P2TR key-path of the funding key tweaked per BIP86, the public key in the envelope leaf and the internal key of the commitment output are both the funding key itself (untweaked), the signature is
  `SIGHASH_DEFAULT`, the inputs' `nSequence` is `0xFFFFFFFD` (BIP68 not enabled, RBF allowed); the reveal transaction's fee and the commitment amount are first computed from the final witness length,
  after the PoW is solved the final payload is written in (length unchanged), then the commit is signed first and the reveal after.
- **Broadcast timing** (D-037, D-063): with h the main-chain block that accepts the block, broadcast when the main-chain tip height is ≥ h + `SETTLEMENT_DEPTH` (2 blocks on devnet, about 10 seconds), commit first and
  reveal after. User ops already took effect when the block was produced; this delay only avoids sending two conflicting settlement transactions into the BTC mempool at the same time during a fast-lane fork; a reorg no deeper than it does not involve
  already broadcast settlement transactions. A broadcast transaction that is neither in the mempool nor confirmed is rebroadcast; the block holds the complete signed settlement transactions, so when the producer is offline any node can
  broadcast on its behalf. Settlement records, the on-chain determination and mempool checks go by the reveal transaction's ID (the transaction carrying the payload).
- For a transaction confirmed on BTC, only which headers and bodies its payload carries matters (§7.5); the transaction itself is not validated again.

Measured (regtest): an ordinary settlement is commit 154 vB + reveal 209 vB (payload 390 bytes); the maximum payload of 81 920 bytes is commit 154 vB + reveal 20 710 vB.

### 7.3 Rescue settlement

Local policy (D-045, D-057, D-077, D-081):

- For a block accepted by the main chain and `SETTLEMENT_DEPTH` deep whose settlement transactions cannot enter the mempool (the producer's funding input was spent, etc.), or wait there too cheaply or too long (below), when the data it carries (this block's data,
  the other headers and the data of each block in `bodies`) has not all landed on-chain yet, the node rebuilds with its own funding wallet: the payload unchanged, assembling and signing the commit / reveal pair at this node's fee rate
  (the envelope uses this node's funding key), broadcasting and tracking it like an ordinary settlement record (rebroadcast, reserved funding inputs, closed once confirmed or once all the data it carries has landed on-chain).
- The order follows the fast-lane main chain: the block's producer rebuilds immediately; when the k-th main-chain block (k ≤ `RESCUE_ORDER_WINDOW` = 6) from the main-chain block that accepts the block is produced by this node, it waits k BTC blocks after finding that it cannot
  broadcast; other nodes wait `RESCUE_ORDER_WINDOW + RESCUE_DELAY` (6 + 2). Before initiating, check the BTC mempool: when a KSN1 transaction claiming to settle the block already exists
  (the last header of its payload is that block), do not initiate.
- The same block may have several valid payloads; the indexer goes by the first one confirmed on BTC and ignores the rest; each node's rescue spends only its own funding, so they do not conflict, and the extra ones merely waste
  fees.
- **Cost**: a rescue copies the payload, so the carrier of the blocks it lands is still the producer of the block that carried the original (§8.6, D-095), and the rescuer pays the BTC fee without income. A producer can therefore shift the BTC
  cost of its own settlement onto the rescuers by spending its funding input after its block is accepted; it still has to mine the block, and the amount at stake per block is one settlement's fee. Accepted as is (D-098); a rescuer who wants the
  carrier reward can send a republish payload with its own payee instead (§7.6), which qualifies after `CARRIER_REPOST_DELAY`.
- **A settlement that did not count** (D-100): a settlement transaction confirmed at or below its block's anchor height after a BTC reorg does not count (§7.1) and lands nothing; rescue treats the block as stuck, and
  redoes its own rescue that confirmed that way. A node rescues a block only once its local BTC tip has reached the block's anchor.
- **Claims in the mempool** (D-098): a mempool transaction claiming to settle the block counts only when it is a commit / reveal pair carrying exactly the block's own payload, its commit spends confirmed outputs only and it pays at least
  this node's fee rate, and only for `RESCUE_ORDER_WINDOW + RESCUE_DELAY` BTC blocks after this node first saw it; otherwise anyone could suppress every rescue of a stuck block with a cheap claim that never confirms.
- **The block's own settlement waiting in the mempool** (D-102): it holds the rescue back on the same terms as a claim, only while it pays at least this node's fee rate and for `RESCUE_ORDER_WINDOW + RESCUE_DELAY`
  BTC blocks after this node first saw it waiting; then the block counts as stuck and the order above starts. Its producer does not rescue it but bumps it through the P2A output, or sends its payload
  again in a new pair when another party's transaction holds that output (§7.7, D-131); a CPFP child is not counted in the pair's fee rate, so another node may still rescue a bumped settlement in its turn.
- **Reserving funding inputs**: transactions of this node that are in flight at the same time (the settlement transactions blocks carry, rescues, republishes, split transactions of funding UTXOs) may not spend the same funding output; after choosing the funding and
  before broadcasting, "check and reserve" atomically, and release when rejected or abandoned. A pair whose record is dropped or closed (its block left the main chain, or its data landed through another
  transaction) keeps its inputs reserved while any of its transactions is in the BTC mempool, where a BTC reorg can also put it back (D-104).

### 7.4 Forced inclusion of native transactions

- **Recognition** (D-036, D-081, D-086): a non-coinbase BTC transaction has exactly one KSN1 data carrier and it is an OP_RETURN output, the payload decodes to `NativeOp { op }`, and
  `op.fee == BtcNative`. A coinbase transaction has no input to bind to and is not a native transaction.
- **Signature** (D-086): verified against the first input of the carrying transaction (§4.2). An op that fails authentication (invalid signature, or copied from another transaction) is recorded as failed and does not consume the nonce.
- **Window** (D-063): let `h(x) = x.anchor.height − NATIVE_CONFIRMATIONS + 1` (a signed integer). Fast-lane block B must include, at the start of its entry list, all native transactions confirmed at BTC heights in
  the window `(max(h(each parent)), h(B)]`, with `btc_txid` and `op` both identical, and no `Forced` entry may follow them. The windows of parallel blocks may overlap,
  and duplicate entries are skipped by nonce when merging; along any selected chain, native transactions still execute in order of BTC height and index within the block, and none is missed.
- **Anchor-invalid and merged blocks** (D-100): the window of an anchor-invalid block cannot be derived from its anchor chain, so its entries are checked against the current best chain when it is accepted (§6.3). Executed in lenient mode (§9.6),
  a forced-inclusion entry whose transaction ID names no native transaction on the merging block's anchor chain (at heights up to its anchor) that carries the same op stands for no path C op and makes the merging block
  invalid, as in strict mode.
- **Order**: ascending by BTC height, and within the same height ascending by the transaction's index in the block.
- **Execution** (§8.1, D-018): when any one of version, chain ID, encoded length, fee method, signature and nonce is not satisfied, it is recorded as a failed op without changing state (nonce unchanged); when all are satisfied, the nonce is incremented,
  and after that `max_gas > NATIVE_MAX_GAS` or a failed action is only recorded as a failed op (the action is rolled back, the nonce increment is kept). The fee the user paid to BTC miners is not refunded.
- **Gas and data size** (D-036): forced-inclusion entries count toward the block's total gas as `min(max_gas, NATIVE_MAX_GAS)` but are themselves not bound by `BLOCK_GAS_LIMIT` (the censorship-resistance
  guarantee must not be blocked by the gas limit); in the payload they take only `btc_txid` (33 bytes). When the forced-inclusion entries already exceed the limit or the payload cannot fit them, the block may not contain any more path B entries.
- **BTC reorg**: a reorg that moves or drops an included native transaction orphans the anchor of every block that included it: those blocks leave the main chain, the window restarts below the
  transaction's new height and it is included again once it confirms, or, double-spent, it is gone, as on BTC (§6.8, D-104). A reorg deeper than `ANCHOR_DEPTH` that leaves the native transaction where
  it was but takes back the block that included it (§6.4) includes it again too, in the first block of the new branch whose window covers it. Included again, it runs in the new branch's order, which BTC
  keeps only for transactions spending one another, and at the start of its block, ahead of the path B ops of the blocks taken off (and of the blocks they merged, unless the next block merges such a block
  again, as it does a parallel block the reorg leaves eligible, §6.2), which come back through the entry pool: an op whose nonce is then out of turn fails without consuming it (Execution above), and the
  account's ops above it wait in the entry pool until the account fills the gap (§10.4, §17.3). devnet uses `NATIVE_CONFIRMATIONS = 1`; 3 is recommended for mainnet (the effective confirmation count is
  `ANCHOR_DEPTH + NATIVE_CONFIRMATIONS`, so it can be lowered accordingly).

### 7.5 Data landing on chain and settlement batching

Which blocks carry settlement transactions (D-077, D-078): any block may carry settlement transactions or not; empty blocks and blocks containing only forced-inclusion entries may carry them too, to carry other blocks' data.
When a block that needs a body (containing path B or unlock entries) carries no settlement transaction, its body is carried on-chain by any later KSN1 payload: usually the settlement transaction of some later block, but it can also be
a republish transaction sent by anyone.

The data landing height (`data(b)` of §6.4) depends only on block contents, and the indexer gets the same result as the node from BTC data alone; whether a block carries settlement transactions does not affect it. The settled closure,
unlocking, BTC finality and the indexer all follow this definition.

The producer's strategy (local, D-063, D-075, D-077):

- Headers carried: starting from the parents, go through the blocks in the past in descending order of blue work (at most 1 000), not searching further into the past of a block that carries settlement transactions, and collect those whose headers have not yet appeared on
  BTC, at most `MAX_SETTLEMENT_HEADERS − 1`, sorted by blue work ascending; the compact encoding takes at most half of the payload limit, dropping the earliest when it exceeds that.
- Bodies carried: among them, the bodies (together with the headers) of blocks that carry no settlement transaction, need a body and whose data has not landed on-chain yet; they share the remaining space with this block's entries, with bodies taking at most
  a quarter of the payload limit, dropping the earliest when they do not fit (republishing fills them in later). Block production and republishing pack by the entries' upper bound (every key other than KSA counted as written in full), so any block's body fits later when
  carried by any payload; §6.3 makes that bound a validity rule (D-100).
- **When to carry settlement transactions (settlement batching, on by default)**: carry them when there is a body to carry (this block's or backlogged) and no settlement transaction is in flight. "In flight" means the most recent block in this block's past that carries settlement
  transactions has a header not yet on-chain and its anchor is at most 2 blocks behind the BTC tip (one waiting in the mempool longer is not waited for anymore: its producer bumps it and other nodes rescue it, §7.3, §7.7). In addition, carry them also when the backlogged bodies
  in total, or this block's own body, exceed an eighth of the payload limit, no longer waiting for the in-flight settlement transaction (the threshold is half of the body budget: before triggering, the backlog is at most twice the threshold,
  which fits in one payload). This way, when data is sparse, the block containing each op settles immediately; with continuous ops, the bodies backlogged while one settlement transaction is in flight (about one BTC block) are carried at once by
  the next block after it confirms, and each BTC block interval usually has only one batched settlement transaction. `--no-settlement-batching` turns batching off: every block that needs a body carries its own settlement
  transactions (faster BTC finality, more settlement transactions).
- When funding is temporarily insufficient, no settlement transaction is carried and path B and unlock entries are included as usual (the body is carried by a later settlement); a producer with no funding wallet configured also includes path B as usual
  (the body is landed on-chain by another carrier, who takes the carrier reward, D-095; the implementation before M23 only produced empty blocks and blocks containing only forced-inclusion entries, producing no bodies that others would have to pay to land on-chain).
- Cost: path B ops in a block without settlement transactions become BTC-final only with the next batch, about one BTC block later than per-block settlement; a producer carrying data for others pays BTC fees, and its income is
  the carrier reward (§8.6, D-095: 5% of each reward slot of each carried block and half of the path B sponsored fees in it), and its own income can only be unlocked once the data of all blocks in its past has landed
  on-chain (§8.5).

### 7.6 Header and body republishing

Local policy (D-047, D-075, D-077). A settlement payload carries at most `MAX_SETTLEMENT_HEADERS` headers; when consecutive blocks without settlement transactions exceed this number, or the chain has had no
block with settlement transactions for a long time, the settled closure stalls and path C ops cannot become BTC-final either. After each round of BTC sync, the node, in the main chain's acceptance order (each main-chain block preceded by the blocks it merges), finds
the first block f whose data is not on-chain. If f carries no settlement transaction and no later settlement payload will include it anymore: the first later block with settlement transactions whose past contains f is more than
`MAX_SETTLEMENT_HEADERS − 1` away from f, or that block's settlement has already landed on-chain without carrying f (the producer carried fewer headers or bodies because of the payload size); or there is no such block after it yet, but there are already
at least `MAX_SETTLEMENT_HEADERS − 1` blocks, or f's anchor is more than `4 × SETTLEMENT_TIMEOUT` blocks behind the BTC tip (no block with settlement transactions for a long time), or a block of a competing chain has
appeared on BTC and one BTC block has passed but fewer than `BTC_FINALITY_CONFS` (D-091: among the tips of the header DAG, a block not on this node's main chain whose fork-choice key `(chain_work, hash)`
is below this node's main-chain tip but above the highest connected block of this node's main chain; of a tip whose selected chain holds a block anchored, or merging a block anchored, off the BTC best chain,
or one the finality determination excluded, only the part below the lowest such block counts, since nothing above it can become final, D-104 -- if this node's chain does not land on-chain in time, the other side's block may be determined final first; waiting one BTC block first is because a parallel block
landing on-chain first is normal, and this node's next block with settlement transactions usually carries its own headers right away) -- then it sends a
`Headers { headers, bodies }` transaction (a commit / reveal pair, funded by this node; while such a competing chain is on BTC, this node's BTC transactions pay the next-block fee rate, §7.7, D-102): containing only the headers that settlement payload cannot fit (from f on, those whose accepting main-chain block is `SETTLEMENT_DEPTH` deep, consecutive,
without settlement transactions and not yet on-chain; when republishing because of a competing chain the depth is not waited for, up to the main-chain tip), plus the bodies of those among them that need a body, truncated to `MAX_BLOCK_DATA_BYTES`. A block with settlement transactions parallel to f (which may come
after it in acceptance order) will not carry it and is not counted. The same run of headers (distinguished by the starting hash) is not sent again until the transaction is confirmed or evicted; several nodes may each send one, and duplicates are
ignored by the indexer. The republish payload carries the payee `payee` (`--carrier-payee`, defaulting to `--producer`; all zeros, i.e. not collecting, when neither is given; D-095): when the republish lands a block's data on-chain for the first time and the data has waited more than
`CARRIER_REPOST_DELAY` (2) BTC blocks, the payee takes that block's carrier reward (§8.6); an earlier republish (typically D-091's republish of this node's own headers)
gets no reward, and the producer share of those blocks is not deducted.

### 7.7 The producer's funding wallet

Local policy (D-037, D-060, D-075, D-109):

- The producer's funding wallet is separate from its payee account (`header.producer`): the node is configured with one BTC funding private key (`--btc-funding-key-file`), and the funding address is its P2TR
  key-path (BIP86) address; on every change of the BTC chain tip the node fetches the confirmed funding UTXOs (confirmed before the anchor height, excluding immature coinbase,
  excluding inputs already used by this node's not-yet-confirmed transactions, including those of blocks that left the main chain, §7.3): with `scantxoutset`, one scan of the UTXO set per tip shared by
  the producer and the rescuer, or from a watch-only wallet of the same bitcoind holding `raw(<funding locking script>)` (`--btc-funding-wallet`), which needs no scan (D-109); on signet,
  where a scan takes 30 to 50 seconds, and on mainnet the wallet is required. When no UTXO is available, or the fetch fails (the producer asks again at the next anchor or after 30 seconds), the
  block carries no settlement transaction.
- **Funding UTXO pool**: settlement transactions can only be funded with confirmed UTXOs and cannot spend each other, so within one BTC block interval the number of transactions that can be funded is at most the number of confirmed UTXOs. When the available confirmed
  UTXOs are fewer than the target count (`--funding-pool-target`, default 64, 0 means no splitting), a split transaction is sent that splits the largest one into several equal outputs (each at least 100 000 sats),
  with at most one split transaction unconfirmed at a time.
- **Fee rate** (D-102, D-117): every BTC transaction of the node (settlements, rescues, republishes, split transactions, CPFP children) pays bitcoind's `estimatesmartfee` for the confirmation target
  (`--settlement-conf-target`, default 3 BTC blocks), kept within the floor `--settlement-feerate` (default 2 sat/vB, not below `MIN_SETTLEMENT_FEERATE`) and the cap `--max-settlement-feerate`
  (default 100 sat/vB); the floor when bitcoind has no estimate. While bitcoind's mempool holds at most half a block (500 000 vB, `getmempoolinfo`'s `bytes`), the next block has room for all of it, and a new
  transaction pays the floor instead, or the mempool's minimum (`mempoolminfee`) when that is higher, up to the cap (D-117). While a lighter competing chain is on BTC (§7.6) the target is the next block, whatever
  room the mempool has, so that this node's data lands before the other side's is final (a split transaction keeps the confirmation target: it does not race).
  A commit / reveal pair this node sent (the settlement of a block it produced, a rescue, a republish) that has waited in the mempool for 2 BTC blocks (at once during such a race) and pays less than the
  estimated rate, whether or not the mempool has room, gets a CPFP child spending the reveal's P2A output and one funding UTXO, lifting the package to the rate. Once it has waited 2 BTC blocks while the mempool
  had room (outside a race), the rate is at least the pair's own rate doubled, and doubled again for every further such block, up to the cap, whatever the estimate (D-117). When the rate rises again, a child
  paying more replaces it (full RBF). A child that left the mempool is sent again only while its pair still waits there and is still one this node settles (its block has not left the main chain, D-104).
  Anyone can attach a large low-rate child to the P2A output first, which a child of this node's would have to out-pay in full (BIP125 rule 3: pinning). When bitcoind refuses this node's first
  child of a pair while the reveal waits in the mempool and another party's transaction spends its P2A output, the pair's payload goes out again, byte for byte, in a new pair funded by this node at
  the bump's rate: it spends nothing of the old pair's, takes its place as the block's rescue or as the republish, and is bumped in turn (D-131). Nothing more is spent on a pair whose block's
  carried data has all landed.
- **Revenue address and hot-wallet cap**: the change is only required to be a standard output script; where it is paid is the producer's choice. When `--revenue-address` (an address on this chain's BTC network, not the funding
  address itself) and `--hot-wallet-cap-sat` (default 1 000 000) are configured, then while assembling this block's settlement transaction, if the total of the hot wallet's UTXOs available for funding, minus the funding inputs selected this time, is still
  not below the cap, the change is paid to the revenue address, otherwise it goes back to the hot wallet; a rescue settlement pays its change by the same rule to this node's own scripts (never to the original producer's), and the change of a republish transaction returns to the hot wallet. The hot wallet's balance therefore stays near the cap,
  and the excess moves to the revenue address (which can be a cold wallet) along with the settlement transactions.

## 8. Fees, incentives and unlocking

Path B is priced in KSA; path C pays only the transaction fee to BTC miners. The producer's income is KSA: the block reward, the sponsored fees paid by path B users, and the carrier reward for carrying other blocks' data onto BTC
(D-095); all of it unlocks only after the data of the corresponding block has landed on BTC and entered the settled closure. The producer posts locks in the exchange contract to swap KSA for BTC, which is used to pay the fees of the settlement transactions (D-078).

### 8.1 Fee rules at the consensus layer

- Path B: before execution, `amount` KSA is deducted from the user's account and recorded in the locked record at this block's acceptance position (§8.5); at unlock it is split by `CARRIER_FEE_BPS` between this block's carrier and
  `header.producer` (§8.6, D-095); if the balance is insufficient, the block cannot go on the chain
  (the entry of a merged block is skipped). After the fee is deducted, the nonce is incremented, and then the action is executed.
- Path C: pays the producer nothing. A single op's `max_gas` must not exceed `NATIVE_MAX_GAS`; exceeding it is a failed op (the nonce is consumed, §7.4).
- On execution failure (exceeding `max_gas`, contract trap, explicit revert, invalid attached funds or insufficient balance), the fee is deducted as usual, the nonce is incremented as usual, and all other state changes and events are
  rolled back; `gas_used` in the receipt is recorded as `max_gas` (receipts do not enter the state root).
- An unlock entry counts 0 gas and pays no fee; the `op_id` of its receipt is empty.

### 8.2 The producer's local policy (not consensus rules)

- Path B uses market pricing: the user gives a token amount in the VmOp, and the producer decides whether to pack it according to locally configured floors: a minimum amount (`--min-token-fee`, default 1) and a minimum
  fee rate (`--min-fee-per-kib`, default 0). The relay floor (`--min-relay-token-fee`, by default the same as `--min-token-fee`; when given, the effective value is the smaller of the two)
  decides whether an op enters the entry pool and is forwarded: an op that reaches the relay floor but not the packing floor enters the pool and is forwarded, to be packed by another producer. The protocol provides no on-chain reference exchange rate; the price risk between the token and BTC is borne by the producer
  itself.
- Entry pool (`ksana-mempool`): admission first does stateless checks (version, chain ID, encoded length, fee mode, signature), then checks against the current state (nonce not below the account nonce; KSA
  balance sufficient to pay the sponsored fees of all queued ops of this account), then policy (relay floor, pool capacity default 10 000, at most 64 queued ops per account, a replacement must raise the sponsored fee by at least a tenth and by at least the relay floor, D-130).
  An op's cost = max(VmOp encoded bytes, `max_gas × MAX_BLOCK_DATA_BYTES / BLOCK_GAS_LIMIT`) (at least 1), fee rate = sponsored fee × 1024 / cost.
  The packing candidates are, for each account, a contiguous run starting at the current nonce (cut off at the first op below the packing floor), selected among the accounts' queue heads by fee rate from high to low (ties by
  sponsored fee, then by `op_id`), forming an open fee market: an op whose `max_gas` fills the whole block cannot crowd out other ops with a slightly higher amount.
- `ksana_estimateFee` returns suggested values: the fee rate at which this node funds settlement now (§7.7; a reference for path C and top-up transactions), the suggested path B fee rate `per_kib` (the median, over the last 20 main-chain blocks, of
  each block's lowest path B entry fee rate, taking the larger middle value for an even count, not below `--min-fee-per-kib`) and the suggested amount for a plain transfer converted from it,
  `amount` (not below `--min-token-fee`); it does not change the conditions under which the node accepts ops.

### 8.3 Gas metering

- 1 gas = 1 wasmtime fuel unit; host functions and built-in actions are charged additionally according to fixed tables (`docs/ABI.md` §3, §4), and the tables are consensus parameters. wasmtime's instruction cost table
  depends on its version, so the wasmtime version itself is a consensus parameter.
- The gas limit is exact (D-020): the VM injects `max_gas + 1` fuel and requires at least 1 left on return, so a call that consumes more than `max_gas` always fails.
- A block's total gas is the sum of its entries' `max_gas` and must not exceed the current rules' `BLOCK_GAS_LIMIT`; forced-inclusion entries count as `min(max_gas, NATIVE_MAX_GAS)` and are not bound by
  the limit, unlock entries count 0; if adding a path B entry pushes the total over the limit, the block is invalid (§7.4).

### 8.4 Fee debt (deleted)

The v1 fee-debt mechanism existed only for path A and was deleted together with path A (D-078): there is no `Debt` entry, and the account state holds only the nonce.

### 8.5 Locking and cumulative unlock of sponsored fees

The producer receives its income only after the data has actually been written to BTC (D-038, D-074, D-079).

- **Acceptance position**: a main-chain block accepts, per the DAG, the new blocks in its past -- first the blocks it merges (the merge set except the selected parent, in consensus order), and finally itself. A block's acceptance position is
  `(height, index)`: `height` is the height of the main-chain block that accepts it, and `index` is its ordinal in that main-chain block's acceptance order (the merged blocks are 0, 1, ... in turn, and the main-chain block itself comes
  last). The acceptance order is the lexicographic order of positions.
- **Locked record**: when a block collects path B sponsored fees, the state key `0x08 ‖ height (u64 big-endian) ‖ index (u32 big-endian)` → `LockedFee { producer, amount }` is written (a merged block's
  entries are recorded at its own position and belong to its producer); a block with no sponsored fees writes no record. Block rewards are not recorded when the block is executed. So when neither the main-chain block nor the blocks it merges have any entries, no
  state is written at all, and the state root equals the selected parent's (empty blocks do not change the state root).
- **Unlock cursor**: state key `0x09` → `UnlockCursor { height, index }`, the acceptance position unlocked up to; when absent, nothing has been unlocked yet.
- **Unlockable segment** (`BlockEnv.unlockable`): the positions on the selected parent's main chain that come after the parent state's unlock cursor in acceptance order, at most `MAX_UNLOCK_BATCH` of them, each position
  with its block hash, producer, DAA score, weight, and whether it gets a reward (the main-chain block itself, and the blocks that are blue in the merge set of the main-chain block merging them). It is computed deterministically by the node and the indexer
  from the DAG (the same trust model as the information about merged blocks), and both give the same segment for the same block. The carrier (the sender of the payload that first landed its data on BTC, §8.6,
  D-095) is not in the segment: at unlock the execution layer queries the anchored view (`BtcView::carrier`), and the node and the indexer answer from the payload senders recorded in the BTC snapshot and the main chain of this block's selected
  parent.
- **Unlock entry** `Unlock { height, index }`: at most one per block. The target position must be in the segment, after the cursor, and the block at that position must have entered the settled closure (per the BTC chain at this block's anchor,
  `BtcView::block_settled`); otherwise a main-chain block cannot go on the chain. The settled closure advances as a whole by main-chain height, so only the target needs checking. Then, for each position after the cursor
  up to and including the target: a block that gets a reward is paid the block reward according to its DAA score and weight (§8.6, split three ways into block share, carrier and producer), then the sponsored-fee record at that position is released (deleted if present,
  split two ways between carrier and producer, §8.6), each added to the respective account's available KSA balance; the cursor is written as the target. A position is unlocked only once.
- A merged block's unlock entry is always skipped (D-100, §9.6): only a main-chain block's own unlock entry advances the cursor, and a merged block's rewards and fees are unlocked by the main-chain blocks that follow.
- Therefore, as long as one block's data has not landed on BTC, the sponsored fees and rewards of all later blocks cannot be unlocked, and every subsequent producer has an incentive to carry its body, start a rescue, or republish it.
- The producer's strategy: under the anchored view, take the highest main-chain height that has entered the settled closure, `settled` (§6.4); the target is the last position in the segment whose acceptance height does not exceed `settled`;
  one BTC block interval is about 120 positions, far below `MAX_UNLOCK_BATCH` (1 024).
- The `locked_fees` of `ksana_getAccount` is computed by the node from the DAG: the sponsored fees of the blocks produced by that account after the cursor, plus the rewards to be paid at unlock (projected with the cap applied, minus the carrier's share),
  plus the reward that account will receive as a carrier (D-095).

### 8.6 KSA and block rewards

- KSA is a built-in asset of the VM: name and symbol `KSA`, 8 decimals, asset ID of 32 zero bytes, issuer recorded as 32 zero bytes, none of the issuer capabilities of §9.3 (D-121); its metadata is
  written in the genesis state.
- **Max supply** `KSA_MAX_SUPPLY`: 2.1 × 10^16 base units on mainnet (210 million KSA, D-089), the upper bound on the sum of the genesis allocation and all block rewards. The total allocation in the genesis config
  must not exceed `KSA_MAX_SUPPLY − upper bound of total rewards`; mainnet's total rewards use up the cap exactly, so mainnet has no genesis allocation (a fair launch, with the block share as the only protocol-level allocation;
  a genesis allocation would require lowering `BLOCK_REWARD` accordingly). devnet still uses the D-074 placeholder value 10^17 (1 billion KSA), leaving headroom for the genesis allocation used in tests.
- **Reward schedule** (D-074, D-089, D-105): the DAA scores are divided into halving periods; period k (k = 0, 1, …) is `REWARD_HALVING_INTERVAL × 2^min(k, REWARD_INTERVAL_DOUBLINGS)` units long,
  i.e. the first `REWARD_INTERVAL_DOUBLINGS` periods double in length one after another and every later period is as long as the last of them, and every DAA score d in period k gets
  `reward(d) = BLOCK_REWARD >> k` (a shift of 128 bits or more gives 0; an interval of 0 means no reward). With `REWARD_INTERVAL_DOUBLINGS` 0 this is `BLOCK_REWARD >> ⌊d / REWARD_HALVING_INTERVAL⌋`.
  Mainnet: `BLOCK_REWARD` 4.2 KSA (420 000 000), `REWARD_HALVING_INTERVAL` 12 500 000 DAA score units (about 723 days at 5 seconds per unit), `REWARD_INTERVAL_DOUBLINGS` 2: the periods are
  12 500 000, 25 000 000 and then 50 000 000 units long (about 2, 4 and then 8 years), so the reward halves in about years 2, 6, 14, 22, 30, … (the round numbers are chosen so that the upper bound of total
  rewards equals the max supply exactly; exactly two years would be 12 614 400). The sum of all reward slots is 209 999 996 KSA (4 KSA less, from the truncation of integer halving); the last base unit is
  paid in period 28, and from DAA score 1 387 500 000 (about 220 years) the reward is 0. The upper bound of total rewards is
  `max_total_rewards = (REWARD_INTERVAL_DOUBLINGS + 2) × BLOCK_REWARD × REWARD_HALVING_INTERVAL` (each doubling period pays at most `BLOCK_REWARD × REWARD_HALVING_INTERVAL`, the periods of equal length
  after them at most twice that together). devnet placeholder values: 9.9 KSA, 25 228 800 (1 460 days), 0 doublings, upper bound about 499.5 million KSA.
- **Reward slots** (D-084): a block with weight w and DAA score D occupies the w reward slots at DAA scores `D − w + 1 … D`, `reward(B) = Σ_{i=0}^{w−1} reward(D − i)`
  (saturating sum; negative slots do not count). Empty and non-empty blocks have the same expected reward per unit of time.
- **Block share** (D-089): for every reward slot with DAA score `d < BLOCK_SHARE_END`,
  `reward(d) × BLOCK_SHARE_BPS / 10 000` (rounded down) is credited to the **block share account**,
  and the rest goes to the producer; later slots go entirely to the producer (whether to continue sharing is to be decided separately: the ratio and the end point are rules that vary with the protocol version, taken per each slot's own DAA score and adjusted through the upgrade schedule,
  §6.9; the ratio must not exceed 10 000). The block share account is given in the genesis config (`block_share_account`; the account is the Taproot output key; like the allocation it enters the config fingerprint, and it is written to state key `0x0B`,
  §10.1); when omitted, block rewards go entirely to the producer. Mainnet's ratio, end point and account are not published yet (D-134): `MAINNET` holds placeholders (0, no share) for the
  ratio and the end point, and a mainnet node does not start until they are filled in (§6.1). The ratio and end point for devnet / signet are 2 000 and 50 457 600 (two devnet halving periods).
- **Carrier reward** (D-095): the carrier of block B, `carrier(B)`, is the sender of the payload that first landed B's data on BTC (`data(B)` of §6.4; with several at the same BTC height, the one earliest in transaction
  position): for a settlement payload (§7.1) it is the `producer` of the block C that carries this settlement transaction (C must already be accepted by the main chain of the selected parent of the block that unlocks B -- as a main-chain block or
  a merged block, not necessarily on the main chain; when B carries the settlement transaction itself, it is B's own producer); for a republish payload it is its `payee` (non-zero), but that counts only
  when `data(B) − B.btc_anchor.height > ANCHOR_DEPTH + CARRIER_REPOST_DELAY` (the data landed only after waiting 3 or more BTC blocks after block production). Each reward slot d of B sets aside `reward(d) × CARRIER_BPS / 10 000` (mainnet 500, i.e. 5%, rounded
  down) for the carrier, taken out of the producer's share (0.21 KSA per slot in mainnet's first halving period); of the sponsored fee of each path B entry executed in B,
  `amount × CARRIER_FEE_BPS / 10 000` (5 000, i.e. half, rounded down) goes to the carrier and the rest to B's producer. A red block has no reward and hence no fixed part; a skipped entry
  has no fee and hence no reward. When B has no carrier (the block holding the settlement payload is not accepted by the main chain, the republish came before the wait above, or `payee` is all zeros) or the carrier is B's own producer, neither share is
  deducted and both go to the producer. The two ratios are rules that vary with the protocol
  version (`Rules`, §6.9, `BLOCK_SHARE_BPS + CARRIER_BPS ≤ 10 000`), taken per the own DAA score of the slot / block being unlocked.
- **Which blocks get rewards**: the main-chain block itself, and the **blue** merged blocks in its merge set (each by its own DAA score and weight); red blocks get no reward (red blocks are parallel blocks beyond the network latency,
  possibly from an attacker). Along any one selected chain, each block appears exactly once, either as a main-chain block or in one main-chain block's merge set, so it gets at most one reward; a block that cannot go on the chain but is merged as
  blue gets a reward all the same.
- **Payout**: at unlock (§8.5) it is credited directly to the producer's available balance (the block share and the carrier's share are credited in the same unlock to the block share account and the carrier respectively, D-095),
  and KSA's `total_supply` increases accordingly;
  the slots of parallel blocks may overlap, so at payout the reward is capped at `KSA_MAX_SUPPLY − current total supply` (first come, first served in acceptance order; the excess is not issued; when a block can only be paid in part,
  the block share's portion is secured first, then the carrier's, and the producer takes what remains), so the total supply never exceeds the cap at any time.
- **Mainnet emission schedule** (D-089, D-105; counted in DAA score, with 5 seconds planned per unit, so "years" are only approximate):

  | Halving period | DAA score | Approx. time | Reward per slot | Carrier / producer and block share (D-095) | Issued this period | Cumulative (of max supply) |
  | --- | --- | --- | --- | --- | --- | --- |
  | 0 | 0 – 12 499 999 | years 0–2 | 4.2 KSA | 0.21 / 3.99 | 52.5 million | 52.5 million (25%) |
  | 1 | 12 500 000 – 37 499 999 | years 2–6 | 2.1 KSA | 0.105 / 1.995 | 52.5 million | 105 million (50%) |
  | 2 | 37 500 000 – 87 499 999 | years 6–14 | 1.05 KSA | 0.0525 / 0.9975 | 52.5 million | 157.5 million (75%) |
  | 3 | 87 500 000 – 137 499 999 | years 14–22 | 0.525 KSA | 0.02625 / 0.49875 | 26.25 million | 183.75 million (87.5%) |
  | 4 | 137 500 000 – 187 499 999 | years 22–30 | 0.2625 KSA | 0.013125 / 0.249375 | 13.125 million | 196.875 million (93.75%) |
  | 5 | 187 500 000 – 237 499 999 | years 30–38 | 0.13125 KSA | 0.0065625 / 0.1246875 | 6.5625 million | 203.4375 million (96.875%) |
  | 6 | 237 500 000 – 287 499 999 | years 38–46 | 0.065625 KSA | 0.00328125 / 0.06234375 | 3.28125 million | 206.71875 million (98.4375%) |
  | 7–28 | from 287 500 000 | years 46–220 | halving each period down to 1 base unit | 5% / 95% | 3.281246 million in total | 209 999 996 KSA (≈ 100%) |

  The carrier's share is not deducted when there is no carrier (it goes to the producer), so the "carrier" column is an upper bound; there are about 17 280 slots per day, so in the first halving period at most 3 629 KSA per day is paid to carriers.

  On mainnet, how the "producer and block share" part divides between the producers and the block share account is not published yet (D-134).
- A contract can burn the assets it holds (including KSA) with `asset_burn`, and the total supply decreases accordingly; this phase does no fee burning and sets no holding requirement for producers (§17).
- Positioning: path C always uses BTC only; sponsorship offers users convenience, and the token captures the value; producers receive the token and pay BTC.

## 9. VM and execution environment

Contracts are compiled to WASM and executed by wasmtime. Determinism is the first requirement: the same initial state and the same entry sequence must yield the same `state_root` on any machine.
The interface between contracts and the VM (exports, calling convention, host function signatures and gas, length limits, state encoding) is defined in `docs/ABI.md`; this section gives only the key points of the rules.

### 9.1 wasmtime configuration

- wasmtime 49.0.1 (the version is a consensus parameter): fuel metering on; NaN canonicalization on; threads, multi-memory and other features that could introduce nondeterminism off; no WASI.
- At deployment the bytecode is fully validated with `wasmparser`: only WebAssembly 1.0 core instructions plus mutable-globals, sign-extension, bulk-memory, multi-value and
  the funcref-table instructions from reference-types are allowed; **no floating point** (any `f32` / `f64` type or instruction); SIMD, threads, multi-memory, memory64,
  exceptions, tail calls, GC and the other proposals are forbidden. Exactly one linear memory defined by the module itself, with an initial page count not above `MAX_WASM_MEMORY / 64 KiB` (256 pages); when growth at run time exceeds
  `MAX_WASM_MEMORY`, `memory.grow` returns −1; at most one funcref table, which must declare a maximum element count not above 4 096; imports may only be host functions of the `ksana` module, with exactly
  matching signatures; `memory`, `ksana_alloc: (i32) -> i32` and `ksana_call: (i32, i32) -> i64` must be exported. If any one of these is not met, deployment is rejected (failed op).
- **Run-time stack metering** (D-073): the point at which a wasm stack overflow triggers depends on native stack frame sizes, which differ between architectures. Therefore each function's stack frame is estimated as `64 + 16 × (parameter count + local count +
  maximum operand stack depth)` bytes (obtained during validation); before compilation, a mutable i32 global is added to the module and every function is rewritten: at entry it adds this function's stack frame estimate, and if that exceeds
  `STACK_BUDGET` (256 KiB) it hits `unreachable`; the function body is wrapped in a block, and after the block the estimate is subtracted. The inserted instructions are charged gas as ordinary instructions; what is stored is still the original bytecode, and `code_hash`
  is unchanged; the instrumentation is a pre-compilation consensus rule (so the `wasm-encoder` version is also a consensus parameter). The count is per contract instance, and each instance is bound by the budget separately. Recursion, function pointers, trait
  objects and `core::fmt` can all be used. At run time, wasmtime's `max_wasm_stack` is additionally set to 32 MiB and blocks are executed on a dedicated thread with a 64 MiB stack, so the native stack limit is never
  hit before the metering.
- The inter-contract call depth limit is `MAX_CALL_DEPTH` (the top level is level 1); a single op has at most 64 instances, and the linear memories of all instances total at most 256 MiB.
- Compilation results are cached by code hash; whether or not they are cached does not affect consensus.

### 9.2 Contract lifecycle

- User deployment: `code_hash = sha256(code)`, `contract_id = sha256(deployer account 32 bytes ‖ nonce as u64 little-endian 8 bytes ‖ code_hash)`; after storing the code and metadata
  (`code_hash`, deployer), the attached funds are transferred in first, then the contract is called with `method = "init"`, `args = init_args`. `init` can only be called at deployment.
- Contracts creating contracts (`create_contract`, D-073): using code already on chain (stored by some `Deploy`), `contract_id = sha256(creator contract ID ‖ salt 32 bytes ‖
  code_hash)` (a 96-byte preimage, unlike the 72 bytes of a user deployment, so the two kinds of ID never coincide); `init` is executed with the creator as the caller, and the deployer is recorded as the creator contract; it traps when the code does not exist,
  the address is already taken, the depth limit is exceeded, or `init` fails.
- Calling convention: every call creates a new instance, and the linear memory is not kept across calls; the input is Borsh `(method: String, args: Vec<u8>)`, and the output is Borsh
  `Result<Vec<u8>, Vec<u8>>` (`Err` is an explicit revert). A trap, running out of gas, an invalid output or one over 64 KiB, and `Err` are all call failures.
- **A failure at any call level fails the whole op**: all state changes and events of this op are rolled back, keeping only the fee deduction and the nonce increment. The exception is `try_call_contract` (D-073):
  when the callee's level (including all calls it makes) fails, only the storage writes, transfers and events after the checkpoint are rolled back, and the caller receives `(status, data)` and continues; the sub-call's gas
  limit is `min(gas, remaining gas − ⌊remaining gas / 64⌋)` (the caller keeps at least 1/64); on success or revert the unused gas is refunded, other failures use up the whole limit; the caller's
  own errors (invalid input, `init`, invalid `funds`, not enough gas to pay for this function) cannot be caught.
- `delegate_call`: executes another contract's code in the context of the current call (storage, balances, `self_id`, `caller`, `call_funds`); on failure the whole op fails.
- Reentrancy is allowed, and storage changes take effect immediately as seen by every level. A contract account's assets can only be moved out by the contract itself (`asset_transfer` / `asset_burn` / attached funds); a user account's assets
  can only be moved out by an op signed by the user. The issuer capabilities of an asset (§9.3, D-121) can stop its movements, never move a balance: there is no clawback.

### 9.3 Host functions

Import module `ksana`, 31 functions in total; see `docs/ABI.md` §3 for signatures, gas and length limits.

| Function | Purpose |
| --- | --- |
| `storage_get` / `storage_set` / `storage_remove` | The contract's private key-value storage (key ≤ 256 bytes, value ≤ 64 KiB) |
| `caller` / `self_id` | The current caller (at the top level the VmOp's signing account, in inter-contract calls the ID of the contract one level up) and the current contract ID |
| `block_height` / `block_timestamp` / `chain_id` / `gas_left` | Height and timestamp of the main-chain block executing this op (ops of a merged block see those of the main-chain block merging it), the chain ID, the remaining gas |
| `btc_anchor_height` / `btc_header` | The BTC height the current block anchors to; reads an 80-byte BTC block header, allowed only for blocks at heights not above anchor − `BTC_QUERY_DEPTH`, otherwise trap |
| `asset_balance` / `asset_transfer` / `asset_issue` / `asset_burn` | Built-in assets: query a balance; transfer out of this contract's account; issue more of this contract's asset (the first issuance writes the name, symbol and decimals and gives the asset the capability to mint); burn assets held by this contract |
| `asset_create` / `asset_freeze` / `asset_allow` / `asset_pause` / `asset_renounce` / `asset_policy` / `asset_status` | Issuer capabilities of built-in assets (D-121): create this contract's asset with the capabilities it declares (mint, freeze, allowlist, pause); freeze or unfreeze an account; put an account on or off the allowlist; pause or resume the asset; give up capabilities for good; read an asset's capabilities and pause; read an account's status flags |
| `create_contract` | Create a contract from code already on chain (§9.2), optionally with attached funds |
| `call_contract` / `try_call_contract` / `delegate_call` | Inter-contract call (optionally with this contract's assets attached), catchable call, delegate call (§9.2) |
| `call_funds` | The list of funds attached to this call |
| `native_tx` | When this op is a path C native op, returns the BTC transaction carrying it (transaction ID and all outputs, in transaction order); path B and read-only calls return "not present" (D-078) |
| `emit_event` | Writes to the event log; does not enter the state root, discarded when the op fails |
| `sha256` / `verify_schnorr` | Common cryptographic primitives (BIP340 signature verification) |

### 9.4 BTC visibility

- A contract can read sufficiently deep BTC block headers (depth at least `BTC_QUERY_DEPTH`) and then verify with the SDK's Merkle proof tools that a BTC transaction has been confirmed (`verify_tx_inclusion`
  also requires the proof of the same block's coinbase transaction to pin the tree depth, preventing a 64-byte transaction from being grafted in as an internal node to forge a transaction, D-072). The depth limit guarantees that a BTC reorg cannot change a contract's
  execution result.
- A path C op reads the transaction carrying it via `native_tx`: the transaction comes from a block on the anchored chain at a height not above the anchor height (anchor-invalid historical blocks follow the current best chain), and the result depends only on the anchored chain;
  all call levels within one op read the same transaction, and the same transaction may be read several times within one op, so the contract must itself prevent the same payment from being credited twice.
- This lets contracts respond trustlessly to payments on BTC: top-up (§5.3) and the "payout triggered by a BTC payment" example are both based on it.

### 9.5 Built-in modules and the development kit

- The built-in modules are implemented in native Rust, not through WASM: Account (nonce) and Asset (KSA and fungible assets issued by contracts).
- Asset metadata and issuer capabilities (D-121): every asset has a name of 1 to 64 bytes, a symbol of 1 to 16 bytes and at most 18 decimals. Its issuing contract holds the capabilities it declared
  when it created the asset (`asset_create`; `asset_issue` creates an asset with `mint` alone), and can only give them up: `mint` issues more, `freeze` stops a frozen account from sending, receiving
  and burning the asset, `allowlist` admits only the accounts it allows, `pause` stops every account but the issuing contract from sending it. Every movement of the asset obeys them (the `AssetTransfer`
  action, attached funds, the asset host functions); the issuing contract is exempt, a refused movement fails the op, and no capability moves a balance. `ksana_getAsset` lists an asset's capabilities
  and `ksana_getAssetStatus` an account's standing, so wallets show which assets an issuer can stop (`docs/ABI.md` §3).
- `ksana-contract-sdk` (no_std, `cdylib`): safe wrappers of the host functions, the `entry!` entry macro, Borsh-compatible decoding, BTC block header / transaction / Merkle branch tools;
  on non-wasm32 targets it is an in-memory mock host, so contract unit tests can run natively. Behind its `standards` feature it also holds the reference implementation of the
  token standards KS-20 and KS-721 and their conformance kit (`docs/STANDARDS.md`, D-120; application conventions, not consensus).
- Example contracts (`contracts/examples`, a separate workspace): the fungible token `token` (the reference KS-20 token); `asset-token` (a fixed-supply token issued as a built-in asset at deployment); the constant-product `amm` using integer arithmetic (either side can be a token contract or a built-in asset); `amm-factory`
  (creates trading pairs with `create_contract`, with addresses computable in advance); `btc-payment-trigger` (a BTC payment triggers a payout, with the beneficiary bound by an `OP_RETURN <32-byte beneficiary>` output);
  `ksa-exchange` (the top-up of §5.3). `contracts/testing/probe` exposes the host functions as methods for testing, and `receiver` and `collection` run the token standards in the
  VM; `ksana-test-contracts` compiles them into wasm byte constants.
  The build output of a contract's wasm varies with the absolute path of its source (D-031), so the determinism tests use fixed bytecode committed to the repository.

### 9.6 Block execution: strict mode and lenient mode

The state of main-chain block C = the state of the selected parent → execute the entries of the other blocks in the merge set in turn (in consensus order) (**lenient mode**) → execute C's own entries (**strict mode**,
the rules of §8.1, §7.4 and §8.5; violating any state-dependent rule disqualifies C). Along any one selected chain, every block's entries are executed exactly once: a block on the chain executes them as its own entries,
the other blocks are executed within the chain block that merges them. In lenient mode:

- the environment (height, timestamp, anchor, BTC view, rules and unlockable segment) is C's;
- an entry that would disqualify the block in strict mode is **skipped**: its receipt is `Skipped` and no state changes (duplicate inclusion, conflicting ops of the same account with the same nonce, insufficient balance,
  etc.), so when the same op is included by two parallel blocks only the one ordered first executes;
- unlock entries are always skipped (D-100): only a main-chain block's own unlock entry advances the cursor, so C's state never depends on how C's BTC view judges a merged block's unlock entry (an answer that can differ
  when C is executed again in anchor-invalid mode, §6.8);
- the path B sponsored fee is recorded at the merged block's own acceptance position and goes to the merged block's producer;
- forced-inclusion entries execute as in strict mode: an entry naming no native transaction of C's anchor chain (at heights up to C's anchor) that carries its op makes C invalid (§7.4, D-100), so executing C again in
  anchor-invalid mode finds the transactions its first execution found;
- entry order and the gas limit are the merged block's own validity rules (§6.3) and are not checked again here;
- when C is at the settlement backlog limit, the merged blocks' path B entries are all skipped.

Outcomes fall into three classes: **failed op** (receipt `Failed`, the block stays valid), **invalid / disqualified block** (determined only by the block's content, the parent state, the BTC data at the anchor and the block context the DAG
gives deterministically), **local fault** (storage, VM engine or BTC data source failure: abort processing, never judge the block invalid; a block executed again in anchor-invalid mode is never judged invalid or
disqualified by its execution either, §6.8). `Skipped` is not a final status: an op that is still valid returns to the entry pool and may be included again later.

## 10. State storage and the standalone indexer

State is organized as a Jellyfish Merkle Tree (`jmt` 0.12) with RocksDB as the underlying store; `state_root` is the root hash of this tree. The indexer shares the same execution code
(`ksana-execution`) and BTC tracking code (`ksana-btc`) with the fast-lane node, but reads only BTC data.

### 10.1 State key space

Keys are concatenated with the prefix first, then `sha256(full key)` is taken as the JMT key hash; values are all Borsh-encoded (`docs/ABI.md` §7).

| Prefix | Key | Value |
| --- | --- | --- |
| 0x01 | account | `AccountState { nonce: u64 }` |
| 0x02 | asset_id ‖ account | `u128` balance; the key is deleted when the balance is 0 |
| 0x03 | asset_id | `AssetMeta { name, symbol, decimals, total_supply, issuer, capabilities, paused }` (§9.5, D-121) |
| 0x04 | contract_id | `ContractMeta { code_hash, deployer }` |
| 0x05 | contract_id ‖ key | raw bytes of contract storage |
| 0x06 | code_hash | raw bytes of contract code |
| 0x07 | (no longer used) | formerly the funding records of path A (D-078) |
| 0x08 | height (u64 big-endian) ‖ index (u32 big-endian) | `LockedFee { producer, amount }`: the sponsored fee at this acceptance position not yet unlocked (§8.5; split with the carrier at unlock time, the record is unchanged, D-095) |
| 0x09 | (none) | `UnlockCursor { height, index }`: the acceptance position unlocked up to (§8.5) |
| 0x0A | (no longer used) | formerly the pending-unlock queue (D-079) |
| 0x0B | (none) | block share account (`[u8; 32]`, written from the genesis config's `block_share_account`, §8.6, D-089); when absent the whole block reward goes to the producer |
| 0x0C | asset_id ‖ account | `u8`: the account's status flags for the asset, frozen and allowed (§9.5, D-121); the key is deleted when no flag is left |

The tree's root depends only on the final set of key-value pairs (D-024): leaf `sha256("JMT::LeafNode" ‖ key_hash ‖ sha256(value))`, internal node `sha256("JMT::IntrnalNode" ‖ left ‖ right)`
(spelled this way in jmt itself), an empty subtree is the 32-byte constant `b"SPARSE_MERKLE_PLACEHOLDER_HASH__"`, and a subtree containing only one leaf is replaced by that leaf directly. The jmt version, like
wasmtime's, is a consensus parameter; `ksana-state` also keeps golden roots computed by an independent implementation to prevent silent changes on upgrade.

### 10.2 Fast-lane node storage

- The block store keeps every block with a valid header (not only the main chain; side-chain blocks and disqualified blocks included), receipts, settlement records and their landing status on BTC; the main-chain height index and the op index
(`op_id` → the position where it executed, the result, the events; when an op appears in several blocks, the position where it executed is kept, and it is recorded as skipped only when it appears only at skipped positions) are kept for the main chain only.
  So are the operation listings (D-111; local, for `ksana_getAccountOps` / `ksana_getLatestOps`, §11.2): each op of the main chain by its position (the acceptance height, the block's place in
  that height's acceptance order, the entry's index), once more under each account it concerns (the signer, the receiver of an asset transfer, the contract called), written in the batches that change the
  main chain and built again from the stored blocks at startup when they do not describe the stored main chain (a data directory from an earlier version).
  It also records every block ever inserted into the DAG in insertion order, the main chain's summaries by height (each main-chain block with the blocks it merges) and, for each accepted block, the height of the main-chain block
  that accepted it: below the pruning point the history is answered from these (D-097).
- State version = main-chain height; each block commits one version (an empty write set is committed too, the root unchanged); the latest `REORG_WINDOW` versions are kept for rollback, and after each commit the versions
  before `latest − REORG_WINDOW` are pruned; every version from the finalized tip's height on is kept as well, for the verdicts of the finality determination (§6.5 item 6, D-101), and with pruning on, every version
from the pruning root's height on (D-097). A read view pinned to a version returns an error once that version is rolled back or pruned, and never reads another branch's data.
- **Pruning** (D-097; local policy, on by default, `--no-pruning` turns it off): after each BTC sync, the pruning root R is the highest block determined final (§6.5) at a BTC height at least `PRUNE_BTC_MARGIN`
  (144) below the BTC tip, and on the main chain; the pruning point P is the block on R's selected chain `min_pruning_depth` (§6.6) below it, and the node prunes once P is at least `PRUNE_STEP` (1 000) blue score
  above the previous one. Memory then holds the block DAG (GHOSTDAG data) and the header DAG from P on, the finality determination with R as its root, the candidate bodies of the blocks whose headers the header DAG still holds (in it or pending),
  the BTC snapshot from `anchor(P) − NATIVE_CONFIRMATIONS` on, and the main chain's summaries from a window base no higher than P's height, the unlock cursor of R's state or the settled closure at that BTC height.
  Everything below is answered from the block store. A pruned block delivered again is a duplicate; a block whose computation needs a pruned block is not processed (which does not make it invalid); peers syncing
  from far behind are served the main chain's acceptance order from the store, so a new node still syncs from the genesis block. Each pruning writes the checkpoint `pruning.ckpt` (Borsh with a SHA-256 checksum,
  replaced atomically), from which a restart restores the DAGs, the finality determination, the bodies and the BTC snapshot, then inserts the blocks stored after it. Without a usable checkpoint, or with pruning off,
  the GHOSTDAG data and the header DAG are rebuilt from the block store at startup and BTC is rescanned from the genesis anchor. A BTC reorg deeper than what was kept (one that revokes R's determination, or reaches
  below the snapshot) stops the node with a highest-severity alert: it has to be resynced from an empty data directory. The orphan pool holds at most 256 blocks.
- **RocksDB memory** (D-103; local settings): the block store and the state store are opened with fixed memory bounds. A column family's write buffer is flushed at 4 MiB and a database's write buffers at 16 MiB
  in all, and the databases of the process share one 32 MiB block cache, which also holds the table files' index and filter blocks. With RocksDB's defaults (a 64 MiB write buffer and a 32 MiB block cache for each
  column family, index blocks outside any cache) the write buffers of the 18 column families kept every write for days, and the node's memory grew with the chain although pruning bounds its own structures.
  On Linux the node allocates with jemalloc, which returns freed memory to the system: with glibc's allocator the memory the node's threads freed stayed in its arenas, and with three nodes over libp2p each one's
  resident memory still rose by about 2 MiB per 1 000 blocks after pruning while the memory in use stayed flat.
- Write order (crash consistency): when the main chain appends a block, the state is committed first, then the block store is written; when rewinding to a fork point, the store's tip is changed first, then the state is rolled back. Crash recovery: taking the block store's tip
  as authoritative, find the highest version v (within `REORG_WINDOW`) that is at most the latest state version and the main-chain height and whose state root agrees with the main-chain block, roll the state back to v, and after syncing with bitcoind re-verify and
  execute the blocks after v on the original main chain as a "reorg"; when no such version exists (state database lost, belonging to another chain, or corrupted), log a highest-level alert, clear the state database, rebuild
  the genesis state, rewind the main chain to the genesis block and, after syncing, re-execute the whole original main chain (D-093). When the fork point rewound to is below the state database's oldest version (pruned), likewise clear it and re-execute from the genesis block up to
  the fork point (this happens only when a BTC reorg revoked finality determinations, §6.5 item 5). A pruning node always finds a consistent version at or above the pruning root (D-097).
- The BTC tracker scans bitcoind's best chain block by block starting near the genesis anchor, or from the pruning checkpoint's snapshot (native transactions, block headers in KSN1 payloads, the transactions that land each block's data on chain); on a BTC reorg it truncates to the fork point
  and rescans, publishing the result as immutable snapshots. When a payload carrying a block body is scanned, this node may not yet know the corresponding block: it is first recorded as unresolved, and counted according to the block's content once the node has stored
  the block. The data directory also holds the P2P node key (`p2p.key`) and the genesis config fingerprint.

### 10.3 Standalone indexer

The indexer is the source of final truth: it connects only to bitcoind, never to any fast-lane node, and rebuilds the fast-lane chain and all state from BTC data alone (D-048, D-063, D-082).

1. Scan BTC blocks in order with the same tracker as the node, recognizing native transactions, settlement payloads and republish payloads.
2. Take the block headers out of the payloads that count (§7.1), check them by the header DAG rules of §6.5 (GHOSTDAG limits, header rules, anchor not below the parents', anchor bound of the timestamp), and build the DAG with the same
   GHOSTDAG; main-chain selection is the same as the node's (§6.4: the selected chain of the best eligible candidate, with the finalized tip as the lower bound; after each round of the finality determination the main
   chain is chosen again under the current BTC chain, retreating below a block a BTC reorg made ineligible, with an alert, D-104).
3. **Block bodies**: the entries block B must force-include, `forced(B)`, are derived from BTC per §7.4 (this needs the headers of B and its parents, and B's anchor must be on the snapshot's best chain).
   When `entries_root(forced(B))` equals the header's `entries_root`, B needs no body; otherwise take, among the payloads carrying B's body, the first candidate whose `entries_root` matches after conversion
   (`Forced` entries are replaced, by position, with the entries of `forced(B)` with the same transaction ID). The body is uniquely determined by `entries_root` and, once determined, no longer changes with the snapshot. For an anchor-invalid block
   (§6.8): when `entries_root` is the root of empty entries, the body is empty; otherwise take the candidate that matches after conversion (`Forced` looks up the native transaction by transaction ID on the current best chain, over the range of §6.3); when there is none,
   derive the forced-inclusion entries from the current best chain over the same height range and adopt them if they match; when still undetermined, wait. The nodes accept an anchor-invalid block only when its entries pass these same
   lookups on their BTC chain (§6.3, D-100), so its body can be determined. The node uses the same rules to rebuild the blocks it lacks from the data on BTC, and answers
   the data landing status of blocks it does not have by the same rules (§6.5 items 3 and 5, D-093).
4. **Processable prefix**: the longest prefix of the main chain from the genesis block in which each block's body and the bodies of the blocks it merges are known and their data has landed on BTC (the settled closure), and the block has
   `SETTLEMENT_DEPTH` confirmations on the indexer's main chain or is on the finalized chain (§6.5). Rationale for the confirmation count: the settlement of a merged parallel block may appear on BTC first, while the header of the main-chain block at the same height is brought up
   only later by another payload; whenever a node broadcasts a settlement its main chain is already `SETTLEMENT_DEPTH` blocks past that block, and these headers all land on BTC afterwards, so the wait does not stall the indexer.
5. Execute the main-chain blocks one by one with exactly the same execution engine as the node (first executing the blocks it merges in lenient mode), with `BtcView` and the unlockable segment given by the same rules, verifying block by block the header's
   `state_root` (this execution is the verdict of §6.5 item 6, D-101). A main-chain block whose `state_root` does not match, whose strict mode fails or which violates the backlog limit is disqualified: mark it, reselect the main chain and continue processing. A block whose body, or the body of a block it merges,
   is invalid (state-independent entry rules, forced inclusion mismatch) is disqualified in the same way, with an alert: the nodes reject it and every block after it, and since the indexer chooses its main chain from the headers on BTC alone, halting on it
   would let anyone with a little hashrate halt every indexer (D-098). Processing halts only when such a block is finalized (item 6).
6. After each BTC sync, advance the finality determination (§6.5): the finalized tip is the lower bound of the main chain, and finalized blocks are executed without waiting for the confirmation count. When the
   determination waits for the verdict on a block whose selected parent is the finalized tip, the indexer executes it on top of the finalized tip's state without committing, and an invalid one is excluded with the
   blocks built on it (D-101); every state version from the finalized tip's height on is kept for that. When a finalized block is invalid (disqualified), halt processing and raise an alert -- since D-101 this can
   only mean the implementation has a non-determinism bug.
7. When a BTC reorg or a main-chain switch takes executed blocks out of the processable prefix, roll back to the highest height at which the two still agree; when the target is below the state database's oldest version (pruned), rebuild from genesis.
   After pruning, the processable prefix starts at the pruning root and a rollback never goes below it: when even the root leaves the main chain, or a BTC reorg revokes the root's finality determination (deeper than the
   pruning margin), the indexer starts over from the genesis anchor by itself, rescanning BTC and rebuilding the state (D-097).
8. Restart: continues from the pruning checkpoint when there is a usable one (D-097), otherwise rescans BTC from the genesis anchor; the first round finds, within `[oldest version, min(latest version, processable height)]`
   (not below the pruning root), the highest version whose state root equals the main-chain block's `state_root` and rolls back to it, rebuilding from genesis when none is found. While the processable height is still below the oldest
   version (bitcoind is behind, for example still syncing or reindexing), nothing can be compared yet and the indexer waits instead of rebuilding (D-098). `indexer.json` in the data directory records the genesis config fingerprint,
   the genesis block header and the most recently verified block.
9. Output: logs, plus the JSON-RPC `indexer_status` / `indexer_stateRoot` (§11.2). The halted state is kept in memory only; after a restart, execution runs again (the same block halts it again);
   local faults (bitcoind, storage, data source) are retried in the next round.
10. **Pruning** (D-097; on by default, `--no-pruning` turns it off): the policy of §10.2, the pruning root being the highest executed block determined final at least the margin ago. Memory holds the header DAG from
   the pruning point on, the finality determination from the root, the bodies of the blocks whose headers the header DAG still holds, the BTC snapshot from the pruning point's anchor and the executed main chain from its window base.
   An index in the data directory (`index/`, RocksDB) keeps the executed main chain's summaries and state roots by height and the height that accepted each block, written before the state of that height is committed;
   it answers the RPC and the carrier rule (§8.6) below the window and rebuilds the window after a restart. Each pruning writes the checkpoint `pruning.ckpt` (as the node's); an unusable one only means rescanning
   BTC from the genesis anchor, and a lost index after pruning makes the indexer start over from the genesis anchor. The index and the state store are opened with the RocksDB memory bounds of §10.2 and share
   one block cache, and on Linux the indexer allocates with jemalloc as the node does (D-103).

### 10.4 The two confirmation statuses

Nodes and the SDK must clearly distinguish these two statuses to applications, and the application decides by amount which one to wait for.

| Status | Condition | Typical latency |
| --- | --- | --- |
| Fast-lane confirmed | included by a main-chain block and executed successfully (an op in a merged block takes effect at the height of the main-chain block that merges it) | about 5 seconds |
| BTC-final | the main-chain block that accepts the block is on the finalized chain determined from BTC chain data, at or below the BTC-final tip (§6.5 item 7): its data and the data of the blocks it merges had `BTC_FINALITY_CONFS` confirmations on BTC while there was no heavier chain on BTC that was published earlier and landed in time, and the BTC block of that determination has `BTC_FINALITY_CONFS` confirmations too (D-114) | from about 110 minutes |

Op status (`ksana_getOpStatus`): `pending` (in the entry pool), `fast_confirmed`, `failed` (included but execution failed, the fee is still charged), `skipped` (included only by blocks merged by the main chain
and unable to execute at merge time, no fee charged; not a final status), `btc_final`. A block's settlement status (`ksana_getSettlement`) additionally gives `settled` (the main-chain block that accepts
the block has entered the settled closure) and `btc_final`.

**How long to wait** (D-098, D-100). A fast-lane confirmation is protected only by the fast lane's own hashrate, and with two-speed difficulty (§6.6) that protection depends on whether the chain is busy. An attacker with a share α of the hashrate who
mines a private branch from the payment block's parent reverses a payment the merchant accepted after the given wait with these probabilities (the best of three attacker strategies, 50 000 races per cell, the fork choice of §6.4;
`crates/ksana-consensus/tests/reorg_race.rs`):

| Chain | α | 30 s | 1 min | 5 min | 10 min | 30 min |
| --- | --- | --- | --- | --- | --- | --- |
| Busy (blocks carry other users' ops) | 5% | 2.0% | 1.5% | 0.06% | 0.03% | 0.002% |
| Busy | 10% | 7.5% | 6.5% | 0.91% | 0.37% | 0.05% |
| Busy | 20% | 27% | 25% | 9.3% | 5.7% | 2.3% |
| Idle (only empty blocks) | 5% | 26% | 20% | 4.3% | 1.8% | 0.38% |
| Idle | 10% | 43% | 33% | 13% | 7.7% | 2.6% |
| Idle | 20% | 69% | 58% | 37% | 28% | 13% |

On an idle chain the honest work arrives in empty blocks of weight 12 to 120, each taking minutes, so a payment is protected by little more than its own block for minutes. Before D-100 a block's own work did not count in the fork choice until a
block was built on it, and a run of cheap weight-1 blocks overtook the payment block far more often: a 10% attacker reversed 73% of idle-chain payments after one minute and 36% after five. On a busy chain an attacker's lucky empty block is worth
12 honest blocks and now counts as soon as it is found, which raised the short waits (a 10% attacker after one minute: 3.5% before D-100; were every block of weight 1, 0.05%). Applications should therefore treat a fast-lane
confirmation as final only for small amounts: on a busy chain, wait about 5 minutes for moderate amounts; on an idle chain, or for large amounts, wait for BTC finality. BTC finality itself is unaffected.

**Deep BTC reorgs** (D-104). A BTC reorg of k blocks, k greater than `ANCHOR_DEPTH`, takes back the fast-lane blocks anchored on the BTC blocks it orphans, those produced during
about k − `ANCHOR_DEPTH` BTC block intervals (about 10 minutes on mainnet for k = 3). Their path B ops return to the entry pool and are included again on the new branch with the same op ID, so a natural deep reorg reverses
a payment only if the sender gets another op with the same nonce included first. A BTC miner able to make such a reorg can prepare the double spend in a fast-lane branch anchored on its withheld BTC
blocks, which the nodes take when they retreat: against such a miner a fast-lane confirmation is worth its anchor's BTC confirmations, and reversing a block m BTC blocks after it was produced takes a reorg
of at least `ANCHOR_DEPTH` + 1 + m blocks. A reorg of fewer than `BTC_FINALITY_CONFS` blocks never takes back a BTC-final block. One replacing the BTC block that
determined it, at least `BTC_FINALITY_CONFS` blocks, takes it off the finalized chain when its new branch carries a heavier chain forking below it, or when it removes the block's data, at least
`2 × BTC_FINALITY_CONFS − 1` blocks; otherwise the block is determined final again on the new branch and reported again once the BTC block of that determination has `BTC_FINALITY_CONFS`
confirmations, at once when the new branch is already that long (§6.5, D-114). A path C op whose block the reorg takes back is included again,
in the new BTC order when the reorg moved its native transaction, at the start of its block, and can fail out of turn (§7.4, §17.3). The account's ops above it wait behind its nonce until it is
included again and, if it fails, until the wallet signs an op for the missing nonce; the entry pool keeps at most 8 of an account's ops behind a nonce gap and drops the last ones (local policy, D-098),
while the nonces path C ops waiting to be included again take in turn from the account's nonce on, their native transactions on the BTC best chain and their ops authenticated, are no gap, as a block
runs them first, in BTC order (local policy, D-104).

## 11. Network, RPC and SDK

Nodes talk to each other over libp2p and expose JSON-RPC externally; users connect through the TypeScript SDK or the CLI. The network layer's message formats, limits and scoring are all local policy (D-040,
D-059, D-063).

### 11.1 P2P

- Transport: TCP (`/dns4` addresses allowed) + Noise + Yamux, plus identify and ping; the node key (ed25519) is stored in `p2p.key` in the data directory.
- gossipsub topics: `/ft/1/blocks` (Borsh(`Block`), including the signed settlement transactions) and `/ft/1/ops` (Borsh(`OpMessage { op }`), path B VmOps, at most `MAX_OP_BYTES`). The message ID is
  the content hash. Manual validation: after processing, the node reports Accept (valid and new, keep forwarding; side-chain blocks that do not win are forwarded too) / Ignore (duplicate, unknown parent, rejected only because of the local view,
  op invalid under local state, local fault: not forwarded, no penalty) / Reject (objectively invalid: invalid block, invalid op format or signature: not forwarded and penalized).
- request-response protocol `/ksana/sync/2`, each frame being a 4-byte little-endian length ‖ Borsh: `Status` (genesis hash, main-chain tip height / hash / chain work (§6.6, 256-bit little-endian),
  up to 64 DAG tips that can still be merged: those whose blue score + `MERGE_DEPTH` is not below the main-chain tip's, by `(chain_work, hash)` descending; local policy, D-104: leaving out
  those anchored, or merging a block anchored, off the node's BTC best chain, such as the blocks a retreat leaves behind, which a peer on that BTC chain would refuse, and carrying in their place the
  blocks below them that templates merge, §6.2: the main-chain tip the retreat went back to, and a parallel block it left behind), `GetBlocks { locator, after, max }`
  (the peer takes the first block F in the locator that is on its main chain and returns the known blocks not in `past(F) ∪ {F}`, by `(blue_work, hash)` ascending, only those with a key greater than `after` when it is given,
  at most `max ≤ 500`; blocks below the point on F's selected chain where the blue score is lower than `blue_score(F) − 2 × MERGE_DEPTH` are omitted), `GetBlock { hash }`, `GetMempool { max ≤ 1000 }`.
  The locator is block hashes on our main chain: the first 10 consecutive from the tip, then at doubling intervals, and finally the genesis block (the server uses only the first 128 entries).
- Sync: after a connection is established, first ask for `Status`; if the genesis hash differs, disconnect and reject the peer; when the peer's chain work is greater, or its tips include blocks we do not have, fetch `GetBlocks` in batches by cursor,
  then fetch the missing parents of orphans by hash (at most 64 per round, 16 rounds), then fetch the peer's entry pool once; after that, ask for `Status` again every 3 seconds, and immediately when a gossiped block's parent is unknown.
  Blocks the finality determination is waiting for are also among the blocks requested from peers (§6.5).
- Refusals while syncing (local policy, D-100 item h): a block delivered by sync and rejected only because of the local view, or a copy whose settlement transactions are refused, is answered from
  memory, without being checked again, while the local BTC tip stays the same and for at most 30 seconds; a refusal of a copy's settlement transactions is remembered for that copy only. The body
  checks against the BTC view and the settlement transactions come before the signature checks. A block missing a parent refused for itself, not because the local BTC view lags, is refused with it,
  for as long as the parent's refusal is remembered, instead of being kept as an orphan, whose missing parent would be fetched and checked again at every sync for as long as the BTC chain stands;
  remembering such a refusal drops the orphans waiting for the block, and those waiting for them, which come back with it in a later sync (local policy, D-104).
- Limits (local policy, D-040, D-132): gossip blocks 16 MiB, gossip ops `MAX_OP_BYTES` (a longer one cannot be valid, §4: it is rejected before it is decoded and penalized); sync requests
  64 KiB, sync responses 32 MiB, those to `Status` 64 KiB and to `GetMempool` 4 MiB (the server sends the leading ops that fit; a client refuses a response longer than its request's limit
  unread, without a penalty while it is within 32 MiB); the length is checked before decoding and full consumption is required. A `GetBlocks` server loads only the leading blocks that fit.
- Scoring: each block / op judged Reject scores 1 point against the source peer; at 3 points it is disconnected and banned for 60 seconds, and a peer connected from a public address bans the
  address too: no inbound connection from it is taken while the ban lasts, those already open stay (D-132); request timeout 30 seconds; a static bootstrap peer is redialed every 2 seconds after it disconnects.
- Sources (local policy, D-132): messages, requests and connections are counted by the address a peer connected from (an IPv4 address, an IPv6 address by its /64), or by the peer itself for a
  loopback, private, link-local or unspecified address (the node's own machine and network, containers) and for an address without IP.
- Rate limiting (D-059, D-132): three token buckets per source, shared by every identity connecting from it and kept across reconnections -- gossip blocks 20 / s, capacity 100; gossip ops
  1 000 / s, capacity 5 000; inbound sync requests 50 / s, capacity 200 (`Status` and `GetBlock` count 1 each, `GetBlocks` and `GetMempool` count 10 each). Gossip messages over quota are
  treated as Ignore before decoding, requests over quota are dropped, and neither is penalized.
- Connections and their work (local policy, D-132): at most 100 inbound connections established, 32 in the handshake, 4 from one address (established and in the handshake together,
  refused before the handshake; not for the addresses counted by peer) and 2 with one peer; outbound connections are capped only per peer. The gossip messages awaiting a verdict hold at
  most 256 MiB for blocks and 32 MiB for ops, a source at most a quarter of each; beyond that a message is treated as Ignore at once. Inbound sync requests are served at most 4 at a time, a
  source has at most 16 in flight, the responses being sent hold at most 256 MiB, a source a quarter of it, and a connection has at most 8 sync streams open; a request over these limits is
  dropped without a penalty.
- Peer discovery: devnet uses a static bootstrap peer list (`--peer`); Kademlia is added later (§17).

### 11.2 Node JSON-RPC

Based on jsonrpsee, serving HTTP and WebSocket on the same port (`--rpc-addr`, default `127.0.0.1:8545`; cross-origin calls from browser pages need `--rpc-cors`). The format, semantics and error codes
of parameters and results are as given in `docs/RPC.md`; the essentials follow. Conventions: byte strings are lowercase hex, `u64` / `u128` are decimal strings; a block identifier is `"latest"`,
a decimal height (main-chain block) or a block hash (any block in the DAG).

| Method | Purpose |
| --- | --- |
| `ksana_submitOp` | submit a Borsh-encoded path B VmOp; pre-checked by consensus rules, accepted or not by local policy; when this node produces blocks it returns only after the new op has entered the template |
| `ksana_getOpStatus` | `pending` / `fast_confirmed` / `failed` / `skipped` / `btc_final`, plus the block that includes it, the acceptance height and the receipt |
| `ksana_getAccountOps` | an account's operations on the main chain (signed by it, transfers to it, calls to it as a contract), newest first, by pages with a position cursor `before` (D-111) |
| `ksana_getLatestOps` | the main chain's operations, newest first, by pages like `ksana_getAccountOps` (D-111) |
| `ksana_getBlock` | header, entries, settlement transactions `{ commit, reveal }`, receipts, `accepted_height`, `merged` (the blocks it merges) |
| `ksana_getAccount` | nonce, available balance per asset, income not yet unlocked as a producer `locked_fees` |
| `ksana_getAsset` | the asset's name, symbol, decimals, total supply, issuer, the issuer capabilities it holds and its pause (D-121) |
| `ksana_getAssetStatus` | an account's status flags for an asset and whether it can send and receive the asset now (D-121) |
| `ksana_call` | read-only contract call on the latest state, may attach `funds`, nothing goes on chain |
| `ksana_getSettlement` | a block's data landing status: settlement transactions (reveal / commit transaction IDs, internal and display byte order), `carrier` (the block carrying its body), `status` (pending / broadcast / confirmed), `settled`, `btc_final` |
| `ksana_estimateFee` | the fee rate this node's settlement funding pays now (§7.7) and the suggested token amount for path B |
| `ksana_getChainInfo` | main-chain tip, DAA score, protocol version, the next scheduled upgrade, the highest version this software supports, the BTC-final tip `finalized` (§6.5 item 7) |
| `ksana_subscribeBlocks` | WebSocket subscription to new main-chain blocks `{ height, hash, entries, timestamp }` |

Error codes: `-32602` malformed parameters; `-32010` invalid op (consensus rules); `-32011` does not meet this node's policy; `-32004` object not found; `-32601` no such method, or not
available (the operation listings on a node whose index is not built, D-111); `-32603` internal error.

Server limits (local policy, D-132; the same for the indexer): a batch holds at most 64 calls, a request body or WebSocket message at most 1 MiB, a response at most 10 MiB; at most 100
connections at once and 16 subscriptions per WebSocket connection. jsonrpsee answers what passes them with its own codes: `-32007` a body over the limit (HTTP 413), `-32008` a response
over it, `-32006` a subscription over the limit, and, as one error object with `id` null answering the whole batch, `-32010` a batch of more than 64 calls and `-32011` a batch whose answers
pass the limit (not the op errors of the same codes, which answer one `ksana_submitOp` with its `id`); HTTP 429 beyond the connections.

Indexer (`ksana-indexer --rpc-addr`, default `127.0.0.1:8546`): `indexer_status` → `{ height, hash, state_root, btc_height, finalized, halted }`
(the highest block executed and verified, the scanned BTC tip, the BTC-final tip (§6.5 item 7), the reason processing halted), `indexer_stateRoot(block identifier)` → `{ height, hash, state_root }`
(answers only for verified blocks).

### 11.3 TypeScript SDK

`sdk/ts` (`@ksana/sdk`, `private`, ESM, D-053): supports both Node.js (≥ 20.19) and browsers; the library code uses only Web standard APIs.

- Consensus types and Borsh codec (the `borsh` package plus length limits, range checks and re-encode comparison, rejecting non-canonical encodings), the JSON of D-007; compact headers and compact entries of payloads
  (`encodePayload` / `decodePayload`, by network parameters); parameters `DEVNET` / `SIGNET` / `MAINNET` and the upgrade schedule (`rulesAt`).
- Networks (D-106): nothing defaults to a network. What signs, builds or decodes takes the parameters of the node it talks to, which `paramsForChainId` / `KsanaRpcClient.getParams` pick by the
  node's chain ID (`ksana_getChainInfo`), throwing `ChainMismatchError` for a node on another network than the one expected or on no network; a path C transaction is never built for an op signed
  for another chain ID.
- Hashes (tagged hash, sighash, `op_id`, block hash, `entries_root`), accounts (`accountOf` is the BIP86 output key; BIP340 signing uses the tweaked private key),
  BIP322 messages and local signing (the account's Taproot address), local self-check; conversion between accounts and addresses (`accountToAddress` / `accountFromAddress` / `parseAccount`,
  D-092); path C signing, signature verification and the wallet signing flow all carry `firstInput` (§4.2).
- Path B: sign and call `ksana_submitOp` directly. Path C: the KSN1 data script and the native transaction (unsigned PSBT, may carry payment outputs; weight limit 400 000 WU); the op is signed with
  `nativeFirstInput(utxos[0])`, and `buildNativeTx` rejects an op not bound to the first UTXO, so the wallet's BIP322 signing request must be issued after coin selection.
- Top-up (§5.3): reading and parsing the exchange contract's locks, `lockArgs` / `claimArgs`, `quoteTopUp`, `buildTopUp` (a claim op signed with a one-time key and bound to `utxos[0]`
  + the PSBT of the payment transaction, returning `payBy`; rejects locks whose beneficiary is not the buyer, that are not `Open`, that are near expiry, or that are overpaid or underpaid; overpayment and underpayment can be explicitly allowed).
- JSON-RPC clients for the node and the indexer (results strictly parsed, unknown fields rejected; the operation listings `getAccountOps` / `getLatestOps` with their positions, D-111),
  WebSocket subscriptions.
- Wallet adapters: Unisat, Xverse (calling their injected providers directly); the wallet must use a Taproot address, which is the account (D-092); other address types are rejected on connect;
  `signVmopWithWallet` has the wallet BIP322-sign the VmOp and self-checks it locally; the mock wallet for tests implements the same interface as Unisat. Xverse uses the ordinals
  (P2TR) address by default. An adapter says which BTC network the wallet is set to, when the wallet tells (`getNetwork`: Unisat `getChain`, Xverse `wallet_getNetwork`; D-106).
- Recommended dependencies (exactly pinned): `@noble/curves`, `@noble/hashes`, `@scure/base`, `@scure/btc-signer`, `borsh`; the shared vectors `tests/vectors/` and the official BIP322
  vectors are checked one by one by the SDK tests. `examples/wallet-demo` is the browser wallet demo page (path B; `docs/SIGNET.md` gives the manual steps on signet).

### 11.4 CLI

`ksana-cli` (Rust, D-051, D-078, D-085) is purely a client: it queries and submits through the node's JSON-RPC, and path C also connects to bitcoind. The network is the node's (D-106): every
command but `key` first reads the node's chain ID (`ksana_getChainInfo`) and signs for that network; the global `--chain devnet|signet|mainnet` (D-096) only states the network expected, and a node on
another network, or on a chain ID of no network, is refused before anything is signed or sent; path C and `top-up` also refuse a bitcoind on another BTC network (`getblockchaininfo`). For `key`, which
does not talk to the node, `--chain` picks the address network (devnet by default). `--rpc` (environment variable `KSANA_RPC`, default `http://127.0.0.1:8545`). Every parameter that takes an account accepts either a Taproot address on this chain's network or 64 hex chars
(D-092; contract IDs are hex only). Commands: `key` (generate or read a private key, print the account, its Taproot address (the same bytes) and the internal key; the private key file is 64 hex
chars, the same as `ksana-keytool`), `account` (with the address) / `asset` / `op` / `block` / `settlement` / `estimate-fee` / `query` (read-only call),
`transfer` / `deploy` / `call` (`--fee-token <amount>` for path B, or `--native --btc-wallet <wallet>` for path C, funded, signed and broadcast by the bitcoind wallet;
`--sig bip340|bip322` selects the VmOp signature scheme, default BIP340;
`--fund [<asset ID>:]<amount>` attaches funds; `--wait` polls until the op is included, continuing to wait on `skipped`), `top-up` (`--exchange --lock --sats --btc-wallet`,
`--allow-overpay`, `--expiry-margin` default 3), `exchange deposit / withdraw / lock / release / show / balance`. The nonce defaults to the account's next
nonce on the node; results are always printed as JSON. There is also `ksana-keytool` (generates a private key and prints the account, the internal key and the funding address: the funding address is the account's address).

## 12. Tech stack and repository layout

The core is in Rust (toolchain pinned by `rust-toolchain.toml`: 1.98.1, edition 2021, with the wasm32-unknown-unknown target), organized as a Cargo workspace; the SDK is in
TypeScript; the development and CI environment is Ubuntu 22.04. bitcoind must be Bitcoin Core 28.0 or later (P2A outputs and package `testmempoolaccept` are needed), with txindex enabled;
devnet uses 28.4. Build and test commands are collected in `CONTRIBUTING.md` at the repository root.

```text
ksana/
├── Cargo.toml                # workspace (members crates/*, tests/integration; default-members crates/*)
├── clippy.toml               # bans HashMap / HashSet / SystemTime::now / Instant::now
├── crates/
│   ├── ksana-primitives/     # consensus types, Borsh codec (length limit checked first), hashes, compact headers and entries, bodies, protocol parameters and upgrade schedule
│   ├── ksana-crypto/         # BIP340 tagged hash, Schnorr, VmOp signatures (BIP340 and BIP322 simple; path C binds the first input), Merkle utilities
│   ├── ksana-state/          # JMT (jmt 0.12) + RocksDB: version = height, commit / rollback / prune / pinned-version read view
│   ├── ksana-vm/             # wasmtime 49 engine config, static checks at deploy time, runtime stack metering instrumentation, host functions and gas table
│   ├── ksana-execution/      # block execution (shared by node and indexer): merge execution, entry rules, sponsored fees and unlocking, rewards, built-in Account / Asset, genesis
│   ├── ksana-consensus/      # Consensus trait and PoW: pluggable mining algorithm (Argon2id), compact bits, work, ASERT, header version, timestamp rules, GHOSTDAG
│   ├── ksana-btc/            # KSN1 data output and witness envelope, native transaction recognition (excluding coinbase) and first input, forced-inclusion window, settlement transaction assembly / signing / verification, payloads, BTC tracker,
│   │                         # settled closure, backlog limit, unlockable segment, header DAG and finality determination, bitcoind RPC client, mock chain for tests
│   ├── ksana-mempool/        # entry pool (local policy): admission, replacement and capacity, packing candidates
│   ├── ksana-rpc/            # JSON-RPC interface, JSON types and server (jsonrpsee, HTTP + WebSocket)
│   ├── ksana-p2p/            # libp2p: gossipsub, /ksana/sync/2, scoring and bans, rate limiting, static bootstrap
│   ├── ksana-node/           # node library and binary: block store, block DAG / main chain / reorg / orphans / crash recovery, BTC tracking, block production loop, settlement batching,
│   │                         # delayed broadcast, op admission, network integration, rescue and republishing, funding UTXO pool, finality, RPC backend; ksana-keytool
│   ├── ksana-indexer/        # standalone indexer library and binary: connects only to bitcoind; header DAG, bodies, settled closure, execution verification, finality, RPC
│   ├── ksana-cli/            # CLI
│   ├── ksana-contract-sdk/   # no_std contract development kit (mock host on non-wasm32 targets)
│   └── ksana-test-contracts/ # compiles the example and test contracts into wasm byte constants (nested cargo, cdylib only)
├── contracts/                # separate workspace for the example contracts: examples/{token, asset-token, amm, amm-factory, btc-payment-trigger, ksa-exchange, poll, and the playground's},
│                             # testing/{probe, receiver, collection}
├── sdk/ts/                   # TypeScript SDK (@ksana/sdk), examples/wallet-demo, test/e2e
├── devnet/                   # docker-compose: bitcoind regtest + init + btc-miner + 3 nodes + indexer; smoke.sh, soak.sh
├── mainnet/                  # mainnet genesis config template (block share account is a placeholder, cannot start as is, D-096) and its notes
├── playground/               # the contract playground (D-119): its page, the examples' modules, the compile service
├── signet/                   # the hosted public signet (D-107): docker-compose for bitcoind on Bitcoin's signet, seed nodes, indexer, the web wallet of ksana-web, BTC proxy and KSA faucet; deploy.sh over ssh, rehearse.sh
├── fuzz/                     # separate cargo-fuzz workspace (nightly)
├── tests/integration/        # regtest end-to-end tests (not in default-members, run by run.sh)
├── tests/vectors/            # test vectors shared by Rust and TS (codec.json, signing.json, merkle.json, pow.json, standards.json)
├── audits/                   # the reports of the security audits
└── docs/                     # SPEC.md, ABI.md, RPC.md, STANDARDS.md, DECISIONS.md, SIGNET.md, ARCHITECTURE.md, HISTORY.md, images/
```

| Purpose | Dependency (versions pinned in `Cargo.lock` / `package-lock.json`) |
| --- | --- |
| BTC data structures, scripts, sighashes and PSBT | `bitcoin` 0.32 |
| bitcoind RPC | `corepc-client` 0.17 |
| Schnorr signatures | `secp256k1` 0.29 |
| WASM execution, validation and instrumentation | `wasmtime` 49.0.1, `wasmparser` / `wasm-encoder` 0.258 (all consensus parameters) |
| State tree and storage | `jmt` 0.12 (consensus parameter), `rocksdb` 0.25 |
| Encoding and hashing | `borsh` 1.8, `sha2` 0.10, `serde`, `hex` |
| Mining algorithm Argon2id | `argon2` 0.6 (RustCrypto, pure Rust; consensus parameter, D-094) |
| Network and RPC | `libp2p` 0.57, `jsonrpsee` 0.26, `tower-http` (CORS) |
| Async, CLI and errors | `tokio` 1, `clap` 4, `thiserror`, `anyhow` |
| Testing | `proptest`, `cargo-fuzz` (`libfuzzer-sys`), `tempfile`, `wat` |
| TS SDK | `@noble/curves` 2.4, `@noble/hashes` 2.4, `@scure/base` 2.4, `@scure/btc-signer` 2.4, `borsh` 2.0; dev dependencies `typescript` 5.9, `esbuild`, `playwright-core` |

When introducing a dependency outside the table above, record the reason in `docs/DECISIONS.md` (D-008 and later entries record the existing additions).

Determinism rules for consensus code (the top of every consensus crate's `lib.rs` copies the same `#![deny(...)]` list verbatim): no floating-point arithmetic, no system time (time is passed in by the caller), no dependence on
the iteration order of HashMap / HashSet (use BTreeMap / BTreeSet); no `unwrap` / `expect` / `panic!` / `unreachable` / `todo` / out-of-bounds indexing / unchecked integer
overflow or lossy conversions; errors are always returned and the block or op judged invalid; external input is length-checked before decoding and must be fully consumed; the execution layer distinguishes "failed op", "invalid block" and
"local fault". The workspace sets `unsafe_code = forbid` globally (the contract SDK uses `deny`, allowing unsafe only at memory allocation and host calls).

## 13. Protocol parameter defaults

All parameters are defined centrally in `ksana-primitives::params` (`Params`), distinguished by network: devnet (`DEVNET`, running on BTC regtest), signet (`SIGNET`) and
mainnet (`MAINNET`, D-096). signet differs from devnet only in `CHAIN_ID` and `ANCHOR_DEPTH` (D-055, D-056); mainnet differs from devnet in `CHAIN_ID`,
`NATIVE_CONFIRMATIONS`, `ANCHOR_DEPTH`, `POW_LIMIT_BITS` and the four KSA issuance figures (D-089); "same" in the table means the same as devnet. The rationale and
status of each value are in §13.1. Node, indexer and CLI select the network with `--chain devnet|signet|mainnet`.

| Parameter | devnet | signet | mainnet | Description |
| --- | --- | --- | --- | --- |
| `CHAIN_ID` | 1001 | 1002 | 1000 | Chain ID, written into every VmOp; no replay between the three networks |
| `MAGIC` | `KSN1` | same | same | Data marker of KSN1 outputs and envelopes |
| `TARGET_BLOCK_TIME` | 5 seconds | same | same | Target fast-lane block interval (per unit of DAA score) |
| `ASERT_HALF_LIFE` | 7 200 seconds | same | same | Half-life of difficulty adjustment, chosen according to the empty-block weight cap (§6.7, D-090); fixed per network, not in the upgrade schedule (D-088) |
| `SETTLEMENT_DEPTH` | 2 blocks | same | same | Fast-lane confirmations required before broadcasting the settlement transaction; confirmations the indexer requires before executing a main-chain block |
| `SETTLEMENT_TIMEOUT` | 6 BTC blocks | same | same | Threshold of the settlement backlog limit (§6.4) |
| `NATIVE_CONFIRMATIONS` | 1 | 1 | 3 | BTC confirmations a native transaction needs before its forced inclusion (the effective confirmation count adds `ANCHOR_DEPTH` on top) |
| `MAX_ANCHOR_LAG` | 1 BTC block | same | same | How many blocks a new block's anchor may lag behind the local BTC tip (beyond `ANCHOR_DEPTH`) |
| `ANCHOR_DEPTH` | 0 | 1 | 2 | How many blocks below the local BTC tip the producer anchors (D-056) |
| `BTC_QUERY_DEPTH` | 6 | same | same | Minimum depth of the BTC block headers a contract may read |
| `BTC_FINALITY_CONFS` | 6 | same | same | Confirmations required to be determined final, and then of the BTC block of that determination to be reported BTC-final (§6.5) |
| `MIN_SETTLEMENT_FEERATE` | 1 sat/vB | same | same | Minimum fee rate of a settlement transaction (commit and reveal each) |
| `BLOCK_GAS_LIMIT` | 100 000 000 | same | same | Gas limit per block (may change with an upgrade) |
| `NATIVE_MAX_GAS` | 1 000 000 | same | same | Gas limit per path C transaction (may change with an upgrade) |
| `MAX_OP_BYTES` | 64 KiB (65 536) | same | same | Maximum encoded length of a single VmOp, including contract code |
| `MAX_BLOCK_DATA_BYTES` | 80 KiB (81 920) | same | same | Upper bound on the total data of a single payload (settlement or republish) |
| `MAX_SETTLEMENT_HEADERS` | 500 | same | same | Maximum number of block headers carried by a single payload (about 25 KB compressed, about 42 minutes of non-empty blocks) |
| `MAX_WASM_MEMORY` | 16 MiB | same | same | Contract linear memory limit |
| `MAX_CALL_DEPTH` | 8 | same | same | Maximum depth of contract-to-contract calls |
| `REORG_WINDOW` | 1 000 blocks | same | same | Number of fast-lane blocks that can be rolled back (a deeper forced switch rebuilds from genesis, D-093; with pruning, the states from the pruning root on are kept as well, D-097) |
| `MEDIAN_TIME_SPAN` | 11 | same | same | Reception rule: the timestamp must exceed the median of this many preceding blocks |
| `MAX_FUTURE_BLOCK_TIME` | 15 seconds | same | same | Reception rule: how far the timestamp may be ahead of local time at most |
| `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` | 14 400 seconds | same | same | Consensus rule: how far the timestamp may be ahead of the anchor BTC block's header time at most (§6.8) |
| `POW_LIMIT_BITS` | `0x207fffff` | same | `0x1f00ffff` | PoW limit (minimum difficulty), also the genesis block's default `bits`; the mainnet target is about 2^240 |
| `POW_ALGORITHM` | `Argon2id` | same | same | Mining algorithm (D-094): the message is the encoded block header, the salt is `"ksana-pow" ‖ CHAIN_ID` |
| `POW_ARGON2_MEMORY` | 2 048 KiB (2 MiB) | same | same | Argon2id memory: the memory one computation per block header occupies, which determines verification time (about 1 ms on an ordinary CPU); fixed per network, not in the upgrade schedule |
| `POW_ARGON2_ITERATIONS` | 1 | same | same | Argon2id passes; fixed per network |
| `POW_ARGON2_LANES` | 1 | same | same | Argon2id parallelism; fixed per network |
| `GHOSTDAG_K` | 16 | same | same | Maximum number of blue blocks in a blue block's anticone |
| `MAX_BLOCK_PARENTS` | 10 | same | same | Maximum number of parents of a non-genesis block |
| `MAX_MERGESET_SIZE` | 128 | same | same | Maximum size of the merge set (including the selected parent) |
| `MERGE_DEPTH` | 720 (blue score) | same | same | Merge depth: a merged block's blue score must not be lower than the selected parent's blue score minus this value, and its selected chain must pass through the merge-depth root this far below the selected parent (§6.3, D-097) |
| `BLOCK_REWARD` | 990 000 000 (9.9 KSA) | same | 420 000 000 (4.2 KSA) | Block reward per reward slot at DAA score 0 (devnet uses the placeholder value of D-074, mainnet follows D-089 and D-105) |
| `REWARD_HALVING_INTERVAL` | 25 228 800 | same | 12 500 000 | Length in DAA score of the first halving period of the block reward (devnet 1 460 days; mainnet about 723 days) |
| `REWARD_INTERVAL_DOUBLINGS` | 0 | same | 2 | How many times the halving period doubles in length, every later period keeping the last length (§8.6, D-105): devnet every period 1 460 days; mainnet periods of about 2, 4 and then 8 years; fixed per network, not in the upgrade schedule |
| `KSA_MAX_SUPPLY` | 10^17 (1 billion KSA) | same | 2.1 × 10^16 (210 million KSA) | Upper bound on the genesis allocation plus all block rewards; on mainnet the upper bound of total rewards is exactly the max supply, with no genesis allocation |
| `BLOCK_SHARE_BPS` | 2 000 (20%) | same | not published (placeholder 0) | Block share in basis points of each reward slot (§8.6, D-089; mainnet's is not published yet, D-134); base value of a rule that varies with the protocol version (§6.9), must not exceed 10 000 |
| `BLOCK_SHARE_END` | 50 457 600 | same | not published (placeholder 0) | DAA score at which the block share ends (exclusive): on devnet two halving periods; mainnet's is not published yet (D-134); base value of a rule that varies with the protocol version |
| `CARRIER_BPS` | 500 (5%) | same | same | Carrier reward in basis points of each reward slot (§8.6, D-095), taken out of the producer's share; its sum with `BLOCK_SHARE_BPS` must not exceed 10 000; base value of a rule that varies with the protocol version |
| `CARRIER_FEE_BPS` | 5 000 (50%) | same | same | Carrier's basis points of the path B sponsored fees (§8.6, D-095), must not exceed 10 000; base value of a rule that varies with the protocol version |
| `CARRIER_REPOST_DELAY` | 2 BTC blocks | same | same | For a republish payload's payee to receive the carrier reward, the block's data must have waited more than this many BTC blocks (§8.6, D-095); fixed per network |
| `MAX_UNLOCK_BATCH` | 1 024 | same | same | Maximum number of positions one unlock entry unlocks |
| `EMPTY_BLOCK_DIFFICULTY` | 12 | same | same | Difficulty multiplier, weight and number of reward slots of the first empty block after a non-empty block (§6.6); fixed per network, not in the upgrade schedule (D-088) |
| `MAX_EMPTY_BLOCK_WEIGHT` | 120 | same | same | Cap on the empty-block weight: the weight of consecutive empty blocks doubles block by block up to it (§6.6, D-090); fixed per network, not in the upgrade schedule |
| `UPGRADES` | empty | same | same | Upgrade schedule (§6.9); none of the three networks has a scheduled upgrade |

Consensus constants outside the parameters: `VMOP_VERSION` 2; `BLOCK_VERSION` 2; `MAX_CALL_FUNDS` 16; the empty root of `entries_root` is `sha256("")`; KSN1 data chunks ≤ 520 bytes;
each settlement transaction's length and weight ≤ 400 000; P2A output 240 sats; `Bip322Simple` signature ≤ 1 024 bytes; VM: `STACK_BUDGET` 256 KiB, stack frame `64 + 16 × value slots`,
tables ≤ 4 096 elements, storage keys ≤ 256 bytes, storage values / call inputs / return values ≤ 65 536 bytes, events ≤ 16 384 bytes, `asset_issue` / `asset_create` meta ≤ 1 024 bytes,
asset names 1–64 bytes, symbols 1–16 bytes, decimals ≤ 18 (D-121),
≤ 64 instances per op, linear memory ≤ 256 MiB in total; see `docs/ABI.md` for the gas table.

Local policy defaults: entry pool minimum sponsored fee 1, capacity 10 000, 64 per account; settlement fee rate 2 sat/vB; hot-wallet cap 1 000 000 sats; funding UTXO pool target 64, each split piece
≥ 100 000 sats; settlement batching on, headers at most 1/2 of a payload, bodies at most 1/4, trigger threshold 1/8, 2 BTC blocks in flight; rescue order window 6, delay 2, quiet-time republish
multiplier 4; P2P asks Status every 3 seconds, redials every 2 seconds, bans for 60 seconds at 3 points, request timeout 30 seconds, token buckets as in §11.1; orphan pool 256; BTC polling 500 ms; devnet nodes
`--min-block-interval-ms 5000`; pruning on, margin `PRUNE_BTC_MARGIN` 144 BTC blocks, step `PRUNE_STEP` 1 000 blue score, at `min_pruning_depth = 3 × (MERGE_DEPTH + GHOSTDAG_K) + 1` = 2 209 below the pruning
root (§6.6, §10.2, D-097).

### 13.1 Parameter rationale

Meaning of the statuses: **settled** -- the project owner's decision or the original spec text, the rationale being the decision itself; **verified** -- tests, measurements or simulations support the value; **unverified, to be measured** -- the value
rests only on estimation or analogy and should be measured before mainnet (the work items before mainnet in §14); **default value, pending the owner's confirmation** -- a value D-096 selected for mainnet that the project owner had not yet explicitly confirmed (§17.2),
and confirming it changes no code; the project owner confirmed all six such values on 2026-10-06 (§17.2), so none carries this status any more. The rationale cites only measurements, tests and analyses already in the repository; where there are none, this is stated as is, nothing is made up. The mainnet genesis `bits` is not a parameter; the genesis config fills it in according to the expected hashrate
(`mainnet/README.md`).

| Parameter | Mainnet value | Rationale | Status |
| --- | --- | --- | --- |
| `CHAIN_ID` | 1000 | Only needs to differ between the three networks; devnet 1001 and signet 1002 are taken (D-055) | Settled (confirmed by the project owner, 2026-10-06) |
| `MAGIC` | `KSN1` | Marker of the data format, frozen together with the format of §7.1 (D-032) | Settled |
| `TARGET_BLOCK_TIME` | 5 seconds | Changed back from 1 second by the project owner on 2026-10-02 (D-075: with 1-second blocks the BTC fees of the settlement transactions are unaffordable); convergence test `ksana-consensus/tests/convergence.rs`: after convergence, the standard deviation of the average interval over a 1 000-block window is about 0.07 seconds | Settled; verified (simulation) |
| `ASERT_HALF_LIFE` | 7 200 seconds | D-090: scaled up with the empty-block weight cap -- at the cap each empty block takes about 600 seconds, and the ratio of half-life to single-block time is the same as D-084's 600 seconds / 60 seconds; with only empty blocks the stationary standard deviation of the schedule error is about 1 800 seconds and the difficulty jitter about ±19% (`convergence.rs`, D-090); the cost is a 12× slower response to hashrate changes; together with `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` it lets a single block enlarge the next block's target by at most 4× (`timestamp_attack.rs`); fixed per network (D-088) | Settled; verified (simulation) |
| `SETTLEMENT_DEPTH` | 2 blocks | The value for 5-second blocks (D-075): the settlement transaction is broadcast after about 10 seconds; the confirmations the indexer waits for before executing a main-chain block (finalized blocks are not waited for, D-082); in the 5-hour soak test the indexer and the 3 nodes agreed height by height (M22) | Settled; verified (devnet) |
| `SETTLEMENT_TIMEOUT` | 6 BTC blocks | Original spec text (§6.4, D-046): when a block's data cannot land on BTC within about 1 hour, the fast lane produces only empty blocks and forced inclusions; behavior under real fee spikes has not been tested | Settled; unverified, to be measured (signet observation) |
| `NATIVE_CONFIRMATIONS` | 3 | The mainnet value §13 originally suggested (D-036): forced inclusion after 5 confirmations in total with `ANCHOR_DEPTH` 2 (about 50 minutes); a BTC reorg that moves an already included native transaction (5 blocks or more) takes the blocks that included it off the main chain, and it is included again on the new branch, in its order (D-104, §7.4); devnet / signet use 1 | Settled (confirmed by the project owner, 2026-10-06) |
| `MAX_ANCHOR_LAG` | 1 BTC block | D-035: a new block's anchor may lag behind the local tip by at most 1 block (beyond `ANCHOR_DEPTH`), so that all nodes see almost the same BTC view | Settled |
| `ANCHOR_DEPTH` | 2 | D-056: anchor to a block below the tip so that the anchor block is not reorged out; signet 1, mainnet 2 -- one block deeper than the most common 1-block reorg; a deeper reorg takes the blocks anchored on the orphaned BTC blocks off the main chain (D-104), and the recorded natural BTC reorgs are 1 or 2 blocks deep; going deeper would only delay path C and everything else that reads BTC (about 10 minutes per block) | Settled (confirmed by the project owner, 2026-10-06; kept on 2026-10-08 with D-104) |
| `BTC_QUERY_DEPTH` | 6 | Contracts can only read BTC block headers with 6 or more confirmations (§9.4), the BTC 6-confirmation convention | Settled |
| `BTC_FINALITY_CONFS` | 6 | D-082 (the project owner's decision): taking back a BTC-final block requires reorging 6 BTC blocks, the BTC 6-confirmation convention. A determination rests on no heavier chain having appeared by the BTC block that made it, so a reorg of that block alone with a heavier chain revokes it; since D-114 a block is reported BTC-final only once that BTC block has 6 confirmations too (without a heavier chain, a reported block leaves the finalized chain only when a reorg of 11 BTC blocks removes its data; BTC-final after about 110 minutes instead of 60) | Settled |
| `MIN_SETTLEMENT_FEERATE` | 1 sat/vB | Consensus floor = BTC's minimum relay fee rate; the actual fee rate is local policy (default 2 sat/vB) | Settled |
| `BLOCK_GAS_LIMIT` | 100 000 000 | The order-of-magnitude design of D-013: a storage write is about 10^4 gas, so the number of storage writes per block is bounded (≤ 10^4); the execution time per unit of gas has not been calibrated on real hardware; adjustable via the upgrade schedule (D-076) | Settled; unverified, to be measured (gas table calibration) |
| `NATIVE_MAX_GAS` | 1 000 000 | D-036: a forced-inclusion entry counts `min(max_gas, NATIVE_MAX_GAS)` toward the block's gas, limiting how much of the fast lane path C can crowd out; adjustable via the upgrade schedule | Settled; unverified, to be measured |
| `MAX_OP_BYTES` | 64 KiB | D-005: the limit of a single VmOp (including contract code); the example contracts (token, amm, amm-factory, ksa-exchange) are all far below it | Settled |
| `MAX_BLOCK_DATA_BYTES` | 80 KiB | D-005: below the roughly 100 kvB limit of a BTC standard transaction; since D-081 the payload is in the witness, and 80 KiB is roughly 20 kvB | Settled |
| `MAX_SETTLEMENT_HEADERS` | 500 | Measured in D-075: a compressed empty-block header is 46 bytes (50 when ASERT adjusted `bits`), a block with entries about 80 bytes, 500 of them about 25 KB, about 42 minutes of non-empty blocks | Verified (measured encoding lengths) |
| `MAX_WASM_MEMORY` / `MAX_CALL_DEPTH` | 16 MiB / 8 | Conservative values from D-013 / D-073; enough for the example and test contracts; larger contracts have not been tested | Settled; unverified, to be measured |
| `REORG_WINDOW` | 1 000 blocks | D-075 (about 83 minutes at 5-second blocks); only determines how many state versions are kept: a deeper forced switch rebuilds from genesis (D-093); a pruning node also keeps every version from the pruning root on, so the switches finality allows never need that (D-097) | Settled (confirmed by the project owner, 2026-10-06) |
| `MEDIAN_TIME_SPAN` | 11 | Reception rule, the BTC median time (BIP113) convention | Settled |
| `MAX_FUTURE_BLOCK_TIME` | 15 seconds | Reception rule, 3 block intervals; only affects reception, not part of consensus | Settled |
| `MAX_TIMESTAMP_AHEAD_OF_ANCHOR` | 14 400 seconds | D-083: a BTC block header's time itself may be up to 2 hours ahead, plus a margin for the anchor lagging behind the tip and for block production; `timestamp_attack.rs`: a single block can enlarge the target by at most 2^(14 400 / 7 200) = 4×, and over-reporting by 1 second makes it invalid | Settled; verified (tests) |
| `POW_LIMIT_BITS` | `0x1f00ffff` | Target about 2^240, at the limit one block is expected per 65 536 Argon2id computations; D-094 measured about 0.98 ms per verification and about 0.86 ms per solve attempt, so about 13 CPU cores suffice for one block per 5 seconds; a floor that is too hard stalls a network with too little hashrate, while one that is too easy is only a matter of ramping up from the genesis `bits` early after launch, so the easy one is chosen; the mainnet hashrate is unknown, and the genesis `bits` is filled in according to the expected hashrate | Settled (confirmed by the project owner, 2026-10-06) |
| `POW_ALGORITHM` and the Argon2id parameters | Argon2id, 2 MiB / 1 pass / 1 lane | D-094 (the project owner's decision): measured about 1 ms per block header, verifiable in a browser; the GPU advantage is estimated at 10–30× from GPU memory bandwidth, not measured; the retuning path of D-094 is to tune the memory and passes before mainnet by the measured GPU advantage and sync cost | Algorithm settled; parameters carried over from devnet, confirmed by the project owner (2026-10-06); GPU advantage unverified, to be measured |
| `GHOSTDAG_K` | 16 | D-063 set 8 for 5-second blocks by Kaspa's method k = min{k : P[Poisson(2Dλ) > k] < δ}, with λ = 0.2 blocks / second and propagation delay bound D = 5 seconds, 2Dλ = 2: P[Poisson(2) > 8] ≈ 2.4 × 10⁻⁴ (δ = 10⁻³ gives 8, δ = 10⁻² gives 6), the same method by which D-075 computed 18 for 1-second blocks. D-100 raised it to 16 against withholding a heavy empty block (§16, §17.4): the honest weight-1 blocks that a withheld empty block of weight 12 can overtake then fit in a blue anticone. At δ = 10⁻³, 16 tolerates a propagation delay bound of about 17.6 seconds (8: about 6.1 seconds); the cost is coloring work (up to K + 1 blue blocks per merge set) and 24 more blocks of pruning depth (D-097); the propagation delay of a real network has not been measured | Settled; unverified, to be measured (propagation delay) |
| `MAX_BLOCK_PARENTS` | 10 | D-063, the same as Kaspa | Settled |
| `MAX_MERGESET_SIZE` | 128 | D-063: bounds the merge-execution work per block; the 5-hour soak test merged 42 parallel blocks in total, nowhere near the limit | Settled; verified (devnet scale) |
| `MERGE_DEPTH` | 720 (blue score) | D-063: about 1 hour at 5-second blocks, the same as Kaspa's 1 hour; since D-097 it also places the merge-depth root, and with `GHOSTDAG_K` sets the pruning depth (2 209 blue score); the pruning tests run with small values (random DAGs with `GHOSTDAG_K` 3 and `MERGE_DEPTH` 20; the node and indexer tests with 2 and 4), the bound itself holds for any values | Settled |
| `BLOCK_REWARD` / `REWARD_HALVING_INTERVAL` / `REWARD_INTERVAL_DOUBLINGS` / `KSA_MAX_SUPPLY` | 4.2 KSA / 12 500 000 / 2 / 210 million KSA | D-089 and D-105 (the project owner's decisions): halving periods of 12 500 000, 25 000 000 and then 50 000 000 DAA score (about 2, 4 and then 8 years at one block per 5 seconds), so the subsidy that pays for landing data on BTC lasts decades longer; the upper bound of total rewards is exactly the max supply, no genesis allocation; the tests in `params.rs` sum exactly (exactly 4 KSA short of the cap, 0 from period 29, about year 220) | Settled; verified (summation test) |
| `BLOCK_SHARE_BPS` / `BLOCK_SHARE_END` | Not published (placeholders 0 / 0) | D-089: a part of each slot below the end point is credited to the block share account, splitting only the block reward and not the sponsored fees; the two values are not published yet, and a mainnet node does not start until they are filled in (D-134); the receiving wallet is the only open item (§17.1) | Settled; not published |
| `CARRIER_BPS` / `CARRIER_FEE_BPS` / `CARRIER_REPOST_DELAY` | 500 (5%) / 5 000 (50%) / 2 | D-095 (the project owner's decision); D-095 "to be observed": whether the fixed part tempts producers to attach their own settlement to every block, and the two ratios are retuned via the upgrade schedule from signet observations | Settled; unverified, to be measured (signet observation) |
| `MAX_UNLOCK_BATCH` | 1 024 | D-074: the maximum number of positions one unlock entry unlocks, bounding the unlock writes of a single block | Settled |
| `EMPTY_BLOCK_DIFFICULTY` / `MAX_EMPTY_BLOCK_WEIGHT` | 12 / 120 | D-084 (N = 12) and D-090 (cap 120, the project owner's decision): the idle header interval doubles from about 60 seconds to about 10 minutes, while the time per unit of DAA score still converges to 5 seconds; node and consensus tests cover weight, work, DAA score and reward slots | Settled; verified (tests) |
| `UPGRADES` | empty | No scheduled upgrade (D-076) | Settled |
| Genesis `bits` (config, not a parameter) | By expected hashrate | D-027: the genesis `bits` can be configured harder than the limit, and ASERT uses it as the anchor; with a 7 200-second half-life the ramp-up from the minimum difficulty is very slow | To be filled in by the project owner at genesis |


## 14. Implementation milestones

M0 through M5 form a demonstrable MVP; from M6 on, items are extended one by one as the project owner decides; the next milestone starts only after all acceptance criteria of a milestone pass (including CI). Progress and the
acceptance details of each milestone were kept in a development log that is not published; `docs/HISTORY.md` summarizes the history before the first public commit.

| Milestone | Content | Status |
| --- | --- | --- |
| M0 Project skeleton | workspace, CI (fmt / clippy / test), docker-compose for devnet; `ksana-primitives` and `ksana-crypto`; shared vectors | Done |
| M1 VM and state | `ksana-vm`, `ksana-state`, `ksana-execution`, the built-in Account / Asset modules, contract SDK, example token contract, `docs/ABI.md`; two-process determinism test | Done |
| M2 Single-node fast lane | `ksana-consensus` (PoW, ASERT), `ksana-mempool`, single-node `ksana-node`, basic RPC; path B; restart recovery | Done |
| M3 BTC integration and settlement | `ksana-btc`: anchoring, construction / signing / verification of settlement transactions and delayed broadcast, P2A, scanning and forced inclusion for path C, unlock entries | Done |
| M4 Multi-node network | `ksana-p2p`, 3-node devnet, block and op broadcast, fork choice and reorg rollback | Done |
| M5 Indexer and exception handling | `ksana-indexer`, rescue settlement, settlement backlog limit, header republishing | Done |
| M6 SDK and wallet | TypeScript SDK, BIP322 simple, Unisat / Xverse adapters, `ksana_estimateFee`, signet parameters and demo page, CLI | Done (the path A acceptance with real wallets on signet became void with the deletion of path A) |
| M6.1 Anchoring and rescue hardening | anchor depth and anchor-invalid historical blocks, rescue order and deduplication | Done |
| M6.2 Security requirements check | peer rate limiting, funding wallet separate from revenue wallet, cross-check against §16 | Done |
| M7 DAG consensus | GHOSTDAG block DAG, block header v2, merge execution, sync protocol v2, decoupled mining algorithm | Done |
| M8 Contract model aligned with the EVM | attached funds, catchable calls, delegate calls, contracts creating contracts, execution environment, runtime stack metering, AMM built-in asset pairs and factory | Done |
| M9 KSA issuance and block rewards | max supply, reward table, blue block rewards, cumulative unlock, `ksana_getAsset` | Done |
| M10 Block interval | changed to 1 second, then back to 5 seconds; header compression in payloads and the funding UTXO pool kept | Done |
| M11 Network upgrade mechanism | upgrade schedule activated by DAA score, rules varying with the version, handling of unknown versions, `ksana_getChainInfo` | Done |
| M12 Settlement batching | one settlement transaction carrying the headers and bodies of several blocks, body republishing | Done |
| M13 Deprecating path A | path A and fee debt removed; `native_tx`, example exchange contract, top-up (SDK and CLI) | Done |
| M14 Data compression | empty blocks do not change the state root, entry compression, Taproot witness envelope (commit / reveal pair) | Done |
| M15 Convergence | BTC-anchored finality, timestamp anchor bound, two-speed difficulty, exchange lock flow, path C signature bound to the first input; documentation and adversarial tests; consensus frozen, SPEC v2 consolidated, merged into `main` | Done |
| M16 Increasing empty-block difficulty | empty-block streak, weight cap 120, half-life 7 200 seconds (D-090); competing-chain republish trigger (D-091); block share (D-089) | Done |
| M17 Accounts are Taproot addresses | account = BIP86 output key, conversion between accounts and addresses, BIP322 accepts only account addresses (D-092) | Done |
| M18 Implementation limit cleanup | deep switches and crash recovery rebuild from genesis, missing blocks rebuilt from BTC payloads (D-093) | Done |
| M19 Adversarial tests | the verification gap of §15: adversarial tests of rule boundaries and malicious inputs | Done |
| M20 / M21 Web wallet and block explorer | the web wallet moved onto `main` (D-064 to D-071; now the ksana-web repository) and the UI/UX wrap-up | Done |
| M22 Mining algorithm changed to Argon2id | CPU-friendly Argon2id, shared vectors, SDK verification (D-094) | Done |
| M23 Carrier reward | carrier determination, three-way split of reward slots, republish payee, local-policy patches to the entry pool (D-095) | Done |
| M24 Consensus freeze | the project owner's sign-off, SPEC v2 final, pending-confirmation items closed out, mainnet parameters and genesis config template (D-096) | Done |
| M25 Bounded node memory | memory of every growing structure measured, merge-depth root rule, GHOSTDAG pruning with checkpoints, pruning in the node and the indexer, the aarch64 determinism job over every crate that computes consensus results (D-097) | Done |
| M26 Security audit | the two adversarial items M19 left (payment reversal and withholding against heavy empty blocks, the cost of lock-without-pay), exchange partial claims, the internal audit of `main` and its local fixes; consensus fixes proposed (D-098) | Done (the consensus fixes landed in M27) |
| M27 Consensus fixes | the audit's consensus fixes the project owner adopted: the fork choice counts a tip's own work, a body fits in a payload, forced-inclusion entries of anchor-invalid and merged blocks, payloads whose headers anchor at or above their own BTC height, `MAX_SETTLEMENT_HEADERS` when decoding, the `nSequence` of settlement transactions, `GHOSTDAG_K` 16; devnet / signet restart from genesis (D-100) | Done |
| M28 Only valid blocks become BTC-final | the finality determination asks for the verdict of executing a candidate on top of the finalized tip and excludes an invalid one (D-101); the node's BTC fees follow `estimatesmartfee` within a floor and a cap, CPFP bumps through the P2A output, a block's own waiting settlement treated like a claim, the next-block rate while a competing chain is on BTC (D-102); devnet / signet restart from genesis | Done |
| M29 Deep BTC reorgs | a BTC reorg deeper than `ANCHOR_DEPTH` takes the blocks anchored on the orphaned BTC blocks off the main chain, in the node and the indexer alike, instead of stopping finality; the BTC reorg sweep and BTC reorgs in the soak test (D-104); devnet / signet restart from genesis together with M28 | Done |
| M30 Halving periods that double | `REWARD_INTERVAL_DOUBLINGS`: the halving periods double in length twice and then stay; mainnet 4.2 KSA per slot over periods of about 2, 4 and then 8 years, the block share and the max supply unchanged (D-105); devnet / signet unchanged | Done |
| M32 Public signet | Ksana on Bitcoin's public signet: the relay probe of both data carriers, one hosted server deployed over ssh (seed node, indexer, web wallet, BTC proxy, KSA faucet), the guide for joining; real wallets on paths B and C; four weeks of observation before mainnet (D-107) | In progress |
| M38 BTC-final waits for the BTC block of the determination | a block is reported BTC-final once the BTC block that determined it has `BTC_FINALITY_CONFS` confirmations, so taking back a reported block takes a BTC reorg of at least `BTC_FINALITY_CONFS` blocks; a height the determination paused at is determined again when it resumes (D-114) | In progress |
| M42 Issuer capabilities of built-in assets | the issuing contract of an asset declares at creation which of mint, freeze, allowlist and pause it holds and can only give them up; every movement of the asset obeys them, none moves a balance (no clawback); asset symbols; `ksana_getAssetStatus` (D-121); devnet / signet restart from genesis | In progress |

M15 acceptance criteria: the finality determination is a deterministic function of the BTC chain data (incremental determination agrees with determination from scratch; a heavier chain that lands on BTC in time wins and a late one is ignored; pause when it cannot be determined; a BTC reorg revokes
the determination); a node's finalized blocks are not reorged by a late heavier chain, a node that chose another chain switches over, a node without the finalized blocks requests them from peers; the indexer executes finalized blocks without waiting for confirmations
and stops with an alert when a finalized block is invalid; a timestamp exactly at the bound is valid and beyond it invalid; `finalized` and `btc_final` in RPC / SDK; two-speed difficulty (effective target, work, DAA score,
reward slots, the same `bits`, empty-block throttling, node and indexer state roots agree); exchange lock flow end to end; a path C op copied into another transaction fails with an invalid signature and does not consume
the nonce, a coinbase is not a native transaction, the SDK and CLI sign by the first input; everything passes locally and in CI; documentation revised.

### Work items before mainnet (no consensus change, to be scheduled by the project owner)

The consensus is frozen (D-096). Before the mainnet launch this non-consensus work remains, in an order set by the project owner (the pruning point, which used to be listed here, was done in M25; it needed the
merge-depth root rule, a consensus change the project owner approved, D-097):

- The block share: its values published in `MAINNET` (D-134) and its receiving wallet (the only open item, §17.1); the mainnet genesis config: timestamp, anchor, genesis `bits` by expected
  hashrate (`mainnet/README.md`).
- Run paths B and C end to end with real wallets (Unisat / Xverse) on the public signet (`docs/SIGNET.md`, M32).
- The measurements marked "unverified, to be measured" in §13.1: the GPU advantage of Argon2id (the retuning path of D-094), the propagation delay of a real network (`GHOSTDAG_K`), the calibration of the gas table
  (`BLOCK_GAS_LIMIT`), and the observation of `SETTLEMENT_TIMEOUT` and the carrier reward on signet; a simulation of the BTC fee market against the landing race of §6.5 (D-101, D-102).
- The node's fee policy (local policy): for the floor paid while the mempool has room (D-117's limitations), a bump after one missed BTC block to the next-block estimate, a memory of a pair the room
  misjudged, and a reading of the room that is not biased low (`getblockstats`). P2A pinning (D-102's limitations) is closed (M48, D-131).
- Security audit (§16): the internal audit is done (M26, D-098) and the consensus fixes the project owner adopted have landed (M27, D-100); the path assessment's finding 1 and the second audit's A2-09 are fixed
  (M28, D-101, D-102; A2-09's pinning residual in M48, D-131), and so is D-101's known limitation (M29, D-104); the second audit's A2-08, entry-pool churn, is fixed as local policy (M47, D-130), and so are the first audit's F-28 and F-29, the limits of the P2P layer and the RPC servers (M49, D-132); an external audit follows. Withholding a heavy empty block (§16) is narrowed by `GHOSTDAG_K` 16; what remains on an idle chain is in §16 and §17.4.
- Update the web explorer's parameter table and settlement page per D-095.

Research items (after mainnet, §17.3): inclusion lists, a settlement funding pool, signature half-aggregation and a global dictionary by account index, forced-inclusion entries carrying the VmOp,
syncing new nodes from a state snapshot at the pruning point, Kademlia node discovery, interoperability with Ethereum. Do not start implementing these items before they are settled.

## 15. Test plan

Testing focuses on determinism and exception paths: any problem that makes state diverge between nodes, or between a node and the indexer, is a top-priority bug.

- **Unit tests**: every crate covers the normal and boundary paths (consensus: algorithm, Argon2id with the shared vectors, ASERT with the official vectors, compact, convergence, GHOSTDAG (including the merge-depth root), pruning, PoW, timestamps; BTC: anchored view, compression
  comparison, data scripts and envelope, native transactions, settlement transactions, mock chain, finality, the pruned structures against unpruned ones; execution: builder, contracts, environment, attached funds, merge, `native_tx`, random sequences, rewards, rules,
  subcalls, unlock, upgrade; node: anchoring, fork, network, fees, algorithm, DAG, rewards, payload, funding pool, upgrade, batching, finality, restart, block interval, pruning; indexer (including pruning); entry pool;
  P2P codec and network; state store interface and golden roots; VM execution, stack and validation). Encoding, signatures and PoW use the fixed vectors in `tests/vectors/`, checked one by one by Rust and the TS SDK.
- **Adversarial simulations** (`ksana-consensus/tests/reorg_race.rs`, `selfish_mining.rs`, D-098): reversing a fast-confirmed payment with a private branch, and withholding weight-1 blocks or a heavy empty block, on the real
  fork choice and GHOSTDAG (D-100); `cargo test` runs quick assertions, the full tables, which §10.4 and §17.4 summarize, are printed with `--ignored`.
- **BTC reorg sweep** (`ksana-indexer/tests/deep_reorg.rs`, D-104): a producing node, an indexer and a node syncing from genesis afterwards go through BTC reorgs of 1 to `ANCHOR_DEPTH` + 2 blocks and of 6
  and 7, at anchor depths 0 to 3, with every kind of content on the orphaned BTC blocks and BTC going back to the old branch, and must agree on the main chain and the state roots, with finality going past
  the retreat; and reorgs of the BTC block that determined a block final, whose new branch confirms a heavier chain forking below it, of 1 block right after the determination and of
  `BTC_FINALITY_CONFS` − 1 and `BTC_FINALITY_CONFS` blocks once the block is reported BTC-final (D-114); `cargo test` runs the boundaries and random cases, the full sweep runs with `--ignored`.
- **Property tests** (proptest): encoding round trips and canonicality (including strategies for compact headers, compact entries and bodies); a random entry sequence executed twice yields the same `state_root`;
  the incremental finality determination equals determination from scratch, also through pauses for verdicts and BTC reorgs replacing the height a pause stopped at, and no BTC reorg
  of fewer than `BTC_FINALITY_CONFS` blocks takes back the BTC-final tip reported (D-114); the header DAG ends on the same main chain whatever the order of the headers and the BTC views it sees on the way (D-104);
  a DAG pruned at `min_pruning_depth` below the finalized tip computes every later block of random DAGs (with and without attacker
  blocks) exactly as the unpruned DAG, and so does one restored from its checkpoint (D-097).
- **Determinism test** (`ksana-execution/tests/determinism.rs`, `harness = false`): a fixed block sequence (built-in asset transfers, path C, contract deployment and calls, trap,
  gas exhaustion, failed transfers, empty blocks, attached funds, catchable calls, delegate calls, contract creation, asset burning, runtime stack metering) is replayed in the parent process and two differently configured child processes (serial
  compilation / parallel compilation with a multi-threaded shared cache); the `state_root` and receipt digests agree byte for byte and are compared against the golden values; CI runs it once each on x86_64 and aarch64
  (on aarch64 CI runs the tests of every crate that computes consensus results -- `ksana-primitives`, `ksana-crypto`, `ksana-state`, `ksana-vm`, `ksana-execution`, `ksana-consensus`, `ksana-btc` -- including the Argon2id PoW values
  against the shared vectors, D-094, D-097).
- **Integration tests** (`tests/integration/`, regtest, `run.sh`: build the TS SDK → compose up → tests → down): the per-milestone `m*_regtest.rs` and `spec15_regtest.rs`
  cover the scenarios in the table below, including the TS SDK's end-to-end scripts (path B / C, top-up, BIP322 signing with the mock wallet) and the wallet demo page check in Chromium.
- **devnet**: `devnet/smoke.sh` (CI's devnet job): 3 nodes converge, the CLI submits a path B transfer, the indexer's and nodes' `state_root` agree at every height; `devnet/soak.sh`
  soak test: randomly submits path B / C ops (signature scheme randomly BIP340 / BIP322), continuously checks the three nodes and the indexer, counts merged parallel blocks, and with `KSANA_SOAK_REORGS_PER_HOUR`
  makes random BTC reorgs of 1 to 3 blocks, after each of which finality must pass the fast-lane tip of that moment within 15 minutes (D-104), and an op a reorg leaves behind a nonce gap (§7.4) must be
  included once the soak test fills the gap, or its nonce filled too when the entry pool dropped or refused it; on a GitHub-hosted runner 5 hours (18 000 seconds, job limit 350 minutes, D-061); longer runs are done on your own machine.
- **Fuzzing** (`fuzz/`, cargo-fuzz, nightly): the four targets `vmop_decode`, `payload_decode`, `btc_inputs`, `host_params`; besides not crashing, they check canonical encoding,
  consistent parsing, identical results across two executions and gas not exceeding the limit; CI runs each target for 60 seconds from the seed corpus.
- **TS SDK**: `npm run lint && npm run build && npm test` (Node 22), checks the shared vectors and the official BIP322 vectors.
- **Contract workspace**: fmt / clippy / test on the native target (mock host) and the wasm32 target.

| Scenario | Expected result |
| --- | --- |
| The same nonce goes through path B and path C at the same time | The one included first takes effect, the other is recorded as failed (when path B comes later it is no longer included, and the node removes it from the entry pool) |
| The same op is included by two parallel blocks | Only the position that executes it takes effect; the receipt at the other is `skipped`, with no fee charged |
| An unlock entry's target is outside the unlockable segment, already unlocked, or the block is not yet in the settled closure | A main-chain block cannot go on-chain; a merged block's unlock entry is skipped whatever its target (D-100) |
| The producer goes offline after broadcasting a block | Other nodes broadcast the settlement transaction on its behalf, or initiate a rescue settlement in order |
| The producer's funding input is spent | The rescue settlement rebuilds it with new funding, the payload unchanged; the indexer goes by the first one confirmed |
| Two producers produce blocks at the same time | Both are merged; no settlement is broadcast before the depth is reached; the lighter side's settlement lands on BTC as usual after the merge, and no two transactions spend the same output |
| BTC fee rates spike and the settlement transaction is stuck in the mempool | Its producer bumps it through the P2A output (CPFP) after 2 BTC blocks and it lands on BTC with the child; other nodes rescue it in their turn when it pays less than their rate or has waited `CLAIM_PATIENCE` BTC blocks (D-102); when another party's large low-rate child holds the P2A output first (pinning), the payload goes out again in a new pair at the bump's rate (D-131); a settlement in flight for more than 2 BTC blocks no longer blocks the next batch |
| A BTC reorg takes out the BTC block that confirmed a settlement | The settlement transactions land on BTC again, and the block they settle, anchored below, keeps its place and state; a reorg of at most `ANCHOR_DEPTH` blocks moves no fast-lane block at all; the finality determination is recomputed on the new branch |
| A BTC reorg of more than `ANCHOR_DEPTH` blocks orphans anchors: with empty blocks, a path B op, a path C op whose native transaction confirms again or is double-spent, settlement transactions waiting in the BTC mempool or landed on the old branch, and BTC going back to the old branch | The main chain retreats below the blocks anchored on the orphaned BTC blocks (or merging such a block) and grows again on the new branch: the path B op is included again, the path C op again or not at all, and the waiting transactions keep their funding inputs; the nodes that had the blocks, a node syncing afterwards and the indexer choose the same main chain, and finality passes the retreat; no final block leaves the main chain for a reorg of at most `BTC_FINALITY_CONFS` blocks whose new branch carries no heavier chain; when BTC goes back, so do the blocks (D-104) |
| A contract reads a BTC block header of insufficient depth | The call fails and the state is rolled back |
| The settlement payload disagrees with the block contents, the body is tampered with | That copy is rejected; the body is self-certified by `entries_root`, and the indexer ignores candidates that do not match |
| A settlement has not landed on BTC after `SETTLEMENT_TIMEOUT` | The fast lane does not roll back; subsequent blocks may contain only forced-inclusion entries, and it recovers once the backlogged bodies land on BTC |
| Only empty blocks for a long time, or producers carry few headers | Republish transactions land the headers and bodies on BTC; the settled closure and the indexer keep advancing |
| A late heavier chain tries to reorg BTC-final blocks | Neither nodes nor the indexer re-select; a node that chose another chain is forced to switch |
| A BTC reorg of the BTC block that determined a block final, whose new branch carries a heavier chain forking below the block | Right after the determination the block leaves the finalized chain and the heavier chain becomes final, on the node and the indexer alike, while neither had reported the block BTC-final; a reorg of `BTC_FINALITY_CONFS` − 1 blocks once it is reported takes nothing back, also while the new branch is shorter for a while; one of `BTC_FINALITY_CONFS` blocks takes it back with an alert (D-114) |
| A BTC reorg replacing the BTC block at which the determination paused, whose new block changes what is determined there | What was determined at that height is revoked and determined again, and the result is the same as determining from scratch (D-114) |
| A lighter competing chain appears on BTC while this node's headers have not landed yet | The node immediately republishes its own headers up to the main-chain tip (D-091); the other side's blocks are not finalized when they reach the confirmation count, and what is finalized is this node's heavier chain |
| The timestamp exceeds the anchor BTC block time plus 4 hours | Invalid block (exactly at the bound is valid) |
| An empty block only meets the non-empty block target | Invalid block; empty blocks are produced at w times the difficulty (w doubles with the streak from N to N_MAX), and ASERT gives both kinds of block the same `bits` |
| The block header version is higher than this software knows | Rejected with an alert, without penalizing the peer; syncs as usual after upgrading |
| A block merges a long low-work side chain forked from an old block | Invalid (the side chain misses the merge-depth root); the check goes no lower than the root's blue work instead of walking the history |
| A node and the indexer run long enough to prune; a node restarts; a new node syncs from a pruned peer | Memory stays bounded by the window; the restart continues from the checkpoint; the new node syncs from the genesis block and agrees block by block; a pruned and an unpruned node judge every block alike (D-097) |
| Paying to a lock that is already claimed / released | The claim fails (the SDK and CLI refuse before paying); a lock can still be claimed within 6 BTC blocks after expiry |
| The seller pays a few sats to its own lock just before the buyer's payment is force-included | The seller's claim buys only its share, for the beneficiary; the rest stays locked and the buyer's payment buys it (D-098) |
| A block with an invalid body lands on BTC with two headers on top of it | The indexer disqualifies it, re-selects the main chain and keeps agreeing with the nodes (D-098) |
| An invalid block (wrong `state_root`, invalid body) lands on BTC first and its data reaches `BTC_FINALITY_CONFS` confirmations | It is excluded from the finality determination with the blocks built on it, and the honest chain becomes final; the same on a running indexer, a fresh one and a node that never had the block (D-101) |
| A block whose anchor names no BTC block lands on BTC first and gets `BTC_FINALITY_CONFS` confirmations, with its body (wrong `state_root`) or without it, or merged by a landed block | It, or the block merging it, is excluded for the anchor before anything else is asked, and node and indexer finalize the honest chain exactly as when the anchor hash is real (D-101) |
| A peer hands over, through sync, a block anchored off the BTC chain: an empty block on an old main-chain block with a made-up anchor hash, an attacker's block on the tip merging it, a block on an idle tip with a made-up anchor hash, or one on the tip anchored on a stale BTC block | The honest producer never merges it or builds on it, and node and indexer finality goes on exactly as when the anchor is real (D-101) |
| A peer sends junk blocks: a parent far below the merge depth, orphans without valid PoW, huge orphans | Rejected before any ancestor query; orphans are checked before they are kept and the pool is capped by bytes (D-098) |
| Many accounts fill the entry pool with ops behind nonce gaps | The pool evicts the ops that stand lowest (behind a nonce gap first) instead of rejecting newcomers; new path B ops still get in (D-098) |
| A BTC payload carries hundreds of junk headers and bodies | One copy of the carrying transaction is kept, not one per body (D-098); a payload with more than `MAX_SETTLEMENT_HEADERS` headers is not recognized (D-100) |
| A block that needs a body carries more entries than any payload can fit | Invalid block (D-100) |
| A new block with a forged anchor arrives by sync with a forced-inclusion entry naming a transaction that is not a native transaction carrying its op | Rejected (invalid when the transaction is a native transaction carrying another op), the same however much the node has pruned; a block merging such an entry is invalid (D-100) |
| A block's attached settlement payload carries a header anchored above the block | Invalid block (D-100) |
| A settlement transaction confirms at or below its block's anchor height after a BTC reorg | It does not count; the block is rescued (D-100) |
| A peer delivers the same refused block again and again while syncing | Answered from memory while the BTC tip stays the same, not checked again (D-100 item h) |
| A payload lands with a header anchored at or above the payload's own BTC height | The payload does not count: its headers have not appeared on BTC through it and it lands no body (D-100) |
| A block carries settlement transactions with an input `nSequence` other than `0xFFFFFFFD` | Invalid block (D-100) |

## 16. Threat model and security requirements

The security of fast-lane confirmations comes from the fast lane's own hashrate, and the security of BTC finality comes from BTC: finalized blocks can only be reorged along with a BTC reorg, and a block reported
BTC-final only along with one of at least `BTC_FINALITY_CONFS` blocks (§6.5 item 7). Applications must choose by amount whether to wait for fast-lane depth
or for BTC finality.

| Threat | Impact | Mitigation |
| --- | --- | --- |
| 51% hashrate attack on the fast lane (including short-term rented CPU / GPU hashrate) | Rolls back fast-lane blocks that are not yet BTC-final; renting hashrate (cloud hosts, GPUs) against the independent Argon2id PoW costs far less than against BTC itself | Only blocks not yet finalized can be rolled back: data is final once it has landed on BTC with `BTC_FINALITY_CONFS` confirmations and no heavier chain landed in time (§6.5), and a block is reported BTC-final once the BTC block of that determination has `BTC_FINALITY_CONFS` confirmations too, so with fast-lane hashrate alone a reported block cannot be taken back, and with it a BTC reorg of at least `BTC_FINALITY_CONFS` blocks is needed (D-114); applications wait for BTC finality according to the amount; the timestamp anchor bound limits the difficulty discount a privately mined chain can borrow (§6.8); the concrete form of the PoW is in D-087, D-088, D-094 |
| Reversing a fast-confirmed payment with a minority of the hashrate | A payment accepted on a fast-lane confirmation is reversed by a private branch; far more likely on an idle chain, where honest blocks are few, and at short waits on a busy chain, where one empty block of the attacker outweighs 12 honest blocks as soon as it is found | Applications choose the wait by amount and chain load from the simulated table in §10.4 (D-098, D-100); BTC finality is unaffected |
| Advantage of GPUs / dedicated hardware on Argon2id | Hashrate concentrates in those with dedicated hardware | The 2 MiB memory hardness keeps the GPU advantage to roughly the multiple of its GPU memory bandwidth (an estimated 10–30×, far less than SHA-256 ASICs); hardware with more on-chip memory still has an advantage; the parameters are fixed per network and can be retuned with a network upgrade (D-094) |
| A heavier chain withheld for a long time | Published afterwards, it reorgs blocks already regarded as final and splits nodes and the indexer | Finality only recognizes data that lands on BTC in time, and a heavier but late chain is ignored; the main chain must pass through the finalized blocks |
| A heavier chain together with a reorg of the BTC block that determined a block final (the review of 2026-10-14) | A determination rests on no heavier chain having connected by the BTC block that made it: a BTC reorg of that block alone, its new branch confirming the headers of a heavier chain forking below the final block, revoked the determination for good, one BTC block right after it and one more for each block since, while the docs promised at least `BTC_FINALITY_CONFS`; within reach of a large BTC pool, or one paid to do it, together with a rented fast-lane majority | A block is reported BTC-final only once the BTC block of its determination has `BTC_FINALITY_CONFS` confirmations: taking it back then takes a reorg of at least `BTC_FINALITY_CONFS` BTC blocks, which takes it off the finalized chain only with a heavier chain on its new branch or when it is at least `2 × BTC_FINALITY_CONFS − 1` blocks and removes the block's data, and nodes and indexers alert when a block they reported is taken back; BTC-final comes after about 110 minutes instead of 60 (§6.5 item 7, D-114) |
| Rushing a lighter chain onto BTC first while idle, or while honest settlements wait behind a fee spike | The honest chain's headers have not landed yet, the other side's blocks reach the confirmation count first and become final, and the heavier honest chain is void; with a fixed honest fee rate, a spike above it (natural, or bought by filling BTC blocks) was enough | As soon as a node sees a competing chain appear on BTC it republishes its own headers up to the main-chain tip (§7.6, D-091), paying the next-block fee rate up to its cap; waiting settlements are bumped through the P2A output (§7.7, D-102); once the honest chain is connected on BTC, the candidate with more chain work is taken (§6.5, D-100). What remains is the accepted case: honest data that no operator pays to land for `BTC_FINALITY_CONFS` BTC blocks |
| Timestamp attack | Pushing timestamps into the future to lower the difficulty | Consensus rule: the timestamp does not exceed the anchor BTC block time plus 4 hours, and it cannot accumulate across anchors |
| Spamming empty blocks | Many headers land on BTC, driving up BTC costs or grabbing rewards | Empty blocks are w times as hard, have weight w and are rewarded per slot (w doubles with the number of consecutive empty blocks from N to N_MAX, D-090), so the reward per unit of time equals that of non-empty blocks; the idle header interval doubles to about 10 minutes; empty blocks do not change the state root and are 14 bytes compressed |
| Withholding weight-1 blocks in idle periods (selfish mining against heavy empty blocks) | A miner hides more than `GHOSTDAG_K` cheap blocks while an honest heavy empty block is being found, then publishes them: the heavy block turns red and gets no reward. Before D-100 a 5% / 10% / 20% miner took 30% / 45% / 56% of the honest reward slots at no cost to itself | The fork choice counts a tip's own work (§6.4, D-100): the heavy block wins as soon as it is published, and the withheld blocks arrive too late and turn red. Simulated, the honest loss falls to 0 / 0 / 0.03% / 1.2% (α = 5% / 10% / 20% / 30%), and the attacker loses a tenth to almost half of its own income |
| Withholding a heavy empty block | A miner keeps an empty block private, extends it with weight-1 blocks and publishes before the honest blocks found meanwhile outweigh it: the honest blocks beyond the first `GHOSTDAG_K` turn red. With D-063's K = 8 a 5% / 10% / 20% / 30% miner took 1.5% / 3.6% / 11% / 23% of the honest reward slots on a busy chain and 1.6% / 5.4% / 13% / 20% on an idle one, at no cost to itself, because an empty block outweighed more than K + 1 weight-1 blocks | `GHOSTDAG_K` 16 (D-100): the honest blocks a withheld empty block of weight 12 can overtake fit in a blue anticone. Simulated: 0 / 0.02% / 1.1% / 10% on a busy chain; on an idle chain, where the private chain can still put more than K blocks in the anticone of an honest empty block, 0.50% / 2.9% / 9.6% / 16% remain (§17.4) |
| An asset's issuer freezes, excludes or pauses holders (D-121) | Holders of that asset cannot move it while the issuing contract keeps them from it | Only an asset whose issuing contract declared the capability when it created the asset; capabilities are only given up, never gained, `ksana_getAsset` lists them and `ksana_getAssetStatus` an account's standing, so a holder knows before holding it; no capability moves a balance (no clawback); KSA and assets created without capabilities are never stopped |
| Producers censor certain users | Ops cannot take effect quickly | Forced inclusion via path C (gas not bound by the block limit); the right to produce blocks rotates with hashrate; inclusion lists to be re-evaluated if producers become highly concentrated (§17) |
| Producers do not broadcast settlement transactions / do not carry bodies | Data does not reach BTC; the indexer and finality cannot advance | Any node can broadcast, rescue or republish on their behalf (a cheap claim in the BTC mempool no longer holds rescues back, §7.3, D-098); income unlocks only after the settled closure; the settlement backlog limit pauses the fast path; carrying others' data earns direct income (carrier reward, §8.6, D-095) |
| Front-running others' data to grab the carrier reward; every block carrying its own settlement without waiting for the batch, to grab the fixed part | Someone without hashrate collects rewards for nothing; more settlement transactions | The reward of a settlement payload goes to the producer of the block that carries it (a block must be mined first), a republish payee must wait 2 BTC blocks, and only the first of duplicate payloads counts (D-095); the extra BTC fees of every block carrying its own settlement are at most equal to the fixed part, which can be changed to per-byte after observation on signet |
| A block whose body no payload can carry | Its data would never land: the settled closure, unlocks, BTC finality and the indexer stop at it, and path B pauses after `SETTLEMENT_TIMEOUT`; before D-100 any miner could produce one | Consensus rule: a block that needs a body is invalid unless its body fits in a republish payload carrying just this block (§6.3, D-100); producers stay within it, and a settlement payload that would be too large carries less or nothing (D-098) |
| A new block with a forged anchor delivered by sync | Accepted as an anchor-invalid historical block; before D-100 its forced-inclusion entries went unchecked, and merging it could stop block production or leave a body the indexer can never determine | Its forced-inclusion entries must be determinable from the current best BTC chain when it is accepted, looked up from the start of its own forced window (§6.8, D-100), and a block merging one whose forced entry names no native transaction carrying its op is invalid (§7.4, §9.6, D-100); producers leave blocks with a merge hazard out of their templates (D-098); a BTC reorg that drops such a transaction afterwards takes the blocks relying on it off the main chain (§6.4, D-104) |
| Replacing the payload in the witness | Settlement broken cheaply (the transaction ID is unchanged) | The payload sits in a leaf script committed to by the output, preceded by a commit transaction |
| Time-locking the settlement transactions | A block delivered by sync carries settlement transactions whose inputs BIP68 locks, so its data cannot land before the lock expires and a rescue is needed | Every input of the commit and the reveal transaction has `nSequence = 0xFFFFFFFD` (§7.2, D-100) |
| Oversized or malformed settlement data | Node crash or state fork; memory or CPU exhaustion of every node and indexer; a header whose timestamp bound cannot be checked when it appears | Strict length limits, `MAX_SETTLEMENT_HEADERS` checked before any header is read; a decoding failure only rejects the copy; canonical encoding; fuzzing; the payloads pending in the tracker share one copy of their carrying transaction, and a parent's merge depth is checked before any ancestor query (D-098); a payload with a header anchored at or above its own BTC height does not count (§7.1, D-100) |
| Deep side chains: a long chain of cheap blocks forked from an old block, offered for merging again and again | Every check walks the whole history (CPU); a block only a full history can judge would split pruned from unpruned nodes | The merge-depth root rule (§6.3, D-097): such a chain cannot be merged, and the check goes no lower than the root's blue work; carried bodies are bounded by the root too, so pruned and unpruned nodes judge alike |
| Memory growth | Node and indexer memory grows with the chain (31.9 GB per active year measured before D-097) until the process fails | Pruning below a BTC-final pruning point (§10.2, §10.3, D-097): memory is bounded by the window the pruning margin and depth keep; RocksDB's write buffers and block cache have fixed bounds, and on Linux jemalloc returns the memory the threads free (§10.2, D-103) |
| VM non-determinism | State fork between nodes | No floating point, NaN canonicalization, architecture-independent stack metering, pinned wasmtime / jmt versions, determinism test, the indexer checks `state_root` |
| Cross-domain signature replay | Assets stolen | Tagged hash domain separation, `chain_id`, nonce; the BIP322 message is the hex of the sighash; the path C signature is bound to the carrying transaction's first input. The protection is one-way: a site that asks for a BIP322 signature of a "random" 64-hex string can present a VmOp sighash, so wallets must show the decoded op before signing (D-098) |
| Mis-sends: transferring KSA to the bytes of a BTC address, transferring BTC to a "Ksana account" | Funds are credited to a key nobody holds, which amounts to loss | An account is a Taproot address (§4, D-092): the two are two spellings of the same bytes, so a mis-send lands on the same key; non-Taproot addresses and contract IDs have no address form, and clients refuse to parse them as accounts |
| Malicious contracts exhausting resources | Node denial of service | Exact fuel metering, memory and instance limits, call depth and stack budget, block gas limit; a read-only `ksana_call` runs at most `BLOCK_GAS_LIMIT` gas and at most 4 at a time per node (D-098) |
| Mempool and network spam | Bandwidth, CPU and storage saturated | Signatures, nonces and balances validated at submission; queued count limited per account; the entry pool is capped by bytes, evicts the ops that stand lowest (behind a nonce gap first, then by fee rate) instead of rejecting newcomers, limits the ops behind a nonce gap and re-checks balances after chain changes; orphans are checked before they are kept and the orphan pool is capped by bytes; the queued gossip of banned peers is dropped (D-098); token-bucket rate limiting by the address a peer connects from, kept across reconnections, length limits (a gossip op within `MAX_OP_BYTES`), scoring and bans, of the address too; inbound connections capped in all, in the handshake, per address and per peer, the gossip awaiting validation and the sync responses being sent held within byte budgets per topic and per source, sync requests served a few at a time; RPC batches, bodies and subscriptions bounded, `ksana_getAccount` linear in the window (D-132) |
| BTC reorg | Anchor blocks disappear, native transactions change height | Anchor depth; anchor-invalid mode; a reorg deeper than the anchor depth takes the blocks anchored on the orphaned BTC blocks off the main chain, and their ops are included again on the new branch (§6.4, D-104); the finality determination rolls back with BTC; alerts; a reorg deeper than the pruning margin (144 BTC blocks) stops the node for a resync and makes the indexer start over from the genesis anchor (D-097) |
| Reversing a fast-confirmed payment with a BTC reorg deeper than `ANCHOR_DEPTH` | A BTC miner who can make the reorg prepares a fast-lane branch with the double spend on its withheld BTC blocks; when the reorg is published the nodes retreat and take that branch (D-104); before D-104 the same branch won anyway, once it became final | Against such a miner a fast-lane confirmation is worth its anchor's BTC confirmations: reversing a block m BTC blocks after it was produced takes a reorg of at least `ANCHOR_DEPTH` + 1 + m blocks, and such a miner can double-spend BTC itself; applications wait by amount (§10.4) |
| Upgrade fork | Nodes that did not upgrade silently follow another chain | The block header version strictly matches by DAA score; an unknown version is only rejected with an alert, and the indexer stops |
| Landing a block with an invalid body on BTC | Formerly halted every indexer until restart (the indexer follows the headers on BTC) | The indexer disqualifies the block like one that cannot land (§10.3, D-098) |
| Getting an invalid block finalized: landing it on BTC first, with its data reaching `BTC_FINALITY_CONFS` confirmations before the honest chain's (path assessment, finding 1) | Before D-101 the finalized chain passed through it: every indexer halted, every node's finality stopped and nothing became BTC-final again, for the cost of one fast-lane block and one BTC transaction pair | Only a block whose execution on top of the finalized tip is valid becomes final; an invalid one is excluded with the blocks built on it, and the next best candidate is taken (§6.5, D-101) |
| Stopping the finality determination with a block whose `btc_anchor` names no BTC block, landed first the same way (the review of D-101) | Its re-execution failed at every BTC tip, or its body could never be determined, and the first version of D-101 waited for an answer: every node's and indexer's determination stopped at that height for good, with pruning, for the same cost | A block anchored, or merging a block anchored, off the BTC chain is excluded before its landing status or verdict is asked (§6.5 item 2, D-101) |
| Stopping it with no BTC transaction: handing a node, through sync, a block anchored off the BTC chain (an anchor that lags far, whose hash is not looked at, or a stale BTC block) for the honest chain to merge or build on (the second review of D-101) | With the exclusion above, an honest block merging it or built on it could never be final, nor could any block after it: one or two fast-lane blocks and a P2P connection stopped every node's and indexer's finality for good | The block template never merges a block anchored off the BTC chain, the main-chain selection never takes one, and a historical block may not anchor above the local tip − `ANCHOR_DEPTH` (§6.2, §6.3, §6.4, D-101) |
| A client signing for another network than its node's (the second audit's A2-11) | The KSN1 magic is the same on every network, so a path C op or a top-up's claim signed with another network's chain ID is force-included, fails on its chain ID, and the BTC payment confirms anyway: the buyer loses it | Clients take the network from the node (D-106): the CLI reads the node's chain ID before every command that talks to it, `--chain` only stating the network expected, and path C and top-up refuse a bitcoind on another BTC network; the TS SDK has no default network and never builds a path C transaction for an op of another chain ID; the web page checks the chain ID when it connects and before anything is signed, refuses mainnet until it supports it, and lists coins, broadcasts and signs path C or a top-up only with a Bitcoin RPC on its network's BTC network and a wallet not set to another BTC network (Unisat lists coins and broadcasts only once it names the page's) |
| A path C op copied into someone else's transaction | The non-paying copy confirms first and consumes the one-time account's nonce; when the buyer's real payment confirms afterwards the claim fails and the BTC is paid for nothing | The signature is bound to the carrying transaction's first input (§4.2, D-086): the copy does not pass authentication and does not consume the nonce; the lock's beneficiary is fixed; the seller releases after expiry; the SDK checks the lock state and beneficiary before paying |
| The seller pays a few sats to its own lock just before the buyer's payment | Formerly the claim closed the lock and returned the rest to the seller, who kept the buyer's BTC and most of the KSA | A claim buys only its share and the rest stays locked for the same beneficiary; a BTC transaction claims a lock once (§5.3, D-098) |
| A buyer who asks for a lock and never pays | The seller pays the fees of `lock` and `release`, and its KSA stays locked until `expiry + RELEASE_DELAY` (at least about 70 minutes on mainnet); the buyer spends nothing | The contract cannot charge the buyer: short expiries, an off-chain deposit or reputation are the seller's tools (D-098) |
| The project token's price falls | Sponsorship runs at a loss, producers stop accepting path B | Market pricing; producers can raise the minimum token amount at any time; users can still use path C |
| Hot wallet stolen | Loss of the funding capital | Funding wallet separate from the payee account; revenue address and hot-wallet cap |

Security requirements:

- All external input (P2P, RPC, BTC data) is treated as untrusted; decoding must check the length limit first and require full consumption.
- Private keys exist only in wallets and locally at the producer; logs must not output any key material.
- The producer's settlement funding wallet is separate from the revenue wallet (`--revenue-address`, `--hot-wallet-cap-sat`), limiting the amount of funds in the hot wallet.
- Consensus code forbids `unwrap()` and `panic!`; errors must be returned and the block or op judged invalid; a local fault must not judge a block invalid.
- Nodes and the indexer log a highest-level alert in the following cases: a reorg deeper than `SETTLEMENT_DEPTH`; the settlement backlog limit is triggered; a finalized block is invalid; a block that meets the BTC
  finality condition is invalid and is excluded (D-101); a BTC reorg takes back a block reported BTC-final (D-114); a forced switch to the finalized chain while
  the state at the fork point has been pruned (the state is rebuilt from the genesis block, D-093); crash recovery finds no state version consistent with the main chain (likewise rebuilt); a block header version higher than this software knows;
  a BTC reorg deeper than what pruning kept (D-097).

## 17. Open questions and matters after mainnet

The consensus is frozen (v2 final, D-096): the project owner signed off on 2026-10-05, apart from the receiving wallet of the block share. The questions v1 and v2 listed in this section are all settled
or void: the data carrier (Taproot witness envelope), the cost of landing headers on BTC (header compression, empty blocks not changing the state root, two-speed difficulty), double-spend protection for path A and the account UTXO dual-signature lock
(path A deleted), issuance and distribution of the project token (D-089), the block production method (PoW, D-088; no merged mining, D-087), the form of the fast-lane PoW (Argon2id, D-094;
empty block headers keep landing on BTC, with the difficulty escalating with the streak, D-090; immediate republishing when a competing chain appears on BTC, D-091), the unification of accounts and BTC addresses (D-092), the carrier reward (D-095),
the mainnet parameters (§13, D-096), bounded node memory (pruning with the merge-depth root rule, D-097). Only three kinds of content remain in this section, plus the consensus fixes the security audit of
2026-10-06 proposed and their outcome (§17.4, D-098, D-100); until settled, the implementation always follows the default option.

### 17.1 The only open item

| Question | Default option | What needs deciding |
| --- | --- | --- |
| The receiving wallet of the block share | The genesis config `block_share_account` is an ordinary account (D-089); the mainnet template `mainnet/genesis.json` uses a placeholder, and node and indexer refuse to start with a config carrying the placeholder (§6.1, D-096) | The address itself, decided by the project owner; the ratio and the end point stay unchanged (D-089) and are not published yet (D-134) |

### 17.2 Mainnet parameter values confirmed by the project owner (2026-10-06)

D-096 marked six values it chose for mainnet "default value, pending the owner's confirmation" (§13.1): `CHAIN_ID` 1000; `NATIVE_CONFIRMATIONS` 3 and `ANCHOR_DEPTH` 2;
`POW_LIMIT_BITS` `0x1f00ffff` and "the genesis `bits` is filled in according to the expected hashrate"; the Argon2id parameters carried over from devnet (2 MiB / 1 pass / 1 lane); `REORG_WINDOW` 1 000.
The project owner confirmed all six on 2026-10-06 ("Confirm all"), so §13.1 records them as settled; no code changed. The other values carried over from devnet (GHOSTDAG 16 / 10 / 128 / 720, K raised by D-100;
two-speed difficulty 12 / 120, the 7 200-second half-life, the gas and length limits) rest on decisions already made and were already settled (§13.1). The Argon2id parameters can still be retuned before mainnet
by the measured GPU advantage (the retuning path of D-094, a work item before mainnet in §14). Before the mainnet launch, changing any of these values means changing one field of `MAINNET`
(`crates/ksana-primitives/src/params.rs`), which does not count as a consensus change.

### 17.3 Deferred to after mainnet

| Matter | Default option | Rationale and trigger conditions |
| --- | --- | --- |
| Native transactions after a deep BTC reorg | Included again in the new branch's order (§7.4): an op whose nonce is then out of turn fails, and the account's ops above it wait until it fills the gap, the entry pool keeping 8 of them (D-098) | Only in a BTC reorg that takes back blocks holding several ops of one account, whether or not it moves the native transaction: from `ANCHOR_DEPTH` + 2 blocks (4 on mainnet) along one chain of blocks, and when the path B op is in a parallel block merged by the block that included the path C op, from `ANCHOR_DEPTH` + 2 blocks when the next block merges the parallel block again, as nodes do (local policy, §6.2), and from `ANCHOR_DEPTH` + 1 (3 on mainnet) when it does not (D-104); the user resubmits, as before D-104 for a native transaction the window had already passed; executing them in nonce order would need new state and inclusion rules |
| Forced-inclusion entries carrying the VmOp | Not carried (the indexer derives it from the BTC transaction per §7.4) | The derivation already covers anchor-invalid blocks (D-056, D-093), and since D-104 a block whose native transaction a deep BTC reorg moved or double-spent leaves the main chain instead of being executed again; to be reconsidered only with a change of the payload format |
| Querying ops dropped from the entry pool (D-068) | The web page marks an op "dropped" after two consecutive queries return `-32004` | Having the node keep the reason an op was dropped (`dropped`) is an RPC improvement, not a consensus change |
| Settlement funding pool | None; producers bring their own BTC, obtained with KSA at the exchange contract | To be reconsidered when small miners lack BTC; first watch how carrying goes on signet after D-095 |
| Carrier reward ratios and wait | 500 / 5 000 / 2 (D-095) | The two ratios are retuned via the upgrade schedule after observation on signet; if the fixed part tempts producers to attach their own settlement to every block, switch to counting by bytes (D-095 "to be observed"); `CARRIER_REPOST_DELAY` is fixed per network |
| Sell-pressure hedging for the token | No fee burning, no token-holding threshold for producers (contracts can burn their own assets) | To be decided from fee-market data once mainnet is running |
| Inclusion lists | None; producers choose entries freely | If producers become highly concentrated and censorship appears, wire in inclusion lists similar to Ethereum's FOCIL via the upgrade schedule |
| Further compression | None | Signature half-aggregation, a global dictionary by account index (the latter makes decoding depend on state): to be re-evaluated when BTC fees become the main cost |
| Interoperability with Ethereum | Not in this phase | Integration approach and trust model for a later phase |
| Syncing new nodes from a state snapshot at the pruning point | New nodes sync from the genesis block, served from the block stores of pruned peers (D-097) | Kaspa-style snapshot sync needs a commitment and proof format for the state and a new sync protocol; worth it once syncing from genesis takes too long |
| Items 5 to 9 of the implementation limit cleanup (D-084 reward slots overlapping with merged blocks, D-085 exchange details, D-086 path C restrictions, the entry environment of merged blocks, stopping when a finalized block is invalid) | Keep as is | Accepted together with the project owner's sign-off. Since D-101 an invalid block is never decided final, so stopping remains only for a non-determinism bug |

### 17.4 Consensus fixes of the security audit (D-098, D-100)

The internal audit of 2026-10-06 (M26) found holes that only a consensus rule closes. M26 shipped the local mitigations; on 2026-10-06 the project owner chose to fix them all and to change the fork choice, and M27 implements the
rules below on top of the bounded merge depth of M25 (D-097), so that devnet and signet restart from genesis once (D-100).

| Problem | Rule adopted (D-100) | What remains |
| --- | --- | --- |
| No rule bounded the body of a block that needs one: a block whose body no payload can carry stops the settled closure, unlocks, BTC finality and the indexer for good (any miner, one block) | A block that needs a body is valid only if its body fits in a republish payload carrying just its own header and body (with a 32-byte payee), within `MAX_BLOCK_DATA_BYTES`, measured with every key written in full as every node measures the bodies it lands (§6.3; the second audit found the exact encoding too lenient) | Nothing |
| A new block can name an old parent and a forged anchor and be accepted as an anchor-invalid historical block (§6.8's rationale did not hold): its forced-inclusion entries went unchecked, and a merge of it failed with a local fault or left a body the indexer can never determine | A merged block's forced entry naming no native transaction carrying the same op on the merging block's anchor chain makes the merging block invalid, and unlock entries of merged blocks are skipped (§9.6); an anchor-invalid block is accepted only if its forced-inclusion entries can be determined from the current best chain, looked up from its window start so that pruning does not change the verdict (§6.3, §6.8); executing such a block again never judges it invalid (§6.8); §6.8's rationale corrected | Nothing: a deep BTC reorg after acceptance takes the blocks relying on what it dropped off the main chain (§6.4, D-104) |
| The header DAG accepted a header whose anchor is at or above the BTC height of the payload that carries it, skipping the timestamp bound, which can pause the finality determination | A payload counts only if every header it carries anchors below its own BTC height (§7.1); the rule is on the payload rather than on the header DAG so that every node and indexer decides the same way whatever order it learns blocks in; the settlement payload a block carries has no header anchored above the block, so it always counts (§7.2) | Nothing |
| `MAX_SETTLEMENT_HEADERS` was not checked when a payload is decoded, although §7.1 said it was | Checked before any header is read, for republish payloads too (§7.1) | Nothing |
| The `nSequence` of settlement transactions was not checked: a block delivered by sync can carry BIP68-locked settlement transactions and force a rescue | Every input of the commit and the reveal transaction has `nSequence = 0xFFFFFFFD` (§7.2) | Nothing |
| Withholding weight-1 blocks in idle periods turned honest heavy empty blocks red (§16) | The fork choice counts a tip's own work, `chain_work = blue_work + work` (§6.4, §6.6); idle-chain reversal falls and short busy-chain reversal rises (§10.4) | Withholding a heavy empty block (§16), below |

A second internal audit the same day, of M25 and the first M27 commits, found that the first versions of the body, payload and anchor-invalid rules could still stop the settled closure for good with one block, and that
whether a node accepted an anchor-invalid block could depend on how much it had pruned; the rules above include its fixes, with the local policy that goes with them (§7.3, §11.1, D-100).

Withholding a heavy empty block takes honest reward slots when an empty block outweighs more than `GHOSTDAG_K + 1` weight-1 blocks (12 against 9 with D-063's K = 8). On a second decision card the project owner chose
on 2026-10-06 to raise `GHOSTDAG_K` to 16 (D-100), which devnet and signet take at the same restart. The share of the honest reward slots a withholding miner takes (`selfish_mining.rs`, with `KSANA_SM_K` / `KSANA_SM_N`
for the other rows, 2 400 simulated hours per cell):

| Parameters | Busy chain, α = 5% / 10% / 20% / 30% | Idle chain, α = 5% / 10% / 20% / 30% |
| --- | --- | --- |
| D-063: `GHOSTDAG_K` 8, `EMPTY_BLOCK_DIFFICULTY` 12 | 1.5% / 3.6% / 11% / 23% | 1.6% / 5.4% / 13% / 20% |
| `GHOSTDAG_K` 12 | 0.08% / 0.58% / 4.6% / 17% | 0.92% / 4.0% / 11% / 18% |
| Adopted: `GHOSTDAG_K` 16 | 0 / 0.02% / 1.1% / 10% | 0.50% / 2.9% / 9.6% / 16% |
| `GHOSTDAG_K` 8, `EMPTY_BLOCK_DIFFICULTY` 9 | 0.25% / 1.1% / 6.0% / 17% | 1.6% / 5.4% / 13% / 20% |
| `GHOSTDAG_K` 8, `EMPTY_BLOCK_DIFFICULTY` 6 | 0.01% / 0.08% / 1.4% / 8.0% | 1.6% / 5.4% / 13% / 20% |

A smaller `EMPTY_BLOCK_DIFFICULTY` does nothing for an idle chain, where the honest blocks are empty blocks too, and puts more empty-block headers on BTC while the chain goes idle. A larger `GHOSTDAG_K` lets blocks with larger anticones
stay blue: with 16 the honest blocks that a withheld empty block of weight 12 can overtake fit in a blue anticone. What remains is on an idle chain, where the honest blocks are empty blocks found minutes apart and the private chain
can still put more than K blocks in the anticone of one of them. The costs of 16: coloring checks up to K + 1 blue blocks per merge set, about twice the worst-case work of 8; the pruning depth of D-097 grows by 24 blocks; payment
reversal (§10.4) moves by at most half a percentage point.

The work items before mainnet (no consensus change) are at the end of §14: real wallets on the public signet, the security audit, the measurements of §13.1, the D-095 update of the web pages.

## 18. Reference projects

Each of these projects implements a part of this design; before implementing the corresponding module it is worth reading their documentation first.

| Project | What can be borrowed |
| --- | --- |
| [Kaspa](https://github.com/kaspanet/rusty-kaspa) | The definition and algorithms of GHOSTDAG (selected parent, merge set, k-cluster coloring, merge depth); this design follows its implementation |
| [Arkade](https://docs.arkadeos.com/learn/architecture/components) | Virtual mempool, DAG conflict handling, preconfirmations and batched settlement |
| [OP_NET](https://docs.opnet.org/getting-started/faq) | Gas paid in BTC, WASM contracts, epoch checkpoints and delayed reward payout |
| [Alkanes](https://alkanes-docs.vercel.app/docs/learn/background/metaprotocols) | Both the contracts and the indexer compile to WASM; state is reproducible |
| [Stacks Nakamoto](https://docs.stacks.co/learn/block-production/what-was-the-nakamoto-upgrade) | Fast block production and BTC finality; blocks not built on the BTC chain tip are rejected |
| [Taiko preconfirmations](https://docs.taiko.xyz/protocol/preconfirmations.md) | Preconfirmer rules, forced inclusion, one L1 submission carrying several small blocks |
| [Rootstock](https://dev.rootstock.io/concepts/foundations/merged-mining/) | How merged mining is integrated with mining pools |
| [Braidpool](https://github.com/braidpool/braidpool/blob/main/docs/overview.md) | Weak-block DAG, deterministic block templates |
| [Arch Network](https://docs.arch.network/whitepaper.pdf) | A UTXO-aware execution environment |
| [Ordinals](https://docs.ordinals.com/inscriptions.html) | The `OP_FALSE OP_IF … OP_ENDIF` envelope in the Taproot witness and the way data is written with a commit / reveal pair of transactions |
