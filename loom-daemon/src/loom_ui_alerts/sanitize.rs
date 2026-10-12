//! Fit raw alert fields to loom-ui's external-alert contract (#11295).
//!
//! loom-ui answers 400 for anything outside it, and a 400 is dropped, so a
//! field that cannot be fixed here is an alert lost. Every function is total:
//! it always returns something loom-ui accepts. Lengths are characters (code
//! points), as loom-ui counts them.

/// Max `source`/`rule` length.
pub const IDENT_MAX: usize = 64;
/// Max `key` length.
pub const KEY_MAX: usize = 128;
/// Max `title` length.
pub const TITLE_MAX: usize = 200;
/// Max `detail` length.
pub const DETAIL_MAX: usize = 2000;
/// Max `link` length.
pub const LINK_MAX: usize = 500;

fn fit(raw: &str, allowed: impl Fn(char) -> bool, max: usize, empty: &str) -> String {
    let mapped: String = raw
        .trim()
        .chars()
        .map(|c| if allowed(c) { c } else { '-' })
        .collect();
    let mapped = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if mapped.is_empty() {
        return empty.to_string();
    }
    if mapped.chars().count() <= max {
        return mapped.to_string();
    }
    // Keep distinct long values distinct: a cut prefix plus a hash of the
    // whole value, so the incident id is stable for the same input.
    let hash = format!("{:08x}", fnv1a(raw) as u32);
    let head: String = mapped.chars().take(max - hash.len() - 1).collect();
    format!("{head}-{hash}")
}

fn fnv1a(s: &str) -> u64 {
    s.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// `source`/`rule`: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
#[must_use]
pub fn ident(raw: &str) -> String {
    fit(
        raw,
        |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'),
        IDENT_MAX,
        "unknown",
    )
}

/// `key`: `[A-Za-z0-9][A-Za-z0-9._/@+-]{0,127}`.
#[must_use]
pub fn key(raw: &str) -> String {
    fit(
        raw,
        |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '@' | '+' | '-'),
        KEY_MAX,
        "none",
    )
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// `title`: 1–200 characters, one line, no control characters. Falls back to
/// `fallback` (the rule) when nothing printable is left.
#[must_use]
pub fn title(raw: &str, fallback: &str) -> String {
    let line = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let line = if line.is_empty() {
        ident(fallback)
    } else {
        line
    };
    cut(&line, TITLE_MAX)
}

/// `detail`: at most 2000 characters, plain text, newlines kept.
#[must_use]
pub fn detail(raw: &str) -> String {
    let clean: String = raw
        .trim()
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    cut(&clean, DETAIL_MAX)
}

/// `link`: an `https://` URL of at most 500 characters with no credentials,
/// else `None` (omitted, never refused).
#[must_use]
pub fn link(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let url = reqwest::Url::parse(raw).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let s = url.to_string();
    (s.chars().count() <= LINK_MAX).then_some(s)
}
