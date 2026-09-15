"""Плоская книга лимитных ордеров — та же структура, что rust/src/orderbook.rs,
переложенная на то, что быстро в CPython.

* Полоса BAND = 8192 тиков от base = first_price - 4096; уровень адресуется тиком.
* Уровень = два плоских списка `head[tick]`, `tail[tick]` (индексы в арене, -1 = пусто).
* Арена ордеров = списки `sq` (qty), `sn` (next, интрузивный free-list), `ids` (холодные).
  На ордер не создаётся ни одного объекта: только int в заранее выделенных списках.
* Битовая карта занятости: 128 слов по 64 бита (`l1`) + одно 128-битное слово (`l2`).
  Обе стороны хранятся в координатах «лучшая цена = МАКСИМАЛЬНЫЙ ключ»
  (bid: key = tick, ask: key = MASK ^ tick), поэтому лучшая цена — это
  `int.bit_length() - 1`, самый дешёвый способ найти старший бит в CPython.
* Горячий путь — один метод `process_orders`, где всё состояние книги живёт в
  локальных переменных (LOAD_FAST), без вызовов функций и обращений к атрибутам.
  В таймере нет ни одной аллокации контейнера.
"""

BAND_BITS = 13
BAND = 1 << BAND_BITS
MASK = BAND - 1
HALF_BAND = BAND >> 1
L1W = BAND >> 6
NIL = -1


class OrderBook:
    __slots__ = (
        "sq", "sn", "ids", "free",
        "ahead", "atail", "al1", "al2", "abest",
        "bhead", "btail", "bl1", "bl2", "bbest",
        "base", "pinned",
        "trades_count", "matched_volume", "rejected",
    )

    def __init__(self, capacity, base_price=0):
        cap = max(capacity, 64)
        self.sq = [0] * cap
        sn = list(range(1, cap + 1))
        sn[cap - 1] = NIL
        self.sn = sn
        self.ids = [0] * cap
        self.free = 0
        # ask: key = MASK ^ tick (меньшая цена -> больший ключ)
        self.ahead = [NIL] * BAND
        self.atail = [NIL] * BAND
        self.al1 = [0] * L1W
        self.al2 = 0
        self.abest = NIL
        # bid: key = tick (большая цена -> больший ключ)
        self.bhead = [NIL] * BAND
        self.btail = [NIL] * BAND
        self.bl1 = [0] * L1W
        self.bl2 = 0
        self.bbest = NIL
        self.base = max(base_price - HALF_BAND, 0)
        self.pinned = base_price != 0
        self.trades_count = 0
        self.matched_volume = 0
        self.rejected = 0

    # ------------------------------------------------------------------ hot path
    def process_orders(self, sides, prices, qtys, ids):
        """Прогнать последовательность ордеров. Ровно то же, что цикл по
        process_order, но состояние книги поднято в локальные переменные."""
        sq = self.sq
        sn = self.sn
        oids = self.ids
        free = self.free
        ahead = self.ahead
        atail = self.atail
        al1 = self.al1
        al2 = self.al2
        abest = self.abest
        bhead = self.bhead
        btail = self.btail
        bl1 = self.bl1
        bl2 = self.bl2
        bbest = self.bbest
        base = self.base
        trades = self.trades_count
        volume = self.matched_volume

        for side, price, qty, oid in zip(sides, prices, qtys, ids):
            tick = price - base
            if tick >> 13:
                # Холодный путь: цена вне полосы. Сдвиг полосы — только на пустой книге.
                if self.pinned and (abest >= 0 or bbest >= 0):
                    self.rejected += 1
                    continue
                base = price - HALF_BAND
                if base < 0:
                    base = 0
                self.base = base
                self.pinned = True
                tick = price - base

            if side:
                # ---------------- SELL: ест bids (key = tick), ложится в asks (key = MASK ^ tick)
                while bbest >= tick:
                    k = bbest
                    h = bhead[k]
                    while True:
                        mq = sq[h]
                        trades += 1
                        if mq > qty:
                            sq[h] = mq - qty
                            volume += qty
                            qty = 0
                            break
                        volume += mq
                        qty -= mq
                        nxt = sn[h]
                        sn[h] = free
                        free = h
                        h = nxt
                        if h < 0 or not qty:
                            break
                    if h < 0:
                        bhead[k] = -1
                        i = k >> 6
                        w = bl1[i] ^ (1 << (k & 63))
                        bl1[i] = w
                        if w:
                            bbest = (i << 6) | (w.bit_length() - 1)
                        else:
                            bl2 ^= 1 << i
                            if bl2:
                                i = bl2.bit_length() - 1
                                bbest = (i << 6) | (bl1[i].bit_length() - 1)
                            else:
                                bbest = -1
                    else:
                        bhead[k] = h
                    if not qty:
                        break
                if qty:
                    node = free
                    if node < 0:
                        self.free = free
                        self._grow()
                        sq = self.sq
                        sn = self.sn
                        oids = self.ids
                        node = self.free
                    free = sn[node]
                    sq[node] = qty
                    sn[node] = -1
                    oids[node] = oid
                    k = tick ^ MASK
                    if ahead[k] < 0:
                        ahead[k] = node
                        atail[k] = node
                        i = k >> 6
                        al1[i] |= 1 << (k & 63)
                        al2 |= 1 << i
                        if k > abest:
                            abest = k
                    else:
                        sn[atail[k]] = node
                        atail[k] = node
            else:
                # ---------------- BUY: ест asks (key = MASK ^ tick), ложится в bids (key = tick)
                ck = tick ^ MASK
                while abest >= ck:
                    k = abest
                    h = ahead[k]
                    while True:
                        mq = sq[h]
                        trades += 1
                        if mq > qty:
                            sq[h] = mq - qty
                            volume += qty
                            qty = 0
                            break
                        volume += mq
                        qty -= mq
                        nxt = sn[h]
                        sn[h] = free
                        free = h
                        h = nxt
                        if h < 0 or not qty:
                            break
                    if h < 0:
                        ahead[k] = -1
                        i = k >> 6
                        w = al1[i] ^ (1 << (k & 63))
                        al1[i] = w
                        if w:
                            abest = (i << 6) | (w.bit_length() - 1)
                        else:
                            al2 ^= 1 << i
                            if al2:
                                i = al2.bit_length() - 1
                                abest = (i << 6) | (al1[i].bit_length() - 1)
                            else:
                                abest = -1
                    else:
                        ahead[k] = h
                    if not qty:
                        break
                if qty:
                    node = free
                    if node < 0:
                        self.free = free
                        self._grow()
                        sq = self.sq
                        sn = self.sn
                        oids = self.ids
                        node = self.free
                    free = sn[node]
                    sq[node] = qty
                    sn[node] = -1
                    oids[node] = oid
                    if bhead[tick] < 0:
                        bhead[tick] = node
                        btail[tick] = node
                        i = tick >> 6
                        bl1[i] |= 1 << (tick & 63)
                        bl2 |= 1 << i
                        if tick > bbest:
                            bbest = tick
                    else:
                        sn[btail[tick]] = node
                        btail[tick] = node

        self.free = free
        self.al2 = al2
        self.abest = abest
        self.bl2 = bl2
        self.bbest = bbest
        self.trades_count = trades
        self.matched_volume = volume

    def process_order(self, side, price, qty, oid):
        """Один ордер — та же семантика, что process_orders на списке из одного элемента."""
        self.process_orders((side,), (price,), (qty,), (oid,))

    # ------------------------------------------------------------------ cold path
    def _grow(self):
        """Арена адресуется индексами, поэтому расширяется без инвалидации ссылок."""
        old = len(self.sq)
        new = old * 2
        self.sq.extend([0] * old)
        self.sn.extend(range(old + 1, new + 1))
        self.sn[new - 1] = NIL
        self.ids.extend([0] * old)
        self.free = old

    # ------------------------------------------------------------------ observation
    def best_bid(self):
        return None if self.bbest < 0 else self.base + self.bbest

    def best_ask(self):
        return None if self.abest < 0 else self.base + (self.abest ^ MASK)

    def bid_levels(self):
        return sum(w.bit_count() for w in self.bl1)

    def ask_levels(self):
        return sum(w.bit_count() for w in self.al1)

    def _count_side(self, l1, head):
        sn = self.sn
        n = 0
        for i, w in enumerate(l1):
            while w:
                b = (w & -w).bit_length() - 1
                w ^= 1 << b
                cur = head[(i << 6) | b]
                while cur >= 0:
                    n += 1
                    cur = sn[cur]
        return n

    def resting_bids(self):
        return self._count_side(self.bl1, self.bhead)

    def resting_asks(self):
        return self._count_side(self.al1, self.ahead)

    def resting_orders(self):
        return self.resting_bids() + self.resting_asks()
