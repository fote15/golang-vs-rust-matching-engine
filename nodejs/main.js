'use strict';
// Протокол замера — ровно как у автора (golang/main.go, rust/src/bin/author_protocol.rs):
// прогрев на 10 000 ордеров, затем ITERS итераций на свежей книге; в таймере — только
// цикл обработки ордеров.

const { generateOrders, DEFAULT_SEED } = require('./generator');
const { OrderBook, processAll } = require('./orderbook');

const N = 100000;
const WARMUP = 10000;

function parseArgs(argv) {
  let iters = 5;
  let seed = DEFAULT_SEED;
  let json = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--iters' && i + 1 < argv.length) {
      const v = parseInt(argv[++i], 10);
      if (Number.isFinite(v) && v > 0) iters = v;
    } else if (a.startsWith('--iters=')) {
      const v = parseInt(a.slice(8), 10);
      if (Number.isFinite(v) && v > 0) iters = v;
    } else if (a === '--seed' && i + 1 < argv.length) {
      seed = parseSeed(argv[++i]);
    } else if (a.startsWith('--seed=')) {
      seed = parseSeed(a.slice(7));
    } else if (a === '--json') {
      json = true;
    }
  }
  return { iters, seed, json };
}

function parseSeed(s) {
  const hex = s.replace(/^0x/i, '');
  if (!/^[0-9a-fA-F]{1,16}$/.test(hex)) {
    process.stderr.write(`bad --seed: ${s}\n`);
    process.exit(2);
  }
  const v = BigInt('0x' + hex);
  return v === 0n ? DEFAULT_SEED : v;
}

function runSingleBenchmark(orders) {
  // Как у автора: свежая книга на каждой итерации, вне таймера.
  const ob = new OrderBook(orders.n);

  const start = process.hrtime.bigint();
  processAll(ob, orders);
  const end = process.hrtime.bigint();

  const ns = Number(end - start);
  return {
    ns,
    trades: ob.tradesCount,
    volume: ob.matchedVolume,
    resting: ob.resting,
    bidLevels: ob.bidLevels(),
    askLevels: ob.askLevels(),
    bestBid: ob.bestBid(),
    bestAsk: ob.bestAsk(),
    rejected: ob.rejected,
  };
}

function fmt(x, digits, width) {
  const s = x.toFixed(digits);
  return width ? s.padStart(width) : s;
}

function main() {
  const { iters, seed, json } = parseArgs(process.argv.slice(2));
  const seedHex = seed.toString(16).toUpperCase();
  const out = [];

  if (!json) {
    out.push('========================================================');
    out.push('           NODEJS MATCHING ENGINE BENCHMARK             ');
    out.push('========================================================');
    out.push(`Workload: ${N} orders per run | ${iters} iterations`);
    out.push(`PRNG: Deterministic Xorshift64 (seed: 0x${seedHex})`);
    out.push('--------------------------------------------------------');
    process.stdout.write(out.join('\n') + '\n');
    out.length = 0;
  }

  // Warmup — как у автора (заодно прогревает JIT).
  const warmupOrders = generateOrders(WARMUP, seed);
  runSingleBenchmark(warmupOrders);

  const orders = generateOrders(N, seed);

  let totalNs = 0;
  let minNs = Infinity;
  let last = null;
  const times = [];

  for (let i = 1; i <= iters; i++) {
    const r = runSingleBenchmark(orders);
    totalNs += r.ns;
    if (r.ns < minNs) minNs = r.ns;
    times.push(r.ns);
    last = r;
    if (!json) {
      const ms = r.ns / 1e6;
      const mops = N / r.ns * 1e3; // (N / (ns/1e9)) / 1e6
      process.stdout.write(
        `  Iteration ${i}: ${fmt(ms, 3, 8)} ms | ${fmt(mops, 2, 10)} M ops/s | ${fmt(r.ns / N, 2, 6)} ns/order\n`,
      );
    }
  }

  const avgNs = totalNs / iters;
  const totalLevels = last.bidLevels + last.askLevels;

  if (json) {
    times.sort((a, b) => a - b);
    const q = (p) => times[Math.min(times.length - 1, Math.floor(p * times.length))] / 1e6;
    const med = times[times.length >> 1] / 1e6;
    process.stdout.write(
      JSON.stringify({
        lang: 'nodejs',
        orders: N,
        iters,
        books_per_iter: 1,
        min_ms: times[0] / 1e6,
        median_ms: med,
        p95_ms: q(0.95),
        p99_ms: q(0.99),
        max_ms: times[times.length - 1] / 1e6,
        median_mops: N / (med * 1e3),
        median_ns_per_order: (med * 1e6) / N,
        trades: last.trades,
        volume: last.volume,
        bid_levels: last.bidLevels,
        ask_levels: last.askLevels,
        resting: last.resting,
        best_bid: last.bestBid,
        best_ask: last.bestAsk,
      }) + '\n',
    );
  } else {
    out.push('--------------------------------------------------------');
    out.push('SUMMARY (NODEJS):');
    out.push(`  Best Time:         ${fmt(minNs / 1e6, 3)} ms (${fmt(N / minNs * 1e3, 2)} M ops/s)`);
    out.push(`  Average Time:      ${fmt(avgNs / 1e6, 3)} ms (${fmt(N / avgNs * 1e3, 2)} M ops/s)`);
    out.push(`  Average Latency:   ${fmt(avgNs / N, 2)} ns/order`);
    out.push(`  Trades Executed:   ${last.trades}`);
    out.push(`  Volume Matched:    ${last.volume}`);
    out.push(`  Resting in Book:   ${last.resting}`);
    out.push(`  Active Levels:     ${totalLevels} (Best Bid: ${last.bestBid}, Best Ask: ${last.bestAsk})`);
    out.push('========================================================');
    out.push('');
    process.stdout.write(out.join('\n') + '\n');
  }

  // Инварианты корректности на дефолтном seed — расхождение = ненулевой код выхода.
  if (seed === DEFAULT_SEED) {
    const expect = {
      trades: 77576,
      volume: 1973216,
      bidLevels: 55,
      askLevels: 50,
      resting: 21620,
      bestBid: 10038,
      bestAsk: 10043,
    };
    let ok = true;
    for (const k of Object.keys(expect)) {
      if (last[k] !== expect[k]) {
        process.stderr.write(`INVARIANT VIOLATED: ${k} = ${last[k]}, expected ${expect[k]}\n`);
        ok = false;
      }
    }
    if (!ok) process.exit(1);
  }
  if (last.rejected !== 0) {
    process.stderr.write(`INVARIANT VIOLATED: rejected = ${last.rejected}, expected 0\n`);
    process.exit(1);
  }
}

main();
