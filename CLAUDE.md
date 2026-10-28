# Ksana working conventions

- `docs/SPEC.md` is the single source of requirements (v2 final: the consensus was frozen on 2026-10-05). The rationale,
  the trade-offs and the gaps the spec left are in `docs/DECISIONS.md`; what each crate and directory does is in
  `docs/ARCHITECTURE.md`; the checks, the rules for consensus code and the conventions for commits are in
  `CONTRIBUTING.md`, which applies in full; `docs/HISTORY.md` puts the decisions and milestones before publication in
  order.
- In a new session or after a context compaction, read the top of SPEC (its version history), the latest entries of
  DECISIONS, CONTRIBUTING.md and `git log --oneline -20` before continuing.
- If the spec is contradictory, has a gap or cannot be implemented: record it in DECISIONS and use the most
  conservative implementation first. A consensus change after the freeze goes through the upgrade mechanism of SPEC
  §6.9, or before mainnet a restart of devnet and signet from genesis, and gets its own decision entry; the next number
  is the one after the last entry of DECISIONS.
- Work in milestones: a milestone's acceptance criteria go in its pull request's description, and each verifiable
  feature point is one commit whose message starts with the milestone's number (`M51: …`).
- Everything committed is in English, including the owner's words quoted in the documents, which appear only in
  translation; the one exception is the contract playground's Chinese pack (`playground/src/i18n/zh-CN/`).
- A change to the CLI, the RPC, the SDK or a parameter updates the documents that describe it, and the matching page of
  ksana-site (github.com/ksana-in-bitcoin/ksana-site). The web wallet and block explorer are ksana-web
  (github.com/ksana-in-bitcoin/ksana-web); both pin this repository as a git submodule.
- Mainnet's block share (its fraction, its end and its account) is not published (D-134): never write its values into
  the repository, its tests or its documents.
- The hosted public signet is operated with `signet/deploy.sh` from the operator's machine (`signet/README.md`); its
  sources come from this repository and from ksana-web and ksana-points checked out beside it (`signet/sources.sh`).
