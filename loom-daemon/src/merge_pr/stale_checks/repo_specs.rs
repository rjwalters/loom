//! Per-repo input declarations for required checks loom has no spec for
//! (#9589).
//!
//! # Why
//!
//! [`super::inputs::SPECS`] describes loom's **own** required checks. In a
//! consumer repo (kicad-tools, klayout-tools, gf180-*, …) none of the required
//! contexts appear there, so every one of them is unspecced, and an unspecced
//! context is stale on **any** base move ([`super::inputs::unknown_check_reason`]).
//! Each merge therefore re-stales every other open PR, and a queue of approved
//! PRs cannot drain without a fresh CI run per PR per merge. On 2026-09-30
//! `2AMLogic/klayout-tools` hit exactly that with 20 approved PRs and a
//! `Lint (ruff)` context.
//!
//! # What this does
//!
//! A consumer repo may track [`DECLARATION_PATH`], which gives each of its
//! required contexts the same `global` / `scanned` / `coupled` /
//! `removal_sensitive` sets a [`CheckSpec`] carries. A declared context is then
//! judged by the SAME five clauses of [`stale_reason_scoped`] as loom's own
//! checks. No new predicate is introduced, so per-file semantics carry over
//! unchanged.
//!
//! # Fail-closed rules (ci-principles rule 9)
//!
//! - **No heuristic default.** An undeclared context keeps today's "any base
//!   move = stale". A guess about what a check reads (its workflow's `paths:`
//!   filter, "the source roots") is not the repo's own text.
//! - **Built-in specs win.** A context [`super::inputs::specs_for`] knows is
//!   never re-read from the declaration.
//! - **Read from the base tip, never the PR head**, so a PR cannot narrow its
//!   own freshness check. The declaration's own path is added to every declared
//!   `global`, so a change to it on EITHER side interacts with every declared
//!   check.
//! - **Any doubt rejects the whole file.** That covers an unreadable file (any
//!   error but a 404), invalid JSON, an unknown field, a duplicate context,
//!   `version` ≠ 1, an empty context name, an empty `global`, or a pattern the
//!   guard's glob dialect cannot express faithfully. A rejected file behaves
//!   exactly like an absent one, plus a `Warning:` naming why.
//! - **Whole-file `ci.yml`.** #9065's block attribution maps jobs to loom's
//!   own components, so it never narrows a declared spec: those are always
//!   judged with [`CiScopes::unscoped`].

use super::inputs::{stale_reason_scoped, CheckSpec, FileSet, StaleReason};
use super::workflow_scope::CiScopes;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The tracked declaration file, relative to the repo root.
pub const DECLARATION_PATH: &str = ".loom/stale-check-inputs.json";

/// The only schema version this build understands.
const SUPPORTED_VERSION: u32 = 1;

/// One declared context's input sets, owned (they come from a file, not a
/// `const` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredSpec {
    pub context: String,
    pub global: Vec<String>,
    pub scanned: Vec<String>,
    pub coupled: Vec<String>,
    pub removal_sensitive: bool,
}

impl DeclaredSpec {
    /// [`stale_reason_scoped`] over this declaration, with [`DECLARATION_PATH`]
    /// as an extra global input and `ci.yml` judged as a whole file.
    #[must_use]
    pub fn stale_reason(&self, d: &FileSet, p: &FileSet) -> Option<StaleReason> {
        let global: Vec<&str> = self
            .global
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(DECLARATION_PATH))
            .collect();
        let scanned: Vec<&str> = self.scanned.iter().map(String::as_str).collect();
        let coupled: Vec<&str> = self.coupled.iter().map(String::as_str).collect();
        let spec = CheckSpec {
            context: &self.context,
            global: &global,
            scanned: &scanned,
            coupled: &coupled,
            removal_sensitive: self.removal_sensitive,
        };
        stale_reason_scoped(&spec, d, p, &CiScopes::unscoped())
    }
}

/// What the base tip says about per-repo declarations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RepoSpecs {
    /// No declaration file (or it was never needed, so never read). Every
    /// unspecced context keeps the any-move-is-stale rule, silently.
    #[default]
    Absent,
    /// A valid declaration, keyed by context.
    Declared(BTreeMap<String, DeclaredSpec>),
    /// A declaration existed (or could not be ruled out) but was not usable.
    /// Behaves exactly like [`Self::Absent`], plus a warning carrying this
    /// reason.
    Rejected(String),
}

impl RepoSpecs {
    /// The declared spec for `context`, if the declaration is valid and names it.
    #[must_use]
    pub fn declared(&self, context: &str) -> Option<&DeclaredSpec> {
        match self {
            Self::Declared(map) => map.get(context),
            _ => None,
        }
    }

    /// Classify a read of [`DECLARATION_PATH`] at the base tip. A 404 is the
    /// one error that proves absence; every other error could be hiding a
    /// declaration, so it is a rejection.
    #[must_use]
    pub fn from_fetch(read: Result<String, String>) -> Self {
        match read {
            Ok(text) => Self::parse(&text),
            Err(e) if is_not_found(&e) => Self::Absent,
            Err(e) => Self::Rejected(format!("it could not be read from the base tip ({e})")),
        }
    }

    /// Parse and validate the declaration text. Never partially accepts: one
    /// bad entry rejects the whole file.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match parse_strict(text) {
            Ok(map) => Self::Declared(map),
            Err(why) => Self::Rejected(why),
        }
    }

    /// The operator-facing warning for a rejected declaration, or `None`.
    #[must_use]
    pub fn rejection_warning(&self) -> Option<String> {
        match self {
            Self::Rejected(why) => Some(format!(
                "required-check freshness guard (#9589): ignoring `{DECLARATION_PATH}` — {why}. \
Every required check without a built-in input spec stays stale on ANY base move until the \
declaration is fixed."
            )),
            _ => None,
        }
    }
}

/// Does this `gh api` failure say the file does not exist?
fn is_not_found(err: &str) -> bool {
    err.contains("HTTP 404")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: u32,
    checks: UniqueChecks,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    global: Vec<String>,
    #[serde(default)]
    scanned: Vec<String>,
    #[serde(default)]
    coupled: Vec<String>,
    #[serde(default)]
    removal_sensitive: bool,
}

/// `checks`, keeping every key: serde's map impls let a repeated key silently
/// overwrite the first, and which of two specs for one context wins is exactly
/// the kind of doubt that must reject.
struct UniqueChecks(Vec<(String, RawSpec)>);

impl<'de> Deserialize<'de> for UniqueChecks {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueChecks;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object mapping required-check contexts to input specs")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, RawSpec>()? {
                    out.push((k, v));
                }
                Ok(UniqueChecks(out))
            }
        }
        de.deserialize_map(V)
    }
}

fn parse_strict(text: &str) -> Result<BTreeMap<String, DeclaredSpec>, String> {
    let raw: RawFile =
        serde_json::from_str(text).map_err(|e| format!("it is not a valid declaration ({e})"))?;
    if raw.version != SUPPORTED_VERSION {
        return Err(format!(
            "its version is {}, and this loom-daemon only understands version {SUPPORTED_VERSION}",
            raw.version
        ));
    }
    let mut out = BTreeMap::new();
    for (context, spec) in raw.checks.0 {
        if context.trim().is_empty() {
            return Err("it declares a check with an empty context name".to_string());
        }
        if out.contains_key(&context) {
            return Err(format!("it declares `{context}` more than once"));
        }
        if spec.global.is_empty() {
            return Err(format!(
                "`{context}` has an empty `global` list (a check's own workflow file is always \
one of its inputs)"
            ));
        }
        for pattern in spec.global.iter().chain(&spec.scanned).chain(&spec.coupled) {
            validate_pattern(pattern)
                .map_err(|why| format!("`{context}`: pattern {pattern:?} {why}"))?;
        }
        out.insert(
            context.clone(),
            DeclaredSpec {
                context,
                global: spec.global,
                scanned: spec.scanned,
                coupled: spec.coupled,
                removal_sensitive: spec.removal_sensitive,
            },
        );
    }
    Ok(out)
}

/// Accept only patterns [`super::inputs::glob_match`] matches the way a reader
/// would expect. Anything else (`?`, `[…]`, `{a,b}`, `!`, `a**b`) would be
/// matched literally, match nothing, and silently narrow the check: fail open.
fn validate_pattern(pattern: &str) -> Result<(), &'static str> {
    if pattern.is_empty() {
        return Err("is empty");
    }
    if pattern.starts_with('/') || pattern.contains('\\') {
        return Err("must be a repo-relative path with `/` separators");
    }
    if pattern
        .chars()
        .any(|c| matches!(c, '?' | '[' | ']' | '{' | '}' | '!'))
    {
        return Err("uses glob syntax the guard does not support (only `*` and `**`)");
    }
    for seg in pattern.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err("has an empty, `.` or `..` path segment");
        }
        if seg.contains("**") && seg != "**" {
            return Err("uses `**` inside a segment (it must be a whole segment)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
