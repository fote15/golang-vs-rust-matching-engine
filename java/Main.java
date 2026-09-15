// JAVA MATCHING ENGINE BENCHMARK
//
// Порт движка rust/src/orderbook.rs на Java без единого объекта на ордер:
//   * плоская книга: уровни адресуются прямо по тику (полоса BAND = 8192 тиков,
//     база привязывается к цене первого ордера), SoA-массивы qty/head/tail;
//   * трёхуровневая битовая карта занятости (l1/l2/l3), лучшая цена — trailing zeros,
//     кэш `best` на каждую сторону;
//   * сторона bid хранится в инвертированных координатах (t ^ MASK), поэтому
//     «лучшая цена» обеих сторон — всегда минимум, и горячий путь не ветвится по стороне;
//   * арена ордеров на int-индексах с интрузивным free-list, id — в холодном массиве;
//   * в таймере — только цикл processOrder; никаких аллокаций: вся память
//     выделяется при создании книги (вне таймера).
//
// Сборка/запуск: ./run.sh [--iters N] [--seed HEX] [--json]

public final class Main {

    // ------------------------------------------------------------------ книга

    static final class OrderBook {
        static final int BAND_BITS = 13;
        static final int BAND = 1 << BAND_BITS;          // 8192 тиков
        static final int MASK = BAND - 1;
        static final int L1_BITS = BAND_BITS - 6;         // 7  -> 128 слов l1 на сторону
        static final int L1W = 1 << L1_BITS;
        static final int L2W = (L1W + 63) >>> 6;          // 2 слова l2 на сторону
        static final int NIL = Integer.MAX_VALUE;         // > любого валидного ключа
        static final int ASK = 0;                         // прямые тики
        static final int BID = 1;                         // инвертированные тики

        // Уровни: индекс = (half << BAND_BITS) | tick
        final long[] lvQty = new long[2 * BAND];
        final int[] lvHead = new int[2 * BAND];
        final int[] lvTail = new int[2 * BAND];
        // Битовая карта занятости
        final long[] l1 = new long[2 * L1W];
        final long[] l2 = new long[2 * L2W];
        final long[] l3 = new long[2];
        final int[] best = new int[2];

        // Арена ордеров (индексы вместо указателей)
        long[] slotQty;
        int[] slotNext;
        long[] ids;     // холодный массив: в матчинге не читается
        int freeHead;

        long base;
        boolean basePinned;
        int resting;
        long tradesCount;
        long matchedVolume;
        long rejected;

        OrderBook(int capacity) {
            int cap = Math.max(capacity, 64);
            slotQty = new long[cap];
            slotNext = new int[cap];
            ids = new long[cap];
            for (int i = 0; i < cap; i++) slotNext[i] = i + 1;
            slotNext[cap - 1] = NIL;
            freeHead = 0;
            java.util.Arrays.fill(lvHead, NIL);
            java.util.Arrays.fill(lvTail, NIL);
            best[0] = NIL;
            best[1] = NIL;
            base = 0;
            basePinned = false;
        }

        // Холодный путь: привязка полосы к рынку (только на пустой книге).
        private boolean rebase(long price) {
            if (basePinned && (resting != 0 || l3[0] != 0 || l3[1] != 0)) return false;
            base = price >= (BAND >>> 1) ? price - (BAND >>> 1) : 0;
            basePinned = true;
            return true;
        }

        // Горячий путь: один ордер.
        void processOrder(long id, long price, long qty, int side) {
            if (qty == 0) { rejected++; return; }
            long t = price - base;
            if (t < 0 || t >= BAND) {
                if (!rebase(price)) { rejected++; return; }
                t = price - base;
                if (t < 0 || t >= BAND) { rejected++; return; }
            }
            final int tick = (int) t;
            final int consume = side & 1;          // Buy=0 ест ASK(0); Sell=1 ест BID(1)
            final int restAt = consume ^ 1;
            final int ckey = tick ^ (-consume & MASK);
            final int rkey = tick ^ (-restAt & MASK);

            final int[] best = this.best;
            for (;;) {
                int b = best[consume];
                if (b > ckey) break;               // NIL = MAX_VALUE выходит тем же сравнением
                qty = take(consume, b, qty);
                if (qty == 0) break;
            }
            if (qty > 0) {
                int node = alloc(id, qty);
                rest(restAt, rkey, node, qty);
            }
        }

        // Забрать ликвидность с одного уровня (лучшего в своей половине).
        private long take(int half, int key, long taker) {
            final int ti = (half << BAND_BITS) | key;
            final long[] sq = slotQty;
            final int[] sn = slotNext;
            int head = lvHead[ti];
            long levelQty = lvQty[ti];
            int free = freeHead;
            long trades = tradesCount;
            long volume = matchedVolume;
            int freed = 0;

            while (head != NIL) {
                long q = sq[head];
                long m = q < taker ? q : taker;
                trades++;
                volume += m;
                taker -= m;
                q -= m;
                levelQty -= m;
                sq[head] = q;
                if (q == 0) {
                    int next = sn[head];
                    sn[head] = free;
                    free = head;
                    head = next;
                    freed++;
                }
                if (taker == 0) break;
            }

            freeHead = free;
            tradesCount = trades;
            matchedVolume = volume;
            resting -= freed;
            lvHead[ti] = head;
            lvQty[ti] = levelQty;
            if (head == NIL) {
                lvTail[ti] = NIL;
                unmark(half, key);
            }
            return taker;
        }

        private int alloc(long id, long qty) {
            if (freeHead == NIL) grow();
            int idx = freeHead;
            freeHead = slotNext[idx];
            slotQty[idx] = qty;
            slotNext[idx] = NIL;
            ids[idx] = id;
            resting++;
            return idx;
        }

        // Холодный путь: расширение арены (в бенчмарке не срабатывает — ёмкость = n).
        private void grow() {
            int old = slotQty.length;
            int cap = old * 2;
            slotQty = java.util.Arrays.copyOf(slotQty, cap);
            slotNext = java.util.Arrays.copyOf(slotNext, cap);
            ids = java.util.Arrays.copyOf(ids, cap);
            for (int i = old; i < cap; i++) slotNext[i] = i + 1;
            slotNext[cap - 1] = NIL;
            freeHead = old;
        }

        private void rest(int half, int key, int node, long qty) {
            final int ti = (half << BAND_BITS) | key;
            int tail = lvTail[ti];
            lvTail[ti] = node;
            lvQty[ti] += qty;
            if (tail == NIL) {
                lvHead[ti] = node;
                mark(half, key);
            } else {
                slotNext[tail] = node;
            }
        }

        private void mark(int half, int t) {
            final int i1 = (half << L1_BITS) | (t >>> 6);
            l1[i1] |= 1L << t;                       // сдвиг long берётся по модулю 64
            l2[(half << 1) | (t >>> 12)] |= 1L << (t >>> 6);
            l3[half] |= 1L << (t >>> 12);
            int b = best[half];
            best[half] = t < b ? t : b;
        }

        // Вызывается только на лучшем тике половины после опустошения уровня.
        private void unmark(int half, int t) {
            final int i1 = (half << L1_BITS) | (t >>> 6);
            long v = l1[i1] & ~(1L << t);
            l1[i1] = v;
            if (v != 0) {
                // Быстрый путь: следующая лучшая цена — в этом же слове карты.
                best[half] = (t & ~63) | Long.numberOfTrailingZeros(v);
                return;
            }
            final int i2 = (half << 1) | (t >>> 12);
            long w2 = l2[i2] & ~(1L << (t >>> 6));
            l2[i2] = w2;
            if (w2 == 0) l3[half] &= ~(1L << (t >>> 12));
            best[half] = scanMin(half);
        }

        private int scanMin(int half) {
            long w3 = l3[half];
            if (w3 == 0) return NIL;
            int i2 = Long.numberOfTrailingZeros(w3);
            int i1 = (i2 << 6) | Long.numberOfTrailingZeros(l2[(half << 1) | i2]);
            return (i1 << 6) | Long.numberOfTrailingZeros(l1[(half << L1_BITS) | i1]);
        }

        // ---- наблюдение (вне горячего пути) ----

        int levelCount(int half) {
            int n = 0;
            for (int i = 0; i < L1W; i++) n += Long.bitCount(l1[(half << L1_BITS) | i]);
            return n;
        }

        int bidLevels() { return levelCount(BID); }
        int askLevels() { return levelCount(ASK); }

        long bestBid() { int b = best[BID]; return b == NIL ? 0 : base + (b ^ MASK); }
        long bestAsk() { int a = best[ASK]; return a == NIL ? 0 : base + a; }
    }

    // ------------------------------------------------------------- генератор

    static final long DEFAULT_SEED = 0xDEADBEEFCAFE1234L;
    static final int N = 100_000;

    static final class Orders {
        final int[] price;
        final int[] qty;
        final byte[] side;
        Orders(int n) { price = new int[n]; qty = new int[n]; side = new byte[n]; }
    }

    static Orders generateOrders(int n, long seed) {
        long v = seed == 0 ? DEFAULT_SEED : seed;
        Orders o = new Orders(n);
        for (int i = 0; i < n; i++) {
            v ^= v << 13; v ^= v >>> 7; v ^= v << 17;
            long r1 = v;
            v ^= v << 13; v ^= v >>> 7; v ^= v << 17;
            long r2 = v;
            v ^= v << 13; v ^= v >>> 7; v ^= v << 17;
            long r3 = v;
            o.side[i] = (byte) (r1 & 1);
            o.price[i] = (int) (10000 + Long.remainderUnsigned(r2, 200) - 100);
            o.qty[i] = (int) (1 + Long.remainderUnsigned(r3, 100));
        }
        return o;
    }

    // ---------------------------------------------------------------- замер

    static final class Result {
        long elapsedNs;
        long trades, volume;
        int bidLevels, askLevels, resting;
        long bestBid, bestAsk;
    }

    static Result runSingle(int n, Orders orders) {
        // Свежая книга на каждую итерацию — вне таймера.
        OrderBook ob = new OrderBook(n);
        final int[] px = orders.price;
        final int[] qt = orders.qty;
        final byte[] sd = orders.side;

        System.gc();

        long start = System.nanoTime();
        for (int i = 0; i < n; i++) {
            ob.processOrder(i + 1, px[i], qt[i], sd[i]);
        }
        long elapsed = System.nanoTime() - start;

        Result r = new Result();
        r.elapsedNs = elapsed;
        r.trades = ob.tradesCount;
        r.volume = ob.matchedVolume;
        r.bidLevels = ob.bidLevels();
        r.askLevels = ob.askLevels();
        r.resting = ob.resting;
        r.bestBid = ob.bestBid();
        r.bestAsk = ob.bestAsk();
        return r;
    }

    public static void main(String[] args) {
        int iterations = 5;
        long seed = DEFAULT_SEED;
        boolean json = false;
        for (int i = 0; i < args.length; i++) {
            switch (args[i]) {
                case "--iters" -> iterations = Integer.parseInt(args[++i]);
                case "--seed" -> {
                    String s = args[++i];
                    if (s.startsWith("0x") || s.startsWith("0X")) s = s.substring(2);
                    seed = Long.parseUnsignedLong(s, 16);
                }
                case "--json" -> json = true;
                default -> {
                    System.err.println("unknown argument: " + args[i]);
                    System.exit(2);
                }
            }
        }
        if (seed == 0) seed = DEFAULT_SEED;
        if (iterations < 1) iterations = 1;

        StringBuilder out = new StringBuilder(4096);
        if (!json) {
            out.append("========================================================\n");
            out.append("            JAVA MATCHING ENGINE BENCHMARK              \n");
            out.append("========================================================\n");
            out.append(String.format(java.util.Locale.ROOT, "Workload: %d orders per run | %d iterations%n", N, iterations));
            out.append(String.format(java.util.Locale.ROOT, "PRNG: Deterministic Xorshift64 (seed: 0x%s)%n", Long.toHexString(seed).toUpperCase()));
            out.append("--------------------------------------------------------\n");
        }

        // Warmup — как у автора: 10 000 ордеров тем же seed через свежую книгу.
        Orders warmup = generateOrders(10_000, seed);
        runSingle(10_000, warmup);

        Orders orders = generateOrders(N, seed);

        long total = 0;
        long min = Long.MAX_VALUE;
        long[] samples = new long[iterations];
        Result last = null;
        for (int i = 1; i <= iterations; i++) {
            Result r = runSingle(N, orders);
            last = r;
            samples[i - 1] = r.elapsedNs;
            total += r.elapsedNs;
            if (r.elapsedNs < min) min = r.elapsedNs;
            if (!json) {
                double ms = r.elapsedNs / 1e6;
                double mops = N / (r.elapsedNs / 1e9) / 1e6;
                double nsPer = (double) r.elapsedNs / N;
                out.append(String.format(java.util.Locale.ROOT, "  Iteration %d: %8.3f ms | %10.2f M ops/s | %6.2f ns/order%n", i, ms, mops, nsPer));
            }
        }

        long avg = total / iterations;
        double bestMs = min / 1e6;
        double avgMs = avg / 1e6;
        double bestOps = N / (min / 1e9) / 1e6;
        double avgOps = N / (avg / 1e9) / 1e6;
        double avgLatency = (double) avg / N;
        int totalLevels = last.bidLevels + last.askLevels;

        if (json) {
            java.util.Arrays.sort(samples);
            double med = samples[iterations / 2] / 1e6;
            out.append(String.format(java.util.Locale.ROOT,
                "{\"lang\":\"java\",\"orders\":%d,\"iters\":%d,\"min_ms\":%.6f,\"median_ms\":%.6f,\"max_ms\":%.6f,"
                + "\"trades\":%d,\"volume\":%d,\"bid_levels\":%d,\"ask_levels\":%d,\"resting\":%d,"
                + "\"best_bid\":%d,\"best_ask\":%d}%n",
                N, iterations, bestMs, med, samples[iterations - 1] / 1e6,
                last.trades, last.volume, last.bidLevels, last.askLevels, last.resting, last.bestBid, last.bestAsk));
        } else {
            out.append("--------------------------------------------------------\n");
            out.append("SUMMARY (JAVA):\n");
            out.append(String.format(java.util.Locale.ROOT, "  Best Time:         %.3f ms (%.2f M ops/s)%n", bestMs, bestOps));
            out.append(String.format(java.util.Locale.ROOT, "  Average Time:      %.3f ms (%.2f M ops/s)%n", avgMs, avgOps));
            out.append(String.format(java.util.Locale.ROOT, "  Average Latency:   %.2f ns/order%n", avgLatency));
            out.append(String.format(java.util.Locale.ROOT, "  Trades Executed:   %d%n", last.trades));
            out.append(String.format(java.util.Locale.ROOT, "  Volume Matched:    %d%n", last.volume));
            out.append(String.format(java.util.Locale.ROOT, "  Resting in Book:   %d%n", last.resting));
            out.append(String.format(java.util.Locale.ROOT, "  Active Levels:     %d (Best Bid: %d, Best Ask: %d)%n", totalLevels, last.bestBid, last.bestAsk));
            out.append("========================================================\n\n");
        }
        System.out.print(out);
        System.out.flush();

        // Инварианты на дефолтном seed — обязательны.
        if (seed == DEFAULT_SEED) {
            boolean ok = last.trades == 77_576
                && last.volume == 1_973_216
                && last.bidLevels == 55
                && last.askLevels == 50
                && last.resting == 21_620
                && last.bestBid == 10_038
                && last.bestAsk == 10_043;
            if (!ok) {
                System.err.printf(java.util.Locale.ROOT, "INVARIANT VIOLATION: trades=%d volume=%d bid_levels=%d ask_levels=%d resting=%d best_bid=%d best_ask=%d%n",
                    last.trades, last.volume, last.bidLevels, last.askLevels, last.resting, last.bestBid, last.bestAsk);
                System.exit(1);
            }
        }
    }
}
