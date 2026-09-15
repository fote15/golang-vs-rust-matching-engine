package main

// Плоская книга на Go — тот же алгоритм, что в rust/src/orderbook.rs:
// прямая индексация уровней по тику, трёхуровневая битовая карта занятости,
// сторона bid в инвертированных координатах (одно условие матчинга для обеих
// сторон), арена ордеров на uint32-индексах вместо указателей.
//
// Зачем: честно ответить на вопрос «а если Go-версию оптимизировать так же?».
// Это НЕ код автора и не претендует на лучший возможный Go; это буквальный
// порт структуры данных, чтобы отделить эффект алгоритма от эффекта языка.
// Инварианты и хеш состояния совпадают с обеими другими реализациями.
//
// Что языку Go здесь недоступно: get_unchecked (bounds check остаётся, где BCE
// не сработал), repr(align), контроль инлайнинга, отсутствие GC write barrier
// (в арене указателей нет, так что барьер не срабатывает — это как раз тот
// приём, который делает пул noscan для GC).

import "math/bits"

const (
	bandBits = 13
	band     = 1 << bandBits
	mask     = band - 1
	l1w      = band / 64
	l2w      = (l1w + 63) / 64
	nilIdx   = ^uint32(0)
)

type flatLvl struct {
	qty  uint64
	head uint32
	tail uint32
}

type flatSlot struct {
	qty  uint64
	next uint32
	_    uint32
}

type half struct {
	best uint32
	l3   uint64
	l2   [l2w]uint64
	l1   [l1w]uint64
	// Массив фиксированной длины, а не слайс: индекс `t & mask` даёт компилятору
	// доказательство границ (BCE), иначе на каждом обращении остаётся проверка.
	lv [band]flatLvl
}

func (h *half) init() {
	h.best = nilIdx
	for i := range h.lv {
		h.lv[i].head, h.lv[i].tail = nilIdx, nilIdx
	}
}

func (h *half) mark(t uint32) {
	t &= mask
	h.l1[t>>6] |= 1 << (t & 63)
	h.l2[(t>>12)&(l2w-1)] |= 1 << ((t >> 6) & 63)
	h.l3 |= 1 << (t >> 12)
	if t < h.best {
		h.best = t
	}
}

func (h *half) scanMin() uint32 {
	if h.l3 == 0 {
		return nilIdx
	}
	i2 := uint32(bits.TrailingZeros64(h.l3))
	i1 := (i2 << 6) | uint32(bits.TrailingZeros64(h.l2[i2&(l2w-1)]))
	return (i1 << 6) | uint32(bits.TrailingZeros64(h.l1[i1&(l1w-1)]))
}

func (h *half) unmark(t uint32) {
	t &= mask
	i1 := t >> 6
	v := h.l1[i1] &^ (1 << (t & 63))
	h.l1[i1] = v
	if v != 0 {
		h.best = (i1 << 6) | uint32(bits.TrailingZeros64(v))
		return
	}
	w2 := &h.l2[(t>>12)&(l2w-1)]
	*w2 &^= 1 << (i1 & 63)
	if *w2 == 0 {
		h.l3 &^= 1 << (t >> 12)
	}
	h.best = h.scanMin()
}

func (h *half) levelCount() int {
	n := 0
	for _, w := range h.l1 {
		n += bits.OnesCount64(w)
	}
	return n
}

// OrderBook — плоская книга. Имя совпадает с книгой автора, чтобы обвязка
// bench/go_bench/main.go собиралась без изменений.
type OrderBook struct {
	h             [2]half // 0 — ask (прямые тики), 1 — bid (инвертированные)
	arena         []flatSlot
	ids           []uint64
	freeHead      uint32
	base          uint64
	basePinned    bool
	resting       uint32
	TradesCount   uint64
	MatchedVolume uint64
	Rejected      uint64
}

func NewOrderBook(capacity int) *OrderBook {
	if capacity < 64 {
		capacity = 64
	}
	ob := &OrderBook{
		arena:    make([]flatSlot, capacity),
		ids:      make([]uint64, capacity),
		freeHead: 0,
	}
	ob.h[0].init()
	ob.h[1].init()
	for i := range ob.arena {
		ob.arena[i].next = uint32(i + 1)
	}
	ob.arena[capacity-1].next = nilIdx
	return ob
}

func (ob *OrderBook) grow() {
	old := len(ob.arena)
	arena := make([]flatSlot, old*2)
	copy(arena, ob.arena)
	for i := old; i < len(arena); i++ {
		arena[i].next = uint32(i + 1)
	}
	arena[len(arena)-1].next = nilIdx
	ids := make([]uint64, old*2)
	copy(ids, ob.ids)
	ob.arena, ob.ids, ob.freeHead = arena, ids, uint32(old)
}

// ProcessOrder — та же сигнатура, что у автора.
func (ob *OrderBook) ProcessOrder(id, price, quantity uint64, side Side) {
	if quantity == 0 {
		ob.Rejected++
		return
	}
	t := price - ob.base
	if t >= band {
		if ob.basePinned && (ob.resting != 0 || ob.h[0].l3 != 0 || ob.h[1].l3 != 0) {
			ob.Rejected++
			return
		}
		ob.base, ob.basePinned = price-band/2, true
		t = price - ob.base
	}
	tick := uint32(t)
	consume := int(side) & 1
	restAt := consume ^ 1
	var cmask, rmask uint32
	if consume == 1 {
		cmask = mask
	}
	if restAt == 1 {
		rmask = mask
	}
	ckey, rkey := tick^cmask, tick^rmask
	qty := quantity

	for {
		best := ob.h[consume].best
		if qty == 0 || best > ckey {
			break
		}
		qty = ob.take(consume, best, qty)
	}
	if qty > 0 {
		node := ob.alloc(id, qty)
		ob.rest(restAt, rkey, node, qty)
	}
}

func (ob *OrderBook) take(hIdx int, key uint32, taker uint64) uint64 {
	hh := &ob.h[hIdx]
	l := &hh.lv[key&mask]
	head, levelQty := l.head, l.qty
	free, trades, volume := ob.freeHead, ob.TradesCount, ob.MatchedVolume
	var freed uint32
	arena := ob.arena
	for taker > 0 && head != nilIdx {
		s := &arena[head]
		m := s.qty
		if taker < m {
			m = taker
		}
		trades++
		volume += m
		taker -= m
		s.qty -= m
		levelQty -= m
		if s.qty == 0 {
			next := s.next
			s.next = free
			free = head
			head = next
			freed++
		}
	}
	ob.freeHead, ob.TradesCount, ob.MatchedVolume = free, trades, volume
	ob.resting -= freed
	l.head, l.qty = head, levelQty
	if head == nilIdx {
		l.tail = nilIdx
		hh.unmark(key)
	}
	return taker
}

func (ob *OrderBook) alloc(id, qty uint64) uint32 {
	if ob.freeHead == nilIdx {
		ob.grow()
	}
	idx := ob.freeHead
	s := &ob.arena[idx]
	ob.freeHead = s.next
	s.qty, s.next = qty, nilIdx
	ob.ids[idx] = id
	ob.resting++
	return idx
}

func (ob *OrderBook) rest(hIdx int, key, node uint32, qty uint64) {
	hh := &ob.h[hIdx]
	l := &hh.lv[key&mask]
	tail := l.tail
	l.tail = node
	l.qty += qty
	if tail == nilIdx {
		l.head = node
		hh.mark(key)
	} else {
		ob.arena[tail].next = node
	}
}

// ---- наблюдение (вне горячего пути) ----

func (ob *OrderBook) BestBid() (uint64, bool) {
	b := ob.h[1].best
	if b == nilIdx {
		return 0, false
	}
	return ob.base + uint64(b^mask), true
}

func (ob *OrderBook) BestAsk() (uint64, bool) {
	a := ob.h[0].best
	if a == nilIdx {
		return 0, false
	}
	return ob.base + uint64(a), true
}

func (ob *OrderBook) levelAt(side Side, price uint64) *flatLvl {
	t := price - ob.base
	if t >= band {
		return nil
	}
	if side == Buy {
		return &ob.h[1].lv[uint32(t)^mask]
	}
	return &ob.h[0].lv[uint32(t)]
}
