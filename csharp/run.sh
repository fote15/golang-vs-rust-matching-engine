#!/bin/bash
# Сборка (при необходимости) и запуск C#-бенчмарка. Аргументы пробрасываются:
#   ./run.sh --iters 5 --seed DEADBEEFCAFE1234 [--json]
set -e
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
DOTNET="/opt/homebrew/opt/dotnet/bin/dotnet"
DLL="$DIR/build/bin/Release/net10.0/MatchingEngine.dll"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1

# find -newer сравнивает mtime с наносекундной точностью (в отличие от bash -nt).
if [ ! -f "$DLL" ] || [ -n "$(find "$DIR" -maxdepth 1 \( -name '*.cs' -o -name '*.csproj' -o -name '*.props' \) -newer "$DLL")" ]; then
  "$DOTNET" build "$DIR/MatchingEngine.csproj" -c Release --nologo -v quiet >/dev/null
fi

exec "$DOTNET" "$DLL" "$@"
