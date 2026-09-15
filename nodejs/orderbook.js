'use strict';
// Плоская книга лимитных ордеров — порт структуры rust/src/orderbook.rs на
// типизированные массивы V8.
//
// * Уровни: одна Int32Array на обе стороны, прямая индексация по тику
//   (полоса BAND = 8192 тиков от цены первого ордера). Ячейка уровня —
//   пара (head, tail) индексов арены.
// * Битовая карта занятости в три яруса из 32-битных слов (V8 умеет только
//   Math.clz32, поэтому слово — 32 бита): l1 = 256 слов, l2 = 8 слов,
//   l3 = 1 слово на сторону. Лучшая цена — trailing zeros, кэшируется в best[].
// * Сторона bid живёт в инвертированных координатах (key = tick ^ MASK):
//   «лучшая цена» обеих сторон — всегда минимум, ветка по стороне
//   вырождается в индекс и XOR с маской.
// * Арена ордеров: Int32Array пар (qty, next), интрузивный free-list по
//   индексам. Ни одного объекта на ордер, ни одной аллокации в горячем цикле.

const BAND_BITS = 13;
const BAND = 1 << BAND_BITS;
const HALF_BAND = BAND >> 1;
const MASK = BAND - 1;
const L1W = BAND >> 5; // 256 слов по 32 бита
const L2W = L1W >> 5; // 8 слов
const NIL = 0x7fffffff; // > любого валидного ключа: пустая сторона выходит из цикла тем же сравнением

class OrderBook {
  constructor(capacity) {
    const cap = Math.max(capacity | 0, 64);
    // Уровни обеих сторон: [half][tick] -> (head, tail). ASK = 0 (прямые тики), BID = 1 (инвертированные).
    this.lv = new Int32Array(2 * BAND * 2).fill(NIL);
    this.l1 = new Int32Array(2 * L1W);
    this.l2 = new Int32Array(2 * L2W);
    this.l3 = new Int32Array(2);
    this.best = new Int32Array(2).fill(NIL);
    // Арена: slot[2*i] = qty, slot[2*i+1] = next.
    this.slot = new Int32Array(cap * 2);
    for (let i = 0; i < cap; i++) this.slot[2 * i + 1] = i + 1;
    this.slot[2 * cap - 1] = NIL;
    // Холодные данные: идентификаторы, в матчинге не читаются.
    this.ids = new Float64Array(cap);
    this.cap = cap;
    this.freeHead = 0;
    this.base = 0;
    this.pinned = false;
    this.resting = 0;
    this.tradesCount = 0;
    this.matchedVolume = 0;
    this.rejected = 0;
  }

  // Холодный путь: удвоить арену. Индексы, а не указатели, поэтому ссылки не инвалидируются.
  grow() {
    const old = this.cap;
    const cap = old * 2;
    const slot = new Int32Array(cap * 2);
    slot.set(this.slot);
    for (let i = old; i < cap; i++) slot[2 * i + 1] = i + 1;
    slot[2 * cap - 1] = NIL;
    const ids = new Float64Array(cap);
    ids.set(this.ids);
    this.slot = slot;
    this.ids = ids;
    this.cap = cap;
    this.freeHead = old;
  }

  bestBid() {
    const b = this.best[1];
    return b === NIL ? 0 : this.base + (b ^ MASK);
  }

  bestAsk() {
    const a = this.best[0];
    return a === NIL ? 0 : this.base + a;
  }

  levelCount(half) {
    const l1 = this.l1;
    let n = 0;
    for (let i = half * L1W, e = i + L1W; i < e; i++) {
      let w = l1[i];
      while (w !== 0) {
        w &= w - 1;
        n++;
      }
    }
    return n;
  }

  bidLevels() {
    return this.levelCount(1);
  }

  askLevels() {
    return this.levelCount(0);
  }
}

// Горячий путь: прогнать все n ордеров через книгу. Всё состояние — в локальных
// переменных и типизированных массивах; запись обратно в объект — один раз в конце.
function processAll(book, orders) {
  const n = orders.n;
  const sideA = orders.side;
  const priceA = orders.price;
  const qtyA = orders.qty;

  const lv = book.lv;
  const l1 = book.l1;
  const l2 = book.l2;
  const l3 = book.l3;
  const best = book.best;
  let slot = book.slot;
  let ids = book.ids;

  let free = book.freeHead;
  let base = book.base;
  let pinned = book.pinned;
  let resting = book.resting;
  let trades = book.tradesCount;
  let volume = book.matchedVolume;
  let rejected = book.rejected;

  for (let i = 0; i < n; i++) {
    const s = sideA[i];
    const p = priceA[i];
    let q = qtyA[i];

    if (q <= 0) {
      rejected++;
      continue;
    }
    let t = p - base;
    if (t < 0 || t >= BAND || !pinned) {
      // Холодный путь: привязать/сдвинуть полосу. На непустой книге цена вне полосы отвергается.
      if (pinned && (resting !== 0 || l3[0] !== 0 || l3[1] !== 0)) {
        rejected++;
        continue;
      }
      base = p - HALF_BAND;
      if (base < 0) base = 0;
      pinned = true;
      t = p - base;
      if (t < 0 || t >= BAND) {
        rejected++;
        continue;
      }
    }

    // Buy (0) ест ASK (половина 0), ложится в BID (1); Sell — наоборот.
    const consume = s;
    const restAt = s ^ 1;
    const ckey = t ^ (-consume & MASK);
    const rkey = t ^ (-restAt & MASK);
    const cbase = consume << BAND_BITS;

    let b = best[consume];
    while (b <= ckey) {
      // Забрать ликвидность с уровня b (FIFO от головы).
      const li = (cbase | b) << 1;
      let h = lv[li];
      while (h !== NIL) {
        const si = h << 1;
        const mq = slot[si];
        const m = mq < q ? mq : q;
        trades++;
        volume += m;
        q -= m;
        if (m === mq) {
          // maker исполнен целиком: слот в free-list, голова — следующий.
          const next = slot[si + 1];
          slot[si + 1] = free;
          free = h;
          h = next;
          resting--;
          if (q === 0) break;
        } else {
          slot[si] = mq - m;
          break; // q == 0
        }
      }
      lv[li] = h;
      if (h !== NIL) break; // taker исчерпан, уровень жив
      lv[li + 1] = NIL;

      // unmark(b): быстрый путь — следующий лучший в том же слове l1.
      const i1 = b >>> 5;
      const w1i = (consume << 8) | i1;
      const v1 = l1[w1i] & ~(1 << (b & 31));
      l1[w1i] = v1;
      if (v1 !== 0) {
        b = (i1 << 5) | (31 - Math.clz32(v1 & -v1));
      } else {
        const i2 = b >>> 10;
        const w2i = (consume << 3) | i2;
        const v2 = l2[w2i] & ~(1 << (i1 & 31));
        l2[w2i] = v2;
        let v3 = l3[consume];
        if (v2 === 0) {
          v3 &= ~(1 << i2);
          l3[consume] = v3;
        }
        if (v3 === 0) {
          b = NIL;
        } else {
          const j2 = 31 - Math.clz32(v3 & -v3);
          const x2 = l2[(consume << 3) | j2];
          const j1 = (j2 << 5) | (31 - Math.clz32(x2 & -x2));
          const x1 = l1[(consume << 8) | j1];
          b = (j1 << 5) | (31 - Math.clz32(x1 & -x1));
        }
      }
      best[consume] = b;
      if (q === 0) break;
    }

    if (q > 0) {
      // alloc
      if (free === NIL) {
        book.freeHead = free;
        book.grow();
        slot = book.slot;
        ids = book.ids;
        free = book.freeHead;
      }
      const node = free;
      const si = node << 1;
      free = slot[si + 1];
      slot[si] = q;
      slot[si + 1] = NIL;
      ids[node] = i + 1; // id = порядковый номер ордера (как у генератора)
      resting++;

      // rest: в хвост очереди уровня rkey половины restAt
      const li = ((restAt << BAND_BITS) | rkey) << 1;
      const tl = lv[li + 1];
      lv[li + 1] = node;
      if (tl === NIL) {
        lv[li] = node;
        // mark(rkey)
        l1[(restAt << 8) | (rkey >>> 5)] |= 1 << (rkey & 31);
        l2[(restAt << 3) | (rkey >>> 10)] |= 1 << ((rkey >>> 5) & 31);
        l3[restAt] |= 1 << (rkey >>> 10);
        if (rkey < best[restAt]) best[restAt] = rkey;
      } else {
        slot[(tl << 1) + 1] = node;
      }
    }
  }

  book.freeHead = free;
  book.base = base;
  book.pinned = pinned;
  book.resting = resting;
  book.tradesCount = trades;
  book.matchedVolume = volume;
  book.rejected = rejected;
}

module.exports = { OrderBook, processAll, BAND, NIL };
