//! A minimal IMDSv2 client: one `PUT` for a session token, then `GET`s.
//!
//! Raw HTTP/1.0 over a [`TcpStream`] with short deadlines: the poller needs
//! nothing more, and a request that cannot connect costs a poll tick, never a
//! hang. The address is a parameter so a test can point it at a local server.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

/// The link-local metadata address.
pub const IMDS_ADDR: &str = "169.254.169.254:80";
/// Session-token path.
pub const TOKEN_PATH: &str = "/latest/api/token";
/// Spot interruption notice: 404 until AWS schedules a reclaim.
pub const SPOT_ACTION_PATH: &str = "/latest/meta-data/spot/instance-action";
/// Rebalance recommendation: 404 until AWS issues one.
pub const REBALANCE_PATH: &str = "/latest/meta-data/events/recommendations/rebalance";
/// The instance type (also the only metadata a non-spot host has to read).
pub const INSTANCE_TYPE_PATH: &str = "/latest/meta-data/instance-type";

const TIMEOUT: Duration = Duration::from_secs(2);
const TOKEN_TTL_SECS: u32 = 300;

/// A metadata source: `Ok(None)` is a 404 (no notice), `Err` anything else.
pub trait Imds {
    fn get(&self, path: &str) -> Result<Option<String>, String>;
}

/// The real IMDSv2 client.
pub struct Http {
    addr: String,
    token: Mutex<Option<String>>,
}

impl Http {
    #[must_use]
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
            token: Mutex::new(None),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        header: Option<(&str, &str)>,
    ) -> Result<(u16, String), String> {
        let addr: SocketAddr = self
            .addr
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| "no address".to_string())?;
        let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        let mut req =
            format!("{method} {path} HTTP/1.0\r\nHost: {}\r\nContent-Length: 0\r\n", self.addr);
        if let Some((k, v)) = header {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        parse_response(&String::from_utf8_lossy(&raw))
    }

    fn fresh_token(&self) -> Result<String, String> {
        let ttl = TOKEN_TTL_SECS.to_string();
        let (status, body) =
            self.request("PUT", TOKEN_PATH, Some(("X-aws-ec2-metadata-token-ttl-seconds", &ttl)))?;
        if status == 200 {
            Ok(body.trim().to_string())
        } else {
            Err(format!("token request answered {status}"))
        }
    }
}

impl Imds for Http {
    fn get(&self, path: &str) -> Result<Option<String>, String> {
        for attempt in 0..2 {
            let cached = self.token.lock().map_err(|e| e.to_string())?.clone();
            let token = match cached {
                Some(t) => t,
                None => {
                    let t = self.fresh_token()?;
                    *self.token.lock().map_err(|e| e.to_string())? = Some(t.clone());
                    t
                }
            };
            let (status, body) =
                self.request("GET", path, Some(("X-aws-ec2-metadata-token", &token)))?;
            match status {
                200 => return Ok(Some(body)),
                404 => return Ok(None),
                // An expired token: drop it and retry once with a new one.
                401 if attempt == 0 => {
                    *self.token.lock().map_err(|e| e.to_string())? = None;
                }
                other => return Err(format!("{path} answered {other}")),
            }
        }
        Err(format!("{path}: token rejected"))
    }
}

/// Split an HTTP response into its status and body.
fn parse_response(raw: &str) -> Result<(u16, String), String> {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "malformed status line".to_string())?;
    Ok((status, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_splits_into_status_and_body() {
        let (s, b) = parse_response("HTTP/1.0 404 Not Found\r\nA: b\r\n\r\nnope").unwrap();
        assert_eq!((s, b.as_str()), (404, "nope"));
        assert!(parse_response("garbage").is_err());
    }
}
