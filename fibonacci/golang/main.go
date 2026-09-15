package main

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

//go:noinline
func fibRecursive(n int) uint64 {
	if n <= 1 {
		return uint64(n)
	}
	return fibRecursive(n-1) + fibRecursive(n-2)
}

// Global sink to prevent compiler dead-code elimination
var Sink uint64

func runTimedBenchmark(durationSec float64, n int) (uint64, time.Duration) {
	deadline := time.Now().Add(time.Duration(durationSec * float64(time.Second)))
	var iterations uint64 = 0
	start := time.Now()

	for time.Now().Before(deadline) {
		res := fibRecursive(n)
		Sink += res
		iterations++
	}

	actualElapsed := time.Since(start)
	return iterations, actualElapsed
}

func runSingleTarget(n int) (uint64, time.Duration) {
	start := time.Now()
	res := fibRecursive(n)
	elapsed := time.Since(start)
	Sink += res
	return res, elapsed
}

func main() {
	durationSec := 4.0
	if len(os.Args) > 1 {
		if val, err := strconv.ParseFloat(os.Args[1], 64); err == nil && val > 0 {
			durationSec = val
		}
	}

	fmt.Printf("========================================================\n")
	fmt.Printf("             GO FIBONACCI BENCHMARK                     \n")
	fmt.Printf("========================================================\n")

	// Part 1: Fixed 4-second time window benchmark
	// We run fib(30) repeatedly for 4.0 seconds to measure throughput
	fmt.Printf("[Test 1] 4.0-Second Fixed-Time Throughput (fib(30)):\n")
	iterations, actualElapsed := runTimedBenchmark(durationSec, 30)
	opsPerSec := float64(iterations) / actualElapsed.Seconds()
	avgMsPerRun := (actualElapsed.Seconds() * 1000.0) / float64(iterations)

	fmt.Printf("  Duration:              %v\n", actualElapsed)
	fmt.Printf("  Completed Calculations: %d\n", iterations)
	fmt.Printf("  Throughput:            %.2f fib(30)/sec\n", opsPerSec)
	fmt.Printf("  Average Latency:       %.3f ms per fib(30)\n", avgMsPerRun)
	fmt.Printf("--------------------------------------------------------\n")

	// Part 2: Heavy recursion (~4 seconds target)
	fmt.Printf("[Test 2] Single Heavy Calculation (~4.0s target):\n")
	res45, el45 := runSingleTarget(45)
	fmt.Printf("  fib(45) = %d in %v (%.2f ms | %.3f s)\n", res45, el45, float64(el45.Microseconds())/1000.0, el45.Seconds())
	res46, el46 := runSingleTarget(46)
	fmt.Printf("  fib(46) = %d in %v (%.2f ms | %.3f s)\n", res46, el46, float64(el46.Microseconds())/1000.0, el46.Seconds())
	fmt.Printf("========================================================\n\n")
}
