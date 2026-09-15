#!/bin/bash
set -e

DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
source "$HOME/.cargo/env" 2>/dev/null || true

DURATION="${1:-4.0}"

echo "=========================================================="
echo "          FIBONACCI 4-SECOND BENCHMARK COMPARISON         "
echo "                     Go vs. Rust                          "
echo "=========================================================="
echo "Host Machine:      $(uname -m) - $(uname -s)"
echo "Go Version:        $(go version)"
echo "Rust Version:      $(rustc --version)"
echo "Benchmark Window:  ${DURATION}s"
echo "=========================================================="
echo ""

echo ">>> Building Go Fibonacci (optimized)..."
cd "$DIR/golang"
go build -ldflags="-s -w" -o fib_go .

echo ">>> Building Rust Fibonacci (release mode)..."
cd "$DIR/rust"
RUSTFLAGS="-C target-cpu=native" cargo build --release -q

echo ""
echo "----------------------------------------------------------"
echo "1. RUNNING GO FIBONACCI BENCHMARK"
echo "----------------------------------------------------------"
cd "$DIR/golang"
./fib_go "$DURATION"

echo "----------------------------------------------------------"
echo "2. RUNNING RUST FIBONACCI BENCHMARK"
echo "----------------------------------------------------------"
cd "$DIR/rust"
./target/release/fibonacci-rust "$DURATION"

echo "=========================================================="
echo "                   BENCHMARK COMPLETE                     "
echo "=========================================================="
