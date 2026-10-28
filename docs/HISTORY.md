# History

Ksana was developed in a private repository from 2026-09-30 until it was published. This repository starts from one
commit made at publication; the private repository's commits and pull requests are not published. What carries the
history over:

- the specification's version history ([SPEC](SPEC.md), at its top) and its milestone table (SPEC §14);
- the design decision log, D-001 to D-133 ([DECISIONS](DECISIONS.md)), which records each change's problem, the options
  weighed, what was chosen and why, and what it touched;
- the second internal audit's report ([audits/](../audits)).

Decisions and milestones (D-xxx, Mxx) are cited throughout the code and the documents; this page puts them in order.

## Before the freeze: 2026-09-30 to 2026-10-05

The first specification (v1, 2026-09-30) described a fast lane on Bitcoin with three ways to submit an operation. The
milestones M0 to M5 built a demonstrable minimum: the workspace and CI, the VM and its state, a single-node fast lane,
settlement on Bitcoin, a three-node network and the indexer. M6 to M15 then reshaped most of it:

- The fast lane became a GHOSTDAG block DAG, so that parallel blocks are merged rather than wasted (M7, D-063), with
  difficulty by DAA score and blocks back at 5 seconds after a trial of 1 second (M10).
- The contract model was aligned with the EVM's: attached funds, catchable calls, delegate calls, contracts that create
  contracts (M8, D-073). KSA got a max supply and block rewards paid when a block's data lands on Bitcoin (M9, D-074),
  and the network an upgrade mechanism by DAA score (M11, D-076).
- One settlement transaction could carry many blocks (M12, D-077); path A, a merged payment in BTC, and fee debt were
  removed in favor of topping up KSA with one BTC transaction (M13, D-078); empty blocks stopped changing the state, and
  payloads moved into a Taproot witness envelope (M14, D-079 to D-081).
- Finality became a function of Bitcoin's chain data, timestamps were bound to the anchored Bitcoin block, and empty
  blocks got a difficulty of their own (M15, D-082 to D-084). SPEC v2 merged the decisions back into the text on 2026-10-03.

M16 to M24 settled what was still open: the KSA issuance and the block share (D-089), empty headers that keep landing
on Bitcoin (D-090), accounts that are Taproot addresses (M17, D-092), Argon2id as the mining algorithm (M22, D-094), and
a reward for whoever lands a block's data on Bitcoin, the carrier (M23, D-095). The web wallet and block explorer
joined the main line (M20, M21). On 2026-10-05 the project owner signed off and the consensus was frozen as SPEC v2
final, with the mainnet parameters and the mainnet genesis template (M24, D-096).

## Hardening: 2026-10-06 to 2026-10-12

- Bounded memory: the merge-depth root rule and pruning of the block DAG, the state and the Bitcoin tracker in the node
  and the indexer (M25, D-097, D-103).
- Two internal audits on 2026-10-06. The first one's local fixes (M26, D-098) and the consensus fixes the project owner
  adopted (M27, D-100) landed; the second one's report is in `audits/`, with the status of each finding.
- Only a block judged valid becomes final (M28, D-101); the node's Bitcoin fees follow the market with CPFP bumps
  (D-102); a deep Bitcoin reorg takes the blocks anchored on the blocks it orphans off the main chain (M29, D-104).
- Halving periods that double in length, so that the subsidy paying for landing data on Bitcoin lasts decades longer
  (M30, D-105).

## The public signet: 2026-10-13 to 2026-10-18

Clients take the network from the node they talk to (M31, D-106). Ksana's public test network started on Bitcoin's
signet, on one server with a seed node, an indexer, the web wallet, a Bitcoin proxy and a KSA faucet (M32, D-107,
D-109, D-112); the node learned to list an account's operations (M35, D-111); the project's site went up at
ksana.network (M37, D-113); a block is reported final only once the Bitcoin block that determined it is deep enough
(M38, D-114); the node's settlement fees drop to the floor while Bitcoin's blocks have room (D-117).

## Contracts and tokens: 2026-10-19 to 2026-10-27

The contract playground at playground.ksana.network, with a compile service beside the signet (M39, M40, D-118,
D-119); the token standards KS-20 and KS-721 with their reference implementation in the contract SDK, and receipts that record
changes to built-in assets (M41, D-120, D-122); issuer capabilities for built-in assets (M42, D-121); a reference for AI
assistants held to the code by tests (M43, D-124); the site's landing page (M44, D-125); and the testnet points, a
season of tasks on the signet (M45, D-126, D-127).

## Before publication: 2026-10-28

Findings the audits had left open were closed: entry-pool churn (M47, D-130), a pinned settlement pair that
goes out again (M48, D-131), and limits on what one peer or client can make a node hold or do (M49, D-132). A
differential fuzz test holds the TypeScript SDK and the playground's ABI codec to the Rust code (M50, D-133).

At publication the web wallet and block explorer moved to [ksana-web](https://github.com/ksana-in-bitcoin/ksana-web)
and the site to [ksana-site](https://github.com/ksana-in-bitcoin/ksana-site), and mainnet's block share was left out
until it is announced (D-134).
