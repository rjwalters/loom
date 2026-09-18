//! Cosign trust-root resolution (epic #7810, PR 6a): which public key /
//! signer identity / OIDC issuer verify a Linux release's detached signature.
//!
//! Port of `loom-daemon-update.sh`'s `resolve_cosign_pubkey`, `_regex_escape`,
//! `resolve_cosign_identity_regexp`, and `resolve_cosign_oidc_issuer`.

use std::path::{Path, PathBuf};

/// KEY mode only (a `.sig` published without a sibling `.pem` certificate):
/// the env override when it names a readable file, else a conventional
/// checked-in path. No public key is committed to this repo, and #5054
/// deliberately did NOT add one — see [`identity_regexp`] for why keyless is
/// the default trust root instead. `None` means "signature present but
/// unverifiable" — a loud skip, never a block (see
/// [`super::signature::verify`]).
#[must_use]
pub fn resolve_pubkey(repo_root: &Path, env_override: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = env_override.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        if is_readable_file(&path) {
            return Some(path);
        }
    }
    [
        repo_root.join(".loom").join("cosign.pub"),
        repo_root.join("defaults").join("cosign.pub"),
    ]
    .into_iter()
    .find(|c| is_readable_file(c))
}

fn is_readable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// Escape POSIX ERE metacharacters so a repo slug or release tag can be
/// embedded literally inside [`identity_regexp`] — the `.` in `github.com` /
/// `v0.17.0` is the one that actually matters, but the whole metacharacter
/// class is escaped so no future tag shape can widen the expected identity.
/// Port of the shell's `sed 's/[][\.^$*+?(){}|\\]/\\&/g'`.
#[must_use]
pub fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if matches!(
            c,
            '[' | ']' | '\\' | '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '{' | '}' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The expected KEYLESS signer identity (a POSIX ERE for cosign's
/// `--certificate-identity-regexp`), or `None` when `repo_slug`/`tag` are not
/// both known.
///
/// WHY KEYLESS IS THE DEFAULT (#5054): signing needs no secret provisioned,
/// verification needs no key material distributed, and the trust root becomes
/// an assertion about *who signed* — "a workflow in the SAME repo this
/// artifact was downloaded from, running at EXACTLY this release's tag, with
/// a certificate issued by GitHub Actions". The workflow FILE is deliberately
/// not pinned (`[^@]+`): pinning it would turn a future rename of
/// `release.yml` into a fleet-wide hard-abort for no security gain — anything
/// able to run a workflow in this repo at this tag can already publish the
/// release assets themselves.
#[must_use]
pub fn identity_regexp(repo_slug: &str, tag: &str) -> Option<String> {
    if repo_slug.is_empty() || tag.is_empty() {
        return None;
    }
    Some(format!(
        r"^https://github\.com/{}/\.github/workflows/[^@]+@refs/tags/{}$",
        regex_escape(repo_slug),
        regex_escape(tag)
    ))
}

/// The expected keyless certificate issuer: the env override when non-empty,
/// else GitHub Actions' OIDC provider.
#[must_use]
pub fn oidc_issuer(env_override: Option<&str>) -> String {
    env_override
        .filter(|s| !s.is_empty())
        .unwrap_or("https://token.actions.githubusercontent.com")
        .to_string()
}

#[cfg(test)]
mod tests;
