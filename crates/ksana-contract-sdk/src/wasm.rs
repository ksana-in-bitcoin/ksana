//! wasm32 backend: host function imports of the `ksana` module, the global bump allocator,
//! `ksana_alloc` / `ksana_call`, the panic handler.
//!
//! Every call creates a fresh instance (ABI §2), so the allocator only moves forward and never frees; linear memory is
//! grown on demand with `memory.grow`, and allocation fails and traps when the module's declared maximum is exceeded.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::arch::wasm32;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::codec::CallResult;

/// Host function imports (ABI §3). Pointers and `usize` both lower to `i32` on wasm32.
mod sys {
    #[link(wasm_import_module = "ksana")]
    extern "C" {
        pub fn storage_get(key_ptr: *const u8, key_len: usize) -> i64;
        pub fn storage_set(key_ptr: *const u8, key_len: usize, val_ptr: *const u8, val_len: usize);
        pub fn storage_remove(key_ptr: *const u8, key_len: usize);
        pub fn caller(dst: *mut u8);
        pub fn self_id(dst: *mut u8);
        pub fn block_height() -> i64;
        pub fn block_timestamp() -> i64;
        pub fn chain_id() -> i64;
        pub fn gas_left() -> i64;
        pub fn btc_anchor_height() -> i64;
        pub fn btc_header(height: i64, dst: *mut u8);
        pub fn asset_balance(asset_ptr: *const u8, account_ptr: *const u8, dst: *mut u8);
        pub fn asset_transfer(asset_ptr: *const u8, to_ptr: *const u8, amount_ptr: *const u8);
        pub fn asset_issue(
            sub_id_ptr: *const u8,
            meta_ptr: *const u8,
            meta_len: usize,
            to_ptr: *const u8,
            amount_ptr: *const u8,
            dst: *mut u8,
        );
        pub fn asset_burn(asset_ptr: *const u8, amount_ptr: *const u8);
        pub fn asset_create(
            sub_id_ptr: *const u8,
            meta_ptr: *const u8,
            meta_len: usize,
            capabilities: i32,
            to_ptr: *const u8,
            amount_ptr: *const u8,
            dst: *mut u8,
        );
        pub fn asset_freeze(sub_id_ptr: *const u8, account_ptr: *const u8, frozen: i32);
        pub fn asset_allow(sub_id_ptr: *const u8, account_ptr: *const u8, allowed: i32);
        pub fn asset_pause(sub_id_ptr: *const u8, paused: i32);
        pub fn asset_renounce(sub_id_ptr: *const u8, capabilities: i32);
        pub fn asset_policy(asset_ptr: *const u8) -> i64;
        pub fn asset_status(asset_ptr: *const u8, account_ptr: *const u8) -> i32;
        pub fn create_contract(
            code_hash_ptr: *const u8,
            salt_ptr: *const u8,
            args_ptr: *const u8,
            args_len: usize,
            funds_ptr: *const u8,
            funds_len: usize,
            dst: *mut u8,
        );
        pub fn call_contract(
            contract_ptr: *const u8,
            input_ptr: *const u8,
            input_len: usize,
            funds_ptr: *const u8,
            funds_len: usize,
        ) -> i64;
        pub fn call_funds() -> i64;
        pub fn native_tx() -> i64;
        pub fn try_call_contract(
            contract_ptr: *const u8,
            input_ptr: *const u8,
            input_len: usize,
            funds_ptr: *const u8,
            funds_len: usize,
            gas: i64,
        ) -> i64;
        pub fn delegate_call(
            contract_ptr: *const u8,
            input_ptr: *const u8,
            input_len: usize,
        ) -> i64;
        pub fn emit_event(ptr: *const u8, len: usize);
        pub fn sha256(ptr: *const u8, len: usize, dst: *mut u8);
        pub fn verify_schnorr(pubkey_ptr: *const u8, msg_ptr: *const u8, sig_ptr: *const u8)
            -> i32;
    }
}

fn abort() -> ! {
    wasm32::unreachable()
}

/// Host function wrappers with the same names and signatures as `crate::mock::host`.
pub(crate) mod host {
    use super::{sys, take_packed};
    use alloc::vec::Vec;

    pub fn trap(_reason: &'static str) -> ! {
        super::abort()
    }

    pub fn storage_get(key: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: the pointer and length come from a valid slice; the host only reads this memory.
        let packed = unsafe { sys::storage_get(key.as_ptr(), key.len()) };
        if packed == -1 {
            None
        } else {
            // SAFETY: a return value other than -1 is a buffer the host allocated through this contract's
            // `ksana_alloc` and filled completely (ABI §3).
            Some(unsafe { take_packed(packed) })
        }
    }

    pub fn storage_set(key: &[u8], value: &[u8]) {
        // SAFETY: both pointers and lengths come from valid slices; the host only reads.
        unsafe { sys::storage_set(key.as_ptr(), key.len(), value.as_ptr(), value.len()) }
    }

    pub fn storage_remove(key: &[u8]) {
        // SAFETY: as above.
        unsafe { sys::storage_remove(key.as_ptr(), key.len()) }
    }

    pub fn caller() -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the host writes exactly 32 bytes to dst.
        unsafe { sys::caller(out.as_mut_ptr()) };
        out
    }

    pub fn self_id() -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the host writes exactly 32 bytes to dst.
        unsafe { sys::self_id(out.as_mut_ptr()) };
        out
    }

    pub fn block_height() -> i64 {
        // SAFETY: no pointer arguments.
        unsafe { sys::block_height() }
    }

    pub fn block_timestamp() -> i64 {
        // SAFETY: no pointer arguments.
        unsafe { sys::block_timestamp() }
    }

    pub fn chain_id() -> i64 {
        // SAFETY: no pointer arguments.
        unsafe { sys::chain_id() }
    }

    pub fn gas_left() -> i64 {
        // SAFETY: no pointer arguments.
        unsafe { sys::gas_left() }
    }

    pub fn btc_anchor_height() -> i64 {
        // SAFETY: no pointer arguments.
        unsafe { sys::btc_anchor_height() }
    }

    pub fn btc_header(height: i64) -> [u8; 80] {
        let mut out = [0u8; 80];
        // SAFETY: the host writes exactly 80 bytes to dst (or traps).
        unsafe { sys::btc_header(height, out.as_mut_ptr()) };
        out
    }

    pub fn asset_balance(asset: &[u8; 32], account: &[u8; 32]) -> [u8; 16] {
        let mut out = [0u8; 16];
        // SAFETY: the input pointers point to 32-byte arrays; the host writes exactly 16 bytes to dst.
        unsafe { sys::asset_balance(asset.as_ptr(), account.as_ptr(), out.as_mut_ptr()) };
        out
    }

    pub fn asset_transfer(asset: &[u8; 32], to: &[u8; 32], amount: &[u8; 16]) {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::asset_transfer(asset.as_ptr(), to.as_ptr(), amount.as_ptr()) }
    }

    pub fn asset_issue(
        sub_id: &[u8; 32],
        meta: &[u8],
        to: &[u8; 32],
        amount: &[u8; 16],
    ) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the input pointers come from valid slices / arrays; the host writes exactly 32 bytes to dst.
        unsafe {
            sys::asset_issue(
                sub_id.as_ptr(),
                meta.as_ptr(),
                meta.len(),
                to.as_ptr(),
                amount.as_ptr(),
                out.as_mut_ptr(),
            )
        };
        out
    }

    pub fn asset_burn(asset: &[u8; 32], amount: &[u8; 16]) {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::asset_burn(asset.as_ptr(), amount.as_ptr()) }
    }

    pub fn asset_create(
        sub_id: &[u8; 32],
        meta: &[u8],
        capabilities: u8,
        to: &[u8; 32],
        amount: &[u8; 16],
    ) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the input pointers come from valid slices / arrays; the host writes exactly 32 bytes to dst.
        unsafe {
            sys::asset_create(
                sub_id.as_ptr(),
                meta.as_ptr(),
                meta.len(),
                i32::from(capabilities),
                to.as_ptr(),
                amount.as_ptr(),
                out.as_mut_ptr(),
            )
        };
        out
    }

    pub fn asset_freeze(sub_id: &[u8; 32], account: &[u8; 32], frozen: bool) {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::asset_freeze(sub_id.as_ptr(), account.as_ptr(), i32::from(frozen)) }
    }

    pub fn asset_allow(sub_id: &[u8; 32], account: &[u8; 32], allowed: bool) {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::asset_allow(sub_id.as_ptr(), account.as_ptr(), i32::from(allowed)) }
    }

    pub fn asset_pause(sub_id: &[u8; 32], paused: bool) {
        // SAFETY: the pointer points to a fixed-length array; the host only reads.
        unsafe { sys::asset_pause(sub_id.as_ptr(), i32::from(paused)) }
    }

    pub fn asset_renounce(sub_id: &[u8; 32], capabilities: u8) {
        // SAFETY: the pointer points to a fixed-length array; the host only reads.
        unsafe { sys::asset_renounce(sub_id.as_ptr(), i32::from(capabilities)) }
    }

    pub fn asset_policy(asset: &[u8; 32]) -> i64 {
        // SAFETY: the pointer points to a fixed-length array; the host only reads.
        unsafe { sys::asset_policy(asset.as_ptr()) }
    }

    pub fn asset_status(asset: &[u8; 32], account: &[u8; 32]) -> i32 {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::asset_status(asset.as_ptr(), account.as_ptr()) }
    }

    pub fn create_contract(
        code_hash: &[u8; 32],
        salt: &[u8; 32],
        args: &[u8],
        funds: &[u8],
    ) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the input pointers come from valid arrays / slices and the host only reads; the host writes exactly
        // 32 bytes to dst.
        unsafe {
            sys::create_contract(
                code_hash.as_ptr(),
                salt.as_ptr(),
                args.as_ptr(),
                args.len(),
                funds.as_ptr(),
                funds.len(),
                out.as_mut_ptr(),
            )
        };
        out
    }

    pub fn call_funds() -> Vec<u8> {
        // SAFETY: no pointer arguments; the return value is a buffer the host allocated through this contract's
        // `ksana_alloc` and filled completely (ABI §3).
        unsafe { take_packed(sys::call_funds()) }
    }

    pub fn native_tx() -> Option<Vec<u8>> {
        // SAFETY: no pointer arguments.
        let packed = unsafe { sys::native_tx() };
        if packed == -1 {
            None
        } else {
            // SAFETY: a return value other than -1 is a buffer the host allocated through this contract's
            // `ksana_alloc` and filled completely (ABI §3).
            Some(unsafe { take_packed(packed) })
        }
    }

    pub fn call_contract(contract: &[u8; 32], input: &[u8], funds: &[u8]) -> Vec<u8> {
        // SAFETY: the pointers come from valid arrays / slices and the host only reads (a `funds` length of 0 means no
        // attached funds).
        let packed = unsafe {
            sys::call_contract(
                contract.as_ptr(),
                input.as_ptr(),
                input.len(),
                funds.as_ptr(),
                funds.len(),
            )
        };
        if packed == -1 {
            super::abort();
        }
        // SAFETY: on success the return value is a buffer the host allocated through this contract's `ksana_alloc` and
        // filled completely (ABI §3).
        unsafe { take_packed(packed) }
    }

    pub fn try_call_contract(contract: &[u8; 32], input: &[u8], funds: &[u8], gas: u64) -> Vec<u8> {
        let gas = i64::from_ne_bytes(gas.to_ne_bytes());
        // SAFETY: the pointers come from valid arrays / slices and the host only reads; the return value is always a
        // buffer the host allocated through this contract's `ksana_alloc` and filled completely.
        unsafe {
            take_packed(sys::try_call_contract(
                contract.as_ptr(),
                input.as_ptr(),
                input.len(),
                funds.as_ptr(),
                funds.len(),
                gas,
            ))
        }
    }

    pub fn delegate_call(contract: &[u8; 32], input: &[u8]) -> Vec<u8> {
        // SAFETY: the pointers come from valid arrays / slices; the host only reads.
        let packed = unsafe { sys::delegate_call(contract.as_ptr(), input.as_ptr(), input.len()) };
        if packed == -1 {
            super::abort();
        }
        // SAFETY: on success the return value is a buffer the host allocated through this contract's `ksana_alloc` and
        // filled completely (ABI §3).
        unsafe { take_packed(packed) }
    }

    pub fn emit_event(data: &[u8]) {
        // SAFETY: the pointer and length come from a valid slice; the host only reads.
        unsafe { sys::emit_event(data.as_ptr(), data.len()) }
    }

    pub fn sha256(data: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: the input comes from a valid slice; the host writes exactly 32 bytes to dst.
        unsafe { sys::sha256(data.as_ptr(), data.len(), out.as_mut_ptr()) };
        out
    }

    pub fn verify_schnorr(pubkey: &[u8; 32], msg: &[u8; 32], sig: &[u8; 64]) -> i32 {
        // SAFETY: the pointers point to fixed-length arrays; the host only reads.
        unsafe { sys::verify_schnorr(pubkey.as_ptr(), msg.as_ptr(), sig.as_ptr()) }
    }
}

// ---------------------------------------------------------------------------
// Packed pointers (ABI §2, §3): the i64 interpreted as unsigned is `(ptr << 32) | len`.

fn pack(ptr: usize, len: usize) -> i64 {
    let (Ok(ptr), Ok(len)) = (u32::try_from(ptr), u32::try_from(len)) else {
        abort()
    };
    let packed = u64::from(ptr).checked_shl(32).unwrap_or_else(|| abort()) | u64::from(len);
    i64::from_ne_bytes(packed.to_ne_bytes())
}

fn unpack(packed: i64) -> (usize, usize) {
    let [l0, l1, l2, l3, p0, p1, p2, p3] = packed.to_le_bytes();
    let ptr = u32::from_le_bytes([p0, p1, p2, p3]);
    let len = u32::from_le_bytes([l0, l1, l2, l3]);
    match (usize::try_from(ptr), usize::try_from(len)) {
        (Ok(ptr), Ok(len)) => (ptr, len),
        _ => abort(),
    }
}

/// Takes ownership of a packed buffer returned by the host as a `Vec<u8>`.
///
/// # Safety
///
/// `packed` must be a packed pointer returned by the host per ABI §3: the buffer was allocated by this contract's
/// `ksana_alloc(len)` and has been filled with `len` bytes.
unsafe fn take_packed(packed: i64) -> Vec<u8> {
    let (ptr, len) = unpack(packed);
    if len == 0 {
        return Vec::new();
    }
    if ptr == 0 {
        abort();
    }
    // SAFETY: `ksana_alloc` allocated through the global allocator with `Layout(len, 1)`, matching the layout of a
    // `Vec<u8>` with capacity `len`; the content has been filled completely by the host.
    unsafe { Vec::from_raw_parts(ptr as *mut u8, len, len) }
}

// ---------------------------------------------------------------------------
// Entry points (called by the exported functions generated by `entry!`).

/// `ksana_alloc(len) -> ptr`: allocates `len` bytes (alignment 1). Traps when `len` is negative or memory runs out.
pub fn ksana_alloc(len: i32) -> i32 {
    let Ok(len) = usize::try_from(len) else {
        abort()
    };
    let ptr = if len == 0 {
        bump(0, 1).as_ptr()
    } else {
        // SAFETY: `len > 0`, alignment 1 is valid. The allocator traps when out of memory and never returns a null
        // pointer.
        unsafe { alloc::alloc::alloc(Layout::from_size_align_unchecked(len, 1)) }
    };
    i32::try_from(ptr as usize).unwrap_or_else(|_| abort())
}

/// `ksana_call(ptr, len) -> packed`: decodes the input, dispatches, returns the packed output (ABI §2).
pub fn ksana_call<F>(ptr: i32, len: i32, dispatch: F) -> i64
where
    F: FnOnce(&str, &[u8]) -> CallResult,
{
    let (Ok(ptr), Ok(len)) = (usize::try_from(ptr), usize::try_from(len)) else {
        abort()
    };
    let input: &[u8] = if len == 0 {
        &[]
    } else if ptr == 0 {
        abort()
    } else {
        // SAFETY: the VM first calls `ksana_alloc(len)` to get `ptr` and writes `len` bytes of input before calling
        // this function (ABI §2).
        unsafe { core::slice::from_raw_parts(ptr as *const u8, len) }
    };
    let output: &'static mut [u8] = crate::entry::run(input, dispatch).leak();
    pack(output.as_ptr() as usize, output.len())
}

// ---------------------------------------------------------------------------
// Global bump allocator.

const PAGE: usize = 64 * 1024;

/// The next free address; 0 means not yet initialized.
static HEAP_NEXT: AtomicUsize = AtomicUsize::new(0);
/// Current size of linear memory in bytes.
static HEAP_END: AtomicUsize = AtomicUsize::new(0);

/// Initializes the heap: starts at the end of the initial linear memory (the stack and static data are all before it);
/// returns the next free address.
fn heap_next() -> usize {
    let next = HEAP_NEXT.load(Ordering::Relaxed);
    if next != 0 {
        return next;
    }
    let end = wasm32::memory_size(0)
        .checked_mul(PAGE)
        .unwrap_or_else(|| abort());
    HEAP_END.store(end, Ordering::Relaxed);
    // Address 0 is reserved (a null pointer means allocation failure).
    let start = end.max(PAGE);
    HEAP_NEXT.store(start, Ordering::Relaxed);
    start
}

/// Ensures `[0, new_end)` is all within linear memory, calling `memory.grow` when necessary; returns false on failure.
fn ensure_capacity(new_end: usize) -> bool {
    let end = HEAP_END.load(Ordering::Relaxed);
    let Some(missing) = new_end.checked_sub(end) else {
        return true;
    };
    if missing == 0 {
        return true;
    }
    let pages = missing.div_ceil(PAGE);
    let previous = wasm32::memory_grow(0, pages);
    if previous == usize::MAX {
        return false;
    }
    match previous
        .checked_add(pages)
        .and_then(|p| p.checked_mul(PAGE))
    {
        Some(new_size) => {
            HEAP_END.store(new_size, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Allocates `size` bytes aligned to `align` (a power of 2). Traps directly when out of memory and never returns a
/// null pointer.
fn bump(size: usize, align: usize) -> NonNull<u8> {
    let next = heap_next();
    let Some(mask) = align.checked_sub(1) else {
        abort()
    };
    let Some(start) = next.checked_add(mask).map(|v| v & !mask) else {
        abort()
    };
    let Some(end) = start.checked_add(size) else {
        abort()
    };
    if !ensure_capacity(end) {
        abort();
    }
    let Some(ptr) = NonNull::new(start as *mut u8) else {
        abort()
    };
    HEAP_NEXT.store(end, Ordering::Relaxed);
    ptr
}

/// Implementation of `realloc`: the most recently allocated block is resized in place, otherwise a new block is
/// allocated and the content copied. Likewise never returns a null pointer.
///
/// # Safety
///
/// `ptr` must be a block allocated by this allocator with `old_size`, `align` and still in use.
unsafe fn resize(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> NonNull<u8> {
    let Some(old) = NonNull::new(ptr) else {
        abort()
    };
    let addr = ptr as usize;
    if addr.checked_add(old_size) == Some(HEAP_NEXT.load(Ordering::Relaxed)) {
        let Some(new_end) = addr.checked_add(new_size) else {
            abort()
        };
        if !ensure_capacity(new_end) {
            abort();
        }
        HEAP_NEXT.store(new_end, Ordering::Relaxed);
        return old;
    }
    let new = bump(new_size, align);
    // SAFETY: the new block was just allocated and does not overlap the old one; both have at least
    // `min(old_size, new_size)` bytes.
    unsafe { core::ptr::copy_nonoverlapping(ptr, new.as_ptr(), old_size.min(new_size)) };
    new
}

struct BumpAllocator;

// SAFETY: single-threaded (wasm32 has no threads); the returned memory blocks do not overlap and satisfy the alignment;
// `dealloc` does nothing; out of memory traps (`GlobalAlloc` allows aborting instead of returning a null pointer).
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(layout.size(), layout.align()).as_ptr()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the `GlobalAlloc::realloc` calling convention guarantees that `ptr` was allocated by this allocator
        // with `layout`.
        unsafe { resize(ptr, layout.size(), layout.align(), new_size) }.as_ptr()
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator;

/// A panic traps directly without formatting any message (avoiding linking `core::fmt`).
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    abort()
}
