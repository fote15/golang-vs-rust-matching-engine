// engine.s — ядро матчинга лимитных ордеров, ARM64 (Apple Silicon), синтаксис Apple clang `as`.
//
// Структура книги повторяет rust/src/orderbook.rs: плоская полоса 8192 тиков на каждую
// сторону, уровни по 16 байт (qty u64, head u32, tail u32), трёхуровневая битовая карта
// занятости (l1[128] / l2[2] / l3), арена ордеров на 32-битных индексах с интрузивным
// free-list. Сторона BID хранится в инвертированных координатах (key = tick ^ 8191),
// поэтому лучшая цена обеих сторон — всегда минимум, а матчинг — один цикл без ветки по стороне.
//
// void engine_run(Book *book, const Order *orders, uint64_t n)
//   Order  = { u64 id @0, u64 price @8, u64 qty @16, u8 side @24, pad[7] }   (32 байта, как в Go/Rust)
//   Book   — раскладка ниже, зеркалится _Static_assert'ами в main.c.
//
// Все счётчики книги (free_head, trades, volume, resting, best обеих сторон, base) живут в
// регистрах на протяжении всего прогона и записываются обратно один раз в эпилоге.
// В горячем цикле нет ни одного вызова, ни одной аллокации, ни одного обращения к стеку.

// ---- раскладка Book ----
.set BOOK_BASE,      0      // u64  нижняя граница полосы: tick = price - base
.set BOOK_FREE,      8      // u32  голова free-list арены (NIL = 0xFFFFFFFF)
.set BOOK_RESTING,   12     // u32  число покоящихся ордеров
.set BOOK_TRADES,    16     // u64
.set BOOK_VOLUME,    24     // u64
.set BOOK_REJECTED,  32     // u64
.set BOOK_PINNED,    40     // u64  полоса привязана к рынку (0/1)
.set BOOK_ARENA,     48     // Slot*   { u64 qty, u32 next, u32 pad }
.set BOOK_IDS,       56     // u32*    холодные id ордеров
.set BOOK_H0,        128    // Half ASK (прямые тики)
.set HALF_SIZE,      0x20480
.set BOOK_H1,        BOOK_H0 + HALF_SIZE   // Half BID (инвертированные тики)

// ---- раскладка Half ----
.set H_LV,   0          // Lvl lv[8192], 16 байт каждый
.set H_L1,   0x20000    // u64 l1[128]
.set H_L2,   0x20400    // u64 l2[2]
.set H_L3,   0x20410    // u64 l3
.set H_BEST, 0x20418    // u32 best (NIL если сторона пуста)

.set BAND,   8192
.set MASK,   8191

// ---- распределение регистров ----
//  x19 arena        x20 cursor        x21 end           x22 free_head
//  x23 trades       x24 volume        x25 resting       x26 best[ASK]
//  x27 best[BID]    x28 base          x15 ids           x16 half0   x17 half1
//  на ордер:  x9 id   x10 ckey   x11 qty   x12 side   x13 bestC   x14 bestR   x7 halfC   x6 halfR
//  scratch:   x0..x5, x8   (x2 = price от загрузки ордера до Lcold_oob/Lrebase включительно)

    .text
    .globl  _engine_run
    .p2align 6
_engine_run:
    stp     x29, x30, [sp, #-96]!
    mov     x29, sp
    stp     x19, x20, [sp, #16]
    stp     x21, x22, [sp, #32]
    stp     x23, x24, [sp, #48]
    stp     x25, x26, [sp, #64]
    stp     x27, x28, [sp, #80]

    mov     x20, x1                         // cursor
    add     x21, x20, x2, lsl #5            // end = orders + n * 32
    add     x16, x0, #BOOK_H0
    add     x17, x16, #0x20000
    add     x17, x17, #HALF_SIZE - 0x20000
    ldr     x28, [x0, #BOOK_BASE]
    ldr     w22, [x0, #BOOK_FREE]
    ldr     w25, [x0, #BOOK_RESTING]
    ldp     x23, x24, [x0, #BOOK_TRADES]
    ldp     x19, x15, [x0, #BOOK_ARENA]
    add     x1, x16, #H_L1
    ldr     w26, [x1, #H_BEST - H_L1]
    add     x1, x17, #H_L1
    ldr     w27, [x1, #H_BEST - H_L1]
    cbz     x2, Lend

    .p2align 5
Lloop:
    ldp     x9, x2, [x20]                   // id @0, price @8
    ldr     x11, [x20, #16]                 // qty @16
    ldrb    w12, [x20, #24]                 // side @24 (u8): 0 = Buy, 1 = Sell
    add     x20, x20, #32                   // sizeof(Order) == 32
    sub     x0, x2, x28                     // tick = price - base
    cmp     x0, #BAND
    b.hs    Lcold_oob
Ltick_ok:
    cbz     x11, Lreject                    // qty == 0 — как в Rust: отказ
    neg     x1, x12
    and     x1, x1, #MASK                   // cmask = side ? MASK : 0
    eor     x10, x0, x1                     // ckey: ключ в координатах поедаемой стороны
    cmp     x12, #0
    csel    x13, x27, x26, ne               // bestC = side ? best[BID] : best[ASK]
    csel    x14, x26, x27, ne               // bestR
    csel    x7,  x17, x16, ne               // halfC
    csel    x6,  x16, x17, ne               // halfR
    cmp     x13, x10                        // NIL (0xFFFFFFFF) > любого ключа → пустая сторона выходит тем же сравнением
    b.hi    Lrest

// ---- take: съесть ликвидность с лучшего уровня bestC ----
Ltake:
    add     x0, x7, x13, lsl #4             // &lv[bestC]
    // Загрузки той же ширины, что и записи: ldp поверх двух str-ов ломает store-to-load forwarding.
    ldr     x1, [x0]                        // level qty
    ldr     w3, [x0, #8]                    // head (не NIL: уровень занят)
Ltake_loop:
    add     x5, x19, x3, lsl #4             // &arena[head]
    ldr     x2, [x5]                        // slot qty
    ldr     w4, [x5, #8]                    // next
    cmp     x2, x11
    csel    x8, x2, x11, lo                 // m = min(maker, taker)
    add     x23, x23, #1                    // trades++
    add     x24, x24, x8                    // volume += m
    sub     x11, x11, x8                    // taker -= m
    sub     x1, x1, x8                      // level qty -= m
    subs    x2, x2, x8                      // maker -= m
    b.ne    Ltake_partial
    // maker исполнен целиком: слот в free-list
    str     w22, [x5, #8]                   // slot.next = free_head
    mov     x22, x3                         // free_head = head
    sub     x25, x25, #1                    // resting--
    mov     w3, w4                          // head = next
    cbz     x11, Ltake_done
    cmn     w3, #1
    b.ne    Ltake_loop                      // есть ещё maker и taker не пуст
Llevel_empty:
    // уровень опустел: qty = 0, head = tail = NIL, снять бит
    mov     x2, #-1
    stp     xzr, x2, [x0]
    // unmark(bestC) на halfC; результат — новый bestC
    lsr     x5, x13, #6
    add     x0, x7, #H_L1
    ldr     x1, [x0, x5, lsl #3]
    mov     x2, #1
    lsl     x2, x2, x13
    bic     x1, x1, x2
    str     x1, [x0, x5, lsl #3]
    cbz     x1, Lunmark_slow
    rbit    x1, x1
    clz     x1, x1
    orr     x13, x1, x5, lsl #6             // быстрый путь: следующая лучшая — в том же слове
Lunmark_done:
    cbz     x11, Lorder_done
    cmp     x13, x10
    b.ls    Ltake
    b       Lrest

Ltake_partial:
    // maker частично исполнен ⇒ taker == 0; head = этот слот (предыдущие могли быть освобождены)
    str     x2, [x5]
    str     x1, [x0]
    str     w3, [x0, #8]
    b       Lorder_done

Ltake_done:
    // taker == 0 после освобождения ≥1 слота
    cmn     w3, #1
    b.eq    Llevel_empty
    str     x1, [x0]
    str     w3, [x0, #8]
    b       Lorder_done

// ---- rest: остаток кладётся в хвост уровня rkey на halfR ----
Lrest:
    cmn     w22, #1
    b.eq    Lcold_nofree
    mov     x3, x22                         // idx = free_head
    add     x5, x19, x3, lsl #4             // &arena[idx]
    ldr     w22, [x5, #8]                   // free_head = slot.next
    mov     w2, #-1                         // next = NIL, pad = 0
    stp     x11, x2, [x5]                   // slot = { qty, NIL }
    str     w9, [x15, x3, lsl #2]           // ids[idx] = id (холодно)
    add     x25, x25, #1                    // resting++
    eor     x8, x10, #MASK                  // rkey = ckey ^ MASK
    add     x0, x6, x8, lsl #4              // &lv[rkey]
    ldr     x1, [x0]
    ldr     w4, [x0, #12]                   // tail
    add     x1, x1, x11
    cmn     w4, #1
    b.eq    Lnew_level
    str     x1, [x0]                        // level qty
    str     w3, [x0, #12]                   // tail = idx
    add     x5, x19, x4, lsl #4
    str     w3, [x5, #8]                    // arena[old tail].next = idx
    b       Lorder_done
Lnew_level:
    orr     x2, x3, x3, lsl #32             // head = tail = idx
    stp     x1, x2, [x0]
    // mark(rkey)
    lsr     x5, x8, #6
    add     x0, x6, #H_L1
    ldr     x1, [x0, x5, lsl #3]
    mov     x2, #1
    lsl     x2, x2, x8
    orr     x4, x1, x2
    str     x4, [x0, x5, lsl #3]
    cmp     x8, x14
    csel    x14, x8, x14, lo                // bestR = min(bestR, rkey), branchless
    cbnz    x1, Lorder_done                 // слово l1 уже было занято ⇒ l2/l3 биты стоят
    lsr     x4, x8, #12
    add     x0, x0, #0x400                  // &l2
    ldr     x1, [x0, x4, lsl #3]
    mov     x2, #1
    lsl     x2, x2, x5
    orr     x1, x1, x2
    str     x1, [x0, x4, lsl #3]
    ldr     x1, [x0, #0x10]                 // l3
    mov     x2, #1
    lsl     x2, x2, x4
    orr     x1, x1, x2
    str     x1, [x0, #0x10]

Lorder_done:
    cmp     x12, #0
    csel    x26, x14, x13, ne               // Buy: best[ASK] = bestC ; Sell: best[ASK] = bestR
    csel    x27, x13, x14, ne
Lnext:
    cmp     x20, x21
    b.lo    Lloop

Lend:
    sub     x0, x16, #BOOK_H0
    str     x28, [x0, #BOOK_BASE]
    str     w22, [x0, #BOOK_FREE]
    str     w25, [x0, #BOOK_RESTING]
    stp     x23, x24, [x0, #BOOK_TRADES]
    add     x1, x16, #H_L1
    str     w26, [x1, #H_BEST - H_L1]
    add     x1, x17, #H_L1
    str     w27, [x1, #H_BEST - H_L1]
    ldp     x19, x20, [sp, #16]
    ldp     x21, x22, [sp, #32]
    ldp     x23, x24, [sp, #48]
    ldp     x25, x26, [sp, #64]
    ldp     x27, x28, [sp, #80]
    ldp     x29, x30, [sp], #96
    ret

// ---- медленный путь unmark: слово l1 обнулилось, спуск по l2/l3 ----
Lunmark_slow:
    lsr     x4, x13, #12
    add     x0, x0, #0x400                  // &l2
    ldr     x1, [x0, x4, lsl #3]
    mov     x2, #1
    lsl     x2, x2, x5
    bic     x1, x1, x2
    str     x1, [x0, x4, lsl #3]
    ldr     x3, [x0, #0x10]                 // l3
    cbnz    x1, Lscan
    mov     x2, #1
    lsl     x2, x2, x4
    bic     x3, x3, x2
    str     x3, [x0, #0x10]
Lscan:
    mov     w13, #-1                        // NIL, если сторона опустела
    cbz     x3, Lunmark_done
    rbit    x1, x3
    clz     x1, x1                          // i2
    ldr     x2, [x0, x1, lsl #3]            // l2[i2]
    rbit    x2, x2
    clz     x2, x2
    orr     x1, x2, x1, lsl #6              // i1
    sub     x0, x0, #0x400                  // &l1
    ldr     x2, [x0, x1, lsl #3]            // l1[i1]
    rbit    x2, x2
    clz     x2, x2
    orr     x13, x2, x1, lsl #6
    b       Lunmark_done

// ---- холодный путь: цена вне полосы ----
Lcold_oob:
    sub     x3, x16, #BOOK_H0               // book
    ldr     x1, [x3, #BOOK_PINNED]
    cbz     x1, Lrebase
    cbnz    x25, Lreject                    // книга не пуста: сдвиг полосы запрещён, отказ
    add     x1, x16, #H_L1
    ldr     x1, [x1, #H_L3 - H_L1]
    cbnz    x1, Lreject
    add     x1, x17, #H_L1
    ldr     x1, [x1, #H_L3 - H_L1]
    cbnz    x1, Lreject
Lrebase:
    mov     x0, x2                          // price (x2 не тронут с момента загрузки ордера)
    subs    x28, x0, #BAND / 2
    csel    x28, x28, xzr, hs               // base = price.saturating_sub(BAND/2)
    mov     x1, #1
    str     x1, [x3, #BOOK_PINNED]
    sub     x0, x0, x28                     // tick, заведомо < BAND
    b       Ltick_ok

Lreject:
    sub     x3, x16, #BOOK_H0
    ldr     x1, [x3, #BOOK_REJECTED]
    add     x1, x1, #1
    str     x1, [x3, #BOOK_REJECTED]
    b       Lnext

// ---- холодный путь: арена исчерпана (недостижимо при capacity >= n) ----
Lcold_nofree:
    sub     x3, x16, #BOOK_H0
    ldr     x1, [x3, #BOOK_REJECTED]
    add     x1, x1, #1
    str     x1, [x3, #BOOK_REJECTED]
    b       Lorder_done
