# Sbitmap Benchmarks

## Comparison Benchmark

The `bench_compare` binary benchmarks sbitmap against a simple lockless bitmap implementation.

### Running the Benchmark

```bash
# Run with default CPUs (0 and 2) - release optimizations
cargo run --bin bench_compare --features libc --release

# Specify custom CPUs (use different physical cores)
cargo run --bin bench_compare --features libc --release -- 0 4

# Run without release (slower, but faster to compile)
cargo run --bin bench_compare --features libc
```

**Important**: CPU 0 and CPU 1 are often hyperthreads on the same physical core. For accurate multi-core testing, use CPUs on different physical cores (e.g., 0 and 2, or 0 and 4).

Check your CPU topology:
```bash
lscpu -e
```

Look at the CORE column - CPUs with different CORE numbers are on different physical cores.

### What It Measures

The benchmark spawns **two tasks** pinned to **different CPUs** (default: CPU 0 and CPU 2). Each task continuously performs:
1. `get()` - Allocate a free bit
2. `put()` - Free the allocated bit

This represents a realistic workload where multiple threads compete for bits from a shared bitmap.

### Metrics

- **Operations per second**: Each operation is one `get()` + `put()` pair
- **Per-task breakdown**: Shows ops/sec for each CPU
- **Total throughput**: Combined ops/sec across both tasks

### Implementations Compared

1. **Sbitmap (Optimized)**
   - Cache-line aligned words (64 bytes each)
   - Per-task allocation hints (thread-local)
   - Optimized shift calculation for spreading

2. **SimpleBitmap (Baseline)**
   - No cache-line alignment
   - No allocation hints (always starts from bit 0)
   - Simple linear scan

### Expected Results

Sbitmap is designed for scenarios with:
- High contention (multiple threads)
- Large bitmaps (where spreading matters)
- Long-running workloads (where hints provide benefit)

SimpleBitmap may perform comparably or better for:
- Small bitmaps (< 1024 bits)
- Low contention scenarios
- Very tight loops (where simplicity wins)

### CPU Pinning

The benchmark uses `sched_setaffinity()` on Linux to pin tasks to specific CPUs. This ensures:
- Consistent measurements
- Real multi-core contention
- Cache effects are visible

On non-Linux platforms, CPU pinning is a no-op.

### Customization

You can modify `benches/compare.rs` to:
- Change bitmap size (line: `let depth = 256;`)
- Adjust duration (line: `let duration = Duration::from_secs(5);`)
- Test more CPUs (add more tasks)
- Test different workload patterns
