// Benchmark comparison between sbitmap and simple lockless bitmap
//
// This benchmark spawns two tasks on different CPUs and measures
// operations per second (each operation is one get() + put() pair)

use sbitmap::Sbitmap;
use std::env;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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

/// Run benchmark workload: continuous get() and put() operations
fn run_workload<B>(
    bitmap: Arc<B>,
    duration: Duration,
    ops_counter: Arc<AtomicU64>,
) where
    B: Send + Sync + 'static,
    B: BitmapOps,
{
    thread::spawn(move || {
        let start = Instant::now();
        let mut local_ops = 0u64;
        let mut hint = 0;

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

/// Run benchmark with N tasks
fn benchmark<B>(name: &str, bitmap: Arc<B>, duration: Duration, depth: usize, num_tasks: usize)
where
    B: Send + Sync + 'static + BitmapOps,
{
    println!("\n=== {} Benchmark ===", name);
    println!("Configuration:");
    println!("  - Duration: {:?}", duration);
    println!("  - Tasks: {}", num_tasks);
    println!("  - Bitmap size: {} bits", depth);

    // Create counter for each task
    let mut ops_counters = Vec::new();
    for _ in 0..num_tasks {
        ops_counters.push(Arc::new(AtomicU64::new(0)));
    }

    // Spawn tasks
    for i in 0..num_tasks {
        let bitmap_clone = Arc::clone(&bitmap);
        let counter = Arc::clone(&ops_counters[i]);
        run_workload(bitmap_clone, duration, counter);
    }

    // Wait for duration + a bit more for threads to finish
    thread::sleep(duration + Duration::from_millis(100));

    let duration_secs = duration.as_secs_f64();
    let mut total_ops = 0u64;

    println!("\nResults:");
    for i in 0..num_tasks {
        let ops = ops_counters[i].load(Ordering::Relaxed);
        let ops_per_sec = ops as f64 / duration_secs;
        println!("  Task {}: {} ops, {} ops/sec ({:.4} Mops/sec)",
                 i, ops, ops_per_sec as u64, ops_per_sec / 1_000_000.0);
        total_ops += ops;
    }

    let total_ops_per_sec = total_ops as f64 / duration_secs;
    println!("  Total: {} ops, {} ops/sec ({:.4} Mops/sec)",
             total_ops, total_ops_per_sec as u64, total_ops_per_sec / 1_000_000.0);
}

fn main() {
    // Parse command line arguments: [depth] [seconds]
    let args: Vec<String> = env::args().collect();

    // Parse bitmap depth (1st parameter)
    let depth = if args.len() >= 2 {
        args[1].parse::<usize>().unwrap_or_else(|_| {
            eprintln!("Error: Invalid bitmap depth '{}'", args[1]);
            eprintln!("Usage: {} [depth] [seconds]", args[0]);
            eprintln!("Example: {} 1024 10  (1024 bits, 10 seconds)", args[0]);
            std::process::exit(1);
        })
    } else {
        256 // Default depth
    };

    // Parse duration in seconds (2nd parameter)
    let duration_secs = if args.len() >= 3 {
        args[2].parse::<u64>().unwrap_or_else(|_| {
            eprintln!("Error: Invalid duration '{}'", args[2]);
            eprintln!("Usage: {} [depth] [seconds]", args[0]);
            eprintln!("Example: {} 1024 10  (1024 bits, 10 seconds)", args[0]);
            std::process::exit(1);
        })
    } else {
        5 // Default 5 seconds
    };

    let duration = Duration::from_secs(duration_secs);

    // Detect number of available CPUs
    let total_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // Use N-1 CPUs if N > 1, else use N
    let num_cpus = if total_cpus > 1 {
        total_cpus - 1
    } else {
        total_cpus
    };

    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║  Sbitmap vs Simple Lockless Bitmap Benchmark Comparison   ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!();
    println!("System: {} CPUs detected, using {} tasks for benchmark", total_cpus, num_cpus);
    println!("Bitmap depth: {} bits", depth);
    println!("Duration: {} seconds", duration_secs);
    println!("Usage: {} [depth] [seconds] (defaults: 256 bits, 5 seconds)", args[0]);
    println!();

    // Benchmark 1: Sbitmap (cache-line optimized with per-task hints)
    let sbitmap = Arc::new(Sbitmap::new(depth, None, false));
    benchmark("Sbitmap (Optimized)", sbitmap, duration, depth, num_cpus);

    // Benchmark 2: SimpleBitmap (no cache-line optimization, no hints)
    let simple = Arc::new(SimpleBitmap::new(depth));
    benchmark("SimpleBitmap (Baseline)", simple, duration, depth, num_cpus);

    println!("\n╔═══════════════════════════════════════════════════════════╗");
    println!("║  Summary                                                  ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!("
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
", num_cpus);
}
