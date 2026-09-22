//! `printf '%b'` / `awk -v` backslash expansion.
//!
//! # Why a port needs this at all
//!
//! Both renderers accumulate their environment block into a shell string that
//! contains literal two-character `\n` sequences and then emit it with
//! `printf '%b'`:
//!
//! ```sh
//! env_entries+="        <key>$(xml_escape "$key")</key>\n        <string>$(xml_escape "$value")</string>\n"
//! …
//! printf '%b' "$env_entries"
//! ```
//!
//! `%b` expands backslash escapes across the **whole** string — including the
//! parts that came from an environment *value*. So an exported
//! `LOOM_X='C:\new'` did not render `C:\new` into the plist; it rendered `C:`,
//! a newline, and `ew`. A value containing `\c` truncated the rest of the
//! plist outright.
//!
//! None of that is deliberate, and every instinct says to drop it. Dropping it
//! is a silent behaviour change in a file that configures a daemon, which is
//! precisely the class `verification-recipes.md` §6 ("the shell was TOLERANT by
//! construction") tells you to preserve and write down rather than quietly
//! improve. It is reproduced here, and the differential corpus deliberately
//! generates backslash-bearing values so the reproduction is measured rather
//! than asserted.
//!
//! # `awk -v` is the same trap one function away
//!
//! `inject_one_plist_env_entry` passes its key and value through `awk -v k=… -v
//! v=…`, and POSIX awk expands escape sequences in a `-v` assignment too. Its
//! set is a subset of `%b`'s (no `\c`, no `\x`, and `\/` and `\"` are additions
//! that reduce to the same character), so [`expand_awk_v`] shares this
//! implementation with those differences applied.

/// What `printf '%b'` produced.
///
/// `\c` stops output immediately — the rest of the string, including anything a
/// later variable would have contributed, is discarded.
#[must_use]
pub fn expand_printf_b(s: &str) -> String {
    expand(s, Flavour::PrintfB)
}

/// What an `awk -v name=value` assignment produced.
#[must_use]
pub fn expand_awk_v(s: &str) -> String {
    expand(s, Flavour::AwkV)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    PrintfB,
    AwkV,
}

#[allow(clippy::too_many_lines)]
fn expand(s: &str, flavour: Flavour) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if i + 1 >= chars.len() {
            // A trailing lone backslash is emitted literally by both.
            out.push('\\');
            i += 1;
            continue;
        }
        let c = chars[i + 1];
        i += 2;
        match c {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '\\' => out.push('\\'),
            'c' if flavour == Flavour::PrintfB => return out,
            'e' | 'E' if flavour == Flavour::PrintfB => out.push('\x1b'),
            '/' | '"' if flavour == Flavour::AwkV => out.push(c),
            '0' if flavour == Flavour::PrintfB => {
                // `\0nnn` — up to three octal digits after the 0.
                let (val, used) = read_radix(&chars, i, 8, 3);
                i += used;
                push_byte(&mut out, val);
            }
            'x' if flavour == Flavour::PrintfB => {
                let (val, used) = read_radix(&chars, i, 16, 2);
                if used == 0 {
                    out.push('\\');
                    out.push('x');
                } else {
                    i += used;
                    push_byte(&mut out, val);
                }
            }
            d if flavour == Flavour::AwkV && d.is_digit(8) => {
                // awk's `\ddd` has no leading `0`.
                let (val, used) = read_radix(&chars, i - 1, 8, 3);
                i += used - 1;
                push_byte(&mut out, val);
            }
            other => {
                // An unrecognised escape is emitted with its backslash intact.
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn read_radix(chars: &[char], from: usize, radix: u32, max: usize) -> (u32, usize) {
    let mut val = 0u32;
    let mut used = 0usize;
    while used < max {
        let Some(c) = chars.get(from + used) else {
            break;
        };
        let Some(d) = c.to_digit(radix) else { break };
        val = val * radix + d;
        used += 1;
    }
    (val, used)
}

fn push_byte(out: &mut String, val: u32) {
    // Values above 0x7f are written as raw bytes by the shell. Rust strings are
    // UTF-8, so the closest faithful rendering is the code point — which agrees
    // for everything the ASCII-only inputs here can produce, and is documented
    // rather than silently lossy for the rest.
    if let Some(c) = char::from_u32(val) {
        out.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backslash_n_in_a_value_really_did_become_a_newline() {
        assert_eq!(expand_printf_b("<string>C:\\new</string>"), "<string>C:\new</string>");
    }

    #[test]
    fn backslash_c_truncates_the_rest_of_the_render() {
        // The whole point: a later key is LOST, not escaped.
        assert_eq!(expand_printf_b("a\\cb<key>LOOM_Z</key>"), "a");
    }

    #[test]
    fn a_doubled_backslash_collapses_to_one() {
        assert_eq!(expand_printf_b("a\\\\nb"), "a\\nb");
    }

    #[test]
    fn an_unknown_escape_keeps_its_backslash() {
        assert_eq!(expand_printf_b("a\\qb"), "a\\qb");
    }

    #[test]
    fn awk_v_has_no_backslash_c_and_no_hex() {
        // The subset difference is the reason the two share an implementation
        // but not a flavour: `\c` is an unknown escape to awk.
        assert_eq!(expand_awk_v("a\\cb"), "a\\cb");
        assert_eq!(expand_awk_v("a\\x41"), "a\\x41");
        assert_eq!(expand_printf_b("a\\x41"), "aA");
    }

    #[test]
    fn octal_forms_differ_between_the_two() {
        assert_eq!(expand_printf_b("\\0101"), "A");
        assert_eq!(expand_awk_v("\\101"), "A");
    }
}
