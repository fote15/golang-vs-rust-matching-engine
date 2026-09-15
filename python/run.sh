#!/usr/bin/env bash
# Запуск бенчмарка. Байткод модулей компилируется при первом импорте в python/build/.
# Аргументы пробрасываются: ./run.sh --iters 5 --seed DEADBEEFCAFE1234 [--json]
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PYTHONPYCACHEPREFIX="$DIR/build"
exec "${PYTHON:-python3}" -OO "$DIR/main.py" "$@"
