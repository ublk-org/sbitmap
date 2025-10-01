// Benchmark comparison between sbitmap and simple lockless bitmap
//
// This benchmark spawns two tasks on different CPUs and measures
// operations per second (each operation is one get() + put() pair)

use sbitmap::Sbitmap;
use std::env;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "linux")]
use std::fs;

/// Simple lockless bitmap without cache-line optimization or hints
/// This serves as a baseline for comparison
struct SimpleBitmap {
    depth: usize,
    words: Vec<AtomicUsize>,
}

impl SimpleBitmap {
    fn new(depth: usize) -> Self {
        let num_words = (depth + 63) / 64;
        let words = (0..num_words).map(|_| AtomicUsize::new(0)).collect();

        Self { depth, words }
    }

    fn get(&self) -> Option<usize> {
        // Simple linear scan through all words
        for (word_idx, word) in self.words.iter().enumerate() {
            loop {
                let current = word.load(Ordering::Relaxed);

                // Find first zero bit
                let inverted = !current;
                if inverted == 0 {
                    break; // Word is full
                }

                let bit_pos = inverted.trailing_zeros() as usize;
                if bit_pos >= 64 {
                    break;
                }

                // Check if bit is within bitmap depth
                let global_bit = word_idx * 64 + bit_pos;
                if global_bit >= self.depth {
                    break;
                }

                // Try to atomically set the bit
                let mask = 1usize << bit_pos;
                let old = word.fetch_or(mask, Ordering::Acquire);
                if (old & mask) == 0 {
                    return Some(global_bit);
                }
                // Bit was already set, continue searching in this word
            }
        }
        None
    }

    fn put(&self, bitnr: usize) {
        if bitnr >= self.depth {
            return;
        }

        let word_idx = bitnr / 64;
        let bit_pos = bitnr % 64;
        let mask = !(1usize << bit_pos);

        self.words[word_idx].fetch_and(mask, Ordering::Release);
    }
}

/// Initialize allocation hint combining stack address + system time for better randomization
///
/// This creates a pseudo-random starting point for each task by combining:
/// - Stack address (different for each thread)
/// - Current time in nanoseconds
/// This helps spread allocations across the bitmap and reduce contention.
fn init_hint(depth: usize) -> usize {
    let stack_var = 0u8;
    let addr = &stack_var as *const _ as usize;
    let time_ns = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as usize;
    ((addr / 64).wrapping_add(time_ns)) % depth
}

/// Run benchmark workload: continuous get() and put() operations
fn run_workload<B>(bitmap: Arc<B>, duration: Duration, ops_counter: Arc<AtomicU64>, depth: usize)
where
    B: Send + Sync + 'static,
    B: BitmapOps,
{
    thread::spawn(move || {
        let start = Instant::now();
        let mut local_ops = 0u64;
        let mut hint = init_hint(depth);

        while start.elapsed() < duration {
            // One operation = get() + put()
            if let Some(bit) = bitmap.get(&mut hint) {
                bitmap.put(bit, &mut hint);
                local_ops += 1;
            }
        }

        ops_counter.fetch_add(local_ops, Ordering::Relaxed);
    });
}

/// Run benchmark workload with batch operations: continuous get_batch() and put_batch() operations
fn run_batch_workload(
    bitmap: Arc<Sbitmap>,
    duration: Duration,
    ops_counter: Arc<AtomicU64>,
    depth: usize,
    batch_size: usize,
) {
    thread::spawn(move || {
        let start = Instant::now();
        let mut local_ops = 0u64;
        let mut hint = init_hint(depth);

        while start.elapsed() < duration {
            // One operation = get_batch() + put_batch()
            if let Some(bit) = bitmap.get_batch(batch_size, &mut hint) {
                bitmap.put_batch(bit, batch_size, &mut hint);
                local_ops += 1;
            }
        }

        ops_counter.fetch_add(local_ops, Ordering::Relaxed);
    });
}

/// Trait for bitmap operations to allow generic benchmarking
trait BitmapOps {
    fn get(&self, hint: &mut usize) -> Option<usize>;
    fn put(&self, bitnr: usize, hint: &mut usize);
}

impl BitmapOps for Sbitmap {
    fn get(&self, hint: &mut usize) -> Option<usize> {
        Sbitmap::get(self, hint)
    }

    fn put(&self, bitnr: usize, hint: &mut usize) {
        Sbitmap::put(self, bitnr, hint)
    }
}

impl BitmapOps for SimpleBitmap {
    fn get(&self, _hint: &mut usize) -> Option<usize> {
        SimpleBitmap::get(self)
    }

    fn put(&self, bitnr: usize, _hint: &mut usize) {
        SimpleBitmap::put(self, bitnr)
    }
}

/// Detect number of NUMA nodes by reading /sys/devices/system/node/ (Linux only)
#[cfg(target_os = "linux")]
fn detect_numa_nodes() -> usize {
    match fs::read_dir("/sys/devices/system/node") {
        Ok(entries) => {
            let count = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("node")
                        && e.file_name()
                            .to_string_lossy()
                            .chars()
                            .skip(4)
                            .all(|c| c.is_ascii_digit())
                })
                .count();
            if count > 0 {
                count
            } else {
                1
            }
        }
        Err(_) => 1, // Default to 1 if we can't detect
    }
}

/// Detect number of NUMA nodes (non-Linux platforms)
#[cfg(not(target_os = "linux"))]
fn detect_numa_nodes() -> usize {
    1 // Default to 1 on non-Linux platforms
}

/// Print usage information
fn print_usage(program: &str) {
    eprintln!("Usage: {} [OPTIONS]", program);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --depth DEPTH      Bitmap depth in bits (default: 32)");
    eprintln!("  --shift SHIFT      log2(bits per word) (default: auto-calculated)");
    eprintln!("  --time TIME        Benchmark duration in seconds (default: 10)");
    eprintln!("  --tasks TASKS      Number of concurrent tasks (default: NUM_CPUS - 1)");
    eprintln!(
        "  --batch NR_BITS    Use get_batch/put_batch with NR_BITS (default: 1, single bit mode)"
    );
    eprintln!("  --round-robin      Enable round-robin allocation mode (default: disabled)");
    eprintln!("  -h, --help         Show this help message");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} --depth 1024 --time 5", program);
    eprintln!("  {} --depth 512 --shift 5 --time 10", program);
    eprintln!("  {} --depth 256 --tasks 8 --round-robin", program);
    eprintln!("  {} --depth 128 --batch 4", program);
}

/// Run benchmark with N tasks
fn benchmark<B>(name: &str, bitmap: Arc<B>, duration: Duration, depth: usize, num_tasks: usize)
where
    B: Send + Sync + 'static + BitmapOps,
{
    benchmark_internal(
        name,
        duration,
        depth,
        num_tasks,
        None,
        |bitmap_clone, counter| {
            run_workload(bitmap_clone, duration, counter, depth);
        },
        bitmap,
    );
}

/// Run batch benchmark with N tasks (Sbitmap only)
fn batch_benchmark(
    name: &str,
    bitmap: Arc<Sbitmap>,
    duration: Duration,
    depth: usize,
    num_tasks: usize,
    batch_size: usize,
) {
    benchmark_internal(
        name,
        duration,
        depth,
        num_tasks,
        Some(batch_size),
        |bitmap_clone, counter| {
            run_batch_workload(bitmap_clone, duration, counter, depth, batch_size);
        },
        bitmap,
    );
}

/// Internal benchmark implementation shared by benchmark() and batch_benchmark()
fn benchmark_internal<B, F>(
    name: &str,
    duration: Duration,
    depth: usize,
    num_tasks: usize,
    batch_size: Option<usize>,
    spawn_workload: F,
    bitmap: Arc<B>,
) where
    B: Send + Sync + 'static,
    F: Fn(Arc<B>, Arc<AtomicU64>),
{
    // Print header
    if batch_size.is_some() {
        println!("\n=== {} Benchmark (Batch Mode) ===", name);
    } else {
        println!("\n=== {} Benchmark ===", name);
    }

    println!("Configuration:");
    println!("  - Duration: {:?}", duration);
    println!("  - Tasks: {}", num_tasks);
    println!("  - Bitmap depth: {} bits", depth);
    if let Some(batch) = batch_size {
        println!("  - Batch size: {} bits", batch);
    }

    // Create counter for each task
    let mut ops_counters = Vec::new();
    for _ in 0..num_tasks {
        ops_counters.push(Arc::new(AtomicU64::new(0)));
    }

    // Spawn tasks
    for i in 0..num_tasks {
        let bitmap_clone = Arc::clone(&bitmap);
        let counter = Arc::clone(&ops_counters[i]);
        spawn_workload(bitmap_clone, counter);
    }

    // Wait for duration + a bit more for threads to finish
    thread::sleep(duration + Duration::from_millis(100));

    let duration_secs = duration.as_secs_f64();
    let mut total_ops = 0u64;

    println!("\nResults:");
    for i in 0..num_tasks {
        let ops = ops_counters[i].load(Ordering::Relaxed);
        let ops_per_sec = ops as f64 / duration_secs;
        println!(
            "  Task {}: {} ops, {} ops/sec ({:.4} Mops/sec)",
            i,
            ops,
            ops_per_sec as u64,
            ops_per_sec / 1_000_000.0
        );
        total_ops += ops;
    }

    let total_ops_per_sec = total_ops as f64 / duration_secs;
    println!(
        "  Total: {} ops, {} ops/sec ({:.4} Mops/sec)",
        total_ops,
        total_ops_per_sec as u64,
        total_ops_per_sec / 1_000_000.0
    );
}

fn main() {
    // Parse command line arguments: --depth DEPTH --shift SHIFT --time TIME --tasks TASKS --round-robin
    let args: Vec<String> = env::args().collect();

    let mut depth = 32usize; // Default depth
    let mut shift: Option<u32> = None; // Default shift (auto-calculate)
    let mut time = 10u64; // Default time in seconds
    let mut tasks: Option<usize> = None; // Default tasks (auto-calculate: NUM_CPUS - 1)
    let mut batch_size = 1usize; // Default batch size (1 = single bit mode)
    let mut round_robin = false; // Default round-robin mode (disabled)

    // Simple argument parser
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --depth requires a value");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                depth = args[i + 1].parse::<usize>().unwrap_or_else(|_| {
                    eprintln!("Error: Invalid depth value '{}'", args[i + 1]);
                    print_usage(&args[0]);
                    std::process::exit(1);
                });
                i += 2;
            }
            "--shift" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --shift requires a value");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                let shift_val = args[i + 1].parse::<u32>().unwrap_or_else(|_| {
                    eprintln!("Error: Invalid shift value '{}'", args[i + 1]);
                    print_usage(&args[0]);
                    std::process::exit(1);
                });
                shift = Some(shift_val);
                i += 2;
            }
            "--time" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --time requires a value");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                time = args[i + 1].parse::<u64>().unwrap_or_else(|_| {
                    eprintln!("Error: Invalid time value '{}'", args[i + 1]);
                    print_usage(&args[0]);
                    std::process::exit(1);
                });
                i += 2;
            }
            "--tasks" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --tasks requires a value");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                let tasks_val = args[i + 1].parse::<usize>().unwrap_or_else(|_| {
                    eprintln!("Error: Invalid tasks value '{}'", args[i + 1]);
                    print_usage(&args[0]);
                    std::process::exit(1);
                });
                if tasks_val == 0 {
                    eprintln!("Error: tasks must be at least 1");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                tasks = Some(tasks_val);
                i += 2;
            }
            "--batch" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --batch requires a value");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                batch_size = args[i + 1].parse::<usize>().unwrap_or_else(|_| {
                    eprintln!("Error: Invalid batch value '{}'", args[i + 1]);
                    print_usage(&args[0]);
                    std::process::exit(1);
                });
                if batch_size == 0 {
                    eprintln!("Error: batch size must be at least 1");
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                i += 2;
            }
            "--round-robin" => {
                round_robin = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_usage(&args[0]);
                std::process::exit(0);
            }
            _ => {
                eprintln!("Error: Unknown argument '{}'", args[i]);
                print_usage(&args[0]);
                std::process::exit(1);
            }
        }
    }

    let duration_secs = time;

    let duration = Duration::from_secs(duration_secs);

    // Detect number of available CPUs
    let total_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // Detect number of NUMA nodes
    let numa_nodes = detect_numa_nodes();

    // Determine number of tasks to run
    let num_cpus = match tasks {
        Some(t) => t, // Use user-specified value
        None => {
            // Default: Use N-1 CPUs if N > 1, else use N
            if total_cpus > 1 {
                total_cpus - 1
            } else {
                total_cpus
            }
        }
    };

    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║  Sbitmap vs Simple Lockless Bitmap Benchmark Comparison   ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!();
    println!(
        "System: {} CPUs detected, {} NUMA nodes, using {} tasks for benchmark",
        total_cpus, numa_nodes, num_cpus
    );
    println!("Bitmap depth: {} bits", depth);

    // Create sbitmap to get actual configuration
    let sbitmap = Arc::new(Sbitmap::new(depth, shift, round_robin));
    let bits_per_word = sbitmap.bits_per_word();

    if let Some(s) = shift {
        println!("Shift: {} (bits per word: {})", s, bits_per_word);
    } else {
        println!("Shift: auto-calculated (bits per word: {})", bits_per_word);
    }
    println!(
        "Round-robin: {}",
        if round_robin { "enabled" } else { "disabled" }
    );
    println!(
        "Batch size: {} bit{}",
        batch_size,
        if batch_size == 1 { "" } else { "s" }
    );
    println!("Duration: {} seconds", duration_secs);
    println!();

    if batch_size > 1 {
        // Batch mode: only benchmark Sbitmap with get_batch/put_batch
        if batch_size > bits_per_word {
            eprintln!(
                "Error: batch size ({}) exceeds bits_per_word ({})",
                batch_size, bits_per_word
            );
            eprintln!("Batch operations require nr_bits <= bits_per_word()");
            std::process::exit(1);
        }
        batch_benchmark("Sbitmap", sbitmap, duration, depth, num_cpus, batch_size);
    } else {
        // Single bit mode: benchmark both Sbitmap and SimpleBitmap
        // Benchmark 1: Sbitmap (cache-line optimized with per-task hints)
        benchmark("Sbitmap (Optimized)", sbitmap, duration, depth, num_cpus);

        // Benchmark 2: SimpleBitmap (no cache-line optimization, no hints)
        let simple = Arc::new(SimpleBitmap::new(depth));
        benchmark("SimpleBitmap (Baseline)", simple, duration, depth, num_cpus);
    }

    if batch_size == 1 {
        println!("\n╔═══════════════════════════════════════════════════════════╗");
        println!("║  Summary                                                  ║");
        println!("╚═══════════════════════════════════════════════════════════╝");
        println!(
            "
Tasks: {} concurrent tasks

Sbitmap optimizations:
  ✓ Cache-line aligned words (64 bytes per word)
  ✓ Per-task allocation hints (caller-provided, lightweight)
  ✓ Optimized shift calculation for better spreading

SimpleBitmap characteristics:
  ✗ No cache-line alignment (false sharing possible)
  ✗ No allocation hints (always starts from bit 0)
  ✗ Linear scan through all words

Expected: Sbitmap should show higher ops/sec due to:
  - Reduced false sharing between CPUs
  - Better cache locality with caller-provided hints
  - Less contention on bitmap words
",
            num_cpus
        );
    } else {
        println!("\n╔═══════════════════════════════════════════════════════════╗");
        println!("║  Batch Mode Summary                                       ║");
        println!("╚═══════════════════════════════════════════════════════════╝");
        println!(
            "
Tasks: {} concurrent tasks
Batch size: {} consecutive bits

Batch operations:
  ✓ Atomic allocation of {} consecutive bits via get_batch()
  ✓ Atomic deallocation of {} consecutive bits via put_batch()
  ✓ All bits guaranteed within single word (no spanning)
  ✓ Lock-free with acquire/release memory ordering
",
            num_cpus, batch_size, batch_size, batch_size
        );
    }
}
