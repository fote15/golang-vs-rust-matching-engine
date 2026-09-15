// main.c — обвязка бенчмарка: генератор Xorshift64, создание книги, таймер, печать.
// Горячий путь (матчинг + укладка + битовая карта) целиком в engine.s.
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define BAND 8192u
#define L1W (BAND / 64)
#define NIL 0xFFFFFFFFu

typedef struct { uint64_t qty; uint32_t head, tail; } Lvl;
typedef struct { uint64_t qty; uint32_t next, pad; } Slot;
// Запись ордера — ровно как golang/types.go и rust/src/types.rs: три u64 + u8 side,
// естественное выравнивание добивает до 32 байт (7 байт паддинга после side).
typedef struct { uint64_t id; uint64_t price; uint64_t qty; uint8_t side; } Order;

typedef struct {
    Lvl      lv[BAND];      // 0x00000
    uint64_t l1[L1W];       // 0x20000
    uint64_t l2[2];         // 0x20400
    uint64_t l3;            // 0x20410
    uint32_t best;          // 0x20418
    uint8_t  _pad[0x20480 - 0x2041C];
} Half;

typedef struct {
    uint64_t base;          // 0
    uint32_t free_head;     // 8
    uint32_t resting;       // 12
    uint64_t trades;        // 16
    uint64_t volume;        // 24
    uint64_t rejected;      // 32
    uint64_t pinned;        // 40
    Slot    *arena;         // 48
    uint32_t*ids;           // 56
    uint8_t  _pad[128 - 64];
    Half     h[2];          // 128: [0] ASK, [1] BID
} Book;

_Static_assert(sizeof(Lvl) == 16, "Lvl");
_Static_assert(sizeof(Slot) == 16, "Slot");
_Static_assert(sizeof(Order) == 32, "Order");
_Static_assert(offsetof(Order, id) == 0, "Order.id");
_Static_assert(offsetof(Order, price) == 8, "Order.price");
_Static_assert(offsetof(Order, qty) == 16, "Order.qty");
_Static_assert(offsetof(Order, side) == 24, "Order.side");
_Static_assert(offsetof(Half, l1) == 0x20000, "l1");
_Static_assert(offsetof(Half, l2) == 0x20400, "l2");
_Static_assert(offsetof(Half, l3) == 0x20410, "l3");
_Static_assert(offsetof(Half, best) == 0x20418, "best");
_Static_assert(sizeof(Half) == 0x20480, "Half");
_Static_assert(offsetof(Book, free_head) == 8, "free");
_Static_assert(offsetof(Book, resting) == 12, "resting");
_Static_assert(offsetof(Book, trades) == 16, "trades");
_Static_assert(offsetof(Book, volume) == 24, "volume");
_Static_assert(offsetof(Book, rejected) == 32, "rejected");
_Static_assert(offsetof(Book, pinned) == 40, "pinned");
_Static_assert(offsetof(Book, arena) == 48, "arena");
_Static_assert(offsetof(Book, ids) == 56, "ids");
_Static_assert(offsetof(Book, h) == 128, "h0");
_Static_assert(offsetof(Book, h[1]) == 128 + 0x20480, "h1");

extern void engine_run(Book *book, const Order *orders, uint64_t n);

// ---- генератор: побитово как golang/generator.go ----
static uint64_t xorshift64(uint64_t *s) {
    uint64_t v = *s;
    v ^= v << 13;
    v ^= v >> 7;
    v ^= v << 17;
    *s = v;
    return v;
}

static Order *generate_orders(size_t n, uint64_t seed) {
    if (seed == 0) seed = 0xDEADBEEFCAFE1234ull;
    Order *o = malloc(n * sizeof(Order));
    if (!o) { perror("malloc"); exit(2); }
    uint64_t s = seed;
    for (size_t i = 0; i < n; i++) {
        uint64_t r1 = xorshift64(&s), r2 = xorshift64(&s), r3 = xorshift64(&s);
        o[i].id    = (uint64_t)(i + 1);
        o[i].side  = (uint8_t)(r1 & 1);
        o[i].price = (uint64_t)(10000 + (int64_t)(r2 % 200) - 100);
        o[i].qty   = 1 + (r3 % 100);
    }
    return o;
}

// ---- книга: свежая на каждую итерацию, вся память тронута до таймера ----
static Book *book_new(size_t capacity) {
    if (capacity < 64) capacity = 64;
    Book *b = NULL;
    if (posix_memalign((void **)&b, 128, sizeof(Book)) != 0) { perror("posix_memalign"); exit(2); }
    memset(b, 0, sizeof(Book));
    for (int h = 0; h < 2; h++) {
        for (size_t i = 0; i < BAND; i++) { b->h[h].lv[i].head = NIL; b->h[h].lv[i].tail = NIL; }
        b->h[h].best = NIL;
    }
    b->arena = malloc(capacity * sizeof(Slot));
    b->ids   = malloc(capacity * sizeof(uint32_t));
    if (!b->arena || !b->ids) { perror("malloc"); exit(2); }
    for (size_t i = 0; i < capacity; i++) {
        b->arena[i].qty = 0; b->arena[i].pad = 0;
        b->arena[i].next = (i + 1 < capacity) ? (uint32_t)(i + 1) : NIL;
        b->ids[i] = 0;
    }
    b->free_head = 0;
    b->base = 0;
    b->pinned = 0;
    return b;
}

static void book_free(Book *b) { free(b->arena); free(b->ids); free(b); }

static unsigned level_count(const Half *h) {
    unsigned n = 0;
    for (size_t i = 0; i < L1W; i++) n += (unsigned)__builtin_popcountll(h->l1[i]);
    return n;
}

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC_RAW, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

typedef struct {
    uint64_t elapsed_ns, trades, volume, best_bid, best_ask;
    unsigned resting, bid_levels, ask_levels;
} RunResult;

static RunResult run_single(const Order *orders, size_t n) {
    Book *b = book_new(n);
    uint64_t t0 = now_ns();
    engine_run(b, orders, n);
    uint64_t t1 = now_ns();
    RunResult r;
    r.elapsed_ns = t1 - t0;
    r.trades = b->trades;
    r.volume = b->volume;
    r.resting = b->resting;
    r.bid_levels = level_count(&b->h[1]);
    r.ask_levels = level_count(&b->h[0]);
    r.best_ask = b->h[0].best == NIL ? 0 : b->base + b->h[0].best;
    r.best_bid = b->h[1].best == NIL ? 0 : b->base + (b->h[1].best ^ (BAND - 1));
    book_free(b);
    return r;
}

int main(int argc, char **argv) {
    const uint64_t DEFAULT_SEED = 0xDEADBEEFCAFE1234ull;
    const size_t N = 100000;
    uint64_t seed = DEFAULT_SEED;
    int iters = 5, json = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--iters") && i + 1 < argc) iters = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--seed") && i + 1 < argc) seed = strtoull(argv[++i], NULL, 16);
        else if (!strcmp(argv[i], "--json")) json = 1;
    }
    if (iters < 1) iters = 1;
    if (seed == 0) seed = DEFAULT_SEED;

    if (!json) {
        printf("========================================================\n");
        printf("             ASM MATCHING ENGINE BENCHMARK              \n");
        printf("========================================================\n");
        printf("Workload: %zu orders per run | %d iterations\n", N, iters);
        printf("PRNG: Deterministic Xorshift64 (seed: 0x%" PRIX64 ")\n", seed);
        printf("--------------------------------------------------------\n");
    }

    Order *warm = generate_orders(10000, seed);
    run_single(warm, 10000);
    free(warm);

    Order *orders = generate_orders(N, seed);

    uint64_t total = 0, best = UINT64_MAX;
    RunResult last;
    memset(&last, 0, sizeof last);
    for (int i = 1; i <= iters; i++) {
        RunResult r = run_single(orders, N);
        total += r.elapsed_ns;
        if (r.elapsed_ns < best) best = r.elapsed_ns;
        last = r;
        if (!json) {
            double ms = (double)r.elapsed_ns / 1e6;
            printf("  Iteration %d: %8.3f ms | %10.2f M ops/s | %6.2f ns/order\n",
                   i, ms, (double)N / (double)r.elapsed_ns * 1e3, (double)r.elapsed_ns / (double)N);
        }
    }
    free(orders);

    double avg_ns = (double)total / (double)iters;
    unsigned levels = last.bid_levels + last.ask_levels;
    if (json) {
        printf("{\"lang\":\"asm\",\"orders\":%zu,\"iters\":%d,\"min_ms\":%.6f,\"avg_ms\":%.6f,"
               "\"trades\":%" PRIu64 ",\"volume\":%" PRIu64 ",\"bid_levels\":%u,\"ask_levels\":%u,"
               "\"resting\":%u,\"best_bid\":%" PRIu64 ",\"best_ask\":%" PRIu64 "}\n",
               N, iters, (double)best / 1e6, avg_ns / 1e6, last.trades, last.volume,
               last.bid_levels, last.ask_levels, last.resting, last.best_bid, last.best_ask);
    } else {
        printf("--------------------------------------------------------\n");
        printf("SUMMARY (ASM):\n");
        printf("  Best Time:         %.3f ms (%.2f M ops/s)\n", (double)best / 1e6, (double)N / (double)best * 1e3);
        printf("  Average Time:      %.3f ms (%.2f M ops/s)\n", avg_ns / 1e6, (double)N / avg_ns * 1e3);
        printf("  Average Latency:   %.2f ns/order\n", avg_ns / (double)N);
        printf("  Trades Executed:   %" PRIu64 "\n", last.trades);
        printf("  Volume Matched:    %" PRIu64 "\n", last.volume);
        printf("  Resting in Book:   %u\n", last.resting);
        printf("  Active Levels:     %u (Best Bid: %" PRIu64 ", Best Ask: %" PRIu64 ")\n",
               levels, last.best_bid, last.best_ask);
        printf("========================================================\n\n");
    }

    if (seed == DEFAULT_SEED) {
        int ok = last.trades == 77576 && last.volume == 1973216 && last.bid_levels == 55 &&
                 last.ask_levels == 50 && last.resting == 21620 && last.best_bid == 10038 &&
                 last.best_ask == 10043;
        if (!ok) {
            fprintf(stderr, "INVARIANT VIOLATION: trades=%" PRIu64 " volume=%" PRIu64
                    " bid_levels=%u ask_levels=%u resting=%u best_bid=%" PRIu64 " best_ask=%" PRIu64 "\n",
                    last.trades, last.volume, last.bid_levels, last.ask_levels, last.resting,
                    last.best_bid, last.best_ask);
            return 1;
        }
    }
    return 0;
}
