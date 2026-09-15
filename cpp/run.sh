#!/usr/bin/env bash
# Builds (if needed) and runs the C++ matching engine benchmark.
# Usage: ./run.sh [--iters N] [--seed HEX] [--json]
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/build/matching-engine-cpp"
SRC="$DIR/main.cpp"
mkdir -p "$DIR/build"
if [[ ! -x "$BIN" || "$SRC" -nt "$BIN" ]]; then
  clang++ -O3 -std=c++20 -mcpu=native -fno-exceptions -fno-rtti -fomit-frame-pointer \
    -o "$BIN" "$SRC"
fi
exec "$BIN" "$@"
