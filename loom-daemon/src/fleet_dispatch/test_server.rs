//! A tiny in-process fleet-dispatch server for tests: it answers each `POST`
//! with what its handler returns and records every request.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::Value;

type Handler = dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync;

/// One recorded request: its path and JSON body.
pub type Recorded = (String, Value);

pub struct TestServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl TestServer {
    /// Serve `handler` on a loopback port until the test process ends.
    pub fn start(handler: impl Fn(&str, &Value) -> (u16, Value) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (handler, seen) = (Arc::clone(&handler), Arc::clone(&seen));
                std::thread::spawn(move || serve(stream, &*handler, &seen));
            }
        });
        Self { addr, requests }
    }

    /// A server that accepts connections and never answers.
    pub fn silent() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        Self {
            addr,
            requests: Arc::default(),
        }
    }

    /// A loopback URL nothing listens on.
    pub fn dead_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every request so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// The requests to `path` so far.
    pub fn requests_to(&self, path: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|(p, _)| p == path)
            .map(|(_, b)| b)
            .collect()
    }

    /// Wait up to two seconds for `n` requests to `path` (detached calls).
    pub fn wait_for(&self, path: &str, n: usize) -> Vec<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let got = self.requests_to(path);
            if got.len() >= n || std::time::Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn serve(stream: TcpStream, handler: &Handler, seen: &Mutex<Vec<Recorded>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, answer) = handler(&path, &body);
    seen.lock().unwrap().push((path, body));
    let answer = answer.to_string();
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{answer}",
        answer.len()
    );
}
