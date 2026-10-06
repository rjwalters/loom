//! Workflow **delivery-dependency** audit (Issue #9790, epic #9769 phase 1
//! "Gitea qualification (gitea-1)").
//!
//! The operation inventory counts *forge API coordination*: the `gh` calls a
//! caller makes to the forge it coordinates through. #9790 asks for a second,
//! separate list: what a workflow fetches from GitHub **while it runs** —
//! action sources, container registries, release downloads, raw content — none
//! of which goes through a forge API caller and none of which the change gate
//! sees. A repository can reach zero required GitHub API coordination on Gitea
//! and still be unable to start a single job because every `uses:` resolves
//! against github.com. Merging the two lists would let one hide the other, so
//! this audit keeps them on separate [`Plane`]s and never claims total network
//! independence: it is a lexical, static enumeration of what the *files* name.
//!
//! What a static scan cannot see — an action that itself downloads from
//! GitHub at run time, a tool installer's default mirror, a runner image's
//! baked-in sources — is exactly what the live gitea-1 run measures. The
//! output says so (`static_only`), so a clean scan is never read as proof.
//!
//! No forge call, no credential, no network: runnable on a laptop, repeatable
//! from a SHA.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::forge_inventory::gate;

/// Which dependency plane a reference sits on. Kept separate on purpose — see
/// the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Plane {
    /// Action code fetched at job start (`uses: owner/repo@ref`).
    ActionSource,
    /// Container/OCI images pulled or pushed (GHCR).
    PackageRegistry,
    /// Binaries or archives from GitHub Releases / source-download hosts.
    ReleaseDownload,
    /// Forge API coordination (`gh …`, `api.github.com`). Owned by the
    /// operation inventory; listed here only so the two can be told apart.
    ForgeApi,
    /// Other github.com-family hosts (OIDC issuer, plain links).
    GithubOther,
    /// No GitHub fetch: resolved inside the repository, or against a
    /// non-GitHub host (the forge's own action mirror, a non-GHCR registry).
    /// Non-GitHub hosts are still network dependencies — just not this
    /// audit's question.
    NoGithubFetch,
}

impl Plane {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Plane::ActionSource => "action-source",
            Plane::PackageRegistry => "package-registry",
            Plane::ReleaseDownload => "release-download",
            Plane::ForgeApi => "forge-api",
            Plane::GithubOther => "github-other",
            Plane::NoGithubFetch => "no-github-fetch",
        }
    }

    /// What it would take to remove this plane's GitHub dependency on a
    /// self-hosted Gitea. An *estimate* by kind — the live run decides.
    #[must_use]
    pub fn integration_estimate(self) -> &'static str {
        match self {
            Plane::ActionSource => {
                "resolved via Gitea [actions] DEFAULT_ACTIONS_URL (documented default: github.com); \
                 zero-GitHub needs each action mirrored on the forge at the pinned SHA and \
                 DEFAULT_ACTIONS_URL=self or absolute `uses:` URLs — per distinct action"
            }
            Plane::PackageRegistry => {
                "move image push/pull to the forge's OCI container registry; \
                 rewrite image names and registry login per workflow"
            }
            Plane::ReleaseDownload => {
                "mirror the pinned tool archives into forge generic packages or bake them \
                 into the runner image; re-pin checksums — per distinct URL"
            }
            Plane::ForgeApi => {
                "forge API coordination — counted by `forge-inventory report`, not here"
            }
            Plane::GithubOther => "review individually (OIDC issuer, documentation link)",
            Plane::NoGithubFetch => "none for GitHub (non-GitHub hosts are out of scope)",
        }
    }
}

/// One referenced dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    pub file: String,
    pub line: usize,
    pub plane: Plane,
    /// The literal reference (`owner/repo@sha`, a URL, an image name, or the
    /// trimmed `gh` line).
    pub reference: String,
    /// Host the reference resolves against.
    pub host: String,
    /// For `uses:` references: pinned to a 40-hex commit SHA?
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha_pinned: Option<bool>,
}

/// Per-plane rollup.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlaneSummary {
    pub references: usize,
    pub distinct: usize,
    pub integration_estimate: &'static str,
}

/// The whole audit.
#[derive(Debug, Clone, Serialize)]
pub struct Audit {
    pub files_scanned: Vec<String>,
    pub planes: BTreeMap<&'static str, PlaneSummary>,
    /// Distinct `uses:` action references (`owner/repo[/path]@ref`).
    pub distinct_actions: Vec<String>,
    pub dependencies: Vec<Dependency>,
    /// Always `true`: this is a lexical scan of the files, not a network
    /// measurement. See the module docs.
    pub static_only: bool,
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The value of a `uses:` key on this line, anchored at line start (after an
/// optional list dash) so prose that mentions `uses:` never matches.
fn uses_value(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let t = t.strip_prefix('-').map_or(t, str::trim_start);
    let rest = t.strip_prefix("uses:")?;
    let v = rest.split_whitespace().next()?;
    Some(v.trim_matches(|c| c == '"' || c == '\''))
}

fn classify_uses(file: &str, line: usize, value: &str) -> Dependency {
    if value.starts_with("./") || value.starts_with(".github/") {
        return Dependency {
            file: file.into(),
            line,
            plane: Plane::NoGithubFetch,
            reference: value.into(),
            host: "repository".into(),
            sha_pinned: None,
        };
    }
    if let Some(image) = value.strip_prefix("docker://") {
        let host = image_host(image);
        let plane = if host == "ghcr.io" {
            Plane::PackageRegistry
        } else {
            Plane::NoGithubFetch
        };
        return Dependency {
            file: file.into(),
            line,
            plane,
            reference: value.into(),
            host,
            sha_pinned: None,
        };
    }
    // `https://host/owner/repo@ref` (Gitea absolute form) or `owner/repo@ref`.
    let (host, path) = match value.split_once("://") {
        Some((_, rest)) => match rest.split_once('/') {
            Some((h, p)) => (h.to_string(), p),
            None => (rest.to_string(), ""),
        },
        None => ("github.com".to_string(), value),
    };
    let pinned = path.rsplit_once('@').is_some_and(|(_, r)| is_hex40(r));
    let plane = if host == "github.com" {
        Plane::ActionSource
    } else {
        Plane::NoGithubFetch
    };
    Dependency {
        file: file.into(),
        line,
        plane,
        reference: value.into(),
        host,
        sha_pinned: Some(pinned),
    }
}

/// Registry host of an image reference (`ghcr.io/a/b:t` → `ghcr.io`,
/// `rust:1` → `docker.io`).
fn image_host(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') => first.to_string(),
        _ => "docker.io".to_string(),
    }
}

/// Bytes that can continue a URL / image token.
fn is_ref_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~/:@%+=?&#$".contains(&b)
}

fn token_at(line: &str, start: usize) -> &str {
    let bytes = line.as_bytes();
    let mut end = start;
    while end < bytes.len() && is_ref_byte(bytes[end]) {
        // `${{ … }}` and `"` end a reference.
        if bytes[end] == b'$' && bytes.get(end + 1) == Some(&b'{') {
            break;
        }
        end += 1;
    }
    line[start..end].trim_end_matches(['.', ',', ':', ')'])
}

/// Plane for a github.com-family URL host + path, or `None` for a host this
/// audit does not track.
fn classify_url(host: &str, path: &str) -> Option<Plane> {
    let h = host.to_ascii_lowercase();
    Some(match h.as_str() {
        "ghcr.io" => Plane::PackageRegistry,
        "api.github.com" | "uploads.github.com" => Plane::ForgeApi,
        "raw.githubusercontent.com"
        | "objects.githubusercontent.com"
        | "release-assets.githubusercontent.com"
        | "codeload.github.com" => Plane::ReleaseDownload,
        "github.com" | "www.github.com" => {
            if path.contains("/releases/download/") || path.contains("/archive/") {
                Plane::ReleaseDownload
            } else {
                Plane::GithubOther
            }
        }
        _ if h.ends_with(".githubusercontent.com") || h.ends_with(".github.com") => {
            Plane::GithubOther
        }
        _ => return None,
    })
}

/// Scan one workflow file's text.
#[must_use]
pub fn scan(file: &str, text: &str) -> Vec<Dependency> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(v) = uses_value(line) {
            out.push(classify_uses(file, lineno, v));
            continue;
        }
        // URLs.
        let mut i = 0;
        while let Some(rel) = line[i..].find("://") {
            let colon = i + rel;
            let scheme_start = line[..colon]
                .rfind(|c: char| !c.is_ascii_alphabetic())
                .map_or(0, |p| p + 1);
            let scheme = &line[scheme_start..colon];
            i = colon + 3;
            if scheme != "https" && scheme != "http" && scheme != "docker" {
                continue;
            }
            let rest = token_at(line, colon + 3);
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            if let Some(plane) = classify_url(host, path) {
                out.push(Dependency {
                    file: file.into(),
                    line: lineno,
                    plane,
                    reference: format!("{scheme}://{rest}"),
                    host: host.to_ascii_lowercase(),
                    sha_pinned: None,
                });
            }
        }
        // Bare `ghcr.io/…` image names (no scheme).
        let mut j = 0;
        while let Some(rel) = line[j..].find("ghcr.io/") {
            let start = j + rel;
            j = start + "ghcr.io/".len();
            let preceded_by_scheme = line[..start].ends_with("://");
            let preceded_ok = start == 0 || !is_ref_byte(line.as_bytes()[start - 1]);
            if preceded_by_scheme || !preceded_ok {
                continue;
            }
            out.push(Dependency {
                file: file.into(),
                line: lineno,
                plane: Plane::PackageRegistry,
                reference: token_at(line, start).to_string(),
                host: "ghcr.io".into(),
                sha_pinned: None,
            });
        }
        // `gh <noun>` CLI calls — the change gate's own lexical rule.
        if gate::scan_text(file, line, 0).0 > 0 {
            out.push(Dependency {
                file: file.into(),
                line: lineno,
                plane: Plane::ForgeApi,
                reference: line.trim().chars().take(160).collect(),
                host: "github.com".into(),
                sha_pinned: None,
            });
        }
    }
    out
}

/// Build the audit over `(path, text)` pairs.
#[must_use]
pub fn audit(files: &[(String, String)]) -> Audit {
    let mut deps = Vec::new();
    for (path, text) in files {
        deps.extend(scan(path, text));
    }
    let mut distinct: BTreeMap<Plane, std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut counts: BTreeMap<Plane, usize> = BTreeMap::new();
    for d in &deps {
        *counts.entry(d.plane).or_default() += 1;
        let key = if d.plane == Plane::ForgeApi && !d.reference.contains("://") {
            // Distinct `gh` call sites are per line, not per text.
            format!("{}:{}", d.file, d.line)
        } else {
            d.reference.clone()
        };
        distinct.entry(d.plane).or_default().insert(key);
    }
    let planes = counts
        .iter()
        .map(|(p, n)| {
            (
                p.as_str(),
                PlaneSummary {
                    references: *n,
                    distinct: distinct.get(p).map_or(0, std::collections::BTreeSet::len),
                    integration_estimate: p.integration_estimate(),
                },
            )
        })
        .collect();
    let distinct_actions = distinct
        .get(&Plane::ActionSource)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    Audit {
        files_scanned: files.iter().map(|(p, _)| p.clone()).collect(),
        planes,
        distinct_actions,
        dependencies: deps,
        static_only: true,
    }
}

/// Is `path` a workflow file this audit reads by default?
#[must_use]
pub fn is_workflow_file(path: &str) -> bool {
    (path.ends_with(".yml") || path.ends_with(".yaml"))
        && (path.starts_with(".github/workflows/") || path.starts_with(".gitea/workflows/"))
}

/// Human-readable rendering.
#[must_use]
pub fn render_text(a: &Audit) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "workflow delivery dependencies — {} file(s), static scan only (not a network measurement)\n",
        a.files_scanned.len()
    );
    let _ = writeln!(s, "{:<18} {:>5} {:>8}  integration estimate", "plane", "refs", "distinct");
    for (name, p) in &a.planes {
        let _ = writeln!(
            s,
            "{name:<18} {:>5} {:>8}  {}",
            p.references, p.distinct, p.integration_estimate
        );
    }
    if !a.distinct_actions.is_empty() {
        let _ = writeln!(s, "\ndistinct actions fetched from github.com:");
        for act in &a.distinct_actions {
            let _ = writeln!(s, "  {act}");
        }
    }
    let mut other: Vec<&Dependency> = a
        .dependencies
        .iter()
        .filter(|d| matches!(d.plane, Plane::PackageRegistry | Plane::ReleaseDownload))
        .collect();
    other.sort_by(|x, y| x.reference.cmp(&y.reference).then(x.file.cmp(&y.file)));
    other.dedup_by(|x, y| x.reference == y.reference);
    if !other.is_empty() {
        let _ = writeln!(s, "\nregistry / release-download references:");
        for d in other {
            let _ =
                writeln!(s, "  {:<16} {}  ({}:{})", d.plane.as_str(), d.reference, d.file, d.line);
        }
    }
    s
}

#[cfg(test)]
#[path = "tests/workflow_deps.rs"]
mod tests;
