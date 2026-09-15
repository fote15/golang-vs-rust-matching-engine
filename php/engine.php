<?php
declare(strict_types=1);

/*
 * PHP MATCHING ENGINE BENCHMARK — та же семантика, генератор и протокол, что у
 * golang/main.go и rust/src/bin/author_protocol.rs.
 *
 * Структура книги повторяет rust/src/orderbook.rs, но в «плоском» PHP:
 *  - полоса цен 8192 тика от базы (base = first_price - 4096), tick = price - base;
 *  - на сторону — packed-массивы уровней head / tail / qty (индекс = тик);
 *  - bid-сторона хранится в инвертированных координатах (key = tick ^ 8191), поэтому
 *    лучшая цена обеих сторон — всегда МИНИМАЛЬНЫЙ занятый ключ;
 *  - трёхуровневая битовая карта занятости на 32-битных словах (l1: 256 слов,
 *    l2: 8 слов, l3: одно слово) + кэш best; trailing zeros — через `w & -w` и LUT
 *    степеней двойки (32 бита на слово, чтобы `-w` никогда не переполнял int);
 *  - арена ордеров: три packed-массива (qty / next / id) на целочисленных индексах,
 *    интрузивный free-list; ни одного объекта на ордер, ни одной аллокации в таймере.
 *
 * Весь горячий цикл живёт в одной функции на локальных переменных: обращение к
 * свойствам объекта и вызовы функций в PHP стоят дороже, чем сам матчинг.
 */

const BAND_BITS = 13;
const BAND      = 1 << BAND_BITS;   // 8192 тиков
const MASK      = BAND - 1;         // 8191: инверсия координат bid-стороны
const L1W       = BAND >> 5;        // 256 слов по 32 бита
const L2W       = L1W >> 5;         // 8 слов по 32 бита
const NIL       = 0x7FFFFFFF;       // «нет узла» / «сторона пуста» (> любого ключа)

const DEFAULT_SEED_HEX = 'DEADBEEFCAFE1234';
const N_ORDERS   = 100000;
const WARMUP_N   = 10000;

// ---------------------------------------------------------------------------
// Генератор: Xorshift64 на знаковых 64-битных int с масками. Вне таймера.
// ---------------------------------------------------------------------------

/** Разбор до 16 hex-цифр в 64-битное значение с переполнением (без float). */
function parse_hex64(string $s): int
{
    $s = strtoupper($s);
    if (str_starts_with($s, '0X')) {
        $s = substr($s, 2);
    }
    if ($s === '' || strlen($s) > 16 || !ctype_xdigit($s)) {
        fwrite(STDERR, "bad --seed: expected 1..16 hex digits\n");
        exit(2);
    }
    $v = 0;
    for ($i = 0, $n = strlen($s); $i < $n; ++$i) {
        $v = ($v << 4) | intval($s[$i], 16);
    }
    return $v;
}

/**
 * Беззнаковый остаток 64-битного значения (хранимого как знаковый int) по малому
 * модулю: u64 = hi*2^32 + lo, обе половины неотрицательны. Без ветвления — одна
 * формула для любого знака.
 */
function umod64(int $v, int $m): int
{
    $hi = ($v >> 32) & 0xFFFFFFFF;
    $lo = $v & 0xFFFFFFFF;
    return (($hi % $m) * (4294967296 % $m) + ($lo % $m)) % $m;
}

/**
 * @return array{0: int[], 1: int[], 2: int[]} side[], price[], qty[] — packed-массивы int.
 */
function generate_orders(int $n, int $seed): array
{
    if ($seed === 0) {
        $seed = parse_hex64(DEFAULT_SEED_HEX);
    }
    $side  = array_fill(0, $n, 0);
    $price = array_fill(0, $n, 0);
    $qty   = array_fill(0, $n, 0);
    $v = $seed;
    for ($i = 0; $i < $n; ++$i) {
        $v ^= $v << 13;
        $v ^= ($v >> 7) & 0x01FFFFFFFFFFFFFF;   // логический сдвиг вправо на 7
        $v ^= $v << 17;
        $r1 = $v;
        $v ^= $v << 13;
        $v ^= ($v >> 7) & 0x01FFFFFFFFFFFFFF;
        $v ^= $v << 17;
        $r2 = $v;
        $v ^= $v << 13;
        $v ^= ($v >> 7) & 0x01FFFFFFFFFFFFFF;
        $v ^= $v << 17;
        $r3 = $v;

        $side[$i]  = $r1 & 1;                       // 0 = Buy, 1 = Sell
        $price[$i] = 10000 + umod64($r2, 200) - 100; // 9900..10099
        $qty[$i]   = 1 + umod64($r3, 100);           // 1..100
    }
    return [$side, $price, $qty];
}

/**
 * Холодный путь: удвоить арену (вызывается только если ордеров больше ёмкости книги;
 * при ёмкости n на n ордеров недостижимо — каждый ордер занимает не более одного слота).
 *
 * @param int[] $slotQ @param int[] $slotN @param int[] $slotId
 * @return array{0: int[], 1: int[], 2: int[], 3: int} массивы и индекс первого нового слота
 */
function grow_arena(array $slotQ, array $slotN, array $slotId): array
{
    $old = count($slotQ);
    for ($g = 0; $g < $old; ++$g) {
        $slotQ[]  = 0;
        $slotN[]  = $old + $g + 1;
        $slotId[] = 0;
    }
    $slotN[$old + $old - 1] = NIL;
    return [$slotQ, $slotN, $slotId, $old];
}

// ---------------------------------------------------------------------------
// Одна итерация замера: свежая книга (вне таймера), затем n ордеров в таймере.
// ---------------------------------------------------------------------------

/**
 * @param int[] $S сторона, @param int[] $P цена, @param int[] $Q количество
 * @return array{elapsed_ns:int, trades:int, volume:int, resting:int,
 *               bid_levels:int, ask_levels:int, best_bid:int, best_ask:int, rejected:int}
 */
function run_book(array $S, array $P, array $Q, int $n): array
{
    // ---- свежая книга: вся память выделяется здесь, до старта таймера ----
    $cap = $n < 64 ? 64 : $n;

    // арена ордеров (SoA): qty / next / id
    $slotQ  = array_fill(0, $cap, 0);
    $slotN  = range(1, $cap);          // intrusive free-list: i -> i+1
    $slotN[$cap - 1] = NIL;
    $slotId = array_fill(0, $cap, 0);
    $free   = 0;

    // ask-сторона (прямые координаты тика)
    $aHead = array_fill(0, BAND, NIL);
    $aTail = array_fill(0, BAND, NIL);
    $aQty  = array_fill(0, BAND, 0);
    $aL1   = array_fill(0, L1W, 0);
    $aL2   = array_fill(0, L2W, 0);
    $aL3   = 0;
    $abest = NIL;

    // bid-сторона (инвертированные координаты: key = tick ^ MASK)
    $bHead = array_fill(0, BAND, NIL);
    $bTail = array_fill(0, BAND, NIL);
    $bQty  = array_fill(0, BAND, 0);
    $bL1   = array_fill(0, L1W, 0);
    $bL2   = array_fill(0, L2W, 0);
    $bL3   = 0;
    $bbest = NIL;

    // LUT: изолированный младший бит (степень двойки) -> его номер
    $CTZ = [];
    for ($b = 0; $b < 32; ++$b) {
        $CTZ[1 << $b] = $b;
    }

    $base     = 0;
    $resting  = 0;
    $trades   = 0;
    $vol      = 0;
    $rejected = 0;

    // ---- таймер: только цикл обработки ордеров ----
    $t0 = hrtime(true);

    for ($i = 0; $i < $n; ++$i) {
        $qty  = $Q[$i];
        $tick = $P[$i] - $base;

        if ($qty === 0) {                       // холодный путь: нулевое количество
            ++$rejected;
            continue;
        }
        if ($tick < 0 || $tick > MASK) {        // холодный путь: цена вне полосы
            if ($resting !== 0) {               // сдвиг полосы только на пустой книге
                ++$rejected;
                continue;
            }
            $base = $P[$i] - (BAND >> 1);
            if ($base < 0) {
                $base = 0;
            }
            $tick = $P[$i] - $base;
        }

        if ($S[$i] === 0) {
            // ================= BUY: ест asks, остаток ложится в bids =================
            while ($abest <= $tick) {
                $t    = $abest;
                $head = $aHead[$t];
                $lq   = $aQty[$t];
                do {
                    $mq = $slotQ[$head];
                    ++$trades;
                    if ($mq <= $qty) {                  // maker исполнен целиком
                        $vol += $mq;
                        $qty -= $mq;
                        $lq  -= $mq;
                        $nx  = $slotN[$head];
                        $slotN[$head] = $free;          // слот -> free-list
                        $free = $head;
                        $head = $nx;
                        --$resting;
                        if ($head === NIL) {
                            break;
                        }
                    } else {                            // taker исполнен целиком
                        $vol += $qty;
                        $slotQ[$head] = $mq - $qty;
                        $lq  -= $qty;
                        $qty  = 0;
                        break;
                    }
                } while ($qty !== 0);
                $aQty[$t] = $lq;
                if ($head === NIL) {
                    // уровень опустел: снять бит, найти следующий минимум
                    $aHead[$t] = NIL;
                    $aTail[$t] = NIL;
                    $i1 = $t >> 5;
                    $w  = $aL1[$i1] & ~(1 << ($t & 31));
                    $aL1[$i1] = $w;
                    if ($w !== 0) {
                        $abest = ($i1 << 5) | $CTZ[$w & -$w];
                    } else {
                        $i2 = $i1 >> 5;
                        $w2 = $aL2[$i2] & ~(1 << ($i1 & 31));
                        $aL2[$i2] = $w2;
                        if ($w2 !== 0) {
                            $j1 = ($i2 << 5) | $CTZ[$w2 & -$w2];
                            $w1 = $aL1[$j1];
                            $abest = ($j1 << 5) | $CTZ[$w1 & -$w1];
                        } else {
                            $aL3 &= ~(1 << $i2);
                            if ($aL3 !== 0) {
                                $i2 = $CTZ[$aL3 & -$aL3];
                                $w2 = $aL2[$i2];
                                $j1 = ($i2 << 5) | $CTZ[$w2 & -$w2];
                                $w1 = $aL1[$j1];
                                $abest = ($j1 << 5) | $CTZ[$w1 & -$w1];
                            } else {
                                $abest = NIL;
                            }
                        }
                    }
                    if ($qty === 0) {
                        break;
                    }
                } else {
                    $aHead[$t] = $head;                 // qty === 0
                    break;
                }
            }
            if ($qty !== 0) {
                // остаток — в хвост очереди bid-уровня
                $k    = $tick ^ MASK;
                $node = $free;
                if ($node === NIL) {                    // холодный путь: арена исчерпана
                    [$slotQ, $slotN, $slotId, $node] = grow_arena($slotQ, $slotN, $slotId);
                }
                $free = $slotN[$node];
                $slotQ[$node]  = $qty;
                $slotN[$node]  = NIL;
                $slotId[$node] = $i + 1;
                ++$resting;
                $tail = $bTail[$k];
                $bTail[$k] = $node;
                $bQty[$k] += $qty;
                if ($tail === NIL) {
                    $bHead[$k] = $node;
                    $bL1[$k >> 5]  |= 1 << ($k & 31);
                    $bL2[$k >> 10] |= 1 << (($k >> 5) & 31);
                    $bL3 |= 1 << ($k >> 10);
                    if ($k < $bbest) {
                        $bbest = $k;
                    }
                } else {
                    $slotN[$tail] = $node;
                }
            }
        } else {
            // ================= SELL: ест bids, остаток ложится в asks =================
            $k = $tick ^ MASK;
            while ($bbest <= $k) {
                $t    = $bbest;
                $head = $bHead[$t];
                $lq   = $bQty[$t];
                do {
                    $mq = $slotQ[$head];
                    ++$trades;
                    if ($mq <= $qty) {
                        $vol += $mq;
                        $qty -= $mq;
                        $lq  -= $mq;
                        $nx  = $slotN[$head];
                        $slotN[$head] = $free;
                        $free = $head;
                        $head = $nx;
                        --$resting;
                        if ($head === NIL) {
                            break;
                        }
                    } else {
                        $vol += $qty;
                        $slotQ[$head] = $mq - $qty;
                        $lq  -= $qty;
                        $qty  = 0;
                        break;
                    }
                } while ($qty !== 0);
                $bQty[$t] = $lq;
                if ($head === NIL) {
                    $bHead[$t] = NIL;
                    $bTail[$t] = NIL;
                    $i1 = $t >> 5;
                    $w  = $bL1[$i1] & ~(1 << ($t & 31));
                    $bL1[$i1] = $w;
                    if ($w !== 0) {
                        $bbest = ($i1 << 5) | $CTZ[$w & -$w];
                    } else {
                        $i2 = $i1 >> 5;
                        $w2 = $bL2[$i2] & ~(1 << ($i1 & 31));
                        $bL2[$i2] = $w2;
                        if ($w2 !== 0) {
                            $j1 = ($i2 << 5) | $CTZ[$w2 & -$w2];
                            $w1 = $bL1[$j1];
                            $bbest = ($j1 << 5) | $CTZ[$w1 & -$w1];
                        } else {
                            $bL3 &= ~(1 << $i2);
                            if ($bL3 !== 0) {
                                $i2 = $CTZ[$bL3 & -$bL3];
                                $w2 = $bL2[$i2];
                                $j1 = ($i2 << 5) | $CTZ[$w2 & -$w2];
                                $w1 = $bL1[$j1];
                                $bbest = ($j1 << 5) | $CTZ[$w1 & -$w1];
                            } else {
                                $bbest = NIL;
                            }
                        }
                    }
                    if ($qty === 0) {
                        break;
                    }
                } else {
                    $bHead[$t] = $head;
                    break;
                }
            }
            if ($qty !== 0) {
                $node = $free;
                if ($node === NIL) {                    // холодный путь: арена исчерпана
                    [$slotQ, $slotN, $slotId, $node] = grow_arena($slotQ, $slotN, $slotId);
                }
                $free = $slotN[$node];
                $slotQ[$node]  = $qty;
                $slotN[$node]  = NIL;
                $slotId[$node] = $i + 1;
                ++$resting;
                $tail = $aTail[$tick];
                $aTail[$tick] = $node;
                $aQty[$tick] += $qty;
                if ($tail === NIL) {
                    $aHead[$tick] = $node;
                    $aL1[$tick >> 5]  |= 1 << ($tick & 31);
                    $aL2[$tick >> 10] |= 1 << (($tick >> 5) & 31);
                    $aL3 |= 1 << ($tick >> 10);
                    if ($tick < $abest) {
                        $abest = $tick;
                    }
                } else {
                    $slotN[$tail] = $node;
                }
            }
        }
    }

    $t1 = hrtime(true);

    // ---- наблюдение за состоянием (вне таймера) ----
    $bidLevels = 0;
    $askLevels = 0;
    for ($w = 0; $w < L1W; ++$w) {
        for ($x = $bL1[$w]; $x !== 0; $x &= $x - 1) {
            ++$bidLevels;
        }
        for ($x = $aL1[$w]; $x !== 0; $x &= $x - 1) {
            ++$askLevels;
        }
    }

    return [
        'elapsed_ns' => $t1 - $t0,
        'trades'     => $trades,
        'volume'     => $vol,
        'resting'    => $resting,
        'bid_levels' => $bidLevels,
        'ask_levels' => $askLevels,
        'best_bid'   => $bbest === NIL ? 0 : $base + ($bbest ^ MASK),
        'best_ask'   => $abest === NIL ? 0 : $base + $abest,
        'rejected'   => $rejected,
    ];
}

// ---------------------------------------------------------------------------
// Протокол автора: warmup 10k, затем ITERS итераций по 100k на свежей книге.
// ---------------------------------------------------------------------------

function main(array $argv): int
{
    $iterations = 5;
    $seedHex    = DEFAULT_SEED_HEX;
    $json       = false;
    for ($i = 1, $c = count($argv); $i < $c; ++$i) {
        switch ($argv[$i]) {
            case '--iters':
                $iterations = max(1, (int)($argv[++$i] ?? 5));
                break;
            case '--seed':
                $seedHex = (string)($argv[++$i] ?? DEFAULT_SEED_HEX);
                break;
            case '--json':
                $json = true;
                break;
            default:
                fwrite(STDERR, "usage: engine.php [--iters N] [--seed HEX] [--json]\n");
                return 2;
        }
    }
    $seed = parse_hex64($seedHex);
    if ($seed === 0) {
        $seed = parse_hex64(DEFAULT_SEED_HEX);
    }
    $isDefaultSeed = ($seed === parse_hex64(DEFAULT_SEED_HEX));
    $n = N_ORDERS;

    if (!$json) {
        echo "========================================================\n";
        echo "             PHP MATCHING ENGINE BENCHMARK\n";
        echo "========================================================\n";
        printf("Workload: %d orders per run | %d iterations\n", $n, $iterations);
        printf("PRNG: Deterministic Xorshift64 (seed: 0x%X)\n", $seed);
        echo "--------------------------------------------------------\n";
    }

    // warmup — как у автора
    [$ws, $wp, $wq] = generate_orders(WARMUP_N, $seed);
    run_book($ws, $wp, $wq, WARMUP_N);
    unset($ws, $wp, $wq);

    [$S, $P, $Q] = generate_orders($n, $seed);

    $total = 0;
    $min   = PHP_INT_MAX;
    $times = [];
    $last  = null;
    for ($it = 1; $it <= $iterations; ++$it) {
        $r = run_book($S, $P, $Q, $n);
        $ns = $r['elapsed_ns'];
        $total += $ns;
        if ($ns < $min) {
            $min = $ns;
        }
        $times[] = $ns;
        $last = $r;
        if (!$json) {
            printf(
                "  Iteration %d: %8.3f ms | %10.2f M ops/s | %6.2f ns/order\n",
                $it,
                $ns / 1e6,
                $n / ($ns / 1e9) / 1e6,
                $ns / $n
            );
        }
    }

    $avg        = intdiv($total, $iterations);
    $avgOps     = $n / ($avg / 1e9);
    $bestOps    = $n / ($min / 1e9);
    $avgLatency = $avg / $n;
    $levels     = $last['bid_levels'] + $last['ask_levels'];

    if ($json) {
        sort($times);
        $pct = static function (array $s, float $p): int {
            $idx = (int)ceil($p * count($s)) - 1;
            return $s[max(0, min(count($s) - 1, $idx))];
        };
        $median = $pct($times, 0.5);
        echo json_encode([
            'lang'                 => 'php',
            'orders'               => $n,
            'iters'                => $iterations,
            'books_per_iter'       => 1,
            'min_ms'               => $min / 1e6,
            'median_ms'            => $median / 1e6,
            'p95_ms'               => $pct($times, 0.95) / 1e6,
            'p99_ms'               => $pct($times, 0.99) / 1e6,
            'max_ms'               => $times[count($times) - 1] / 1e6,
            'median_mops'          => $n / ($median / 1e9) / 1e6,
            'median_ns_per_order'  => $median / $n,
            'trades'               => $last['trades'],
            'volume'               => $last['volume'],
            'bid_levels'           => $last['bid_levels'],
            'ask_levels'           => $last['ask_levels'],
            'resting'              => $last['resting'],
        ]), "\n";
    } else {
        echo "--------------------------------------------------------\n";
        echo "SUMMARY (PHP):\n";
        printf("  Best Time:         %.3f ms (%.2f M ops/s)\n", $min / 1e6, $bestOps / 1e6);
        printf("  Average Time:      %.3f ms (%.2f M ops/s)\n", $avg / 1e6, $avgOps / 1e6);
        printf("  Average Latency:   %.2f ns/order\n", $avgLatency);
        printf("  Trades Executed:   %d\n", $last['trades']);
        printf("  Volume Matched:    %d\n", $last['volume']);
        printf("  Resting in Book:   %d\n", $last['resting']);
        printf(
            "  Active Levels:     %d (Best Bid: %d, Best Ask: %d)\n",
            $levels,
            $last['best_bid'],
            $last['best_ask']
        );
        echo "========================================================\n\n";
    }

    if ($isDefaultSeed) {
        $expected = [
            'trades'     => 77576,
            'volume'     => 1973216,
            'bid_levels' => 55,
            'ask_levels' => 50,
            'resting'    => 21620,
            'best_bid'   => 10038,
            'best_ask'   => 10043,
        ];
        foreach ($expected as $key => $want) {
            if ($last[$key] !== $want) {
                fwrite(STDERR, "INVARIANT VIOLATED: $key = {$last[$key]}, expected $want\n");
                return 1;
            }
        }
    }
    return 0;
}

exit(main($argv));
