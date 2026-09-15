use crate::types::{Order, Side};

pub struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        let s = if seed == 0 { 0xDEADBEEFCAFE1234 } else { seed };
        Self { state: s }
    }

    /// Названо `next_u64`, а не `next`: одноимённый метод путается с
    /// `Iterator::next` (clippy: should_implement_trait).
    #[inline(always)]
    pub fn next_u64(&mut self) -> u64 {
        let mut v = self.state;
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        self.state = v;
        v
    }
}

pub fn generate_orders(n: usize, seed: u64) -> Vec<Order> {
    let mut rng = Xorshift64::new(seed);
    let mut orders = Vec::with_capacity(n);

    for i in 0..n {
        let r1 = rng.next_u64();
        let r2 = rng.next_u64();
        let r3 = rng.next_u64();

        let side = if (r1 & 1) == 0 { Side::Buy } else { Side::Sell };
        let price_offset = (r2 % 200) as i64 - 100;
        let price = (10000i64 + price_offset) as u64;
        let qty = 1 + (r3 % 100);

        orders.push(Order {
            id: (i + 1) as u64,
            price,
            quantity: qty,
            side,
        });
    }

    orders
}
