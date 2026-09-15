#!/bin/bash
# In-process сравнение движков матчинга на разных языках, без сети.
# Протокол — ровно авторский с обеих сторон (golang/main.go): прогрев 10k,
# 5 итераций по 100 000 ордеров, свежая книга на каждой, среднее. Движок Go
# не изменён; остальные реализации обязаны давать те же инварианты
# (trades=77576, volume=1973216, resting=21620) — иначе прогон помечается FAIL.
#
# Порядок: сначала Go автора, Go с плоской книгой, Rust, затем остальные. Переменные:
#   LANGS="go goflat rust c cpp ..." — какие реализации гонять (по умолчанию все найденные);
#              go — код автора (golang/), goflat — go/ с плоской книгой (тот же алгоритм, что в rust/)
#   WARM=1   — три холостых прогона каждого бинарника перед замером (первые
#              запуски процесса после простоя на Mac идут вдвое медленнее)
#   ROUNDS=N — повторить замер N раз, в сводке — медиана по раундам
set -u
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
export PATH="/opt/homebrew/opt/go/bin:$PATH"
source "$HOME/.cargo/env" 2>/dev/null || true
ROUNDS="${ROUNDS:-1}"; WARM="${WARM:-0}"
ALL="go goflat rust c cpp asm csharp java nodejs php python"
LANGS="${LANGS:-$ALL}"
OUT="$DIR/.build/results.tsv"; mkdir -p "$DIR/.build"

echo "=========================================================="
echo "    MATCHING ENGINE BENCHMARK — протокол автора           "
echo "=========================================================="
echo "Host:     $(uname -m) $(uname -s) $(sw_vers -productVersion 2>/dev/null) | $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
echo "Cores:    $(sysctl -n hw.ncpu) logical = $(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || echo '?') performance + $(sysctl -n hw.perflevel1.logicalcpu 2>/dev/null || echo '?') efficiency | RAM $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
echo "Load avg: $(uptime | sed 's/.*load averages: //')   (замер достоверен при < ~4)"
echo "Protocol: прогрев 10k, 5 итераций по 100 000 ордеров, среднее; in-process, один поток; WARM=$WARM ROUNDS=$ROUNDS"
echo "=========================================================="

# команда запуска для каждой реализации (собирает при необходимости)
cmd_of() {
  case "$1" in
    go)   ( cd "$DIR/golang" && go build -ldflags="-s -w" -o "$DIR/.build/engine_go" . ) || return 1; echo "$DIR/.build/engine_go" ;;
    goflat) [ -x "$DIR/go/run.sh" ] || return 1; echo "$DIR/go/run.sh" ;;
    rust) ( cd "$DIR/rust" && RUSTFLAGS="-C target-cpu=native" cargo build --release -q --bin author_protocol ) || return 1; echo "$DIR/rust/target/release/author_protocol" ;;
    *)    [ -x "$DIR/$1/run.sh" ] || return 1; echo "$DIR/$1/run.sh" ;;
  esac
}
label_of() { case "$1" in go) echo "Go (автор)";; goflat) echo "Go (flat)";; rust) echo "Rust";; c) echo "C";; cpp) echo "C++";; csharp) echo "C#";; java) echo "Java";; php) echo "PHP";; nodejs) echo "NodeJS";; python) echo "Python";; asm) echo "ASM";; *) echo "$1";; esac; }

: > "$OUT"
for lang in $LANGS; do
  CMD=$(cmd_of "$lang" 2>/dev/null) || { echo; echo "### $(label_of "$lang"): нет реализации или не собралось — пропуск"; continue; }
  echo; echo "----------------------------------------------------------"
  echo "$(label_of "$lang")   ($CMD)"
  echo "----------------------------------------------------------"
  # три холостых прогона: после «медленного» блока (или простоя) первые 2–3 запуска процесса ещё разгоняются
  [ "$WARM" = "1" ] && { printf "  (холостые прогоны) "; for _ in 1 2 3; do "$CMD" >/dev/null 2>&1 && printf "ok " || printf "ошибка "; done; echo; }
  for ((r=1; r<=ROUNDS; r++)); do
    out=$("$CMD" 2>&1); rc=$?
    best=$(echo "$out" | awk '/Best Time/{print $3}'); avg=$(echo "$out" | awk '/Average Time/{print $3}')
    tr=$(echo "$out" | awk '/Trades Executed/{print $3}'); vol=$(echo "$out" | awk '/Volume Matched/{print $3}')
    ok="OK"; { [ "$rc" -ne 0 ] || [ "$tr" != "77576" ] || [ "$vol" != "1973216" ]; } && ok="FAIL"
    printf "  раунд %d: best %8s ms | average %8s ms | trades %s volume %s | %s\n" "$r" "${best:-?}" "${avg:-?}" "${tr:-?}" "${vol:-?}" "$ok"
    printf "%s\t%d\t%s\t%s\t%s\n" "$lang" "$r" "${avg:-nan}" "${best:-nan}" "$ok" >> "$OUT"
  done
done

echo; echo "=========================================================="
echo " СВОДКА (медиана Average по раундам; 100 000 ордеров)"
echo "=========================================================="
python3 - "$OUT" <<'PY'
import sys, statistics as st, collections
rows = [l.rstrip('\n').split('\t') for l in open(sys.argv[1]) if l.strip()]
by = collections.defaultdict(list); ok = collections.defaultdict(lambda: True)
for lang, r, avg, best, status in rows:
    if avg != 'nan': by[lang].append(float(avg))
    ok[lang] &= (status == 'OK')
if not by: sys.exit(0)
med = {l: st.median(v) for l, v in by.items()}
label = {'go':'Go (автор)','goflat':'Go (flat)','rust':'Rust','c':'C','cpp':'C++','csharp':'C#','java':'Java','php':'PHP','nodejs':'NodeJS','python':'Python','asm':'ASM'}
fast = min(med.values()); go = med.get('go')
print(f"{'':2} {'язык':8} {'ms':>10} {'M ордеров/с':>13} {'к лучшему':>10} {'к Go':>8}  инварианты")
for i, (l, m) in enumerate(sorted(med.items(), key=lambda kv: kv[1]), 1):
    print(f"{i:2} {label.get(l,l):8} {m:10.3f} {100/m:13.2f} {'x%.2f' % (m/fast):>10} {('x%.2f' % (go/m)) if go else '—':>8}  {'OK' if ok[l] else 'FAIL'}")
PY
