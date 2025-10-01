// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Fast and scalable bitmap implementation based on Linux kernel's sbitmap
//
// This module provides lock-free, cache-line optimized bitmap allocation
// designed for high-concurrency scenarios like IO tag allocation.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Cache line size for modern x86_64/aarch64 processors
const CACHE_LINE_SIZE: usize = 64;

/// Bits per word (typically 64 on 64-bit systems)
const BITS_PER_WORD: usize = usize::BITS as usize;

/// Cache-line aligned bitmap word to prevent false sharing
///
/// Each word is placed on its own cache line to ensure that concurrent
/// operations on different words don't cause cache line ping-pong.
#[repr(align(64))]
struct SbitmapWord {
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
        let map_nr = depth.div_ceil(bits_per_word);

        let map = (0..map_nr).map(|_| SbitmapWord::new()).collect();

        log::debug!(
            "sbitmap::new: depth={depth}, shift={shift}, map_nr={map_nr}, bits_per_word={bits_per_word}, round_robin={round_robin}"
        );

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

    /// Find nr_bits consecutive zero bits in a word starting from hint
    ///
    /// Returns the starting position if found, None otherwise.
    #[inline]
    fn find_next_zero_batch(
        word: usize,
        depth: usize,
        hint: usize,
        nr_bits: usize,
    ) -> Option<usize> {
        if depth < nr_bits || hint > depth.saturating_sub(nr_bits) {
            return None;
        }

        let mask = (1usize << nr_bits).wrapping_sub(1);

        for start in hint..=(depth - nr_bits) {
            let bits_mask = mask << start;
            if (word & bits_mask) == 0 {
                return Some(start);
            }
        }

        None
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

    /// Try to allocate nr_bits consecutive bits from a specific word
    fn get_batch_from_word(
        &self,
        word: &AtomicUsize,
        depth: usize,
        alloc_hint: usize,
        nr_bits: usize,
        wrap: bool,
    ) -> Option<usize> {
        if depth < nr_bits {
            return None;
        }

        let mut hint = alloc_hint;
        let wrap = wrap && hint > 0; // don't wrap if starting from 0

        loop {
            // Read current word value
            let current = word.load(Ordering::Relaxed);

            // Find nr_bits consecutive zero bits starting from hint
            let nr = match Self::find_next_zero_batch(current, depth, hint, nr_bits) {
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

            // Try to atomically set all nr_bits bits
            let mask = (1usize << nr_bits).wrapping_sub(1);
            let bits_mask = mask << nr;
            let old = word.fetch_or(bits_mask, Ordering::Acquire);

            // Check if all bits were zero before we set them
            if (old & bits_mask) == 0 {
                return Some(nr);
            }

            // Some bits were already set, continue searching from next position
            hint = nr + 1;
            if hint > depth.saturating_sub(nr_bits) {
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

    /// Find and allocate nr_bits consecutive bits starting from the given index
    fn find_batch(
        &self,
        start_index: usize,
        alloc_hint: usize,
        nr_bits: usize,
        wrap: bool,
    ) -> Option<usize> {
        let mut index = start_index;
        let mut hint = alloc_hint;

        for _ in 0..self.map_nr {
            let depth = self.map_depth(index);
            if depth >= nr_bits {
                if let Some(bit) =
                    self.get_batch_from_word(&self.map[index].word, depth, hint, nr_bits, wrap)
                {
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

    /// Allocate a free bit from the bitmap
    ///
    /// This operation provides acquire barrier semantics on success.
    ///
    /// # Arguments
    /// * `hint` - Mutable reference to caller's allocation hint for reducing contention
    ///
    /// # Returns
    /// * `Some(bit_number)` - Successfully allocated bit number
    /// * `None` - No free bits available
    pub fn get(&self, hint: &mut usize) -> Option<usize> {
        // Validate and sanitize hint
        if *hint >= self.depth {
            *hint = 0;
        }

        let h = *hint;
        let index = self.bit_to_index(h);

        // Calculate bit offset within the word
        let alloc_hint = if self.round_robin {
            self.bit_to_offset(h)
        } else {
            0
        };

        let allocated = self.find_bit(index, alloc_hint, !self.round_robin);

        // Update hint based on allocation result
        match allocated {
            None => {
                // Map is full, reset hint to 0
                *hint = 0;
            }
            Some(nr) if nr == h || self.round_robin => {
                // Only update if we used the hint or in round-robin mode
                let next_hint = nr + 1;
                *hint = if next_hint >= self.depth {
                    0
                } else {
                    next_hint
                };
            }
            _ => {
                // Don't update hint if we didn't use it
            }
        }

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
    /// * `hint` - Mutable reference to caller's allocation hint for better cache locality
    pub fn put(&self, bitnr: usize, hint: &mut usize) {
        if bitnr >= self.depth {
            return; // Invalid bit number
        }

        let index = self.bit_to_index(bitnr);
        let offset = self.bit_to_offset(bitnr);

        // Clear the bit atomically with release semantics
        self.clear_bit(offset, &self.map[index].word);

        // Update hint for better cache locality (non-round-robin mode)
        if !self.round_robin && bitnr < self.depth {
            *hint = bitnr;
        }
    }

    /// Allocate nr_bits consecutive free bits from the bitmap
    ///
    /// This operation provides acquire barrier semantics on success.
    /// Only supports nr_bits <= bits_per_word() to ensure all bits are in the same word.
    ///
    /// # Arguments
    /// * `nr_bits` - Number of consecutive bits to allocate
    /// * `hint` - Mutable reference to caller's allocation hint for reducing contention
    ///
    /// # Returns
    /// * `Some(start_bit)` - Successfully allocated starting bit number
    /// * `None` - No consecutive nr_bits available or nr_bits > bits_per_word()
    pub fn get_batch(&self, nr_bits: usize, hint: &mut usize) -> Option<usize> {
        // Validate nr_bits
        if nr_bits == 0 || nr_bits > self.bits_per_word() {
            return None;
        }

        // Fall back to single bit allocation for nr_bits == 1
        if nr_bits == 1 {
            return self.get(hint);
        }

        // Validate and sanitize hint
        if *hint >= self.depth {
            *hint = 0;
        }

        let h = *hint;
        let index = self.bit_to_index(h);

        // Calculate bit offset within the word
        let alloc_hint = if self.round_robin {
            self.bit_to_offset(h)
        } else {
            0
        };

        let allocated = self.find_batch(index, alloc_hint, nr_bits, !self.round_robin);

        // Update hint based on allocation result
        match allocated {
            None => {
                // Map is full, reset hint to 0
                *hint = 0;
            }
            Some(nr) if nr == h || self.round_robin => {
                // Only update if we used the hint or in round-robin mode
                let next_hint = nr + nr_bits;
                *hint = if next_hint >= self.depth {
                    0
                } else {
                    next_hint
                };
            }
            _ => {
                // Don't update hint if we didn't use it
            }
        }

        allocated
    }

    /// Free nr_bits consecutive previously allocated bits
    ///
    /// This operation provides release barrier semantics, ensuring that
    /// all writes to data associated with these bits are visible before
    /// the bits are freed.
    /// Only supports nr_bits <= bits_per_word() to ensure all bits are in the same word.
    ///
    /// # Arguments
    /// * `bitnr` - The starting bit number to free (must have been returned by get_batch())
    /// * `nr_bits` - Number of consecutive bits to free
    /// * `hint` - Mutable reference to caller's allocation hint for better cache locality
    pub fn put_batch(&self, bitnr: usize, nr_bits: usize, hint: &mut usize) {
        // Validate nr_bits
        if nr_bits == 0 || nr_bits > self.bits_per_word() {
            return;
        }

        // Fall back to single bit deallocation for nr_bits == 1
        if nr_bits == 1 {
            self.put(bitnr, hint);
            return;
        }

        // Validate range
        if bitnr >= self.depth || bitnr + nr_bits > self.depth {
            return; // Invalid bit range
        }

        let start_index = self.bit_to_index(bitnr);
        let end_index = self.bit_to_index(bitnr + nr_bits - 1);

        // Ensure all bits are in the same word
        if start_index != end_index {
            return;
        }

        let offset = self.bit_to_offset(bitnr);
        let mask = (1usize << nr_bits).wrapping_sub(1);
        let clear_mask = !(mask << offset);

        self.map[start_index]
            .word
            .fetch_and(clear_mask, Ordering::Release);

        // Update hint for better cache locality (non-round-robin mode)
        if !self.round_robin && bitnr < self.depth {
            *hint = bitnr;
        }
    }

    /// Get the total number of bits in the bitmap
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Get the number of bits per word
    pub fn bits_per_word(&self) -> usize {
        1usize << self.shift
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

        let mut hint = 0;
        // Allocate a bit
        let bit = sb.get(&mut hint).expect("Should allocate a bit");
        assert!(bit < 64);
        assert!(sb.test_bit(bit));

        // Free the bit
        sb.put(bit, &mut hint);
        assert!(!sb.test_bit(bit));
    }

    #[test]
    fn test_sbitmap_exhaustion() {
        let sb = Sbitmap::new(8, None, false);
        let mut allocated = Vec::new();
        let mut hint = 0;

        // Allocate all bits
        for _ in 0..8 {
            let bit = sb.get(&mut hint).expect("Should allocate bit");
            allocated.push(bit);
        }

        // Next allocation should fail
        assert!(sb.get(&mut hint).is_none());

        // Free one bit
        sb.put(allocated[0], &mut hint);

        // Should be able to allocate again
        let bit = sb.get(&mut hint).expect("Should allocate after free");
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
                let mut hint = 0;

                // Allocate some bits
                for _ in 0..100 {
                    if let Some(bit) = sb_clone.get(&mut hint) {
                        local_bits.push(bit);
                    }
                }

                // Free them
                for bit in local_bits {
                    sb_clone.put(bit, &mut hint);
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
        let mut hint = 0;

        let bit1 = sb.get(&mut hint).unwrap();
        assert_eq!(sb.weight(), 1);

        let bit2 = sb.get(&mut hint).unwrap();
        assert_eq!(sb.weight(), 2);

        sb.put(bit1, &mut hint);
        assert_eq!(sb.weight(), 1);

        sb.put(bit2, &mut hint);
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_sbitmap_small() {
        // Test with small bitmap (< BITS_PER_WORD)
        let sb = Sbitmap::new(5, None, false);
        assert_eq!(sb.depth(), 5);

        let mut bits = Vec::new();
        let mut hint = 0;
        for _ in 0..5 {
            bits.push(sb.get(&mut hint).expect("Should allocate"));
        }

        // Should be exhausted
        assert!(sb.get(&mut hint).is_none());
        assert_eq!(sb.weight(), 5);

        // Free all
        for bit in bits {
            sb.put(bit, &mut hint);
        }
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_sbitmap_large() {
        // Test with large bitmap spanning multiple words
        let sb = Sbitmap::new(10000, None, false);
        assert_eq!(sb.depth(), 10000);

        let mut bits = Vec::new();
        let mut hint = 0;
        for _ in 0..100 {
            bits.push(sb.get(&mut hint).expect("Should allocate"));
        }

        assert_eq!(sb.weight(), 100);

        for bit in bits {
            sb.put(bit, &mut hint);
        }
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_per_task_hints() {
        let sb = Arc::new(Sbitmap::new(128, None, false));

        // Each thread maintains its own hint in local context
        let mut handles = vec![];
        for _ in 0..4 {
            let sb = Arc::clone(&sb);
            handles.push(thread::spawn(move || {
                let mut hint = 0;
                // Each thread allocates and frees, updating its own hint
                for _ in 0..10 {
                    if let Some(bit) = sb.get(&mut hint) {
                        sb.put(bit, &mut hint);
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

    #[test]
    fn test_bits_per_word() {
        let sb = Sbitmap::new(128, Some(6), false);
        assert_eq!(sb.bits_per_word(), 64); // 2^6 = 64

        let sb2 = Sbitmap::new(128, Some(5), false);
        assert_eq!(sb2.bits_per_word(), 32); // 2^5 = 32

        let sb3 = Sbitmap::new(128, Some(4), false);
        assert_eq!(sb3.bits_per_word(), 16); // 2^4 = 16
    }

    #[test]
    fn test_round_robin() {
        // Test that round-robin mode allocates bits in sequential order
        let sb = Sbitmap::new(16, None, true);
        let mut hint = 0;
        let mut allocated = Vec::new();

        // Allocate several bits - should be sequential in round-robin mode
        for i in 0..8 {
            let bit = sb.get(&mut hint).expect("Should allocate bit");
            allocated.push(bit);
            // In round-robin mode, bits should be allocated sequentially
            assert_eq!(
                bit, i,
                "Round-robin should allocate bit {} but got {}",
                i, bit
            );
        }

        // Free some bits in the middle
        sb.put(allocated[3], &mut hint); // Free bit 3
        sb.put(allocated[5], &mut hint); // Free bit 5

        // Allocate more bits - should continue from where we left off (bit 8)
        // and NOT reuse the freed bits 3 and 5 immediately
        let bit8 = sb.get(&mut hint).expect("Should allocate bit 8");
        assert_eq!(bit8, 8, "Round-robin should continue sequentially");

        let bit9 = sb.get(&mut hint).expect("Should allocate bit 9");
        assert_eq!(bit9, 9, "Round-robin should continue sequentially");

        // When we wrap around, we should find the freed bits
        // Allocate more to fill up to the end
        for i in 10..16 {
            let bit = sb.get(&mut hint).expect("Should allocate bit");
            assert_eq!(bit, i, "Round-robin should allocate bit {}", i);
        }

        // Now it should wrap around and find bit 3 (first freed bit)
        let bit = sb.get(&mut hint).expect("Should wrap around");
        assert_eq!(bit, 3, "Should wrap around and find bit 3");

        // Then find bit 5
        let bit = sb.get(&mut hint).expect("Should find bit 5");
        assert_eq!(bit, 5, "Should find bit 5");

        // Now bitmap should be full
        assert!(sb.get(&mut hint).is_none(), "Bitmap should be full");
        assert_eq!(sb.weight(), 16);
    }

    #[test]
    fn test_round_robin_concurrent() {
        // Test that round-robin mode works correctly with concurrent threads
        let sb = Arc::new(Sbitmap::new(128, None, true));
        let mut handles = vec![];

        // Use atomic vectors to collect allocated bits from each thread
        let thread1_bits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread2_bits = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Thread 1: allocate 32 bits
        {
            let sb_clone = Arc::clone(&sb);
            let bits = Arc::clone(&thread1_bits);
            handles.push(thread::spawn(move || {
                let mut hint = 0;
                let mut local_bits = Vec::new();
                for _ in 0..32 {
                    if let Some(bit) = sb_clone.get(&mut hint) {
                        local_bits.push(bit);
                    }
                }
                *bits.lock().unwrap() = local_bits;
            }));
        }

        // Thread 2: allocate 32 bits
        {
            let sb_clone = Arc::clone(&sb);
            let bits = Arc::clone(&thread2_bits);
            handles.push(thread::spawn(move || {
                let mut hint = 0;
                let mut local_bits = Vec::new();
                for _ in 0..32 {
                    if let Some(bit) = sb_clone.get(&mut hint) {
                        local_bits.push(bit);
                    }
                }
                *bits.lock().unwrap() = local_bits;
            }));
        }

        // Wait for both threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        let bits1 = thread1_bits.lock().unwrap();
        let bits2 = thread2_bits.lock().unwrap();

        // Both threads should have allocated 32 bits each
        assert_eq!(bits1.len(), 32);
        assert_eq!(bits2.len(), 32);

        // Total should be 64 bits allocated
        assert_eq!(sb.weight(), 64);

        // Verify no bit is allocated twice - create a set of all allocated bits
        let mut all_bits = std::collections::HashSet::new();
        for &bit in bits1.iter() {
            assert!(all_bits.insert(bit), "Bit {} allocated twice", bit);
            assert!(bit < 128, "Bit {} out of range", bit);
        }
        for &bit in bits2.iter() {
            assert!(all_bits.insert(bit), "Bit {} allocated twice", bit);
            assert!(bit < 128, "Bit {} out of range", bit);
        }

        // Should have exactly 64 unique bits
        assert_eq!(all_bits.len(), 64);

        // Verify round-robin behavior: each thread's bits should be in ascending order
        // In round-robin mode, hint advances sequentially, so bits should be allocated
        // in increasing order (with possible gaps due to concurrent access)
        fn verify_round_robin_order(bits: &[usize], depth: usize) {
            for i in 1..bits.len() {
                let prev = bits[i - 1];
                let curr = bits[i];

                // In round-robin, next bit should be >= prev (increasing order)
                // or it wrapped around to beginning (curr < prev means wrap-around)
                // Wrap-around is OK if we're near the end
                let wrapped_around = curr < prev && prev > depth / 2;

                assert!(
                    curr > prev || wrapped_around,
                    "Round-robin order violated: bits[{}]={}, bits[{}]={} (not increasing and no wrap-around)",
                    i - 1, prev, i, curr
                );
            }
        }

        verify_round_robin_order(&bits1, 128);
        verify_round_robin_order(&bits2, 128);

        // Free all bits from both threads
        let mut hint = 0;
        for &bit in bits1.iter() {
            sb.put(bit, &mut hint);
        }
        for &bit in bits2.iter() {
            sb.put(bit, &mut hint);
        }

        // All bits should be free now
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_batch_basic() {
        let sb = Sbitmap::new(64, None, false);
        let mut hint = 0;

        // Allocate 4 consecutive bits
        let start = sb.get_batch(4, &mut hint).expect("Should allocate 4 bits");
        assert!(start < 64);

        // Verify all 4 bits are set
        for i in 0..4 {
            assert!(sb.test_bit(start + i), "Bit {} should be set", start + i);
        }
        assert_eq!(sb.weight(), 4);

        // Free the 4 bits
        sb.put_batch(start, 4, &mut hint);

        // Verify all 4 bits are clear
        for i in 0..4 {
            assert!(!sb.test_bit(start + i), "Bit {} should be clear", start + i);
        }
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_batch_multiple_allocations() {
        let sb = Sbitmap::new(128, None, false);
        let mut hint = 0;
        let mut batches = Vec::new();

        // Allocate multiple batches of different sizes
        batches.push(sb.get_batch(3, &mut hint).expect("Should allocate 3 bits"));
        batches.push(sb.get_batch(5, &mut hint).expect("Should allocate 5 bits"));
        batches.push(sb.get_batch(2, &mut hint).expect("Should allocate 2 bits"));

        assert_eq!(sb.weight(), 3 + 5 + 2);

        // Free all batches
        sb.put_batch(batches[0], 3, &mut hint);
        sb.put_batch(batches[1], 5, &mut hint);
        sb.put_batch(batches[2], 2, &mut hint);

        assert_eq!(sb.weight(), 0);
    }

    #[test]
    fn test_batch_exhaustion() {
        // Create a small bitmap where we can easily exhaust consecutive bits
        let sb = Sbitmap::new(16, Some(4), false); // 16 bits per word
        let mut hint = 0;

        // Allocate bits in a pattern that leaves no room for 4 consecutive bits
        // Pattern: allocate 3, skip 1, allocate 3, skip 1, etc.
        let bit0 = sb.get_batch(3, &mut hint).expect("Should allocate 3 bits");
        assert_eq!(bit0, 0);

        let _bit4 = sb.get(&mut hint).expect("Should skip to bit 3");
        let _bit5 = sb
            .get_batch(3, &mut hint)
            .expect("Should allocate bits 4-6");

        let _bit8 = sb.get(&mut hint).expect("Should skip to bit 7");
        let _bit9 = sb
            .get_batch(3, &mut hint)
            .expect("Should allocate bits 8-10");

        let _bit12 = sb.get(&mut hint).expect("Should skip to bit 11");
        let _bit13 = sb
            .get_batch(3, &mut hint)
            .expect("Should allocate bits 12-14");

        // Now we have: XXX_XXX_XXX_XXX_ (where X is allocated, _ is free)
        // Trying to allocate 4 consecutive bits should fail
        assert!(
            sb.get_batch(4, &mut hint).is_none(),
            "Should not find 4 consecutive bits"
        );

        // But we can still allocate single bits
        assert!(sb.get(&mut hint).is_some());
    }

    #[test]
    fn test_batch_edge_cases() {
        let sb = Sbitmap::new(64, None, false);
        let mut hint = 0;

        // Test nr_bits = 0
        assert!(sb.get_batch(0, &mut hint).is_none());

        // Test nr_bits > bits_per_word
        let too_large = sb.bits_per_word() + 1;
        assert!(sb.get_batch(too_large, &mut hint).is_none());

        // Test nr_bits = 1 (should work like regular get)
        let bit = sb.get_batch(1, &mut hint).expect("Should allocate 1 bit");
        assert!(sb.test_bit(bit));
        sb.put_batch(bit, 1, &mut hint);
        assert!(!sb.test_bit(bit));

        // Test put_batch with invalid parameters
        sb.put_batch(100, 4, &mut hint); // Out of range, should be no-op
        assert_eq!(sb.weight(), 0);

        sb.put_batch(62, 4, &mut hint); // Would go past depth (64), should be no-op
        assert_eq!(sb.weight(), 0);

        sb.put_batch(10, 0, &mut hint); // nr_bits = 0, should be no-op
        assert_eq!(sb.weight(), 0);
    }

    #[test]
    #[allow(unused_assignments)]
    fn test_batch_word_boundary() {
        // Create bitmap with known word size
        let sb = Sbitmap::new(128, Some(6), false); // 2^6 = 64 bits per word
        let mut hint = 0;

        // Allocate bits near the end of the first word (bits 60-63)
        for i in 60..64 {
            hint = i;
            sb.get(&mut hint).expect("Should allocate bit");
        }

        // Try to allocate a batch starting at bit 62 (would span word boundary)
        // This should fail because bits 62, 63 are in word 0, and bits 64, 65 are in word 1
        hint = 62;
        let batch = sb.get_batch(4, &mut hint);

        // The batch should either:
        // 1. Not start at 62 (because it can't span words), or
        // 2. Be None if no suitable position found
        if let Some(start) = batch {
            // If we got a batch, verify all bits are in the same word
            let start_word = start / 64;
            let end_word = (start + 3) / 64;
            assert_eq!(start_word, end_word, "Batch should not span word boundary");
            sb.put_batch(start, 4, &mut hint);
        }

        // Verify put_batch rejects spanning word boundary
        hint = 0;
        sb.put_batch(62, 4, &mut hint); // Should be rejected (spans words)
                                        // Bits 60-63 should still be allocated since put_batch should reject this
        assert_eq!(sb.weight(), 4);
    }

    #[test]
    fn test_batch_concurrent() {
        let sb = Arc::new(Sbitmap::new(1024, None, false));
        let mut handles = vec![];

        // Spawn multiple threads to allocate and free batches
        for _ in 0..8 {
            let sb_clone = Arc::clone(&sb);
            let handle = thread::spawn(move || {
                let mut local_batches = Vec::new();
                let mut hint = 0;

                // Allocate some batches of varying sizes
                for size in [2, 3, 4, 5, 2, 3].iter() {
                    if let Some(start) = sb_clone.get_batch(*size, &mut hint) {
                        local_batches.push((start, *size));
                    }
                }

                // Verify all allocated bits are set
                for (start, size) in &local_batches {
                    for i in 0..*size {
                        assert!(sb_clone.test_bit(*start + i));
                    }
                }

                // Free them
                for (start, size) in local_batches {
                    sb_clone.put_batch(start, size, &mut hint);
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
    fn test_batch_fragmentation() {
        // Test that batch allocation works correctly with fragmented bitmaps
        let sb = Sbitmap::new(64, None, false);
        let mut hint = 0;

        // Allocate all bits first
        let mut all_bits = Vec::new();
        for _ in 0..64 {
            if let Some(bit) = sb.get(&mut hint) {
                all_bits.push(bit);
            }
        }
        assert_eq!(sb.weight(), 64);

        // Free every other bit to create a fragmented pattern: _X_X_X_X...
        for i in (0..64).step_by(2) {
            sb.put(all_bits[i], &mut hint);
        }
        assert_eq!(sb.weight(), 32);

        // Trying to allocate 2 consecutive bits should fail (all free bits are isolated)
        hint = 0;
        assert!(
            sb.get_batch(2, &mut hint).is_none(),
            "Should not find 2 consecutive bits in fragmented bitmap"
        );

        // Free an adjacent bit to create a gap of 2 consecutive free bits
        sb.put(all_bits[1], &mut hint);

        // Now we should be able to allocate a batch of 2
        hint = 0;
        let batch = sb.get_batch(2, &mut hint);
        assert!(
            batch.is_some(),
            "Should find 2 consecutive bits after creating gap"
        );

        // The batch should be bits 0 and 1
        if let Some(start) = batch {
            assert_eq!(
                start, 0,
                "Should allocate the first available consecutive pair"
            );
        }
    }

    #[test]
    fn test_batch_round_robin() {
        // Test batch allocation in round-robin mode
        let sb = Sbitmap::new(64, None, true);
        let mut hint = 0;

        // In round-robin mode, batches should be allocated sequentially
        let batch1 = sb.get_batch(3, &mut hint).expect("Should allocate batch 1");
        assert_eq!(batch1, 0, "First batch should start at 0");

        let batch2 = sb.get_batch(3, &mut hint).expect("Should allocate batch 2");
        assert_eq!(batch2, 3, "Second batch should start at 3");

        let batch3 = sb.get_batch(4, &mut hint).expect("Should allocate batch 3");
        assert_eq!(batch3, 6, "Third batch should start at 6");

        // Free and verify
        sb.put_batch(batch1, 3, &mut hint);
        sb.put_batch(batch2, 3, &mut hint);
        sb.put_batch(batch3, 4, &mut hint);

        assert_eq!(sb.weight(), 0);
    }
}
