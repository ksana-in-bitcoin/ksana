# Security policy

Ksana has not launched on mainnet. The networks that run today (devnet and signet) restart from genesis whenever a
consensus change requires it, and nothing on them holds value.

## Reporting a vulnerability

Do not describe a vulnerability in an issue, a discussion or a pull request. Report it privately through GitHub's
private vulnerability reporting (the repository's Security tab, "Report a vulnerability"), or send it to a maintainer
of the `ksana-in-bitcoin` organization through a private channel you already share with them. Include:

- the affected component (crate, SDK, contract, or the web wallet of the ksana-web repository) and the commit you
  looked at;
- what an attacker can do, what it costs them, and who is affected (one node, every node, the indexer, users' funds);
- the steps or a test that reproduces it.

The report and its follow-up stay private until the fix is merged.

## What counts

Anything that lets someone, at a cost below the damage done:

- make honest nodes, or a node and the indexer, disagree about the chain or its state (consensus divergence);
- stop block production, BTC settlement, BTC finality or the indexer (liveness);
- exhaust a node's or the indexer's CPU, memory or disk from the network or from BTC data (denial of service);
- take, lock or create funds outside the rules (rewards, fees, unlocks, the supply cap, the carrier reward, the
  example contracts);
- crash a node, the indexer, the CLI or the SDK with external input.

## Audits

Both audits so far are internal reviews; they do not replace an independent audit before mainnet. Their commit IDs
refer to the development history before the repository was published, which is not public.

| Date | Kind | Code | Report |
|------|------|------|--------|
| 2026-10-06 | Second internal audit | Reviewed at `11f0576` with the chain-base change's patches; findings re-checked on `main` at `5819b26` | [audits/2026-10-06-second-internal-audit.md](audits/2026-10-06-second-internal-audit.md) |
| 2026-10-06 | First internal audit | `main` at `ef07411` | Not published; its fixes are D-098 (M26) and D-100 (M27), and the second report gives the status of each of its findings |
