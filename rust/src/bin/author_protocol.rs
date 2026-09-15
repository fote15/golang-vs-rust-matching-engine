//! Текущий движок под ОРИГИНАЛЬНЫМ протоколом замера автора бенчмарка
//! (коммит b1e2dbd, `rust/src/main.rs`): прогрев на 10k, затем 5 итераций,
//! на каждой — свежая книга, публикуются Best и Average по 5 итерациям.
//!
//! Нужен для прямого ответа на вопрос «а если мерить ровно так, как мерил
//! автор?». Протокол воспроизведён один в один; отличается только API
//! движка (`bid_levels()` вместо `bids.len()`), потому что структура книги
//! другая. Число итераций можно поднять через `--iters`, чтобы увидеть
//! распределение, но по умолчанию — ровно 5, как у автора.

use matching_engine_rust::generator::generate_orders;
use matching_engine_rust::orderbook::OrderBook;
use matching_engine_rust::types::Order;
use std::time::{Duration, Instant};

struct BenchResult {
    elapsed: Duration,
    throughput: f64,
    avg_latency_ns: f64,
}

#[allow(clippy::type_complexity)]
fn run_single_benchmark(
    n: usize,
    orders: &[Order],
) -> (BenchResult, u64, u64, usize, usize, usize, u64, u64) {
    // Как у автора: книга создаётся заново на каждой итерации, вне таймера.
    let mut ob = OrderBook::with_capacity(n);

    let start = Instant::now();
    for &order in orders {
        ob.process_order(order);
    }
    let elapsed = start.elapsed();

    let total_orders = n as f64;
    let seconds = elapsed.as_secs_f64();
    let ops = total_orders / seconds;
    let avg_latency_ns = (elapsed.as_nanos() as f64) / total_orders;

    let best_bid = ob.best_bid().unwrap_or(0);
    let best_ask = ob.best_ask().unwrap_or(0);
    let total_levels = ob.bid_levels() + ob.ask_levels();
    let resting = ob.resting_orders();

    (
        BenchResult {
            elapsed,
            throughput: ops,
            avg_latency_ns,
        },
        ob.trades_count,
        ob.matched_volume,
        resting,
        0,
        total_levels,
        best_bid,
        best_ask,
    )
}

fn main() {
    const SEED: u64 = 0xDEADBEEFCAFE1234;
    const N: usize = 100_000;

    let args: Vec<String> = std::env::args().collect();
    let iterations: usize = args
        .iter()
        .position(|a| a == "--iters")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    println!("========================================================");
    println!("            RUST MATCHING ENGINE BENCHMARK              ");
    println!("========================================================");
    println!("Workload: {N} orders per run | {iterations} iterations");
    println!("PRNG: Deterministic Xorshift64 (seed: 0x{SEED:X})");
    println!("Protocol: author's original (b1e2dbd), fresh book per iteration");
    println!("--------------------------------------------------------");

    // Warmup run — как у автора.
    let warmup_orders = generate_orders(10_000, SEED);
    run_single_benchmark(10_000, &warmup_orders);

    let orders = generate_orders(N, SEED);

    let mut total_elapsed = Duration::ZERO;
    let mut min_elapsed = Duration::MAX;

    let mut trades = 0;
    let mut volume = 0;
    let mut resting = 0;
    let mut total_levels = 0;
    let mut best_bid = 0;
    let mut best_ask = 0;

    for i in 1..=iterations {
        let (res, t, v, r, _, lvls, bb, ba) = run_single_benchmark(N, &orders);
        total_elapsed += res.elapsed;
        if res.elapsed < min_elapsed {
            min_elapsed = res.elapsed;
        }
        trades = t;
        volume = v;
        resting = r;
        total_levels = lvls;
        best_bid = bb;
        best_ask = ba;

        println!(
            "  Iteration {}: {:8.3} ms | {:10.2} M ops/s | {:6.2} ns/order",
            i,
            res.elapsed.as_secs_f64() * 1000.0,
            res.throughput / 1_000_000.0,
            res.avg_latency_ns
        );
    }

    let avg_elapsed = total_elapsed / (iterations as u32);
    let avg_ops = (N as f64) / avg_elapsed.as_secs_f64();
    let avg_latency = (avg_elapsed.as_nanos() as f64) / (N as f64);
    let best_ops = (N as f64) / min_elapsed.as_secs_f64();

    println!("--------------------------------------------------------");
    println!("SUMMARY (RUST):");
    println!(
        "  Best Time:         {:.3} ms ({:.2} M ops/s)",
        min_elapsed.as_secs_f64() * 1000.0,
        best_ops / 1_000_000.0
    );
    println!(
        "  Average Time:      {:.3} ms ({:.2} M ops/s)",
        avg_elapsed.as_secs_f64() * 1000.0,
        avg_ops / 1_000_000.0
    );
    println!("  Average Latency:   {avg_latency:.2} ns/order");
    println!("  Trades Executed:   {trades}");
    println!("  Volume Matched:    {volume}");
    println!("  Resting in Book:   {resting}");
    println!("  Active Levels:     {total_levels} (Best Bid: {best_bid}, Best Ask: {best_ask})");
    println!("========================================================\n");

    assert_eq!(trades, 77_576, "инвариант сделок нарушен");
    assert_eq!(volume, 1_973_216, "инвариант объёма нарушен");
    assert_eq!(total_levels, 105, "инвариант числа уровней нарушен");
    assert_eq!(resting, 21_620, "инвариант покоящихся ордеров нарушен");
}
