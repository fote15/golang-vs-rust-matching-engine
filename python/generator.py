"""Xorshift64 и генератор ордеров — побитово как golang/generator.go.

Работает вне таймера, скорость не важна; корректность u64-переполнения
обеспечивается маской (1 << 64) - 1.
"""

MASK64 = (1 << 64) - 1
DEFAULT_SEED = 0xDEADBEEFCAFE1234


def generate_orders(n, seed):
    """Возвращает четыре параллельных списка: sides, prices, qtys, ids.

    side: 0 = Buy, 1 = Sell; price: 9900..10099; qty: 1..100; id: i + 1.
    """
    v = seed & MASK64
    if v == 0:
        v = DEFAULT_SEED
    sides = [0] * n
    prices = [0] * n
    qtys = [0] * n
    ids = list(range(1, n + 1))
    m = MASK64
    for i in range(n):
        v ^= (v << 13) & m
        v ^= v >> 7
        v ^= (v << 17) & m
        r1 = v
        v ^= (v << 13) & m
        v ^= v >> 7
        v ^= (v << 17) & m
        r2 = v
        v ^= (v << 13) & m
        v ^= v >> 7
        v ^= (v << 17) & m
        r3 = v
        sides[i] = r1 & 1
        prices[i] = 10000 + (r2 % 200) - 100
        qtys[i] = 1 + (r3 % 100)
    return sides, prices, qtys, ids
