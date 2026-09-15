//! Деградация с ростом книги, in-process: N ордеров чанками, нс/ордер на чанк
//! и число покоящихся ордеров — деградирует ли движок с глубиной книги.
use matching_engine_rust::generator::generate_orders;
use matching_engine_rust::orderbook::OrderBook;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000_000);
    let chunk: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let orders = generate_orders(n, 0xDEADBEEFCAFE1234);
    let mut ob = OrderBook::with_capacity(n + 64);
    for (i, c) in orders.chunks(chunk).enumerate() {
        let t = Instant::now();
        for &o in c {
            ob.process_order(o);
        }
        let dt = t.elapsed().as_secs_f64();
        println!(
            "{{\"lang\":\"rust\",\"chunk\":{i},\"orders_done\":{},\"resting\":{},\"ns_per_order\":{:.3}}}",
            (i + 1) * c.len(),
            ob.resting_orders(),
            dt * 1e9 / c.len() as f64
        );
    }
    eprintln!("trades {} volume {}", ob.trades_count, ob.matched_volume);
}
