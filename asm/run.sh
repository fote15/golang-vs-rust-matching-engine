#!/bin/sh
# Сборка (при необходимости) и запуск бенчмарка ASM. Аргументы пробрасываются: --iters N --seed HEX [--json]
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/build/matching-engine-asm"
mkdir -p "$DIR/build"
if [ ! -x "$BIN" ] || [ "$DIR/main.c" -nt "$BIN" ] || [ "$DIR/engine.s" -nt "$BIN" ]; then
    clang -O3 -mcpu=native -std=c11 -o "$BIN" "$DIR/main.c" "$DIR/engine.s"
fi
exec "$BIN" "$@"
