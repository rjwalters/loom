//! Log filter for tmux `pipe-pane` output and post-processing agent logs —
//! the native port of `loom_tools.log_filter` (#4275), behind `strip-ansi.sh`.
//!
//! Two modes:
//!
//! 1. **Stdin pipeline** (default, no args) — real-time filter for tmux
//!    `pipe-pane`. Strips ANSI escapes, carriage returns, backspaces, control
//!    characters, blank lines, and consecutive duplicate lines.
//! 2. **File post-processing** (`--file <path>`) — deep cleaning of captured
//!    agent logs: everything from mode 1 plus Claude Code TUI noise removal
//!    (spinner characters, animation text, thinking indicators, permission
//!    banners, separator lines, ASCII art banners, and short redraw debris).

use std::io::{BufRead, Write};
use std::sync::OnceLock;

use regex::Regex;

/// Spinner characters used by the Claude Code TUI thinking animation.
///
/// NOTE: `⏺` (U+23FA) is intentionally excluded — it is the tool-call marker
/// used by thinking-stall detection. Stripping it would cause false-positive
/// stall detection (issue #2835).
const SPINNERS: [char; 6] = [
    '\u{2736}', '\u{273b}', '\u{273d}', '\u{2733}', '\u{2722}', '\u{00b7}',
];

/// Animation words displayed during thinking/processing. Covers multiple Claude
/// Code versions — v2.1.40+ added "Frosting", "Befuddling", "Moseying", etc.
/// (issue #2835).
const ANIMATION_WORDS: &[&str] = &[
    "Nucleating",
    "Pollinating",
    "Shimmying",
    "Transmuting",
    "Crunching",
    "Pondering",
    "Germinating",
    "Synthesizing",
    "Crystallizing",
    "Manifesting",
    "Percolating",
    "Composing",
    "Ruminating",
    "Brainstorming",
    "Evaluating",
    "Theorizing",
    "Envisioning",
    "Distilling",
    "Formulating",
    "Catalyzing",
    "Incubating",
    "Calibrating",
    "Conjuring",
    "Fermenting",
    "Contemplating",
    "Architecting",
    "Deliberating",
    "Decoding",
    "Weaving",
    "Assembling",
    "Deconstructing",
    "Extrapolating",
    "Interpolating",
    "Meditating",
    "Originating",
    "Philosophizing",
    "Reflecting",
    "Simulating",
    "Triangulating",
    "Unbundling",
    "Visualizing",
    "Crunched",
    // Newer animation words observed in Claude Code v2.1.40+ (issue #2835)
    "Befuddling",
    "Frosting",
    "Moseying",
    "Sashaying",
    "Waltzing",
    "Ambling",
    "Beguiling",
    "Brooding",
    "Bumbling",
    "Dawdling",
    "Dithering",
    "Floundering",
    "Fretting",
    "Fumbling",
    "Gallivanting",
    "Humming",
    "Idling",
    "Lollygagging",
    "Meandering",
    "Milling",
    "Mulling",
    "Noodling",
    "Perambulating",
    "Perusing",
    "Pottering",
    "Puttering",
    "Rambling",
    "Sifting",
    "Stewing",
    "Tinkering",
    "Toiling",
    "Wandering",
    "Whirring",
];

struct Patterns {
    ansi: Regex,
    control: Regex,
    unicode_control: Regex,
    animation: Regex,
    separator: Regex,
    permission: Regex,
    prompt: Regex,
    banner: Regex,
    banner_info: Regex,
    thinking: Regex,
    esc_interrupt: Regex,
    ctrl_b: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut words: Vec<String> = ANIMATION_WORDS.iter().map(|w| regex::escape(w)).collect();
        words.sort_unstable();
        let anim = words.join("|");
        #[allow(clippy::expect_used)]
        Patterns {
            // Mirrors `loom_tools.common.logging._ANSI_ESCAPE_PATTERN`:
            // CSI sequences, OSC sequences (BEL or ST terminated), charset
            // selection (ESC ( B / ESC ) 0) and keypad modes (ESC = / ESC >).
            ansi: Regex::new(
                r"(?s)\x1b(?:\[[?0-9;]*[A-Za-z]|\].*?(?:\x07|\x1b\\)|[()][0-9AB]|[=>])",
            )
            .expect("ansi regex"),
            control: Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").expect("control regex"),
            // Unicode Cc/Cf minus the two we keep (newline, tab). The ASCII Cc
            // range is already handled by `control` above; this catches the C1
            // block and format characters (ZWJ, LRM, …).
            unicode_control: Regex::new(r"[[\p{Cc}\p{Cf}]--[\t\n]]").expect("unicode control regex"),
            animation: Regex::new(&format!(
                r"^[✶✻✽✳✢·\s]*({anim})…?(\s*\(.*\))?\s*$"
            ))
            .expect("animation regex"),
            separator: Regex::new(r"^[─━]{4,}\s*$").expect("separator regex"),
            permission: Regex::new(r"⏵⏵\s*bypass permissions on").expect("permission regex"),
            prompt: Regex::new(r#"^❯\s*(Try\s+".*")?\s*$"#).expect("prompt regex"),
            banner: Regex::new(r"^[\s▐▛▜▌▝█▘░▒▓]+$").expect("banner regex"),
            banner_info: Regex::new(
                r"^[\s▐▛▜▌▝█▘░▒▓]+\s*(Claude Code|Opus|Sonnet|Haiku|Claude Max)",
            )
            .expect("banner info regex"),
            thinking: Regex::new(
                r"^\s*\((thinking|thought for \d+s|\d+[sm]?\s*·\s*[↓↑]\s*[\d.,]+k?\s*tokens(\s*·\s*thinking)?)\)\s*$",
            )
            .expect("thinking regex"),
            esc_interrupt: Regex::new(r"·\s*esc to interrupt").expect("esc interrupt regex"),
            ctrl_b: Regex::new(r"^ctrl\+b ctrl\+b").expect("ctrl-b regex"),
        }
    })
}

/// Remove ANSI escape sequences from `text`.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    patterns().ansi.replace_all(text, "").into_owned()
}

/// Return `true` if `line` is Claude Code TUI rendering noise.
///
/// Expects `line` to already have ANSI sequences stripped.
#[must_use]
pub fn is_tui_noise(line: &str) -> bool {
    tui_noise_common(line) || is_spinner_debris(line)
}

/// TUI noise check safe for real-time streaming (the pipe-pane filter).
///
/// Applies every [`is_tui_noise`] check except that the short-fragment debris
/// heuristic only fires when the line starts with a spinner character. That
/// prevents false-positive suppression of legitimate short content lines
/// (single-word test output, short variable names) in a real-time stream where
/// file-level context is unavailable (issue #2798).
#[must_use]
pub fn is_tui_noise_realtime(line: &str) -> bool {
    if tui_noise_common(line) {
        return true;
    }
    match line.chars().next() {
        Some(c) if SPINNERS.contains(&c) => is_spinner_debris(line),
        _ => false,
    }
}

fn tui_noise_common(line: &str) -> bool {
    let p = patterns();
    p.animation.is_match(line)
        || p.separator.is_match(line)
        || p.permission.is_match(line)
        || p.prompt.is_match(line)
        || p.banner.is_match(line)
        || p.banner_info.is_match(line)
        || p.thinking.is_match(line)
        || p.esc_interrupt.is_match(line)
        || p.ctrl_b.is_match(line)
}

/// Return `true` if `line` is spinner chars and/or a short redraw fragment.
fn is_spinner_debris(line: &str) -> bool {
    let stripped = strip_leading_spinners(line).trim().to_string();
    // Pure spinner line.
    if stripped.is_empty() {
        return true;
    }
    // Short fragment without spaces — redraw debris ("u", "ca", "Nl", "i…").
    if stripped.chars().count() <= 5 && !stripped.contains(' ') {
        // Preserve log-header comment lines (# ...).
        if stripped.starts_with('#') {
            return false;
        }
        // Preserve 3+ digit numbers (test output like "364").
        let digits = stripped.chars().all(|c| c.is_ascii_digit());
        if digits && stripped.len() >= 3 {
            return false;
        }
        return true;
    }
    false
}

fn strip_leading_spinners(line: &str) -> &str {
    line.trim_start_matches(|c| SPINNERS.contains(&c))
}

/// Clean a single line of terminal output.
///
/// Returns the cleaned line, or `None` if the line should be suppressed
/// (blank / whitespace-only after cleaning).
#[must_use]
pub fn clean_line(raw: &str) -> Option<String> {
    // Strip trailing \r before splitting so that "content\r" doesn't resolve to
    // an empty last segment (which would be suppressed as blank).
    let raw = raw.trim_end_matches('\r');

    // Process carriage returns: keep only the last segment. This handles
    // spinner animation where lines are overwritten with \r.
    let raw = match raw.rsplit_once('\r') {
        Some((_, last)) => last,
        None => raw,
    };

    let line = strip_ansi(raw);
    let line = apply_backspaces(&line);
    let p = patterns();
    let line = p.control.replace_all(&line, "");
    let line = p.unicode_control.replace_all(&line, "").into_owned();

    if line.trim().is_empty() {
        return None;
    }
    Some(line)
}

/// Apply backspace erasure: each `\x08` deletes the character before it, and
/// leading backspaces are dropped. Mirrors the Python loop.
fn apply_backspaces(line: &str) -> String {
    if !line.contains('\u{8}') {
        return line.to_string();
    }
    let mut out: Vec<char> = Vec::with_capacity(line.len());
    for ch in line.chars() {
        if ch == '\u{8}' {
            out.pop();
        } else {
            out.push(ch);
        }
    }
    out.into_iter().collect()
}

/// Deep-clean a captured agent log, returning the cleaned text.
///
/// Applies [`clean_line`] then removes Claude Code TUI noise patterns and
/// collapses blank runs into single separators.
#[must_use]
pub fn clean_text(input: &str) -> String {
    let mut output: Vec<String> = Vec::new();
    let mut blank_run = 0usize;

    for raw_line in input.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\n', '\r']);

        let Some(cleaned) = clean_line(line) else {
            blank_run += 1;
            continue;
        };
        if is_tui_noise(&cleaned) {
            blank_run += 1;
            continue;
        }
        let cleaned = strip_leading_spinners(&cleaned).to_string();

        if blank_run > 0 && !output.is_empty() {
            output.push(String::new());
        }
        blank_run = 0;
        output.push(cleaned);
    }

    if output.is_empty() {
        String::new()
    } else {
        let mut s = output.join("\n");
        s.push('\n');
        s
    }
}

/// Deep-clean a captured agent log file. A missing/unreadable file yields `""`,
/// matching the Python `errors="replace"` best-effort read.
#[must_use]
pub fn clean_file(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => clean_text(&String::from_utf8_lossy(&bytes)),
        Err(_) => String::new(),
    }
}

/// Real-time stdin → stdout filter (the default `strip-ansi.sh` mode).
///
/// Reads line by line, cleans, drops TUI noise, and collapses consecutive
/// duplicate lines into a `[repeated N more times]` summary.
pub fn filter_stream<R: BufRead, W: Write>(reader: R, writer: &mut W) -> std::io::Result<()> {
    let mut prev_line: Option<String> = None;
    let mut dup_count: usize = 0;

    for raw in reader.lines() {
        let Ok(raw) = raw else { break };

        let Some(cleaned) = clean_line(&raw) else {
            continue;
        };
        if is_tui_noise_realtime(&cleaned) {
            continue;
        }
        // Strip leading spinner chars so dedup treats spinner-prefixed and
        // plain lines as the same content ("✶ Fixing…" / "✻ Fixing…").
        let cleaned = strip_leading_spinners(&cleaned).to_string();
        if cleaned.trim().is_empty() {
            continue;
        }

        if prev_line.as_deref() == Some(cleaned.as_str()) {
            dup_count += 1;
            continue;
        }
        if dup_count > 0 && prev_line.is_some() {
            write_repeat_summary(writer, dup_count)?;
        }
        writeln!(writer, "{cleaned}")?;
        writer.flush()?;
        prev_line = Some(cleaned);
        dup_count = 0;
    }

    if dup_count > 0 && prev_line.is_some() {
        let _ = write_repeat_summary(writer, dup_count);
    }
    Ok(())
}

fn write_repeat_summary<W: Write>(writer: &mut W, dup_count: usize) -> std::io::Result<()> {
    let plural = if dup_count > 1 { "s" } else { "" };
    writeln!(writer, "  [repeated {dup_count} more time{plural}]")?;
    writer.flush()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn stream(input: &str) -> String {
        let mut out = Vec::new();
        filter_stream(std::io::Cursor::new(input.as_bytes()), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn strips_csi_osc_charset_and_keypad_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\after"), "after");
        assert_eq!(strip_ansi("\x1b(Bplain"), "plain");
        assert_eq!(strip_ansi("\x1b=\x1b>x"), "x");
        assert_eq!(strip_ansi("\x1b[?25lhidden"), "hidden");
    }

    #[test]
    fn clean_line_keeps_last_carriage_return_segment() {
        assert_eq!(clean_line("first\rsecond").unwrap(), "second");
        assert_eq!(clean_line("content\r").unwrap(), "content");
    }

    #[test]
    fn clean_line_applies_backspaces() {
        assert_eq!(clean_line("abc\u{8}d").unwrap(), "abd");
        assert_eq!(clean_line("\u{8}\u{8}abc").unwrap(), "abc");
    }

    #[test]
    fn clean_line_suppresses_blank_and_control_only_lines() {
        assert!(clean_line("").is_none());
        assert!(clean_line("   ").is_none());
        assert!(clean_line("\x1b[0m").is_none());
        assert!(clean_line("\u{200e}").is_none());
    }

    #[test]
    fn clean_line_preserves_tabs() {
        assert_eq!(clean_line("a\tb").unwrap(), "a\tb");
    }

    #[test]
    fn detects_tui_noise() {
        assert!(is_tui_noise("✻ Pondering…"));
        assert!(is_tui_noise("Frosting… (12s)"));
        assert!(is_tui_noise("────────────"));
        assert!(is_tui_noise("⏵⏵ bypass permissions on"));
        assert!(is_tui_noise("❯"));
        assert!(is_tui_noise("▐▛███▜▌"));
        assert!(is_tui_noise("(thinking)"));
        assert!(is_tui_noise("(thought for 12s)"));
        assert!(is_tui_noise("(3s · ↓ 1.2k tokens · thinking)"));
        assert!(is_tui_noise("42s · esc to interrupt"));
        assert!(is_tui_noise("ctrl+b ctrl+b"));
    }

    #[test]
    fn preserves_real_content() {
        assert!(!is_tui_noise("running 42 tests"));
        assert!(!is_tui_noise("# log header"));
        assert!(!is_tui_noise("364"));
    }

    /// The ⏺ tool-call marker must survive (issue #2835): stripping it causes
    /// false-positive thinking-stall detection.
    #[test]
    fn tool_call_marker_is_not_noise() {
        let line = clean_line("⏺ Bash(cargo test)").unwrap();
        assert!(line.starts_with('⏺'));
        assert!(!is_tui_noise(&line));
        assert!(!is_tui_noise_realtime(&line));
    }

    /// Issue #2798: the realtime variant must NOT suppress short content lines
    /// that lack a spinner prefix, while the file variant still does.
    #[test]
    fn realtime_variant_spares_short_unprefixed_lines() {
        assert!(!is_tui_noise_realtime("OK"));
        assert!(!is_tui_noise_realtime("yes"));
        assert!(is_tui_noise("OK"));
        assert!(is_tui_noise_realtime("✻ca"));
    }

    #[test]
    fn stream_collapses_consecutive_duplicates() {
        let out = stream("same line\nsame line\nsame line\nother\n");
        assert_eq!(out, "same line\n  [repeated 2 more times]\nother\n");
    }

    #[test]
    fn stream_singular_repeat_summary() {
        let out = stream("dup\ndup\ntail\n");
        assert!(out.contains("[repeated 1 more time]"));
        assert!(!out.contains("more times"));
    }

    #[test]
    fn stream_flushes_trailing_duplicate_count() {
        let out = stream("dup\ndup\ndup\n");
        assert_eq!(out, "dup\n  [repeated 2 more times]\n");
    }

    #[test]
    fn stream_collapses_spinner_prefixed_duplicates() {
        let out = stream("✶ Fixing the bug\n✻ Fixing the bug\n");
        assert_eq!(out, " Fixing the bug\n  [repeated 1 more time]\n");
    }

    #[test]
    fn stream_drops_ansi_and_blank_lines() {
        let out = stream("\x1b[32mgreen text\x1b[0m\n\n   \nplain\n");
        assert_eq!(out, "green text\nplain\n");
    }

    #[test]
    fn clean_text_collapses_blank_runs_to_one_separator() {
        let out = clean_text("first line\n\n\n\nsecond line\n");
        assert_eq!(out, "first line\n\nsecond line\n");
    }

    #[test]
    fn clean_text_drops_leading_blank_run() {
        let out = clean_text("\n\nfirst line\n");
        assert_eq!(out, "first line\n");
    }

    /// The file (deep-clean) mode DOES apply the short-fragment debris
    /// heuristic — a bare 5-char token with no spaces is redraw debris. The
    /// realtime mode deliberately does not (issue #2798).
    #[test]
    fn clean_text_drops_short_unspaced_fragments() {
        assert_eq!(clean_text("alpha\n"), "");
        assert_eq!(clean_text("# header\n"), "# header\n");
        assert_eq!(clean_text("364\n"), "364\n");
        assert_eq!(stream("alpha\n"), "alpha\n");
    }

    #[test]
    fn clean_text_removes_tui_noise_and_strips_spinner_prefixes() {
        let out = clean_text("✻ Pondering…\n✶ real content here\n────────\n");
        assert_eq!(out, " real content here\n");
    }

    #[test]
    fn clean_text_on_empty_input_is_empty() {
        assert_eq!(clean_text(""), "");
        assert_eq!(clean_text("\n\n\n"), "");
    }

    #[test]
    fn clean_file_missing_path_is_empty() {
        assert_eq!(clean_file(std::path::Path::new("/nonexistent/loom/log")), "");
    }
}
