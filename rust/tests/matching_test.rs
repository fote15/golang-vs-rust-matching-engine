use matching_engine_rust::generator::generate_orders;
use matching_engine_rust::orderbook::{OrderBook, SubmitError, BAND};
use matching_engine_rust::types::{Order, Side};

#[test]
fn test_matching_logic() {
    let mut ob = OrderBook::with_capacity(100);

    // 1. Ask 10 @ 100 ложится в книгу
    ob.process_order(Order {
        id: 1,
        price: 100,
        quantity: 10,
        side: Side::Sell,
    });
    assert_eq!(ob.ask_levels(), 1);
    assert_eq!(ob.level_qty(Side::Sell, 100), 10);
    assert_eq!(ob.best_ask(), Some(100));

    // 2. Bid 6 @ 100 — частичное исполнение встречного аска
    ob.process_order(Order {
        id: 2,
        price: 100,
        quantity: 6,
        side: Side::Buy,
    });
    assert_eq!(ob.trades_count, 1);
    assert_eq!(ob.matched_volume, 6);
    assert_eq!(ob.level_qty(Side::Sell, 100), 4);

    // 3. Агрессивный Bid 10 @ 101 — добирает аск и 6 остаётся в книге
    ob.process_order(Order {
        id: 3,
        price: 101,
        quantity: 10,
        side: Side::Buy,
    });
    assert_eq!(ob.trades_count, 2);
    assert_eq!(ob.matched_volume, 10);
    assert_eq!(ob.ask_levels(), 0);
    assert_eq!(ob.bid_levels(), 1);
    assert_eq!(ob.best_bid(), Some(101));
    assert_eq!(ob.level_qty(Side::Buy, 101), 6);
}

#[test]
fn test_price_time_priority() {
    let mut ob = OrderBook::with_capacity(64);
    // Три аска на одном уровне — должны исполняться в порядке поступления (FIFO).
    ob.process_order(Order {
        id: 10,
        price: 500,
        quantity: 5,
        side: Side::Sell,
    });
    ob.process_order(Order {
        id: 11,
        price: 500,
        quantity: 5,
        side: Side::Sell,
    });
    ob.process_order(Order {
        id: 12,
        price: 500,
        quantity: 5,
        side: Side::Sell,
    });
    assert_eq!(ob.level_orders(Side::Sell, 500), 3);
    assert_eq!(ob.front_order_id(Side::Sell, 500), Some(10));

    ob.process_order(Order {
        id: 13,
        price: 500,
        quantity: 5,
        side: Side::Buy,
    });
    assert_eq!(ob.front_order_id(Side::Sell, 500), Some(11));
    assert_eq!(ob.level_orders(Side::Sell, 500), 2);
}

#[test]
fn test_price_priority_across_levels() {
    let mut ob = OrderBook::with_capacity(64);
    ob.process_order(Order {
        id: 1,
        price: 105,
        quantity: 5,
        side: Side::Sell,
    });
    ob.process_order(Order {
        id: 2,
        price: 101,
        quantity: 5,
        side: Side::Sell,
    });
    ob.process_order(Order {
        id: 3,
        price: 103,
        quantity: 5,
        side: Side::Sell,
    });
    // Лучший аск — самый дешёвый
    assert_eq!(ob.best_ask(), Some(101));

    // Тейкер сметает 101 и 103, но не трогает 105
    ob.process_order(Order {
        id: 4,
        price: 104,
        quantity: 10,
        side: Side::Buy,
    });
    assert_eq!(ob.matched_volume, 10);
    assert_eq!(ob.best_ask(), Some(105));
    assert_eq!(ob.bid_levels(), 0);
}

#[test]
fn test_out_of_band_is_rejected_not_corrupting() {
    let mut ob = OrderBook::new(64, 10_000);
    assert_eq!(
        ob.submit(1, 10_000, 0, Side::Buy),
        Err(SubmitError::ZeroQuantity)
    );
    assert_eq!(ob.rejected, 1);

    // Кладём ордер — теперь книга непуста и полоса зафиксирована.
    assert!(ob.submit(2, 10_000, 7, Side::Buy).is_ok());
    assert_eq!(ob.best_bid(), Some(10_000));

    // Цена вне полосы на НЕПУСТОЙ книге отвергается (аналог ценового лимита биржи).
    let far = 10_000 + BAND as u64;
    assert_eq!(
        ob.submit(3, far, 10, Side::Buy),
        Err(SubmitError::PriceOutOfBand)
    );
    assert_eq!(ob.rejected, 2);
    // Книга не повреждена
    assert_eq!(ob.best_bid(), Some(10_000));
    assert_eq!(ob.resting_orders(), 1);
}

#[test]
fn test_rebase_on_empty_book() {
    // На пустой книге полоса свободно переезжает вслед за рынком.
    let mut ob = OrderBook::with_capacity(64);
    assert!(ob.submit(1, 100, 5, Side::Sell).is_ok());
    assert_eq!(ob.best_ask(), Some(100));
    ob.submit(2, 100, 5, Side::Buy).unwrap(); // выметаем книгу
    assert_eq!(ob.ask_levels(), 0);
    // Книга пуста -> полоса переезжает на совершенно другой ценовой диапазон
    assert!(ob.submit(3, 5_000_000, 5, Side::Sell).is_ok());
    assert_eq!(ob.best_ask(), Some(5_000_000));
    assert_eq!(ob.rejected, 0);
}

#[test]
fn test_arena_growth_beyond_capacity() {
    // Ёмкость меньше числа покоящихся ордеров — арена обязана вырасти без порчи данных.
    let mut ob = OrderBook::new(64, 1_000);
    for i in 0..500u64 {
        ob.process_order(Order {
            id: i + 1,
            price: 1_000 + (i % 50),
            quantity: 3,
            side: Side::Buy,
        });
    }
    assert_eq!(ob.resting_orders(), 500);
    assert_eq!(ob.trades_count, 0);
    assert_eq!(ob.best_bid(), Some(1_049));
    // Всё выметается встречной стороной
    ob.process_order(Order {
        id: 9_999,
        price: 1_000,
        quantity: 1_500,
        side: Side::Sell,
    });
    assert_eq!(ob.matched_volume, 1_500);
    assert_eq!(ob.resting_orders(), 0);
}

#[test]
fn test_reset_is_clean_and_reusable() {
    let mut ob = OrderBook::with_capacity(1024);
    for i in 0..100u64 {
        ob.process_order(Order {
            id: i,
            price: 9_990 + (i % 20),
            quantity: 4,
            side: Side::Buy,
        });
    }
    ob.reset();
    assert_eq!(ob.trades_count, 0);
    assert_eq!(ob.matched_volume, 0);
    assert_eq!(ob.bid_levels(), 0);
    assert_eq!(ob.ask_levels(), 0);
    assert_eq!(ob.resting_orders(), 0);
    assert_eq!(ob.best_bid(), None);
    assert_eq!(ob.best_ask(), None);
    // После reset книга полностью работоспособна
    ob.process_order(Order {
        id: 1,
        price: 10_000,
        quantity: 1,
        side: Side::Sell,
    });
    assert_eq!(ob.best_ask(), Some(10_000));
}

#[test]
fn test_determinism_100k() {
    const SEED: u64 = 0xDEADBEEFCAFE1234;
    const N: usize = 100_000;

    let orders = generate_orders(N, SEED);
    let mut ob = OrderBook::with_capacity(N + 64);

    for order in orders {
        ob.process_order(order);
    }

    // Ровно те же числа, что даёт эталонная Go-реализация.
    assert_eq!(ob.trades_count, 77_576);
    assert_eq!(ob.matched_volume, 1_973_216);
    assert_eq!(ob.bid_levels(), 55);
    assert_eq!(ob.ask_levels(), 50);
    assert_eq!(ob.resting_orders(), 21_620);
    assert_eq!(ob.best_bid(), Some(10_038));
    assert_eq!(ob.best_ask(), Some(10_043));
    assert_eq!(ob.rejected, 0);
}

#[test]
fn test_book_is_send() {
    // Прежняя реализация на *mut OrderNode не была Send и требовала unsafe impl.
    fn assert_send<T: Send>() {}
    assert_send::<OrderBook>();
}
