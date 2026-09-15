#!/usr/bin/env bash
# Запуск Node.js-реализации. Сборка не нужна (чистый JS, без зависимостей).
# Аргументы пробрасываются как есть: --iters N --seed HEX [--json]
set -euo pipefail
cd "$(dirname "$0")"
command -v node >/dev/null 2>&1 || { echo "node not found in PATH" >&2; exit 127; }
exec node main.js "$@"
