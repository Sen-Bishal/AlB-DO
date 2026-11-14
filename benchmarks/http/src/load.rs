// Based on the CTO's T-0035 reference/loadgen-main.rs: HTTP/1.1 keep-alive,
// one in-flight request per connection and rotating forwarded client addresses.
use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const IO_TIMEOUT: Duration = Duration::from_secs(30);

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

async fn line<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<String> {
    let mut bytes = Vec::new();
    let n = r.take(8193).read_until(b'\n', &mut bytes).await?;
    if n == 0 {
        return Err(ErrorKind::UnexpectedEof.into());
    }
    if n > 8192 || !bytes.ends_with(b"\r\n") {
        return Err(invalid("invalid or oversized HTTP line"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("non-UTF8 HTTP line"))
}

async fn discard<R: AsyncBufRead + Unpin>(
    r: &mut R,
    n: u64,
    capture: &mut Option<&mut Vec<u8>>,
) -> io::Result<()> {
    if let Some(bytes) = capture {
        let n = usize::try_from(n).map_err(|_| invalid("fixture body too large"))?;
        let start = bytes.len();
        let end = start
            .checked_add(n)
            .filter(|end| *end <= 32 * 1024 * 1024)
            .ok_or_else(|| invalid("fixture body exceeds 32 MiB"))?;
        bytes.resize(end, 0);
        r.read_exact(&mut bytes[start..end]).await?;
        return Ok(());
    }
    // Drain without allocating a response-sized buffer on every request.
    let read = tokio::io::copy(&mut r.take(n), &mut tokio::io::sink()).await?;
    if read != n {
        return Err(ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

pub async fn response<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<(u16, u64, bool)> {
    response_into(r, None).await
}

pub async fn response_into<R: AsyncBufRead + Unpin>(
    r: &mut R,
    mut capture: Option<&mut Vec<u8>>,
) -> io::Result<(u16, u64, bool)> {
    let status_line = line(r).await?;
    let mut fields = status_line.split_whitespace();
    if fields.next() != Some("HTTP/1.1") {
        return Err(invalid("expected HTTP/1.1"));
    }
    let status = fields
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (200..=599).contains(s))
        .ok_or_else(|| invalid("invalid HTTP status"))?;
    let (mut length, mut chunked, mut close) = (None, false, false);
    let mut header_bytes = status_line.len();
    loop {
        let header = line(r).await?;
        header_bytes += header.len();
        if header_bytes > 64 * 1024 {
            return Err(invalid("oversized HTTP headers"));
        }
        if header == "\r\n" {
            break;
        }
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| invalid("invalid HTTP header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(invalid("duplicate content-length"));
            }
            length = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| invalid("invalid content-length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") {
                return Err(invalid("unsupported transfer encoding"));
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("connection") {
            close = value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("close"));
        }
    }
    if chunked && length.is_some() {
        return Err(invalid("ambiguous HTTP framing"));
    }
    let mut body = 0u64;
    if chunked {
        loop {
            let size = line(r).await?;
            let n = u64::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| invalid("invalid chunk size"))?;
            if n == 0 {
                let mut trailer_bytes = 0;
                loop {
                    let trailer = line(r).await?;
                    trailer_bytes += trailer.len();
                    if trailer_bytes > 64 * 1024 {
                        return Err(invalid("oversized HTTP trailers"));
                    }
                    if trailer == "\r\n" {
                        break;
                    }
                    if !trailer.contains(':') {
                        return Err(invalid("invalid HTTP trailer"));
                    }
                }
                break;
            }
            discard(r, n, &mut capture).await?;
            let mut end = [0; 2];
            r.read_exact(&mut end).await?;
            if end != *b"\r\n" {
                return Err(invalid("invalid chunk terminator"));
            }
            body = body
                .checked_add(n)
                .ok_or_else(|| invalid("body size overflow"))?;
        }
    } else if let Some(n) = length {
        discard(r, n, &mut capture).await?;
        body = n;
    } else if status != 204 && status != 304 {
        return Err(invalid("response has no explicit body framing"));
    }
    Ok((status, body, close))
}

#[derive(Default, Debug)]
pub struct Stats {
    pub statuses: BTreeMap<u16, u64>,
    pub errors: u64,
    pub first_error: Option<String>,
    pub bytes: u64,
    pub latency_ns: u128,
    pub elapsed: f64,
}

impl Stats {
    pub fn ok(&self) -> u64 {
        self.statuses.get(&200).copied().unwrap_or(0)
    }
    pub fn total(&self) -> u64 {
        self.statuses.values().sum()
    }
    pub fn non200(&self) -> u64 {
        self.total() - self.ok()
    }
    pub fn valid(&self) -> bool {
        self.ok() > 0 && self.non200() == 0 && self.errors == 0
    }
    pub fn rate(&self) -> f64 {
        self.ok() as f64 / self.elapsed
    }
    pub fn counts(&self) -> String {
        self.statuses
            .iter()
            .map(|(s, n)| format!("{s}={n}"))
            .collect::<Vec<_>>()
            .join(",")
    }
    fn error(&mut self, error: impl ToString) {
        self.errors += 1;
        if self.first_error.is_none() {
            self.first_error = Some(error.to_string());
        }
    }
}

pub async fn run(
    addr: &str,
    path: &str,
    conns: usize,
    duration: Duration,
    counter: Arc<AtomicU64>,
) -> Stats {
    let start = Instant::now();
    let deadline = start + duration;
    let mut workers = Vec::new();
    for _ in 0..conns {
        let (addr, path, counter) = (addr.to_owned(), path.to_owned(), counter.clone());
        workers.push(tokio::spawn(async move {
            let mut stats = Stats::default();
            let socket = match tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(&addr)).await {
                Ok(Ok(socket)) => socket,
                other => { stats.error(format!("connect: {other:?}")); return stats; }
            };
            if let Err(error) = socket.set_nodelay(true) { stats.error(error); return stats; }
            let mut r = BufReader::with_capacity(64 * 1024, socket);
            while Instant::now() < deadline {
                // Persist the counter across warmups, rounds and routes. Exactly
                // 65,536 subjects, with the same accounting paid by both routes.
                let k = counter.fetch_add(1, Relaxed) % 65536;
                let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nX-Forwarded-For: 10.1.{}.{}\r\nAccept: text/html\r\n\r\n", k / 256, k % 256);
                let sent = Instant::now();
                let result = tokio::time::timeout(IO_TIMEOUT, async {
                    r.get_mut().write_all(request.as_bytes()).await?;
                    response(&mut r).await
                }).await;
                match result {
                    Ok(Ok((status, bytes, close))) => {
                        *stats.statuses.entry(status).or_default() += 1;
                        stats.bytes += bytes;
                        stats.latency_ns += sent.elapsed().as_nanos();
                        if close { stats.error("server closed keep-alive connection"); break; }
                    }
                    Ok(Err(error)) => { stats.error(error); break; }
                    Err(error) => { stats.error(error); break; }
                }
            }
            stats
        }));
    }
    let mut total = Stats::default();
    for worker in workers {
        match worker.await {
            Ok(stats) => {
                for (status, n) in stats.statuses {
                    *total.statuses.entry(status).or_default() += n;
                }
                total.errors += stats.errors;
                if total.first_error.is_none() {
                    total.first_error = stats.first_error;
                }
                total.bytes += stats.bytes;
                total.latency_ns += stats.latency_ns;
            }
            Err(error) => total.error(error),
        }
    }
    total.elapsed = start.elapsed().as_secs_f64();
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_content_length_and_refuse_oversized_fixture_body() {
        let mut body = Vec::new();
        let mut wire = &b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"[..];
        assert_eq!(
            response_into(&mut wire, Some(&mut body)).await.unwrap(),
            (200, 3, false)
        );
        assert_eq!(body, b"abc");
        let mut body = Vec::new();
        let mut oversized = &b"HTTP/1.1 200 OK\r\nContent-Length: 33554433\r\n\r\n"[..];
        assert!(response_into(&mut oversized, Some(&mut body))
            .await
            .is_err());
        assert_eq!(
            body.capacity(),
            0,
            "oversized body allocated before refusal"
        );
    }

    #[tokio::test]
    async fn chunked_trailers_do_not_consume_the_next_response() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=x\r\nabc\r\n0\r\nX-Test: yes\r\n\r\nHTTP/1.1 429 Too Many\r\nContent-Length: 2\r\n\r\nno";
        let mut r = &wire[..];
        let mut captured = Vec::new();
        assert_eq!(
            response_into(&mut r, Some(&mut captured)).await.unwrap(),
            (200, 3, false)
        );
        assert_eq!(captured, b"abc");
        assert_eq!(response(&mut r).await.unwrap(), (429, 2, false));
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn malformed_or_truncated_framing_fails() {
        for wire in [
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Length: wrong\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx!!",
            "HTTP/1.1 200 OK\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert!(response(&mut wire.as_bytes()).await.is_err(), "{wire}");
        }
    }

    #[tokio::test]
    async fn missing_headers_terminator_is_an_error() {
        assert!(
            response(&mut &b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n"[..])
                .await
                .is_err()
        );
    }
}
