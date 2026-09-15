# C++ matching engine
Build & run: `./run.sh [--iters N] [--seed HEX] [--json]` — compiles `main.cpp` with `clang++ -O3 -std=c++20 -mcpu=native -fno-exceptions -fno-rtti` into `build/matching-engine-cpp`, zero dependencies.
Same Xorshift64 generator, semantics and measurement protocol as `golang/main.go`; exits non-zero if the default-seed invariants (trades 77576, volume 1973216, 55/50 levels, resting 21620) do not hold.
Data structure mirrors `rust/src/orderbook.rs`: flat array of 16-byte price levels indexed directly by tick (band of 8192 ticks),
three-level occupancy bitmap with best price via trailing zeros (bids in inverted tick coordinates, so both sides share one branchless loop),
order arena on u32 indices with an intrusive free-list; all memory is allocated when the book is built, nothing is allocated inside the timed loop.
