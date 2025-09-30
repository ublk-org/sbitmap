# Sbitmap Benchmarks

## Comparison Benchmark

The `bench_compare` binary benchmarks sbitmap against a simple lockless bitmap implementation.

### Running the Benchmark

```bash
# Run with defaults (32 bits, 10 seconds, N-1 tasks)
cargo run --bin bench_compare --release

# Specify bitmap depth (1024 bits, 10 seconds)
cargo run --bin bench_compare --release -- 1024

# Specify bitmap depth and duration (512 bits, 10 seconds)
cargo run --bin bench_compare --release -- 512 10

# Quick test (128 bits, 2 seconds)
cargo run --bin bench_compare --release -- 128 2
```

### Parameters

- `[depth]` - Bitmap size in bits (default: 32)
- `[seconds]` - Benchmark duration in seconds (default: 5)

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

On a 16-CPU system with default settings:

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
  Task 2: 5573127 ops, 1114625 ops/sec (1.1146 Mops/sec)
  ...
  Task 14: 5491968 ops, 1098393 ops/sec (1.0984 Mops/sec)
  Total: 80981724 ops, 16196344 ops/sec (16.1963 Mops/sec)

=== SimpleBitmap (Baseline) Benchmark ===
Configuration:
  - Duration: 10s
  - Tasks: 15
  - Bitmap size: 32 bits

Results:
  Task 0: 5905360 ops, 1181072 ops/sec (1.1811 Mops/sec)
  ...
  Total: 86664153 ops, 17332830 ops/sec (17.3328 Mops/sec)
```

### Notes

- The benchmark does **not** pin tasks to specific CPUs - it lets the OS scheduler distribute them
- This provides a more realistic multi-core contention scenario
- Performance will vary based on CPU architecture and system load
- For more consistent results, minimize background processes during benchmarking
