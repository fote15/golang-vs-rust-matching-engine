package main

// Xorshift64 — побитово тот же, что golang/generator.go и rust/src/generator.rs.
type Xorshift64 struct{ state uint64 }

func NewXorshift64(seed uint64) *Xorshift64 {
	if seed == 0 {
		seed = 0xDEADBEEFCAFE1234
	}
	return &Xorshift64{state: seed}
}

func (x *Xorshift64) Next() uint64 {
	v := x.state
	v ^= v << 13
	v ^= v >> 7
	v ^= v << 17
	x.state = v
	return v
}

func GenerateOrders(n int, seed uint64) []OrderInput {
	rng := NewXorshift64(seed)
	orders := make([]OrderInput, n)
	for i := 0; i < n; i++ {
		r1, r2, r3 := rng.Next(), rng.Next(), rng.Next()
		orders[i] = OrderInput{
			ID:       uint64(i + 1),
			Price:    uint64(10000 + int64(r2%200) - 100),
			Quantity: 1 + (r3 % 100),
			Side:     Side(r1 & 1),
		}
	}
	return orders
}
