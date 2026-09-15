#!/bin/bash
# Go с плоской книгой (тот же алгоритм, что в rust/): сборка при необходимости и запуск.
cd "$(dirname "$0")" || exit 1
export PATH="/opt/homebrew/opt/go/bin:$PATH"
mkdir -p build
if [ ! -x build/engine ] || [ -n "$(find . -maxdepth 1 -name '*.go' -newer build/engine)" ]; then
  go build -ldflags="-s -w" -o build/engine . || exit 1
fi
exec build/engine "$@"
