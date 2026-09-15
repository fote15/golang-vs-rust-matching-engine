//! Бенчмарк-обвязка Rust-движка: прогрев, много итераций, публикуются
//! min / median / p95 / p99, а не среднее по пяти запускам.
//! Ровно протокол автора (5 итераций, среднее) — в `src/bin/author_protocol.rs`.
//!
//! Параметры:
//!   --iters N    число замеряемых итераций (по умолчанию 51)
//!   --orders N   число ордеров в потоке (по умолчанию 100000)
//!   --seed HEX   seed генератора (по умолчанию DEADBEEFCAFE1234)
//!   --json       одна строка JSON в конце (для скриптов и графиков)
//!   --trace-hash после КАЖДОГО ордера подмешать в FNV-1a хеш наблюдаемое
//!                состояние книги (сделки, объём, лучшие цены, число уровней,
//!                объём лучших уровней, id ордера в голове очереди — FIFO)
//!                и напечатать итоговый хеш. Совпадение с Go означает, что
//!                обе реализации проходят через одинаковые состояния на каждом шаге.
//!   --pin        QoS user-interactive (держать поток на P-ядре); по умолчанию
//!                ВЫКЛЮЧЕНО, чтобы условия были одинаковыми с Go
//!
//! Для малых N (< 20000) одна итерация — это K свежих книг подряд, по N
//! ордеров в каждую: иначе замер тонет в разрешении часов (41.67 нс на
//! Apple Silicon). K = 20000/N, но не больше 250. Внутри таймера — только
//! вызовы движка, книги создаются заранее. Ровно так же считает Go-обвязка.
//!
//! Бинарник использует библиотечный крейт, а не пересобирает модули заново:
//! иначе рядом с lib компилируется вторая копия движка.

use matching_engine_rust::generator::generate_orders;
use matching_engine_rust::orderbook::OrderBook;
use matching_engine_rust::types::{Order, Side};
use std::time::Instant;

/// Просим планировщик macOS держать поток на performance-ядрах.
#[cfg(target_os = "macos")]
fn pin_to_pcore() {
    // SAFETY: pthread_set_qos_class_self_np — публичный API Darwin,
    // действует на текущий поток и не трогает память процесса.
    unsafe {
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
#[cfg(not(target_os = "macos"))]
fn pin_to_pcore() {}

struct Stats {
    min: f64,
    median: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn summarize(mut s: Vec<f64>) -> Stats {
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    Stats {
        min: s[0],
        median: s[n / 2],
        p95: s[(n * 95) / 100],
        p99: s[(n * 99) / 100],
        max: s[n - 1],
    }
}

fn arg_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// FNV-1a по наблюдаемому состоянию книги после каждого ордера — зеркало
/// `traceHash` в Go-обвязке, порядок и ширина полей совпадают побайтово.
fn trace_hash(orders: &[Order], n: usize) -> u64 {
    let mut ob = OrderBook::with_capacity(n);
    let mut h: u64 = 14695981039346656037;
    let mut mix = |v: u64| {
        for i in 0..8 {
            h ^= (v >> (8 * i)) & 0xff;
            h = h.wrapping_mul(1099511628211);
        }
    };
    for &o in orders {
        ob.process_order(o);
        let (bb, ba) = (ob.best_bid(), ob.best_ask());
        mix(ob.trades_count);
        mix(ob.matched_volume);
        mix(bb.unwrap_or(0));
        mix(ba.unwrap_or(0));
        mix(((ob.bid_levels() as u64) << 32) | ob.ask_levels() as u64);
        // объём лучших уровней и id ордера в голове очереди: проверка price-time priority
        mix(bb.map_or(0, |p| ob.level_qty(Side::Buy, p)));
        mix(ba.map_or(0, |p| ob.level_qty(Side::Sell, p)));
        mix(bb
            .and_then(|p| ob.front_order_id(Side::Buy, p))
            .unwrap_or(0));
        mix(ba
            .and_then(|p| ob.front_order_id(Side::Sell, p))
            .unwrap_or(0));
    }
    h
}

fn books_per_iter(n: usize) -> usize {
    if n >= 20_000 {
        1
    } else {
        (20_000 / n).clamp(1, 250)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--pin") {
        pin_to_pcore();
    }
    let iterations: usize = arg_val(&args, "--iters")
        .and_then(|v| v.parse().ok())
        .unwrap_or(51);
    let n: usize = arg_val(&args, "--orders")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
        .max(1);
    let seed: u64 = arg_val(&args, "--seed")
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0xDEADBEEFCAFE1234);
    let as_json = args.iter().any(|a| a == "--json" || a == "--trace-hash");
    let k = books_per_iter(n);

    if !as_json {
        println!("========================================================");
        println!("            RUST MATCHING ENGINE BENCHMARK              ");
        println!("========================================================");
        println!("Workload: {n} orders per run | {iterations} iterations | {k} books/iter");
        println!("PRNG: Deterministic Xorshift64 (seed: 0x{seed:X})");
        println!("Engine: flat band book + hierarchical bitmap + branchless side");
        println!("--------------------------------------------------------");
    }

    let orders: Vec<Order> = generate_orders(n, seed);

    if args.iter().any(|a| a == "--trace-hash") {
        println!(
            "{{\"lang\":\"rust\",\"orders\":{n},\"seed\":\"{seed:016X}\",\"trace_hash\":\"{:016x}\"}}",
            trace_hash(&orders, n)
        );
        return;
    }

    let run = |books: &mut [OrderBook]| {
        for b in books.iter_mut() {
            for &o in &orders {
                b.process_order(o);
            }
        }
    };
    let new_books = || -> Vec<OrderBook> { (0..k).map(|_| OrderBook::with_capacity(n)).collect() };

    // Прогрев: страницы, кэш инструкций, размещение потока на P-ядре.
    for _ in 0..5 {
        let mut books = new_books();
        run(&mut books);
        std::hint::black_box(&books);
    }

    let mut samples = Vec::with_capacity(iterations);
    let mut last: Option<OrderBook> = None;
    for i in 1..=iterations {
        let mut books = new_books();
        let start = Instant::now();
        run(&mut books);
        let elapsed = start.elapsed().as_secs_f64() / k as f64;
        std::hint::black_box(&books);
        samples.push(elapsed);
        last = books.pop();
        if !as_json {
            if i <= 5 || i == iterations {
                println!(
                    "  Iteration {i:>3}: {:10.4} ms | {:10.2} M ops/s | {:7.2} ns/order",
                    elapsed * 1e3,
                    n as f64 / elapsed / 1e6,
                    elapsed * 1e9 / n as f64
                );
            } else if i == 6 {
                println!("  ...");
            }
        }
    }

    let book = last.expect("iterations >= 1");
    let st = summarize(samples);
    let trades = book.trades_count;
    let volume = book.matched_volume;
    let (bl, al) = (book.bid_levels(), book.ask_levels());
    let resting = book.resting_orders();

    if as_json {
        println!(
            "{{\"lang\":\"rust\",\"orders\":{n},\"iters\":{iterations},\"books_per_iter\":{k},\"min_ms\":{:.6},\"median_ms\":{:.6},\"p95_ms\":{:.6},\"p99_ms\":{:.6},\"max_ms\":{:.6},\"median_mops\":{:.4},\"median_ns_per_order\":{:.4},\"trades\":{trades},\"volume\":{volume},\"bid_levels\":{bl},\"ask_levels\":{al},\"resting\":{resting}}}",
            st.min * 1e3,
            st.median * 1e3,
            st.p95 * 1e3,
            st.p99 * 1e3,
            st.max * 1e3,
            n as f64 / st.median / 1e6,
            st.median * 1e9 / n as f64
        );
    } else {
        println!("--------------------------------------------------------");
        println!("SUMMARY (RUST):");
        println!(
            "  Best Time:         {:.4} ms ({:.2} M ops/s)",
            st.min * 1e3,
            n as f64 / st.min / 1e6
        );
        println!(
            "  Median Time:       {:.4} ms ({:.2} M ops/s)",
            st.median * 1e3,
            n as f64 / st.median / 1e6
        );
        println!(
            "  p95 / p99 / max:   {:.4} ms / {:.4} ms / {:.4} ms",
            st.p95 * 1e3,
            st.p99 * 1e3,
            st.max * 1e3
        );
        println!(
            "  Median Latency:    {:.2} ns/order",
            st.median * 1e9 / n as f64
        );
        println!("  Trades Executed:   {trades}");
        println!("  Volume Matched:    {volume}");
        println!("  Resting in Book:   {resting} (Bid levels: {bl}, Ask levels: {al})");
        println!(
            "  Best Bid / Ask:    {} / {}",
            book.best_bid()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            book.best_ask()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!("========================================================\n");
    }

    if n == 100_000 && seed == 0xDEADBEEFCAFE1234 {
        assert_eq!(trades, 77_576, "инвариант сделок нарушен");
        assert_eq!(volume, 1_973_216, "инвариант объёма нарушен");
        assert_eq!((bl, al), (55, 50), "инвариант числа уровней нарушен");
        assert_eq!(resting, 21_620, "инвариант покоящихся ордеров нарушен");
    }
}
