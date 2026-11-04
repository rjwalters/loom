//! The one POSIX single-quote escaper (#13561).
//!
//! Every module that interpolates a value into shell text used to carry its own
//! copy of this primitive; a quoting fix had to land in each. They all call
//! [`shell_quote`] now.

/// `s` as one POSIX shell word: always single-quoted, with each embedded `'`
/// written `'\''`. A rule with no "safe character" list has no list to get wrong.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn quotes_plain_empty_and_spaced_values() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b $c"), "'a b $c'");
    }

    #[test]
    fn escapes_an_embedded_single_quote() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn keeps_a_newline_literal_inside_the_quotes() {
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }
}
