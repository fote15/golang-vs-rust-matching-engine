package main

// Те же типы, что в golang/types.go у автора; каталог golang/ не трогаем,
// поэтому дублируем (пакет main нельзя импортировать).
type Side uint8

const (
	Buy  Side = 0
	Sell Side = 1
)

type OrderInput struct {
	ID       uint64
	Price    uint64
	Quantity uint64
	Side     Side
}
