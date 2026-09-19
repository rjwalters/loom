//! Environment-knob resolution, matching the shell's truthiness exactly.
//!
//! The shell tested flags with `[[ "$X" =~ ^(1|true|yes)$ ]]` and
//! `^(0|false|no)$`, which are **case-sensitive** and match the WHOLE value.
//! `TRUE`, `Yes` and `1 ` were all "unset" to it. That is surprising, but it is
//! the behaviour forty knobs and 233 assertions were written against, so it is
//! reproduced rather than improved — a port that silently widens truthiness
//! changes what an operator's `LOOM_WATCHDOG_ESCALATE=TRUE` does.

/// `^(1|true|yes)$` — the shell's affirmative test, case-sensitive.
#[must_use]
pub fn is_true(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes")
}

/// `^(0|false|no)$` — the shell's negative test, case-sensitive.
#[must_use]
pub fn is_false(value: &str) -> bool {
    matches!(value, "0" | "false" | "no")
}

/// Read a knob, treating unset and empty alike — `${X:-default}` semantics.
#[must_use]
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// A tri-state knob: `Some(true)` when explicitly affirmative, `Some(false)`
/// when explicitly negative, `None` when unset, empty, or unrecognised.
///
/// `None` for an unrecognised value is deliberate and load-bearing: the shell's
/// `if ... elif ...` pairs left the variable at its default when a value
/// matched neither pattern, so a typo'd `LOOM_WATCHDOG_SYSTEMD_PROBE=ture` fell
/// through to the marker's own setting rather than flipping the probe off.
#[must_use]
pub fn tri(name: &str) -> Option<bool> {
    let raw = var(name)?;
    if is_true(&raw) {
        Some(true)
    } else if is_false(&raw) {
        Some(false)
    } else {
        None
    }
}

/// A numeric knob, falling back when unset, empty or not all-digits — the
/// shell's `[[ "$X" =~ ^[0-9]+$ ]] || X=default` guard.
#[must_use]
pub fn num(name: &str, default: u64) -> u64 {
    var(name)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_is_case_sensitive_and_whole_value() {
        for v in ["1", "true", "yes"] {
            assert!(is_true(v), "{v} should be true");
        }
        for v in ["TRUE", "True", "Yes", "y", "on", "1 ", " 1", ""] {
            assert!(!is_true(v), "{v} must NOT be true — the shell did not accept it");
        }
        for v in ["0", "false", "no"] {
            assert!(is_false(v), "{v} should be false");
        }
        for v in ["FALSE", "No", "off", "n"] {
            assert!(!is_false(v), "{v} must NOT be false — the shell did not accept it");
        }
    }

    #[test]
    fn a_value_matching_neither_pattern_leaves_the_default_standing() {
        // The shell's if/elif fell through, keeping the marker's setting. An
        // unrecognised value must therefore be indistinguishable from unset.
        assert!(!is_true("ture"));
        assert!(!is_false("ture"));
    }
}
