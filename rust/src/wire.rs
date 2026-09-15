//! Zero-copy разбор проволочного формата.
//!
//! `serde_json` здесь запрещён правилом проекта и не нужен: схема фиксирована,
//! все значения — беззнаковые целые. Сканер работает прямо по байтам тела
//! запроса, не выделяет ни одного байта в куче и не строит промежуточных
//! структур: batch отдаётся вызывающей стороне через колбэк.

/// Одно сообщение об ордере в том виде, в каком оно приходит по проводу.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OrderMsg {
    pub id: u64,
    pub price: u64,
    pub quantity: u64,
    pub side: u8,
}

#[inline(always)]
fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() {
        match b[*i] {
            b' ' | b'\t' | b'\n' | b'\r' => *i += 1,
            _ => return,
        }
    }
}

#[inline(always)]
fn expect(b: &[u8], i: &mut usize, c: u8) -> Option<()> {
    skip_ws(b, i);
    if *i < b.len() && b[*i] == c {
        *i += 1;
        Some(())
    } else {
        None
    }
}

/// Разбор беззнакового целого. Переполнение отбрасывает сообщение,
/// а не молча заворачивает значение.
#[inline(always)]
fn scan_u64(b: &[u8], i: &mut usize) -> Option<u64> {
    skip_ws(b, i);
    let start = *i;
    let mut v: u64 = 0;
    while *i < b.len() {
        let c = b[*i];
        if !c.is_ascii_digit() {
            break;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
        *i += 1;
    }
    (*i > start).then_some(v)
}

/// Считать строку ключа между кавычками, вернуть её байты.
#[inline(always)]
fn scan_key<'a>(b: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    skip_ws(b, i);
    if *i >= b.len() || b[*i] != b'"' {
        return None;
    }
    *i += 1;
    let start = *i;
    while *i < b.len() && b[*i] != b'"' {
        *i += 1;
    }
    if *i >= b.len() {
        return None;
    }
    let k = &b[start..*i];
    *i += 1;
    Some(k)
}

/// Разбор одного объекта ордера начиная с `{`.
///
/// Ключи распознаются по длине и первому байту — этого достаточно, чтобы
/// различить `id` / `price` / `quantity` / `side`, и это дешевле сравнения строк.
/// Порядок ключей произвольный, как и у `encoding/json` на стороне Go.
#[inline(always)]
fn scan_order_obj(b: &[u8], i: &mut usize) -> Option<OrderMsg> {
    expect(b, i, b'{')?;
    let mut m = OrderMsg::default();
    let mut seen = 0u8;
    skip_ws(b, i);
    if *i < b.len() && b[*i] == b'}' {
        *i += 1;
        return None; // пустой объект — не ордер
    }
    loop {
        let k = scan_key(b, i)?;
        expect(b, i, b':')?;
        let v = scan_u64(b, i)?;
        match (k.len(), k.first().copied().unwrap_or(0)) {
            (2, b'i') => {
                m.id = v;
                seen |= 1;
            }
            (5, b'p') => {
                m.price = v;
                seen |= 2;
            }
            (8, b'q') => {
                m.quantity = v;
                seen |= 4;
            }
            (4, b's') => {
                m.side = v as u8;
                seen |= 8;
            }
            _ => {} // неизвестный ключ игнорируем, как это делает encoding/json
        }
        skip_ws(b, i);
        match b.get(*i) {
            Some(b',') => {
                *i += 1;
            }
            Some(b'}') => {
                *i += 1;
                break;
            }
            _ => return None,
        }
    }
    (seen & 0b1110 == 0b1110).then_some(m) // price, quantity, side обязательны
}

/// Разбор тела вида `{"id":1,"price":10000,"quantity":5,"side":0}`.
#[inline]
pub fn scan_single(body: &[u8]) -> Option<OrderMsg> {
    let mut i = 0usize;
    scan_order_obj(body, &mut i)
}

/// Разбор тела вида `{"orders":[{...},{...}]}` без единой аллокации.
/// Каждый разобранный ордер немедленно отдаётся в `sink`.
/// Возвращает количество разобранных ордеров.
#[inline]
pub fn scan_batch<F: FnMut(OrderMsg)>(body: &[u8], mut sink: F) -> Option<usize> {
    let mut i = 0usize;
    expect(body, &mut i, b'{')?;
    let mut n = 0usize;
    loop {
        let k = scan_key(body, &mut i)?;
        expect(body, &mut i, b':')?;
        if k == b"orders" {
            expect(body, &mut i, b'[')?;
            skip_ws(body, &mut i);
            if body.get(i) == Some(&b']') {
                i += 1;
            } else {
                loop {
                    let m = scan_order_obj(body, &mut i)?;
                    sink(m);
                    n += 1;
                    skip_ws(body, &mut i);
                    match body.get(i) {
                        Some(b',') => i += 1,
                        Some(b']') => {
                            i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
        } else {
            return None;
        }
        skip_ws(body, &mut i);
        match body.get(i) {
            Some(b',') => i += 1,
            Some(b'}') | None => break,
            _ => break,
        }
    }
    Some(n)
}

/// Быстрая запись беззнакового целого в буфер без форматтера.
#[inline(always)]
pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        out.push(tmp[n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_basic() {
        let m = scan_single(br#"{"id":7,"price":10000,"quantity":5,"side":1}"#).unwrap();
        assert_eq!(
            m,
            OrderMsg {
                id: 7,
                price: 10000,
                quantity: 5,
                side: 1
            }
        );
    }

    #[test]
    fn single_any_key_order_and_whitespace() {
        let m =
            scan_single(b"{ \"side\" : 0 , \"quantity\":42,\n\"price\": 99 ,\"id\":3 }").unwrap();
        assert_eq!(
            m,
            OrderMsg {
                id: 3,
                price: 99,
                quantity: 42,
                side: 0
            }
        );
    }

    #[test]
    fn single_unknown_key_ignored() {
        let m = scan_single(br#"{"id":1,"symbol":5,"price":10,"quantity":2,"side":0}"#).unwrap();
        assert_eq!(m.price, 10);
    }

    #[test]
    fn single_rejects_malformed() {
        assert!(scan_single(br#"{"id":1,"price":}"#).is_none());
        assert!(scan_single(br#"{"id":1}"#).is_none()); // нет обязательных полей
        assert!(scan_single(b"not json").is_none());
        assert!(scan_single(b"{}").is_none());
    }

    #[test]
    fn single_rejects_overflow() {
        assert!(
            scan_single(br#"{"id":1,"price":99999999999999999999999,"quantity":1,"side":0}"#)
                .is_none()
        );
    }

    #[test]
    fn batch_basic() {
        let mut got = Vec::new();
        let n = scan_batch(
            br#"{"orders":[{"id":1,"price":10,"quantity":2,"side":0},{"id":2,"price":11,"quantity":3,"side":1}]}"#,
            |m| got.push(m),
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(got[0].price, 10);
        assert_eq!(got[1].side, 1);
    }

    #[test]
    fn batch_empty_array() {
        let mut got = Vec::new();
        assert_eq!(scan_batch(br#"{"orders":[]}"#, |m| got.push(m)), Some(0));
        assert!(got.is_empty());
    }

    #[test]
    fn batch_rejects_truncated() {
        let mut got = Vec::new();
        assert!(scan_batch(br#"{"orders":[{"id":1,"price":10,"#, |m| got.push(m)).is_none());
    }

    #[test]
    fn write_u64_roundtrip() {
        for v in [0u64, 1, 9, 10, 99, 100, 12345, u64::MAX] {
            let mut b = Vec::new();
            write_u64(&mut b, v);
            assert_eq!(String::from_utf8(b).unwrap(), v.to_string());
        }
    }
}
