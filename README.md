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

Create a new sbitmap with `depth` bits. The `shift` parameter controls how many bits per word (default is auto-calculated for optimal cache usage). The `round_robin` parameter enables strict round-robin allocation order (usually `false` for better performance).

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
- **Scalability**: Linear scaling with number of CPUs up to bitmap size

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
# Run with defaults (32 bits, 10 seconds, N-1 tasks)
cargo run --bin bench_compare --release

# Specify bitmap depth (1024 bits, 10 seconds)
cargo run --bin bench_compare --release -- 1024

# Specify bitmap depth and duration (512 bits, 10 seconds)
cargo run --bin bench_compare --release -- 512 10
```

This benchmark:
- Auto-detects available CPUs and spawns N-1 concurrent tasks
- Measures operations per second (get + put pairs)
- Compares sbitmap vs a baseline lockless implementation
- Defaults: 32 bits, 10 seconds, N-1 tasks (where N is total CPU count)

Parameters:
- `[depth]` - Bitmap size in bits (default: 32)
- `[seconds]` - Benchmark duration (default: 5)

See [benches/README.md](benches/README.md) for more details.

Example output on a 16-CPU system:
```
System: 16 CPUs detected, using 15 tasks for benchmark
Bitmap depth: 32 bits
Duration: 10 seconds

=== Sbitmap (Optimized) Benchmark ===
Configuration:
  - Duration: 5s
  - Tasks: 15
  - Bitmap size: 32 bits

Results:
  Task 0: 4835248 ops, 967049 ops/sec (0.9670 Mops/sec)
  Task 1: 5389185 ops, 1077837 ops/sec (1.0778 Mops/sec)
  ...
  Task 14: 5491968 ops, 1098393 ops/sec (1.0984 Mops/sec)
  Total: 80981724 ops, 16196344 ops/sec (16.1963 Mops/sec)
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
