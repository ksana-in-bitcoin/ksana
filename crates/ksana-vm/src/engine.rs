//! The wasmtime engine and contract calls (SPEC §9.1–9.3, docs/ABI.md).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use ksana_primitives::asset::{STATUS_ALLOWED, STATUS_FROZEN};
use ksana_primitives::hash::sha256;
use ksana_primitives::{check_funds, Funds, Params};
use wasmtime::{
    AsContextMut, Caller, Config, Engine, Extern, Linker, Memory, Module, OperatorCost,
    ResourceLimiter, Store, Trap,
};

use crate::gas;
use crate::host::{
    AssetMetaInput, CallOutcome, CallRequest, ContractCode, FailureReason, Host, HostError, VmError,
};
use crate::instrument::instrument;
use crate::limits::{
    MAX_ASSET_META_BYTES, MAX_EVENT_BYTES, MAX_FUNDS_BYTES, MAX_INSTANCES_PER_OP, MAX_OUTPUT_BYTES,
    MAX_STORAGE_KEY_BYTES, MAX_STORAGE_VALUE_BYTES, MAX_TABLE_ELEMENTS, MAX_TOTAL_MEMORY_PER_OP,
    MAX_WASM_STACK,
};
use crate::validate::{
    validate_code, CodeError, EXPORT_ALLOC, EXPORT_CALL, EXPORT_MEMORY, HOST_MODULE,
};

/// The reserved method name: can only be called at deployment.
pub const INIT_METHOD: &str = "init";
/// Maximum length of the call input (Borsh `(method, args)`).
pub const MAX_CALL_INPUT_BYTES: usize = 65_536;
/// Compilation cache capacity (by code hash; whether cached or not does not affect consensus).
const CACHE_CAPACITY: usize = 256;

type HResult<T> = wasmtime::Result<T>;

/// An internal error marker meaning "this op has failed or must be aborted"; the real reason is recorded in [`Ctx`].
#[derive(Debug)]
struct Abort;

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ksana: op aborted")
    }
}

impl std::error::Error for Abort {}

#[derive(Default)]
struct ModuleCache {
    modules: BTreeMap<[u8; 32], Module>,
    order: VecDeque<[u8; 32]>,
}

struct Shared {
    engine: Engine,
    max_memory: u64,
    max_call_depth: usize,
    btc_query_depth: u32,
    chain_id: u32,
    cache: Mutex<ModuleCache>,
}

impl Shared {
    fn module(&self, code_hash: &[u8; 32], code: &[u8]) -> Result<Module, String> {
        {
            let cache = self
                .cache
                .lock()
                .map_err(|_| "compilation cache lock poisoned".to_string())?;
            if let Some(module) = cache.modules.get(code_hash) {
                return Ok(module.clone());
            }
        }
        // code that has passed the deployment check: re-analyze the frames and insert runtime stack metering
        // (D-073) before compiling
        let frames = validate_code(code, self.max_memory).map_err(|e| e.to_string())?;
        let instrumented = instrument(code, &frames).map_err(|e| e.to_string())?;
        let module = Module::new(&self.engine, &instrumented).map_err(|e| e.to_string())?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| "compilation cache lock poisoned".to_string())?;
        if !cache.modules.contains_key(code_hash) {
            if cache.order.len() >= CACHE_CAPACITY {
                if let Some(old) = cache.order.pop_front() {
                    cache.modules.remove(&old);
                }
            }
            cache.order.push_back(*code_hash);
            cache.modules.insert(*code_hash, module.clone());
        }
        Ok(module)
    }
}

/// The contract VM (SPEC §9).
pub struct Vm {
    shared: Arc<Shared>,
}

struct Limiter {
    max_memory: u64,
    total_memory: u64,
    limit_hit: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> HResult<bool> {
        let desired = u64::try_from(desired).unwrap_or(u64::MAX);
        let current = u64::try_from(current).unwrap_or(u64::MAX);
        let total = self
            .total_memory
            .saturating_add(desired.saturating_sub(current));
        if desired > self.max_memory || total > MAX_TOTAL_MEMORY_PER_OP {
            self.limit_hit = true;
            return Ok(false);
        }
        self.total_memory = total;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> HResult<bool> {
        if desired > MAX_TABLE_ELEMENTS {
            self.limit_hit = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES_PER_OP
    }

    fn tables(&self) -> usize {
        MAX_INSTANCES_PER_OP
    }

    fn memories(&self) -> usize {
        MAX_INSTANCES_PER_OP
    }
}

struct Frame {
    contract: [u8; 32],
    caller: [u8; 32],
    /// The funds attached to this call level (D-073).
    funds: Vec<Funds>,
}

struct Ctx<H: Host> {
    host: H,
    limiter: Limiter,
    frames: Vec<Frame>,
    instances: usize,
    linker: Option<Arc<Linker<Ctx<H>>>>,
    shared: Arc<Shared>,
    /// A host internal error: processing of the whole block must be aborted.
    fatal: Option<String>,
    /// The deterministic failure reason (the first one).
    reason: Option<FailureReason>,
}

/// Records the failure reason and aborts execution.
fn fail<H: Host, T>(
    store: &mut impl AsContextMut<Data = Ctx<H>>,
    reason: FailureReason,
) -> HResult<T> {
    let mut ctx = store.as_context_mut();
    let data = ctx.data_mut();
    if data.reason.is_none() {
        data.reason = Some(reason);
    }
    Err(wasmtime::Error::new(Abort))
}

fn rejected<H: Host, T>(store: &mut impl AsContextMut<Data = Ctx<H>>, msg: &str) -> HResult<T> {
    fail(store, FailureReason::Rejected(msg.to_string()))
}

fn fatal<H: Host, T>(store: &mut impl AsContextMut<Data = Ctx<H>>, msg: String) -> HResult<T> {
    let mut ctx = store.as_context_mut();
    let data = ctx.data_mut();
    if data.fatal.is_none() {
        data.fatal = Some(msg);
    }
    Err(wasmtime::Error::new(Abort))
}

/// Calls `Host`, mapping `HostError` to a failure or a fatal error.
fn with_host<H: Host, T>(
    store: &mut impl AsContextMut<Data = Ctx<H>>,
    f: impl FnOnce(&mut H) -> Result<T, HostError>,
) -> HResult<T> {
    let result = f(&mut store.as_context_mut().data_mut().host);
    match result {
        Ok(v) => Ok(v),
        Err(HostError::Failed(msg)) => fail(store, FailureReason::Rejected(msg)),
        Err(HostError::Fatal(msg)) => fatal(store, msg),
    }
}

/// The guard units by which the fuel injected into wasmtime exceeds the gas limit (D-020).
///
/// wasmtime checks fuel only at function entry, loop headers and after variable-length bulk operations; the
/// straight-line code after the last check and the function return are not checked; consumption beyond the injected
/// amount is saturated to 0 in `get_fuel`, indistinguishable from "exactly used up". With 1 extra unit injected:
/// remaining ≥ 1 if and only if consumption ≤ the limit; remaining is 0 if and only if consumption exceeds the limit
/// (including the excess masked by saturation).
const FUEL_GUARD: u64 = 1;

/// Charges gas; this op fails when it is insufficient (or when the limit was already exceeded earlier).
fn charge<H: Host>(store: &mut impl AsContextMut<Data = Ctx<H>>, amount: u64) -> HResult<()> {
    let fuel = store.as_context_mut().get_fuel()?;
    match fuel
        .checked_sub(FUEL_GUARD)
        .and_then(|available| available.checked_sub(amount))
    {
        Some(rest) => store
            .as_context_mut()
            .set_fuel(rest.saturating_add(FUEL_GUARD)),
        None => {
            store.as_context_mut().set_fuel(0)?;
            fail(store, FailureReason::OutOfGas)
        }
    }
}

/// Interprets a wasm i32 as an unsigned offset or length.
fn unsigned(v: i32) -> usize {
    usize::try_from(u32::from_ne_bytes(v.to_ne_bytes())).unwrap_or(usize::MAX)
}

fn pack(ptr: usize, len: usize) -> Option<i64> {
    let ptr = u64::from(u32::try_from(ptr).ok()?);
    let len = u64::from(u32::try_from(len).ok()?);
    Some(i64::from_ne_bytes(((ptr << 32) | len).to_ne_bytes()))
}

fn unpack(v: i64) -> (usize, usize) {
    let v = u64::from_ne_bytes(v.to_ne_bytes());
    let ptr = usize::try_from(v >> 32).unwrap_or(usize::MAX);
    let len = usize::try_from(v & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    (ptr, len)
}

fn caller_memory<H: Host>(c: &mut Caller<'_, Ctx<H>>) -> HResult<Memory> {
    match c.get_export(EXPORT_MEMORY) {
        Some(Extern::Memory(m)) => Ok(m),
        _ => rejected(c, "missing exported memory"),
    }
}

/// Reads `len` bytes from the caller's memory; fails when `len` exceeds `max` (the length should already have been
/// charged before the call).
fn read_bytes<H: Host>(c: &mut Caller<'_, Ctx<H>>, ptr: i32, len: usize) -> HResult<Vec<u8>> {
    let mem = caller_memory(c)?;
    let start = unsigned(ptr);
    let bytes = start
        .checked_add(len)
        .and_then(|end| mem.data(&*c).get(start..end))
        .map(<[u8]>::to_vec);
    match bytes {
        Some(b) => Ok(b),
        None => fail(c, FailureReason::Trap("memory out of bounds".into())),
    }
}

fn read_array<H: Host, const N: usize>(c: &mut Caller<'_, Ctx<H>>, ptr: i32) -> HResult<[u8; N]> {
    let bytes = read_bytes(c, ptr, N)?;
    let mut out = [0u8; N];
    for (d, s) in out.iter_mut().zip(bytes) {
        *d = s;
    }
    Ok(out)
}

fn write_bytes<H: Host>(c: &mut Caller<'_, Ctx<H>>, ptr: i32, data: &[u8]) -> HResult<()> {
    let mem = caller_memory(c)?;
    let start = unsigned(ptr);
    let ok = match start.checked_add(data.len()) {
        Some(end) => match mem.data_mut(&mut *c).get_mut(start..end) {
            Some(dst) => {
                dst.copy_from_slice(data);
                true
            }
            None => false,
        },
        None => false,
    };
    if ok {
        Ok(())
    } else {
        fail(c, FailureReason::Trap("memory out of bounds".into()))
    }
}

/// Calls the contract's own `ksana_alloc` to allocate a buffer and writes `data` into it, returning the packed
/// `(ptr << 32) | len`.
fn alloc_and_write<H: Host>(c: &mut Caller<'_, Ctx<H>>, data: &[u8]) -> HResult<i64> {
    let alloc = match c.get_export(EXPORT_ALLOC) {
        Some(Extern::Func(f)) => f.typed::<i32, i32>(&*c)?,
        _ => return rejected(c, "missing ksana_alloc"),
    };
    let len = match i32::try_from(data.len()) {
        Ok(l) => l,
        Err(_) => return rejected(c, "returned data too long"),
    };
    let ptr = alloc.call(&mut *c, len)?;
    write_bytes(c, ptr, data)?;
    match pack(unsigned(ptr), data.len()) {
        Some(p) => Ok(p),
        None => rejected(c, "returned data too long"),
    }
}

fn current_frame<H: Host>(c: &mut Caller<'_, Ctx<H>>) -> HResult<([u8; 32], [u8; 32])> {
    match c.data().frames.last() {
        Some(f) => Ok((f.contract, f.caller)),
        None => fatal(c, "call stack is empty".into()),
    }
}

/// Reads a `sub_id` and derives the ID of the current contract's asset, `sha256(contract ‖ sub_id)` (D-015):
/// `asset_issue`, `asset_create` and the asset management functions name an asset this way, so only its issuing
/// contract can issue or manage it (D-121). Returns the contract and the asset ID.
fn own_asset<H: Host>(c: &mut Caller<'_, Ctx<H>>, subp: i32) -> HResult<([u8; 32], [u8; 32])> {
    let sub_id = read_array::<H, 32>(c, subp)?;
    let (issuer, _) = current_frame(c)?;
    let mut preimage = [0u8; 64];
    for (d, s) in preimage.iter_mut().zip(issuer.iter().chain(sub_id.iter())) {
        *d = *s;
    }
    Ok((issuer, sha256(&preimage)))
}

/// Reads an on / off argument (0 or 1); any other value fails the op.
fn switch<H: Host>(c: &mut Caller<'_, Ctx<H>>, v: i32) -> HResult<bool> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => rejected(c, "a switch argument must be 0 or 1"),
    }
}

/// Reads a capability set (D-121): a value outside `u8` fails the op (the host refuses undefined bits).
fn capabilities<H: Host>(c: &mut Caller<'_, Ctx<H>>, v: i32) -> HResult<u8> {
    match u8::try_from(v) {
        Ok(caps) => Ok(caps),
        Err(_) => rejected(c, "capabilities out of range"),
    }
}

/// Checks the length limit and charges by length (before reading).
fn check_len_and_charge<H: Host>(
    c: &mut Caller<'_, Ctx<H>>,
    len: i32,
    max: usize,
    base: u64,
    per_byte: u64,
) -> HResult<usize> {
    let len = unsigned(len);
    if len > max {
        return rejected(c, "length exceeds the limit");
    }
    charge(c, gas::cost(base, per_byte, len))?;
    Ok(len)
}

/// Reads the funds attached to a contract-to-contract call (Borsh `Vec<Funds>`; length 0 means none attached): first
/// checks the length limit and charges by item count, requiring the input to be fully consumed and to satisfy
/// [`check_funds`].
fn read_funds<H: Host>(c: &mut Caller<'_, Ctx<H>>, ptr: i32, len: i32) -> HResult<Vec<Funds>> {
    let len = unsigned(len);
    if len == 0 {
        return Ok(Vec::new());
    }
    if len > MAX_FUNDS_BYTES {
        return rejected(c, "attached funds encoding too long");
    }
    let bytes = read_bytes(c, ptr, len)?;
    let Ok(funds) = borsh::from_slice::<Vec<Funds>>(&bytes) else {
        return rejected(c, "attached funds are not a valid Vec<Funds> encoding");
    };
    if let Err(e) = check_funds(&funds) {
        return rejected(c, e);
    }
    let n = u64::try_from(funds.len()).unwrap_or(u64::MAX);
    charge(c, gas::FUNDS_PER_ASSET.saturating_mul(n))?;
    Ok(funds)
}

/// Transfers the attached funds from `from` to `to` (the op fails on insufficient balance).
fn transfer_funds<H: Host>(
    c: &mut Caller<'_, Ctx<H>>,
    from: &[u8; 32],
    to: &[u8; 32],
    funds: &[Funds],
) -> HResult<()> {
    for f in funds {
        with_host(c, |h| h.transfer_attached(&f.asset, from, to, f.amount))?;
    }
    Ok(())
}

/// The common prelude of contract-to-contract calls (`call_contract` / `try_call_contract` / `delegate_call`): checks
/// the input length and charges by byte, reads the callee contract ID and the input; the input must be a valid
/// `(method, args)` encoding, and the method name must not be `init`. These are all caller errors: the whole op fails.
fn read_call<H: Host>(
    c: &mut Caller<'_, Ctx<H>>,
    cp: i32,
    ip: i32,
    il: i32,
) -> HResult<([u8; 32], Vec<u8>)> {
    let len = check_len_and_charge(
        c,
        il,
        MAX_CALL_INPUT_BYTES,
        gas::CALL_CONTRACT_BASE,
        gas::CALL_CONTRACT_PER_INPUT_BYTE,
    )?;
    let callee = read_array::<H, 32>(c, cp)?;
    let input = read_bytes(c, ip, len)?;
    Ok((callee, input))
}

/// Checks the call input: a valid `(method, args)` encoding whose method name is not `init`.
fn check_call_input<H: Host>(c: &mut Caller<'_, Ctx<H>>, input: &[u8]) -> HResult<()> {
    match decode_input(input) {
        Some((method, _)) if method == INIT_METHOD => {
            rejected(c, "init can only be called at deployment")
        }
        Some(_) => Ok(()),
        None => rejected(c, "call input is not a valid (method, args) encoding"),
    }
}

/// The result status of `try_call_contract` (D-073): success.
const TRY_OK: u8 = 0;
/// The callee reverted explicitly; the data is the revert reason.
const TRY_REVERT: u8 = 1;
/// Other failures (trap, out of gas, rule violation, contract does not exist, depth exceeded, attached funds transfer
/// failed); the data is empty.
const TRY_FAILED: u8 = 2;

/// Writes the result of `try_call_contract` (Borsh `(status: u8, data: Vec<u8>)`) into the caller's memory and
/// returns it packed.
fn try_result<H: Host>(c: &mut Caller<'_, Ctx<H>>, status: u8, data: &[u8]) -> HResult<i64> {
    let encoded = match borsh::to_vec(&(status, data)) {
        Ok(e) => e,
        Err(e) => return fatal(c, format!("encoding the call result: {e}")),
    };
    alloc_and_write(c, &encoded)
}

/// Whether a callee failure can be caught by `try_call_contract`: a host internal error cannot (processing of the
/// whole block is aborted); a failure with a determinable reason (revert, trap, out of gas, rule violation, resource
/// limit exceeded) can, returning the reason and clearing the record.
fn catchable<H: Host>(c: &mut Caller<'_, Ctx<H>>, err: &wasmtime::Error) -> Option<FailureReason> {
    let data = c.data_mut();
    if data.fatal.is_some() {
        return None;
    }
    let limit_hit = std::mem::replace(&mut data.limiter.limit_hit, false);
    if let Some(reason) = data.reason.take() {
        return Some(reason);
    }
    if let Some(trap) = err.downcast_ref::<Trap>() {
        return Some(if *trap == Trap::OutOfFuel {
            FailureReason::OutOfGas
        } else {
            FailureReason::Trap(trap.to_string())
        });
    }
    limit_hit.then(|| FailureReason::Rejected("contract resources exceed the limit".into()))
}

/// Instantiates the contract and calls the entry point; returns the data in `Ok` on success, aborts the whole op on
/// failure (including an `Err` revert).
fn invoke<H: Host>(
    store: &mut impl AsContextMut<Data = Ctx<H>>,
    contract: [u8; 32],
    caller: [u8; 32],
    funds: Vec<Funds>,
    code: &ContractCode,
    input: &[u8],
) -> HResult<Vec<u8>> {
    let (module, linker) = {
        let mut ctx = store.as_context_mut();
        let data = ctx.data_mut();
        if data.instances >= MAX_INSTANCES_PER_OP {
            return rejected(store, "number of contract instances exceeds the limit");
        }
        data.instances = data.instances.saturating_add(1);
        let module = data.shared.module(&code.code_hash, &code.code);
        let linker = data.linker.clone();
        (module, linker)
    };
    let module = match module {
        Ok(m) => m,
        Err(e) => {
            return fatal(
                store,
                format!("compiling the deployed contract failed: {e}"),
            )
        }
    };
    let linker = match linker {
        Some(l) => l,
        None => return fatal(store, "linker not initialized".into()),
    };
    store.as_context_mut().data_mut().frames.push(Frame {
        contract,
        caller,
        funds,
    });

    let instance = match linker.instantiate(&mut *store, &module) {
        Ok(i) => i,
        Err(e) => {
            if e.downcast_ref::<Trap>().is_some() || e.downcast_ref::<Abort>().is_some() {
                return Err(e);
            }
            if store.as_context_mut().data().limiter.limit_hit {
                return rejected(store, "contract resources exceed the limit");
            }
            return fatal(store, format!("instantiation failed: {e}"));
        }
    };
    let (alloc, entry, memory) = match (
        instance.get_typed_func::<i32, i32>(&mut *store, EXPORT_ALLOC),
        instance.get_typed_func::<(i32, i32), i64>(&mut *store, EXPORT_CALL),
        instance.get_memory(&mut *store, EXPORT_MEMORY),
    ) {
        (Ok(a), Ok(e), Some(m)) => (a, e, m),
        _ => return rejected(store, "missing required exports"),
    };
    let len = match i32::try_from(input.len()) {
        Ok(l) => l,
        Err(_) => return rejected(store, "input too long"),
    };
    let ptr = unsigned(alloc.call(&mut *store, len)?);
    let written = ptr
        .checked_add(input.len())
        .and_then(|end| memory.data_mut(&mut *store).get_mut(ptr..end))
        .map(|dst| dst.copy_from_slice(input))
        .is_some();
    if !written {
        return fail(store, FailureReason::Trap("memory out of bounds".into()));
    }
    let ptr_i32 = i32::from_ne_bytes(u32::try_from(ptr).unwrap_or(u32::MAX).to_ne_bytes());
    let packed = entry.call(&mut *store, (ptr_i32, len))?;
    let (out_ptr, out_len) = unpack(packed);
    if out_len > MAX_OUTPUT_BYTES {
        return rejected(store, "return value too long");
    }
    let output = out_ptr
        .checked_add(out_len)
        .and_then(|end| memory.data(&*store).get(out_ptr..end))
        .map(<[u8]>::to_vec);
    let output = match output {
        Some(o) => o,
        None => {
            return fail(
                store,
                FailureReason::Trap("return value out of bounds".into()),
            )
        }
    };
    let result: Result<Vec<u8>, Vec<u8>> = match borsh::from_slice(&output) {
        Ok(r) => r,
        Err(_) => {
            return rejected(
                store,
                "return value is not a valid Result<Vec<u8>, Vec<u8>> encoding",
            )
        }
    };
    store.as_context_mut().data_mut().frames.pop();
    match result {
        Ok(data) => Ok(data),
        Err(reason) => fail(store, FailureReason::Revert(reason)),
    }
}

fn decode_input(input: &[u8]) -> Option<(String, Vec<u8>)> {
    borsh::from_slice::<(String, Vec<u8>)>(input).ok()
}

fn build_linker<H: Host>(engine: &Engine) -> HResult<Linker<Ctx<H>>> {
    let mut l: Linker<Ctx<H>> = Linker::new(engine);
    l.func_wrap(
        HOST_MODULE,
        "storage_get",
        |mut c: Caller<'_, Ctx<H>>, kp: i32, kl: i32| -> HResult<i64> {
            let len = check_len_and_charge(
                &mut c,
                kl,
                MAX_STORAGE_KEY_BYTES,
                gas::STORAGE_GET_BASE,
                gas::STORAGE_GET_PER_BYTE,
            )?;
            let key = read_bytes(&mut c, kp, len)?;
            let (contract, _) = current_frame(&mut c)?;
            match with_host(&mut c, |h| h.storage_get(&contract, &key))? {
                None => Ok(-1),
                Some(value) => {
                    charge(&mut c, gas::cost(0, gas::STORAGE_GET_PER_BYTE, value.len()))?;
                    alloc_and_write(&mut c, &value)
                }
            }
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "storage_set",
        |mut c: Caller<'_, Ctx<H>>, kp: i32, kl: i32, vp: i32, vl: i32| -> HResult<()> {
            let (k, v) = (unsigned(kl), unsigned(vl));
            if k > MAX_STORAGE_KEY_BYTES || v > MAX_STORAGE_VALUE_BYTES {
                return rejected(&mut c, "storage key or value exceeds the length limit");
            }
            charge(
                &mut c,
                gas::cost(
                    gas::STORAGE_SET_BASE,
                    gas::STORAGE_SET_PER_BYTE,
                    k.saturating_add(v),
                ),
            )?;
            let key = read_bytes(&mut c, kp, k)?;
            let value = read_bytes(&mut c, vp, v)?;
            let (contract, _) = current_frame(&mut c)?;
            with_host(&mut c, |h| h.storage_set(&contract, &key, &value))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "storage_remove",
        |mut c: Caller<'_, Ctx<H>>, kp: i32, kl: i32| -> HResult<()> {
            let len = check_len_and_charge(
                &mut c,
                kl,
                MAX_STORAGE_KEY_BYTES,
                gas::STORAGE_REMOVE_BASE,
                gas::STORAGE_REMOVE_PER_BYTE,
            )?;
            let key = read_bytes(&mut c, kp, len)?;
            let (contract, _) = current_frame(&mut c)?;
            with_host(&mut c, |h| h.storage_remove(&contract, &key))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "caller",
        |mut c: Caller<'_, Ctx<H>>, dst: i32| -> HResult<()> {
            charge(&mut c, gas::ENV_QUERY)?;
            let (_, caller) = current_frame(&mut c)?;
            write_bytes(&mut c, dst, &caller)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "self_id",
        |mut c: Caller<'_, Ctx<H>>, dst: i32| -> HResult<()> {
            charge(&mut c, gas::ENV_QUERY)?;
            let (contract, _) = current_frame(&mut c)?;
            write_bytes(&mut c, dst, &contract)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "block_height",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            charge(&mut c, gas::ENV_QUERY)?;
            let height = c.data().host.block_height();
            match i64::try_from(height) {
                Ok(h) => Ok(h),
                Err(_) => fatal(&mut c, "block height exceeds i64".into()),
            }
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "block_timestamp",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            charge(&mut c, gas::ENV_QUERY)?;
            let timestamp = c.data().host.block_timestamp();
            match i64::try_from(timestamp) {
                Ok(t) => Ok(t),
                Err(_) => fatal(&mut c, "block timestamp exceeds i64".into()),
            }
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "chain_id",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            charge(&mut c, gas::ENV_QUERY)?;
            Ok(i64::from(c.data().shared.chain_id))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "gas_left",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            // charge this function's cost first, returning the gas left after the charge (wasmtime has saved the fuel
            // before the host function call, so the reading is exact)
            charge(&mut c, gas::ENV_QUERY)?;
            let left = c.get_fuel()?.saturating_sub(FUEL_GUARD);
            Ok(i64::try_from(left).unwrap_or(i64::MAX))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "btc_anchor_height",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            charge(&mut c, gas::ENV_QUERY)?;
            Ok(i64::from(c.data().host.btc_anchor_height()))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "btc_header",
        |mut c: Caller<'_, Ctx<H>>, height: i64, dst: i32| -> HResult<()> {
            charge(&mut c, gas::BTC_HEADER)?;
            let anchor = c.data().host.btc_anchor_height();
            let depth = c.data().shared.btc_query_depth;
            let allowed = anchor
                .checked_sub(depth)
                .and_then(|max| u32::try_from(height).ok().filter(|h| *h <= max));
            let Some(h) = allowed else {
                return rejected(&mut c, "insufficient BTC block header depth");
            };
            match with_host(&mut c, |host| host.btc_header(h))? {
                Some(header) => write_bytes(&mut c, dst, &header),
                None => rejected(&mut c, "BTC block header does not exist"),
            }
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_balance",
        |mut c: Caller<'_, Ctx<H>>, ap: i32, accp: i32, dst: i32| -> HResult<()> {
            charge(&mut c, gas::ASSET_BALANCE)?;
            let asset = read_array::<H, 32>(&mut c, ap)?;
            let account = read_array::<H, 32>(&mut c, accp)?;
            let balance = with_host(&mut c, |h| h.asset_balance(&asset, &account))?;
            write_bytes(&mut c, dst, &balance.to_le_bytes())
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_transfer",
        |mut c: Caller<'_, Ctx<H>>, ap: i32, top: i32, amp: i32| -> HResult<()> {
            charge(&mut c, gas::ASSET_TRANSFER)?;
            let asset = read_array::<H, 32>(&mut c, ap)?;
            let to = read_array::<H, 32>(&mut c, top)?;
            let amount = u128::from_le_bytes(read_array::<H, 16>(&mut c, amp)?);
            let (from, _) = current_frame(&mut c)?;
            with_host(&mut c, |h| h.asset_transfer(&asset, &from, &to, amount))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_burn",
        |mut c: Caller<'_, Ctx<H>>, ap: i32, amp: i32| -> HResult<()> {
            charge(&mut c, gas::ASSET_BURN)?;
            let asset = read_array::<H, 32>(&mut c, ap)?;
            let amount = u128::from_le_bytes(read_array::<H, 16>(&mut c, amp)?);
            let (from, _) = current_frame(&mut c)?;
            with_host(&mut c, |h| h.asset_burn(&asset, &from, amount))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "create_contract",
        |mut c: Caller<'_, Ctx<H>>,
         hp: i32,
         sp: i32,
         ap: i32,
         al: i32,
         fp: i32,
         fl: i32,
         dst: i32|
         -> HResult<()> {
            let len = check_len_and_charge(
                &mut c,
                al,
                MAX_CALL_INPUT_BYTES,
                gas::CREATE_CONTRACT_BASE,
                gas::CALL_CONTRACT_PER_INPUT_BYTE,
            )?;
            let code_hash = read_array::<H, 32>(&mut c, hp)?;
            let salt = read_array::<H, 32>(&mut c, sp)?;
            let args = read_bytes(&mut c, ap, len)?;
            let funds = read_funds(&mut c, fp, fl)?;
            let input = borsh::to_vec(&(INIT_METHOD, args))?;
            if input.len() > MAX_CALL_INPUT_BYTES {
                return rejected(&mut c, "call input too long");
            }
            if c.data().frames.len() >= c.data().shared.max_call_depth {
                return rejected(&mut c, "contract call depth exceeds the limit");
            }
            let Some(code) = with_host(&mut c, |h| h.code(&code_hash))? else {
                return rejected(&mut c, "code does not exist");
            };
            charge(&mut c, gas::cost(0, gas::CALL_PER_CODE_BYTE, code.len()))?;
            // address = sha256(creator ‖ salt ‖ code_hash) (D-073)
            let (creator, _) = current_frame(&mut c)?;
            let mut preimage = [0u8; 96];
            for (d, s) in preimage
                .iter_mut()
                .zip(creator.iter().chain(salt.iter()).chain(code_hash.iter()))
            {
                *d = *s;
            }
            let id = sha256(&preimage);
            if with_host(&mut c, |h| h.contract_code(&id))?.is_some() {
                return rejected(&mut c, "contract already exists");
            }
            with_host(&mut c, |h| h.create_contract(&id, &code_hash, &creator))?;
            transfer_funds(&mut c, &creator, &id, &funds)?;
            let code = ContractCode { code_hash, code };
            invoke(&mut c, id, creator, funds, &code, &input)?;
            write_bytes(&mut c, dst, &id)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_issue",
        |mut c: Caller<'_, Ctx<H>>,
         subp: i32,
         mp: i32,
         ml: i32,
         top: i32,
         amp: i32,
         dst: i32|
         -> HResult<()> {
            let len = check_len_and_charge(
                &mut c,
                ml,
                MAX_ASSET_META_BYTES,
                gas::ASSET_ISSUE_BASE,
                gas::ASSET_ISSUE_PER_BYTE,
            )?;
            let (issuer, asset) = own_asset(&mut c, subp)?;
            let meta_bytes = read_bytes(&mut c, mp, len)?;
            let to = read_array::<H, 32>(&mut c, top)?;
            let amount = u128::from_le_bytes(read_array::<H, 16>(&mut c, amp)?);
            let Ok(meta) = borsh::from_slice::<AssetMetaInput>(&meta_bytes) else {
                return rejected(&mut c, "meta of asset_issue is not a valid encoding");
            };
            with_host(&mut c, |h| {
                h.asset_issue(&asset, &issuer, &meta, &to, amount)
            })?;
            write_bytes(&mut c, dst, &asset)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_create",
        |mut c: Caller<'_, Ctx<H>>,
         subp: i32,
         mp: i32,
         ml: i32,
         caps: i32,
         top: i32,
         amp: i32,
         dst: i32|
         -> HResult<()> {
            let len = check_len_and_charge(
                &mut c,
                ml,
                MAX_ASSET_META_BYTES,
                gas::ASSET_CREATE_BASE,
                gas::ASSET_CREATE_PER_BYTE,
            )?;
            let (issuer, asset) = own_asset(&mut c, subp)?;
            let meta_bytes = read_bytes(&mut c, mp, len)?;
            let to = read_array::<H, 32>(&mut c, top)?;
            let amount = u128::from_le_bytes(read_array::<H, 16>(&mut c, amp)?);
            let caps = capabilities(&mut c, caps)?;
            let Ok(meta) = borsh::from_slice::<AssetMetaInput>(&meta_bytes) else {
                return rejected(&mut c, "meta of asset_create is not a valid encoding");
            };
            with_host(&mut c, |h| {
                h.asset_create(&asset, &issuer, &meta, caps, &to, amount)
            })?;
            write_bytes(&mut c, dst, &asset)
        },
    )?;
    for (name, flag) in [
        ("asset_freeze", STATUS_FROZEN),
        ("asset_allow", STATUS_ALLOWED),
    ] {
        l.func_wrap(
            HOST_MODULE,
            name,
            move |mut c: Caller<'_, Ctx<H>>, subp: i32, accp: i32, on: i32| -> HResult<()> {
                charge(&mut c, gas::ASSET_SET_STATUS)?;
                let (issuer, asset) = own_asset(&mut c, subp)?;
                let account = read_array::<H, 32>(&mut c, accp)?;
                let on = switch(&mut c, on)?;
                with_host(&mut c, |h| {
                    h.asset_set_status(&asset, &issuer, &account, flag, on)
                })
            },
        )?;
    }
    l.func_wrap(
        HOST_MODULE,
        "asset_pause",
        |mut c: Caller<'_, Ctx<H>>, subp: i32, paused: i32| -> HResult<()> {
            charge(&mut c, gas::ASSET_PAUSE)?;
            let (issuer, asset) = own_asset(&mut c, subp)?;
            let paused = switch(&mut c, paused)?;
            with_host(&mut c, |h| h.asset_pause(&asset, &issuer, paused))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_renounce",
        |mut c: Caller<'_, Ctx<H>>, subp: i32, caps: i32| -> HResult<()> {
            charge(&mut c, gas::ASSET_RENOUNCE)?;
            let (issuer, asset) = own_asset(&mut c, subp)?;
            let caps = capabilities(&mut c, caps)?;
            with_host(&mut c, |h| h.asset_renounce(&asset, &issuer, caps))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_policy",
        |mut c: Caller<'_, Ctx<H>>, ap: i32| -> HResult<i64> {
            charge(&mut c, gas::ASSET_POLICY)?;
            let asset = read_array::<H, 32>(&mut c, ap)?;
            // -1 when the asset does not exist, otherwise the capabilities in bits 0–7 and the pause in bit 8
            Ok(match with_host(&mut c, |h| h.asset_policy(&asset))? {
                None => -1,
                Some(p) if p.paused => i64::from(p.capabilities) | 0x100,
                Some(p) => i64::from(p.capabilities),
            })
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "asset_status",
        |mut c: Caller<'_, Ctx<H>>, ap: i32, accp: i32| -> HResult<i32> {
            charge(&mut c, gas::ASSET_STATUS)?;
            let asset = read_array::<H, 32>(&mut c, ap)?;
            let account = read_array::<H, 32>(&mut c, accp)?;
            let status = with_host(&mut c, |h| h.asset_status(&asset, &account))?;
            Ok(i32::from(status))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "call_funds",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            let funds = match c.data().frames.last() {
                Some(f) => f.funds.clone(),
                None => return fatal(&mut c, "call stack is empty".into()),
            };
            let encoded = match borsh::to_vec(&funds) {
                Ok(e) => e,
                Err(e) => return fatal(&mut c, format!("encoding the attached funds: {e}")),
            };
            charge(
                &mut c,
                gas::cost(
                    gas::CALL_FUNDS_BASE,
                    gas::CALL_FUNDS_PER_BYTE,
                    encoded.len(),
                ),
            )?;
            alloc_and_write(&mut c, &encoded)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "native_tx",
        |mut c: Caller<'_, Ctx<H>>| -> HResult<i64> {
            charge(&mut c, gas::NATIVE_TX_BASE)?;
            let Some(tx) = with_host(&mut c, |h| h.native_tx())? else {
                return Ok(-1);
            };
            let encoded = match borsh::to_vec(&tx) {
                Ok(e) => e,
                Err(e) => return fatal(&mut c, format!("encoding the native transaction: {e}")),
            };
            charge(&mut c, gas::cost(0, gas::NATIVE_TX_PER_BYTE, encoded.len()))?;
            alloc_and_write(&mut c, &encoded)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "call_contract",
        |mut c: Caller<'_, Ctx<H>>, cp: i32, ip: i32, il: i32, fp: i32, fl: i32| -> HResult<i64> {
            let (callee, input) = read_call(&mut c, cp, ip, il)?;
            let funds = read_funds(&mut c, fp, fl)?;
            check_call_input(&mut c, &input)?;
            if c.data().frames.len() >= c.data().shared.max_call_depth {
                return rejected(&mut c, "contract call depth exceeds the limit");
            }
            let Some(code) = with_host(&mut c, |h| h.contract_code(&callee))? else {
                return rejected(&mut c, "callee contract does not exist");
            };
            charge(
                &mut c,
                gas::cost(0, gas::CALL_PER_CODE_BYTE, code.code.len()),
            )?;
            let (me, _) = current_frame(&mut c)?;
            transfer_funds(&mut c, &me, &callee, &funds)?;
            let output = invoke(&mut c, callee, me, funds, &code, &input)?;
            alloc_and_write(&mut c, &output)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "try_call_contract",
        |mut c: Caller<'_, Ctx<H>>,
         cp: i32,
         ip: i32,
         il: i32,
         fp: i32,
         fl: i32,
         gas_limit: i64|
         -> HResult<i64> {
            // caller errors (the input, the encoding and rules of the attached funds, gas insufficient for the
            // charges below) cannot be caught: the whole op fails.
            let (callee, input) = read_call(&mut c, cp, ip, il)?;
            let funds = read_funds(&mut c, fp, fl)?;
            check_call_input(&mut c, &input)?;
            let (me, _) = current_frame(&mut c)?;
            if c.data().frames.len() >= c.data().shared.max_call_depth {
                return try_result(&mut c, TRY_FAILED, &[]);
            }
            let Some(code) = with_host(&mut c, |h| h.contract_code(&callee))? else {
                return try_result(&mut c, TRY_FAILED, &[]);
            };
            charge(
                &mut c,
                gas::cost(0, gas::CALL_PER_CODE_BYTE, code.code.len()),
            )?;
            // from here on, a callee failure only rolls back this level (D-073)
            with_host(&mut c, |h| h.checkpoint())?;
            if let Err(e) = transfer_funds(&mut c, &me, &callee, &funds) {
                if catchable(&mut c, &e).is_none() {
                    return Err(e);
                }
                with_host(&mut c, |h| h.revert_checkpoint())?;
                return try_result(&mut c, TRY_FAILED, &[]);
            }
            // the sub-call's gas limit: the smaller of the requested value (0 means no separate limit) and 63/64 of
            // the remaining gas; the caller keeps at least 1/64 to handle a failure.
            // The callee starts with exactly "limit + guard unit" of fuel, the same as a top-level call (D-020), so
            // the limit is exact.
            let available = c.get_fuel()?.saturating_sub(FUEL_GUARD);
            let cap = available.saturating_sub(available / 64);
            let requested = u64::from_ne_bytes(gas_limit.to_ne_bytes());
            let limit = if requested == 0 {
                cap
            } else {
                requested.min(cap)
            };
            let held = available.saturating_sub(limit);
            let depth = c.data().frames.len();
            c.set_fuel(limit.saturating_add(FUEL_GUARD))?;
            let result = invoke(&mut c, callee, me, funds, &code, &input);
            // remaining ≥ guard unit if and only if the callee's consumption did not exceed the limit
            let left = c.get_fuel()?;
            let outcome = match result {
                Ok(output) if left >= FUEL_GUARD => Ok(output),
                Ok(_) => Err(FailureReason::OutOfGas),
                Err(e) => match catchable(&mut c, &e) {
                    Some(reason) => Err(reason),
                    None => return Err(e),
                },
            };
            c.data_mut().frames.truncate(depth);
            match outcome {
                Ok(output) => {
                    with_host(&mut c, |h| h.release_checkpoint())?;
                    c.set_fuel(held.saturating_add(left))?;
                    try_result(&mut c, TRY_OK, &output)
                }
                // a revert is a normal return of the callee: the unused gas is refunded to the caller; other failures
                // use up the sub-call's whole limit
                Err(FailureReason::Revert(data)) if left >= FUEL_GUARD => {
                    with_host(&mut c, |h| h.revert_checkpoint())?;
                    c.set_fuel(held.saturating_add(left))?;
                    try_result(&mut c, TRY_REVERT, &data)
                }
                Err(_) => {
                    with_host(&mut c, |h| h.revert_checkpoint())?;
                    c.set_fuel(held.saturating_add(FUEL_GUARD))?;
                    try_result(&mut c, TRY_FAILED, &[])
                }
            }
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "delegate_call",
        |mut c: Caller<'_, Ctx<H>>, cp: i32, ip: i32, il: i32| -> HResult<i64> {
            let (target, input) = read_call(&mut c, cp, ip, il)?;
            check_call_input(&mut c, &input)?;
            if c.data().frames.len() >= c.data().shared.max_call_depth {
                return rejected(&mut c, "contract call depth exceeds the limit");
            }
            let Some(code) = with_host(&mut c, |h| h.contract_code(&target))? else {
                return rejected(&mut c, "callee contract does not exist");
            };
            charge(
                &mut c,
                gas::cost(0, gas::CALL_PER_CODE_BYTE, code.code.len()),
            )?;
            // executes the target contract's code in the current call's context: storage, balances, self_id, caller
            // and the attached funds are all unchanged (D-073)
            let (me, caller, funds) = match c.data().frames.last() {
                Some(f) => (f.contract, f.caller, f.funds.clone()),
                None => return fatal(&mut c, "call stack is empty".into()),
            };
            let output = invoke(&mut c, me, caller, funds, &code, &input)?;
            alloc_and_write(&mut c, &output)
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "emit_event",
        |mut c: Caller<'_, Ctx<H>>, p: i32, len: i32| -> HResult<()> {
            let len = check_len_and_charge(
                &mut c,
                len,
                MAX_EVENT_BYTES,
                gas::EMIT_EVENT_BASE,
                gas::EMIT_EVENT_PER_BYTE,
            )?;
            let data = read_bytes(&mut c, p, len)?;
            let (contract, _) = current_frame(&mut c)?;
            with_host(&mut c, |h| h.emit_event(&contract, &data))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "sha256",
        |mut c: Caller<'_, Ctx<H>>, p: i32, len: i32, dst: i32| -> HResult<()> {
            let max = usize::try_from(c.data().shared.max_memory).unwrap_or(usize::MAX);
            let len =
                check_len_and_charge(&mut c, len, max, gas::SHA256_BASE, gas::SHA256_PER_BYTE)?;
            let data = read_bytes(&mut c, p, len)?;
            write_bytes(&mut c, dst, &sha256(&data))
        },
    )?;
    l.func_wrap(
        HOST_MODULE,
        "verify_schnorr",
        |mut c: Caller<'_, Ctx<H>>, pkp: i32, mp: i32, sp: i32| -> HResult<i32> {
            charge(&mut c, gas::VERIFY_SCHNORR)?;
            let pubkey = read_array::<H, 32>(&mut c, pkp)?;
            let msg = read_array::<H, 32>(&mut c, mp)?;
            let sig = read_array::<H, 64>(&mut c, sp)?;
            Ok(i32::from(
                ksana_crypto::schnorr_verify(&pubkey, &msg, &sig).is_ok(),
            ))
        },
    )?;
    Ok(l)
}

impl Vm {
    /// Configures wasmtime per SPEC §9.1: fuel metering, NaN canonicalization, threads, multi-memory, SIMD and other
    /// features disabled, no WASI.
    pub fn new(params: &Params) -> Result<Self, VmError> {
        Self::with_parallel_compilation(params, true)
    }

    /// Same as [`Vm::new`], with parallel compilation optionally disabled (for determinism tests: the result must be
    /// the same under different thread counts).
    pub fn with_parallel_compilation(params: &Params, parallel: bool) -> Result<Self, VmError> {
        let mut config = Config::new();
        // wasmtime's `threads` and `gc` compile features are not enabled: threads, GC, function-references and
        // exceptions are always off; reference-types (the new encoding of funcref tables) stays on. The remaining
        // proposals are disabled explicitly here.
        config
            .consume_fuel(true)
            .operator_cost(OperatorCost::new())
            .cranelift_nan_canonicalization(true)
            .wasm_multi_memory(false)
            .wasm_relaxed_simd(false)
            .wasm_simd(false)
            .wasm_tail_call(false)
            .wasm_gc(false)
            .wasm_memory64(false)
            .wasm_extended_const(false)
            .wasm_custom_page_sizes(false)
            .wasm_wide_arithmetic(false)
            .wasm_multi_value(true)
            .wasm_bulk_memory(true)
            // wasmtime requires max_wasm_stack not to exceed async_stack_size; this VM does not use async.
            .async_stack_size(MAX_WASM_STACK)
            .max_wasm_stack(MAX_WASM_STACK)
            .parallel_compilation(parallel);
        let engine = Engine::new(&config).map_err(|e| VmError::Internal(e.to_string()))?;
        let max_call_depth =
            usize::try_from(params.max_call_depth).map_err(|e| VmError::Internal(e.to_string()))?;
        Ok(Vm {
            shared: Arc::new(Shared {
                engine,
                max_memory: params.max_wasm_memory_bytes,
                max_call_depth,
                btc_query_depth: params.btc_query_depth,
                chain_id: params.chain_id,
                cache: Mutex::new(ModuleCache::default()),
            }),
        })
    }

    /// The deployment-time check: the static rules (docs/ABI.md sections 1 and 5) + compilable by wasmtime. Returns
    /// the `code_hash`.
    pub fn validate_code(&self, code: &[u8]) -> Result<[u8; 32], CodeError> {
        validate_code(code, self.shared.max_memory)?;
        let code_hash = sha256(code);
        self.shared
            .module(&code_hash, code)
            .map_err(CodeError::Invalid)?;
        Ok(code_hash)
    }

    /// Executes one top-level contract call; `gas_limit` is the available fuel. `host` is handed back as is after the
    /// call.
    pub fn call<H: Host>(
        &self,
        host: H,
        req: CallRequest,
        gas_limit: u64,
    ) -> (H, Result<CallOutcome, VmError>) {
        let ctx = Ctx {
            host,
            limiter: Limiter {
                max_memory: self.shared.max_memory,
                total_memory: 0,
                limit_hit: false,
            },
            frames: Vec::new(),
            instances: 0,
            linker: None,
            shared: Arc::clone(&self.shared),
            fatal: None,
            reason: None,
        };
        let mut store = Store::new(&self.shared.engine, ctx);
        store.limiter(|c| &mut c.limiter);
        let result = self.run(&mut store, req, gas_limit);
        let remaining = store.get_fuel().unwrap_or(0);
        let ctx = store.into_data();
        let outcome = match result {
            // remaining fuel without the guard unit means consumption has exceeded the limit: the excess after the
            // last check also counts as out of gas
            Ok(output) => Ok(match remaining.checked_sub(FUEL_GUARD) {
                Some(left) => CallOutcome::Success {
                    output,
                    gas_used: gas_limit.saturating_sub(left),
                },
                None => CallOutcome::Failure(FailureReason::OutOfGas),
            }),
            Err(err) => {
                if let Some(msg) = ctx.fatal {
                    Err(VmError::Host(msg))
                } else if let Some(reason) = ctx.reason {
                    Ok(CallOutcome::Failure(reason))
                } else if let Some(trap) = err.downcast_ref::<Trap>() {
                    if *trap == Trap::OutOfFuel {
                        Ok(CallOutcome::Failure(FailureReason::OutOfGas))
                    } else {
                        Ok(CallOutcome::Failure(FailureReason::Trap(trap.to_string())))
                    }
                } else if ctx.limiter.limit_hit {
                    Ok(CallOutcome::Failure(FailureReason::Rejected(
                        "contract resources exceed the limit".into(),
                    )))
                } else {
                    Err(VmError::Internal(err.to_string()))
                }
            }
        };
        (ctx.host, outcome)
    }

    fn run<H: Host>(
        &self,
        store: &mut Store<Ctx<H>>,
        req: CallRequest,
        gas_limit: u64,
    ) -> HResult<Vec<u8>> {
        store.set_fuel(gas_limit.saturating_add(FUEL_GUARD))?;
        let linker = build_linker::<H>(&self.shared.engine)?;
        store.data_mut().linker = Some(Arc::new(linker));
        if req.method == INIT_METHOD && !req.allow_init {
            return rejected(store, "init can only be called at deployment");
        }
        let input = borsh::to_vec(&(req.method, req.args))?;
        if input.len() > MAX_CALL_INPUT_BYTES {
            return rejected(store, "call input too long");
        }
        let code = match store.data_mut().host.contract_code(&req.contract) {
            Ok(Some(code)) => code,
            Ok(None) => return rejected(store, "contract does not exist"),
            Err(HostError::Failed(msg)) => return fail(store, FailureReason::Rejected(msg)),
            Err(HostError::Fatal(msg)) => return fatal(store, msg),
        };
        charge(
            store,
            gas::cost(0, gas::CALL_PER_CODE_BYTE, code.code.len()),
        )?;
        invoke(store, req.contract, req.caller, req.funds, &code, &input)
    }
}
