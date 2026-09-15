//! Матчинг-сервер.
//!
//! # Архитектура
//!
//! * **Шардирование по ядрам.** N потоков, у каждого — собственный слушающий
//!   сокет с `SO_REUSEPORT` на одном порту и собственный однопоточный рантайм
//!   tokio. Ядро само балансирует соединения между шардами. Нет общего
//!   accept-мьютекса, нет work stealing, нет межъядерных побудок.
//! * **Свой HTTP/1.1.** Нужны только строка запроса, `Content-Length` и тело.
//!   Никакого hyper и никаких аллокаций на запрос: буферы живут по соединению
//!   и переиспользуются.
//! * **Zero-copy разбор тела.** См. [`matching_engine_rust::wire`] — сканер по
//!   байтам вместо `serde_json`, ноль промежуточных структур на batch.
//! * **Спинлок вместо мьютекса ОС.** Книга удерживается порядка микросекунды;
//!   futex-мьютекс с уходом в ядро на таком масштабе стоит дороже самой работы.
//!   Спин с экспоненциальным backoff и `spin_loop()`, уступка планировщику
//!   только после долгого ожидания.

use matching_engine_rust::orderbook::OrderBook;
use matching_engine_rust::types::Side;
use matching_engine_rust::wire::{scan_batch, scan_single, write_u64};

use matching_engine_rust::wire::OrderMsg;
use std::cell::UnsafeCell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------- спинлок

/// Книга под спинлоком. Выровнена по линии кэша: флаг блокировки не делит
/// линию с чужими данными, иначе соседние ядра будут гонять линию туда-сюда.
#[repr(align(64))]
struct SpinBook {
    lock: AtomicBool,
    _pad: [u8; 63],
    book: UnsafeCell<OrderBook>,
}

// SAFETY: доступ к `book` возможен только через `lock()`, который обеспечивает
// взаимное исключение через AtomicBool с Acquire/Release. Ссылка наружу не
// утекает: гард отдаёт `&mut` со своим временем жизни.
unsafe impl Sync for SpinBook {}
unsafe impl Send for SpinBook {}

struct Guard<'a>(&'a SpinBook);

impl SpinBook {
    fn new(book: OrderBook) -> Self {
        Self {
            lock: AtomicBool::new(false),
            _pad: [0; 63],
            book: UnsafeCell::new(book),
        }
    }

    #[inline]
    fn lock(&self) -> Guard<'_> {
        let mut backoff = 1u32;
        loop {
            if self
                .lock
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Guard(self);
            }
            // Ждём освобождения чтением (Relaxed), чтобы не долбить шину
            // эксклюзивными запросами линии кэша.
            while self.lock.load(Ordering::Relaxed) {
                for _ in 0..backoff {
                    std::hint::spin_loop();
                }
                if backoff < 1024 {
                    backoff <<= 1;
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }
}

impl Guard<'_> {
    #[inline(always)]
    fn book(&mut self) -> &mut OrderBook {
        // SAFETY: гард существует только при захваченном флаге, значит
        // исключительный доступ гарантирован.
        unsafe { &mut *self.0.book.get() }
    }
}

impl Drop for Guard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.lock.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------- HTTP

/// Стартовая ёмкость буфера разобранных ордеров. НЕ потолок: буфер растёт
/// под размер батча, иначе большие батчи молча теряли бы хвост.
const STAGING_INIT: usize = 4096;

const HDR_OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";
const HDR_204: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
const HDR_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
const HDR_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";

#[inline]
fn find_crlf2(b: &[u8], from: usize) -> Option<usize> {
    if b.len() < 4 {
        return None;
    }
    let start = from.saturating_sub(3);
    b[start..]
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| start + p)
}

/// Найти `Content-Length` в блоке заголовков. Сравнение без учёта регистра
/// и без аллокаций.
#[inline]
fn content_length(head: &[u8]) -> usize {
    const KEY: &[u8] = b"content-length";
    let mut i = 0;
    while i + KEY.len() < head.len() {
        if head[i] == b'\n' {
            let s = i + 1;
            if s + KEY.len() <= head.len()
                && head[s..s + KEY.len()].eq_ignore_ascii_case(KEY)
                && head.get(s + KEY.len()) == Some(&b':')
            {
                let mut j = s + KEY.len() + 1;
                while j < head.len() && (head[j] == b' ' || head[j] == b'\t') {
                    j += 1;
                }
                let mut v = 0usize;
                while j < head.len() && head[j].is_ascii_digit() {
                    v = v * 10 + (head[j] - b'0') as usize;
                    j += 1;
                }
                return v;
            }
        }
        i += 1;
    }
    0
}

/// Маршрут запроса, распознанный по строке запроса.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Order,
    Batch,
    BatchBin,
    Stats,
    Reset,
    Unknown,
}

#[inline]
fn route_of(line: &[u8]) -> Route {
    // "POST /batch HTTP/1.1"
    let Some(sp) = line.iter().position(|&c| c == b' ') else {
        return Route::Unknown;
    };
    let rest = &line[sp + 1..];
    let end = rest.iter().position(|&c| c == b' ').unwrap_or(rest.len());
    match &rest[..end] {
        b"/order" => Route::Order,
        b"/batch" => Route::Batch,
        b"/batch/bin" => Route::BatchBin,
        b"/stats" => Route::Stats,
        b"/reset" => Route::Reset,
        _ => Route::Unknown,
    }
}

#[inline]
fn side_of(v: u8) -> Side {
    if v == 0 {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// Собрать JSON-ответ без форматтера и без аллокаций сверх переиспользуемого буфера.
#[inline]
fn respond_engine(out: &mut Vec<u8>, trades: u64, volume: u64) {
    let mut body = [0u8; 64];
    let mut n = 0;
    macro_rules! put {
        ($s:expr) => {{
            let s: &[u8] = $s;
            body[n..n + s.len()].copy_from_slice(s);
            n += s.len();
        }};
    }
    put!(b"{\"trades\":");
    let mut tmp = Vec::with_capacity(20);
    write_u64(&mut tmp, trades);
    put!(&tmp);
    put!(b",\"volume\":");
    tmp.clear();
    write_u64(&mut tmp, volume);
    put!(&tmp);
    put!(b"}");

    out.extend_from_slice(HDR_OK);
    write_u64(out, n as u64);
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(&body[..n]);
}

async fn serve_conn(mut sock: TcpStream, state: Arc<SpinBook>) {
    let _ = sock.set_nodelay(true);
    let mut buf: Vec<u8> = vec![0; 64 * 1024];
    let mut filled = 0usize;
    let mut out: Vec<u8> = Vec::with_capacity(8 * 1024);
    // Промежуточный буфер разобранных ордеров. Живёт по соединению и
    // переиспользуется: аллокаций на запрос нет.
    let mut staging: Vec<OrderMsg> = Vec::with_capacity(STAGING_INIT);

    loop {
        // --- дочитать заголовки ---
        let head_end = loop {
            if let Some(p) = find_crlf2(&buf[..filled], 0) {
                break p + 4;
            }
            if filled == buf.len() {
                buf.resize(buf.len() * 2, 0);
            }
            match sock.read(&mut buf[filled..]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => filled += n,
            }
        };

        let line_end = buf[..head_end]
            .iter()
            .position(|&c| c == b'\r')
            .unwrap_or(0);
        let route = route_of(&buf[..line_end]);
        let clen = content_length(&buf[..head_end]);

        // --- дочитать тело ---
        let need = head_end + clen;
        while filled < need {
            if need > buf.len() {
                buf.resize(need.next_power_of_two(), 0);
            }
            match sock.read(&mut buf[filled..]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => filled += n,
            }
        }

        out.clear();
        {
            let body = &buf[head_end..need];
            match route {
                Route::Order => {
                    if let Some(m) = scan_single(body) {
                        let mut g = state.lock();
                        let b = g.book();
                        let _ = b.submit(m.id, m.price, m.quantity, side_of(m.side));
                        let (t, v) = (b.trades_count, b.matched_volume);
                        drop(g);
                        respond_engine(&mut out, t, v);
                    } else {
                        out.extend_from_slice(HDR_400);
                    }
                }
                Route::Batch => {
                    // Разбор идёт СНАРУЖИ критической секции. Это принципиально:
                    // матчинг сотни ордеров стоит около микросекунды, а разбор
                    // пяти килобайт JSON — около восьми. Держать лок во время
                    // разбора значит сериализовать всю работу на одном ядре;
                    // измерено — сервер упирался ровно в 99% одного ядра.
                    // Теперь разбор параллелится по шардам, а сериализуется
                    // только сам матчинг.
                    let n = scan_batch(body, |m| staging.push(m));
                    if n.is_some() {
                        let mut g = state.lock();
                        let b = g.book();
                        for m in staging.iter() {
                            let _ = b.submit(m.id, m.price, m.quantity, side_of(m.side));
                        }
                        let (t, v) = (b.trades_count, b.matched_volume);
                        drop(g);
                        respond_engine(&mut out, t, v);
                    } else {
                        out.extend_from_slice(HDR_400);
                    }
                    staging.clear();
                }
                Route::BatchBin => {
                    // Бинарный протокол: поток записей по 24 байта
                    // little-endian [id u64][price u64][qty u32][side u8][pad u24].
                    // Декодирование — тоже вне лока.
                    const REC: usize = 24;
                    if !body.len().is_multiple_of(REC) {
                        out.extend_from_slice(HDR_400);
                    } else {
                        for c in body.chunks_exact(REC) {
                            staging.push(OrderMsg {
                                id: u64::from_le_bytes(c[0..8].try_into().unwrap()),
                                price: u64::from_le_bytes(c[8..16].try_into().unwrap()),
                                quantity: u32::from_le_bytes(c[16..20].try_into().unwrap()) as u64,
                                side: c[20],
                            });
                        }
                        let mut g = state.lock();
                        let b = g.book();
                        for m in staging.iter() {
                            let _ = b.submit(m.id, m.price, m.quantity, side_of(m.side));
                        }
                        let (t, v) = (b.trades_count, b.matched_volume);
                        drop(g);
                        respond_engine(&mut out, t, v);
                        staging.clear();
                    }
                }
                Route::Stats => {
                    let mut g = state.lock();
                    let b = g.book();
                    let (t, v, bl, al, r, rej) = (
                        b.trades_count,
                        b.matched_volume,
                        b.bid_levels(),
                        b.ask_levels(),
                        b.resting_orders(),
                        b.rejected,
                    );
                    drop(g);
                    let mut body = Vec::with_capacity(128);
                    body.extend_from_slice(b"{\"trades\":");
                    write_u64(&mut body, t);
                    body.extend_from_slice(b",\"volume\":");
                    write_u64(&mut body, v);
                    body.extend_from_slice(b",\"bid_levels\":");
                    write_u64(&mut body, bl as u64);
                    body.extend_from_slice(b",\"ask_levels\":");
                    write_u64(&mut body, al as u64);
                    body.extend_from_slice(b",\"resting\":");
                    write_u64(&mut body, r as u64);
                    // Ордера вне ценовой полосы отвергаются (limit-up/limit-down);
                    // счётчик публикуется, чтобы потеря была видна, а не молчалива.
                    body.extend_from_slice(b",\"rejected\":");
                    write_u64(&mut body, rej);
                    body.extend_from_slice(b"}");
                    out.extend_from_slice(HDR_OK);
                    write_u64(&mut out, body.len() as u64);
                    out.extend_from_slice(b"\r\n\r\n");
                    out.extend_from_slice(&body);
                }
                Route::Reset => {
                    state.lock().book().reset();
                    out.extend_from_slice(HDR_204);
                }
                Route::Unknown => out.extend_from_slice(HDR_404),
            }
        }

        if sock.write_all(&out).await.is_err() {
            return;
        }

        // --- сдвинуть остаток (HTTP-пайплайнинг) ---
        if filled > need {
            buf.copy_within(need..filled, 0);
            filled -= need;
        } else {
            filled = 0;
        }
    }
}

/// Поднять QoS текущего потока до USER_INTERACTIVE (только macOS).
#[cfg(target_os = "macos")]
fn pin_qos_user_interactive() {
    // SAFETY: публичный API Darwin, действует только на текущий поток.
    unsafe {
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
#[cfg(not(target_os = "macos"))]
fn pin_qos_user_interactive() {}

/// Число производительных ядер. На Apple Silicon логических ядер больше,
/// чем performance-ядер, и запускать воркеры на efficiency-ядрах вредно.
fn perf_cores() -> usize {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: sysctlbyname с корректным именем, буфером нужного размера
        // и валидными указателями; при ошибке значение остаётся нетронутым.
        unsafe {
            extern "C" {
                fn sysctlbyname(
                    name: *const i8,
                    oldp: *mut core::ffi::c_void,
                    oldlenp: *mut usize,
                    newp: *mut core::ffi::c_void,
                    newlen: usize,
                ) -> i32;
            }
            let mut v: i32 = 0;
            let mut len = core::mem::size_of::<i32>();
            let name = c"hw.perflevel0.logicalcpu";
            if sysctlbyname(
                name.as_ptr(),
                &mut v as *mut i32 as *mut core::ffi::c_void,
                &mut len,
                core::ptr::null_mut(),
                0,
            ) == 0
                && v > 0
            {
                return v as usize;
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn reuseport_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    // Ключевой момент шардирования: несколько сокетов на одном порту,
    // ядро само раскладывает входящие соединения по шардам.
    sock.set_reuse_port(true)?;
    sock.set_tcp_nodelay(true)?;
    sock.bind(&addr.into())?;
    sock.listen(4096)?;
    sock.set_nonblocking(true)?;
    Ok(sock.into())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:8082".to_string())
        .parse()
        .expect("некорректный адрес");
    // По умолчанию — число PERFORMANCE-ядер, а не всех логических.
    // Измерено на M4 (4P + 6E): 4 воркера дают 18.4M ордеров/с, 10 воркеров —
    // 16.8M при большем расходе CPU. E-ядра здесь только добавляют
    // конкуренцию за спинлок, не добавляя пропускной способности.
    let shards: usize = std::env::var("SHARDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(perf_cores)
        .max(1);

    // Ёмкость арены ордеров. Рост арены — это O(n) копирование, которое
    // видно в хвосте задержек, поэтому размер задаётся заранее исходя из
    // максимального числа одновременно открытых ордеров площадки.
    let arena: usize = std::env::var("ARENA")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1 << 24);

    // QoS-класс воркеров (только macOS), включается QOS=1. Гипотеза была, что
    // USER_INTERACTIVE уберёт миллисекундные выбросы хвоста, вызванные
    // вытеснением воркеров. Замер (batch=100, 64 соединения, 3 чередующихся
    // пары): max 14.2/4.8/5.6 мс без QoS против 10.9/4.0/10.7 мс с QoS, p99.9
    // без систематической разницы. Эффекта нет, поэтому по умолчанию выключено:
    // выбросы приходят не от приоритета потока.
    let qos_enabled = std::env::var("QOS").map(|v| v == "1").unwrap_or(false);

    let state = Arc::new(SpinBook::new(OrderBook::new(arena, 10_000)));
    println!(
        "rust matching-engine: {addr}, воркеров: {shards}, арена: {arena}, qos: {qos_enabled}"
    );

    // Раздача соединений по воркерам делается ЯВНО, а не через SO_REUSEPORT.
    //
    // Причина: на Darwin SO_REUSEPORT не балансирует входящие соединения между
    // сокетами, как это делает Linux, — весь трафик достаётся одному сокету.
    // Измерено: из десяти потоков работал ровно один (98% CPU против 0% у
    // остальных девяти). Поэтому accept идёт в одном месте, а сокеты
    // раскладываются по воркерам круговым перебором. На Linux флаг остаётся
    // полезным, но корректность раскладки больше от него не зависит.
    let mut senders = Vec::with_capacity(shards);
    let mut handles = Vec::with_capacity(shards);
    for shard in 0..shards {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::net::TcpStream>();
        senders.push(tx);
        let state = Arc::clone(&state);
        let qos = qos_enabled;
        handles.push(std::thread::spawn(move || {
            if qos {
                pin_qos_user_interactive();
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .thread_name(format!("worker-{shard}"))
                .build()
                .expect("не удалось создать рантайм");
            rt.block_on(async move {
                // Ожидание нового соединения не крутит процессор:
                // recv().await парковано в цикле событий рантайма.
                while let Some(std_sock) = rx.recv().await {
                    if std_sock.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if let Ok(sock) = TcpStream::from_std(std_sock) {
                        tokio::spawn(serve_conn(sock, Arc::clone(&state)));
                    }
                }
            });
        }));
    }

    let listener = reuseport_listener(addr).expect("не удалось открыть слушающий сокет");
    listener.set_nonblocking(false).expect("blocking accept");
    let mut next = 0usize;
    loop {
        match listener.accept() {
            Ok((sock, _)) => {
                let _ = sock.set_nodelay(true);
                // Круговой перебор: соединения долгоживущие (keep-alive),
                // поэтому простого round-robin достаточно.
                let i = next % shards;
                next += 1;
                // Сокет передаётся во владение воркеру; при мёртвом воркере
                // он просто закроется вместе с ошибкой отправки.
                let _ = senders[i].send(sock);
            }
            Err(_) => continue,
        }
    }
}
