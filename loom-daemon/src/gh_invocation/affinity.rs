//! A request's routing identity (W4-B): [`affinity_key`].
//!
//! A split repo's reads are spread across the reader pool per request
//! ([`crate::forge_identity::route`]). GitHub ETags are credential-specific,
//! so a request that moved between readers would lose its `304`; the key
//! that picks the reader must therefore name **what is read**, and nothing
//! about how the answer is fetched or shaped:
//!
//! - **kept**, in argv order: the subcommand words and positional args, the
//!   method (`-X` / `--method`), the request fields (`-f` / `-F` /
//!   `--field` / `--raw-field`) and every other flag, which may select what
//!   is read (`--json`, `--state`, `--limit`, `-R`, …);
//! - **dropped**: `-H` / `--header` (a rotating `If-None-Match` must never
//!   move a URL), `--jq` / `-q`, `--template` / `-t`, `--include` / `-i`,
//!   `--paginate`, `--hostname` and `--cache`, which only shape or transport
//!   the same response.

use std::ffi::OsString;

/// Flags that shape or transport a response, and take a value.
const DROPPED_VALUED: &[&str] = &[
    "-H",
    "--header",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--hostname",
    "--cache",
];

/// Flags that shape or transport a response, with no value.
const DROPPED_BARE: &[&str] = &["-i", "--include", "--paginate"];

/// Separator between kept arguments: a byte no argv element normally holds,
/// so `["a b"]` and `["a", "b"]` never collide.
const SEP: char = '\u{1f}';

/// The routing identity of one `gh` argv (see the module docs). Two calls
/// that differ only in a dropped flag — an `If-None-Match` header, a `--jq`
/// filter — share a key and therefore a reader.
#[must_use]
pub fn affinity_key(args: &[OsString]) -> String {
    let mut kept: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        let (name, inline_value) = match arg.split_once('=') {
            Some((n, _)) if n.starts_with("--") => (n, true),
            _ => (arg.as_ref(), false),
        };
        if DROPPED_VALUED.contains(&name) {
            // `--jq=.x` carries its value inline; `--jq .x` in the next arg.
            i += if inline_value { 1 } else { 2 };
            continue;
        }
        if DROPPED_BARE.contains(&name) {
            i += 1;
            continue;
        }
        kept.push(arg.into_owned());
        i += 1;
    }
    kept.join(&SEP.to_string())
}

/// A conditional read's affinity key: its URL with any leading `/` and an
/// `https://api.github.com/` prefix removed, so the spellings `gh api`
/// accepts for one resource share a key.
#[must_use]
pub fn url_affinity_key(url: &str) -> String {
    let url = url.trim();
    let url = url.strip_prefix("https://api.github.com").unwrap_or(url);
    url.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(args: &[&str]) -> String {
        affinity_key(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn transport_and_shaping_flags_never_change_the_key() {
        let bare = key(&["api", "repos/acme/hot/issues/7"]);
        for variant in [
            &[
                "api",
                "--include",
                "repos/acme/hot/issues/7",
                "-H",
                "If-None-Match: \"a\"",
            ][..],
            &[
                "api",
                "-i",
                "repos/acme/hot/issues/7",
                "-H",
                "If-None-Match: \"b\"",
            ],
            &["api", "repos/acme/hot/issues/7", "--jq", ".state"],
            &["api", "repos/acme/hot/issues/7", "-q", ".title"],
            &["api", "repos/acme/hot/issues/7", "--paginate"],
            &["api", "--hostname", "github.com", "repos/acme/hot/issues/7"],
            &["api", "repos/acme/hot/issues/7", "--template", "{{.x}}"],
            &["api", "repos/acme/hot/issues/7", "--cache", "1h"],
            &[
                "api",
                "repos/acme/hot/issues/7",
                "--jq=.state",
                "--header=X: y",
            ],
        ] {
            assert_eq!(key(variant), bare, "{variant:?}");
        }
    }

    #[test]
    fn two_calls_differing_only_in_etag_share_a_key() {
        assert_eq!(
            key(&[
                "api",
                "-i",
                "repos/acme/hot/pulls",
                "-H",
                "If-None-Match: W/\"1\""
            ]),
            key(&[
                "api",
                "-i",
                "repos/acme/hot/pulls",
                "-H",
                "If-None-Match: W/\"2\""
            ]),
        );
    }

    #[test]
    fn what_is_read_changes_the_key() {
        let base = key(&["api", "repos/acme/hot/issues"]);
        for other in [
            &["api", "repos/acme/hot/pulls"][..],
            &["api", "repos/acme/hot/issues", "-f", "state=closed"],
            &["api", "repos/acme/hot/issues", "-F", "per_page=50"],
            &["api", "repos/acme/hot/issues", "--method", "GET"],
            &["issue", "list", "-R", "acme/hot"],
        ] {
            assert_ne!(key(other), base, "{other:?}");
        }
        // Field order is argv order.
        assert_ne!(
            key(&["api", "x", "-f", "a=1", "-f", "b=2"]),
            key(&["api", "x", "-f", "b=2", "-f", "a=1"]),
        );
        // No ambiguity between one argument with a space and two arguments.
        assert_ne!(key(&["api", "a b"]), key(&["api", "a", "b"]));
    }

    #[test]
    fn url_spellings_of_one_resource_share_a_key() {
        let k = url_affinity_key("repos/acme/hot/issues?state=open");
        assert_eq!(url_affinity_key("/repos/acme/hot/issues?state=open"), k);
        assert_eq!(url_affinity_key("https://api.github.com/repos/acme/hot/issues?state=open"), k);
    }
}
