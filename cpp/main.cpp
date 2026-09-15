// C++ matching engine benchmark — semantics, generator and measurement protocol
// identical to golang/main.go and rust/src/bin/author_protocol.rs.
//
// Data structure (mirrors rust/src/orderbook.rs):
//   * flat array of price levels, direct tick indexing (band of 8192 ticks,
//     base = first price - 4096);
//   * three-level occupancy bitmap (l1: 128 x u64, l2: 2 x u64, l3: u64);
//     best price = trailing zeros; bid side stored in inverted coordinates
//     (key = tick ^ MASK) so "best" is always the minimum for both sides;
//   * order arena addressed by u32 indices, intrusive free-list, 16-byte slots,
//     order ids in a cold array (never read while matching);
//   * zero allocations inside the timed loop: arena and levels are allocated
//     when the book is constructed (outside the timer).
//
// Build: clang++ -O3 -std=c++20 -mcpu=native -fno-exceptions -fno-rtti main.cpp

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>

// ------------------------------------------------------------------------------------
// Generator (bit-identical to golang/generator.go)
// ------------------------------------------------------------------------------------

struct Order {
    uint64_t id;
    uint64_t price;
    uint64_t qty;
    uint32_t side;   // 0 = Buy, 1 = Sell
    uint32_t _pad;
};

static constexpr uint64_t DEFAULT_SEED = 0xDEADBEEFCAFE1234ULL;

struct Xorshift64 {
    uint64_t s;
    explicit Xorshift64(uint64_t seed) : s(seed == 0 ? DEFAULT_SEED : seed) {}
    inline uint64_t next() {
        uint64_t v = s;
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        s = v;
        return v;
    }
};

static void generate_orders(Order* out, size_t n, uint64_t seed) {
    Xorshift64 rng(seed);
    for (size_t i = 0; i < n; ++i) {
        uint64_t r1 = rng.next();
        uint64_t r2 = rng.next();
        uint64_t r3 = rng.next();
        out[i].id = (uint64_t)(i + 1);
        out[i].side = (uint32_t)(r1 & 1);
        out[i].price = (uint64_t)((int64_t)10000 + (int64_t)(r2 % 200) - 100);
        out[i].qty = 1 + (r3 % 100);
        out[i]._pad = 0;
    }
}

// ------------------------------------------------------------------------------------
// Order book: flat levels + hierarchical bitmap + index arena
// ------------------------------------------------------------------------------------

static constexpr unsigned BAND_BITS = 13;
static constexpr size_t   BAND      = size_t(1) << BAND_BITS;   // 8192 ticks
static constexpr uint32_t MASK      = (uint32_t)(BAND - 1);
static constexpr size_t   L1W       = BAND / 64;                // 128 words
static constexpr size_t   L2W       = (L1W + 63) / 64;          // 2 words
static constexpr uint32_t NIL       = 0xFFFFFFFFu;
static constexpr unsigned ASK       = 0;
static constexpr unsigned BID       = 1;

#define ALWAYS_INLINE inline __attribute__((always_inline))
#define NOINLINE_COLD __attribute__((noinline, cold))

// 16 bytes: four levels per cache line, addressed by shift.
struct Lvl {
    uint64_t qty;
    uint32_t head;
    uint32_t tail;
};

// 16 bytes: order slot in the arena. The id lives in a cold array.
struct Slot {
    uint64_t qty;
    uint32_t next;
    uint32_t _pad;
};

struct alignas(64) Half {
    uint32_t best;          // minimum occupied key in own coordinates, NIL if empty
    uint64_t l3;
    uint64_t l2[L2W];
    uint64_t* l1;           // L1W words
    Lvl*      lv;           // BAND levels

    void init() {
        best = NIL;
        l3 = 0;
        l2[0] = 0; l2[1] = 0;
        l1 = static_cast<uint64_t*>(std::calloc(L1W, sizeof(uint64_t)));
        lv = static_cast<Lvl*>(std::malloc(BAND * sizeof(Lvl)));
        if (!l1 || !lv) { std::fputs("oom\n", stderr); std::exit(3); }
        for (size_t i = 0; i < BAND; ++i) lv[i] = Lvl{0, NIL, NIL};
    }
    void destroy() { std::free(l1); std::free(lv); }

    ALWAYS_INLINE void mark(uint32_t t) {
        l1[t >> 6]  |= uint64_t(1) << (t & 63);
        l2[t >> 12] |= uint64_t(1) << ((t >> 6) & 63);
        l3          |= uint64_t(1) << (t >> 12);
        best = t < best ? t : best;   // branchless min (csel)
    }

    ALWAYS_INLINE void unmark(uint32_t t) {
        // Fast path: the l1 word is already in a register; if levels remain in it,
        // the next best key is in the same word (all lower words are empty because
        // unmark is only ever called on the current best).
        uint32_t i1 = t >> 6;
        uint64_t v = l1[i1] & ~(uint64_t(1) << (t & 63));
        l1[i1] = v;
        if (v != 0) {
            best = (i1 << 6) | (uint32_t)__builtin_ctzll(v);
            return;
        }
        uint32_t i2 = t >> 12;
        uint64_t w2 = l2[i2] & ~(uint64_t(1) << (i1 & 63));
        l2[i2] = w2;
        if (w2 != 0) {
            uint32_t j1 = (i2 << 6) | (uint32_t)__builtin_ctzll(w2);
            best = (j1 << 6) | (uint32_t)__builtin_ctzll(l1[j1]);
            return;
        }
        l3 &= ~(uint64_t(1) << i2);
        if (l3 == 0) { best = NIL; return; }
        uint32_t j2 = (uint32_t)__builtin_ctzll(l3);
        uint32_t j1 = (j2 << 6) | (uint32_t)__builtin_ctzll(l2[j2]);
        best = (j1 << 6) | (uint32_t)__builtin_ctzll(l1[j1]);
    }

    size_t level_count() const {
        size_t n = 0;
        for (size_t i = 0; i < L1W; ++i) n += (size_t)__builtin_popcountll(l1[i]);
        return n;
    }
};

struct OrderBook {
    Half      h[2];         // h[ASK] direct ticks, h[BID] inverted ticks
    Slot*     arena;
    uint64_t* ids;          // cold: order ids
    uint32_t  free_head;
    uint32_t  cap;
    uint64_t  base;
    bool      base_pinned;
    uint32_t  resting;
    uint64_t  trades_count;
    uint64_t  matched_volume;
    uint64_t  rejected;

    explicit OrderBook(size_t capacity) {
        cap = (uint32_t)(capacity < 64 ? 64 : capacity);
        arena = static_cast<Slot*>(std::malloc(size_t(cap) * sizeof(Slot)));
        ids   = static_cast<uint64_t*>(std::calloc(cap, sizeof(uint64_t)));
        if (!arena || !ids) { std::fputs("oom\n", stderr); std::exit(3); }
        for (uint32_t i = 0; i < cap; ++i) arena[i] = Slot{0, i + 1, 0};
        arena[cap - 1].next = NIL;
        h[0].init();
        h[1].init();
        free_head = 0;
        base = 0;
        base_pinned = false;
        resting = 0;
        trades_count = 0;
        matched_volume = 0;
        rejected = 0;
    }
    ~OrderBook() {
        h[0].destroy(); h[1].destroy();
        std::free(arena); std::free(ids);
    }
    OrderBook(const OrderBook&) = delete;
    OrderBook& operator=(const OrderBook&) = delete;

    // Cold path: pin or move the band. Only allowed while the book is empty.
    NOINLINE_COLD bool rebase(uint64_t price) {
        if (base_pinned && (resting != 0 || h[0].l3 != 0 || h[1].l3 != 0)) return false;
        base = price >= (BAND / 2) ? price - (BAND / 2) : 0;
        base_pinned = true;
        return true;
    }

    NOINLINE_COLD void reject() { ++rejected; }

    // Cold path: the arena is index-addressed so it can grow without invalidating links.
    NOINLINE_COLD void grow() {
        uint32_t ncap = cap * 2;
        Slot* na = static_cast<Slot*>(std::malloc(size_t(ncap) * sizeof(Slot)));
        uint64_t* ni = static_cast<uint64_t*>(std::malloc(size_t(ncap) * sizeof(uint64_t)));
        if (!na || !ni) { std::fputs("oom\n", stderr); std::exit(3); }
        std::memcpy(na, arena, size_t(cap) * sizeof(Slot));
        std::memcpy(ni, ids, size_t(cap) * sizeof(uint64_t));
        for (uint32_t i = cap; i < ncap; ++i) na[i] = Slot{0, i + 1, 0};
        na[ncap - 1].next = NIL;
        std::free(arena); std::free(ids);
        arena = na; ids = ni;
        free_head = cap;
        cap = ncap;
    }

    // Take liquidity from one level (key = current best of that half).
    ALWAYS_INLINE uint64_t take(unsigned half, uint32_t key, uint64_t taker) {
        Half& hh = h[half];
        Lvl& l = hh.lv[key];
        uint32_t head = l.head;
        uint64_t level_qty = l.qty;

        // Keep counters in registers: the compiler cannot prove no aliasing otherwise.
        uint32_t fh = free_head;
        uint64_t trades = trades_count;
        uint64_t volume = matched_volume;
        uint32_t freed = 0;
        Slot* ar = arena;

        while (taker > 0 && head != NIL) {
            Slot& s = ar[head];
            uint64_t sq = s.qty;
            uint64_t m = sq < taker ? sq : taker;
            trades += 1;
            volume += m;
            taker -= m;
            sq -= m;
            level_qty -= m;
            s.qty = sq;
            if (sq == 0) {
                uint32_t nx = s.next;
                s.next = fh;
                fh = head;
                head = nx;
                freed += 1;
            }
        }

        free_head = fh;
        trades_count = trades;
        matched_volume = volume;
        resting -= freed;

        l.head = head;
        l.qty = level_qty;
        if (head == NIL) {
            l.tail = NIL;
            hh.unmark(key);
        }
        return taker;
    }

    ALWAYS_INLINE uint32_t alloc(uint64_t id, uint64_t qty) {
        if (__builtin_expect(free_head == NIL, 0)) grow();
        uint32_t idx = free_head;
        Slot& s = arena[idx];
        free_head = s.next;
        s.qty = qty;
        s.next = NIL;
        ids[idx] = id;
        resting += 1;
        return idx;
    }

    ALWAYS_INLINE void rest(unsigned half, uint32_t key, uint32_t node, uint64_t qty) {
        Half& hh = h[half];
        Lvl& l = hh.lv[key];
        uint32_t tail = l.tail;
        l.tail = node;
        l.qty += qty;
        if (tail == NIL) {
            l.head = node;
            hh.mark(key);
        } else {
            arena[tail].next = node;
        }
    }

    // Hot path. Branchless with respect to side.
    ALWAYS_INLINE void process_order(uint64_t id, uint64_t price, uint64_t qty, uint32_t side) {
        if (__builtin_expect(qty == 0, 0)) { reject(); return; }
        uint64_t t64 = price - base;
        if (__builtin_expect(t64 >= BAND, 0)) {
            if (!rebase(price)) { reject(); return; }
            t64 = price - base;
            if (t64 >= BAND) { reject(); return; }
        }
        uint32_t tick = (uint32_t)t64;

        unsigned consume = side & 1;          // Buy eats ASK(0), Sell eats BID(1)
        unsigned rest_at = consume ^ 1;
        uint32_t cmask = (0u - (uint32_t)(consume == BID)) & MASK;
        uint32_t rmask = (0u - (uint32_t)(rest_at == BID)) & MASK;
        uint32_t ckey = tick ^ cmask;
        uint32_t rkey = tick ^ rmask;

        // Single loop for both sides: best is always the minimum in own coordinates;
        // NIL > any key, so an empty half exits by the same comparison.
        for (;;) {
            uint32_t best = h[consume].best;
            if (qty == 0 || best > ckey) break;
            qty = take(consume, best, qty);
        }
        if (qty > 0) {
            uint32_t node = alloc(id, qty);
            rest(rest_at, rkey, node, qty);
        }
    }

    uint64_t best_bid() const { uint32_t b = h[BID].best; return b == NIL ? 0 : base + (uint64_t)(b ^ MASK); }
    uint64_t best_ask() const { uint32_t a = h[ASK].best; return a == NIL ? 0 : base + (uint64_t)a; }
    size_t bid_levels() const { return h[BID].level_count(); }
    size_t ask_levels() const { return h[ASK].level_count(); }
};

// ------------------------------------------------------------------------------------
// Benchmark (protocol of golang/main.go)
// ------------------------------------------------------------------------------------

static inline uint64_t now_ns() {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC_RAW, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

struct RunStats {
    uint64_t elapsed_ns;
    uint64_t trades, volume;
    size_t resting, bid_levels, ask_levels;
    uint64_t best_bid, best_ask;
};

__attribute__((noinline))
static RunStats run_single_benchmark(size_t n, const Order* orders) {
    OrderBook ob(n);   // fresh book, capacity n — outside the timer

    uint64_t start = now_ns();
    for (size_t i = 0; i < n; ++i) {
        const Order& o = orders[i];
        ob.process_order(o.id, o.price, o.qty, o.side);
    }
    uint64_t elapsed = now_ns() - start;

    RunStats r;
    r.elapsed_ns = elapsed;
    r.trades = ob.trades_count;
    r.volume = ob.matched_volume;
    r.resting = ob.resting;
    r.bid_levels = ob.bid_levels();
    r.ask_levels = ob.ask_levels();
    r.best_bid = ob.best_bid();
    r.best_ask = ob.best_ask();
    return r;
}

static uint64_t parse_hex(const char* s) {
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) s += 2;
    return std::strtoull(s, nullptr, 16);
}

int main(int argc, char** argv) {
    const size_t N = 100000;
    size_t iterations = 5;
    uint64_t seed = DEFAULT_SEED;
    bool json = false;

    for (int i = 1; i < argc; ++i) {
        if (std::strcmp(argv[i], "--iters") == 0 && i + 1 < argc) {
            long v = std::strtol(argv[++i], nullptr, 10);
            if (v > 0) iterations = (size_t)v;
        } else if (std::strcmp(argv[i], "--seed") == 0 && i + 1 < argc) {
            seed = parse_hex(argv[++i]);
            if (seed == 0) seed = DEFAULT_SEED;
        } else if (std::strcmp(argv[i], "--json") == 0) {
            json = true;
        }
    }

    if (!json) {
        std::printf("========================================================\n");
        std::printf("             C++ MATCHING ENGINE BENCHMARK              \n");
        std::printf("========================================================\n");
        std::printf("Workload: %zu orders per run | %zu iterations\n", N, iterations);
        std::printf("PRNG: Deterministic Xorshift64 (seed: 0x%llX)\n", (unsigned long long)seed);
        std::printf("--------------------------------------------------------\n");
    }

    // Warmup: 10k orders through a fresh book, not printed.
    Order* warmup = static_cast<Order*>(std::malloc(10000 * sizeof(Order)));
    generate_orders(warmup, 10000, seed);
    run_single_benchmark(10000, warmup);
    std::free(warmup);

    Order* orders = static_cast<Order*>(std::malloc(N * sizeof(Order)));
    generate_orders(orders, N, seed);

    uint64_t total_ns = 0;
    uint64_t min_ns = ~uint64_t(0);
    RunStats last{};

    for (size_t i = 1; i <= iterations; ++i) {
        RunStats r = run_single_benchmark(N, orders);
        total_ns += r.elapsed_ns;
        if (r.elapsed_ns < min_ns) min_ns = r.elapsed_ns;
        last = r;
        if (!json) {
            double ms = (double)r.elapsed_ns / 1e6;
            double ops = (double)N / ((double)r.elapsed_ns / 1e9);
            std::printf("  Iteration %zu: %8.3f ms | %10.2f M ops/s | %6.2f ns/order\n",
                        i, ms, ops / 1e6, (double)r.elapsed_ns / (double)N);
        }
    }
    std::free(orders);

    uint64_t avg_ns = total_ns / iterations;
    double avg_ms = (double)avg_ns / 1e6;
    double min_ms = (double)min_ns / 1e6;
    double avg_ops = (double)N / ((double)avg_ns / 1e9);
    double best_ops = (double)N / ((double)min_ns / 1e9);
    size_t total_levels = last.bid_levels + last.ask_levels;

    if (json) {
        std::printf("{\"lang\":\"cpp\",\"orders\":%zu,\"iters\":%zu,\"min_ms\":%.6f,\"avg_ms\":%.6f,"
                    "\"trades\":%llu,\"volume\":%llu,\"bid_levels\":%zu,\"ask_levels\":%zu,\"resting\":%zu,"
                    "\"best_bid\":%llu,\"best_ask\":%llu}\n",
                    N, iterations, min_ms, avg_ms,
                    (unsigned long long)last.trades, (unsigned long long)last.volume,
                    last.bid_levels, last.ask_levels, last.resting,
                    (unsigned long long)last.best_bid, (unsigned long long)last.best_ask);
    } else {
        std::printf("--------------------------------------------------------\n");
        std::printf("SUMMARY (C++):\n");
        std::printf("  Best Time:         %.3f ms (%.2f M ops/s)\n", min_ms, best_ops / 1e6);
        std::printf("  Average Time:      %.3f ms (%.2f M ops/s)\n", avg_ms, avg_ops / 1e6);
        std::printf("  Average Latency:   %.2f ns/order\n", (double)avg_ns / (double)N);
        std::printf("  Trades Executed:   %llu\n", (unsigned long long)last.trades);
        std::printf("  Volume Matched:    %llu\n", (unsigned long long)last.volume);
        std::printf("  Resting in Book:   %zu\n", last.resting);
        std::printf("  Active Levels:     %zu (Best Bid: %llu, Best Ask: %llu)\n",
                    total_levels, (unsigned long long)last.best_bid, (unsigned long long)last.best_ask);
        std::printf("========================================================\n\n");
    }

    // Mandatory invariants on the default seed: non-zero exit on any mismatch.
#ifdef FAULT_INJECT
    last.trades += 1;   // test hook: verifies that the invariant check really fails
#endif
    if (seed == DEFAULT_SEED) {
        bool ok = last.trades == 77576 && last.volume == 1973216 &&
                  last.bid_levels == 55 && last.ask_levels == 50 &&
                  last.resting == 21620 && last.best_bid == 10038 && last.best_ask == 10043;
        if (!ok) {
            std::fprintf(stderr,
                "INVARIANT VIOLATION: trades=%llu volume=%llu bid_levels=%zu ask_levels=%zu "
                "resting=%zu best_bid=%llu best_ask=%llu\n",
                (unsigned long long)last.trades, (unsigned long long)last.volume,
                last.bid_levels, last.ask_levels, last.resting,
                (unsigned long long)last.best_bid, (unsigned long long)last.best_ask);
            return 1;
        }
    }
    return 0;
}
