// Матчинг-движок на C#: плоская книга с прямой индексацией по тику,
// трёхуровневая битовая карта занятости, арена ордеров на 32-битных индексах.
// Структура и семантика — один в один с rust/src/orderbook.rs; протокол замера
// и формат вывода — как у golang/main.go.
//
// Ни одного объекта на ордер, ни одной аллокации в управляемой куче в таймере:
// вся память книги — нативные блоки (NativeMemory), выделенные в конструкторе.

using System;
using System.Diagnostics;
using System.Globalization;
using System.Numerics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace MatchingEngine;

/// <summary>Входной ордер. 32 байта, последовательный доступ в таймере.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct OrderInput
{
    public ulong Id;
    public ulong Price;
    public ulong Quantity;
    public ulong Side; // 0 = Buy, 1 = Sell
}

public struct Xorshift64
{
    private ulong _state;

    public Xorshift64(ulong seed)
    {
        _state = seed == 0 ? 0xDEADBEEFCAFE1234UL : seed;
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public ulong Next()
    {
        ulong v = _state;
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        _state = v;
        return v;
    }
}

public static class Generator
{
    public static OrderInput[] Generate(int n, ulong seed)
    {
        var rng = new Xorshift64(seed);
        var orders = new OrderInput[n];
        for (int i = 0; i < n; i++)
        {
            ulong r1 = rng.Next();
            ulong r2 = rng.Next();
            ulong r3 = rng.Next();
            ulong side = r1 & 1;
            long priceOffset = (long)(r2 % 200) - 100;
            ulong price = (ulong)(10000 + priceOffset);
            ulong qty = 1 + (r3 % 100);
            orders[i].Id = (ulong)(i + 1);
            orders[i].Price = price;
            orders[i].Quantity = qty;
            orders[i].Side = side;
        }
        return orders;
    }
}

/// <summary>Уровень цены: ровно 16 байт, четыре на линию кэша.</summary>
[StructLayout(LayoutKind.Sequential, Size = 16)]
internal struct Lvl
{
    public ulong Qty;
    public uint Head;
    public uint Tail;
}

/// <summary>Ячейка арены: ровно 16 байт. Id хранится в холодном массиве.</summary>
[StructLayout(LayoutKind.Sequential, Size = 16)]
internal struct Slot
{
    public ulong Qty;
    public uint Next;
    public uint Pad;
}

/// <summary>Одна сторона книги. Выровнена по 64 байтам: две стороны не делят линию.</summary>
[StructLayout(LayoutKind.Sequential, Size = 64)]
internal unsafe struct Half
{
    public uint Best;      // минимальный занятый тик в своих координатах, NIL — пусто
    public uint Pad0;
    public ulong L3;
    public fixed ulong L2[OrderBook.L2W];
    public ulong* L1;      // L1W слов
    public Lvl* Lv;        // BAND уровней
}

public sealed unsafe class OrderBook : IDisposable
{
    public const int BandBits = 13;
    public const int Band = 1 << BandBits;
    public const uint Mask = Band - 1;
    public const int L1W = Band / 64;
    public const int L2W = (L1W + 63) / 64;
    public const uint Nil = uint.MaxValue;
    private const int Ask = 0;
    private const int Bid = 1;

    private Half* _h;        // [2], ASK — прямые тики, BID — инвертированные (t ^ Mask)
    private Slot* _arena;
    private ulong* _ids;
    private uint _cap;
    private uint _freeHead;
    private ulong _base;
    private bool _basePinned;
    private uint _resting;
    public ulong TradesCount;
    public ulong MatchedVolume;
    public ulong Rejected;

    public OrderBook(int capacity)
    {
        uint cap = (uint)Math.Max(capacity, 64);
        _cap = cap;
        _h = (Half*)NativeMemory.AlignedAlloc((nuint)(sizeof(Half) * 2), 64);
        for (int s = 0; s < 2; s++)
        {
            Half* hh = _h + s;
            hh->Best = Nil;
            hh->L3 = 0;
            for (int i = 0; i < L2W; i++) hh->L2[i] = 0;
            hh->L1 = (ulong*)NativeMemory.AlignedAlloc((nuint)(sizeof(ulong) * L1W), 64);
            for (int i = 0; i < L1W; i++) hh->L1[i] = 0;
            hh->Lv = (Lvl*)NativeMemory.AlignedAlloc((nuint)(sizeof(Lvl) * Band), 64);
            for (int i = 0; i < Band; i++)
            {
                hh->Lv[i].Qty = 0;
                hh->Lv[i].Head = Nil;
                hh->Lv[i].Tail = Nil;
            }
        }
        _arena = (Slot*)NativeMemory.AlignedAlloc((nuint)(sizeof(Slot) * cap), 64);
        _ids = (ulong*)NativeMemory.AlignedAlloc((nuint)(sizeof(ulong) * cap), 64);
        for (uint i = 0; i < cap; i++)
        {
            _arena[i].Qty = 0;
            _arena[i].Next = i + 1;
            _arena[i].Pad = 0;
            _ids[i] = 0;
        }
        _arena[cap - 1].Next = Nil;
        _freeHead = 0;
        _base = 0;
        _basePinned = false;
        _resting = 0;
        TradesCount = 0;
        MatchedVolume = 0;
        Rejected = 0;
    }

    public void Dispose()
    {
        if (_h == null) return;
        for (int s = 0; s < 2; s++)
        {
            NativeMemory.AlignedFree(_h[s].L1);
            NativeMemory.AlignedFree(_h[s].Lv);
        }
        NativeMemory.AlignedFree(_h);
        NativeMemory.AlignedFree(_arena);
        NativeMemory.AlignedFree(_ids);
        _h = null;
        _arena = null;
        _ids = null;
    }

    // ---- битовая карта ----

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static void Mark(Half* hh, uint t)
    {
        hh->L1[t >> 6] |= 1UL << (int)(t & 63);
        hh->L2[t >> 12] |= 1UL << (int)((t >> 6) & 63);
        hh->L3 |= 1UL << (int)(t >> 12);
        uint best = hh->Best;
        hh->Best = t < best ? t : best;
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static void Unmark(Half* hh, uint t)
    {
        // Быстрый путь: если в слове l1 остались уровни, следующая лучшая цена — в нём же
        // (unmark вызывается только на минимальном занятом тике).
        uint i1 = t >> 6;
        ulong* w1 = hh->L1 + i1;
        ulong v = *w1 & ~(1UL << (int)(t & 63));
        *w1 = v;
        if (v != 0)
        {
            hh->Best = (i1 << 6) | (uint)BitOperations.TrailingZeroCount(v);
            return;
        }
        UnmarkSlow(hh, t);
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static void UnmarkSlow(Half* hh, uint t)
    {
        uint i1 = t >> 6;
        uint i2 = t >> 12;
        ulong v2 = hh->L2[i2] & ~(1UL << (int)(i1 & 63));
        hh->L2[i2] = v2;
        if (v2 == 0)
        {
            ulong v3 = hh->L3 & ~(1UL << (int)i2);
            hh->L3 = v3;
            if (v3 == 0)
            {
                hh->Best = Nil;
                return;
            }
        }
        hh->Best = ScanMin(hh);
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static uint ScanMin(Half* hh)
    {
        uint i2 = (uint)BitOperations.TrailingZeroCount(hh->L3);
        uint i1 = (i2 << 6) | (uint)BitOperations.TrailingZeroCount(hh->L2[i2]);
        return (i1 << 6) | (uint)BitOperations.TrailingZeroCount(hh->L1[i1]);
    }

    private static int LevelCount(Half* hh)
    {
        int n = 0;
        for (int i = 0; i < L1W; i++) n += BitOperations.PopCount(hh->L1[i]);
        return n;
    }

    // ---- холодный путь ----

    [MethodImpl(MethodImplOptions.NoInlining)]
    private bool Rebase(ulong price)
    {
        if (_basePinned && (_resting != 0 || _h[0].L3 != 0 || _h[1].L3 != 0)) return false;
        ulong half = Band / 2;
        _base = price >= half ? price - half : 0;
        _basePinned = true;
        return true;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void Grow()
    {
        uint old = _cap;
        uint nw = old * 2;
        Slot* arena = (Slot*)NativeMemory.AlignedAlloc((nuint)(sizeof(Slot) * nw), 64);
        ulong* ids = (ulong*)NativeMemory.AlignedAlloc((nuint)(sizeof(ulong) * nw), 64);
        Buffer.MemoryCopy(_arena, arena, (long)sizeof(Slot) * nw, (long)sizeof(Slot) * old);
        Buffer.MemoryCopy(_ids, ids, (long)sizeof(ulong) * nw, (long)sizeof(ulong) * old);
        for (uint i = old; i < nw; i++)
        {
            arena[i].Qty = 0;
            arena[i].Next = i + 1;
            arena[i].Pad = 0;
            ids[i] = 0;
        }
        arena[nw - 1].Next = Nil;
        NativeMemory.AlignedFree(_arena);
        NativeMemory.AlignedFree(_ids);
        _arena = arena;
        _ids = ids;
        _cap = nw;
        _freeHead = old;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void SubmitSlow(ulong id, ulong price, ulong qty, ulong side)
    {
        if (qty == 0) { Rejected++; return; }
        if (!Rebase(price)) { Rejected++; return; }
        ulong t = price - _base;
        if (t >= (ulong)Band) { Rejected++; return; }
        Execute(id, (uint)t, qty, side);
    }

    // ---- горячий путь ----

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public void ProcessOrder(ulong id, ulong price, ulong qty, ulong side)
    {
        ulong t = price - _base; // wrapping
        if (t >= (ulong)Band || qty == 0)
        {
            SubmitSlow(id, price, qty, side);
            return;
        }
        Execute(id, (uint)t, qty, side);
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private void Execute(ulong id, uint tick, ulong qty, ulong side)
    {
        // Buy (0) ест ASK (0), ложится в BID (1); Sell — наоборот. Без ветки по стороне.
        int consume = (int)(side & 1);
        int restAt = consume ^ 1;
        uint cmask = (uint)(-consume) & Mask;
        uint rmask = (uint)(-restAt) & Mask;
        uint ckey = tick ^ cmask;
        uint rkey = tick ^ rmask;

        Half* hc = _h + consume;
        while (true)
        {
            uint best = hc->Best; // Nil > любого ключа: пустая сторона выходит тем же сравнением
            if (qty == 0 || best > ckey) break;
            qty = Take(hc, best, qty);
        }

        if (qty > 0)
        {
            uint node = Alloc(id, qty);
            Rest(_h + restAt, rkey, node, qty);
        }
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private ulong Take(Half* hh, uint key, ulong taker)
    {
        Lvl* l = hh->Lv + key;
        uint head = l->Head;
        ulong levelQty = l->Qty;

        Slot* arena = _arena;
        uint free = _freeHead;
        ulong trades = TradesCount;
        ulong volume = MatchedVolume;
        uint freed = 0;

        while (taker > 0 && head != Nil)
        {
            Slot* s = arena + head;
            ulong sq = s->Qty;
            ulong m = sq < taker ? sq : taker;
            trades++;
            volume += m;
            taker -= m;
            sq -= m;
            levelQty -= m;
            s->Qty = sq;
            if (sq == 0)
            {
                uint next = s->Next;
                s->Next = free;
                free = head;
                head = next;
                freed++;
            }
        }

        _freeHead = free;
        TradesCount = trades;
        MatchedVolume = volume;
        _resting -= freed;

        l->Head = head;
        l->Qty = levelQty;
        if (head == Nil)
        {
            l->Tail = Nil;
            Unmark(hh, key);
        }
        return taker;
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private uint Alloc(ulong id, ulong qty)
    {
        uint idx = _freeHead;
        if (idx == Nil)
        {
            Grow();
            idx = _freeHead;
        }
        Slot* s = _arena + idx;
        _freeHead = s->Next;
        s->Qty = qty;
        s->Next = Nil;
        _ids[idx] = id;
        _resting++;
        return idx;
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private void Rest(Half* hh, uint key, uint node, ulong qty)
    {
        Lvl* l = hh->Lv + key;
        uint tail = l->Tail;
        l->Tail = node;
        l->Qty += qty;
        if (tail == Nil)
        {
            l->Head = node;
            Mark(hh, key);
        }
        else
        {
            _arena[tail].Next = node;
        }
    }

    // ---- наблюдение (вне таймера) ----

    public int BidLevels => LevelCount(_h + Bid);
    public int AskLevels => LevelCount(_h + Ask);
    public int RestingOrders => (int)_resting;
    public ulong BestBid => _h[Bid].Best == Nil ? 0 : _base + (_h[Bid].Best ^ Mask);
    public ulong BestAsk => _h[Ask].Best == Nil ? 0 : _base + _h[Ask].Best;
}

public static class Program
{
    private const ulong DefaultSeed = 0xDEADBEEFCAFE1234UL;
    private const int N = 100_000;

    private struct RunResult
    {
        public long ElapsedNs;
        public ulong Trades, Volume, BestBid, BestAsk;
        public int Resting, BidLevels, AskLevels;
    }

    [MethodImpl(MethodImplOptions.AggressiveOptimization)]
    private static RunResult RunSingle(int n, OrderInput[] orders)
    {
        using var ob = new OrderBook(n);
        GC.Collect();
        GC.WaitForPendingFinalizers();

        RunResult r;
        long elapsedNs;
        unsafe
        {
            fixed (OrderInput* p = orders)
            {
                OrderInput* end = p + n;
                long start = Stopwatch.GetTimestamp();
                for (OrderInput* o = p; o < end; o++)
                {
                    ob.ProcessOrder(o->Id, o->Price, o->Quantity, o->Side);
                }
                long stop = Stopwatch.GetTimestamp();
                elapsedNs = (long)((stop - start) * (1_000_000_000.0 / Stopwatch.Frequency));
            }
        }

        r.ElapsedNs = elapsedNs;
        r.Trades = ob.TradesCount;
        r.Volume = ob.MatchedVolume;
        r.Resting = ob.RestingOrders;
        r.BidLevels = ob.BidLevels;
        r.AskLevels = ob.AskLevels;
        r.BestBid = ob.BestBid;
        r.BestAsk = ob.BestAsk;
        return r;
    }

    public static int Main(string[] args)
    {
        var inv = CultureInfo.InvariantCulture;
        ulong seed = DefaultSeed;
        int iterations = 5;
        bool json = false;
        for (int i = 0; i < args.Length; i++)
        {
            if (args[i] == "--iters" && i + 1 < args.Length)
            {
                iterations = int.Parse(args[++i], inv);
            }
            else if (args[i] == "--seed" && i + 1 < args.Length)
            {
                string s = args[++i];
                if (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase)) s = s.Substring(2);
                seed = ulong.Parse(s, NumberStyles.HexNumber, inv);
                if (seed == 0) seed = DefaultSeed;
            }
            else if (args[i] == "--json")
            {
                json = true;
            }
        }
        if (iterations < 1) iterations = 1;

        if (!json)
        {
            Console.WriteLine("========================================================");
            Console.WriteLine("             C# MATCHING ENGINE BENCHMARK               ");
            Console.WriteLine("========================================================");
            Console.WriteLine(string.Format(inv, "Workload: {0} orders per run | {1} iterations", N, iterations));
            Console.WriteLine(string.Format(inv, "PRNG: Deterministic Xorshift64 (seed: 0x{0:X})", seed));
            Console.WriteLine("--------------------------------------------------------");
        }

        // Warmup — как у автора: 10k ордеров тем же seed через свежую книгу.
        var warmup = Generator.Generate(10_000, seed);
        RunSingle(10_000, warmup);

        var orders = Generator.Generate(N, seed);

        long totalNs = 0;
        long minNs = long.MaxValue;
        RunResult last = default;

        for (int i = 1; i <= iterations; i++)
        {
            var res = RunSingle(N, orders);
            totalNs += res.ElapsedNs;
            if (res.ElapsedNs < minNs) minNs = res.ElapsedNs;
            last = res;
            if (!json)
            {
                double ms = res.ElapsedNs / 1_000_000.0;
                double mops = N / (res.ElapsedNs / 1e9) / 1_000_000.0;
                double nsPer = (double)res.ElapsedNs / N;
                Console.WriteLine(string.Format(inv, "  Iteration {0}: {1,8:F3} ms | {2,10:F2} M ops/s | {3,6:F2} ns/order", i, ms, mops, nsPer));
            }
        }

        long avgNs = totalNs / iterations;
        double bestMs = minNs / 1_000_000.0;
        double avgMs = avgNs / 1_000_000.0;
        double bestOps = N / (minNs / 1e9) / 1_000_000.0;
        double avgOps = N / (avgNs / 1e9) / 1_000_000.0;
        double avgLat = (double)avgNs / N;
        int totalLevels = last.BidLevels + last.AskLevels;

        if (json)
        {
            Console.WriteLine(string.Format(inv,
                "{{\"lang\":\"csharp\",\"orders\":{0},\"iters\":{1},\"min_ms\":{2:F6},\"avg_ms\":{3:F6},\"trades\":{4},\"volume\":{5},\"bid_levels\":{6},\"ask_levels\":{7},\"resting\":{8},\"best_bid\":{9},\"best_ask\":{10}}}",
                N, iterations, bestMs, avgMs, last.Trades, last.Volume, last.BidLevels, last.AskLevels, last.Resting, last.BestBid, last.BestAsk));
        }
        else
        {
            Console.WriteLine("--------------------------------------------------------");
            Console.WriteLine("SUMMARY (C#):");
            Console.WriteLine(string.Format(inv, "  Best Time:         {0:F3} ms ({1:F2} M ops/s)", bestMs, bestOps));
            Console.WriteLine(string.Format(inv, "  Average Time:      {0:F3} ms ({1:F2} M ops/s)", avgMs, avgOps));
            Console.WriteLine(string.Format(inv, "  Average Latency:   {0:F2} ns/order", avgLat));
            Console.WriteLine(string.Format(inv, "  Trades Executed:   {0}", last.Trades));
            Console.WriteLine(string.Format(inv, "  Volume Matched:    {0}", last.Volume));
            Console.WriteLine(string.Format(inv, "  Resting in Book:   {0}", last.Resting));
            Console.WriteLine(string.Format(inv, "  Active Levels:     {0} (Best Bid: {1}, Best Ask: {2})", totalLevels, last.BestBid, last.BestAsk));
            Console.WriteLine("========================================================");
            Console.WriteLine();
        }

        if (seed == DefaultSeed)
        {
            bool ok = last.Trades == 77_576
                   && last.Volume == 1_973_216
                   && last.BidLevels == 55
                   && last.AskLevels == 50
                   && last.Resting == 21_620
                   && last.BestBid == 10_038
                   && last.BestAsk == 10_043;
            if (!ok)
            {
                Console.Error.WriteLine(string.Format(inv,
                    "INVARIANT VIOLATION: trades={0} volume={1} bid_levels={2} ask_levels={3} resting={4} best_bid={5} best_ask={6}",
                    last.Trades, last.Volume, last.BidLevels, last.AskLevels, last.Resting, last.BestBid, last.BestAsk));
                return 1;
            }
        }
        return 0;
    }
}
