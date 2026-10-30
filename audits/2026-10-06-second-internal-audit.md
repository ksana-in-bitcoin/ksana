# Second internal security audit (2026-10-06)

> This report was written before the repository was published. Its commit IDs and the milestones it names (M25,
> M27 and the like, `docs/SPEC.md` §14) refer to the development history before the first public commit, which was
> not published, and the web wallet's paths (`web/...`) are in the ksana-web repository now.

This is the second internal review of Ksana before mainnet. It covers what changed after the first audit
(D-098 / M26): block DAG pruning and the merge-depth rule (D-097 / M25), the consensus fixes of the first
audit with `GHOSTDAG_K` 16 (M27, D-100) and the chain-base change to the block DAG, and it takes a fresh look at every
consensus-critical path. It also gives the status of each finding of the first audit.

Ksana has not launched on mainnet. Devnet and signet restart from genesis after these consensus changes, and nothing
on them holds value.

## Summary

**Audited code.** The review was done on M25's head `dd24020` plus the first five M27 commits, the code that was
about to ship; in `main`'s history that code is `11f0576`. The chain-base change was reviewed as patches on the same
code. Every finding was then re-checked on `main` at `5819b26`, after M25, M27 and the chain-base
change merged; the statuses below are as of that commit.

**Results.** 22 findings and 8 informational notes: 2 critical, 1 high, 9 medium and 10 low. The critical and high
findings were all found in M27 before it merged and were fixed in M27. On `5819b26` 6 findings are fixed, 2 are
partly fixed and 14 are open: four medium (A2-08 to A2-11) and the ten low ones. Two of the open ones should be fixed
before mainnet: A2-09, because a block's low-fee settlement waiting in a congested BTC mempool pauses path B for
everyone, and A2-11, because a top-up on the wrong network loses the buyer's BTC.

The two critical findings are the same kind of failure the first audit's F-01 and F-02 were: a block that every node
accepts but whose data can never land on BTC. Once such a block is merged, the settled prefix stops at it; after
`SETTLEMENT_TIMEOUT` BTC blocks the settlement backlog limit allows only empty or forced-only blocks, so path B stops
for everyone, and unlocks, BTC finality, the indexer and pruning stop with it. Both came from M27's own fixes:

- A2-02: D-100 d makes the BTC scanner ignore a whole payload when any header in it anchors at or above the height it
  confirms at, but block acceptance did not apply the same rule to the other headers of an attached settlement. One
  block with one extra header anchored at `u32::MAX` in its attached payload, plus one small BTC transaction pair,
  was enough.
- A2-03: D-100 b bounds a block's body by the exact size of a republish payload, where a key repeated in the payload
  costs one byte, while every node that lands someone else's body measures it with each key at full size. About
  1 000 transfers between the same two accounts passed the rule and could never be landed by anyone but their
  producer, who simply did not attach a settlement. One block, no BTC transaction.

Both were reproduced end to end (a node test each) before the fix, and the fixes landed in M27.

The high finding, A2-01, is where pruning (D-097) meets M27's rule for anchor-invalid blocks (D-100 c): whether a node
accepts such a block, and whether an indexer can determine its body, depended on how far that node had pruned its BTC
snapshot. One block of PoW split pruned from unpruned nodes and indexers.

The medium findings are local denial-of-service paths (work done before the proof of work is checked, refusals that
are not remembered, cheap dead tips and entry-pool churn that keep honest producers rebuilding templates, unbounded
pending headers from BTC payloads), one consensus edge case in anchor-invalid re-execution after a BTC reorg (A2-04),
a rescue that a block's own low-fee settlement holds back for as long as it stays in the BTC mempool, with no fee
bumping for any settlement or rescue (A2-09), and a wallet-side default that can lose a buyer's BTC on mainnet (A2-11).

**First audit.** Of its 42 findings, 18 are fixed, 4 partly fixed, 6 documented as accepted limits and 14 open. All
three critical findings are fixed, and four of the five high ones; the fifth, F-07 (cold wasm compiles priced at one gas
per byte), waits for the gas calibration of SPEC §13.1. The open ones are local or client-side: P2P and RPC limits, VM
pricing, logging, the header DAG's error mapping, the op index, the CLI, the SDK and the web app. Its two simulation
results on payment reversal and withholding were re-run under `GHOSTDAG_K` 16 (SPEC §17.4). The table in [Status of the
first audit's findings](#status-of-the-first-audits-findings) gives each one with the code that closes it or what
remains.

**Long run.** `main`'s code (`5819b26`; `1ea0150` adds only this report and `SECURITY.md`) passed the CI-only checks
and a 10-hour devnet soak on the owner's Mac (three fast-lane nodes and the indexer, random path B and path C ops, the
indexer checked against the nodes height by height): 6 052 ops without a failure, equal block hashes and state roots
on all four services at the end, four pruning steps each and no restart. Node memory changed little at each pruning
step but kept rising slowly, from about 150 MB at the first step to 166 to 192 MB after 10 hours; the cause is not
known yet. A first attempt, on a heavily loaded Mac running on a draining battery, stopped after 62 minutes when an op
waited longer than the CLI's 300 s: the known devnet timing effect of D-067, not a regression, which the owner chose
to keep because mainnet and signet, with 10-minute BTC blocks, are not affected. An earlier 5-hour soak of M25's
head passed with pruning active: no failed op, equal block hashes and state roots on all four services at every height
checked, two pruning steps each, and flat memory.

## Scope and method

What was reviewed:

- Consensus: GHOSTDAG and fork choice by chain work (D-100 a), the merge-depth rule and pruning (D-097), two-speed
  difficulty and ASERT, the header DAG and BTC-anchored finality, the bodies of blocks determined from BTC, the
  forced-inclusion rules of anchored and anchor-invalid blocks, block execution (strict and lenient), rewards, unlocks
  and the carrier share.
- BTC layer: the tracker and its snapshots, payload recognition and counting, settlement transactions, rescue and
  republish, the funding pool.
- Node: acceptance, the orphan pool, the main chain and reorgs, crash recovery, checkpoints, the block producer, the
  entry pool, P2P and RPC.
- Indexer: tracking, body determination, execution and disqualification, restarts.
- Encoding, the TypeScript SDK, the example contracts, the CLI and the web app.

How:

- Eight review passes split by component, plus the lead's own pass over the cross-cutting paths. Every candidate was
  traced through its real callers in both the node and the indexer, with mainnet parameters in mind
  (`NATIVE_CONFIRMATIONS` 3, `ANCHOR_DEPTH` 2, `MAX_ANCHOR_LAG` 1, `GHOSTDAG_K` 16, `MERGE_DEPTH` 720,
  `min_pruning_depth` 2 209, `MAX_BLOCK_DATA_BYTES` 80 KiB, `MAX_SETTLEMENT_HEADERS` 500, `SETTLEMENT_TIMEOUT` 6).
- The lead verified every finding along its call chain before it was reported; the severe ones were sent to the
  owning workstream as soon as they were confirmed, so they could be fixed before merge.
- Repro tests were written for the critical and high findings (listed under each finding). They were run on the
  audited head and are not part of the repository; the fixes carry their own regression tests.
- Severity: **critical** = any participant can stop the chain or split honest nodes for good at a small cost;
  **high** = a consensus split or a lasting halt that needs a specific setup; **medium** = local denial of service or
  a consensus edge case that needs a rare event; **low** = bounded local cost, hardening, or an operator-side issue;
  **info** = documentation and observations.

Not covered: the wasm runtime's own correctness (wasmtime is a pinned dependency), Bitcoin Core, the browser wallets,
performance under real network load (costs below are estimates unless a test is named), and the economics of the
reward rules beyond what the simulations in SPEC §17.4 cover.

## Findings

Line references in findings that are fixed point at `11f0576`, the audited code in `main`'s history; references in
open findings point at `5819b26`.

| ID | Severity | Finding | Status |
|----|----------|---------|--------|
| A2-02 | Critical | An attached settlement payload that never counts stops the settled closure | Fixed (M27) |
| A2-03 | Critical | The consensus body bound is exact, the landing code measures an upper bound | Fixed (M27) |
| A2-01 | High | Whether an anchor-invalid block is accepted depends on local pruning | Fixed (M27) |
| A2-04 | Medium | Re-execution in anchor-invalid mode can disqualify a valid block after a BTC reorg | Fixed (M27) |
| A2-05 | Medium | GHOSTDAG work before the proof of work, merge-depth walks not memoized | Fixed (chain-base change) |
| A2-06 | Medium | Refusals that are not invalidity are never remembered | Partly fixed (M27) |
| A2-07 | Medium | Cheap dead tips keep honest producers rebuilding templates | Partly fixed (chain-base change); signet's PoW limit raised after this report in M51 (D-136) |
| A2-08 | Medium | Entry-pool churn keeps honest producers rebuilding templates | Open; fixed after this report in M47 (D-130) |
| A2-09 | Medium | A block's own low-fee settlement holds its rescue back, and nothing raises a stuck fee | Open, fix before mainnet; fixed after this report in M28 (D-102), its P2A pinning residual in M48 (D-131) |
| A2-10 | Medium | Headers with unknown parents from BTC payloads are held unchecked and without a size limit | Open; fixed after this report in M51 (D-135) |
| A2-11 | Medium | The CLI, SDK and web app default to devnet and never check the node's chain ID | Open, fix before mainnet; fixed after this report in M31 (D-106) |
| A2-12 | Medium | A fresh indexer holds the whole history in memory before it first prunes | Fixed (chain-base change) |
| A2-13 to A2-22 | Low | Ten bounded local costs and hardening items, in the [Low](#low) table | Open; A2-15 and A2-19 closed after this report in M51 (D-135) |

### Critical

#### A2-02: an attached settlement payload that never counts stops the settled closure (D-100 d)

**Where.** `crates/ksana-btc/src/payload.rs:61-96` (`settlement_belongs_to`), `crates/ksana-btc/src/tracker.rs:445-449`
and `:1410` (`payload_counts`), `crates/ksana-node/src/rescue.rs:379-387` and `:664-667`,
`crates/ksana-node/src/chain.rs:2477-2484` and `:3953-3955` (`test_settlement`).

**What was wrong.** D-100 d (the fix of the first audit's F-09) makes the BTC scanner ignore a settlement or republish
payload as a whole when any header in it anchors at or above the BTC height the payload confirms at. Block acceptance
checked that the last header of a block's attached payload is the block and that the entries match, but let the other
headers be anything ("headers prove themselves by PoW"), and the compact codec writes an anchor height as a plain
`u32`. So a block S whose attached payload is `[a header anchored at u32::MAX, S's header]` was valid on every node,
while its payload could never count.

Nothing healed it:

- rescue skips a block whose attached reveal bitcoind already knows (`test_settlement` returns `Ok` for a confirmed
  reveal), and a rescue would have reused the same payload;
- republish returns as soon as the first block whose data has not landed carries a settlement of its own, and waits
  for that settlement to carry the blocks before it;
- carrying skips blocks that carry a settlement and does not walk past them.

**Impact.** S's data never lands. `prefix_height` is `None` from S on; after `SETTLEMENT_TIMEOUT` BTC blocks the
settlement backlog limit allows only empty or forced-only blocks, so path B stops on every node; unlocks, BTC
finality, the indexer and the pruning point stop with it. Recovery needs new code on every node.

**Who and cost.** Any miner: one fast-lane block and one commit/reveal pair (a few hundred vbytes). The extra header
needs no valid proof of work.

**Evidence.** Unit test (both halves of the asymmetry) and node test: a second node accepts a producer's block
re-signed with such a payload; the pair confirms, the block's data never lands, the node neither rescues nor
republishes it, logs "fast path paused (D-046)" every round, and never includes a path B op from its pool again.

**Fix.** Fixed in M27, within D-100 d. Every header of a block's attached settlement payload must anchor at or
below the block's own anchor height (`settlement_belongs_to`; a copy that breaks it is refused, the block hash is not
marked). The reveal commits to the block's anchor, so on any BTC chain holding that anchor it confirms above it and the
payload counts; an honest payload carries headers from the block's past, whose anchors never exceed the block's own (a
block anchored below its parents' highest anchor is invalid), so no honest block changes. Local part: rescue no longer
takes a reveal that bitcoind knows as landed. A settlement, or this node's own rescue, confirmed on the best chain in a
scanned block without counting (a BTC reorg mined it again at or below the block's anchor) is rescued with the same
payload once the BTC tip has reached the anchor. The audit's node test, turned around: the poisoned copy is refused, the
honest copy accepted, and after the producer confirms the poisoned pair on BTC another node rescues the block with the
honest payload and path B goes on.

#### A2-03: the consensus body bound is exact, the landing code measures an upper bound (D-100 b)

**Where.** `crates/ksana-btc/src/payload.rs:197-224` (`check_body_fits`) against `payload.rs:139-141` (`body_len`),
`crates/ksana-primitives/src/compact_entries.rs:219-237` (`max_entry_len`, `max_body_len`),
`crates/ksana-node/src/rescue.rs:756-762` and `:791-793` (republish), `crates/ksana-node/src/chain.rs:245-261` and
`:2515` (carrying), `rescue.rs:374` (rescue only for blocks that carry a settlement).

**What was wrong.** D-100 b (the fix of the first audit's F-01) makes a block that needs a body valid only if a
republish payload holding just its header and body fits in `MAX_BLOCK_DATA_BYTES`, measured by the exact encoding. In
that encoding a key already seen in the payload is a one-byte reference (D-080). Everything that lands another
block's body measures it with `max_body_len`, which writes every key other than KSA's in full (33 bytes): republish
adds that bound and stops before the first header when it exceeds the limit (the exact-size loop after it can only
drop headers, and an empty list sends nothing), and carrying drops a body above a quarter of the limit. Rescue exists
only for blocks that carry their own settlement.

**Impact.** A block of path B transfers between the same two accounts, with no settlement of its own, passes
D-100 b and is accepted and merged everywhere, but no node can ever land its body: the same permanent halt as A2-02.

**Who and cost.** Any miner: one fast-lane block, no BTC transaction. The ops are the attacker's own.

**Evidence.** Unit test: 1 061 transfers give an exact republish payload of 81 864 bytes (limit 81 920) and an upper
bound of 149 712 bytes. Node test: a funded node merges a side block of 1 031 such transfers; over
`SETTLEMENT_TIMEOUT + 8` BTC blocks its body never lands, nothing is republished, the node logs "fast path paused
(D-046)" every round, and a path B op in its pool is never included.

**Fix.** Fixed in M27, within D-100 b. `check_body_fits` measures `headers_payload_fixed_len([header]) +
max_body_len(entries)`, the expression republish uses for its first block, so the body of every valid block fits a
republish payload of its own; the exact encoding is never longer. Honest producers already packed to this bound
(`body_room`, with every entry counted by `entry_data_len`). The audit's node test, turned around: a side block of 1 031
transfers (exact encoding 81 616 bytes, upper bound 147 544 bytes) is invalid, and path B goes on.

### High

#### A2-01: whether an anchor-invalid block is accepted depends on local pruning (D-097 with D-100 c)

**Where.** `crates/ksana-btc/src/bodies.rs:164-186` (`check_stale_entries`) and `:105-118` (`convert_by_txid`),
`crates/ksana-btc/src/tracker.rs:537-548` (`native_tx`, `native_tx_at_most`), `:652-663` (`BtcSnapshot::prune`) and
`:1818-1826` (`AnchoredView::native_input`), `crates/ksana-node/src/chain.rs:1248` and
`crates/ksana-indexer/src/indexer.rs:1341` (the snapshot cut).

**What was wrong.** D-100 c requires each forced entry of an anchor-invalid block that needs a body to name a native
transaction on the current BTC best chain, "at any height". The node checks this when it accepts the block and when
its producer considers merging it, the indexer uses the same lookup to determine the block's body, and lenient
execution of a merged block looks the transaction up again. All three search the local BTC snapshot, and D-097 prunes
native transactions below `cut = the pruning point's anchor - NATIVE_CONFIRMATIONS` from it. The pruning point moves
in steps from wherever each node first pruned, so nodes hold different ranges, and `--no-pruning` nodes and freshly
synced ones hold more.

**Impact.** One block X naming an old native transaction T (below some nodes' cut, above others'), delivered as a
historical block on an old main-chain parent with a made-up anchor:

- nodes that still hold T accept X and merge it; nodes that pruned T refuse X and every block after it, so they cannot
  follow the heavier chain, and their BTC finality decision stops for good at the block that merges X (X stays
  undetermined), so their pruning stops too and their memory grows from then on;
- indexers split the same way: a pruned indexer can never determine X's body and stops at the merging block;
- a node that prunes T's height between accepting X and executing the merging block skips the entry the producer
  executed and computes another state root; with T an op of the attacker's own account whose nonce only matches later,
  that disqualifies the block on that node or indexer.

A local filter cannot prevent it: the attacker merges X in a block of its own, and honest producers build on that.

**Who and cost.** Any miner: one block of PoW; T can be anyone's native transaction (or the attacker's own, a few
hundred sats).

**Evidence.** Unit test: the same forged header and entries give `Ok` on an unpruned snapshot and `Missing` on the
same snapshot pruned at height 8; `convert_by_txid` and `native_tx_at_most` split the same way.

**Fix.** Fixed in M27, within D-100 c. The forced entries of an anchor-invalid block are looked up only from the start
of its own forced window to the window end of a block anchored now, `[h(parent anchor) + 1, h(tip - ANCHOR_DEPTH)]`
(`ksana_btc::bodies::stale_range`), at acceptance, in the producer's merge-hazard check and in the indexer's conversion.
The window start is above the pruning cut of every node and indexer still processing the block (its parents anchor at or
above the pruning point's anchor), so pruning no longer changes a verdict: the audit's unit test now gives the same
verdicts and conversions on pruned and unpruned snapshots. A merged forced entry naming no native transaction that
carries its op on the merging block's anchor chain, at or below its anchor, now makes the merging block invalid instead
of being skipped, so a block's first execution and a later re-execution in anchor-invalid mode find the same
transactions. Honest producers never make such a block: they anchor at `max(tip - ANCHOR_DEPTH, parent anchor)` from the
same BTC snapshot their merge-hazard check and their own build use, and leave a merged block whose execution fails out
of their templates. What remains (SPEC §17.3): after a BTC reorg of `ANCHOR_DEPTH + 1` blocks or more, a node or indexer
that executes such a block again stops with an alert until BTC holds the data again, instead of computing another state.

### Medium

#### A2-04: re-execution in anchor-invalid mode answers at any height, so a valid block can be disqualified after a BTC reorg

**Where.** `crates/ksana-btc/src/tracker.rs:1774-1798` and `:1809-1826` (`block_settled`, `carrier`, `native_input` of
`AnchoredView`), `crates/ksana-execution/src/executor.rs:594-626` (unlock checks) and `:1065-1069` (lenient skip),
`crates/ksana-node/src/lib.rs:112`, `crates/ksana-indexer/src/indexer.rs:1230`.

**What was wrong.** When a main-chain block C is executed again after a BTC reorg removed its anchor (D-056), its view
answers from the current chain at any height, while the first execution answered at or below C's anchor. That keeps
positive answers (a transaction mined again higher is still found), but a negative answer can turn positive: a block
not yet settled at C's anchor is settled now. In lenient mode a negative answer is a silent skip, so a miner can plant
one: a side block M whose only entry is an unlock of a position not yet settled is merged by C and skipped; when C is
re-executed after a reorg of `ANCHOR_DEPTH + 1` blocks, M's unlock executes, moves the unlock cursor past C's own
target, C's own unlock fails strictly, and C is judged invalid. The node disqualifies it; the indexer disqualifies it
and halts once C is BTC-final; no new node can sync past it. The same applies to a merged forced entry naming a
transaction above C's anchor (A2-01's window bound removes that variant).

**Who and cost.** Any miner can prepare it with one block per BTC block; it fires only on a natural BTC reorg of
`ANCHOR_DEPTH + 1` blocks (3 on mainnet) shortly after C.

**Fix.** Fixed in M27. A verdict that a block is invalid when it is executed again in anchor-invalid mode is now a local
fault with an alert, in the node and the indexer, like a mismatching state root, so a reorg can no longer get a valid
block disqualified. On a decision card the project owner chose (2026-10-07) to always skip the unlock entries of merged
blocks (D-100 item c (4)), so a merged block no longer moves the unlock cursor through an answer that can change after a
BTC reorg. The planted forced-entry variant is closed by A2-01's fix.

#### A2-05: GHOSTDAG work before the proof of work, and `passes_root` walks not memoized

**Where.** `crates/ksana-node/src/chain.rs:3008` (`dag.compute`) before `:3016` (`check_header`);
`crates/ksana-btc/src/headers.rs:496` before `:504`; `crates/ksana-consensus/src/ghostdag.rs:871-892`
(`passes_root`), called per merge-set block from `ordered_mergeset`.

**What was wrong.** A block whose parents are all known skips the orphan pre-check and goes straight to `compute`, and
the header DAG does the same for headers from BTC payloads. D-097's `passes_root` walks down the selected chain until
it meets the merge-depth root's chain or drops below the root's blue work, which bounds it by work, not by block count:
a long low-work side chain branching above the root makes each walk as long as the side chain, and the walk is
repeated for each of up to 128 merge-set blocks without memoization. A header with no valid PoW pays all of it before
being refused, under the process lock (node) or the finality-state lock (BTC payloads), and a new nonce gives a new
hash.

**Who and cost.** Any peer, after building one long cheap side chain (cheap on devnet and signet; on mainnet bounded by
the attacker's hash power and ASERT's easing of a side chain's difficulty). Through BTC payloads it costs BTC fees and
reaches every node and indexer. Local DoS.

**Fix.** Fixed in the chain-base change. The merge-depth root walks of one GHOSTDAG computation share their
verdicts, so each block is walked once, and the node and the header DAG check the PoW against the header's own `bits`
before GHOSTDAG (`Consensus::check_header_pow`, then `check_header_rules`; the set of valid blocks is unchanged). A
header that gets that far needs about 2^16 Argon2id hashes at the easiest mainnet `bits`, and then costs one such
computation.

#### A2-06: refusals that are not invalidity are never remembered, and a sync peer can resend them without limit

**Where.** `crates/ksana-node/src/chain.rs:2799-2807` (only the invalid set and the DAG short-circuit a delivery),
`crates/ksana-node/src/net.rs:178-191` (verdicts, BTC retries), `crates/ksana-p2p/src/session.rs:323-329` (round
bound from the peer-claimed height).

**What was wrong.** A block refused for a reason that depends on this node's view (`Rejected`, including M27's
`StaleEntries::Missing`, or `Btc`) is validated from scratch on every delivery: GHOSTDAG `compute`, PoW,
`resolve_all`, every path B signature and the body-size encoding, under the process lock, then answered `Ignore`
without penalty. `Btc` refusals are retried five times, each with a BTC sync and a 200 ms sleep: an anchor-invalid
block carrying a settlement whose commit input exists nowhere costs six full validations per copy. A sync peer can
return hundreds of copies per `GetBlocks` response, for as many rounds as its claimed tip height allows, and the tips
it lists in `Status` are fetched again on every sync.

**Who and cost.** Any peer, with one or two blocks of PoW. Local DoS (seconds of process-lock time per response).

**Fix.** Partly fixed in M27 (local policy, D-100 item h). A block delivered by sync and refused for a reason that is
not objective is answered from memory while the BTC tip stays the same, for at most 30 seconds and 4 096 entries: per
block when the reason holds for every copy (its anchor, an anchor-invalid block's forced entries), per copy (the block
hash with a digest of its settlement transactions) when the reason is the settlement; and the body checks against the
BTC view and the settlement come before the signature checks. Still open, for a later hardening change: `net.rs` still
retries a `Btc` refusal five times with a 200 ms sleep each (now answered from memory unless the BTC tip changed), and a
sync session neither penalizes repeated blocks nor bounds its rounds other than by the peer's claimed height.

#### A2-07: cheap dead tips keep honest producers rebuilding templates

**Where.** `crates/ksana-node/src/chain.rs:2243-2336` (`template`, one `compute` per lighter tip under the DAG read
lock), the producer's staleness rule (any DAG insertion invalidates the running template), ASERT by DAA score
(`crates/ksana-consensus/src/asert.rs`).

**What was wrong.** A side block with a fresh timestamp on an old main-chain block lets its children be mined at a
much lower difficulty (ASERT sees them far ahead of schedule), down to the PoW limit. Such tips are accepted as side
blocks and relayed, can never be merged, and stay until pruning passes their parent. Every template computes GHOSTDAG
for each lighter tip, including `merge_root` and `chain_floor` walks, and every DAG insertion makes the running
template stale, so a stream of dead tips keeps honest producers rebuilding instead of mining.

**Who and cost.** A miner with a small share of the hash rate (free on devnet and signet). Local DoS on block
production; bounded by the pruning window on pruning nodes.

**Fix.** Partly fixed in the chain-base change. A template no longer tries tips beyond the merge depth of the
main-chain tip (the filter is the exact complement of GHOSTDAG's bound), and its trials share one merge-depth root, so
dead tips cost a template almost nothing. Still open: such side blocks are still relayed to every node, and any DAG
insertion still makes the running template stale.

After this report, the same side blocks can also put their headers on BTC, where they count in the header DAG of every
node and indexer until pruning even after M51 (D-135), since their parents have counted. M51 raises signet's PoW limit
from devnet's `0x207fffff` to `0x1f0fffff` (D-136): a block at the limit takes about 4 096 Argon2id evaluations instead
of about 2, whether it is relayed or landed on BTC, about 3.5 CPU-days for a full BTC block of such headers. Mainnet's
limit, `0x1f00ffff`, already asks about 65 536.

#### A2-08: entry-pool churn keeps honest producers rebuilding templates

**Where.** `crates/ksana-mempool/src/lib.rs:414-419` (replacement needs only a higher fee) and `:475` (generation
bump), `crates/ksana-node/src/producer.rs:626-634` (the solve loop goes stale after one chunk).

**What was wrong.** Every insert or replacement in the entry pool bumps its generation; the producer's solve loop
notices after one 16-nonce Argon2id chunk and rebuilds the template (per-tip GHOSTDAG, one state read per pool account
under the pool lock, merged execution). An op behind a nonce gap is never packed and never pays, yet it can be
replaced for one more unit of fee indefinitely and is relayed to every node.

**Who and cost.** Anyone with a funded account, nearly free. Local DoS on honest hash rate; a busy honest pool has the
same effect.

**Fix.** Open (local policy), for a later hardening change. Suggested: rebuild a running template on an entry-pool
change at most every few hundred milliseconds, while a new main-chain tip, a new BTC tip and the template lifetime stay
immediate; require a minimum fee increase for a replacement, as BIP125 does on BTC, and do not count a change that
cannot alter the packing candidates (an op behind a nonce gap is never one); skip the state read for accounts whose
first pending nonce has not changed since the last build.

**Update (2026-10-28).** Fixed after this report in M47 (D-130), along the lines suggested, as local policy. A
replacement must raise the sponsored fee by at least a tenth and by at least the relay floor (`Policy::replacement_fee`),
so the number of replacements a balance allows grows with its logarithm. The pool's generation moves only for a change
that can alter the packing candidates: an op behind a nonce gap or after one below the packing floor, its replacement
and its eviction leave it alone, and the node marks a change when the main-chain tip moved while an op was admitted.
Instead of a fixed delay, a pool change makes the running template stale only once it has been solved for four times
as long as building it took (`POOL_REFRESH_RATIO`), which keeps rebuilding for pool changes within a fifth of the
producer's time whatever their source; a new main-chain tip, a change of the block DAG, a new BTC tip and the template
lifetime stay immediate. The producer reads each account's nonce once per main-chain tip. Tests:
`ksana-mempool/tests/pool.rs` (the least replacement fee, the generation against the candidates, evictions, a random
workload checking that an unchanged generation means unchanged candidates) and
`ksana-node/tests/adversarial_liveness.rs` (`ops_behind_a_nonce_gap_do_not_keep_the_producer_rebuilding`).

#### A2-09: a block's own low-fee settlement holds its rescue back, and nothing raises a stuck fee

**Where.** `crates/ksana-node/src/rescue.rs` (`rescue_settlements`: the check of the block's own attached pair;
`honors`; `keep_alive`), `crates/ksana-node/src/main.rs` (`--settlement-feerate`, default 2 sat/vB).

**What was wrong.** Rescue skips a block whose own attached settlement is in the BTC mempool, or could still enter it,
with no fee-rate or patience check; the first audit's F-38 fix (`honors`, `CLAIM_PATIENCE`) applies only to other
parties' claims. A producer that attaches a settlement at the minimum relay fee during a fee spike (on purpose or not)
leaves the block's data unlanded for as long as the pair stays unconfirmed, and the backlog limit stops path B on
every node after `SETTLEMENT_TIMEOUT` BTC blocks.

More generally, no settlement or rescue ever raises its fee. The fee rate is a fixed operator setting; SPEC §7.2 lets
the producer raise it by `estimatesmartfee`, but no code does, and `keep_alive` only rebroadcasts. Other nodes replace
a rescue that pays less than their own rate, or one older than `CLAIM_PATIENCE` BTC blocks, but at their own fixed
rate, so during a fee spike above the common default every rescue waits until fees fall. CPFP through the reveal's P2A
output is possible, but only by hand. M27 adds one more case that needs a rescue: a settlement that a BTC reorg mines
again at or below its block's anchor no longer counts (the fix of A2-02). Later blocks' settlements wait for it only
while its block's anchor is within `IN_FLIGHT_BTC_BLOCKS` (2) BTC blocks of the tip (settlement batching, D-077); the
settled closure waits until it confirms.

**Who and cost.** Any producer, at the cost of the pair's eventual fee, or ordinary fee-market conditions. Liveness,
network-wide, temporary.

**Fix.** Open (local), to fix before mainnet; M27's rescue changes do not touch this branch. Suggested: treat a block's
own attached pair in the BTC mempool like another party's claim (`honors`, `CLAIM_PATIENCE`): when it pays less than
this node's fee rate, or has stayed unconfirmed for `CLAIM_PATIENCE` BTC blocks, rescue the block at this node's rate.
Estimate the fee rate (`estimatesmartfee`, as SPEC §7.2 already allows) instead of a fixed setting, and bump this node's
own settlement or rescue through its P2A output when it stays unconfirmed for a few BTC blocks. The `chain.rs` comment
that calls a stuck own settlement "handled by rescue and the backlog limit" should change with it.

**Update (2026-10-08).** Fixed after this report in M28 (D-102), along the lines suggested. A block's own pair waiting
in the BTC mempool holds a rescue back on the terms of another party's claim: only while it pays this node's fee rate,
and for `CLAIM_PATIENCE` BTC blocks; its producer bumps it instead of rescuing it. The fee rate is bitcoind's
`estimatesmartfee` for `--settlement-conf-target` (default 3 BTC blocks) kept between `--settlement-feerate` and
`--max-settlement-feerate` (default 100 sat/vB), with the next-block target while a lighter competing chain is on BTC
(D-091). A pair this node sent that waits `BUMP_AFTER` (2) BTC blocks below that rate (at once while a lighter competing
chain is on BTC) gets a CPFP child through the reveal's P2A output, replaced by a richer one when the rate rises again.
The `chain.rs` comment changed with it. Residuals, from the review of M28 (D-102 limitations): other nodes judge a
waiting pair by its own fee rate and do not count a CPFP child, so they may still rescue a bumped settlement after their
delay (an extra fee, not a liveness problem); and anyone can pin a pair by attaching a large low-rate child to the
reveal's P2A output first, after which this node's child must pay more than that child's whole fee (BIP125 rule 3) and
is refused every round, so the data lands only through another node's rescue, which can be pinned the same way (a lone
producer never rescues its own pair). The pinning residual stayed open (see the update of 2026-10-28).

**Update (2026-10-28).** The pinning residual is closed in M48 (D-131), as local policy with no consensus change. When
bitcoind refuses this node's first CPFP child of a pair while the reveal waits in the BTC mempool and another party's
transaction spends its P2A output, the pair's payload goes out again, byte for byte, in a new pair funded by this node at
the bump's rate. The new pair spends nothing of the old one's, so the pinning child cannot hold it back; it takes the old
pair's place as the block's rescue (the producer's own settlement included) or as the republish, and is bumped in turn;
a pair whose block's data has landed is no longer bumped. TRUC (version 3) transactions were weighed and do not fit: the
pair's version and the reveal's outputs are consensus, and TRUC's limits leave no room for commit, reveal and child.
Residuals (D-131 limitations): after a restart this node's own earlier child looks like another party's and costs one
extra pair, a helpful child of another party's is treated like a pinning one, and each pin costs a pair at the bump's
rate while rates rise, bounded by the cap.

#### A2-10: headers with unknown parents from BTC payloads are held unchecked and without a size limit

**Where.** `crates/ksana-btc/src/headers.rs:426-433` (held pending with no check), `:264` (dropped only when pruning
passes the header's own anchor), `:216` (cloned into every checkpoint); `crates/ksana-btc/src/bodies.rs:292-302`
(candidate bodies collected for every header hash, no cap per hash); `crates/ksana-primitives/src/compact.rs:48` (255
parents per header in the codec).

**What was wrong.** A header whose parents are unknown is held with no PoW, parent-count, version or bits check until
the pruning point's anchor passes the header's own anchor, which the attacker sets just below the payload's height.
Back-referenced parents cost one or two bytes in a payload and 32 bytes plus an index entry in memory.

**Who and cost.** Anyone paying BTC fees: roughly 150 to 250 MB on every node and indexer per BTC block of such
payloads (estimated from type layouts), held for about a day, forever with `--no-pruning`, and written into every
checkpoint. Local DoS (residual of the first audit's F-25 and F-35).

**Fix.** Open (local), for a later hardening change. The chain-base PR's PoW-first order (A2-05) applies only to headers
whose parents are all known; M27 caps a payload at 500 headers, which the estimate above already assumes. Suggested: run
the context-free checks before holding a header pending: parent count, distinct parents, version, `bits` within the PoW
limit, and the PoW against the header's own `bits` at weight 1, which every valid header passes (its effective target is
the largest); cap the pending set by count and bytes; collect candidate bodies only for headers the DAG holds; cap the
candidates per hash and in total.

Fixed after this report in M51 (D-135), in the protocol rather than locally, since a full cap on the pending set has to
drop headers, valid ones the finality determination may wait for included. A header in a payload that counts now
counts only when its anchor is on the BTC best chain, it passes the header checks and its parents have counted, within
one BTC block in any order; an appearance that does not count leaves no trace, so nothing is held pending and no
payload waits to be resolved. A body lands only with its counted header, checked against `entries_root` when its BTC
block is scanned, and landed bodies are kept as the bytes of their transactions, one copy per transaction. Devnet and
signet restart from genesis.

#### A2-11: the CLI, SDK and web app default to devnet and never check the node's chain ID

**Where.** `crates/ksana-cli/src/main.rs:53-54` (`--chain` defaults to devnet; nothing calls `get_chain_info`),
`sdk/ts/src/topUp.ts:273`, `sdk/ts/src/pathC.ts`, `sdk/ts/src/codec.ts` (`params` defaults to `DEVNET`),
`web/src/chain/rpc/chain.ts:297-298` (anything other than signet is treated as devnet).

**What was wrong.** A buyer who runs `top-up` against a mainnet or signet node without `--chain` signs the claim with
devnet's chain ID. The data output's magic is the same on every network, so the node force-includes the native
transaction, the op fails on its chain ID, and the payment to the seller's script confirms anyway.

**Who and cost.** No attacker is needed; a dishonest seller simply keeps the BTC. The buyer loses the payment.

**Fix.** Open (client side), to fix before mainnet. Suggested: the CLI reads the node's chain ID (`ksana_getChainInfo`)
before any command that signs and refuses a mismatch, or takes the network from the node instead of defaulting to
devnet; the SDK makes `params` required where it now defaults to `DEVNET` (`buildTopUp`, `buildNativeTx`,
`nativePayload` and the codec functions), or offers a helper that picks them by the node's chain ID; the web app checks
the chain ID when it connects and supports mainnet or refuses it.

**Update (2026-10-13).** Fixed after this report in M31 (D-106), along the lines suggested. The CLI takes the network
from the node: every command but `key` first reads the node's chain ID (`ksana_getChainInfo`) and signs for that
network, refusing a chain ID of no network; `--chain` no longer defaults to devnet and only states the network
expected, refusing a node on another one before anything is signed or sent. Path C and `top-up` also refuse a bitcoind
on another BTC network (`getblockchaininfo`) before anything is funded, since there the payment is real and the node
never sees it. In the TypeScript SDK nothing defaults to a network: `params` is required by `buildTopUp`,
`buildNativeTx`, `nativePayload` and the codec functions, the generic codec no longer encodes a payload, and
`KsanaRpcClient.getParams` / `paramsForChainId` pick the parameters by the node's chain ID and throw
`ChainMismatchError` on a mismatch; `buildNativeTx` refuses an op signed for another chain ID. The web app checks the
node's chain ID when it connects and again before anything is signed, offers to switch to the node's network, and
refuses mainnet until it supports it. Like the CLI, it also checks the network of its Bitcoin RPC, which the review of
the fix found it did not: the page lists the wallet's UTXOs
there with a `scantxoutset` `raw()` descriptor, which matches the same script on every network, and broadcasts there,
so with a bitcoind on another network the wallet would sign and the transaction, for a top-up a real payment to the
seller, would go where the node never looks. The page now reads `getblockchaininfo` before every listing of coins and
every broadcast, and before path C or a top-up is signed, and refuses a `chain` other than its network's (regtest for
devnet, signet for signet), saying why. The review of that increment (same file) found the same gap in the wallet's
own network on the signet page, as an optional item since only test coins were at stake: `tb1` is testnet3's,
testnet4's and signet's alike, so a Unisat set to a testnet passed the address check, and without a Bitcoin RPC the
page listed that testnet's coins through Unisat and broadcast there. In a follow-up the page asks the wallet which BTC
network it is set to (Unisat `getChain`, Xverse `wallet_getNetwork`), refuses one set to another network when it
connects and before path C or a top-up is signed, and uses Unisat's own listing and broadcast only for the connected
Unisat wallet once it names the page's network. Tests: `crates/ksana-cli/tests/chain.rs` (a node on another network or
on none refused with nothing submitted, a bitcoind on another BTC network refused before funding),
`sdk/ts/test/network.test.ts`, `sdk/ts/test/wallet.test.ts` (the network each wallet says it is set to),
`web/src/chain/rpc/chain.test.ts`, `web/src/wallet/btc.test.ts` (a Bitcoin RPC or a wallet on another BTC network
refused before coins are listed, anything is broadcast or the wallet is asked to sign).

#### A2-12: a fresh indexer holds the whole history in memory before it first prunes

**Where.** `crates/ksana-indexer/src/indexer.rs:632-676` (`sync_once`: the BTC tracker is drained to the tip before
`process` and `maybe_prune` run).

**What was wrong.** An indexer that starts from the genesis anchor, or starts over after a deep BTC reorg, scans all
of BTC first, then executes every block, and prunes only at the end of that first round. Until then its BTC snapshot,
header DAG and main-chain window hold the whole history, and the backlog and settled-prefix walks run from height 1.
The node does not have this problem: it prunes after each 500-block BTC round.

**Who and cost.** No attacker: any new indexer on a long chain. Memory and time grow with the chain's age.

**Fix.** Fixed in the chain-base change. `Indexer::sync_once` runs the finality decision, the processing and the
pruning after every tracker round of at most 500 BTC blocks, as the node does. An indexer restarted from a checkpoint
still scans to the BTC tip before it verifies its state store and prunes, so its memory then grows with its downtime.

### Low

| ID | Finding | Where | Status |
|----|---------|-------|--------|
| A2-13 | After a BTC reorg at least `ANCHOR_DEPTH + NATIVE_CONFIRMATIONS` deep (5 on mainnet), a forced-only block whose anchor was reorged out can only be derived from the new chain's window; nodes and indexers that saw it before keep its cached body, a fresh one marks it invalid or waits for it. A forced-only body cannot be republished, so SPEC §17.3's "heals once the body is republished" does not apply. | `crates/ksana-btc/src/bodies.rs` (`check_stale_entries`, `stale_body`); `crates/ksana-node/src/chain.rs` (`check_body`) | Open. SPEC §17.3 lists the deep-reorg limits, but its D-056 row still says the pause heals once the body is republished, which a forced-only body cannot be. |
| A2-14 | Native transactions are looked up by ID with a linear scan: in anchored execution over every native transaction kept up to the anchor height, once per forced entry, which includes every template build (`merge_hazard`) and lenient execution; for an anchor-invalid block over its `stale_range`, once per forced entry on acceptance and in the indexer. Up to about 2 400 forced entries per forged block times the native transactions in range. | `crates/ksana-btc/src/tracker.rs` (`native_tx_at_most`, `native_tx_in`, `AnchoredView::native_input`); `crates/ksana-btc/src/bodies.rs` (`check_stale_entries`, `convert_by_txid`) | Open. Since M27 a refusal on acceptance is remembered for 30 seconds while the BTC tip stays the same (A2-06), and an anchor-invalid block's lookups cover only its `stale_range` instead of the whole snapshot. |
| A2-15 | `BtcSnapshot::unresolved` (payloads naming blocks this node does not know yet) has no cap, and `resolve_all` probes every pending key, including inside `accept_one` under the process lock; the indexer re-runs it and the body re-derivation of every undetermined block on each 500 ms poll under its known lock. Bounded by pruning and BTC fees. | `crates/ksana-btc/src/tracker.rs:1460-1470`, `:1338-1358`; `crates/ksana-node/src/chain.rs:3048` | Open. Closed after this report in M51 (D-135): nothing waits to be resolved any more, the unresolved payloads, `resolve_all` and the undetermined bodies are gone. |
| A2-16 | Compact header decoding is still quadratic in the canonical-reference check (the first audit's F-33): about 16 million 32-byte comparisons for a 500-header payload, reachable without BTC fees through the attached settlement of a block sent over P2P, under the process lock. | `crates/ksana-primitives/src/compact.rs:157-180`; `sdk/ts/src/compact.ts:124-132` | Open. M27 checks the header count (at most 500) before decoding; the figure above already assumes that cap. |
| A2-17 | The TypeScript SDK recognizes a native transaction by its outputs only and ignores D-081 (a transaction with a KSN1 envelope in an input and a KSN1 output has two carriers and is not recognized by nodes), so it can report a path C op that nodes will not execute. | `sdk/ts/src/pathC.ts:136-165` against `crates/ksana-btc/src/data.rs:129-174` | Open. |
| A2-18 | The state store prunes a whole jump of retained versions (at least `PRUNE_STEP`) in one RocksDB write batch inside the chain commit, and a pruning error after the commit is reported as a failed connect. | `crates/ksana-state` pruning, called from `crates/ksana-node/src/chain.rs` (`commit_chain`) | Open. |
| A2-19 | A local fault in the middle of connecting pending headers drops the children already taken out of the pending set; their payloads are not scanned again, so this observer's header DAG lacks them until someone republishes. Trigger: a local fault only. | `crates/ksana-btc/src/headers.rs:144-162`, `:439-448` | Open. Closed after this report in M51 (D-135): no header is held pending, a header joins the DAG only once its parents have, and a local fault fails the BTC sync round, which scans the block again. |
| A2-20 | The `REORG_WINDOW` refusal stops at the first candidate too deep to switch to instead of trying the next one, and a forced switch that keeps failing repeats its whole cost on every call (a full re-execution from genesis with `--no-pruning`). | `crates/ksana-node/src/chain.rs` (`best_candidate`, `rewind_deep`) | Open. |
| A2-21 | One fork block landed on BTC between a node's highest BTC-connected main-chain block and its tip makes every funded node republish up to its tip right after `COMPETITOR_GRACE` (D-091), with no coordination between nodes: each pays for its own copy. | `crates/ksana-node/src/chain.rs:1756-1799`; `crates/ksana-node/src/rescue.rs:750-771` | Open. |
| A2-22 | The indexer writes `indexer.json` with a write and a rename but no fsync, unlike its checkpoint; after a crash a torn file stops it from starting, and deleting the file then hits the "state store not empty" error. | `crates/ksana-indexer/src/meta.rs` (`Meta::save`) | Open. |

### Info

- DAA score counts the weight of red merged blocks too, so honest merges of cheap side blocks raise honest difficulty
  slightly (`ghostdag.rs`; not quantified).
- The finality walks (`extends_tip`, `advance` in `crates/ksana-btc/src/finality.rs`) are linear in the distance from
  the finalized tip, which only grows while the finality decision pauses.
- After pruning, delivering the genesis block marks the genesis hash invalid, so later re-deliveries of height-1
  blocks read "parent is invalid" instead of duplicates; the in-memory invalid set can be cycled before any PoW check.
- D-097 item 5 says old data directories get `dag_seq` written back; the code writes back `summaries` and `accepted`
  only (moot for this release: every network restarts from genesis).
- Orphans can be dropped or stranded when a local fault interrupts their parent's acceptance; the checkpoint is built
  and synced to disk under the process lock.
- One BTC payment can claim locks in two exchange instances that share a payment script (a seller misconfiguration).
- `borsh-js` string decoding may throw on very long strings (not confirmed).
- Documentation: SPEC §7.7 says a republish's change returns to the hot wallet, while the code applies the D-060
  revenue rule to republishes too; a comment in `chain.rs` says a stuck own settlement is "handled by rescue and the
  backlog limit" (A2-09); SPEC §17.4 and the simulations understate the cost of `GHOSTDAG_K` 16 (they model no
  propagation delay); the exchange wording in `docs/ABI.md`, D-098 d against SPEC §16, the `payBy` wording and the
  header comment of `pathC.ts` drifted from the code; the development log still gives `min_pruning_depth` as 2 185 (the
  `GHOSTDAG_K` 8 value) in one place. SPEC §7.5's claim that a block's body always fits a later payload did not
  follow from D-100 b as audited (A2-03); M27 corrected both the rule and the text.

## Status of the first audit's findings

The first internal audit (2026-10-06, `main` at `ef07411`) reported F-01 to F-42 and three simulation results, S-01 to
S-03. Its fixes landed in M26 (D-098) and M27 (D-100). Status as of `5819b26`:

| ID | First audit (short) | Status | Notes |
|----|---------------------|--------|-------|
| F-01 | Body size not bounded by consensus | Fixed (M27, D-100 b) | The bound and the landing code first disagreed (A2-03, fixed in M27) |
| F-02 | Forged-anchor block via sync: forced entries unchecked | Fixed (M27, D-100 c) | Forced entries are checked on acceptance in both modes; A2-01 and A2-04 fixed in M27; deep BTC reorgs remain (A2-13) |
| F-03 | Tracker copies a whole BTC transaction per unknown carried body | Fixed (M26) | Residual A2-15, closed in M51 (D-135) |
| F-04 | GHOSTDAG ancestor walk before merge depth and PoW | Fixed (M26, chain-base change) | The chain-base change closed the residual (A2-05): PoW first, memoized merge-depth walks |
| F-05 | Orphan pool without PoW, parent-count or byte bounds | Fixed (M26) | Orphans need PoW at their own bits, a valid parent list and a fitting body; capped at 256 blocks and 32 MiB. No per-peer share |
| F-06 | Exchange: a dust payment closes a lock | Fixed (M26) | See the info item on two exchange instances sharing a script |
| F-07 | Cold compiles priced at 1 gas per byte, FIFO cache | Open | Benchmark first; pricing belongs to the SPEC §13.1 gas calibration |
| F-08 | Indexer halts on a bad body of a non-final block | Fixed (M26) | Only a BTC-final block halts; since M27 an anchor-invalid block judged invalid on re-execution is a local fault (SPEC §17.3) |
| F-09 | Header DAG accepts headers anchored at or above their payload's height | Fixed (M27, D-100 d) | Its acceptance side was first missing (A2-02, fixed in M27) |
| F-10 | Unbounded ancestor walks when checking carried bodies | Fixed (M26) | The past is collected once per block, bounded by the merge-depth root |
| F-11 | Republish budget ignored the 32-byte payee | Fixed (M26) | |
| F-12 | Entry pool filled by ops behind nonce gaps | Fixed (M26) | Residual A2-08, fixed in M47 (D-130) |
| F-13 | Producer shifts the cost of landing its block to rescuers | Documented (SPEC §7.3) | |
| F-14 | `ksana_call` without a gas cap | Fixed (M26) | |
| F-15 | Top-up trusts an unverified exchange contract and node | Open | |
| F-16 | `MAX_SETTLEMENT_HEADERS` not checked when decoding | Fixed (D-100 e) | Rust and TypeScript |
| F-17 | A copied path C op hides the real op's status | Open | Also: a disconnect deletes the op-index entry without checking for another inclusion |
| F-18 | TypeScript SDK strips a leading BOM in method names | Fixed (M26) | |
| F-19 | BIP322 message is a bare sighash | Documented (SPEC §16) | |
| F-20 | Merkle proofs: CVE-2012-2459 duplicate position | Documented | A code fix stays open |
| F-21 | Exchange payment script can be squatted | Documented | The SPEC §16 row is missing |
| F-22 | CLI top-up: fixed fee rate, no payment deadline shown | Open | See also A2-11 (fixed after this report in M31: the claim is signed for the node's network) |
| F-23 | Indexer wipes its state on restart when bitcoind is behind | Fixed (M26) | It waits until bitcoind reaches its oldest kept version, then verifies by state root; since the chain-base change processing after a restart also waits for the scan to reach the BTC tip |
| F-24 | Web mock wallet key in the query string | Open | |
| F-25 | Unbounded sets (pending headers, invalid set, `best_candidate`, `headers_seen`) | Partly fixed; fixed after this report in M51 (D-135) | Invalid set capped; `best_candidate` uses a tip index; `headers_seen` pruned; since the chain-base change templates skip tips beyond the merge depth (A2-07). Open at this report: pending headers (A2-10), which M51 removes |
| F-26 | Header DAG time checks not repeated after a BTC reorg | Documented (D-098); settled after this report in M51 (D-135) | A header counts on the current BTC chain only with its anchor on that chain, so its time bound is the one already checked |
| F-27 | Header DAG maps `GhostdagError::Corrupt` to invalid | Open | `headers.rs` `From<GhostdagError> for Reject`; the node maps the same error to a local fault |
| F-28 | P2P: no connection limits, banned peers' queued gossip, sync server concurrency | Partly fixed (M26); fixed after this report in M49 (D-132) | Banned peers' gossip is dropped. Open at this report: connection limits, per-peer buckets, ops decoded with the 16 MiB limit, the shared queue, unbounded sync-serve tasks, `GetMempool` responses up to about 32 MiB; M49 bounds them all (connections by address, quotas by address that outlive connections, ops within `MAX_OP_BYTES`, budgets per topic and per source, serving 4 at a time within a byte budget, `GetMempool` responses within 4 MiB). The orphan pool's share per peer stays open (F-05) |
| F-29 | RPC: unlimited batches and bodies, `getAccount` cost | Open; fixed after this report in M49 (D-132) | Batches of at most 64 calls, bodies of 1 MiB, 16 subscriptions per connection, for the node and the indexer; `getAccount` reads a balance per asset and finds carriers in an index of the window |
| F-30 | Secrets on the command line, key files unchecked | Open | |
| F-31 | SPEC text on the rescue's change output | Fixed (SPEC §7.7) | The republish sentence next to it still differs from the code |
| F-32 | VM: event pricing, sticky `limit_hit`, bulk-memory tests | Open | |
| F-33 | Super-linear compact header decoding | Open | Bounded by the 500-header cap; see A2-16 |
| F-34 | TypeScript `decodePayload` defaults to devnet | Open; fixed after this report in M31 (D-106) | `params` is required; see A2-11 |
| F-35 | Indexer candidate bodies and pending headers unbounded | Partly fixed; fixed after this report in M51 (D-135) | Dropped at each pruning step; no size bound at this report (A2-10); M51 holds neither |
| F-36 | Duplicate block messages trigger `update_virtual` | Open | Cheaper now (tips index) |
| F-37 | Anchor-invalid heavier side chain retried forever | Open | By design (D-056): a state root mismatch or an invalid verdict on re-execution in anchor-invalid mode is a local fault after the block entered the DAG, so the switch is tried again on every update |
| F-38 | A fake settlement claim in the BTC mempool stops every rescue | Fixed (M26) | Residuals: the patience window restarts per claim and on restart; a block's own pair (A2-09, fixed after this report in M28) |
| F-39 | Work per block linear in the chain length | Partly fixed (D-097) | Bounded by the pruning window, since the chain-base change also while an indexer catches up (A2-12); open with `--no-pruning` |
| F-40 | Settlement `nSequence` not checked | Fixed (D-100) | |
| F-41 | The payload sender is taken from the last header | Documented (D-098) | |
| F-42 | One log line per invalid header | Open | At most 500 lines per payload since the header cap |
| S-01 | Payment reversal with heavy empty blocks | Re-tuned (D-100, `GHOSTDAG_K` 16) | Tables in SPEC §17.4 |
| S-02 | Withholding light blocks on an idle chain | Re-tuned (D-100 fork choice by chain work, `GHOSTDAG_K` 16) | Honest reward lost at 10 % attacker hash rate: busy 0.02 %, idle 2.9 % |
| S-03 | Lock without paying | Documented | |

## Checked and found sound

The points below were traced and hold on the audited head; they are listed so a later review knows what was looked at.

- **Fork choice (D-100 a).** Every place that chooses a tip keys on `chain_work = blue_work + work` with the hash as
  tie-break: GHOSTDAG's selected parent, the node's and the header DAG's `best_candidate`, finality's candidate, the
  producer, the P2P `Status`, `btc_competitor` and the simulations. `blue_work` alone is used only to order blocks
  (merge sets, sync cursors, carrying). Chain work strictly increases along a selected chain, so the early stops over
  the tips index are sound, and blue work still increases along every parent edge, so merge-set topological order
  holds.
- **GHOSTDAG and the merge-depth rule (D-097).** k-cluster coloring matches SPEC §6.6; the merge-depth root's
  boundaries are exact (equal blue work, root at genesis, root at the selected parent); pruned parents are treated as
  below the root everywhere; `min_pruning_depth = 3 × (MERGE_DEPTH + GHOSTDAG_K) + 1` leaves the margin the root walk
  needs. A full-versus-pruned equivalence test exists and passes.
- **Pruning (D-097).** The pruning point, the header DAG, finality, candidate bodies and the BTC snapshot are pruned
  together under the process lock with checks before any change; the checkpoint is written atomically; a restart uses
  it only when the root, the state version and the main-chain window are all consistent, and otherwise replays.
  Everything a block in the pruning point's future needs from the snapshot is kept, except what A2-01 describes.
- **Finality (D-082).** Incremental evaluation equals evaluation from scratch; a pause redoes the same height; a BTC
  reorg rolls back; node and indexer reach the same "not landed yet" verdicts (D-093).
- **Payload counting (D-100 d).** The scanner applies `payload_counts` once, before every consumer, in the code shared
  by node and indexer, so landings, the header DAG, candidate bodies and the carrier see the same payloads (the missing
  acceptance side is A2-02). The anchor time (D-083) is always available when a header enters the header DAG.
- **Settlement transactions.** The structure, `nSequence`, inputs, outputs, envelope, commitment, signatures and fee
  floor are checked on reception and on sync alike; only the mempool test is reception-only, as SPEC §6.3 intends.
- **Execution.** A skipped forced entry writes nothing and consumes no nonce; a duplicate fails on its nonce;
  `check_forced_prefix` demands exactly the window; unlock checks run before the view is queried; reward splits add up
  to the slot total, saturate, and stop at `KSA_MAX_SUPPLY` in the order share, carrier, producer; empty blocks write
  nothing; the genesis checks (duplicates, overflow, supply cap, placeholder share account) hold.
- **Node.** Only `InvalidBlock` disqualifies; a forced switch must pass through the finalized block and restores the
  old chain on failure; `GhostdagError::Pruned` never becomes `InvalidBlock`; the orphan pool checks PoW at the
  orphan's own bits and is capped; state versions cover every switch the rules allow.
- **Producer, rescue and P2P.** The entry pool's byte accounting, eviction order and replacement rule; the anchor and
  timestamp bounds and `body_room` arithmetic of templates; the producer's self-checks of its settlement; sync-server
  caps on `GetBlocks`, `GetMempool`, locators and tips; `ksana_call`'s gas cap and concurrency limit; RPC bound to
  localhost with CORS off by default.
- **Encoding.** Length checked before decoding, full consumption, minimal varints, bounded preallocation, the key
  dictionary, strict UTF-8 method names, canonical `check_bodies`; the Rust and TypeScript codecs agree on all of
  these (the native-transaction recognition gap is A2-17).

## Tests and runs

- Unit and property tests of `ksana-primitives`, `ksana-consensus` (the two long simulation tables excluded) and
  `ksana-btc` on the audited head: all passed.
- Repro tests written for this audit, run on the audited head (not committed; each fix carries its own regression
  test):

  | Finding | Test | Result on the audited code (`11f0576`) |
  |---------|------|----------------------------------------|
  | A2-01 | `ksana-btc` unit test: one forged header and entries on an unpruned and a pruned snapshot | `Ok` versus `Missing` |
  | A2-02 | `ksana-btc` unit test; `ksana-node` test in `adversarial_liveness.rs` | the payload belongs to the block and never counts; the fast path stays paused |
  | A2-03 | `ksana-btc` unit test; `ksana-node` test in `adversarial_liveness.rs` | 81 864 exact against 149 712 upper-bound bytes; the body never lands and the fast path stays paused |

- Long runs on the owner's Mac: a 5-hour devnet soak of M25's head `dd24020` passed (18 000 s, 2 301 path B and
  1 148 path C ops without a failure; block hashes and state roots equal on three nodes and the indexer at every height
  checked, up to height 4 470; no restart or alert; pruning at 3 h 26 min and again about 1.5 h later, on all four
  services in the same second; checkpoints of about 4.1 MB per node and 3.4 MB for the indexer; resident memory flat
  at 129 to 162 MB per node and 72 to 80 MB for the indexer).
- A 10-hour devnet soak of `main` at `1ea0150` (the code of `5819b26`, with the devnet image built from `912a506`, the
  same tree) passed on 2026-10-08, on the Mac on AC power throughout (load 5.4 on 8 cores at the start; no thermal or
  CPU speed limit recorded): 36 000 s, 4 063 path B and 1 989 path C ops without a failure; block hashes and state
  roots equal on three nodes and the indexer at the end, at height 7 271; no restart; 68 main-chain switches, all of
  depth 1, and no BTC reorg; four pruning steps on all four services, each in the same second, at 3 h 26 min and then
  1.5 to 1.8 hours apart, each about 1 000 blocks with the pruning point about 2 200 heights below the root;
  checkpoints of about 4.15 MB per node and 3.5 MB for the indexer. The fast path paused three times (D-046), for 8 to
  11 s each while the Mac's load was 6.5 to 8.7, without a failed op; their alert lines are the only hits of the log
  checks that must stay at 0 on an idle machine (no block was invalid, no block production failed and no settlement
  confirmed without counting). Resident memory changed little at each pruning step, but on the nodes it kept rising,
  from about 150 MB at the first step to 166 to 192 MB at the end; the indexer stayed at 80 to 97 MB. Why the nodes'
  memory keeps rising after pruning is not known yet.
- The first 10-hour attempt, on `5819b26` on 2026-10-07, stopped after 62 minutes. The Mac was heavily loaded (load 20
  to 71) and ran mostly on a battery that drained from 30 % to 1 % because its charger could not keep up. From about
  minute 40, settlement timeouts (D-046) paused the fast path more and more often, and at minute 62 an op waited longer
  than the CLI's 300 s during two pauses that came almost back to back. Up to then 549 ops had passed without a failure,
  and the four services agreed at height 1 119. Queries on its logs found no regression: every regression signal in the
  node logs was at 0, the first hour had 35 main-chain switches against 34 for `dd24020`, all of depth 1, block
  production matched `dd24020`'s up to BTC height 300 and fell only after that, and the settlements of the blocks the
  alerts named went out 6 to 12 BTC blocks after their carriers' anchors, each alert stopping within one BTC block of
  that broadcast. This is the devnet timing effect of D-067: a settlement goes out only once `SETTLEMENT_DEPTH`
  fast-lane blocks follow its carrier, which on a loaded machine, with the wait for a BTC block, can outlast the 60 s
  `SETTLEMENT_TIMEOUT` window (6 BTC blocks of 10 s on devnet), and during a pause only ever-heavier empty blocks
  (D-090) can supply that depth. The owner chose to keep this behavior on devnet, since mainnet and signet, with
  10-minute BTC blocks, are not affected.
- Checks of the shipped code: M27 passed the CI-only jobs on the owner's Mac (two-process determinism with the
  golden state roots unchanged, fuzzing, the three-node devnet smoke test with a short soak, the web end-to-end test)
  and the full regtest suite; the audit's re-check tests of A2-01 to A2-03 and the affected suites pass on M27's fixes
  (`ksana-btc` 52 unit tests, `ksana-execution` merge 11 and unlock 11, `ksana-node` adversarial_liveness 13 and
  adversarial_blocks 9, `ksana-indexer` 22). For the chain-base change a differential test ran its GHOSTDAG against M27's, with the
  fork-choice key also compared after every step: identical over 900 random DAGs (about 282 000 accepted blocks,
  47 000 prunes, 49 000 checkpoint restores, 880 000 template-style trials and 1.65 million fork-choice checks); the
  merged code is byte-identical to the code that test ran on. On 2026-10-07 `main`'s code (`5819b26`, the same tree
  as the chain-base change's head `912a506`) passed the CI-only jobs on the Mac: the determinism job on aarch64 (478 tests, golden
  state roots unchanged), fuzzing (5 targets for 60 s each, no crash), the three-node devnet smoke test (three
  producers, up to height 12) with its 180 s soak (20 path B and 11 path C ops without a failure), the web end-to-end
  test and the full regtest suite, run alone (35 of 35, no timeout or rerun).

## Appendix: parameters used in the analysis

| Parameter | Mainnet | Signet | Devnet |
|-----------|---------|--------|--------|
| `NATIVE_CONFIRMATIONS` | 3 | 1 | 1 |
| `ANCHOR_DEPTH` | 2 | 1 | 0 |
| `MAX_ANCHOR_LAG` | 1 | 1 | 1 |
| `GHOSTDAG_K` | 16 | 16 | 16 |
| `MERGE_DEPTH` | 720 | 720 | 720 |
| `min_pruning_depth` | 2 209 | 2 209 | 2 209 |
| `MAX_BLOCK_DATA_BYTES` | 80 KiB | 80 KiB | 80 KiB |
| `MAX_SETTLEMENT_HEADERS` | 500 | 500 | 500 |
| `SETTLEMENT_TIMEOUT` | 6 | 6 | 6 |
