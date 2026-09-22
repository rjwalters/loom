//! Environment harvesting and XML escaping — the two primitives both
//! renderers are built on.
//!
//! # The harvest is a LINE filter, not a variable filter
//!
//! Both renderers read the process environment through the same pipeline:
//!
//! ```sh
//! done < <(env | grep -E '^(LOOM_[A-Za-z0-9_]*|GH_TOKEN|GITEA_TOKEN|FORGE_TOKEN)=' || true)
//! ```
//!
//! `env` prints `KEY=VALUE\n` per variable and `grep` is **line-oriented**, so
//! a variable whose value contains a newline is *silently truncated at the
//! first newline*: `LOOM_X=$'a\nb'` contributes the line `LOOM_X=a` (kept) and
//! the line `b` (dropped, it matches nothing). The obvious Rust port —
//! iterating `std::env::vars()` and testing each key — does **not** do that; it
//! would bake the whole multi-line value into the plist/unit, where a raw
//! newline in a systemd `Environment=` line is a different directive entirely.
//!
//! This is `verification-recipes.md` §6 "Cause 3: a `grep -E` pattern is not a
//! regex" in its purest form, so the pipeline is reproduced as a pipeline:
//! render the text `env` would have printed, split it on `\n`, and filter
//! lines. The truncation is then structural rather than remembered.
//!
//! # Order is environ order, not sorted order
//!
//! `env` walks `environ` in place, so a variable that already existed keeps its
//! slot when re-`export`ed and a brand-new one is appended at the end.
//! [`std::env::vars_os`] iterates the same array, and `setenv(3)` has the same
//! replace-in-place/append semantics as bash's `export`, so calling
//! [`std::env::set_var`] in the script's own order reproduces the rendered
//! `EnvironmentVariables` order without modelling environ separately.

/// `xml_escape()` — `&` first, then `<` and `>`, exactly as the shell ordered
/// its three substitutions. Escaping `&` last would double-escape the `&` it
/// had just introduced.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Does a line match `^(LOOM_[A-Za-z0-9_]*|GH_TOKEN|GITEA_TOKEN|FORGE_TOKEN)=`?
///
/// The `LOOM_` branch is greedy over `[A-Za-z0-9_]` and then requires `=`.
/// Because `=` is not in that character class the greedy match always stops at
/// the first character outside it, so there is nothing for a backtracking
/// engine to reconsider: the test is "everything between `LOOM_` and the first
/// non-word character is word characters, and that character is `=`".
///
/// `LOOM_=x` matches (the `*` accepts an empty run) and so did the shell's.
#[must_use]
pub fn line_matches_forwarded_key(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("LOOM_") {
        let stop = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        return rest[stop..].starts_with('=');
    }
    line.starts_with("GH_TOKEN=")
        || line.starts_with("GITEA_TOKEN=")
        || line.starts_with("FORGE_TOKEN=")
}

/// The `KEY=VALUE` text `env` would print for this process, in environ order.
///
/// Non-UTF-8 names/values are rendered lossily rather than skipped: `env`
/// prints the raw bytes and `grep` matches on them, so dropping such a variable
/// would remove a key the shell forwarded.
#[must_use]
pub fn env_text() -> String {
    let mut out = String::new();
    for (k, v) in std::env::vars_os() {
        out.push_str(&k.to_string_lossy());
        out.push('=');
        out.push_str(&v.to_string_lossy());
        out.push('\n');
    }
    out
}

/// The forwarded `(key, value)` pairs, in environ order, with the shell's
/// newline truncation applied.
///
/// `key` is everything before the first `=` (`${line%%=*}`) and `value` is
/// everything after it (`${line#*=}`), so `LOOM_A=1=2` yields `("LOOM_A",
/// "1=2")` as it did in bash.
#[must_use]
pub fn forwarded_env_pairs() -> Vec<(String, String)> {
    forwarded_env_pairs_from(&env_text())
}

/// [`forwarded_env_pairs`] with the `env` text injected, so the newline and
/// ordering behaviour is testable without mutating process-global state.
#[must_use]
pub fn forwarded_env_pairs_from(text: &str) -> Vec<(String, String)> {
    text.split('\n')
        .filter(|line| !line.is_empty())
        .filter(|line| line_matches_forwarded_key(line))
        .filter_map(|line| {
            let eq = line.find('=')?;
            Some((line[..eq].to_string(), line[eq + 1..].to_string()))
        })
        .collect()
}

/// `is_session_scoped_env_key <KEY>` — a per-invocation AGENT SESSION variable
/// that must never be baked into durable daemon config (#6568).
///
/// `LOOM_RUNTIME` is stripped here but is deliberately NOT a detection signal
/// for [`super::guards::session_context_keys`]; see that function.
#[must_use]
pub fn is_session_scoped_env_key(key: &str) -> bool {
    key.starts_with("LOOM_SWEEP_")
        || matches!(key, "LOOM_TERMINAL_ID" | "LOOM_ROLE" | "LOOM_RUNTIME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_value_is_truncated_at_the_first_newline() {
        // The shell tolerance this port exists to preserve: `grep` saw two
        // lines and kept one. A key-based filter would have kept "a\nb".
        let pairs = forwarded_env_pairs_from("LOOM_X=a\nb\nLOOM_Y=z\n");
        assert_eq!(
            pairs,
            vec![
                ("LOOM_X".to_string(), "a".to_string()),
                ("LOOM_Y".to_string(), "z".to_string())
            ]
        );
    }

    #[test]
    fn the_pattern_is_a_prefix_test_not_a_whole_line_test() {
        assert!(line_matches_forwarded_key("LOOM_A=1"));
        assert!(line_matches_forwarded_key("LOOM_=1"), "the `*` accepts empty");
        assert!(line_matches_forwarded_key("GH_TOKEN=x"));
        // `GH_TOKENX=` is not `GH_TOKEN=`: the alternation is followed by `=`.
        assert!(!line_matches_forwarded_key("GH_TOKENX=x"));
        // A hyphen is not in `[A-Za-z0-9_]`, so the greedy run stops there and
        // the next character is not `=`.
        assert!(!line_matches_forwarded_key("LOOM_A-B=1"));
        assert!(!line_matches_forwarded_key("PATH=/bin"));
        assert!(!line_matches_forwarded_key("XLOOM_A=1"), "`^` anchors it");
    }

    #[test]
    fn the_value_keeps_every_later_equals_sign() {
        let pairs = forwarded_env_pairs_from("LOOM_A=1=2\n");
        assert_eq!(pairs, vec![("LOOM_A".to_string(), "1=2".to_string())]);
    }

    #[test]
    fn xml_escape_does_not_double_escape_its_own_ampersand() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn session_keys_are_the_four_the_renderers_strip() {
        for k in [
            "LOOM_SWEEP_CLAIM_OWNED",
            "LOOM_SWEEP_",
            "LOOM_TERMINAL_ID",
            "LOOM_ROLE",
            "LOOM_RUNTIME",
        ] {
            assert!(is_session_scoped_env_key(k), "{k}");
        }
        for k in ["LOOM_SWEEP", "LOOM_ROLES", "LOOM_WORK_FINDER", "GH_TOKEN"] {
            assert!(!is_session_scoped_env_key(k), "{k}");
        }
    }
}
