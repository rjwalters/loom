//! The hidden `<!-- loom:provenance v1 … -->` record (#9027, D33 lean form).
//!
//! One line, `key=value` fields in a fixed order. Values may contain spaces
//! (`build=0.19.409 <40-hex> clean`), so a field runs until the next known
//! `key=`; [`super::sanitize`] guarantees no value contains `=` or `>`, which
//! is what makes that split unambiguous and keeps the comment closed.
use std::path::Path;
use std::process::Command;

use super::{sanitize, UNKNOWN};

pub const PREFIX: &str = "<!-- loom:provenance v1 ";
const SUFFIX: &str = " -->";
const KEYS: [&str; 7] = [
    "build", "prompts", "sweep", "story", "trace", "host", "base",
];

/// One provenance record. Every field is sanitized text or [`UNKNOWN`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub build: String,
    pub prompts: String,
    pub sweep: String,
    pub story: String,
    pub trace: String,
    pub host: String,
    pub base: String,
}

impl Marker {
    fn fields(&self) -> [&String; 7] {
        [
            &self.build,
            &self.prompts,
            &self.sweep,
            &self.story,
            &self.trace,
            &self.host,
            &self.base,
        ]
    }

    /// The single marker line.
    #[must_use]
    pub fn render(&self) -> String {
        let body: Vec<String> = KEYS
            .iter()
            .zip(self.fields())
            .map(|(key, value)| format!("{key}={}", sanitize(value)))
            .collect();
        format!("{PREFIX}{}{SUFFIX}", body.join(" "))
    }

    /// Parse a marker line; `None` unless it is a complete v1 record.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let body = line.trim().strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
        let mut values: [Option<String>; 7] = Default::default();
        let mut current: Option<usize> = None;
        for token in body.split_whitespace() {
            let started = token
                .split_once('=')
                .and_then(|(key, value)| KEYS.iter().position(|k| *k == key).map(|i| (i, value)));
            match (started, current) {
                (Some((i, value)), _) => {
                    if values[i].is_some() {
                        return None;
                    }
                    values[i] = Some(value.to_string());
                    current = Some(i);
                }
                (None, Some(i)) => {
                    let value = values[i].get_or_insert_with(String::new);
                    value.push(' ');
                    value.push_str(token);
                }
                (None, None) => return None,
            }
        }
        let [build, prompts, sweep, story, trace, host, base] = values;
        Some(Self {
            build: build?,
            prompts: prompts?,
            sweep: sweep?,
            story: story?,
            trace: trace?,
            host: host?,
            base: base?,
        })
    }
}

/// Inputs the caller supplies; everything else is derived here.
#[derive(Debug, Default)]
pub struct MarkerInputs<'a> {
    pub issue: Option<u32>,
    pub sweep: Option<&'a str>,
    pub base_ref: Option<&'a str>,
}

/// Build the PR-body record for the checkout at `root`.
#[must_use]
pub fn collect(root: &Path, inputs: &MarkerInputs<'_>) -> Marker {
    let (story, trace) = match inputs.issue {
        Some(issue) => super::story_and_trace(root, issue),
        None => (UNKNOWN.to_string(), UNKNOWN.to_string()),
    };
    let sweep = inputs
        .sweep
        .map(str::to_string)
        .or_else(|| std::env::var("LOOM_SWEEP_ID").ok())
        .unwrap_or_default();
    let host = crate::sweep_registry::host_identity();
    Marker {
        build: crate::self_update::BUILD_STAMP.to_string(),
        prompts: prompts_field(root),
        sweep: or_unknown(sweep),
        story,
        trace,
        host: if host == crate::sweep_registry::UNKNOWN_HOST {
            UNKNOWN.to_string()
        } else {
            host
        },
        base: base_commit(root, inputs.base_ref),
    }
}

fn or_unknown(value: String) -> String {
    if value.trim().is_empty() {
        UNKNOWN.to_string()
    } else {
        value
    }
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<loom_version> <40-hex>|unknown` from `.loom/install-metadata.json`; the
/// abbreviated `loom_commit` older installs record is not a full SHA, so it
/// is reported as `unknown` rather than passed off as one.
#[must_use]
pub fn prompts_field(root: &Path) -> String {
    let Some(meta) = std::fs::read_to_string(root.join(".loom/install-metadata.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return UNKNOWN.to_string();
    };
    let version = meta["loom_version"].as_str().unwrap_or(UNKNOWN);
    let commit = ["loom_commit_full", "loom_commit"]
        .iter()
        .filter_map(|k| meta[*k].as_str())
        .find(|c| is_full_sha(c))
        .unwrap_or(UNKNOWN);
    format!("{version} {commit}")
}

/// The full SHA this branch forked from `base_ref` (default `origin/HEAD`,
/// then `origin/main`), or `unknown`.
fn base_commit(root: &Path, base_ref: Option<&str>) -> String {
    let refs: Vec<&str> = match base_ref {
        Some(r) => vec![r],
        None => vec!["origin/HEAD", "origin/main"],
    };
    refs.into_iter()
        .find_map(|r| {
            let out = Command::new("git")
                .current_dir(root)
                .args(["merge-base", "HEAD", r])
                .output()
                .ok()?;
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && is_full_sha(&sha)).then_some(sha)
        })
        .unwrap_or_else(|| UNKNOWN.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> Marker {
        Marker {
            build: "0.19.409 059d712020000000000000000000000000000000 clean".into(),
            prompts: "0.19.409 unknown".into(),
            sweep: "sweep-issue-9027-1790400000".into(),
            story: "rjwalters/loom#9027".into(),
            trace: "4e99cc89953fb6bd604a06896b83dc81".into(),
            host: "loom-worker-1".into(),
            base: UNKNOWN.into(),
        }
    }

    #[test]
    fn marker_round_trips_and_is_one_hidden_line() {
        let line = sample().render();
        assert_eq!(
            line,
            "<!-- loom:provenance v1 build=0.19.409 059d712020000000000000000000000000000000 clean \
             prompts=0.19.409 unknown sweep=sweep-issue-9027-1790400000 story=rjwalters/loom#9027 \
             trace=4e99cc89953fb6bd604a06896b83dc81 host=loom-worker-1 base=unknown -->"
        );
        assert!(!line.contains('\n'));
        assert_eq!(line.matches("-->").count(), 1);
        assert_eq!(Marker::parse(&line), Some(sample()));
    }

    #[test]
    fn hostile_values_cannot_forge_fields_or_close_the_comment() {
        let mut marker = sample();
        marker.sweep = "x --> <b>".into();
        marker.host = "h trace=0000".into();
        let line = marker.render();
        assert_eq!(line.matches("-->").count(), 1);
        let parsed = Marker::parse(&line).unwrap();
        assert_eq!(parsed.sweep, UNKNOWN);
        assert_eq!(parsed.host, UNKNOWN);
        assert_eq!(parsed.trace, sample().trace);
    }

    #[test]
    fn incomplete_or_foreign_lines_do_not_parse() {
        assert_eq!(Marker::parse("<!-- loom:provenance v1 build=x -->"), None);
        assert_eq!(Marker::parse("<!-- loom:lease host=a sweep=b -->"), None);
        let dup = sample()
            .render()
            .replace("base=unknown", "base=unknown build=x");
        assert_eq!(Marker::parse(&dup), None);
    }

    #[test]
    fn prompts_field_requires_a_full_sha() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(prompts_field(dir.path()), UNKNOWN);
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let meta = dir.path().join(".loom/install-metadata.json");
        std::fs::write(&meta, r#"{"loom_version":"0.19.211","loom_commit":"21c0923f"}"#).unwrap();
        assert_eq!(prompts_field(dir.path()), "0.19.211 unknown");
        let full = "21c0923fe515069f5f1b39827ae7c44d273e7ef1";
        std::fs::write(&meta, format!(r#"{{"loom_version":"0.19.211","loom_commit":"{full}"}}"#))
            .unwrap();
        assert_eq!(prompts_field(dir.path()), format!("0.19.211 {full}"));
    }
}
