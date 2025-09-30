// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Fast and scalable bitmap implementation based on Linux kernel's sbitmap
//
// This module provides lock-free, cache-line optimized bitmap allocation
// designed for high-concurrency scenarios like journal entry allocation
// in RAID1 systems.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Cache line size for modern x86_64/aarch64 processors
const CACHE_LINE_SIZE: usize = 64;

/// Bits per word (typically 64 on 64-bit systems)
const BITS_PER_WORD: usize = usize::BITS as usize;

// Per-task allocation hint for reducing contention
// Each task/thread maintains its own hint to avoid conflicts
thread_local! {
    static ALLOC_HINT: Cell<usize> = Cell::new(0);
}

/// Cache-line aligned bitmap word to prevent false sharing
///
/// Each word is placed on its own cache line to ensure that concurrent
/// operations on different words don't cause cache line ping-pong.
#[repr(align(64))]
pub struct SbitmapWord {
    /// Atomic bitmap word - bits set to 1 are allocated, 0 are free
    word: AtomicUsize,
    /// Padding to fill the cache line
    _padding: [u8; CACHE_LINE_SIZE - std::mem::size_of::<AtomicUsize>()],
}

impl SbitmapWord {
    /// Create a new sbitmap word with all bits free
    fn new() -> Self {
        Self {
            word: AtomicUsize::new(0),
            _padding: [0; CACHE_LINE_SIZE - std::mem::size_of::<AtomicUsize>()],
        }
    }
}

/// Scalable bitmap for lock-free bit allocation
///
/// The bitmap is spread across multiple cache lines to reduce contention
/// in multi-threaded scenarios. Each task maintains its own allocation
/// hint to start searching from different positions.
pub struct Sbitmap {
    /// Total number of bits in the bitmap
    depth: usize,
    /// log2(bits per word) - used for fast division/modulo
    shift: u32,
    /// Number of words in the bitmap
    map_nr: usize,
    /// Array of cache-line aligned bitmap words
    map: Vec<SbitmapWord>,
    /// Whether to use strict round-robin allocation
    round_robin: bool,
}

impl Sbitmap {
    /// Create a new sbitmap with the specified depth
    ///
    /// # Arguments
    /// * `depth` - Total number of bits to allocate
    /// * `shift` - Optional log2(bits per word). If None, a sensible default is chosen
    /// * `round_robin` - If true, use strict round-robin allocation order
    ///
    /// # Returns
    /// A new Sbitmap instance
    pub fn new(depth: usize, shift: Option<u32>, round_robin: bool) -> Self {
        let shift = shift.unwrap_or_else(|| Self::calculate_shift(depth));
        let bits_per_word = 1usize << shift;
        let map_nr = (depth + bits_per_word - 1) / bits_per_word; // DIV_ROUND_UP

        let map = (0..map_nr).map(|_| SbitmapWord::new()).collect();

        Self {
            depth,
            shift,
            map_nr,
            map,
            round_robin,
        }
    }

    /// Calculate optimal shift value based on bitmap depth
    ///
    /// This follows the kernel's heuristic: for small bitmaps, use fewer
    /// bits per word to spread across more cache lines for better parallelism.
    fn calculate_shift(depth: usize) -> u32 {
        let mut shift = BITS_PER_WORD.trailing_zeros();

        // If the bitmap is small, shrink the number of bits per word so
        // we spread over a few cachelines, at least. If less than 4
        // bits, just forget about it, it's not going to work optimally.
        if depth >= 4 {
            while (4usize << shift) > depth {
                shift -= 1;
            }
        }

        shift
    }

    /// Get the depth (number of bits) for a specific word index
    #[inline]
    fn map_depth(&self, index: usize) -> usize {
        if index == self.map_nr - 1 {
            // Last word may have fewer bits
            self.depth - (index << self.shift)
        } else {
            1usize << self.shift
        }
    }

    /// Convert bit number to word index
    #[inline]
    fn bit_to_index(&self, bitnr: usize) -> usize {
        bitnr >> self.shift
    }

    /// Convert bit number to bit offset within word
    #[inline]
    fn bit_to_offset(&self, bitnr: usize) -> usize {
        bitnr & ((1usize << self.shift) - 1)
    }

    /// Find the next zero bit in a word starting from hint
    ///
    /// This is equivalent to kernel's find_next_zero_bit within a word.
    #[inline]
    fn find_next_zero_bit(word: usize, depth: usize, hint: usize) -> Option<usize> {
        // Mask off bits before hint
        let mask = !((1usize << hint).wrapping_sub(1));
        let word = word | !mask;

        // Find first zero bit
        let inverted = !word;
        if inverted == 0 {
            return None;
        }

        let bit = inverted.trailing_zeros() as usize;
        if bit < depth {
            Some(bit)
        } else {
            None
        }
    }

    /// Atomically test and set a bit (acquire semantics)
    ///
    /// Returns true if the bit was successfully allocated (was 0, now 1)
    #[inline]
    fn test_and_set_bit_lock(&self, bit_offset: usize, word: &AtomicUsize) -> bool {
        let mask = 1usize << bit_offset;
        let old = word.fetch_or(mask, Ordering::Acquire);
        (old & mask) == 0 // true if bit was previously 0
    }

    /// Atomically clear a bit (release semantics)
    #[inline]
    fn clear_bit(&self, bit_offset: usize, word: &AtomicUsize) {
        let mask = !(1usize << bit_offset);
        word.fetch_and(mask, Ordering::Release);
    }

    /// Try to allocate a bit from a specific word
    fn get_from_word(
        &self,
        word: &AtomicUsize,
        depth: usize,
        alloc_hint: usize,
        wrap: bool,
    ) -> Option<usize> {
        let mut hint = alloc_hint;
        let wrap = wrap && hint > 0; // don't wrap if starting from 0

        loop {
            // Read current word value
            let current = word.load(Ordering::Relaxed);

            // Find next zero bit
            let nr = match Self::find_next_zero_bit(current, depth, hint) {
                Some(bit) => bit,
                None => {
                    // If we started with an offset and wrapping is allowed,
                    // try again from the beginning
                    if hint > 0 && wrap {
                        hint = 0;
                        continue;
                    }
                    return None;
                }
            };

            // Try to atomically set the bit
            if self.test_and_set_bit_lock(nr, word) {
                return Some(nr);
            }

            // Bit was already set, continue searching
            hint = nr + 1;
            if hint >= depth - 1 {
                hint = 0;
            }
        }
    }

    /// Find and allocate a bit starting from the given index
    fn find_bit(&self, start_index: usize, alloc_hint: usize, wrap: bool) -> Option<usize> {
        let mut index = start_index;
        let mut hint = alloc_hint;

        for _ in 0..self.map_nr {
            let depth = self.map_depth(index);
            if depth > 0 {
                if let Some(bit) = self.get_from_word(&self.map[index].word, depth, hint, wrap) {
                    return Some((index << self.shift) + bit);
                }
            }

            // Move to next word
            hint = 0;
            index += 1;
            if index >= self.map_nr {
                index = 0;
            }
        }

        None
    }

    /// Update per-task allocation hint before get
    #[inline]
    fn update_hint_before_get(&self, depth: usize) -> usize {
        ALLOC_HINT.with(|hint| {
            let h = hint.get();
            if h >= depth {
                // Hint is out of range, reset to 0
                hint.set(0);
                0
            } else {
                h
            }
        })
    }

    /// Update per-task allocation hint after successful get
    #[inline]
    fn update_hint_after_get(&self, hint: usize, allocated: Option<usize>) {
        ALLOC_HINT.with(|h| {
            match allocated {
                None => {
                    // Map is full, reset hint to 0
                    h.set(0);
                }
                Some(nr) if nr == hint || self.round_robin => {
                    // Only update if we used the hint or in round-robin mode
                    let next_hint = nr + 1;
                    let next_hint = if next_hint >= self.depth {
                        0
                    } else {
                        next_hint
                    };
                    h.set(next_hint);
                }
                _ => {
                    // Don't update hint if we didn't use it
                }
            }
        });
    }

    /// Allocate a free bit from the bitmap
    ///
    /// This operation provides acquire barrier semantics on success.
    ///
    /// # Returns
    /// * `Some(bit_number)` - Successfully allocated bit number
    /// * `None` - No free bits available
    pub fn get(&self) -> Option<usize> {
        let depth = self.depth;
        let hint = self.update_hint_before_get(depth);
        let index = self.bit_to_index(hint);

        // Calculate bit offset within the word
        let alloc_hint = if self.round_robin {
            self.bit_to_offset(hint)
        } else {
            0
        };

        let allocated = self.find_bit(index, alloc_hint, !self.round_robin);
        self.update_hint_after_get(hint, allocated);
        allocated
    }

    /// Free a previously allocated bit
    ///
    /// This operation provides release barrier semantics, ensuring that
    /// all writes to data associated with this bit are visible before
    /// the bit is freed.
    ///
    /// # Arguments
    /// * `bitnr` - The bit number to free (must have been returned by get())
    pub fn put(&self, bitnr: usize) {
        if bitnr >= self.depth {
            return; // Invalid bit number
        }

        let index = self.bit_to_index(bitnr);
        let offset = self.bit_to_offset(bitnr);

        // Clear the bit atomically with release semantics
        self.clear_bit(offset, &self.map[index].word);

        // Update per-task hint for better cache locality
        if !self.round_robin && bitnr < self.depth {
            ALLOC_HINT.with(|hint| {
                hint.set(bitnr);
            });
        }
    }

    /// Get the total number of bits in the bitmap
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Check if a specific bit is set (allocated)
    pub fn test_bit(&self, bitnr: usize) -> bool {
        if bitnr >= self.depth {
            return false;
        }

        let index = self.bit_to_index(bitnr);
        let offset = self.bit_to_offset(bitnr);
        let word = self.map[index].word.load(Ordering::Relaxed);

        (word & (1usize << offset)) != 0
    }

    /// Count the number of allocated (set) bits
    pub fn weight(&self) -> usize {
        let mut count = 0;
        for i in 0..self.map_nr {
            let word = self.map[i].word.load(Ordering::Relaxed);
            let depth = self.map_depth(i);
            let mask = if depth == BITS_PER_WORD {
                usize::MAX
            } else {
                (1usize << depth) - 1
            };
            count += (word & mask).count_ones() as usize;
        }
        count
    }

    /// Set the round-robin allocation mode
    ///
    /// In round-robin mode, allocation always continues from the last
    /// allocated position. This is stricter but less efficient than
    /// the default mode.
    pub fn set_round_robin(&mut self, enable: bool) {
        self.round_robin = enable;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_sbitmap_basic() {
        let sb = Sbitmap::new(64, None, false);
        assert_eq!(sb.depth(), 64);

        // Allocate a bit
        let bit = sb.get().expect("Should allocate a bit");
        assert!(bit < 64);
        assert!(sb.test_bit(bit));

        // Free the bit
        sb.put(bit);
        assert!(!sb.test_bit(bit));
    }

    #[test]
    fn test_sbitmap_exhaustion() {
        let sb = Sbitmap::new(8, None, false);
        let mut allocated = Vec::new();

        // Allocate all bits
        for _ in 0..8 {
            let bit = sb.get().expect("Should allocate bit");
            allocated.push(bit);
        }

        // Next allocation should fail
        assert!(sb.get().is_none());

        // Free one bit
        sb.put(allocated[0]);

        // Should be able to allocate again
        let bit = sb.get().expect("Should allocate after free");
        assert_eq!(bit, allocated[0]);
    }

    #[test]
    fn test_sbitmap_concurrent() {
        let sb = Arc::new(Sbitmap::new(1024, None, false));
        let mut handles = vec![];

        // Spawn multiple threads to allocate and free bits
        for _ in 0..8 {
            let sb_clone = Arc::clone(&sb);
            let handle = thread::spawn(move || {
                let mut local_bits = Vec::new();

                // Allocate some bits
                for _ in 0..100 {
                    if let Some(bit) = sb_clone.get() {
                        local_bits.push(bit);
                    }
                }

                // Free them
                for bit in local_bits {
                    sb_clone.put(bit);
                }
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // All bits should be free
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_sbitmap_weight() {
        let sb = Sbitmap::new(64, None, false);

        let bit1 = sb.get().unwrap();
        assert_eq!(sb.weight(), 1);

        let bit2 = sb.get().unwrap();
        assert_eq!(sb.weight(), 2);

        sb.put(bit1);
        assert_eq!(sb.weight(), 1);

        sb.put(bit2);
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_sbitmap_small() {
        // Test with small bitmap (< BITS_PER_WORD)
        let sb = Sbitmap::new(5, None, false);
        assert_eq!(sb.depth(), 5);

        let mut bits = Vec::new();
        for _ in 0..5 {
            bits.push(sb.get().expect("Should allocate"));
        }

        // Should be exhausted
        assert!(sb.get().is_none());
        assert_eq!(sb.weight(), 5);

        // Free all
        for bit in bits {
            sb.put(bit);
        }
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_sbitmap_large() {
        // Test with large bitmap spanning multiple words
        let sb = Sbitmap::new(10000, None, false);
        assert_eq!(sb.depth(), 10000);

        let mut bits = Vec::new();
        for _ in 0..100 {
            bits.push(sb.get().expect("Should allocate"));
        }

        assert_eq!(sb.weight(), 100);

        for bit in bits {
            sb.put(bit);
        }
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_per_task_hints() {
        let sb = Arc::new(Sbitmap::new(128, None, false));

        // Each thread should maintain its own hint
        let mut handles = vec![];
        for _ in 0..4 {
            let sb = Arc::clone(&sb);
            handles.push(thread::spawn(move || {
                // Each thread allocates and frees, updating its own hint
                for _ in 0..10 {
                    if let Some(bit) = sb.get() {
                        sb.put(bit);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All bits should be free after all threads complete
        assert_eq!(sb.weight(), 0);
    }
}
