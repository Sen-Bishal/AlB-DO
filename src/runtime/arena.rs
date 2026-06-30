//! Persistent warmup arena + QuickJS-managed request memory (Movement III, redesigned).
//!
//! QuickJS allocates *everything* — every object, string, shape and atom — through a
//! single `JSMallocFunctions` table. This module supplies that table. It splits memory by
//! LIFETIME, but defers request-time lifetime management to QuickJS itself:
//!
//! * **persistent region** — a bump arena for warmup/bootstrap state (builtins, loaded
//!   modules, the runtime-global tables QuickJS grows once during warmup). Never freed
//!   while the engine lives; reclaimed wholesale when the engine drops.
//! * **request-time memory** — everything a render/handler allocates after warmup goes to
//!   the *system allocator* and is freed per-block by QuickJS's own refcount + cycle
//!   collector via [`ArenaControl::dealloc`].
//!
//! ## Why not a resettable request region?
//!
//! An earlier design bump-allocated request memory into a second region and reclaimed it
//! in O(1) by resetting a cursor at the request boundary. That is only sound if nothing
//! the runtime still references lives in the region by request end — but QuickJS interns
//! long-lived **shapes** (hidden classes) and **atoms** (property-name strings) through
//! the same allocation path *during* the request, keyed off the per-request object shapes
//! and property names it sees. Those interned structures stay reachable from the
//! runtime-global tables (`shape_hash`, `atom_array`) across requests. A wholesale reset
//! freed that still-live memory, so the next request that reused a dangling shape/atom hit
//! a use-after-free (`js_free_shape0`'s `ref_count == 0` assert, or an access violation).
//!
//! It surfaced the instant a route received per-request props whose *shape* the warmup had
//! not already interned — e.g. a dynamic `[slug]` route, whose `{ params: { slug } }` props
//! object has a shape the empty-props (`{}`) warmup never created. There is no general way
//! to pre-intern every shape/atom an arbitrary app will produce (data-dependent keys,
//! conditional render paths), so warmup tricks can only ever be patchwork. The correct
//! model is to let QuickJS — which tracks each block's true lifetime — own request memory.
//! The persistent bump still pays off for the one-time warmup state that genuinely never
//! frees.
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

#[inline]
fn round_up(size: usize) -> usize {
    (size + ALLOC_ALIGN - 1) & !(ALLOC_ALIGN - 1)
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
/// allocation callbacks. The persistent region outlives every allocation because it is
/// only freed when the last `Arc` drops.
pub struct ArenaControl {
    persistent: Region,
    /// When true, fresh allocations are request-time and routed to the system allocator
    /// (QuickJS owns their lifetime, freeing each via `dealloc`). When false (warmup /
    /// bootstrap), they bump the persistent region.
    in_request: AtomicBool,

    alloc_calls: AtomicUsize,
    realloc_calls: AtomicUsize,
    dealloc_calls: AtomicUsize,
    /// Persistent-region overflow that spilled to the system allocator.
    fallback_allocs: AtomicUsize,
    /// A persistent block QuickJS grew while serving a request (rare — warmup state is
    /// normally stable). Kept persistent so it never moves into freeable memory.
    persistent_grew_in_request: AtomicUsize,
    /// Outstanding request-time (system) bytes. Returns to its baseline after each render
    /// once QuickJS frees the render's garbage — the steady-state health signal.
    system_live_bytes: AtomicUsize,
    /// High-water mark of `system_live_bytes`.
    system_peak_bytes: AtomicUsize,
}

/// A point-in-time view of arena usage, used by the guardrail tests and diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaStats {
    pub persistent_used: usize,
    /// Outstanding request-time bytes the system allocator currently holds.
    pub system_live_bytes: usize,
    /// High-water mark of request-time bytes since construction.
    pub system_peak_bytes: usize,
    pub alloc_calls: usize,
    pub realloc_calls: usize,
    pub dealloc_calls: usize,
    pub fallback_allocs: usize,
    pub persistent_grew_in_request: usize,
}

impl ArenaControl {
    pub fn new(persistent_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            persistent: Region::new(persistent_cap),
            in_request: AtomicBool::new(false),
            alloc_calls: AtomicUsize::new(0),
            realloc_calls: AtomicUsize::new(0),
            dealloc_calls: AtomicUsize::new(0),
            fallback_allocs: AtomicUsize::new(0),
            persistent_grew_in_request: AtomicUsize::new(0),
            system_live_bytes: AtomicUsize::new(0),
            system_peak_bytes: AtomicUsize::new(0),
        })
    }

    pub fn with_default_caps() -> Arc<Self> {
        Self::new(DEFAULT_PERSISTENT_CAP)
    }

    /// Enter request mode: fresh allocations become QuickJS-managed system blocks.
    pub fn begin_request(&self) {
        self.in_request.store(true, Relaxed);
    }

    /// Leave request mode. No region is reset: request memory is owned by QuickJS and
    /// freed per-block via [`Self::dealloc`] (refcount + the cycle collector the engine
    /// runs before this call). Resetting a region here is exactly the unsound step the
    /// old design took — see the module docs.
    pub fn end_request(&self) {
        self.in_request.store(false, Relaxed);
    }

    pub fn stats(&self) -> ArenaStats {
        ArenaStats {
            persistent_used: self.persistent.top.load(Relaxed),
            system_live_bytes: self.system_live_bytes.load(Relaxed),
            system_peak_bytes: self.system_peak_bytes.load(Relaxed),
            alloc_calls: self.alloc_calls.load(Relaxed),
            realloc_calls: self.realloc_calls.load(Relaxed),
            dealloc_calls: self.dealloc_calls.load(Relaxed),
            fallback_allocs: self.fallback_allocs.load(Relaxed),
            persistent_grew_in_request: self.persistent_grew_in_request.load(Relaxed),
        }
    }

    #[inline]
    fn is_persistent(&self, user: *mut u8) -> bool {
        self.persistent.contains(user as usize)
    }

    /// Bump `usable` bytes off the persistent region, spilling to the system allocator if
    /// it is full. A spilled block is a normal system block (freed via `dealloc`).
    fn alloc_persistent(&self, usable: usize, zero: bool) -> *mut u8 {
        let total = HEADER_SIZE + usable;
        if let Some(base) = self.persistent.bump(total) {
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
            self.alloc_system(usable, zero)
        }
    }

    /// System allocation, tracked in the request-time live-bytes gauge.
    fn alloc_system(&self, usable: usize, zero: bool) -> *mut u8 {
        let user = system_alloc(usable, zero);
        if !user.is_null() {
            let live = self.system_live_bytes.fetch_add(usable, Relaxed) + usable;
            self.system_peak_bytes.fetch_max(live, Relaxed);
        }
        user
    }

    fn alloc(&self, size: usize, zero: bool) -> *mut u8 {
        self.alloc_calls.fetch_add(1, Relaxed);
        let usable = round_up(size);
        if self.in_request.load(Relaxed) {
            // Request-time: hand QuickJS a system block it can free per-object. We must
            // NOT bump this into a resettable region — QuickJS interns long-lived
            // shapes/atoms through here mid-request, and a wholesale reset would free
            // memory still reachable from the runtime-global tables. (See module docs.)
            self.alloc_system(usable, zero)
        } else {
            // Warmup / bootstrap: permanent state — bump the persistent region.
            self.alloc_persistent(usable, zero)
        }
    }

    /// SAFETY: `user` must be a live pointer previously returned by this arena.
    unsafe fn dealloc(&self, user: *mut u8) {
        self.dealloc_calls.fetch_add(1, Relaxed);
        if self.is_persistent(user) {
            // Persistent block: reclaimed wholesale when the engine drops.
            return;
        }
        // System block (request-time, or a persistent-region spill).
        let usable = read_header(user);
        self.system_live_bytes.fetch_sub(usable, Relaxed);
        system_dealloc(user);
    }

    /// SAFETY: `user` (if non-null) must be a live pointer previously returned by this arena.
    unsafe fn realloc(&self, user: *mut u8, new_size: usize) -> *mut u8 {
        if user.is_null() {
            return self.alloc(new_size, false);
        }
        self.realloc_calls.fetch_add(1, Relaxed);

        let new_usable = round_up(new_size);
        let old_usable = read_header(user);

        if !self.is_persistent(user) {
            // System block: a real realloc, with the live-bytes gauge adjusted by the delta.
            let fresh = system_realloc(user, old_usable, new_usable);
            if !fresh.is_null() {
                if new_usable >= old_usable {
                    let delta = new_usable - old_usable;
                    let live = self.system_live_bytes.fetch_add(delta, Relaxed) + delta;
                    self.system_peak_bytes.fetch_max(live, Relaxed);
                } else {
                    self.system_live_bytes
                        .fetch_sub(old_usable - new_usable, Relaxed);
                }
            }
            return fresh;
        }

        // Persistent block. Fast path: grow/shrink the most recent allocation in its
        // region — just move the bump cursor and keep the same address.
        let block_base = user as usize - HEADER_SIZE;
        let off = block_base - self.persistent.base;
        let old_total = HEADER_SIZE + old_usable;
        let new_total = HEADER_SIZE + new_usable;
        if off + old_total == self.persistent.top.load(Relaxed) && off + new_total <= self.persistent.cap
        {
            self.persistent.top.store(off + new_total, Relaxed);
            (block_base as *mut usize).write(new_usable);
            return user;
        }

        // Slow path: copy into a fresh persistent block so the pointer stays persistent
        // (warmup state must never move into freeable memory). The old block is dead
        // space, reclaimed when the engine drops.
        if self.in_request.load(Relaxed) {
            self.persistent_grew_in_request.fetch_add(1, Relaxed);
        }
        let fresh = self.alloc_persistent(new_usable, false);
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

// SAFETY: every block (persistent or system) carries a size header, payloads are
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

/// A header-prefixed system allocation: request-time memory, or a persistent-region spill.
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

/// SAFETY: `user` must be a system block (not owned by the persistent region).
unsafe fn system_dealloc(user: *mut u8) {
    let base = user.sub(HEADER_SIZE);
    let usable = (base as *const usize).read();
    let layout = Layout::from_size_align_unchecked(HEADER_SIZE + usable, ALLOC_ALIGN);
    alloc::dealloc(base, layout);
}

/// SAFETY: `user` must be a system block with the given old usable size.
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
        let ctl = ArenaControl::new(64 * 1024);
        let p = alloc(&ctl, 13);
        // 13 rounds up to the next 8-byte multiple.
        assert_eq!(unsafe { ArenaAllocator::usable_size(p) }, 16);
        assert_eq!(p as usize % ALLOC_ALIGN, 0);
    }

    #[test]
    fn warmup_allocations_bump_persistent_request_allocations_go_to_system() {
        let ctl = ArenaControl::new(64 * 1024);

        // Outside a request (warmup/bootstrap): bump the persistent region.
        let persistent = alloc(&ctl, 32);
        assert!(ctl.is_persistent(persistent));
        let watermark = ctl.stats().persistent_used;
        assert!(watermark > 0);

        // Inside a request: QuickJS-managed system memory, not the persistent region.
        ctl.begin_request();
        let mut a = ArenaAllocator::new(ctl.clone());
        let scoped = a.alloc(32);
        assert!(!ctl.is_persistent(scoped), "request alloc must be a system block");
        assert!(ctl.stats().system_live_bytes >= 32);
        assert!(ctl.stats().system_peak_bytes >= 32);

        // `end_request` does NOT reset anything — the block lives until QuickJS frees it.
        ctl.end_request();
        assert!(ctl.stats().system_live_bytes >= 32);
        assert_eq!(
            ctl.stats().persistent_used,
            watermark,
            "request memory never touches the persistent watermark"
        );

        // QuickJS frees it per-block; the live gauge returns to baseline.
        unsafe { a.dealloc(scoped) };
        assert_eq!(ctl.stats().system_live_bytes, 0);
        assert_eq!(ctl.stats().persistent_used, watermark);
    }

    #[test]
    fn steady_state_request_churn_never_grows_persistent() {
        let ctl = ArenaControl::new(64 * 1024);
        let _persistent = alloc(&ctl, 100);
        let watermark = ctl.stats().persistent_used;

        for _ in 0..50 {
            ctl.begin_request();
            let mut a = ArenaAllocator::new(ctl.clone());
            let mut blocks = Vec::new();
            for _ in 0..16 {
                blocks.push(a.alloc(64));
            }
            // QuickJS frees the request's blocks (refcount/GC) — modelled here explicitly.
            for b in blocks {
                unsafe { a.dealloc(b) };
            }
            ctl.end_request();
            // Every cycle returns request memory to zero and never touches persistent.
            assert_eq!(ctl.stats().system_live_bytes, 0);
            assert_eq!(ctl.stats().persistent_used, watermark);
        }
    }

    #[test]
    fn persistent_overflow_falls_back_to_system_and_frees_cleanly() {
        let ctl = ArenaControl::new(1024);
        let mut a = ArenaAllocator::new(ctl.clone());
        let big = a.alloc(8192); // larger than the persistent region
        assert!(!big.is_null());
        assert!(!ctl.is_persistent(big));
        assert_eq!(ctl.stats().fallback_allocs, 1);
        assert_eq!(unsafe { ArenaAllocator::usable_size(big) }, 8192);
        unsafe { a.dealloc(big) }; // must not corrupt / double-free
    }

    #[test]
    fn realloc_of_top_persistent_block_grows_in_place() {
        let ctl = ArenaControl::new(64 * 1024);
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
    fn realloc_of_buried_persistent_block_copies_and_preserves_bytes() {
        let ctl = ArenaControl::new(64 * 1024);
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
        let ctl = ArenaControl::new(64 * 1024);
        let mut a = ArenaAllocator::new(ctl.clone());

        // A persistent block, then bury it so realloc must move.
        let persistent = a.alloc(16);
        let _bury = a.alloc(16);

        ctl.begin_request();
        let moved = unsafe { a.realloc(persistent, 64) };
        // The grown block must remain persistent even mid-request, so it is never a
        // system block QuickJS could free out from under a runtime-global reference.
        assert!(ctl.is_persistent(moved));
        assert_eq!(ctl.stats().persistent_grew_in_request, 1);
        ctl.end_request();
        assert!(ctl.is_persistent(moved));
    }

    #[test]
    fn system_block_realloc_tracks_live_bytes() {
        let ctl = ArenaControl::new(64 * 1024);
        let mut a = ArenaAllocator::new(ctl.clone());
        ctl.begin_request();
        let p = a.alloc(32);
        assert_eq!(ctl.stats().system_live_bytes, 32);
        let grown = unsafe { a.realloc(p, 96) };
        assert_eq!(ctl.stats().system_live_bytes, 96);
        let shrunk = unsafe { a.realloc(grown, 48) };
        assert_eq!(ctl.stats().system_live_bytes, 48);
        unsafe { a.dealloc(shrunk) };
        assert_eq!(ctl.stats().system_live_bytes, 0);
        ctl.end_request();
    }
}
