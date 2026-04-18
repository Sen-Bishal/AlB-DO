//! Atomic bitmap dirty-tracking for the SoA IR column store.
//!
//! Cycle 2 of the SoA IR refactor replaces the per-component
//! [`SentinelRing`](super::hot_set::SentinelRing) linked list with a flat
//! `Box<[AtomicU64]>` indexed by column position. Each bit is one component
//! slot in [`IrColumns`](crate::ir::columns::IrColumns):
//!
//! * **Mark**: `bits[idx / 64].fetch_or(1 << (idx % 64), AcqRel)` — wait-free,
//!   no allocation, branch-predictable on the producer side.
//! * **Drain**: per word `swap(0, AcqRel)` then a `trailing_zeros` pop loop —
//!   64 dirty flags per cache-line load, branch-predictable inner loop.
//!
//! Compared to the linked-list ring this eliminates the per-node pointer
//! chase (one cache miss per dirty entry) and replaces it with a sequential
//! scan over `~capacity / 64` cache lines.
//!
//! The kernel [`hash_diff_into_bitmap`] folds a SIMD `u64x4` equality
//! compare across two `&[u64]` hash columns directly into bitmap words,
//! letting the reconcile pass write 64 dirty bits per output cache line.

use std::sync::atomic::{AtomicU64, Ordering};

use wide::u64x4;

const BITS_PER_WORD: usize = 64;

/// Lock-free dirty bitmap. Bit `idx` represents component slot `idx` in the
/// owning [`IrColumns`](crate::ir::columns::IrColumns).
#[derive(Debug)]
pub struct DirtyBitmap {
    bits: Box<[AtomicU64]>,
    capacity: usize,
}

impl DirtyBitmap {
    /// Allocates a bitmap that tracks `capacity` slots.
    ///
    /// At least one word is always allocated so `mark`/`drain` need no
    /// special-case for empty stores.
    pub fn with_capacity(capacity: usize) -> Self {
        let words = capacity.div_ceil(BITS_PER_WORD).max(1);
        let mut storage = Vec::with_capacity(words);
        for _ in 0..words {
            storage.push(AtomicU64::new(0));
        }
        Self {
            bits: storage.into_boxed_slice(),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn word_count(&self) -> usize {
        self.bits.len()
    }

    /// Sets the bit for `idx`. Returns `true` iff the bit was previously
    /// clear (i.e. this call is the first dirty mark for the slot since the
    /// last drain).
    pub fn mark(&self, idx: usize) -> bool {
        if idx >= self.capacity {
            return false;
        }
        let (word, bit) = (idx / BITS_PER_WORD, 1u64 << (idx % BITS_PER_WORD));
        let prev = self.bits[word].fetch_or(bit, Ordering::AcqRel);
        prev & bit == 0
    }

    /// Reads the bit at `idx` without modifying it.
    pub fn is_set(&self, idx: usize) -> bool {
        if idx >= self.capacity {
            return false;
        }
        let (word, bit) = (idx / BITS_PER_WORD, 1u64 << (idx % BITS_PER_WORD));
        self.bits[word].load(Ordering::Acquire) & bit != 0
    }

    /// Clears the bit at `idx`. Returns `true` iff the bit was previously set.
    pub fn clear(&self, idx: usize) -> bool {
        if idx >= self.capacity {
            return false;
        }
        let (word, bit) = (idx / BITS_PER_WORD, 1u64 << (idx % BITS_PER_WORD));
        let prev = self.bits[word].fetch_and(!bit, Ordering::AcqRel);
        prev & bit != 0
    }

    /// Atomically OR-merges `mask` into word `word_idx`.
    ///
    /// Used by SIMD kernels that produce 64 dirty bits at a time (one word
    /// of equality results) and need to fold them into the live bitmap
    /// without surrendering the wait-free property of `mark`.
    pub fn or_word(&self, word_idx: usize, mask: u64) {
        if let Some(slot) = self.bits.get(word_idx) {
            slot.fetch_or(mask, Ordering::AcqRel);
        }
    }

    /// Returns the number of currently set bits. `O(word_count)`; intended
    /// for diagnostics, not the hot path.
    pub fn count_set(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.load(Ordering::Acquire).count_ones() as usize)
            .sum()
    }

    /// `true` iff no bit is set. Cheap; aborts on the first non-zero word.
    pub fn is_empty(&self) -> bool {
        self.bits
            .iter()
            .all(|word| word.load(Ordering::Acquire) == 0)
    }

    /// Drains every set bit, calling `on_dirty(idx)` exactly once per
    /// dirty slot, then returns the total count.
    ///
    /// Implemented as one `swap(0, AcqRel)` per word followed by a
    /// `trailing_zeros`/`bits &= bits - 1` pop loop. Worst-case work is
    /// `O(capacity / 64 + dirty_count)` and the inner loop is fully
    /// branch-predictable.
    ///
    /// Bits that observers set after the per-word `swap` for that word are
    /// preserved for the next drain: the swap only consumes the snapshot it
    /// observed.
    pub fn drain<F>(&self, mut on_dirty: F) -> usize
    where
        F: FnMut(usize),
    {
        let mut total = 0;
        for (word_idx, atomic_word) in self.bits.iter().enumerate() {
            let mut word = atomic_word.swap(0, Ordering::AcqRel);
            while word != 0 {
                let bit_pos = word.trailing_zeros() as usize;
                let global_idx = word_idx * BITS_PER_WORD + bit_pos;
                if global_idx < self.capacity {
                    on_dirty(global_idx);
                    total += 1;
                }
                word &= word - 1;
            }
        }
        total
    }
}

/// Compares two equal-length hash columns lane-by-lane and folds the
/// inequality mask into `bitmap` using `wide::u64x4` SIMD.
///
/// Returns the number of mismatched lanes (i.e. dirty marks that were
/// OR-ed in). Slots beyond `bitmap.capacity()` are still compared but
/// produce no observable side effect.
///
/// The kernel is the cycle-2 reconcile primitive: hot path code computes
/// new hashes into a `Vec<u64>`, then calls this function once to derive
/// the dirty set in a single linear scan, ~4 hashes per SIMD cycle, with
/// 64 results emitted per cache-line write to the bitmap.
pub fn hash_diff_into_bitmap(old: &[u64], new: &[u64], bitmap: &DirtyBitmap) -> usize {
    let len = old.len().min(new.len());
    if len == 0 {
        return 0;
    }

    let mut total = 0;
    let word_count = len.div_ceil(BITS_PER_WORD);

    for word_idx in 0..word_count {
        let base = word_idx * BITS_PER_WORD;
        let end = (base + BITS_PER_WORD).min(len);
        let mut word_mask: u64 = 0;
        let mut bit_pos: usize = 0;

        // SIMD body: 4 lanes per iteration → 16 iterations cover one word.
        let mut i = base;
        while i + 4 <= end {
            let old_v = u64x4::new([old[i], old[i + 1], old[i + 2], old[i + 3]]);
            let new_v = u64x4::new([new[i], new[i + 1], new[i + 2], new[i + 3]]);
            // `cmp_eq` returns u64::MAX where lanes are equal, 0 otherwise;
            // we want the inverse (mismatch ⇒ dirty).
            let eq = old_v.cmp_eq(new_v).to_array();
            for lane in eq {
                if lane == 0 {
                    word_mask |= 1u64 << bit_pos;
                    total += 1;
                }
                bit_pos += 1;
            }
            i += 4;
        }

        // Scalar tail for the ragged end of the column.
        while i < end {
            if old[i] != new[i] {
                word_mask |= 1u64 << bit_pos;
                total += 1;
            }
            i += 1;
            bit_pos += 1;
        }

        if word_mask != 0 {
            bitmap.or_word(word_idx, word_mask);
        }
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn empty_bitmap_round_trips() {
        let bitmap = DirtyBitmap::with_capacity(0);
        assert_eq!(bitmap.capacity(), 0);
        assert!(bitmap.is_empty());
        assert_eq!(bitmap.drain(|_| panic!("should not drain")), 0);
    }

    #[test]
    fn mark_and_drain_visit_each_index_once() {
        let bitmap = DirtyBitmap::with_capacity(200);
        for idx in [0, 1, 63, 64, 130, 199] {
            assert!(bitmap.mark(idx));
            assert!(!bitmap.mark(idx), "second mark should be a no-op winner");
        }
        let mut drained = Vec::new();
        let count = bitmap.drain(|idx| drained.push(idx));
        assert_eq!(count, 6);
        assert_eq!(drained, vec![0, 1, 63, 64, 130, 199]);
        assert!(bitmap.is_empty());
    }

    #[test]
    fn out_of_range_marks_are_ignored() {
        let bitmap = DirtyBitmap::with_capacity(10);
        assert!(!bitmap.mark(10));
        assert!(!bitmap.mark(usize::MAX));
        assert!(bitmap.is_empty());
    }

    #[test]
    fn clear_removes_a_set_bit() {
        let bitmap = DirtyBitmap::with_capacity(64);
        assert!(bitmap.mark(7));
        assert!(bitmap.is_set(7));
        assert!(bitmap.clear(7));
        assert!(!bitmap.is_set(7));
        assert!(!bitmap.clear(7));
    }

    #[test]
    fn count_set_reports_total_dirty() {
        let bitmap = DirtyBitmap::with_capacity(150);
        bitmap.mark(0);
        bitmap.mark(64);
        bitmap.mark(128);
        assert_eq!(bitmap.count_set(), 3);
    }

    #[test]
    fn concurrent_marks_do_not_lose_bits() {
        let bitmap = Arc::new(DirtyBitmap::with_capacity(1024));
        thread::scope(|scope| {
            for thread_idx in 0..8 {
                let bitmap = Arc::clone(&bitmap);
                scope.spawn(move || {
                    for slot in (thread_idx..1024).step_by(8) {
                        bitmap.mark(slot);
                    }
                });
            }
        });
        let mut drained = Vec::new();
        bitmap.drain(|idx| drained.push(idx));
        assert_eq!(drained.len(), 1024);
        for (expected, actual) in (0..1024).zip(drained) {
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn hash_diff_into_bitmap_marks_only_mismatched_lanes() {
        let bitmap = DirtyBitmap::with_capacity(8);
        let old = [1u64, 2, 3, 4, 5, 6, 7, 8];
        let new = [1u64, 2, 9, 4, 5, 6, 7, 0];
        let dirty = hash_diff_into_bitmap(&old, &new, &bitmap);
        assert_eq!(dirty, 2);
        assert!(bitmap.is_set(2));
        assert!(bitmap.is_set(7));
        assert!(!bitmap.is_set(0));
        assert!(!bitmap.is_set(3));
    }

    #[test]
    fn hash_diff_into_bitmap_handles_ragged_tail() {
        // 67 lanes — exercises both the SIMD body and the scalar tail
        // across two words of the bitmap.
        let old = vec![0u64; 67];
        let mut new = vec![0u64; 67];
        let dirty_indices = [0usize, 5, 63, 64, 66];
        for &idx in &dirty_indices {
            new[idx] = 0xDEAD_BEEF;
        }
        let bitmap = DirtyBitmap::with_capacity(67);
        let dirty = hash_diff_into_bitmap(&old, &new, &bitmap);
        assert_eq!(dirty, dirty_indices.len());
        for &idx in &dirty_indices {
            assert!(bitmap.is_set(idx), "index {idx} should be dirty");
        }
        assert_eq!(bitmap.count_set(), dirty_indices.len());
    }

    #[test]
    fn hash_diff_into_bitmap_unequal_lengths_uses_min() {
        let bitmap = DirtyBitmap::with_capacity(4);
        let old = [1u64, 2, 3, 4, 5];
        let new = [1u64, 7, 3];
        let dirty = hash_diff_into_bitmap(&old, &new, &bitmap);
        assert_eq!(dirty, 1);
        assert!(bitmap.is_set(1));
    }
}
