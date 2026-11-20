//! Shared truthiness parsing for env/config flag strings.
//!
//! One definition of "is this string truthy" for the flags that accept
//! `1`/`true`/`yes`/`on` (trimmed, ASCII case-insensitive); anything else,
//! including garbage, is `false`. Copies with different accepted values or
//! return shapes (bash-mirroring, default-on, tri-state) deliberately stay
//! local to their modules.

/// `true` for `1`, `true`, `yes`, `on` (trimmed, case-insensitive); else `false`.
#[must_use]
pub fn parse_truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Read env var `name`: `None` when unset, otherwise [`parse_truthy`] of its value.
#[must_use]
pub fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| parse_truthy(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_values() {
        for v in [
            "1", "true", "yes", "on", "TRUE", "Yes", "ON", " on ", "\ttrue\n",
        ] {
            assert!(parse_truthy(v), "{v:?} should be truthy");
        }
    }

    #[test]
    fn falsy_values() {
        for v in ["0", "false", "no", "off", "", "  ", "maybe", "2", "onn"] {
            assert!(!parse_truthy(v), "{v:?} should be falsy");
        }
    }

    #[test]
    fn env_flag_unset_is_none() {
        assert_eq!(env_flag("LOOM_ENV_FLAG_TEST_DEFINITELY_UNSET_13376"), None);
    }
}
