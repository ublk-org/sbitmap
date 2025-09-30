// Basic usage example for sbitmap

use sbitmap::Sbitmap;
use std::sync::Arc;
use std::thread;

fn main() {
    println!("=== Sbitmap Basic Example ===\n");

    // Example 1: Simple allocation
    println!("1. Simple Allocation:");
    let sb = Sbitmap::new(64, None, false);
    println!("   Created bitmap with {} bits", sb.depth());

    let bit = sb.get().expect("Should allocate a bit");
    println!("   Allocated bit: {}", bit);
    println!("   Currently allocated: {} bits", sb.weight());

    sb.put(bit);
    println!("   Freed bit: {}", bit);
    println!("   Currently allocated: {} bits\n", sb.weight());

    // Example 2: Multiple allocations
    println!("2. Multiple Allocations:");
    let sb = Sbitmap::new(16, None, false);
    let mut bits = Vec::new();

    for i in 0..10 {
        if let Some(bit) = sb.get() {
            bits.push(bit);
            println!("   Allocation {}: bit {}", i + 1, bit);
        }
    }

    println!("   Total allocated: {} bits", sb.weight());

    // Free half
    for _ in 0..5 {
        if let Some(bit) = bits.pop() {
            sb.put(bit);
        }
    }
    println!("   After freeing 5: {} bits allocated\n", sb.weight());

    // Example 3: Concurrent allocation
    println!("3. Concurrent Allocation (8 threads):");
    let sb = Arc::new(Sbitmap::new(1024, None, false));
    let mut handles = vec![];

    for thread_id in 0..8 {
        let sb = Arc::clone(&sb);
        handles.push(thread::spawn(move || {
            let mut allocated = Vec::new();

            // Each thread allocates 50 bits
            for _ in 0..50 {
                if let Some(bit) = sb.get() {
                    allocated.push(bit);
                }
            }

            println!(
                "   Thread {} allocated {} bits (first: {}, last: {})",
                thread_id,
                allocated.len(),
                allocated.first().unwrap_or(&0),
                allocated.last().unwrap_or(&0)
            );

            // Free them all
            for bit in allocated {
                sb.put(bit);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    println!("   Final allocated bits: {}", sb.weight());
    println!("   All threads completed successfully\n");

    // Example 4: Exhaustion handling
    println!("4. Bitmap Exhaustion:");
    let sb = Sbitmap::new(8, None, false);
    let mut bits = Vec::new();

    // Allocate all bits
    for _ in 0..8 {
        if let Some(bit) = sb.get() {
            bits.push(bit);
        }
    }

    println!("   Allocated all {} bits", bits.len());

    // Try to allocate one more
    match sb.get() {
        Some(bit) => println!("   Unexpectedly got bit: {}", bit),
        None => println!("   Correctly returned None (bitmap full)"),
    }

    // Free one and try again
    if let Some(bit) = bits.pop() {
        sb.put(bit);
        println!("   Freed bit: {}", bit);
    }

    match sb.get() {
        Some(bit) => println!("   Successfully allocated bit: {}", bit),
        None => println!("   Failed to allocate"),
    }

    println!("\n=== Example Complete ===");
}
