//! Матчинг-движок: плоская книга с прямой индексацией по тику,
//! иерархическая битовая карта занятости и branchless горячий путь.
//!
//! # Почему так, а не Vec<Box<PriceLevel>> + бинарный поиск
//!
//! * **Нет pointer chasing.** Уровни лежат в одном плоском массиве, ордера — в одной
//!   арене. Ни одного `Box`, `Rc`, `Arc` в горячем пути. Доступ к уровню — это
//!   `base + tick*16`, один сдвиг, а не разыменование указателя из вектора указателей.
//! * **Нет O(n) сдвигов.** Создание и удаление уровня — это установка/сброс бита,
//!   а не `Vec::insert`/`Vec::remove(0)` с memmove хвоста.
//! * **Нет бинарного поиска.** Лучшая цена — спуск по трёхуровневой битовой карте:
//!   три `trailing_zeros`, всё в L1.
//! * **Нет ветки по стороне.** Сторона bid хранится в инвертированных координатах
//!   тика (`j = t ^ MASK`), поэтому «лучшая цена» обеих сторон — это всегда МИНИМУМ,
//!   а условие матчинга одно и то же. Ветка `if side == Buy` (монетка 50/50, около
//!   половины промахов предсказателя) вырождается в индекс массива и XOR с маской.
//! * **Индексы вместо указателей.** `u32` вдвое меньше указателя и позволяет арене
//!   расти без инвалидации ссылок — с `*mut OrderNode` это было бы UB.
//! * **`Send` без `unsafe impl`.** Книга не содержит сырых указателей, поэтому
//!   безопасно переносится между потоками.

use crate::types::{Order, Side};

/// Ширина ценовой полосы в тиках. Степень двойки: индексация сдвигом, маска для инверсии.
/// 8192 тика ≈ активная полоса одного инструмента; выход за неё обрабатывается
/// холодным путём (см. [`SubmitError`]).
pub const BAND_BITS: usize = 13;
pub const BAND: usize = 1 << BAND_BITS;
/// Маска инверсии координат для стороны bid.
const MASK: u32 = (BAND - 1) as u32;
const L1W: usize = BAND / 64;
const L2W: usize = L1W.div_ceil(64);

const NIL: u32 = u32::MAX;

/// Индекс половины книги в `Book::h`. `ASK` хранит прямые тики, `BID` — инвертированные.
const ASK: usize = 0;
const BID: usize = 1;

/// Уровень цены. Ровно 16 байт: четыре уровня на 64-байтную линию кэша,
/// адресация сдвигом без умножения.
#[derive(Clone, Copy)]
#[repr(C)]
struct Lvl {
    qty: u64,
    head: u32,
    tail: u32,
}

/// Ячейка ордера в арене. Ровно 16 байт. `id` вынесен в холодный массив:
/// в горячем цикле матчинга он не читается ни разу.
#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    qty: u64,
    next: u32,
    _pad: u32,
}

/// Одна сторона книги: уровни плюс трёхуровневая битовая карта занятости.
/// Выровнена по линии кэша, чтобы две стороны не делили одну линию.
#[repr(align(64))]
struct Half {
    /// Кэш минимального занятого тика в собственных координатах. `NIL` — сторона пуста.
    best: u32,
    l3: u64,
    l2: [u64; L2W],
    l1: Box<[u64]>,
    lv: Box<[Lvl]>,
}

impl Half {
    fn new() -> Self {
        Self {
            best: NIL,
            l3: 0,
            l2: [0u64; L2W],
            l1: vec![0u64; L1W].into_boxed_slice(),
            lv: vec![
                Lvl {
                    qty: 0,
                    head: NIL,
                    tail: NIL
                };
                BAND
            ]
            .into_boxed_slice(),
        }
    }

    #[inline(always)]
    fn mark(&mut self, t: usize) {
        // SAFETY: вызывающая сторона гарантирует t < BAND, поэтому
        // t>>6 < L1W и t>>12 < L2W.
        unsafe {
            *self.l1.get_unchecked_mut(t >> 6) |= 1u64 << (t & 63);
            *self.l2.get_unchecked_mut(t >> 12) |= 1u64 << ((t >> 6) & 63);
        }
        self.l3 |= 1u64 << (t >> 12);
        let t32 = t as u32;
        // branchless min
        self.best = if t32 < self.best { t32 } else { self.best };
    }

    #[inline(always)]
    fn unmark(&mut self, t: usize) {
        // Быстрый путь: слово карты l1[t>>6] уже прочитано здесь же и лежит в
        // регистре. Если после сброса бита в нём остались уровни, следующая
        // лучшая цена — в этом же слове, и она берётся одним trailing_zeros
        // без единого обращения к памяти.
        //
        // Корректность: `best` — минимальный занятый тик половины, а unmark
        // вызывается только на нём (единственная точка вызова — take(), где
        // уровень опустошён). Если в слове i1 остались биты, то новый минимум
        // тоже в нём: все младшие слова пусты по построению карты.
        //
        // Замер: быстрый путь срабатывает в 78% случаев, полный трёхуровневый
        // спуск scan_min() — цепочка из трёх ЗАВИСИМЫХ обращений в L1
        // (~12 тактов), которую внеочередное исполнение спрятать не может,
        // потому что результат нужен немедленно для условия цикла матчинга.
        let i1 = t >> 6;
        // SAFETY: вызывающая сторона гарантирует t < BAND, значит
        // i1 < L1W и t>>12 < L2W.
        unsafe {
            let w1 = self.l1.get_unchecked_mut(i1);
            let v = *w1 & !(1u64 << (t & 63));
            *w1 = v;
            if v != 0 {
                self.best = ((i1 << 6) as u32) | v.trailing_zeros();
                return;
            }
            let w2 = self.l2.get_unchecked_mut(t >> 12);
            *w2 &= !(1u64 << (i1 & 63));
            if *w2 == 0 {
                self.l3 &= !(1u64 << (t >> 12));
            }
        }
        self.best = self.scan_min();
    }

    /// Минимальный занятый тик: три зависимых обращения к L1-кэшу и три CTZ.
    #[inline(always)]
    fn scan_min(&self) -> u32 {
        if self.l3 == 0 {
            return NIL;
        }
        // SAFETY: l3 != 0, поэтому i2 указывает на ненулевое слово l2,
        // а оно — на ненулевое слово l1; оба индекса в границах по построению карты.
        unsafe {
            let i2 = self.l3.trailing_zeros() as usize;
            let i1 = (i2 << 6) | (self.l2.get_unchecked(i2).trailing_zeros() as usize);
            ((i1 << 6) as u32) | self.l1.get_unchecked(i1).trailing_zeros()
        }
    }

    #[inline]
    fn level_count(&self) -> usize {
        self.l1.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn clear(&mut self) {
        // Чистим только реально затронутые уровни — по битовой карте,
        // а не весь массив в BAND элементов.
        for i1 in 0..L1W {
            let mut w = self.l1[i1];
            while w != 0 {
                let b = w.trailing_zeros() as usize;
                w &= w - 1;
                self.lv[(i1 << 6) | b] = Lvl {
                    qty: 0,
                    head: NIL,
                    tail: NIL,
                };
            }
            self.l1[i1] = 0;
        }
        self.l2 = [0u64; L2W];
        self.l3 = 0;
        self.best = NIL;
    }
}

/// Причина отказа в приёме ордера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// Цена вне ценовой полосы книги.
    PriceOutOfBand,
    /// Нулевое количество.
    ZeroQuantity,
}

/// Результат обработки ордера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fill {
    /// Сколько сделок произошло.
    pub trades: u32,
    /// Сколько объёма исполнено.
    pub filled: u64,
    /// Сколько осталось лежать в книге (0, если ордер исполнен целиком).
    pub resting: u64,
}

/// Книга лимитных ордеров одного инструмента.
pub struct OrderBook {
    h: [Half; 2],
    arena: Box<[Slot]>,
    /// Холодные данные: идентификаторы ордеров. В матчинге не читаются.
    ids: Box<[u64]>,
    free_head: u32,
    /// Нижняя граница ценовой полосы: `tick = price - base`.
    base: u64,
    /// База ещё не привязана к рынку: первый же ордер центрирует полосу вокруг своей цены.
    base_pinned: bool,
    resting: u32,
    pub trades_count: u64,
    pub matched_volume: u64,
    pub rejected: u64,
}

impl OrderBook {
    /// Полоса центрируется вокруг цены `base_price`.
    pub fn new(capacity: usize, base_price: u64) -> Self {
        let cap = capacity.max(64);
        let mut arena = vec![
            Slot {
                qty: 0,
                next: NIL,
                _pad: 0
            };
            cap
        ]
        .into_boxed_slice();
        for (i, s) in arena.iter_mut().enumerate() {
            s.next = (i + 1) as u32;
        }
        arena[cap - 1].next = NIL;
        Self {
            h: [Half::new(), Half::new()],
            arena,
            ids: vec![0u64; cap].into_boxed_slice(),
            free_head: 0,
            base: base_price.saturating_sub((BAND / 2) as u64),
            base_pinned: base_price != 0,
            resting: 0,
            trades_count: 0,
            matched_volume: 0,
            rejected: 0,
        }
    }

    /// Полоса не привязана заранее: она центрируется вокруг цены первого ордера.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::new(capacity, 0)
    }

    #[inline(always)]
    fn tick_of(&self, price: u64) -> Option<u32> {
        let t = price.wrapping_sub(self.base);
        if t < BAND as u64 {
            Some(t as u32)
        } else {
            None
        }
    }

    /// Холодный путь: привязать или сдвинуть полосу.
    ///
    /// Сдвиг допускается только на пустой книге — иначе пришлось бы переносить
    /// все покоящиеся уровни, а это скрытая O(n) пауза посреди торгов. На непустой
    /// книге цена вне полосы отвергается: ровно так ведут себя биржевые
    /// ценовые лимиты (limit-up / limit-down).
    #[cold]
    #[inline(never)]
    fn rebase(&mut self, price: u64) -> bool {
        if self.base_pinned && (self.resting != 0 || self.h[0].l3 != 0 || self.h[1].l3 != 0) {
            return false;
        }
        self.base = price.saturating_sub((BAND / 2) as u64);
        self.base_pinned = true;
        true
    }

    #[inline(always)]
    fn price_of(&self, tick: u32) -> u64 {
        self.base + tick as u64
    }

    /// Приём ордера. Горячий путь.
    #[inline(always)]
    pub fn submit(
        &mut self,
        id: u64,
        price: u64,
        quantity: u64,
        side: Side,
    ) -> Result<Fill, SubmitError> {
        if quantity == 0 {
            return Err(self.reject(SubmitError::ZeroQuantity));
        }
        let tick = match self.tick_of(price) {
            Some(t) => t,
            None => {
                if !self.rebase(price) {
                    return Err(self.reject(SubmitError::PriceOutOfBand));
                }
                // SAFETY по логике: после успешного rebase цена заведомо внутри полосы.
                match self.tick_of(price) {
                    Some(t) => t,
                    None => return Err(self.reject(SubmitError::PriceOutOfBand)),
                }
            }
        };
        Ok(self.execute(id, tick, quantity, side))
    }

    #[cold]
    #[inline(never)]
    fn reject(&mut self, e: SubmitError) -> SubmitError {
        self.rejected += 1;
        e
    }

    /// Ядро матчинга. Полностью branchless по стороне.
    #[inline(always)]
    fn execute(&mut self, id: u64, tick: u32, mut qty: u64, side: Side) -> Fill {
        // Покупатель ест asks (половина 0) и ложится в bids (половина 1); продавец наоборот.
        let consume = (side as usize) & 1; // Buy=0 -> ест ASK(0); Sell=1 -> ест BID(1)
        let rest_at = consume ^ 1;
        // Половина BID живёт в инвертированных координатах, ASK — в прямых.
        let cmask = ((consume == BID) as u32).wrapping_neg() & MASK;
        let rmask = ((rest_at == BID) as u32).wrapping_neg() & MASK;
        let ckey = tick ^ cmask;
        let rkey = tick ^ rmask;

        let start_trades = self.trades_count;
        let want = qty;

        // Единый цикл для обеих сторон: лучшая цена — всегда минимум в своих координатах.
        loop {
            // SAFETY: consume ∈ {0,1}.
            let best = unsafe { self.h.get_unchecked(consume).best };
            // NIL = u32::MAX > любого валидного ключа, поэтому пустая книга
            // выходит из цикла тем же сравнением — без отдельной ветки.
            if qty == 0 || best > ckey {
                break;
            }
            qty = self.take(consume, best, qty);
        }

        if qty > 0 {
            let node = self.alloc(id, qty);
            self.rest(rest_at, rkey, node, qty);
        }

        Fill {
            trades: (self.trades_count - start_trades) as u32,
            filled: want - qty,
            resting: qty,
        }
    }

    /// Забрать ликвидность с одного уровня.
    #[inline(always)]
    fn take(&mut self, half: usize, key: u32, mut taker: u64) -> u64 {
        let ti = key as usize;
        // SAFETY: half ∈ {0,1}; key получен из `best`, значит уровень занят и ti < BAND.
        unsafe {
            let hh = self.h.get_unchecked_mut(half);
            let l = hh.lv.get_unchecked_mut(ti);
            let (mut head, mut level_qty) = (l.head, l.qty);

            // Счётчики держим в регистрах: иначе LLVM перечитывает поля self
            // на каждой итерации, не в силах доказать отсутствие алиасинга.
            let mut free = self.free_head;
            let mut trades = self.trades_count;
            let mut volume = self.matched_volume;
            let mut freed = 0u32;

            while taker > 0 && head != NIL {
                let slot = self.arena.get_unchecked_mut(head as usize);
                let m = if slot.qty < taker { slot.qty } else { taker };
                trades += 1;
                volume += m;
                taker -= m;
                slot.qty -= m;
                level_qty -= m;
                if slot.qty == 0 {
                    let next = slot.next;
                    slot.next = free;
                    free = head;
                    head = next;
                    freed += 1;
                }
            }

            self.free_head = free;
            self.trades_count = trades;
            self.matched_volume = volume;
            self.resting -= freed;

            let hh = self.h.get_unchecked_mut(half);
            let l = hh.lv.get_unchecked_mut(ti);
            l.head = head;
            l.qty = level_qty;
            if head == NIL {
                l.tail = NIL;
                hh.unmark(ti);
            }
        }
        taker
    }

    #[inline(always)]
    fn alloc(&mut self, id: u64, qty: u64) -> u32 {
        if self.free_head == NIL {
            self.grow();
        }
        let idx = self.free_head;
        // SAFETY: после grow() free_head заведомо валиден и < arena.len().
        unsafe {
            let s = self.arena.get_unchecked_mut(idx as usize);
            self.free_head = s.next;
            s.qty = qty;
            s.next = NIL;
            *self.ids.get_unchecked_mut(idx as usize) = id;
        }
        self.resting += 1;
        idx
    }

    /// Арена адресуется индексами, а не указателями, поэтому её можно
    /// расширять без инвалидации ссылок — с `*mut OrderNode` это было бы UB.
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let old = self.arena.len();
        let new = old * 2;
        let mut arena = vec![
            Slot {
                qty: 0,
                next: NIL,
                _pad: 0
            };
            new
        ]
        .into_boxed_slice();
        arena[..old].copy_from_slice(&self.arena);
        for (i, s) in arena.iter_mut().enumerate().skip(old) {
            s.next = (i + 1) as u32;
        }
        arena[new - 1].next = NIL;
        let mut ids = vec![0u64; new].into_boxed_slice();
        ids[..old].copy_from_slice(&self.ids);
        self.arena = arena;
        self.ids = ids;
        self.free_head = old as u32;
    }

    #[inline(always)]
    fn rest(&mut self, half: usize, key: u32, node: u32, qty: u64) {
        let t = key as usize;
        // SAFETY: half ∈ {0,1}; key = tick^mask, а tick < BAND, значит t < BAND.
        unsafe {
            let hh = self.h.get_unchecked_mut(half);
            let l = hh.lv.get_unchecked_mut(t);
            let tail = l.tail;
            l.tail = node;
            l.qty += qty;
            if tail == NIL {
                l.head = node;
                hh.mark(t);
            } else {
                self.arena.get_unchecked_mut(tail as usize).next = node;
            }
        }
    }

    // ---- наблюдение за состоянием (вне горячего пути) ----

    #[inline]
    pub fn best_bid(&self) -> Option<u64> {
        let b = self.h[BID].best;
        (b != NIL).then(|| self.price_of(b ^ MASK))
    }

    #[inline]
    pub fn best_ask(&self) -> Option<u64> {
        let a = self.h[ASK].best;
        (a != NIL).then(|| self.price_of(a))
    }

    #[inline]
    pub fn bid_levels(&self) -> usize {
        self.h[BID].level_count()
    }

    #[inline]
    pub fn ask_levels(&self) -> usize {
        self.h[ASK].level_count()
    }

    /// Суммарный объём на уровне указанной цены.
    pub fn level_qty(&self, side: Side, price: u64) -> u64 {
        let Some(tick) = self.tick_of(price) else {
            return 0;
        };
        match side {
            Side::Buy => self.h[BID].lv[(tick ^ MASK) as usize].qty,
            Side::Sell => self.h[ASK].lv[tick as usize].qty,
        }
    }

    /// Число ордеров на уровне (обход списка — только для наблюдения).
    pub fn level_orders(&self, side: Side, price: u64) -> usize {
        let Some(tick) = self.tick_of(price) else {
            return 0;
        };
        let (half, t) = match side {
            Side::Buy => (BID, tick ^ MASK),
            Side::Sell => (ASK, tick),
        };
        let mut n = 0;
        let mut cur = self.h[half].lv[t as usize].head;
        while cur != NIL {
            n += 1;
            cur = self.arena[cur as usize].next;
        }
        n
    }

    /// Идентификатор ордера в голове очереди уровня.
    pub fn front_order_id(&self, side: Side, price: u64) -> Option<u64> {
        let tick = self.tick_of(price)?;
        let (half, t) = match side {
            Side::Buy => (BID, tick ^ MASK),
            Side::Sell => (ASK, tick),
        };
        let head = self.h[half].lv[t as usize].head;
        (head != NIL).then(|| self.ids[head as usize])
    }

    #[inline]
    pub fn resting_orders(&self) -> usize {
        self.resting as usize
    }

    /// Сбросить книгу, сохранив выделенную память. Аллокаций не делает.
    pub fn reset(&mut self) {
        self.h[0].clear();
        self.h[1].clear();
        let n = self.arena.len();
        for (i, s) in self.arena.iter_mut().enumerate() {
            s.qty = 0;
            s.next = (i + 1) as u32;
        }
        self.arena[n - 1].next = NIL;
        self.free_head = 0;
        self.resting = 0;
        self.base_pinned = false;
        self.trades_count = 0;
        self.matched_volume = 0;
        self.rejected = 0;
    }

    /// Совместимость с прежним API бенчмарка.
    #[inline(always)]
    pub fn process_order(&mut self, order: Order) {
        let _ = self.submit(order.id, order.price, order.quantity, order.side);
    }
}
