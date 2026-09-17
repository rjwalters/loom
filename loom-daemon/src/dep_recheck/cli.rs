//! The CLI boundary for the re-check fingerprints (epic #7810, PR 4).
//!
//! # What is contract here
//!
//! `curator.md` invokes `dep-recheck-fingerprint.sh` by path and **`eval`s**
//! its `KEY=VALUE` output. So three things are frozen:
//!
//! - **argv**: the same subcommands and long flags;
//! - **stdout**: the same keys, in the same order, with the same quoting — see
//!   [`shell_quote`], whose asymmetric application is deliberate;
//! - **exit codes**: `0` evaluated, `2` usage error, `3` missing dependency.
//!
//! `defaults/scripts/tests/test-dep-recheck-fingerprint.sh` drives all of it
//! and was kept rather than translated: assertions written against the shell
//! implementation still passing against this one is the equivalence evidence.
//!
//! # `eval` safety
//!
//! Like `claim-staleness.sh`, the output is built only from a fixed enum, a hex
//! hash, and pre-sorted plain-text lines — never raw forge text — so no comment
//! or PR body can reach a caller's shell through `eval`.

use super::{decide, extract, forge, named, premise, recheck};
use std::path::Path;

/// Which fingerprint is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    DepRecheck,
    OperatorPremise,
    NamedDependency,
    ExtractRefs,
    Decide,
}

/// `dep-recheck-fingerprint.sh`'s argv.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub number: Option<i64>,
    pub repo: Option<String>,
    pub refs: Option<String>,
    pub stdin: bool,
    pub verdict: Option<String>,
    pub block_reason: String,
    pub orthogonal: String,
    pub bot_login: Option<String>,
    pub json: bool,
    /// `Some` when `--hash` was passed at all, including `--hash ''`. The
    /// distinction is load-bearing: an empty hash is a legitimate value
    /// (nothing to report), so "absent" cannot be spelled as "empty".
    pub hash: Option<String>,
    pub prior_hash: String,
    pub prior_age_hours: Option<String>,
    pub heartbeat_hours: Option<String>,
}

/// Exit code for a usage error.
const EX_USAGE: i32 = 2;

fn die(msg: &str, code: i32) -> i32 {
    eprintln!("dep-recheck-fingerprint.sh: {msg}");
    code
}

/// Quote a value the way bash's `printf %q` does, for the fields whose values
/// may contain whitespace or newlines and which callers `eval`.
///
/// Applied to `REFS` only. `BLOCKERS` and `DEPS` are emitted **unquoted**,
/// exactly as the shell does — even though they are also multi-line. That
/// asymmetry is preserved rather than normalised: a caller of one is not a
/// caller of the other, and changing either side's quoting silently changes
/// what their `eval` produces.
#[must_use]
pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    // bash uses the unquoted form for a "safe" word, which is what the existing
    // fixtures see for a single-number or empty REFS list.
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-.,:/+=@%^".contains(c))
    {
        return value.to_string();
    }
    // `$'...'` for anything containing a newline, matching bash; plain single
    // quotes otherwise.
    if value.contains('\n') || value.contains('\t') {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\t', "\\t");
        return format!("$'{escaped}'");
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Run the requested subcommand, returning its exit code.
#[must_use]
pub fn run(cwd: &Path, sub: Sub, opts: &Opts, stdin_text: Option<&str>) -> i32 {
    match dispatch(cwd, sub, opts, stdin_text) {
        Ok(code) | Err(code) => code,
    }
}

fn dispatch(cwd: &Path, sub: Sub, opts: &Opts, stdin_text: Option<&str>) -> Result<i32, i32> {
    if sub == Sub::Decide {
        return run_decide(opts);
    }

    if opts.stdin {
        if opts.number.is_some() {
            return Err(die("--stdin and --number are mutually exclusive", EX_USAGE));
        }
        if opts.refs.is_some() {
            return Err(die("--stdin and --refs are mutually exclusive", EX_USAGE));
        }
    } else if sub == Sub::OperatorPremise {
        // operator-premise's live mode fetches each --refs number
        // independently; it never needs the parent issue's own number.
        if opts.refs.as_deref().unwrap_or("").is_empty() {
            return Err(die("one of --refs or --stdin is required", EX_USAGE));
        }
    } else {
        let Some(n) = opts.number else {
            return Err(die("one of --number or --stdin is required", EX_USAGE));
        };
        if n <= 0 {
            return Err(die(&format!("--number must be a positive integer (got '{n}')"), EX_USAGE));
        }
    }

    if let Some(v) = opts.verdict.as_deref() {
        if !v.is_empty() && v != "blocked" && v != "clear" {
            return Err(die(
                &format!("--verdict must be 'blocked' or 'clear' (got '{v}')"),
                EX_USAGE,
            ));
        }
    }

    match sub {
        Sub::DepRecheck => run_dep_recheck(cwd, opts, stdin_text),
        Sub::OperatorPremise => run_operator_premise(cwd, opts, stdin_text),
        Sub::NamedDependency => run_named_dependency(cwd, opts, stdin_text),
        Sub::ExtractRefs => run_extract_refs(cwd, opts, stdin_text),
        Sub::Decide => unreachable!("handled above"),
    }
}

/// Decode a `--stdin` document, or exit 2 with the shell's message.
fn decode<T: serde::de::DeserializeOwned>(text: &str, required: &str) -> Result<T, i32> {
    serde_json::from_str(text)
        .map_err(|_| die(&format!("input JSON must have {required}"), EX_USAGE))
}

fn run_dep_recheck(cwd: &Path, opts: &Opts, stdin_text: Option<&str>) -> Result<i32, i32> {
    let prs = if opts.stdin {
        decode::<recheck::Input>(stdin_text.unwrap_or(""), "a top-level 'prs' array")?.prs
    } else {
        forge::fetch_prs(opts.number.unwrap_or(0), opts.repo.as_deref(), cwd)
            .map_err(|e| die(&e.to_string(), 1))?
    };

    let o = recheck::compute(&prs, opts.verdict.as_deref(), &opts.block_reason, &opts.orthogonal);
    if opts.json {
        println!(
            "{}",
            serde_json::json!({
                "verdict": o.verdict,
                "blockers": o.blockers,
                "block_reason": o.block_reason,
                "orthogonal": o.orthogonal,
                "conclusion_hash": o.conclusion_hash,
            })
        );
    } else {
        println!("VERDICT={}", o.verdict);
        println!("BLOCKERS={}", o.blockers);
        println!("BLOCK_REASON={}", o.block_reason);
        println!("ORTHOGONAL={}", o.orthogonal);
        println!("CONCLUSION_HASH={}", o.conclusion_hash);
    }
    Ok(0)
}

fn run_operator_premise(cwd: &Path, opts: &Opts, stdin_text: Option<&str>) -> Result<i32, i32> {
    let refs = if opts.stdin {
        decode::<premise::Input>(stdin_text.unwrap_or(""), "a top-level 'refs' array")?.refs
    } else {
        let numbers: Vec<i64> = opts
            .refs
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        forge::fetch_refs(&numbers, opts.repo.as_deref(), cwd)
            .map_err(|e| die(&e.to_string(), 1))?
    };

    let o = premise::compute(&refs);
    if opts.json {
        println!(
            "{}",
            serde_json::json!({
                "verdict": o.verdict,
                "refs": o.refs,
                "conclusion_hash": o.conclusion_hash,
            })
        );
    } else {
        println!("VERDICT={}", o.verdict);
        // Quoted: consumers eval these assignments and the list is multi-line.
        println!("REFS={}", shell_quote(&o.refs));
        println!("CONCLUSION_HASH={}", o.conclusion_hash);
    }
    Ok(0)
}

fn run_named_dependency(cwd: &Path, opts: &Opts, stdin_text: Option<&str>) -> Result<i32, i32> {
    let deps = if opts.stdin {
        decode::<named::Input>(stdin_text.unwrap_or(""), "a top-level 'deps' array")?.deps
    } else {
        forge::fetch_named_deps(opts.number.unwrap_or(0), opts.repo.as_deref(), cwd)
            .map_err(|e| die(&e.to_string(), 1))?
    };

    let o = named::compute(&deps);
    if opts.json {
        println!(
            "{}",
            serde_json::json!({
                "verdict": o.verdict,
                "deps": o.deps,
                "conclusion_hash": o.conclusion_hash,
            })
        );
    } else {
        println!("VERDICT={}", o.verdict);
        println!("DEPS={}", o.deps);
        println!("CONCLUSION_HASH={}", o.conclusion_hash);
    }
    Ok(0)
}

fn run_extract_refs(cwd: &Path, opts: &Opts, stdin_text: Option<&str>) -> Result<i32, i32> {
    let input = if opts.stdin {
        decode::<extract::Input>(
            stdin_text.unwrap_or(""),
            "top-level 'body' (string) and 'comments' (array) fields",
        )?
    } else {
        forge::fetch_body_and_comments(opts.number.unwrap_or(0), opts.repo.as_deref(), cwd)
            .map_err(|e| die(&e.to_string(), 1))?
    };

    let bot = opts
        .bot_login
        .as_deref()
        .unwrap_or(extract::DEFAULT_BOT_LOGIN);
    let refs = extract::extract(&input, bot);
    if opts.json {
        println!("{}", serde_json::json!({ "refs": refs }));
    } else {
        println!("REFS={}", shell_quote(&refs));
    }
    Ok(0)
}

fn run_decide(opts: &Opts) -> Result<i32, i32> {
    // `--hash ''` is meaningful (nothing to report this pass), so the flag
    // being ABSENT is the error — not the value being empty.
    let Some(hash) = opts.hash.as_deref() else {
        return Err(die(
            "--hash is required for decide (pass --hash '' when there is nothing \
             to report this pass)",
            EX_USAGE,
        ));
    };

    if !opts.prior_hash.is_empty() && opts.prior_age_hours.is_none() {
        return Err(die("--prior-age-hours is required when --prior-hash is non-empty", EX_USAGE));
    }
    let age = match opts.prior_age_hours.as_deref() {
        Some(s) => s.parse::<u64>().map_err(|_| {
            die(
                &format!("--prior-age-hours must be a non-negative integer (got '{s}')"),
                EX_USAGE,
            )
        })?,
        None => 0,
    };
    let window = match opts.heartbeat_hours.as_deref() {
        Some(s) => s.parse::<u64>().map_err(|_| {
            die(
                &format!("--heartbeat-hours must be a non-negative integer (got '{s}')"),
                EX_USAGE,
            )
        })?,
        None => std::env::var("LOOM_DEP_RECHECK_HEARTBEAT_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(decide::DEFAULT_HEARTBEAT_HOURS),
    };

    let action = decide::decide(hash, &opts.prior_hash, age, window);
    if opts.json {
        println!("{}", serde_json::json!({ "action": action.as_str(), "claim": action.claims() }));
    } else {
        println!("ACTION={}", action.as_str());
        println!("CLAIM={}", action.claims());
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
