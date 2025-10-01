# Sbitmap Benchmarks

## Comparison Benchmark

The `bench_compare` binary benchmarks sbitmap against a simple lockless bitmap implementation.

### Running the Benchmark

```bash
# Run with defaults (32 bits, auto shift, 10 seconds, N-1 tasks)
cargo run --bin bench_compare --release

# Specify bitmap depth and duration
cargo run --bin bench_compare --release -- --depth 1024 --time 5

# Specify bitmap depth, shift, and duration
cargo run --bin bench_compare --release -- --depth 512 --shift 5 --time 10

# Quick test (128 bits, 2 seconds)
cargo run --bin bench_compare --release -- --depth 128 --time 2

# Run with specific number of tasks
cargo run --bin bench_compare --release -- --depth 256 --tasks 8

# Show help
cargo run --bin bench_compare --release -- --help
```

### Options

- `--depth DEPTH` - Bitmap depth in bits (default: 32)
- `--shift SHIFT` - log2(bits per word), auto-calculated if not specified
- `--time TIME` - Benchmark duration in seconds (default: 10)
- `--tasks TASKS` - Number of concurrent tasks (default: NUM_CPUS - 1)

The benchmark auto-detects available CPUs and uses N-1 tasks (where N is total CPU count). This leaves one CPU for system tasks and ensures maximum contention testing.

### What It Measures

The benchmark spawns **N-1 concurrent tasks** (where N is the number of CPUs). Each task continuously performs:
1. `get(&mut hint)` - Allocate a free bit using caller-provided hint
2. `put(bit, &mut hint)` - Free the allocated bit and update hint

This represents a realistic workload where multiple threads compete for bits from a shared bitmap.

### Metrics

- **Operations per second**: Each operation is one `get()` + `put()` pair
- **Mops/sec**: Millions of operations per second for easier readability
- **Per-task breakdown**: Shows ops/sec and Mops/sec for each task
- **Total throughput**: Combined ops/sec and Mops/sec across all tasks

### Implementations Compared

1. **Sbitmap (Optimized)**
   - Cache-line aligned words (64 bytes each)
   - Per-task allocation hints (caller-provided, lightweight)
   - Optimized shift calculation for better spreading

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
- Small bitmaps (< 256 bits)
- Low contention scenarios
- Very tight loops (where simplicity wins)

### Example Output

On a 32-CPU system with default settings:

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

### Notes

- The benchmark does **not** pin tasks to specific CPUs - it lets the OS scheduler distribute them
- This provides a more realistic multi-core contention scenario
- Performance will vary based on CPU architecture and system load
- For more consistent results, minimize background processes during benchmarking
