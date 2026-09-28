//! `loom-daemon worker proxy-rotate` — the in-container half of host-side
//! account rotation (issue #8818).
//!
//! Run by `claude-wrapper.sh` inside a proxied Claude container when the
//! active account is exhausted, auth-dead or out of concurrent-session slots.
//! It asks the host proxy (at `$ANTHROPIC_BASE_URL`, authorized by the
//! placeholder in `$CLAUDE_CODE_OAUTH_TOKEN`) to rotate the launch's account,
//! and on success prints one shell-evalable line:
//!
//! ```text
//! export LOOM_TOKEN_NAME='<new account name>'
//! ```
//!
//! The placeholder is unchanged — the proxy swapped the credential behind it —
//! so the wrapper retries with the same environment.
//!
//! # Refusals
//!
//! Exit 78 without sending anything when the credential variable does not hold
//! a Loom placeholder (this path must never put a REAL credential on the wire,
//! even to a proxy) or the base URL is not a plain-`http` proxy URL. Exit 1 when
//! the proxy refused or could not be reached; stderr names the proxy's reason
//! token (`no_upstream_evidence`, `pool_exhausted`, …), never a body verbatim.

use super::registry::Upstream;
use super::rotation::{is_account_name, ROTATE_PATH};
use super::PLACEHOLDER_PREFIX;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Arguments for `loom-daemon worker proxy-rotate`.
#[derive(clap::Args, Debug)]
pub struct RotateArgs {
    /// Why the current account failed: `usage-limit`, `session-window`,
    /// `auth-dead` or `concurrent-session` (no bad-mark, swap only).
    #[arg(long, value_name = "REASON", value_parser = ["usage-limit", "session-window", "auth-dead", "concurrent-session"])]
    pub reason: String,
    /// Narrow an exhaustion mark to the model class the host is running.
    #[arg(long)]
    pub model_scoped: bool,
    /// Variable holding this launch's placeholder.
    #[arg(long, value_name = "VAR", default_value = "CLAUDE_CODE_OAUTH_TOKEN")]
    pub credential_env: String,
    /// Variable holding the proxy's base URL.
    #[arg(long, value_name = "VAR", default_value = "ANTHROPIC_BASE_URL")]
    pub base_url_env: String,
}

/// A failed rotation: exit code plus a secret-free message.
#[derive(Debug, PartialEq, Eq)]
pub struct RotateError {
    pub code: i32,
    pub message: String,
}

fn config(message: impl Into<String>) -> RotateError {
    RotateError {
        code: 78,
        message: message.into(),
    }
}

fn failed(message: impl Into<String>) -> RotateError {
    RotateError {
        code: 1,
        message: message.into(),
    }
}

/// Ask the proxy at `base_url` to rotate the launch `placeholder` identifies.
/// Returns the new account's name.
pub fn request_rotation(
    base_url: &str,
    placeholder: &str,
    reason: &str,
    model_scoped: bool,
) -> Result<String, RotateError> {
    if !placeholder.starts_with(PLACEHOLDER_PREFIX) {
        return Err(config(
            "proxy-rotate: the credential variable does not hold a Loom placeholder; \
             this is not a proxied launch, refusing to send it anywhere",
        ));
    }
    if !base_url.trim().to_ascii_lowercase().starts_with("http://") {
        return Err(config("proxy-rotate: the base URL is not a plain-http proxy URL"));
    }
    let proxy = Upstream::parse(base_url)
        .map_err(|why| config(format!("proxy-rotate: base URL: {why}")))?;
    let port = proxy.port().unwrap_or(80);
    let addr = (proxy.host(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| failed("proxy-rotate: cannot resolve the proxy address"))?;

    let body = serde_json::json!({ "reason": reason, "modelScoped": model_scoped }).to_string();
    let request = format!(
        "POST {ROTATE_PATH} HTTP/1.1\r\nhost: {}\r\nauthorization: Bearer {placeholder}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        proxy.authority(),
        body.len()
    );
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| failed(format!("proxy-rotate: cannot reach the proxy: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(120))).ok();
    stream
        .write_all(request.as_bytes())
        .map_err(|e| failed(format!("proxy-rotate: send failed: {e}")))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| failed(format!("proxy-rotate: read failed: {e}")))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap_or_default();
    if status != 200 {
        let why = json
            .get("reason")
            .and_then(|v| v.as_str())
            .filter(|t| t.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
            .unwrap_or("unrecognized_response");
        return Err(failed(format!("proxy-rotate: refused by the proxy ({status} {why})")));
    }
    json.get("account")
        .and_then(|v| v.as_str())
        .filter(|name| is_account_name(name))
        .map(str::to_string)
        .ok_or_else(|| failed("proxy-rotate: the proxy returned no usable account name"))
}

/// Entry point for `loom-daemon worker proxy-rotate`.
pub fn cli(args: RotateArgs) -> anyhow::Result<()> {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    match request_rotation(
        &var(&args.base_url_env),
        &var(&args.credential_env),
        &args.reason,
        args.model_scoped,
    ) {
        Ok(account) => {
            println!("export LOOM_TOKEN_NAME='{account}'");
            Ok(())
        }
        Err(error) => {
            eprintln!("{}", error.message);
            std::process::exit(error.code);
        }
    }
}
