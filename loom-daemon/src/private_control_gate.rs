//! `loom-daemon check-private-control-version` — the CI gate that makes the
//! "changing the forced policy is a `CONTROL_VERSION` bump" convention real
//! (Issue #8858).
//!
//! The runtime check in `tokens_pool::private_workspace::bundle` compares a
//! sealed manifest with the *same build's* `POLICY`, so it cannot notice a
//! policy edit that forgot the version bump. This gate compares the two
//! declarations in `bundle.rs` between the PR merge base and the head, by
//! reading the source out of git objects (`git show <rev>:<path>`): no network,
//! no build, no mutable remote refs.
//!
//! Rules:
//! * `POLICY` is compared as a key/value map: order and formatting are
//!   irrelevant; an added, removed or changed entry is a semantic change.
//! * A semantic change requires `head CONTROL_VERSION > base CONTROL_VERSION`.
//! * A `CONTROL_VERSION` that *decreases* fails even with an unchanged policy:
//!   the revision is monotonic.
//! * Missing refs, a missing/duplicate/unparseable declaration, duplicate
//!   policy keys, and a `POLICY` length annotation that disagrees with its
//!   entries all fail rather than pass silently.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

/// Repository-relative path of the file that declares the policy.
pub const DEFAULT_SOURCE_PATH: &str = "loom-daemon/src/tokens_pool/private_workspace/bundle.rs";

/// The two declarations the gate measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declarations {
    pub control_version: u64,
    pub policy: BTreeMap<String, String>,
}

/// Outcome of a comparison that did not error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing semantic changed (a version-only increase is also fine).
    Ok(String),
    /// Policy changed and the version increased.
    OkBumped(String),
    /// Policy changed without a strictly greater version, or version decreased.
    Violation(String),
}

/// Blank out `//` and `/* */` comments, leaving string literals intact.
fn strip_comments(src: &str) -> Result<String> {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '"' {
            out.push(c);
            i += 1;
            loop {
                let Some(&d) = b.get(i) else {
                    bail!("unterminated string literal");
                };
                out.push(d);
                i += 1;
                if d == '\\' {
                    if let Some(&e) = b.get(i) {
                        out.push(e);
                        i += 1;
                    }
                } else if d == '"' {
                    break;
                }
            }
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            loop {
                if i + 1 >= b.len() {
                    bail!("unterminated block comment");
                }
                if b[i] == '*' && b[i + 1] == '/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            out.push(' ');
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

/// Byte offsets of every `const <name>` declaration. `const` and the name are
/// matched as separate tokens (any whitespace between them, including newlines
/// or a comment already collapsed to a space), with identifier boundaries on
/// both words.
fn find_decls(text: &str, name: &str) -> Vec<usize> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(p) = text[from..].find("const") {
        let at = from + p;
        let after = at + "const".len();
        from = after;
        if text[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let rest = &text[after..];
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            continue; // `const` not followed by whitespace (e.g. `constant`)
        }
        if let Some(tail) = trimmed.strip_prefix(name) {
            if !tail.chars().next().is_some_and(is_ident) {
                hits.push(at);
            }
        }
    }
    hits
}

fn parse_string_literal(chars: &[char], i: &mut usize) -> Result<String> {
    if chars.get(*i) != Some(&'"') {
        bail!("expected a string literal");
    }
    *i += 1;
    let mut s = String::new();
    loop {
        let Some(&c) = chars.get(*i) else {
            bail!("unterminated string literal");
        };
        *i += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let Some(&e) = chars.get(*i) else {
                    bail!("unterminated escape");
                };
                *i += 1;
                s.push(match e {
                    'n' => '\n',
                    't' => '\t',
                    '\\' => '\\',
                    '"' => '"',
                    other => bail!("unsupported escape `\\{other}` in a policy literal"),
                });
            }
            other => s.push(other),
        }
    }
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
}

fn expect(chars: &[char], i: &mut usize, want: char) -> Result<()> {
    skip_ws(chars, i);
    if chars.get(*i) == Some(&want) {
        *i += 1;
        Ok(())
    } else {
        let got: String = chars.iter().skip(*i).take(12).collect();
        bail!("expected `{want}` but found `{got}`")
    }
}

fn parse_version(clean: &str) -> Result<u64> {
    let hits = find_decls(clean, "CONTROL_VERSION");
    let at = match hits.as_slice() {
        [] => bail!("no `const CONTROL_VERSION` declaration found"),
        [one] => *one,
        many => bail!(
            "{} `const CONTROL_VERSION` declarations found; exactly one is required",
            many.len()
        ),
    };
    let rest = &clean[at..];
    let semi = rest
        .find(';')
        .ok_or_else(|| anyhow!("`const CONTROL_VERSION` has no terminating `;`"))?;
    let decl = &rest[..semi];
    let (_, rhs) = decl
        .split_once('=')
        .ok_or_else(|| anyhow!("`const CONTROL_VERSION` has no `=` initializer"))?;
    let rhs = rhs.trim().replace('_', "");
    rhs.parse::<u64>().map_err(|_| {
        anyhow!("`CONTROL_VERSION` initializer `{}` is not a plain integer literal", rhs)
    })
}

fn parse_policy(clean: &str) -> Result<BTreeMap<String, String>> {
    let hits = find_decls(clean, "POLICY");
    let at = match hits.as_slice() {
        [] => bail!("no `const POLICY` declaration found"),
        [one] => *one,
        many => bail!("{} `const POLICY` declarations found; exactly one is required", many.len()),
    };
    let rest = &clean[at..];
    let eq = rest
        .find('=')
        .ok_or_else(|| anyhow!("`const POLICY` has no `=` initializer"))?;
    // Declared length, e.g. `[(&str, &str); 10]`.
    let ty = &rest[..eq];
    let declared_len = ty
        .rsplit_once(';')
        .and_then(|(_, n)| n.trim().trim_end_matches(']').trim().parse::<usize>().ok());
    let chars: Vec<char> = rest[eq + 1..].chars().collect();
    let mut i = 0;
    expect(&chars, &mut i, '[')?;
    let mut policy = BTreeMap::new();
    let mut count = 0usize;
    loop {
        skip_ws(&chars, &mut i);
        match chars.get(i) {
            Some(']') => {
                i += 1;
                break;
            }
            Some('(') => {
                i += 1;
                skip_ws(&chars, &mut i);
                let key = parse_string_literal(&chars, &mut i).context("policy key")?;
                expect(&chars, &mut i, ',')?;
                skip_ws(&chars, &mut i);
                let val = parse_string_literal(&chars, &mut i).context("policy value")?;
                skip_ws(&chars, &mut i);
                if chars.get(i) == Some(&',') {
                    i += 1;
                }
                expect(&chars, &mut i, ')')?;
                count += 1;
                if policy.insert(key.clone(), val).is_some() {
                    bail!("duplicate POLICY key `{key}`");
                }
                skip_ws(&chars, &mut i);
                if chars.get(i) == Some(&',') {
                    i += 1;
                }
            }
            _ => {
                let got: String = chars.iter().skip(i).take(12).collect();
                bail!("unexpected `{got}` in the POLICY initializer; every entry must be a (\"KEY\", \"VALUE\") pair of string literals");
            }
        }
    }
    expect(&chars, &mut i, ';')?;
    if let Some(n) = declared_len {
        if n != count {
            bail!("POLICY is annotated with length {n} but has {count} entries");
        }
    }
    Ok(policy)
}

/// Parse `CONTROL_VERSION` and `POLICY` out of Rust source text.
pub fn parse_declarations(src: &str) -> Result<Declarations> {
    let clean = strip_comments(src)?;
    Ok(Declarations {
        control_version: parse_version(&clean)?,
        policy: parse_policy(&clean)?,
    })
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resolve(repo: &Path, which: &str, rev: &str) -> Result<String> {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .map(|s| s.trim().to_string())
    .map_err(|_| {
        anyhow!(
            "{which} ref `{rev}` does not resolve to a commit in this checkout; \
             fetch it first (CI needs `fetch-depth: 0`, or `git fetch origin <ref>`)"
        )
    })
}

fn load(repo: &Path, label: &str, sha: &str, path: &str) -> Result<Declarations> {
    let text = git(repo, &["show", &format!("{sha}:{path}")])
        .with_context(|| format!("cannot read {path} at the {label} ({sha})"))?;
    parse_declarations(&text).with_context(|| format!("{path} at the {label} ({sha})"))
}

fn describe_changes(
    base: &BTreeMap<String, String>,
    head: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in head {
        match base.get(k) {
            None => out.push(format!("  + {k} = {v:?} (added)")),
            Some(old) if old != v => out.push(format!("  ~ {k}: {old:?} -> {v:?} (changed)")),
            _ => {}
        }
    }
    for (k, v) in base {
        if !head.contains_key(k) {
            out.push(format!("  - {k} = {v:?} (removed)"));
        }
    }
    out
}

/// Pure comparison of two parsed declaration sets.
pub fn compare(base: &Declarations, head: &Declarations) -> Verdict {
    let changes = describe_changes(&base.policy, &head.policy);
    let (b, h) = (base.control_version, head.control_version);
    if h < b {
        return Verdict::Violation(format!(
            "CONTROL_VERSION decreased from {b} to {h}; the revision is monotonic and must never go down."
        ));
    }
    if changes.is_empty() {
        return Verdict::Ok(format!(
            "POLICY unchanged vs the merge base (CONTROL_VERSION {b} -> {h})."
        ));
    }
    let list = changes.join("\n");
    if h > b {
        Verdict::OkBumped(format!(
            "POLICY changed and CONTROL_VERSION was bumped ({b} -> {h}):\n{list}"
        ))
    } else {
        Verdict::Violation(format!(
            "The private-control POLICY changed but CONTROL_VERSION is still {b}:\n{list}\n\n\
             A policy change invalidates already-bound session identities, so it must bump \
             CONTROL_VERSION in {DEFAULT_SOURCE_PATH} (strictly greater than {b}) and the \
             bump must be described in defaults/docs/private-control-bundle.md. \
             This check is a release-compatibility gate, not permission to weaken the policy."
        ))
    }
}

/// Compare the declarations at the merge base of `base`/`head` with those at
/// `head`, reading everything from git objects in `repo`.
pub fn check(repo: &Path, base: &str, head: &str, path: &str) -> Result<Verdict> {
    let base_sha = resolve(repo, "base", base)?;
    let head_sha = resolve(repo, "head", head)?;
    let mb = git(repo, &["merge-base", &base_sha, &head_sha]).map_err(|e| {
        anyhow!(
            "no merge base between `{base}` and `{head}` ({e}); the checkout is probably shallow, \
             use `fetch-depth: 0`"
        )
    })?;
    let mb = mb.trim();
    let base_decl = load(repo, "merge base", mb, path)?;
    let head_decl = load(repo, "head", &head_sha, path)?;
    Ok(compare(&base_decl, &head_decl))
}

#[cfg(test)]
mod tests;
