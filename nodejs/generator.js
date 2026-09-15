'use strict';
// Xorshift64 на BigInt: побитово совпадает с golang/generator.go и rust/src/generator.rs.
// Работает ВНЕ таймера, поэтому скорость BigInt не важна; результат — плоские
// типизированные массивы (SoA), которые движок читает без объектов.

const MASK64 = (1n << 64n) - 1n;
const DEFAULT_SEED = 0xDEADBEEFCAFE1234n;

function generateOrders(n, seed) {
  let v = BigInt.asUintN(64, seed);
  if (v === 0n) v = DEFAULT_SEED;

  const side = new Uint8Array(n);
  const price = new Int32Array(n);
  const qty = new Int32Array(n);

  for (let i = 0; i < n; i++) {
    v ^= (v << 13n) & MASK64;
    v ^= v >> 7n;
    v ^= (v << 17n) & MASK64;
    const r1 = v;
    v ^= (v << 13n) & MASK64;
    v ^= v >> 7n;
    v ^= (v << 17n) & MASK64;
    const r2 = v;
    v ^= (v << 13n) & MASK64;
    v ^= v >> 7n;
    v ^= (v << 17n) & MASK64;
    const r3 = v;

    side[i] = Number(r1 & 1n);
    price[i] = 10000 + Number(r2 % 200n) - 100;
    qty[i] = 1 + Number(r3 % 100n);
  }
  return { n, side, price, qty };
}

module.exports = { generateOrders, DEFAULT_SEED };
