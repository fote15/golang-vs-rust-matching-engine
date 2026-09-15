package main

// Протокол автора (golang/main.go), формат вывода тот же: прогрев 10k,
// 5 итераций по 100 000 ордеров, свежая книга на каждой, runtime.GC()
// перед таймером, среднее. Отличие от golang/ — только движок:
// плоская книга + битовая карта + арена на uint32-индексах (orderbook.go),
// то есть тот же алгоритм, что в rust/, на Go.

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"time"
)

func runSingle(n int, orders []OrderInput) (time.Duration, *OrderBook) {
	ob := NewOrderBook(n)
	runtime.GC()
	start := time.Now()
	for i := 0; i < n; i++ {
		o := &orders[i]
		ob.ProcessOrder(o.ID, o.Price, o.Quantity, o.Side)
	}
	return time.Since(start), ob
}

func main() {
	var seed uint64 = 0xDEADBEEFCAFE1234
	const n = 100000
	iterations := 5
	for i, a := range os.Args {
		if a == "--iters" && i+1 < len(os.Args) {
			if v, err := strconv.Atoi(os.Args[i+1]); err == nil && v > 0 {
				iterations = v
			}
		}
		if a == "--seed" && i+1 < len(os.Args) {
			s := os.Args[i+1]
			if len(s) > 2 && (s[:2] == "0x" || s[:2] == "0X") {
				s = s[2:]
			}
			if v, err := strconv.ParseUint(s, 16, 64); err == nil {
				seed = v
			}
		}
	}

	fmt.Printf("========================================================\n")
	fmt.Printf("           GO (FLAT) MATCHING ENGINE BENCHMARK          \n")
	fmt.Printf("========================================================\n")
	fmt.Printf("Workload: %d orders per run | %d iterations\n", n, iterations)
	fmt.Printf("PRNG: Deterministic Xorshift64 (seed: 0x%X)\n", seed)
	fmt.Printf("--------------------------------------------------------\n")

	runSingle(10000, GenerateOrders(10000, seed))
	orders := GenerateOrders(n, seed)

	var total time.Duration
	best := time.Duration(1<<63 - 1)
	var last *OrderBook
	for i := 1; i <= iterations; i++ {
		el, ob := runSingle(n, orders)
		total += el
		if el < best {
			best = el
		}
		last = ob
		fmt.Printf("  Iteration %d: %8.3f ms | %10.2f M ops/s | %6.2f ns/order\n",
			i, float64(el.Nanoseconds())/1e6, float64(n)/el.Seconds()/1e6, float64(el.Nanoseconds())/float64(n))
	}
	avg := total / time.Duration(iterations)

	nb, na := last.h[1].levelCount(), last.h[0].levelCount()
	bb, _ := last.BestBid()
	ba, _ := last.BestAsk()
	fmt.Printf("--------------------------------------------------------\n")
	fmt.Printf("SUMMARY (GO FLAT):\n")
	fmt.Printf("  Best Time:         %.3f ms (%.2f M ops/s)\n", float64(best.Nanoseconds())/1e6, float64(n)/best.Seconds()/1e6)
	fmt.Printf("  Average Time:      %.3f ms (%.2f M ops/s)\n", float64(avg.Nanoseconds())/1e6, float64(n)/avg.Seconds()/1e6)
	fmt.Printf("  Average Latency:   %.2f ns/order\n", float64(avg.Nanoseconds())/float64(n))
	fmt.Printf("  Trades Executed:   %d\n", last.TradesCount)
	fmt.Printf("  Volume Matched:    %d\n", last.MatchedVolume)
	fmt.Printf("  Resting in Book:   %d\n", last.resting)
	fmt.Printf("  Active Levels:     %d (Best Bid: %d, Best Ask: %d)\n", nb+na, bb, ba)
	fmt.Printf("========================================================\n\n")

	if seed == 0xDEADBEEFCAFE1234 && (last.TradesCount != 77576 || last.MatchedVolume != 1973216 || last.resting != 21620 || nb != 55 || na != 50) {
		fmt.Fprintf(os.Stderr, "INVARIANT MISMATCH\n")
		os.Exit(1)
	}
}
