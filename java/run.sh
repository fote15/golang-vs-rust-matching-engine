#!/usr/bin/env bash
# Сборка (при необходимости) и запуск Java-бенчмарка. Аргументы пробрасываются: --iters N --seed HEX [--json]
set -euo pipefail
cd "$(dirname "$0")"
JDK=/opt/homebrew/opt/openjdk/bin
mkdir -p build
if [ ! -f build/Main.class ] || [ Main.java -nt build/Main.class ]; then
    "$JDK/javac" -d build Main.java
fi
exec "$JDK/java" -XX:+UseSerialGC -Xms512m -Xmx512m -XX:+AlwaysPreTouch -cp build Main "$@"
