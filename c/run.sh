#!/usr/bin/env bash
# Builds (if needed) and runs the C matching engine benchmark.
# Usage: ./run.sh [--iters N] [--seed HEX] [--json]
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build
BIN=build/matching-engine-c
if [ ! -x "$BIN" ] || [ main.c -nt "$BIN" ]; then
    clang -O3 -mcpu=native -std=c11 -Wall -Wextra -o "$BIN" main.c
fi
exec "$BIN" "$@"
