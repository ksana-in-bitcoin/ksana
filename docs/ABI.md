# Ksana Contract ABI

This document defines the language-independent interface between contracts and the VM (the M1 deliverable required by SPEC §9.2). All numbers are consensus parameters, defined in `ksana-vm`;
where this document and the code disagree, the code is authoritative and this document is corrected to match. Design trade-offs are in `docs/DECISIONS.md` D-012 to D-019 (for the state encoding see also D-033, D-038).
The token standards KS-20 and KS-721, conventions between contracts on top of this interface, are in `docs/STANDARDS.md` (D-120).

## 1. Module requirements (static checks at deployment)

At deployment the VM fully validates the bytecode with `wasmparser`; if any of the following is not satisfied, deployment is rejected (failed op):

- Only WebAssembly 1.0 core instructions are allowed, plus the following deterministic extensions: mutable-globals, sign-extension, bulk-memory
  (`memory.copy` / `memory.fill` / `memory.init` / `data.drop`), multi-value, and the funcref-table instructions of reference-types.
- **No floating point**: any `f32` / `f64` value type (function signatures, locals, globals, block types) and any floating-point instruction
  (including `f32.const`, floating-point load/store, conversions, `nontrapping-fptoint`).
- Forbidden: SIMD (`v128`), threads / atomics / shared memory, multi-memory, memory64, exceptions, tail calls,
  GC / function-references, extended-const, custom page sizes, wide arithmetic and all other proposals.
- Exactly one linear memory, defined by the module itself (not imported), with an initial size of at most `MAX_WASM_MEMORY / 64 KiB = 256` pages;
  the declared maximum page count is unrestricted; when memory growth at runtime would exceed `MAX_WASM_MEMORY`, `memory.grow` returns −1.
- At most one table, with element type `funcref`, defined by the module itself; it must declare a maximum element count, which must not exceed `MAX_TABLE_ELEMENTS = 4096`.
- Imports may only be the functions of the `ksana` module listed in section 3, with exactly matching signatures; importing memories, tables or globals is not allowed.
- Required exports: `memory` (the linear memory), `ksana_alloc: (i32) -> i32`, `ksana_call: (i32, i32) -> i64`.
- Recursion and indirect calls are not restricted: stack depth is metered deterministically at runtime (section 5, D-073).

## 2. Calling convention

Every call creates a new instance; linear memory is not preserved across calls. Persistent data can only be written to contract storage.

1. The VM calls `ksana_alloc(len)` to obtain a buffer pointer and writes the input into that buffer.
2. The VM calls `ksana_call(ptr, len)`. The input is the Borsh encoding of `(method: String, args: Vec<u8>)`.
3. The return value `i64`, interpreted as unsigned, is `(out_ptr << 32) | out_len`, pointing at the Borsh encoding of `Result<Vec<u8>, Vec<u8>>`
   (Borsh's variant tag for `Result` is **0 = `Err`, 1 = `Ok`**, followed by the u32 little-endian length and the data):
   - `Ok(data)`: the call succeeded and `data` is the result;
   - `Err(reason)`: an explicit revert (SPEC §8.1).
4. All of the following are call failures: a trap (including `unreachable`, out-of-bounds access, division by zero), running out of gas, output that is not a valid
   `Result<Vec<u8>, Vec<u8>>` encoding, output exceeding `MAX_OUTPUT_BYTES = 64 KiB`, and `Err`.
5. **A failure at any call level fails the whole op**: all state changes and events of the op are rolled back; only the fee deduction and the nonce increment remain (SPEC §9.2).
   The exception is `try_call_contract` (D-073): when the callee fails, only that level is rolled back and the caller continues after receiving the result (section 3).
6. Deployment: right after the code and the contract metadata are stored, the contract is called with `method = "init"`, `args = init_args`. `init` can only be called at deployment;
   afterwards any `Call` / `call_contract` using the method name `init` fails.
7. Attached funds (D-073): the `funds` of `Deploy` / `Call` and the `funds` of `call_contract` are a `Vec<Funds { asset: [u8; 32], amount: u128 }>`,
   in strictly ascending order of asset ID (no duplicates), each amount greater than 0, at most `MAX_CALL_FUNDS = 16` items. Before entering the callee, the caller (at the top level the signing account,
   for contract-to-contract calls the calling contract one level up) transfers these assets to the callee's account; at deployment they are transferred to the new contract before `init` runs. If the rules are not met or the balance is insufficient, the op fails;
   when the op fails, the transfers are rolled back together with the other state changes. The callee reads the funds attached to the current call with `call_funds`.

From inside host functions the VM calls back into `ksana_alloc` to allocate memory for the returned data; this execution is also charged gas.

## 3. Host functions (import module `ksana`)

Pointers are offsets (`i32`) into the caller's linear memory; out of bounds traps. Functions marked "writes" write a fixed-length result to `dst`.
"Packed return" means `i64 = (ptr << 32) | len`, where the buffer is allocated by the VM calling this contract's `ksana_alloc`; `-1` means "does not exist".
In the gas column, `/B` means charged per byte.

| Function | Signature | gas | Semantics |
| --- | --- | --- | --- |
| `storage_get` | `(key_ptr, key_len) -> i64` | 2,000 + 1/B (key+value) | Reads this contract's storage; packed return of the value, `-1` if it does not exist |
| `storage_set` | `(key_ptr, key_len, val_ptr, val_len)` | 10,000 + 10/B (key+value) | Writes to this contract's storage |
| `storage_remove` | `(key_ptr, key_len)` | 5,000 + 1/B (key) | Removes the key from this contract's storage |
| `caller` | `(dst)` | 100 | Writes the 32-byte caller: at the top level the VmOp's signing account, for contract-to-contract calls the ID of the calling contract one level up |
| `self_id` | `(dst)` | 100 | Writes the 32-byte ID of the current contract |
| `block_height` | `() -> i64` | 100 | Fast-lane block height |
| `btc_anchor_height` | `() -> i64` | 100 | The BTC height the current block anchors to |
| `block_timestamp` | `() -> i64` | 100 | Timestamp of the main-chain block executing this op (seconds; ops of a merged block see the timestamp of the main-chain block that merges it, D-073) |
| `chain_id` | `() -> i64` | 100 | Chain ID (devnet 1001, signet 1002) |
| `gas_left` | `() -> i64` | 100 | Gas remaining after this function's own cost is deducted |
| `btc_header` | `(height: i64, dst)` | 2,000 | Writes the 80-byte BTC block header; traps when `height > anchor − BTC_QUERY_DEPTH`, when negative, or when nonexistent |
| `asset_balance` | `(asset_ptr, account_ptr, dst)` | 2,000 | Writes the 16-byte balance (u128 little-endian) |
| `asset_transfer` | `(asset_ptr, to_ptr, amount_ptr)` | 10,000 | Transfers `amount` (16-byte u128 little-endian) out of **this contract's account**; traps if the balance is insufficient, the asset does not exist or the asset's rules refuse the movement (D-121, below) |
| `asset_issue` | `(sub_id_ptr, meta_ptr, meta_len, to_ptr, amount_ptr, dst)` | 20,000 + 1/B (meta) | Issues more of this contract's asset and credits it to `to`; writes the 32-byte asset ID (see section 6). The first issuance creates the asset with the capability `mint` alone; afterwards the asset must still have `mint` (D-121) |
| `asset_create` | `(sub_id_ptr, meta_ptr, meta_len, capabilities: i32, to_ptr, amount_ptr, dst)` | 20,000 + 1/B (meta) | Creates this contract's asset with the issuer capabilities `capabilities` (a bit set, D-121) and credits `amount` of it to `to`; writes the 32-byte asset ID. Traps if the asset exists, `capabilities` has a bit outside `0x0F`, or the asset's rules refuse `to` |
| `asset_freeze` | `(sub_id_ptr, account_ptr, frozen: i32)` | 10,000 | Freezes (`1`) or unfreezes (`0`) `account` for this contract's asset (D-121); freezing needs the capability `freeze`, unfreezing never does |
| `asset_allow` | `(sub_id_ptr, account_ptr, allowed: i32)` | 10,000 | Puts `account` on (`1`) or takes it off (`0`) the allowlist of this contract's asset (D-121); putting it on needs the capability `allowlist` |
| `asset_pause` | `(sub_id_ptr, paused: i32)` | 10,000 | Pauses (`1`) or resumes (`0`) this contract's asset (D-121); pausing needs the capability `pause` |
| `asset_renounce` | `(sub_id_ptr, capabilities: i32)` | 10,000 | Gives up `capabilities` of this contract's asset for good (D-121); capabilities it no longer has are ignored, and giving up `pause` resumes the asset |
| `asset_policy` | `(asset_ptr) -> i64` | 2,000 | The asset's capabilities in bits 0–7 and `0x100` when it is paused; `-1` if the asset does not exist (D-121) |
| `asset_status` | `(asset_ptr, account_ptr) -> i32` | 2,000 | The account's status flags for the asset as recorded: `1` frozen, `2` allowed; 0 when it has none (D-121) |
| `asset_burn` | `(asset_ptr, amount_ptr)` | 10,000 | Burns `amount` held by **this contract's account**; the asset's total supply decreases accordingly (KSA too, D-073); traps if the balance is insufficient, the asset does not exist or the asset's rules refuse this contract as a sender (D-121) |
| `create_contract` | `(code_hash_ptr, salt_ptr, args_ptr, args_len, funds_ptr, funds_len, dst)` | 100,000 + 1/B (args) + 1/B (code) + 10,000/item (attached funds) | Creates a contract from code already on chain (stored by some earlier `Deploy`) (D-073): ID per section 6; runs `init(args)` with this contract as the caller, transferring the attached funds out of this contract's account beforehand; writes the 32-byte contract ID. Traps if the code does not exist, the address is already taken, the depth limit is exceeded or `init` fails |
| `call_contract` | `(contract_ptr, input_ptr, input_len, funds_ptr, funds_len) -> i64` | 20,000 + 1/B (input) + 1/B (callee code) + 10,000/item (attached funds) | Calls another contract; `input` as in section 2; `funds` is a Borsh-encoded `Vec<Funds>` (`funds_len = 0` means nothing attached), transferred from **this contract's account** to the callee (section 2, item 7); on success, packed return of the data in `Ok` |
| `call_funds` | `() -> i64` | 100 + 1/B (returned data) | Packed return of the funds attached to the current call (a Borsh-encoded `Vec<Funds>`; an empty list when nothing is attached) |
| `native_tx` | `() -> i64` | 2,000 + 1/B (returned data) | When this op is a path C native op, packed return of the BTC transaction carrying it (D-078): a Borsh-encoded `NativeTx { txid: [u8; 32], outputs: Vec<NativeOutput { value: u64, script: Vec<u8> }> }` (the transaction ID in internal byte order, all outputs in transaction order, including the KSN1 data output); path B ops and read-only calls return `-1` |
| `try_call_contract` | `(contract_ptr, input_ptr, input_len, funds_ptr, funds_len, gas: i64) -> i64` | Same as `call_contract` | Catchable call (D-073): packed return of Borsh `(status: u8, data: Vec<u8>)`, with `status` 0 = success (`data` is the data in `Ok`), 1 = the callee reverted (`data` is the reason), 2 = other failure (`data` is empty); see below |
| `delegate_call` | `(contract_ptr, input_ptr, input_len) -> i64` | Same as `call_contract` (no attached funds) | Executes the code of contract `contract` in **the context of the current call** (storage, balances, `self_id`, `caller`, `call_funds`) (D-073); on failure the whole op fails; on success, packed return of the data in `Ok` |
| `emit_event` | `(ptr, len)` | 1,000 + 5/B | Records an event (not part of the state root; discarded when the op fails) |
| `sha256` | `(ptr, len, dst)` | 500 + 1/B | Writes the 32-byte SHA-256 |
| `verify_schnorr` | `(pubkey_ptr, msg_ptr, sig_ptr) -> i32` | 50,000 | BIP340: 32-byte x-only public key, 32-byte message digest, 64-byte signature; returns 1 if valid, otherwise 0 |

Length limits (exceeding one is a call failure): storage key `MAX_STORAGE_KEY_BYTES = 256`, storage value `MAX_STORAGE_VALUE_BYTES = 65,536`,
event `MAX_EVENT_BYTES = 16,384`, the meta of `asset_issue` and `asset_create` `MAX_ASSET_META_BYTES = 1,024`, call input `MAX_CALL_INPUT_BYTES = 65,536`,
the `funds` encoding of `call_contract` `4 + 48 × MAX_CALL_FUNDS = 772` bytes (the length is checked before decoding, and the input must be fully consumed).

Resource limits within a single op (every call creates a new instance): number of instances `MAX_INSTANCES_PER_OP = 64`, linear memory of all instances combined
`MAX_TOTAL_MEMORY_PER_OP = 256 MiB`. Exceeding either fails the op.

The `meta` of `asset_issue` and `asset_create` is a Borsh-encoded `AssetMetaInput { name: String, symbol: String, decimals: u8 }`: a name of 1 to 64 bytes,
a symbol of 1 to 16 bytes (UTF-8, counted in bytes) and at most 18 decimals, otherwise trap (D-121).
The metadata is written when the asset is first issued; the `meta` passed on every later issuance must exactly match the recorded name, symbol and decimals, otherwise trap.
Traps if the total supply overflows u128.

Issuer capabilities (D-121). An asset's issuing contract holds the capabilities it declared when it created the asset, a bit set: `0x01` mint, `0x02` freeze,
`0x04` allowlist, `0x08` pause (`0x10` and above are undefined). `asset_create` declares them; `asset_issue` creates an asset with `mint` alone. They can only be given up
(`asset_renounce`), never gained, and KSA has none. The asset management functions `asset_create`, `asset_freeze`, `asset_allow`, `asset_pause` and `asset_renounce`
name the asset by this contract's `sub_id`, so only its issuing contract manages an asset; a switch argument other than 0 or 1, or a capability set outside `u8`,
traps. Every movement of the asset (the `AssetTransfer` action, the funds attached to a `Deploy`, a `Call` or a contract-to-contract call, `asset_transfer`,
`asset_issue`, `asset_create` and `asset_burn`) obeys its rules, whatever the amount; the issuing contract is exempt from all of them:

- With `freeze`, a frozen account neither sends, receives nor burns the asset.
- With `allowlist`, only the allowed accounts send, receive or burn it; an asset created with an allowlist can only go to its issuing contract until it allows an account.
- While it is paused, no account but the issuing contract sends or burns it; receiving from the issuing contract goes on.
- A flag set while the asset had the capability is kept but no longer counts once the capability is given up; setting a flag, or pausing, needs the capability, clearing one,
  or resuming, never does. The issuing contract has no status for its own asset.
- There is no clawback: the issuing contract never moves an account's balance; an account's assets move only by the account's own signature (SPEC §9.2).

A refusal by the asset's rules fails the op like an insufficient balance does (`try_call_contract` catches a refusal of its attached funds as `status = 2`); its message
starts with `rejected:`, e.g. `rejected: sender is frozen for this asset`, `rejected: recipient is not on this asset's allowlist`, `rejected: asset is paused` or
`rejected: asset supply is fixed`.

Call depth of `call_contract`: the top-level call is level 1; exceeding `MAX_CALL_DEPTH` (8 on devnet) traps.
Reentrancy (A → B → A) is allowed, and storage changes take effect immediately as seen by every level. `try_call_contract` and `delegate_call` likewise occupy a level.

`native_tx` (D-078): all call levels within one op (including `try_call_contract`, `delegate_call` and the `init` of `create_contract`) read the same transaction;
the transaction comes from a block on the anchor chain at a height not above the anchor height (anchor-invalid historical blocks follow the current best chain, D-056), and the result depends only on the anchor chain. The same transaction may be read several times within one
op (for example, called twice via another contract); contracts must themselves prevent the same payment from being credited twice (the example exchange contract marks the lock as claimed, D-085).

`try_call_contract` (D-073):

- The caller's own errors cannot be caught and fail the whole op: input out of bounds, too long, or not a valid `(method, args)` encoding; method name `init`; an invalid `funds` encoding or rule violation;
  insufficient gas to pay this function's cost (including loading the callee code).
- Callee does not exist, or depth limit exceeded: `status = 2`, no call is made. Otherwise a checkpoint is set, the attached funds are transferred from this contract to the callee (insufficient balance: roll back, `status = 2`),
  then the callee runs with the sub-call's gas limit. When the callee's level (including all calls it makes) fails, all storage writes, transfers and events after the checkpoint are rolled back,
  and the caller continues; an internal host error still aborts processing of the whole block.
- The sub-call's gas limit = `min(gas, remaining gas − ⌊remaining gas / 64⌋)` (`gas` interpreted as unsigned, 0 means no separate limit); the caller keeps at least 1/64 to handle a failure.
  The limit is exact (same as at the top level, D-020). On success or revert, unused gas is refunded to the caller; other failures (trap, out of gas, rule violation) consume the whole limit.

## 4. Gas

- 1 gas = 1 wasmtime fuel (SPEC §8.3); host functions additionally charge gas per the table in section 3. The VM explicitly configures wasmtime's
  `OperatorCost::new()`: ordinary instructions 1, `nop` / `drop` / structured control instructions 0; `memory.copy` / `memory.fill` /
  `memory.init` are additionally charged 1 per byte, `memory.grow` 1 per page, table operations 1 per element. This default table depends on the wasmtime version,
  so **the wasmtime version itself is a consensus parameter**, and upgrading it is a consensus change.
- The gas limit of each op is the VmOp's `max_gas`. Fixed costs of the built-in actions:

| Action | gas |
| --- | --- |
| `AssetTransfer` | 10,000 |
| `Deploy` | 100,000 + 20/B (code) + 10,000/item (attached funds) + the execution of `init` (including loading the code at 1/B) |
| `Call` | 20,000 + 10,000/item (attached funds) + 1/B (code) + contract execution |

- When an op fails, `gas_used` in the receipt is recorded as `max_gas` (receipts are not part of the state root).
- The gas limit is exact: a call that consumes more than `max_gas` always fails, including when the excess occurs after wasmtime's last fuel check
  (the VM injects `max_gas + 1` fuel and checks on return, D-020).

## 5. Deterministic stack limit (runtime metering, D-073)

The point at which a wasm stack overflow triggers depends on native stack frame sizes, which differ across architectures and would break determinism. Stack depth is therefore metered by an architecture-independent estimate,
exceeding the limit traps deterministically, and the native stack never overflows first:

- Each function's stack frame is estimated as `64 + 16 × (number of params + number of locals + maximum operand stack depth)` bytes (derived by the validator during the deployment check).
- Before compilation, a mutable i32 global is added to the module (indexed after the existing globals) and every defined function is rewritten: at entry, the function's stack frame estimate is added,
  and `unreachable` is executed if it exceeds `STACK_BUDGET = 262,144` bytes (256 KiB); the original body is wrapped in a block with the same result type (`return` becomes a `br` that exits
  this block), and after the block the function's stack frame estimate is subtracted. The inserted instructions are charged gas as ordinary instructions.
- The count is per contract instance: every contract-to-contract call creates a new instance, each instance is separately bounded by `STACK_BUDGET`, and the call depth is additionally bounded by `MAX_CALL_DEPTH`.
- What is stored is still the original bytecode, and `code_hash` is unchanged; the instrumentation is a pre-compilation consensus rule (so the `wasm-encoder` version is also a consensus parameter).
- Recursion, function pointers, trait objects and `core::fmt` can all be used (the restrictions of D-021 are lifted).
- At runtime, wasmtime's `max_wasm_stack` is set to 32 MiB and blocks are executed on a dedicated thread with a 64 MiB stack. The estimated usage of 8 levels of contract calls each using its full budget
  totals 2 MiB (plus the host functions' frames at each level), leaving a 16× margin, so the native stack limit is never hit before the metering.

## 6. Identifiers

- `code_hash = sha256(code)`.
- `contract_id = sha256(deployer account 32 bytes ‖ nonce as u64 little-endian 8 bytes ‖ code_hash 32 bytes)` (SPEC §9.2).
- Contracts created by contracts (`create_contract`, D-073): `contract_id = sha256(creator contract ID 32 bytes ‖ salt 32 bytes ‖ code_hash 32 bytes)`.
  The preimage is 96 bytes, a different length from the 72-byte preimage of user deployments, so the two kinds of ID never collide. The deployer in the contract metadata is recorded as the creator contract.
- Assets issued by contracts: `asset_id = sha256(issuing contract ID 32 bytes ‖ sub_id 32 bytes)`. The asset ID of the project token KSA is 32 zero bytes.

## 7. Built-in modules and state encoding

State keys are concatenated with a prefix first, and their sha256 is then used as the JMT key hash (SPEC §10.1). All values are Borsh-encoded.

| Prefix | Key | Value |
| --- | --- | --- |
| `0x01` | account | `AccountState { nonce: u64 }` (D-078 removed `fee_debt`) |
| `0x02` | asset_id ‖ account | `u128` balance; the key is deleted when the balance is 0 |
| `0x03` | asset_id | `AssetMeta { name: String, symbol: String, decimals: u8, total_supply: u128, issuer: [u8; 32], capabilities: u8, paused: bool }` (D-121: `symbol`, the issuer's `capabilities` and `paused`) |
| `0x04` | contract_id | `ContractMeta { code_hash: [u8; 32], deployer: [u8; 32] }` |
| `0x05` | contract_id ‖ key | Raw bytes of contract storage |
| `0x06` | code_hash | Raw bytes of contract code |
| `0x07` | (no longer used) | Formerly the funds records of path A (D-033), removed together with path A (D-078) |
| `0x08` | height (u64 big-endian) ‖ index (u32 big-endian) | `LockedFee { producer: [u8; 32], amount: u128 }`: the not-yet-unlocked path B sponsored fees of the block at acceptance position `(height, index)` (D-079) |
| `0x09` | (none) | `UnlockCursor { height: u64, index: u32 }`: the acceptance position unlocked up to (D-079); absent when nothing has been unlocked yet |
| `0x0A` | (no longer used) | Formerly the elements of the pending-unlock queue (D-074), removed with D-079 |
| `0x0B` | (none) | `[u8; 32]`: the block share account, written from the genesis config's `block_share_account` (D-089); absent means the whole block reward goes to the producer |
| `0x0C` | asset_id ‖ account | `u8`: the account's status flags for the asset, `0x01` frozen, `0x02` allowed (D-121); the key is deleted when no flag is left |

- An account can be a user (the output key of a Taproot address, D-092) or a contract ID; both share the 0x02 balance table.
- **Acceptance position** (D-079): a main-chain block accepts the new blocks in its past per the DAG -- first the blocks it merges (the merge set except the selected parent, in consensus order), and last
  itself. A block's acceptance position is `(height, index)`: `height` is the height of the main-chain block that accepts it, `index` is its ordinal in that main-chain block's acceptance order (the merged
  blocks are 0, 1, ... in turn, and the main-chain block itself comes last). The acceptance order is the lexicographic order of positions.
- Path B sponsored fees are recorded on receipt under `0x08 ‖ height ‖ index` of the block that charged them (entries of a merged block are recorded at its own position and belong to its producer, D-063);
  a block with no sponsored fees writes no record. Block rewards are not recorded when the block is executed: when neither the main-chain block nor the blocks it merges have any entries, no state is written and the state root equals the selected parent's.
- Unlock (D-038, D-074, D-079): at most one `Unlock { height, index }` per block. The node and the indexer provide the unlockable segment (`BlockEnv::unlockable`:
  the positions on the selected parent's main chain that come after the parent state's unlock cursor in acceptance order, at most `MAX_UNLOCK_BATCH` of them, each with block hash, producer, DAA score and whether it
  earns a reward); the target position must be in the segment, after the cursor, and the block at that position must already be in the settled closure. Then positions after the cursor are unlocked in turn up to and including the target: reward-earning blocks
  (the main-chain block itself and blue merged blocks) are paid the block reward per their DAA score and weight (KSA's `AssetMeta.total_supply` increases accordingly, capped at `KSA_MAX_SUPPLY`),
  then the sponsored-fee record at that position is released, and both are added to its producer's KSA balance (`0x02`); when the state has a block share account (`0x0B`), each reward slot splits off, per that slot's rules,
  `BLOCK_SHARE_BPS / 10 000` (for slots whose DAA score is below `BLOCK_SHARE_END`) added to that account's balance, with the rest added to the producer; when capped, the share's portion is guaranteed first (D-089);
  the cursor is written as the target. Each position is unlocked only once.
- KSA's `AssetMeta` is written in the genesis state: the name and symbol `KSA`, 8 decimals, the issuer 32 zero bytes, no capabilities and not paused (D-121).
  KSA's fees, unlocks and rewards change balances directly; its transfers pass the same rules as any asset's, which never refuse them since it has no capabilities.
