# C matching engine

Build & run: `./run.sh [--iters N] [--seed HEX] [--json]` (compiles `main.c` with
`clang -O3 -mcpu=native -std=c11` into `build/matching-engine-c` when missing or stale).

Data structure: flat price band of 8192 ticks with 16-byte levels indexed directly by tick,
BID side stored in inverted tick coordinates so both sides share one branch-free matching loop,
three-level occupancy bitmap (best price = chained trailing-zero counts), and an order arena of
16-byte slots on u32 indices with an intrusive free list. No allocation inside the timed loop.
Same Xorshift64 generator, semantics, protocol (10k warmup, fresh book per iteration) and output
format as `golang/main.go`; default-seed invariants are asserted and mismatch exits non-zero.
