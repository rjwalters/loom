//! Extra static request headers for an `otlp` exporter entry, read from an
//! owner-only file (Issue #10961).
//!
//! An OTLP receiver behind an identity-aware proxy authenticates with headers
//! the exporter's one `Authorization: Bearer <ingest key>` cannot express (a
//! client id and client secret pair, say). `headers_file` on an
//! `observability.exporters` entry names a file of such headers, so the daemon
//! can reach that receiver without a local collector whose only job is to add
//! them.
//!
//! # File format
//!
//! One `Name: value` per line.
//!
//! - Blank lines, and lines whose first non-blank character is `#`, are
//!   ignored. There are **no inline comments**: `#` is a legal character in a
//!   header value, so everything after the first `:` belongs to the value.
//! - The name is everything before the first `:` and must match the HTTP
//!   `token` grammar (RFC 9110 §5.6.2) exactly — no space before the colon.
//! - Spaces and tabs around the value are trimmed. The value must be non-empty
//!   visible ASCII (interior spaces and tabs allowed).
//! - A name may appear once (names compare case-insensitively).
//! - `Host`, `Content-Type`, `Content-Length`, `Transfer-Encoding` and
//!   `Connection` belong to the exporter and are refused.
//! - A file that yields no header at all is refused: the operator asked for
//!   headers, and silently sending none would only fail later, at the proxy.
//!
//! # Values are secrets
//!
//! Every value is treated as a credential. This module never puts one in a
//! log line, an error, or a `Debug` rendering:
//!
//! - [`HeadersFileError`] names the **file** and, for a bad line, the **line
//!   number** and a fixed reason. It carries no text read from the file — not
//!   even a header name, since a malformed line may be a bare secret.
//! - [`RequestHeaders`] implements `Debug` by hand (names only) and
//!   deliberately implements neither `Display`, `Clone` nor `Serialize`, so it
//!   cannot be formatted or copied into a status surface or a telemetry
//!   record by accident. Each value is additionally marked sensitive to the
//!   HTTP stack.
//!
//! # Permissions
//!
//! On Unix the file must be a regular file with no group or other permission
//! bits (`chmod 600`); anything looser is refused before a byte is parsed.
//! The mode is read from the opened handle, so the file checked is the file
//! read.
//!
//! Not gated behind the `otlp` Cargo feature: the config surface (and the
//! policy below) exists in every build, and nothing here depends on the OTLP
//! wire types.

use std::fmt;
use std::io::Read;
use std::path::Path;

use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};

/// Upper bound on a headers file. A handful of headers is a few hundred
/// bytes; this only stops a mistaken path (a log, a device) being slurped.
pub const MAX_HEADERS_FILE_BYTES: u64 = 64 * 1024;

/// Headers the exporter (or its HTTP client) sets itself. Overriding any of
/// them would corrupt the request rather than authenticate it.
const EXPORTER_OWNED: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// Why one line of a headers file was refused. A fixed vocabulary on purpose:
/// the rendered reason can never contain text from the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// No `:` separating a name from a value.
    MissingColon,
    /// Empty, or a character outside the HTTP `token` grammar.
    InvalidName,
    /// Nothing after the colon once whitespace is trimmed.
    EmptyValue,
    /// A control or non-ASCII character in the value.
    InvalidValue,
    /// The same name appeared on an earlier line.
    DuplicateName,
    /// One of the headers the exporter owns.
    ExporterOwned,
}

impl LineError {
    fn reason(self) -> &'static str {
        match self {
            LineError::MissingColon => "expected `Name: value` (no colon found)",
            LineError::InvalidName => {
                "header name is empty or has a character outside the HTTP token grammar"
            }
            LineError::EmptyValue => "header value is empty",
            LineError::InvalidValue => {
                "header value has a character that is not visible ASCII, space or tab"
            }
            LineError::DuplicateName => "header name already set on an earlier line",
            LineError::ExporterOwned => {
                "this header is set by the exporter and cannot be overridden"
            }
        }
    }
}

/// What was wrong with a headers file as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadersFileProblem {
    /// `open`/`stat`/`read` failed; the text is the OS error, never content.
    Unreadable(String),
    /// A directory, FIFO, socket or device.
    NotRegularFile,
    /// Group or other permission bits are set; carries the mode's low 9 bits.
    LoosePermissions(u32),
    /// Larger than [`MAX_HEADERS_FILE_BYTES`].
    TooLarge,
    /// Not UTF-8.
    NotUtf8,
    /// One line was refused (1-based line number).
    Line { line: usize, error: LineError },
    /// Only blank lines and comments.
    NoHeaders,
}

/// A refused headers file: the path, and what was wrong. Safe to log and to
/// surface on `loom-daemon status` — it holds nothing read from the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadersFileError {
    pub path: String,
    pub problem: HeadersFileProblem,
}

impl fmt::Display for HeadersFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match &self.problem {
            HeadersFileProblem::Unreadable(error) => {
                write!(f, "could not read headers file {path}: {error}")
            }
            HeadersFileProblem::NotRegularFile => {
                write!(f, "headers file {path} is not a regular file")
            }
            HeadersFileProblem::LoosePermissions(mode) => write!(
                f,
                "headers file {path} is accessible by group or others (mode {mode:04o}); \
                 restrict it to its owner (chmod 600)"
            ),
            HeadersFileProblem::TooLarge => {
                write!(f, "headers file {path} is larger than {MAX_HEADERS_FILE_BYTES} bytes")
            }
            HeadersFileProblem::NotUtf8 => write!(f, "headers file {path} is not valid UTF-8"),
            HeadersFileProblem::Line { line, error } => {
                write!(f, "headers file {path} line {line}: {}", error.reason())
            }
            HeadersFileProblem::NoHeaders => write!(f, "headers file {path} contains no headers"),
        }
    }
}

impl std::error::Error for HeadersFileError {}

/// The parsed contents of a headers file. See the module docs for why this
/// type is neither `Clone`, `Display` nor `Serialize`.
#[derive(Default)]
pub struct RequestHeaders {
    entries: Vec<(HeaderName, HeaderValue)>,
}

impl fmt::Debug for RequestHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.names().map(|name| (name, "<redacted>")))
            .finish()
    }
}

impl RequestHeaders {
    /// Read and parse `path`. Reads the file every time it is called — there
    /// is no cache — so an exporter that is rebuilt picks up a rotated
    /// credential.
    ///
    /// # Errors
    ///
    /// Any reason in [`HeadersFileProblem`]; nothing is returned partially.
    pub fn load(path: &Path) -> Result<Self, HeadersFileError> {
        let shown = path.display().to_string();
        let fail = |problem| HeadersFileError {
            path: shown.clone(),
            problem,
        };
        let unreadable =
            |error: std::io::Error| fail(HeadersFileProblem::Unreadable(error.to_string()));
        // Classify before opening: opening a FIFO for reading would block.
        if !std::fs::metadata(path).map_err(unreadable)?.is_file() {
            return Err(fail(HeadersFileProblem::NotRegularFile));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        // …and again on the handle, so the mode checked is the mode of the
        // bytes about to be read even if the path was swapped in between.
        let metadata = file.metadata().map_err(unreadable)?;
        if !metadata.is_file() {
            return Err(fail(HeadersFileProblem::NotRegularFile));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(fail(HeadersFileProblem::LoosePermissions(mode)));
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_HEADERS_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        if bytes.len() as u64 > MAX_HEADERS_FILE_BYTES {
            return Err(fail(HeadersFileProblem::TooLarge));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| fail(HeadersFileProblem::NotUtf8))?;
        Self::parse(text).map_err(fail)
    }

    /// Parse headers-file text. Split from [`Self::load`] so the grammar is
    /// testable without a filesystem.
    fn parse(text: &str) -> Result<Self, HeadersFileProblem> {
        let mut entries: Vec<(HeaderName, HeaderValue)> = Vec::new();
        for (index, raw) in text.split('\n').enumerate() {
            let refuse = |error| HeadersFileProblem::Line {
                line: index + 1,
                error,
            };
            let line = raw.trim_matches([' ', '\t', '\r']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| refuse(LineError::MissingColon))?;
            if name.is_empty() || !name.bytes().all(is_token_char) {
                return Err(refuse(LineError::InvalidName));
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| refuse(LineError::InvalidName))?;
            if EXPORTER_OWNED.contains(&name.as_str()) {
                return Err(refuse(LineError::ExporterOwned));
            }
            if entries.iter().any(|(existing, _)| *existing == name) {
                return Err(refuse(LineError::DuplicateName));
            }
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() {
                return Err(refuse(LineError::EmptyValue));
            }
            if !value
                .bytes()
                .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
            {
                return Err(refuse(LineError::InvalidValue));
            }
            let mut value =
                HeaderValue::from_str(value).map_err(|_| refuse(LineError::InvalidValue))?;
            value.set_sensitive(true);
            entries.push((name, value));
        }
        if entries.is_empty() {
            return Err(HeadersFileProblem::NoHeaders);
        }
        Ok(RequestHeaders { entries })
    }

    /// No headers — what an exporter without `headers_file` carries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many headers the file supplied.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The header names (lower-cased), in file order. Names are not secret.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }

    /// Whether the file supplies `Authorization`, replacing the default
    /// Bearer header.
    #[must_use]
    pub fn sets_authorization(&self) -> bool {
        self.entries.iter().any(|(name, _)| name == AUTHORIZATION)
    }

    /// Add the request's authentication: `Authorization: Bearer <bearer>`
    /// unless the file supplies its own `Authorization`, then every header
    /// from the file. With no headers this is exactly `bearer_auth(bearer)`.
    pub fn apply(&self, request: reqwest::RequestBuilder, bearer: &str) -> reqwest::RequestBuilder {
        let request = if self.sets_authorization() {
            request
        } else {
            request.bearer_auth(bearer)
        };
        self.entries
            .iter()
            .fold(request, |request, (name, value)| request.header(name.clone(), value.clone()))
    }
}

/// RFC 9110 §5.6.2 `tchar`.
fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The config-time policy for an exporter entry's `headers_file`, run in
/// [`super::spawn_task`]'s policy pass before any secret is read. `Ok` when
/// the entry has no `headers_file`; otherwise the detail its `misconfigured`
/// status reports.
///
/// - Only an `otlp` entry may carry one: refusing beats silently dropping
///   headers the operator asked for.
/// - The path must be a non-empty string.
/// - The endpoint must be `https`, or loopback. The headers are credentials,
///   and unlike the ingest key they are typically accepted by a whole proxy,
///   not one receiver — so this entry never sends them in cleartext off the
///   host. Entries without `headers_file` keep the existing endpoint rules.
///
/// # Errors
///
/// The reason the entry is refused, worded for `loom-daemon status`.
pub fn entry_policy(
    is_otlp: bool,
    endpoint: &str,
    headers_file: Option<&str>,
) -> Result<(), String> {
    let Some(path) = headers_file else {
        return Ok(());
    };
    if !is_otlp {
        return Err("headers_file is only supported on an otlp exporter entry".to_string());
    }
    if path.trim().is_empty() {
        return Err("headers_file must be a non-empty string path".to_string());
    }
    let https = reqwest::Url::parse(endpoint).is_ok_and(|url| url.scheme() == "https");
    if !https && !super::endpoint_policy::is_loopback_endpoint(endpoint) {
        return Err("headers_file requires an https endpoint (or a loopback one): \
             refusing to send its headers in cleartext"
            .to_string());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
