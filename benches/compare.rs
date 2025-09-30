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

/// Set CPU affinity for current thread (Linux-specific)
#[cfg(target_os = "linux")]
fn set_cpu_affinity(cpu: usize) -> Result<(), String> {
    use std::mem;

    let mut cpu_set: libc::cpu_set_t = unsafe { mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut cpu_set);
        libc::CPU_SET(cpu, &mut cpu_set);

        let result = libc::sched_setaffinity(
            0, // current thread
            mem::size_of::<libc::cpu_set_t>(),
            &cpu_set,
        );

        if result != 0 {
            return Err(format!("Failed to set CPU affinity: {}", result));
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_cpu_affinity(_cpu: usize) -> Result<(), String> {
    // No-op on non-Linux platforms
    Ok(())
}

/// Run benchmark workload: continuous get() and put() operations
fn run_workload<B>(
    bitmap: Arc<B>,
    duration: Duration,
    cpu: usize,
    ops_counter: Arc<AtomicU64>,
) where
    B: Send + Sync + 'static,
    B: BitmapOps,
{
    thread::spawn(move || {
        // Pin to CPU
        if let Err(e) = set_cpu_affinity(cpu) {
            eprintln!("Warning: {}", e);
        }

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

/// Run benchmark with two tasks on different CPUs
fn benchmark<B>(name: &str, bitmap: Arc<B>, duration: Duration, cpu0: usize, cpu1: usize)
where
    B: Send + Sync + 'static + BitmapOps,
{
    println!("\n=== {} Benchmark ===", name);
    println!("Configuration:");
    println!("  - Duration: {:?}", duration);
    println!("  - Tasks: 2 (pinned to CPU {} and CPU {})", cpu0, cpu1);
    println!("  - Bitmap size: 256 bits");

    let ops_task0 = Arc::new(AtomicU64::new(0));
    let ops_task1 = Arc::new(AtomicU64::new(0));

    // Spawn task on CPU cpu0
    let b0 = Arc::clone(&bitmap);
    let c0 = Arc::clone(&ops_task0);
    run_workload(b0, duration, cpu0, c0);

    // Spawn task on CPU cpu1
    let b1 = Arc::clone(&bitmap);
    let c1 = Arc::clone(&ops_task1);
    run_workload(b1, duration, cpu1, c1);

    // Wait for duration + a bit more for threads to finish
    thread::sleep(duration + Duration::from_millis(100));

    let ops0 = ops_task0.load(Ordering::Relaxed);
    let ops1 = ops_task1.load(Ordering::Relaxed);
    let total_ops = ops0 + ops1;
    let duration_secs = duration.as_secs_f64();

    println!("\nResults:");
    println!("  Task 0 (CPU {}): {} ops, {:.2} ops/sec",
             cpu0, ops0, ops0 as f64 / duration_secs);
    println!("  Task 1 (CPU {}): {} ops, {:.2} ops/sec",
             cpu1, ops1, ops1 as f64 / duration_secs);
    println!("  Total: {} ops, {:.2} ops/sec",
             total_ops, total_ops as f64 / duration_secs);
}

fn main() {
    // Parse command line arguments for CPU cores
    let args: Vec<String> = env::args().collect();

    // Default to CPU 0 and CPU 2 (likely different physical cores)
    // CPU 0 and CPU 1 are often hyperthreads on the same core
    let (cpu0, cpu1) = if args.len() >= 3 {
        let c0 = args[1].parse::<usize>().unwrap_or_else(|_| {
            eprintln!("Error: Invalid CPU ID '{}'", args[1]);
            eprintln!("Usage: {} [cpu0] [cpu1]", args[0]);
            eprintln!("Example: {} 0 2  (use CPU 0 and CPU 2)", args[0]);
            std::process::exit(1);
        });
        let c1 = args[2].parse::<usize>().unwrap_or_else(|_| {
            eprintln!("Error: Invalid CPU ID '{}'", args[2]);
            eprintln!("Usage: {} [cpu0] [cpu1]", args[0]);
            std::process::exit(1);
        });
        (c0, c1)
    } else {
        println!("Note: Using default CPUs 0 and 2 (likely different physical cores)");
        println!("      To specify CPUs: {} <cpu0> <cpu1>", args[0]);
        println!("      Example: {} 0 4", args[0]);
        println!();
        (0, 2)
    };

    if cpu0 == cpu1 {
        eprintln!("Warning: CPU {} and CPU {} are the same!", cpu0, cpu1);
        eprintln!("         This will not test multi-core contention.");
    }

    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║  Sbitmap vs Simple Lockless Bitmap Benchmark Comparison   ║");
    println!("╚═══════════════════════════════════════════════════════════╝");

    let duration = Duration::from_secs(5);
    let depth = 256;

    // Benchmark 1: Sbitmap (cache-line optimized with per-task hints)
    let sbitmap = Arc::new(Sbitmap::new(depth, None, false));
    benchmark("Sbitmap (Optimized)", sbitmap, duration, cpu0, cpu1);

    // Benchmark 2: SimpleBitmap (no cache-line optimization, no hints)
    let simple = Arc::new(SimpleBitmap::new(depth));
    benchmark("SimpleBitmap (Baseline)", simple, duration, cpu0, cpu1);

    println!("\n╔═══════════════════════════════════════════════════════════╗");
    println!("║  Summary                                                  ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!("
CPUs used: {} and {}

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

Note: For best results, use CPUs on different physical cores.
      On most systems, CPU 0 and CPU 1 are hyperthreads (same core).
      Use 'lscpu -e' to view your CPU topology.
", cpu0, cpu1);
}
