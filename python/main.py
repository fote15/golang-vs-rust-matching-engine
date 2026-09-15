"""Протокол замера автора (golang/main.go) на чистом CPython.

warmup 10k -> генерация 100k -> ITERS раз: свежая книга, таймер только вокруг
прогона ордеров. Формат вывода — строка в строку как у Go/Rust.
"""

import gc
import sys
import time

from generator import DEFAULT_SEED, generate_orders
from orderbook import OrderBook

N = 100_000
WARMUP_N = 10_000

EXPECTED = {
    "trades": 77_576,
    "volume": 1_973_216,
    "bid_levels": 55,
    "ask_levels": 50,
    "resting": 21_620,
    "best_bid": 10_038,
    "best_ask": 10_043,
}


def run_single_benchmark(n, orders):
    sides, prices, qtys, ids = orders
    # Свежая книга на каждой итерации — вне таймера, как у автора.
    ob = OrderBook(n)
    gc.collect()
    gc.disable()
    try:
        start = time.perf_counter_ns()
        ob.process_orders(sides, prices, qtys, ids)
        elapsed_ns = time.perf_counter_ns() - start
    finally:
        gc.enable()
    return elapsed_ns, ob


def parse_args(argv):
    iters = 5
    seed = DEFAULT_SEED
    as_json = False
    i = 1
    while i < len(argv):
        a = argv[i]
        if a == "--iters" and i + 1 < len(argv):
            iters = int(argv[i + 1])
            i += 2
        elif a == "--seed" and i + 1 < len(argv):
            s = argv[i + 1].strip().lower()
            if s.startswith("0x"):
                s = s[2:]
            seed = int(s, 16) & ((1 << 64) - 1)
            if seed == 0:
                seed = DEFAULT_SEED
            i += 2
        elif a == "--json":
            as_json = True
            i += 1
        else:
            sys.stderr.write(f"unknown argument: {a}\n")
            sys.exit(2)
    return iters, seed, as_json


def main():
    iters, seed, as_json = parse_args(sys.argv)
    out = sys.stdout

    if not as_json:
        out.write("========================================================\n")
        out.write("            PYTHON MATCHING ENGINE BENCHMARK            \n")
        out.write("========================================================\n")
        out.write(f"Workload: {N} orders per run | {iters} iterations\n")
        out.write(f"PRNG: Deterministic Xorshift64 (seed: 0x{seed:X})\n")
        out.write("--------------------------------------------------------\n")

    warmup = generate_orders(WARMUP_N, seed)
    run_single_benchmark(WARMUP_N, warmup)

    orders = generate_orders(N, seed)

    total_ns = 0
    min_ns = None
    times = []
    ob = None
    for i in range(1, iters + 1):
        elapsed_ns, ob = run_single_benchmark(N, orders)
        times.append(elapsed_ns)
        total_ns += elapsed_ns
        if min_ns is None or elapsed_ns < min_ns:
            min_ns = elapsed_ns
        if not as_json:
            ms = elapsed_ns / 1e6
            mops = N / elapsed_ns * 1e3  # (N / (ns * 1e-9)) / 1e6
            out.write(
                f"  Iteration {i}: {ms:8.3f} ms | {mops:10.2f} M ops/s | {elapsed_ns / N:6.2f} ns/order\n"
            )

    trades = ob.trades_count
    volume = ob.matched_volume
    bid_levels = ob.bid_levels()
    ask_levels = ob.ask_levels()
    resting = ob.resting_orders()
    best_bid = ob.best_bid() or 0
    best_ask = ob.best_ask() or 0
    total_levels = bid_levels + ask_levels

    avg_ns = total_ns / iters
    best_ms = min_ns / 1e6
    avg_ms = avg_ns / 1e6
    best_mops = N / min_ns * 1e3
    avg_mops = N / avg_ns * 1e3

    if as_json:
        st = sorted(times)
        p = lambda q: st[min(len(st) - 1, int(q * (len(st) - 1) + 0.5))] / 1e6
        med = st[len(st) // 2] / 1e6
        out.write(
            '{"lang":"python","orders":%d,"iters":%d,"books_per_iter":1,'
            '"min_ms":%.6f,"median_ms":%.6f,"p95_ms":%.6f,"p99_ms":%.6f,"max_ms":%.6f,'
            '"median_mops":%.4f,"median_ns_per_order":%.4f,'
            '"trades":%d,"volume":%d,"bid_levels":%d,"ask_levels":%d,"resting":%d}\n'
            % (N, iters, st[0] / 1e6, med, p(0.95), p(0.99), st[-1] / 1e6,
               N / med / 1e3, med * 1e6 / N,
               trades, volume, bid_levels, ask_levels, resting)
        )
    else:
        out.write("--------------------------------------------------------\n")
        out.write("SUMMARY (PYTHON):\n")
        out.write(f"  Best Time:         {best_ms:.3f} ms ({best_mops:.2f} M ops/s)\n")
        out.write(f"  Average Time:      {avg_ms:.3f} ms ({avg_mops:.2f} M ops/s)\n")
        out.write(f"  Average Latency:   {avg_ns / N:.2f} ns/order\n")
        out.write(f"  Trades Executed:   {trades}\n")
        out.write(f"  Volume Matched:    {volume}\n")
        out.write(f"  Resting in Book:   {resting}\n")
        out.write(f"  Active Levels:     {total_levels} (Best Bid: {best_bid}, Best Ask: {best_ask})\n")
        out.write("========================================================\n\n")
    out.flush()

    if seed == DEFAULT_SEED:
        got = {
            "trades": trades,
            "volume": volume,
            "bid_levels": bid_levels,
            "ask_levels": ask_levels,
            "resting": resting,
            "best_bid": best_bid,
            "best_ask": best_ask,
        }
        bad = [k for k in EXPECTED if got[k] != EXPECTED[k]]
        if bad:
            for k in bad:
                sys.stderr.write(f"INVARIANT VIOLATED: {k}: got {got[k]}, expected {EXPECTED[k]}\n")
            sys.exit(1)


if __name__ == "__main__":
    main()
