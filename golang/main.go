package main

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"time"
)

type BenchResult struct {
	Elapsed      time.Duration
	Throughput   float64
	AvgLatencyNs float64
}

func runStreamBenchmark(n int, seed uint64) (BenchResult, uint64, uint64, int, int, int, uint64, uint64) {
	// Pre-allocate pool scaled to expected resting depth (~22% resting orders)
	poolCap := n / 4
	if poolCap < 1_000_000 {
		poolCap = 1_000_000
	}

	ob := NewOrderBook(poolCap)
	rng := NewXorshift64(seed)

	runtime.GC()

	start := time.Now()
	for i := 1; i <= n; i++ {
		r1 := rng.Next()
		r2 := rng.Next()
		r3 := rng.Next()

		side := Side(r1 & 1)
		priceOffset := int64(r2%200) - 100
		price := uint64(10000 + priceOffset)
		qty := 1 + (r3 % 100)

		ob.ProcessOrder(uint64(i), price, qty, side)
	}
	elapsed := time.Since(start)

	totalOrders := float64(n)
	seconds := elapsed.Seconds()
	ops := totalOrders / seconds
	avgLatencyNs := float64(elapsed.Nanoseconds()) / totalOrders

	var bestBid, bestAsk uint64
	if len(ob.Bids) > 0 {
		bestBid = ob.Bids[0].Price
	}
	if len(ob.Asks) > 0 {
		bestAsk = ob.Asks[0].Price
	}

	restingBids := 0
	for _, l := range ob.Bids {
		restingBids += int(l.OrderCount)
	}
	restingAsks := 0
	for _, l := range ob.Asks {
		restingAsks += int(l.OrderCount)
	}

	return BenchResult{
		Elapsed:      elapsed,
		Throughput:   ops,
		AvgLatencyNs: avgLatencyNs,
	}, ob.TradesCount, ob.MatchedVolume, restingBids, restingAsks, len(ob.Bids) + len(ob.Asks), bestBid, bestAsk
}

func main() {
	const seed uint64 = 0xDEADBEEFCAFE1234
	n := 100_000_000

	if len(os.Args) > 1 {
		if val, err := strconv.Atoi(os.Args[1]); err == nil && val > 0 {
			n = val
		}
	}

	fmt.Printf("========================================================\n")
	fmt.Printf("             GO MATCHING ENGINE BENCHMARK               \n")
	fmt.Printf("========================================================\n")
	fmt.Printf("Orders to Process:      %d (%.1f Million)\n", n, float64(n)/1_000_000.0)
	fmt.Printf("PRNG:                   Deterministic Xorshift64\n")
	fmt.Printf("Seed:                   0x%X\n", seed)
	fmt.Printf("--------------------------------------------------------\n")
	fmt.Printf("Starting benchmark...\n")

	res, trades, volume, restingB, restingA, totalLevels, bestBid, bestAsk := runStreamBenchmark(n, seed)

	fmt.Printf("--------------------------------------------------------\n")
	fmt.Printf("SUMMARY (GO):\n")
	fmt.Printf("  Total Elapsed Time:  %v (%.2f ms | %.2f s)\n", res.Elapsed, float64(res.Elapsed.Microseconds())/1000.0, res.Elapsed.Seconds())
	fmt.Printf("  Throughput:          %.2f orders/sec (%.2f M ops/s)\n", res.Throughput, res.Throughput/1_000_000.0)
	fmt.Printf("  Average Latency:     %.2f ns/order\n", res.AvgLatencyNs)
	fmt.Printf("  Trades Executed:     %d\n", trades)
	fmt.Printf("  Volume Matched:      %d\n", volume)
	fmt.Printf("  Resting in Book:     %d (Bids: %d, Asks: %d)\n", restingB+restingA, restingB, restingA)
	fmt.Printf("  Active Price Levels: %d (Best Bid: %d, Best Ask: %d)\n", totalLevels, bestBid, bestAsk)
	fmt.Printf("========================================================\n\n")
}
