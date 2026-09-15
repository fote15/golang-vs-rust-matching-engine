/*
 * C matching engine benchmark — same generator, same semantics, same
 * measurement protocol and output format as golang/main.go.
 *
 * Data structure (mirrors rust/src/orderbook.rs):
 *   - flat price band of BAND = 8192 ticks, level = base + tick, 16-byte Lvl;
 *   - two halves: ASK in direct tick coordinates, BID in inverted (t ^ MASK),
 *     so "best price" of either side is always the MINIMUM occupied tick and
 *     the matching loop has no branch on side;
 *   - three-level occupancy bitmap (l1: 128 words, l2: 2 words, l3: 1 word);
 *     best tick = chain of trailing-zero counts, cached in `best`;
 *   - order arena of 16-byte slots addressed by u32 indices with an intrusive
 *     free list; order ids live in a cold array that the hot path never reads;
 *   - zero heap allocation inside the timed loop.
 *
 * Build: clang -O3 -mcpu=native -std=c11 main.c -o build/matching-engine-c
 */
#define _DARWIN_C_SOURCE
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define BAND_BITS 13u
#define BAND (1u << BAND_BITS)
#define MASK (BAND - 1u)
#define L1W (BAND / 64u)          /* 128 */
#define L2W ((L1W + 63u) / 64u)   /* 2   */
#define NIL UINT32_MAX
#define ASK 0u
#define BID 1u

#define LIKELY(x) __builtin_expect(!!(x), 1)
#define UNLIKELY(x) __builtin_expect(!!(x), 0)
#define ALWAYS_INLINE static inline __attribute__((always_inline))
#define COLD static __attribute__((noinline, cold))

/* ---------------------------------------------------------------- types */

typedef struct {
    uint64_t qty;
    uint32_t head;
    uint32_t tail;
} Lvl; /* 16 bytes: four levels per 64-byte line */

typedef struct {
    uint64_t qty;
    uint32_t next;
    uint32_t pad;
} Slot; /* 16 bytes */

typedef struct __attribute__((aligned(128))) {
    uint32_t best;      /* min occupied tick in own coordinates, NIL if empty */
    uint32_t pad0;
    uint64_t l3;
    uint64_t l2[L2W];
    uint64_t l1[L1W];
    Lvl lv[BAND];
} Half;

typedef struct __attribute__((aligned(128))) {
    Half h[2];          /* h[ASK] direct ticks, h[BID] inverted ticks */
    Slot *arena;
    uint64_t *ids;      /* cold: never read in the hot path */
    uint32_t cap;
    uint32_t free_head;
    uint64_t base;      /* tick = price - base */
    uint32_t base_pinned;
    uint32_t resting;
    uint64_t trades;
    uint64_t volume;
    uint64_t rejected;
} Book;

typedef struct {
    uint64_t id;
    uint64_t price;
    uint64_t qty;
    uint8_t  side;      /* 0 = Buy, 1 = Sell */
} Order; /* 32 bytes — та же раскладка, что OrderInput в Go и Order в Rust; генерируется вне таймера */

/* ------------------------------------------------------------- generator */

static uint64_t xorshift64(uint64_t *state) {
    uint64_t v = *state;
    v ^= v << 13;
    v ^= v >> 7;
    v ^= v << 17;
    *state = v;
    return v;
}

static Order *generate_orders(uint32_t n, uint64_t seed) {
    uint64_t s = seed ? seed : 0xDEADBEEFCAFE1234ull;
    Order *o = (Order *)malloc((size_t)n * sizeof(Order));
    if (!o) { fprintf(stderr, "oom\n"); exit(2); }
    for (uint32_t i = 0; i < n; i++) {
        uint64_t r1 = xorshift64(&s);
        uint64_t r2 = xorshift64(&s);
        uint64_t r3 = xorshift64(&s);
        o[i].side = (uint8_t)(r1 & 1u);
        o[i].price = 10000u + (r2 % 200u) - 100u;
        o[i].qty = 1u + (r3 % 100u);
        o[i].id = i + 1u;
    }
    return o;
}

/* ------------------------------------------------------------------ book */

static void half_init(Half *h) {
    h->best = NIL;
    h->l3 = 0;
    memset(h->l2, 0, sizeof h->l2);
    memset(h->l1, 0, sizeof h->l1);
    for (uint32_t i = 0; i < BAND; i++) {
        h->lv[i].qty = 0;
        h->lv[i].head = NIL;
        h->lv[i].tail = NIL;
    }
}

static void arena_link(Slot *a, uint32_t from, uint32_t to) {
    for (uint32_t i = from; i < to; i++) {
        a[i].qty = 0;
        a[i].next = i + 1u;
        a[i].pad = 0;
    }
    a[to - 1u].next = NIL;
}

/* Fresh book with a given capacity. All memory is allocated AND touched
 * here, outside the timer. Band is unpinned: the first order centres it. */
static Book *book_new(uint32_t capacity) {
    uint32_t cap = capacity < 64u ? 64u : capacity;
    Book *b = NULL;
    if (posix_memalign((void **)&b, 128, sizeof(Book)) != 0 || !b) {
        fprintf(stderr, "oom\n");
        exit(2);
    }
    half_init(&b->h[ASK]);
    half_init(&b->h[BID]);
    if (posix_memalign((void **)&b->arena, 128, (size_t)cap * sizeof(Slot)) != 0) {
        fprintf(stderr, "oom\n");
        exit(2);
    }
    b->ids = (uint64_t *)calloc(cap, sizeof(uint64_t));
    if (!b->ids) { fprintf(stderr, "oom\n"); exit(2); }
    arena_link(b->arena, 0, cap);
    b->cap = cap;
    b->free_head = 0;
    b->base = 0;
    b->base_pinned = 0;
    b->resting = 0;
    b->trades = 0;
    b->volume = 0;
    b->rejected = 0;
    return b;
}

static void book_free(Book *b) {
    free(b->arena);
    free(b->ids);
    free(b);
}

/* Cold: pin or move the band. Only allowed on an empty book. */
COLD int book_rebase(Book *b, uint64_t price) {
    if (b->base_pinned && (b->resting != 0 || b->h[0].l3 != 0 || b->h[1].l3 != 0))
        return 0;
    b->base = price >= (uint64_t)(BAND / 2u) ? price - (BAND / 2u) : 0;
    b->base_pinned = 1;
    return 1;
}

/* Cold: double the arena. Never fires on this workload (resting <= n). */
COLD void book_grow(Book *b) {
    uint32_t old = b->cap, nw = old * 2u;
    Slot *a = NULL;
    if (posix_memalign((void **)&a, 128, (size_t)nw * sizeof(Slot)) != 0) {
        fprintf(stderr, "oom\n");
        exit(2);
    }
    memcpy(a, b->arena, (size_t)old * sizeof(Slot));
    arena_link(a, old, nw);
    uint64_t *ids = (uint64_t *)calloc(nw, sizeof(uint64_t));
    if (!ids) { fprintf(stderr, "oom\n"); exit(2); }
    memcpy(ids, b->ids, (size_t)old * sizeof(uint64_t));
    free(b->arena);
    free(b->ids);
    b->arena = a;
    b->ids = ids;
    b->cap = nw;
    b->free_head = old;
}

ALWAYS_INLINE void half_mark(Half *h, uint32_t t) {
    h->l1[t >> 6] |= 1ull << (t & 63u);
    h->l2[t >> 12] |= 1ull << ((t >> 6) & 63u);
    h->l3 |= 1ull << (t >> 12);
    h->best = t < h->best ? t : h->best; /* branchless min (csel) */
}

ALWAYS_INLINE uint32_t half_scan_min(const Half *h) {
    if (h->l3 == 0) return NIL;
    uint32_t i2 = (uint32_t)__builtin_ctzll(h->l3);
    uint32_t i1 = (i2 << 6) | (uint32_t)__builtin_ctzll(h->l2[i2]);
    return (i1 << 6) | (uint32_t)__builtin_ctzll(h->l1[i1]);
}

/* Called only on the current best tick (level just emptied). Fast path:
 * the next best is in the same l1 word — one ctz, no further memory. */
ALWAYS_INLINE void half_unmark(Half *h, uint32_t t) {
    uint32_t i1 = t >> 6;
    uint64_t v = h->l1[i1] & ~(1ull << (t & 63u));
    h->l1[i1] = v;
    if (LIKELY(v != 0)) {
        h->best = (i1 << 6) | (uint32_t)__builtin_ctzll(v);
        return;
    }
    uint64_t w2 = h->l2[t >> 12] & ~(1ull << (i1 & 63u));
    h->l2[t >> 12] = w2;
    if (w2 == 0) h->l3 &= ~(1ull << (t >> 12));
    h->best = half_scan_min(h);
}

/* Hot path: run all orders through the book. Counters live in registers
 * for the whole loop and are written back once. No allocation inside. */
static void __attribute__((noinline))
book_run(Book *restrict b, const Order *restrict ord, uint32_t n) {
    uint64_t trades = b->trades;
    uint64_t volume = b->volume;
    uint32_t free_head = b->free_head;
    uint32_t resting = b->resting;
    Slot *restrict arena = b->arena;
    uint64_t *restrict ids = b->ids;
    uint64_t base = b->base;

    for (uint32_t i = 0; i < n; i++) {
        uint64_t price = ord[i].price;
        uint64_t qty = ord[i].qty;
        uint32_t side = (uint32_t)(ord[i].side & 1u);

        uint64_t t64 = price - base;
        if (UNLIKELY(t64 >= BAND)) {
            if (!book_rebase(b, price)) { b->rejected++; continue; }
            base = b->base;
            t64 = price - base;
            if (t64 >= BAND) { b->rejected++; continue; }
        }
        uint32_t tick = (uint32_t)t64;

        /* Buy (0) consumes ASK half (0) and rests in BID (1); Sell the reverse.
         * BID half is in inverted coordinates: key = tick ^ MASK. */
        uint32_t consume = side;
        uint32_t rest_at = side ^ 1u;
        uint32_t cmask = (0u - consume) & MASK;   /* consume==BID ? MASK : 0 */
        uint32_t rmask = cmask ^ MASK;            /* rest_at==BID ? MASK : 0 */
        uint32_t ckey = tick ^ cmask;
        uint32_t rkey = tick ^ rmask;

        Half *hc = &b->h[consume];
        for (;;) {
            uint32_t best = hc->best;   /* NIL > any key: empty side exits here */
            if (qty == 0 || best > ckey) break;

            Lvl *l = &hc->lv[best];
            uint32_t head = l->head;
            uint64_t lq = l->qty;
            uint32_t freed = 0;
            while (qty > 0 && head != NIL) {
                Slot *s = &arena[head];
                uint64_t sq = s->qty;
                uint64_t m = sq < qty ? sq : qty;
                trades += 1;
                volume += m;
                qty -= m;
                sq -= m;
                lq -= m;
                s->qty = sq;
                if (sq == 0) {
                    uint32_t nx = s->next;
                    s->next = free_head;
                    free_head = head;
                    head = nx;
                    freed += 1;
                }
            }
            resting -= freed;
            l->head = head;
            l->qty = lq;
            if (head == NIL) {
                l->tail = NIL;
                half_unmark(hc, best);
            }
        }

        if (qty > 0) {
            if (UNLIKELY(free_head == NIL)) {
                b->free_head = free_head;
                book_grow(b);
                free_head = b->free_head;
                arena = b->arena;
                ids = b->ids;
            }
            uint32_t idx = free_head;
            Slot *s = &arena[idx];
            free_head = s->next;
            s->qty = qty;
            s->next = NIL;
            ids[idx] = ord[i].id;
            resting += 1;

            Half *hr = &b->h[rest_at];
            Lvl *l = &hr->lv[rkey];
            uint32_t tail = l->tail;
            l->tail = idx;
            l->qty += qty;
            if (tail == NIL) {
                l->head = idx;
                half_mark(hr, rkey);
            } else {
                arena[tail].next = idx;
            }
        }
    }

    b->trades = trades;
    b->volume = volume;
    b->free_head = free_head;
    b->resting = resting;
}

/* ------------------------------------------------- observation (cold) */

static uint32_t half_levels(const Half *h) {
    uint32_t c = 0;
    for (uint32_t i = 0; i < L1W; i++) c += (uint32_t)__builtin_popcountll(h->l1[i]);
    return c;
}

static uint64_t book_best_bid(const Book *b) {
    uint32_t k = b->h[BID].best;
    return k == NIL ? 0 : b->base + (uint64_t)(k ^ MASK);
}

static uint64_t book_best_ask(const Book *b) {
    uint32_t k = b->h[ASK].best;
    return k == NIL ? 0 : b->base + (uint64_t)k;
}

/* ------------------------------------------------------------- benchmark */

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC_RAW, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

typedef struct {
    uint64_t elapsed_ns;
    uint64_t trades, volume;
    uint32_t bid_levels, ask_levels, resting;
    uint64_t best_bid, best_ask;
} RunResult;

static RunResult run_single(uint32_t n, const Order *orders) {
    Book *b = book_new(n);          /* fresh book, outside the timer */

    uint64_t t0 = now_ns();
    book_run(b, orders, n);
    uint64_t t1 = now_ns();

    RunResult r;
    r.elapsed_ns = t1 - t0;
    r.trades = b->trades;
    r.volume = b->volume;
    r.bid_levels = half_levels(&b->h[BID]);
    r.ask_levels = half_levels(&b->h[ASK]);
    r.resting = b->resting;
    r.best_bid = book_best_bid(b);
    r.best_ask = book_best_ask(b);
    book_free(b);
    return r;
}

static uint64_t parse_hex(const char *s) {
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) s += 2;
    return strtoull(s, NULL, 16);
}

int main(int argc, char **argv) {
    uint64_t seed = 0xDEADBEEFCAFE1234ull;
    const uint32_t n = 100000u;
    uint32_t iterations = 5;
    int as_json = 0;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--iters") == 0 && i + 1 < argc) {
            iterations = (uint32_t)strtoul(argv[++i], NULL, 10);
            if (iterations == 0) iterations = 1;
        } else if (strcmp(argv[i], "--seed") == 0 && i + 1 < argc) {
            seed = parse_hex(argv[++i]);
            if (seed == 0) seed = 0xDEADBEEFCAFE1234ull;
        } else if (strcmp(argv[i], "--json") == 0) {
            as_json = 1;
        }
    }

    if (!as_json) {
        printf("========================================================\n");
        printf("             C MATCHING ENGINE BENCHMARK                \n");
        printf("========================================================\n");
        printf("Workload: %u orders per run | %u iterations\n", n, iterations);
        printf("PRNG: Deterministic Xorshift64 (seed: 0x%" PRIX64 ")\n", seed);
        printf("--------------------------------------------------------\n");
    }

    /* Warmup: 10k orders through a fresh book, not printed. */
    Order *warm = generate_orders(10000u, seed);
    (void)run_single(10000u, warm);
    free(warm);

    Order *orders = generate_orders(n, seed);

    uint64_t total_ns = 0, min_ns = UINT64_MAX;
    RunResult last;
    memset(&last, 0, sizeof last);

    for (uint32_t i = 1; i <= iterations; i++) {
        RunResult r = run_single(n, orders);
        total_ns += r.elapsed_ns;
        if (r.elapsed_ns < min_ns) min_ns = r.elapsed_ns;
        last = r;
        double ms = (double)r.elapsed_ns / 1e6;
        double ops = (double)n / ((double)r.elapsed_ns / 1e9);
        double lat = (double)r.elapsed_ns / (double)n;
        if (!as_json)
            printf("  Iteration %u: %8.3f ms | %10.2f M ops/s | %6.2f ns/order\n",
                   i, ms, ops / 1e6, lat);
    }
    free(orders);

    uint64_t avg_ns = total_ns / iterations;
    double best_ms = (double)min_ns / 1e6;
    double avg_ms = (double)avg_ns / 1e6;
    double best_ops = (double)n / ((double)min_ns / 1e9);
    double avg_ops = (double)n / ((double)avg_ns / 1e9);
    double avg_lat = (double)avg_ns / (double)n;
    uint32_t levels = last.bid_levels + last.ask_levels;

    if (as_json) {
        printf("{\"lang\":\"c\",\"orders\":%u,\"iters\":%u,\"min_ms\":%.6f,\"avg_ms\":%.6f,"
               "\"trades\":%" PRIu64 ",\"volume\":%" PRIu64 ",\"bid_levels\":%u,\"ask_levels\":%u,"
               "\"resting\":%u,\"best_bid\":%" PRIu64 ",\"best_ask\":%" PRIu64 "}\n",
               n, iterations, best_ms, avg_ms, last.trades, last.volume,
               last.bid_levels, last.ask_levels, last.resting, last.best_bid, last.best_ask);
    } else {
        printf("--------------------------------------------------------\n");
        printf("SUMMARY (C):\n");
        printf("  Best Time:         %.3f ms (%.2f M ops/s)\n", best_ms, best_ops / 1e6);
        printf("  Average Time:      %.3f ms (%.2f M ops/s)\n", avg_ms, avg_ops / 1e6);
        printf("  Average Latency:   %.2f ns/order\n", avg_lat);
        printf("  Trades Executed:   %" PRIu64 "\n", last.trades);
        printf("  Volume Matched:    %" PRIu64 "\n", last.volume);
        printf("  Resting in Book:   %u\n", last.resting);
        printf("  Active Levels:     %u (Best Bid: %" PRIu64 ", Best Ask: %" PRIu64 ")\n",
               levels, last.best_bid, last.best_ask);
        printf("========================================================\n\n");
    }

    /* Mandatory invariants on the default seed. */
    if (seed == 0xDEADBEEFCAFE1234ull) {
        int ok = last.trades == 77576ull && last.volume == 1973216ull &&
                 last.bid_levels == 55u && last.ask_levels == 50u &&
                 last.resting == 21620u && last.best_bid == 10038ull &&
                 last.best_ask == 10043ull;
        if (!ok) {
            fprintf(stderr,
                    "INVARIANT MISMATCH: trades=%" PRIu64 " volume=%" PRIu64
                    " bid_levels=%u ask_levels=%u resting=%u best_bid=%" PRIu64
                    " best_ask=%" PRIu64 "\n",
                    last.trades, last.volume, last.bid_levels, last.ask_levels,
                    last.resting, last.best_bid, last.best_ask);
            return 1;
        }
    }
    return 0;
}
