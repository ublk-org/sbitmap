# sbitmap - Scalable Bitmap Allocator

A fast and scalable lock-free bitmap implementation based on the Linux kernel's sbitmap.

## Overview

`sbitmap` provides a high-performance, cache-line optimized bitmap for concurrent bit allocation across multiple threads. It's designed for scenarios where many threads need to allocate and free bits from a shared pool efficiently.

## Features

- **Lock-free**: All operations use atomic instructions without locks
- **Cache-line aligned**: Each bitmap word is on its own cache line to prevent false sharing
- **Lightweight hints**: Callers pass allocation hints by reference - no thread-local overhead
- **Scalable**: Tested with high concurrency workloads
- **Memory efficient**: Bit-level granularity with minimal overhead

## Design

This implementation is based on the Linux kernel's sbitmap (from `lib/sbitmap.c`), specifically designed for:

- High-concurrency scenarios (multiple queues, multiple threads)
- Efficient resource allocation (journal entries, tags, etc.)
- Low-latency allocation and deallocation

### Key Optimizations

1. **Cache-line separation**: Each `SbitmapWord` is aligned to 64 bytes
2. **Per-task allocation hints**: Caller-provided hints reduce contention without thread-local overhead
3. **Atomic operations**: Acquire/Release semantics for correctness
4. **No deferred clearing**: Direct atomic bit clearing for simplicity

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
sbitmap = "0.1"
```

### Basic Example

```rust
use sbitmap::Sbitmap;

// Create a bitmap with 1024 bits (non-round-robin mode)
let sb = Sbitmap::new(1024, None, false);

// Each caller maintains its own allocation hint
let mut hint = 0;

// Allocate a bit
if let Some(bit) = sb.get(&mut hint) {
    // Use the allocated bit
    println!("Allocated bit: {}", bit);

    // Free it when done
    sb.put(bit, &mut hint);
}
```

### Concurrent Usage

```rust
use sbitmap::Sbitmap;
use std::sync::Arc;
use std::thread;

let sb = Arc::new(Sbitmap::new(1024, None, false));
let mut handles = vec![];

for _ in 0..8 {
    let sb = Arc::clone(&sb);
    handles.push(thread::spawn(move || {
        // Each thread maintains its own hint in local context
        let mut hint = 0;

        // Each thread can safely allocate/free bits
        if let Some(bit) = sb.get(&mut hint) {
            // Do work...
            sb.put(bit, &mut hint);
        }
    }));
}

for h in handles {
    h.join().unwrap();
}
```

## API

### `Sbitmap::new(depth: usize, shift: Option<u32>, round_robin: bool) -> Self`

Create a new sbitmap with `depth` bits. The `shift` parameter controls how many bits per word (2^shift bits per word) and is critical for performance - it determines how bits are spread across multiple cache-line aligned words. When `None`, the shift is auto-calculated for optimal cache usage. The `round_robin` parameter enables strict round-robin allocation order (usually `false` for better performance).

**Understanding the shift parameter:**
- The shift value spreads bits among multiple words, which is key to sbitmap performance
- Each word is on a separate cache line (64 bytes), reducing contention between CPUs
- Smaller shift = more words = better spreading = less contention (but more memory overhead)
- Larger shift = fewer words = more contention (but better memory efficiency)

### `get(&self, hint: &mut usize) -> Option<usize>`

Allocate a free bit. The `hint` parameter is a mutable reference to the caller's allocation hint, which helps reduce contention by spreading allocations across different parts of the bitmap. Returns `Some(bit_number)` on success or `None` if no free bits are available.

### `put(&self, bitnr: usize, hint: &mut usize)`

Free a previously allocated bit. The `hint` parameter is updated to improve cache locality for subsequent allocations.

### `test_bit(&self, bitnr: usize) -> bool`

Check if a bit is currently allocated.

### `weight(&self) -> usize`

Count the number of currently allocated bits.

### `depth(&self) -> usize`

Get the total number of bits in the bitmap.

## Use Cases

- **Tag allocation**: I/O tag allocation for block devices
- **Resource pools**: Any scenario requiring efficient concurrent resource allocation
- **Lock-free data structures**: Building block for concurrent algorithms
- **NUMA machine**: improvement on NUMA machines is obvious

## Performance Characteristics

- **Allocation**: O(n) worst case, O(1) average with hints
- **Deallocation**: O(1)
- **Memory overhead**: ~56 bytes per word (64 bits) due to cache-line alignment
- **Thread safety**: Lock-free with atomic operations
- **Scalability**: Linear scaling with number of CPUs up to bitmap depth

## Performance Tuning

The `shift` parameter is crucial for tuning sbitmap performance based on your workload:

**When to use a smaller shift:**
- **High contention**: When many threads are competing heavily for bit allocation and release, use a smaller shift to spread bits across more words and reduce contention on individual cache lines
- **NUMA systems**: Machines with multiple NUMA nodes benefit significantly from smaller shift values, as this distributes memory accesses across more cache lines and reduces cross-node traffic
- **Many concurrent allocators**: Systems with a high CPU count see better scalability with smaller shift values

**Examples:**
```rust
// High contention scenario (32-core NUMA system)
let sb = Sbitmap::new(1024, Some(4), false);  // 2^4 = 16 bits per word, 64 words

// Low contention scenario (4-core system)
let sb = Sbitmap::new(1024, Some(6), false);  // 2^6 = 64 bits per word, 16 words

// Let sbitmap decide (recommended starting point)
let sb = Sbitmap::new(1024, None, false);     // Auto-calculated based on depth
```

**Trade-offs:**
- Smaller shift improves performance under contention but uses more memory (each word needs 64 bytes for cache-line alignment)
- Larger shift reduces memory overhead but increases contention when many threads compete
- The auto-calculated shift (when `None`) provides a balanced default suitable for most workloads

## Memory Ordering

- `get()`: Acquire semantics - ensures allocated bit is visible before use
- `put()`: Release semantics - ensures all writes complete before bit is freed

## Comparison with Alternatives

| Feature | sbitmap | Mutex + BitVec | AtomicBitSet |
|---------|---------|----------------|--------------|
| Lock-free | ✅ | ❌ | ✅ |
| Cache-optimized | ✅ | ❌ | ❌ |
| Per-thread hints | ✅ | ❌ | ❌ |
| Kernel-proven design | ✅ | ❌ | ❌ |

## Benchmarks

To compare sbitmap performance against a simple lockless bitmap:

```bash
# Run with defaults (32 bits, auto shift, 10 seconds, N-1 tasks)
cargo run --bin bench_compare --release

# Specify bitmap depth and duration
cargo run --bin bench_compare --release -- --depth 1024 --time 5

# Specify bitmap depth, shift, and duration
cargo run --bin bench_compare --release -- --depth 512 --shift 5 --time 10

# Show help
cargo run --bin bench_compare --release -- --help
```

This benchmark:
- Auto-detects available CPUs and spawns N-1 concurrent tasks
- Measures operations per second (get + put pairs)
- Compares sbitmap vs a baseline lockless implementation
- Defaults: 32 bits, auto-calculated shift, 10 seconds, N-1 tasks (where N is total CPU count)

Options:
- `--depth DEPTH` - Bitmap depth in bits (default: 32)
- `--shift SHIFT` - log2(bits per word), auto-calculated if not specified
- `--time TIME` - Benchmark duration in seconds (default: 10)
- `--tasks TASKS` - Number of concurrent tasks (default: NUM_CPUS - 1)
- `--round-robin` - Enable round-robin allocation mode (default: disabled)

See [benches/README.md](benches/README.md) for more details.

Example output on a 32-CPU system:

```
System: 32 CPUs detected, 2 NUMA nodes, using 31 tasks for benchmark
Bitmap depth: 32 bits
Shift: auto-calculated (bits per word: 8)
Duration: 10 seconds


=== Sbitmap (Optimized) Benchmark ===
Configuration:
  - Duration: 10s
  - Tasks: 31
  - Bitmap depth: 32 bits

Results:
  Task 0: 3101117 ops, 310111 ops/sec (0.3101 Mops/sec)
  ...
  Task 30: 3169582 ops, 316958 ops/sec (0.3170 Mops/sec)
  Total: 93604448 ops, 9360444 ops/sec (9.3604 Mops/sec)

=== SimpleBitmap (Baseline) Benchmark ===
Configuration:
  - Duration: 10s
  - Tasks: 31
  - Bitmap depth: 32 bits

Results:
  Task 0: 1998241 ops, 199824 ops/sec (0.1998 Mops/sec)
  ...
  Task 30: 1835360 ops, 183536 ops/sec (0.1835 Mops/sec)
  Total: 62530560 ops, 6253056 ops/sec (6.2531 Mops/sec)
```

## License

Licensed under either of:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Credits

Based on the Linux kernel's sbitmap implementation by Jens Axboe and Facebook contributors.
