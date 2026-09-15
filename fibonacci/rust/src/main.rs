use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

#[inline(never)]
fn fib_recursive(n: u32) -> u64 {
    if n <= 1 {
        return n as u64;
    }
    fib_recursive(n - 1) + fib_recursive(n - 2)
}

fn run_timed_benchmark(duration: Duration, n: u32) -> (u64, Duration) {
    let mut iterations: u64 = 0;
    let start = Instant::now();

    while start.elapsed() < duration {
        let res = black_box(fib_recursive(black_box(n)));
        black_box(res);
        iterations += 1;
    }

    let actual_elapsed = start.elapsed();
    (iterations, actual_elapsed)
}

fn run_single_target(n: u32) -> (u64, Duration) {
    let start = Instant::now();
    let res = black_box(fib_recursive(black_box(n)));
    let elapsed = start.elapsed();
    (res, elapsed)
}

fn main() {
    let mut duration_secs = 4.0f64;

    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        if let Ok(val) = args[1].parse::<f64>() {
            if val > 0.0 {
                duration_secs = val;
            }
        }
    }

    let duration = Duration::from_secs_f64(duration_secs);

    println!("========================================================");
    println!("            RUST FIBONACCI BENCHMARK                    ");
    println!("========================================================");

    // Part 1: Fixed 4.0-second time window throughput (fib(30))
    println!("[Test 1] 4.0-Second Fixed-Time Throughput (fib(30)):");
    let (iterations, actual_elapsed) = run_timed_benchmark(duration, 30);
    let ops_per_sec = (iterations as f64) / actual_elapsed.as_secs_f64();
    let avg_ms_per_run = (actual_elapsed.as_secs_f64() * 1000.0) / (iterations as f64);

    println!("  Duration:               {:?}", actual_elapsed);
    println!("  Completed Calculations: {}", iterations);
    println!("  Throughput:             {:.2} fib(30)/sec", ops_per_sec);
    println!("  Average Latency:        {:.3} ms per fib(30)", avg_ms_per_run);
    println!("--------------------------------------------------------");

    // Part 2: Heavy recursion (~4 seconds target)
    println!("[Test 2] Single Heavy Calculation (~4.0s target):");
    let (res45, el45) = run_single_target(45);
    println!(
        "  fib(45) = {} in {:?} ({:.2} ms | {:.3} s)",
        res45,
        el45,
        el45.as_secs_f64() * 1000.0,
        el45.as_secs_f64()
    );
    let (res46, el46) = run_single_target(46);
    println!(
        "  fib(46) = {} in {:?} ({:.2} ms | {:.3} s)",
        res46,
        el46,
        el46.as_secs_f64() * 1000.0,
        el46.as_secs_f64()
    );
    let (res47, el47) = run_single_target(47);
    println!(
        "  fib(47) = {} in {:?} ({:.2} ms | {:.3} s)",
        res47,
        el47,
        el47.as_secs_f64() * 1000.0,
        el47.as_secs_f64()
    );
    println!("========================================================\n");
}
