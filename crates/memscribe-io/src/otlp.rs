//! A loopback-only OTLP/HTTP receiver (whitepaper §7, §11).
//!
//! Behind the `otlp` cargo feature, this exposes a tiny HTTP/1.1 endpoint that
//! ingests OpenTelemetry GenAI records pushed over the network and turns each
//! one into a [`memscribe_core::RawRecord`] — exactly the shape the existing
//! OTel adapter ([`memscribe_adapters::otel`]) already knows how to parse. The
//! pipeline downstream of the source layer is unchanged: bytes in, normalized
//! events out.
//!
//! ## Design (deliberately minimal)
//!
//! - **No async runtime, no tonic, no `hyper`.** A receiver that only needs to
//!   accept short JSON POSTs on loopback does not justify pulling a full async
//!   stack into Memscribe. We use [`std::net::TcpListener`] with a hand-rolled
//!   HTTP/1.1 request reader that understands exactly the two things a pusher
//!   sends: a request line, headers, and a `Content-Length`-delimited body.
//! - **Loopback only (§11).** [`OtlpReceiver::bind`] rejects any address whose
//!   IP is not a loopback address, so the default build never opens a port that
//!   is reachable off-host. There is no flag to widen this.
//! - **Lossless + version-tolerant, like every adapter.** The body may be
//!   OTLP/HTTP-JSON (one JSON object, or a JSON array of records) or NDJSON
//!   (one JSON record per line). Each record becomes one `RawRecord` carrying a
//!   synthetic [`SourceLocation`] (`otlp://<peer>` with a per-record line
//!   number) so the adapter and any audit replay see the same provenance shape
//!   they get from a file.
//! - **Never panics on malformed input.** A body that is not valid JSON / not
//!   valid UTF-8 yields `400 Bad Request` and the server keeps serving. The
//!   accept loop swallows per-connection I/O errors rather than tearing down.
//!
//! ## Usage
//!
//! ```no_run
//! use memscribe_io::otlp::OtlpReceiver;
//! use std::sync::mpsc;
//!
//! let recv = OtlpReceiver::bind("127.0.0.1:0").unwrap();
//! let (tx, rx) = mpsc::channel();
//! let handle = recv.serve_into(tx);
//! // ... another part of the program drains `rx` for batches of RawRecords ...
//! handle.shutdown();
//! ```

use memscribe_core::{RawRecord, SourceLocation};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Maximum request body we will buffer, in bytes. OTel GenAI records are small;
/// this is a guard against a misbehaving or hostile local pusher, not a tuning
/// knob. Bodies larger than this are rejected with `413 Payload Too Large`.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of header lines we will read before giving up on a request.
const MAX_HEADER_LINES: usize = 256;

/// A loopback-only HTTP receiver for OpenTelemetry GenAI records.
///
/// Construct with [`OtlpReceiver::bind`], then drive it with one of the serve
/// methods. Each accepted POST body is decoded into zero or more
/// [`RawRecord`]s that the OTel adapter can parse.
#[derive(Debug)]
pub struct OtlpReceiver {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl OtlpReceiver {
    /// Bind the receiver to `addr`, which **must** resolve to a loopback
    /// address (`127.0.0.0/8` or `::1`). Pass port `0` for an OS-assigned
    /// ephemeral port and read it back with [`OtlpReceiver::local_addr`].
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the address does not resolve, resolves to a
    /// non-loopback address ([`io::ErrorKind::InvalidInput`]), or the socket
    /// cannot be bound.
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let mut last_err = io::Error::new(
            io::ErrorKind::InvalidInput,
            "no socket address resolved from input",
        );
        for candidate in addr.to_socket_addrs()? {
            if !candidate.ip().is_loopback() {
                last_err = io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "OTLP receiver refuses to bind non-loopback address {candidate} \
                         (whitepaper §11: loopback only)"
                    ),
                );
                continue;
            }
            match TcpListener::bind(candidate) {
                Ok(listener) => {
                    let local_addr = listener.local_addr()?;
                    return Ok(OtlpReceiver {
                        listener,
                        local_addr,
                    });
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// The address the receiver is actually listening on (resolves port `0`).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the blocking accept loop on the current thread, handing every batch
    /// of decoded [`RawRecord`]s to `on_batch`. Returns when `should_stop`
    /// observes `true` (set it from another thread, then poke the listener —
    /// see [`OtlpReceiver::serve_into`] / [`ServeHandle`] for the wired-up
    /// version).
    ///
    /// A connection that yields no records (empty or malformed body) still gets
    /// a response; `on_batch` is simply not called for it.
    pub fn serve_blocking<F>(&self, mut on_batch: F, should_stop: &AtomicBool)
    where
        F: FnMut(Vec<RawRecord>),
    {
        for stream in self.listener.incoming() {
            if should_stop.load(Ordering::SeqCst) {
                break;
            }
            match stream {
                Ok(stream) => {
                    // A single malformed/hostile connection must never take the
                    // server down: handle it, log nothing fancy, keep serving.
                    let records = handle_connection(stream, self.local_addr);
                    if let Ok(records) = records {
                        if !records.is_empty() {
                            on_batch(records);
                        }
                    }
                }
                // Accept errors (e.g. the wake-up poke on shutdown) are not
                // fatal; re-check the stop flag and continue.
                Err(_) => {
                    if should_stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
    }

    /// Spawn the accept loop on a background thread and stream every decoded
    /// batch of [`RawRecord`]s into `sink`. Returns a [`ServeHandle`] for
    /// graceful shutdown.
    ///
    /// The channel send is the back-pressure boundary: if the receiver of
    /// `sink` is dropped, the loop stops on the next batch.
    #[must_use]
    pub fn serve_into(self, sink: Sender<Vec<RawRecord>>) -> ServeHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let local_addr = self.local_addr;
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            self.serve_blocking(
                |batch| {
                    // A dropped consumer means "stop": flip the flag so the
                    // loop exits after this connection.
                    if sink.send(batch).is_err() {
                        thread_stop.store(true, Ordering::SeqCst);
                    }
                },
                &thread_stop,
            );
        });
        ServeHandle {
            stop,
            local_addr,
            join: Some(join),
        }
    }
}

/// Handle to a running [`OtlpReceiver`] accept loop, for graceful shutdown.
///
/// Dropping the handle requests shutdown and joins the loop thread.
#[derive(Debug)]
pub struct ServeHandle {
    stop: Arc<AtomicBool>,
    local_addr: SocketAddr,
    join: Option<JoinHandle<()>>,
}

impl ServeHandle {
    /// The address the underlying receiver is listening on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Request shutdown and block until the accept loop has stopped.
    ///
    /// Sets the stop flag, then opens (and immediately drops) one loopback
    /// connection to wake the blocking `accept()` so the loop can observe the
    /// flag and return promptly.
    pub fn shutdown(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Poke the listener so a thread parked in `accept()` wakes up.
        if let Ok(stream) = TcpStream::connect(self.local_addr) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ServeHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.signal_and_join();
        }
    }
}

/// Read and answer a single HTTP/1.1 request, returning the decoded records.
///
/// Always writes a response (so the client never hangs) and never propagates a
/// decode failure as an error — a bad body is a `400`, not a server fault. The
/// only `Err` returned is a genuine transport failure on this one connection,
/// which the caller treats as "skip and keep serving".
fn handle_connection(stream: TcpStream, local_addr: SocketAddr) -> io::Result<Vec<RawRecord>> {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| local_addr.to_string(), |p| p.to_string());
    let mut reader = BufReader::new(stream);

    match read_request(&mut reader) {
        Ok(Request::Post { body }) => match decode_body(&body, &peer) {
            Ok(records) => {
                let n = records.len();
                write_response(
                    reader.get_mut(),
                    200,
                    "OK",
                    &format!("{{\"accepted\":{n}}}"),
                )?;
                Ok(records)
            }
            Err(BodyError::TooLarge) => {
                write_response(
                    reader.get_mut(),
                    413,
                    "Payload Too Large",
                    "{\"error\":\"body too large\"}",
                )?;
                Ok(Vec::new())
            }
            Err(BodyError::Malformed) => {
                write_response(
                    reader.get_mut(),
                    400,
                    "Bad Request",
                    "{\"error\":\"malformed body\"}",
                )?;
                Ok(Vec::new())
            }
        },
        Ok(Request::Other) => {
            // Any non-POST verb (GET health check, etc.) is answered but
            // produces no records.
            write_response(
                reader.get_mut(),
                405,
                "Method Not Allowed",
                "{\"error\":\"POST only\"}",
            )?;
            Ok(Vec::new())
        }
        Err(RequestError::TooLarge) => {
            write_response(
                reader.get_mut(),
                413,
                "Payload Too Large",
                "{\"error\":\"body too large\"}",
            )?;
            Ok(Vec::new())
        }
        Err(RequestError::Malformed) => {
            write_response(
                reader.get_mut(),
                400,
                "Bad Request",
                "{\"error\":\"malformed request\"}",
            )?;
            Ok(Vec::new())
        }
        // A genuine transport error (peer hung up mid-headers, etc.): best
        // effort, swallow it and move on without crashing.
        Err(RequestError::Io) => Ok(Vec::new()),
    }
}

/// The parsed request, reduced to what the receiver cares about.
enum Request {
    Post { body: Vec<u8> },
    Other,
}

enum RequestError {
    Malformed,
    TooLarge,
    /// A genuine transport failure on this connection. We only branch on the
    /// *kind* — the underlying error is not surfaced, since the policy is the
    /// same for every transport fault: drop this connection, keep serving.
    Io,
}

impl From<io::Error> for RequestError {
    fn from(_: io::Error) -> Self {
        RequestError::Io
    }
}

/// Parse the request line + headers, then read a `Content-Length`-delimited
/// body. Deliberately strict and tiny: we only support exactly what a local
/// OTLP pusher needs (no chunked transfer-encoding, no keep-alive).
fn read_request<R: Read>(reader: &mut BufReader<R>) -> Result<Request, RequestError> {
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        // Connection opened and closed with no data (e.g. the shutdown poke).
        return Err(RequestError::Malformed);
    }
    let method = request_line
        .split_whitespace()
        .next()
        .ok_or(RequestError::Malformed)?;
    let is_post = method.eq_ignore_ascii_case("POST");

    let mut content_length: Option<usize> = None;
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line terminates the header block.
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| RequestError::Malformed)?,
                );
            }
        }
    }

    if !is_post {
        return Ok(Request::Other);
    }

    let len = content_length.unwrap_or(0);
    if len > MAX_BODY_BYTES {
        return Err(RequestError::TooLarge);
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Request::Post { body })
}

/// Why a body could not be turned into records.
enum BodyError {
    Malformed,
    TooLarge,
}

/// Turn a POSTed body into zero or more [`RawRecord`]s.
///
/// Accepts three shapes, in priority order:
/// 1. A single JSON **array** of records → one `RawRecord` per element.
/// 2. A single JSON **object** (OTLP/HTTP-JSON, possibly pretty-printed) → one
///    `RawRecord` holding the object serialized to a single compact line.
/// 3. **NDJSON** — one JSON value per line → one `RawRecord` per non-blank line.
///
/// Every emitted record's bytes are a single compact JSON line, which is
/// exactly what the OTel adapter parses (it reads one JSON value per record).
fn decode_body(body: &[u8], peer: &str) -> Result<Vec<RawRecord>, BodyError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(BodyError::TooLarge);
    }
    let text = std::str::from_utf8(body).map_err(|_| BodyError::Malformed)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // Shape 1 & 2: the whole body is one JSON value.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Ok(records_from_value(value, peer));
    }

    // Shape 3: NDJSON. Each non-blank line must itself be valid JSON, otherwise
    // the body is malformed and we reject the whole thing (the pusher sent us
    // something we cannot losslessly route).
    let mut out = Vec::new();
    let mut line_no = 1u64;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Validate each line is JSON so a half-written stream becomes a 400
        // rather than silently producing Unknown events downstream.
        if serde_json::from_str::<serde_json::Value>(line).is_err() {
            return Err(BodyError::Malformed);
        }
        out.push(mk_record(line.as_bytes().to_vec(), peer, line_no));
        line_no += 1;
    }
    Ok(out)
}

/// Expand a single parsed JSON value into records: arrays fan out element-wise,
/// everything else (object / scalar) becomes one record.
fn records_from_value(value: serde_json::Value, peer: &str) -> Vec<RawRecord> {
    match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let line = serde_json::to_string(&item).unwrap_or_else(|_| item.to_string());
                mk_record(line.into_bytes(), peer, (i as u64) + 1)
            })
            .collect(),
        other => {
            let line = serde_json::to_string(&other).unwrap_or_else(|_| other.to_string());
            vec![mk_record(line.into_bytes(), peer, 1)]
        }
    }
}

/// Build one `RawRecord` with a synthetic `otlp://<peer>` source location.
fn mk_record(bytes: Vec<u8>, peer: &str, line_no: u64) -> RawRecord {
    RawRecord::new(
        bytes,
        SourceLocation::new(format!("otlp://{peer}"), 0, line_no),
    )
}

/// Write a minimal HTTP/1.1 response with a JSON body and close the connection.
fn write_response<W: Write>(
    writer: &mut W,
    status: u16,
    reason: &str,
    body: &str,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    writer.write_all(response.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Duration;

    /// POST `body` to `addr` and return the raw HTTP response text.
    fn post(addr: SocketAddr, content_type: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect to receiver");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let request = format!(
            "POST /v1/logs HTTP/1.1\r\n\
             Host: 127.0.0.1\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            len = body.len(),
        );
        stream.write_all(request.as_bytes()).expect("write request");
        stream.flush().expect("flush request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
    }

    const GENAI_RECORD: &str = r#"{"time":"2026-06-22T10:00:05Z","gen_ai.operation.name":"chat","gen_ai.conversation.id":"sess-1","gen_ai.cli.user_prompt":"Use Postgres."}"#;

    #[test]
    fn bind_rejects_non_loopback() {
        // 0.0.0.0 binds every interface — must be refused (whitepaper §11).
        let err = OtlpReceiver::bind("0.0.0.0:0").expect_err("must refuse non-loopback");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn bind_accepts_loopback_ephemeral_port() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind loopback");
        assert!(recv.local_addr().ip().is_loopback());
        assert_ne!(recv.local_addr().port(), 0, "ephemeral port resolved");
    }

    #[test]
    fn post_single_json_object_yields_one_roundtripping_record() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        let response = post(addr, "application/json", GENAI_RECORD);
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

        let batch = rx.recv_timeout(Duration::from_secs(5)).expect("a batch");
        assert_eq!(batch.len(), 1, "one record for one object");
        let rec = &batch[0];

        // The record's bytes must be valid JSON that round-trips to the same
        // logical value the pusher sent — that is what the adapter parses.
        let sent: serde_json::Value = serde_json::from_str(GENAI_RECORD).unwrap();
        let got: serde_json::Value =
            serde_json::from_slice(&rec.bytes).expect("record bytes are JSON");
        assert_eq!(got, sent, "record round-trips the posted GenAI record");

        // Provenance points at the loopback peer, not a file.
        assert!(rec.location.file.to_string_lossy().starts_with("otlp://"));

        handle.shutdown();
    }

    #[test]
    fn post_json_array_fans_out_to_one_record_each() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        let body = format!("[{GENAI_RECORD},{GENAI_RECORD}]");
        let response = post(addr, "application/json", &body);
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

        let batch = rx.recv_timeout(Duration::from_secs(5)).expect("a batch");
        assert_eq!(batch.len(), 2, "two array elements → two records");
        for rec in &batch {
            serde_json::from_slice::<serde_json::Value>(&rec.bytes)
                .expect("each record is valid JSON");
        }

        handle.shutdown();
    }

    #[test]
    fn post_ndjson_yields_one_record_per_line() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        let body = format!("{GENAI_RECORD}\n{GENAI_RECORD}\n{GENAI_RECORD}\n");
        let response = post(addr, "application/x-ndjson", &body);
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

        let batch = rx.recv_timeout(Duration::from_secs(5)).expect("a batch");
        assert_eq!(batch.len(), 3, "three lines → three records");
        // Line numbers in provenance are 1-based and monotonic.
        assert_eq!(batch[0].location.line_no, 1);
        assert_eq!(batch[2].location.line_no, 3);

        handle.shutdown();
    }

    #[test]
    fn malformed_body_returns_400_and_server_keeps_serving() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        // Garbage body → 400, no record, no panic.
        let bad = post(addr, "application/json", "{not valid json at all");
        assert!(bad.starts_with("HTTP/1.1 400"), "got: {bad}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "malformed body emits no batch"
        );

        // The server is still alive: a good POST afterwards still works.
        let good = post(addr, "application/json", GENAI_RECORD);
        assert!(good.starts_with("HTTP/1.1 200"), "got: {good}");
        let batch = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server still serving after a bad request");
        assert_eq!(batch.len(), 1);

        handle.shutdown();
    }

    #[test]
    fn empty_body_is_accepted_with_no_records() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        let response = post(addr, "application/json", "");
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "empty body emits no batch"
        );

        handle.shutdown();
    }

    #[test]
    fn non_post_method_is_rejected_without_records() {
        let recv = OtlpReceiver::bind("127.0.0.1:0").expect("bind");
        let addr = recv.local_addr();
        let (tx, rx) = mpsc::channel();
        let handle = recv.serve_into(tx);

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write");
        stream.flush().expect("flush");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(
            response.starts_with("HTTP/1.1 405"),
            "GET should be 405, got: {response}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "GET emits no batch"
        );

        handle.shutdown();
    }
}
