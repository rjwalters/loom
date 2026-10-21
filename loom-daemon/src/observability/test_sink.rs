//! A hand-rolled loopback HTTP/1.1 sink shared by the exporter tests: no
//! framework dependency, just enough parsing to read one request's path,
//! headers and body off a `std::net::TcpListener` connection and answer it.
//! It runs on a blocking thread (the daemon's tests are not all async), so an
//! async `reqwest::Client` talks to it over a real loopback socket exactly as
//! it would talk to a real sink.
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request as the sink saw it: path, lower-cased header names with their
/// values, and the body.
#[derive(Debug, Clone)]
pub(crate) struct SinkRequest {
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl SinkRequest {
    pub(crate) fn values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// `(status, body, delay)` — one scripted answer.
pub(crate) type Reply = (u16, String, Duration);

/// What the sink answers with: the queued replies first (oldest first), then
/// `fallback` for every request after that.
struct Script {
    queue: VecDeque<Reply>,
    fallback: (u16, String),
    content_type: String,
}

pub(crate) struct HttpSink {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<SinkRequest>>>,
    script: Arc<Mutex<Script>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpSink {
    /// Answer every request with one fixed status and body.
    pub(crate) fn answering(status: u16, body: &str) -> Self {
        Self::scripted(Vec::new(), "application/json", (status, body.to_string()), |_| {})
    }

    /// Answer with `replies` in order, then `fallback`. `check` runs on each
    /// parsed request before it is recorded; a panic in it drops the request.
    pub(crate) fn scripted(
        replies: Vec<Reply>,
        content_type: &str,
        fallback: (u16, String),
        check: fn(&SinkRequest),
    ) -> Self {
        let script = Script {
            queue: replies.into(),
            fallback,
            content_type: content_type.to_string(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Self::serve(listener, Arc::default(), script, check)
    }

    fn serve(
        listener: TcpListener,
        requests: Arc<Mutex<Vec<SinkRequest>>>,
        script: Script,
        check: fn(&SinkRequest),
    ) -> Self {
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let script = Arc::new(Mutex::new(script));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = {
            let (requests, script, shutdown) = (requests.clone(), script.clone(), shutdown.clone());
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).ok();
                            let Some(request) = read_request(&mut stream) else {
                                continue;
                            };
                            check(&request);
                            // Record the request *before* writing the response:
                            // `emit_batch(...).await` on the client side unblocks
                            // as soon as the response bytes are visible, which can
                            // race ahead of this thread's next line. Recording
                            // first guarantees any caller that has observed the
                            // response also observes the recorded request (#7256).
                            requests.lock().unwrap().push(request);
                            let (status, body, delay, content_type) = {
                                let mut script = script.lock().unwrap();
                                let (status, body, delay) =
                                    script.queue.pop_front().unwrap_or_else(|| {
                                        let (status, body) = script.fallback.clone();
                                        (status, body, Duration::ZERO)
                                    });
                                (status, body, delay, script.content_type.clone())
                            };
                            std::thread::sleep(delay);
                            let response = format!("HTTP/1.1 {status} response\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        HttpSink {
            addr,
            requests,
            script,
            shutdown,
            handle: Some(handle),
        }
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn requests(&self) -> Vec<SinkRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn set_status(&self, status: u16) {
        self.script.lock().unwrap().fallback.0 = status;
    }

    pub(crate) fn set_body(&self, body: &str) {
        self.script.lock().unwrap().fallback.1 = body.to_string();
    }

    /// Simulate the sink going offline: join the accept thread and drop the
    /// listener, so a *new* POST hits connection-refused.
    pub(crate) fn kill(mut self) -> (SocketAddr, Arc<Mutex<Vec<SinkRequest>>>) {
        self.stop();
        // `requests` is cloned (both point at the same underlying data — it is
        // an `Arc`) rather than moved, because `Self` implements `Drop` and
        // cannot be partially moved out of.
        (self.addr, self.requests.clone())
    }

    /// Revive a killed sink on the **same** address (rebinding a plain TCP
    /// listening socket with no established connections is safe immediately
    /// after close — no TIME_WAIT is incurred on a never-accepted listener).
    pub(crate) fn revive(addr: SocketAddr, requests: Arc<Mutex<Vec<SinkRequest>>>) -> Self {
        let script = Script {
            queue: VecDeque::new(),
            fallback: (200, String::new()),
            content_type: "application/json".to_string(),
        };
        Self::serve(TcpListener::bind(addr).unwrap(), requests, script, |_| {})
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HttpSink {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read one request: headers, then `Content-Length` bytes of body, with a 5 s
/// read timeout. `None` on a malformed or truncated request.
pub(crate) fn read_request(stream: &mut TcpStream) -> Option<SinkRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let content_length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_end..(header_end + content_length).min(buf.len())].to_vec();
    Some(SinkRequest {
        path,
        headers,
        body,
    })
}
