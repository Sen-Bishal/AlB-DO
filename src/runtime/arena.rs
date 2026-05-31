//! Request-scoped bump arena for the QuickJS runtime executor (Movement III).
//!
//! QuickJS allocates *everything* — every object, string, shape and atom — through
//! a single `JSMallocFunctions` table. This module supplies that table as a bump
//! allocator split into two regions so that the cost model becomes:
//!
//! * **persistent region** — warmup state (builtins, loaded modules) and any table
//!   the runtime grows in place across requests. Never rewound while the engine lives.
//! * **request region** — everything a single render allocates. Allocation is a
//!   pointer bump; `free` is a no-op; at the request boundary the whole region is
//!   reclaimed in O(1) by resetting the bump cursor. No per-allocation GC churn.
//!
//! The invariant that makes the O(1) reset safe: by the time a render returns, QuickJS
//! has refcounted away every acyclic request object (and removed its shapes/atoms from
//! the runtime-global tables); the engine then runs the cycle collector once before the
//! reset so cyclic garbage is gone too. What remains referenced from the global tables
//! lives only in the persistent region. The single hazard — QuickJS *reallocating* a
//! persistent table mid-render — is neutralised by dispatching `realloc`/`dealloc` on
//! the pointer's region (a persistent pointer's realloc stays persistent), not on the
//! current mode.
//!
//! Single-threaded by construction: a QuickJS `Runtime` is not `Send`, and the engine
//! drives allocation and the `begin_request`/`end_request` control points from that one
//! thread. Counters use relaxed atomics purely so the handle stays `Send + Sync`.

// An allocator is pointer arithmetic by nature: it stores region bases as integers,
// reconstructs pointers from them, and offsets within blocks. These casts and the
// offset math are intrinsic here (and provenance-correct on every supported target via
// exposed-provenance `as` semantics), so the crate-wide restriction lints are scoped off
// for this module rather than littered across every line.
#![allow(clippy::as_conversions)]
#![allow(clippy::arithmetic_side_effects)]

use rquickjs::allocator::Allocator;
use std::alloc::{self, Layout};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

/// QuickJS's largest scalar is a `u64`, so every block must be 8-byte aligned.
const ALLOC_ALIGN: usize = std::mem::align_of::<u64>();
/// Each block is prefixed with its usable size so the static `usable_size` can recover
/// it from a bare pointer. One `usize` is already `ALLOC_ALIGN` wide, so the header keeps
/// the payload aligned.
const HEADER_SIZE: usize = ALLOC_ALIGN;

const DEFAULT_PERSISTENT_CAP: usize = 16 * 1024 * 1024;
const DEFAULT_REQUEST_CAP: usize = 16 * 1024 * 1024;

#[inline]
fn round_up(size: usize) -> usize {
    (size + ALLOC_ALIGN - 1) & !(ALLOC_ALIGN - 1)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RegionKind {
    Persistent,
    Request,
}

/// A fixed-capacity slab we bump-allocate within. The backing memory is committed once
/// at construction; `top` is the only thing that moves.
struct Region {
    base: usize,
    cap: usize,
    top: AtomicUsize,
}

impl Region {
    fn new(cap: usize) -> Self {
        let cap = round_up(cap.max(ALLOC_ALIGN));
        let layout = Layout::from_size_align(cap, ALLOC_ALIGN).expect("arena region layout");
        // SAFETY: cap is non-zero and the layout is valid.
        let base = unsafe { alloc::alloc(layout) };
        if base.is_null() {
            alloc::handle_alloc_error(layout);
        }
        Self {
            base: base as usize,
            cap,
            top: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.cap
    }

    /// Bump `total` bytes off the front; returns the block's base address or `None` when
    /// the region is exhausted.
    #[inline]
    fn bump(&self, total: usize) -> Option<usize> {
        let off = self.top.load(Relaxed);
        let end = off.checked_add(total)?;
        if end <= self.cap {
            self.top.store(end, Relaxed);
            Some(self.base + off)
        } else {
            None
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.cap, ALLOC_ALIGN).expect("arena region layout");
        // SAFETY: `base`/`cap` are exactly what we allocated in `new`.
        unsafe { alloc::dealloc(self.base as *mut u8, layout) };
    }
}

/// Shared arena state. The engine holds one `Arc` to drive the request boundary and read
/// stats; QuickJS's runtime holds another (inside the [`ArenaAllocator`] it owns) for the
/// allocation callbacks. The regions outlive every allocation because they are only freed
/// when the last `Arc` drops.
pub struct ArenaControl {
    persistent: Region,
    request: Region,
    in_request: AtomicBool,

    alloc_calls: AtomicUsize,
    realloc_calls: AtomicUsize,
    dealloc_calls: AtomicUsize,
    fallback_allocs: AtomicUsize,
    persistent_grew_in_request: AtomicUsize,
    request_peak: AtomicUsize,
}

/// A point-in-time view of arena usage, used by the guardrail test and diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaStats {
    pub persistent_used: usize,
    pub request_used: usize,
    pub request_peak: usize,
    pub alloc_calls: usize,
    pub realloc_calls: usize,
    pub dealloc_calls: usize,
    pub fallback_allocs: usize,
    pub persistent_grew_in_request: usize,
}

impl ArenaControl {
    pub fn new(persistent_cap: usize, request_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            persistent: Region::new(persistent_cap),
            request: Region::new(request_cap),
            in_request: AtomicBool::new(false),
            alloc_calls: AtomicUsize::new(0),
            realloc_calls: AtomicUsize::new(0),
            dealloc_calls: AtomicUsize::new(0),
            fallback_allocs: AtomicUsize::new(0),
            persistent_grew_in_request: AtomicUsize::new(0),
            request_peak: AtomicUsize::new(0),
        })
    }

    pub fn with_default_caps() -> Arc<Self> {
        Self::new(DEFAULT_PERSISTENT_CAP, DEFAULT_REQUEST_CAP)
    }

    /// Enter request mode: subsequent fresh allocations are request-scoped.
    pub fn begin_request(&self) {
        self.in_request.store(true, Relaxed);
    }

    /// Leave request mode and reclaim the request region in O(1). The caller must have run
    /// the cycle collector first so QuickJS holds no live reference above the watermark.
    pub fn end_request(&self) {
        let used = self.request.top.load(Relaxed);
        self.request_peak.fetch_max(used, Relaxed);
        self.request.top.store(0, Relaxed);
        self.in_request.store(false, Relaxed);
    }

    pub fn stats(&self) -> ArenaStats {
        ArenaStats {
            persistent_used: self.persistent.top.load(Relaxed),
            request_used: self.request.top.load(Relaxed),
            request_peak: self.request_peak.load(Relaxed),
            alloc_calls: self.alloc_calls.load(Relaxed),
            realloc_calls: self.realloc_calls.load(Relaxed),
            dealloc_calls: self.dealloc_calls.load(Relaxed),
            fallback_allocs: self.fallback_allocs.load(Relaxed),
            persistent_grew_in_request: self.persistent_grew_in_request.load(Relaxed),
        }
    }

    #[inline]
    fn region(&self, kind: RegionKind) -> &Region {
        match kind {
            RegionKind::Persistent => &self.persistent,
            RegionKind::Request => &self.request,
        }
    }

    /// Which region (if any) owns a user pointer. `None` means it came from the system
    /// fallback path.
    #[inline]
    fn locate(&self, user: *mut u8) -> Option<RegionKind> {
        let addr = user as usize;
        if self.request.contains(addr) {
            Some(RegionKind::Request)
        } else if self.persistent.contains(addr) {
            Some(RegionKind::Persistent)
        } else {
            None
        }
    }

    /// Allocate `usable` bytes from a specific region, falling back to the system
    /// allocator when the region is full. `zero` requests zero-initialised memory.
    fn alloc_in(&self, kind: RegionKind, usable: usize, zero: bool) -> *mut u8 {
        let total = HEADER_SIZE + usable;
        if let Some(base) = self.region(kind).bump(total) {
            // SAFETY: `bump` guaranteed `total` bytes from `base`, aligned to ALLOC_ALIGN.
            unsafe {
                (base as *mut usize).write(usable);
                let user = (base + HEADER_SIZE) as *mut u8;
                if zero {
                    ptr::write_bytes(user, 0, usable);
                }
                user
            }
        } else {
            self.fallback_allocs.fetch_add(1, Relaxed);
            system_alloc(usable, zero)
        }
    }

    fn alloc(&self, size: usize, zero: bool) -> *mut u8 {
        self.alloc_calls.fetch_add(1, Relaxed);
        let kind = if self.in_request.load(Relaxed) {
            RegionKind::Request
        } else {
            RegionKind::Persistent
        };
        self.alloc_in(kind, round_up(size), zero)
    }

    /// SAFETY: `user` must be a live pointer previously returned by this arena.
    unsafe fn dealloc(&self, user: *mut u8) {
        self.dealloc_calls.fetch_add(1, Relaxed);
        match self.locate(user) {
            // In-region frees are no-ops; the region is reclaimed wholesale on reset
            // (request) or when the engine drops (persistent).
            Some(_) => {}
            None => system_dealloc(user),
        }
    }

    /// SAFETY: `user` (if non-null) must be a live pointer previously returned by this arena.
    unsafe fn realloc(&self, user: *mut u8, new_size: usize) -> *mut u8 {
        if user.is_null() {
            return self.alloc(new_size, false);
        }
        self.realloc_calls.fetch_add(1, Relaxed);

        let new_usable = round_up(new_size);
        let old_usable = read_header(user);

        let Some(kind) = self.locate(user) else {
            return system_realloc(user, old_usable, new_usable);
        };

        let region = self.region(kind);
        let block_base = user as usize - HEADER_SIZE;
        let old_total = HEADER_SIZE + old_usable;
        let new_total = HEADER_SIZE + new_usable;

        // Fast path: growing/shrinking the most recent allocation in its region — just
        // move the bump cursor and keep the same address. This is the common case for
        // QuickJS's incremental string and array growth.
        let off = block_base - region.base;
        let is_region_top = off + old_total == region.top.load(Relaxed);
        if is_region_top && off + new_total <= region.cap {
            region.top.store(off + new_total, Relaxed);
            (block_base as *mut usize).write(new_usable);
            return user;
        }

        // Slow path: copy into a fresh block *in the same region* so a persistent pointer
        // stays persistent (and survives the next request reset). The old block becomes
        // dead space — reclaimed on reset for the request region, or accepted as taper for
        // the persistent region.
        if kind == RegionKind::Persistent && self.in_request.load(Relaxed) {
            self.persistent_grew_in_request.fetch_add(1, Relaxed);
        }
        let fresh = self.alloc_in(kind, new_usable, false);
        if fresh.is_null() {
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(user, fresh, old_usable.min(new_usable));
        fresh
    }
}

/// The `Allocator` QuickJS owns. Cloning shares the same [`ArenaControl`].
pub struct ArenaAllocator {
    control: Arc<ArenaControl>,
}

impl ArenaAllocator {
    pub fn new(control: Arc<ArenaControl>) -> Self {
        Self { control }
    }
}

// SAFETY: every block (region or fallback) carries a size header, payloads are
// ALLOC_ALIGN-aligned, and `usable_size` recovers the header written at allocation time.
unsafe impl Allocator for ArenaAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        self.control.alloc(size, false)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        match count.checked_mul(size) {
            Some(0) | None => ptr::null_mut(),
            Some(total) => self.control.alloc(total, true),
        }
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        self.control.dealloc(ptr);
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        self.control.realloc(ptr, new_size)
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize
    where
        Self: Sized,
    {
        read_header(ptr)
    }
}

#[inline]
unsafe fn read_header(user: *mut u8) -> usize {
    (user.sub(HEADER_SIZE) as *const usize).read()
}

/// Fallback path: a header-prefixed system allocation, used only when a region is full or
/// a single block exceeds region capacity.
fn system_alloc(usable: usize, zero: bool) -> *mut u8 {
    let total = HEADER_SIZE + usable;
    let layout = match Layout::from_size_align(total, ALLOC_ALIGN) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };
    // SAFETY: total is non-zero (>= HEADER_SIZE) and the layout is valid.
    let base = unsafe {
        if zero {
            alloc::alloc_zeroed(layout)
        } else {
            alloc::alloc(layout)
        }
    };
    if base.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (base as *mut usize).write(usable);
        base.add(HEADER_SIZE)
    }
}

/// SAFETY: `user` must be a system-fallback pointer (not owned by either region).
unsafe fn system_dealloc(user: *mut u8) {
    let base = user.sub(HEADER_SIZE);
    let usable = (base as *const usize).read();
    let layout = Layout::from_size_align_unchecked(HEADER_SIZE + usable, ALLOC_ALIGN);
    alloc::dealloc(base, layout);
}

/// SAFETY: `user` must be a system-fallback pointer with the given old usable size.
unsafe fn system_realloc(user: *mut u8, old_usable: usize, new_usable: usize) -> *mut u8 {
    let base = user.sub(HEADER_SIZE);
    let old_layout = Layout::from_size_align_unchecked(HEADER_SIZE + old_usable, ALLOC_ALIGN);
    let fresh = alloc::realloc(base, old_layout, HEADER_SIZE + new_usable);
    if fresh.is_null() {
        return ptr::null_mut();
    }
    (fresh as *mut usize).write(new_usable);
    fresh.add(HEADER_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Drives the arena's Allocator surface directly, mirroring how QuickJS would call it.
    fn alloc(ctl: &Arc<ArenaControl>, size: usize) -> *mut u8 {
        let mut a = ArenaAllocator::new(ctl.clone());
        a.alloc(size)
    }

    #[test]
    fn header_records_rounded_usable_size_for_static_lookup() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);
        let p = alloc(&ctl, 13);
        // 13 rounds up to the next 8-byte multiple.
        assert_eq!(unsafe { ArenaAllocator::usable_size(p) }, 16);
        assert_eq!(p as usize % ALLOC_ALIGN, 0);
    }

    #[test]
    fn fresh_allocations_follow_request_mode() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);

        let persistent = alloc(&ctl, 32);
        assert_eq!(ctl.locate(persistent), Some(RegionKind::Persistent));

        ctl.begin_request();
        let scoped = alloc(&ctl, 32);
        assert_eq!(ctl.locate(scoped), Some(RegionKind::Request));
        assert!(ctl.stats().request_used > 0);

        ctl.end_request();
        // The request region is reclaimed wholesale; the persistent watermark is untouched.
        assert_eq!(ctl.stats().request_used, 0);
        let persistent_after = ctl.stats().persistent_used;
        assert!(persistent_after > 0);
        assert!(ctl.stats().request_peak >= 32);
    }

    #[test]
    fn end_request_reclaims_without_disturbing_persistent_watermark() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);
        let _persistent = alloc(&ctl, 100);
        let watermark = ctl.stats().persistent_used;

        for _ in 0..50 {
            ctl.begin_request();
            for _ in 0..16 {
                let _ = alloc(&ctl, 64);
            }
            ctl.end_request();
            // Every cycle returns the request region to empty and never touches persistent.
            assert_eq!(ctl.stats().request_used, 0);
            assert_eq!(ctl.stats().persistent_used, watermark);
        }
    }

    #[test]
    fn oversized_allocation_falls_back_to_system_and_frees_cleanly() {
        let ctl = ArenaControl::new(1024, 1024);
        let mut a = ArenaAllocator::new(ctl.clone());
        let big = a.alloc(8192); // larger than either region
        assert!(!big.is_null());
        assert_eq!(ctl.locate(big), None);
        assert_eq!(ctl.stats().fallback_allocs, 1);
        assert_eq!(unsafe { ArenaAllocator::usable_size(big) }, 8192);
        unsafe { a.dealloc(big) }; // must not corrupt / double-free
    }

    #[test]
    fn realloc_of_top_block_grows_in_place() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);
        let mut a = ArenaAllocator::new(ctl.clone());
        let p = a.alloc(32);
        unsafe {
            ptr::write_bytes(p, 0xAB, 32);
            let grown = a.realloc(p, 64);
            assert_eq!(grown, p, "top-of-region realloc keeps the same address");
            assert_eq!(ArenaAllocator::usable_size(grown), 64);
            // Original bytes are preserved.
            assert_eq!(*grown.add(0), 0xAB);
            assert_eq!(*grown.add(31), 0xAB);
        }
    }

    #[test]
    fn realloc_of_buried_block_copies_and_preserves_bytes() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);
        let mut a = ArenaAllocator::new(ctl.clone());
        let p = a.alloc(16);
        unsafe { ptr::write_bytes(p, 0xCD, 16) };
        let _on_top = a.alloc(16); // bury `p` so it is no longer the region top
        unsafe {
            let moved = a.realloc(p, 48);
            assert_ne!(moved, p);
            assert_eq!(ArenaAllocator::usable_size(moved), 48);
            assert_eq!(*moved.add(0), 0xCD);
            assert_eq!(*moved.add(15), 0xCD);
        }
    }

    #[test]
    fn persistent_pointer_realloc_during_request_stays_persistent() {
        let ctl = ArenaControl::new(64 * 1024, 64 * 1024);
        let mut a = ArenaAllocator::new(ctl.clone());

        // A persistent block, then bury it so realloc must move.
        let persistent = a.alloc(16);
        let _bury = a.alloc(16);

        ctl.begin_request();
        let moved = unsafe { a.realloc(persistent, 64) };
        // The grown block must remain in the persistent region so the request reset below
        // does not free memory the runtime still references.
        assert_eq!(ctl.locate(moved), Some(RegionKind::Persistent));
        assert_eq!(ctl.stats().persistent_grew_in_request, 1);
        ctl.end_request();
        assert_eq!(ctl.locate(moved), Some(RegionKind::Persistent));
    }
}
