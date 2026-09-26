//! The hidden `<!-- loom:provenance v1 … -->` record (#9027, harness-ops
//! D33 `docs/provenance.md` §PR marker).
//!
//! One line, `key=value` fields in the policy's order: `build prompts sweep
//! story trace host base run`, then the optional `installs`. Values may
//! contain spaces (`build=0.19.409 <40-hex> clean`), so a field runs until the
//! next `key=` token. [`super::sanitize`] guarantees no value contains `=` or
//! `>`, which is what makes that split unambiguous and keeps the comment
//! closed. Unrecognised keys (phase 2's `prompt_sha256=`, `runtime=`, …) are
//! skipped with their values, as the policy requires of parsers.
use std::path::Path;
use std::process::Command;

use super::{sanitize, NONE, UNKNOWN};

pub const PREFIX: &str = "<!-- loom:provenance v1 ";
const SUFFIX: &str = " -->";
const KEYS: [&str; 9] = [
    "build", "prompts", "sweep", "story", "trace", "host", "base", "run", "installs",
];
/// `installs` is optional; every other v1 key is always present.
const OPTIONAL: usize = 8;

/// One provenance record. Every field is sanitized text, [`UNKNOWN`] or [`NONE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// `<version> <40-hex> clean|dirty|unknown` — always three parts.
    pub build: String,
    /// `<version> <40-hex>` — always two parts (or `none`).
    pub prompts: String,
    pub sweep: String,
    pub story: String,
    pub trace: String,
    pub host: String,
    pub base: String,
    /// `<owner/repo>/actions/runs/<id>/<attempt>` or `none`.
    pub run: String,
    /// `<version> <40-hex>` of a program this action installs, if any.
    pub installs: Option<String>,
}

/// Sanitize `value` into exactly `parts` tokens, each unsafe or missing part
/// the literal `unknown`, so consumers that split the field keep its shape.
fn shaped(value: &str, parts: usize) -> String {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() == 1 && tokens[0] == NONE {
        return NONE.to_string();
    }
    if tokens.len() != parts {
        return vec![UNKNOWN; parts].join(" ");
    }
    tokens
        .iter()
        .map(|t| sanitize(t))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Marker {
    /// The single marker line.
    #[must_use]
    pub fn render(&self) -> String {
        let mut fields = vec![
            ("build", shaped(&self.build, 3)),
            ("prompts", shaped(&self.prompts, 2)),
            ("sweep", sanitize(&self.sweep)),
            ("story", sanitize(&self.story)),
            ("trace", sanitize(&self.trace)),
            ("host", sanitize(&self.host)),
            ("base", sanitize(&self.base)),
            ("run", sanitize(&self.run)),
        ];
        if let Some(installs) = &self.installs {
            fields.push(("installs", shaped(installs, 2)));
        }
        let body: Vec<String> = fields
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        format!("{PREFIX}{}{SUFFIX}", body.join(" "))
    }

    /// Parse a marker line; `None` unless every v1 key is present exactly
    /// once. Unrecognised `key=` tokens end the previous value and are
    /// skipped together with their own value.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        enum Slot {
            Start,
            Known(usize),
            Skipped,
        }
        let body = line.trim().strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
        let mut values: [Option<String>; 9] = Default::default();
        let mut slot = Slot::Start;
        for token in body.split_whitespace() {
            let key = token.split_once('=').filter(|(key, _)| {
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
            match (key, &slot) {
                (Some((key, value)), _) => match KEYS.iter().position(|k| *k == key) {
                    Some(i) => {
                        if values[i].is_some() {
                            return None;
                        }
                        values[i] = Some(value.to_string());
                        slot = Slot::Known(i);
                    }
                    None => slot = Slot::Skipped,
                },
                (None, Slot::Known(i)) => {
                    let value = values[*i].get_or_insert_with(String::new);
                    value.push(' ');
                    value.push_str(token);
                }
                (None, Slot::Skipped) => {}
                (None, Slot::Start) => return None,
            }
        }
        let installs = values[OPTIONAL].take();
        let [build, prompts, sweep, story, trace, host, base, run, _] = values;
        Some(Self {
            build: build?,
            prompts: prompts?,
            sweep: sweep?,
            story: story?,
            trace: trace?,
            host: host?,
            base: base?,
            run: run?,
            installs,
        })
    }
}

/// Inputs the caller supplies; everything else is derived here.
#[derive(Debug, Default)]
pub struct MarkerInputs<'a> {
    /// The issue the PR closes, when the caller already knows it.
    pub issue: Option<u32>,
    /// The PR body, to derive the story from its closing references (D32
    /// §"Which story a PR belongs to") when `issue` is not given.
    pub body: Option<&'a str>,
    pub sweep: Option<&'a str>,
    pub base_ref: Option<&'a str>,
}

/// Which story a PR belongs to, per D32: exactly one closing reference joins
/// that issue's story; zero is no story (`none`); more than one makes the PR
/// its own story, whose number does not exist until the PR is created, so
/// the record says `unknown` rather than borrowing one of the issues.
fn pr_story(root: &Path, inputs: &MarkerInputs<'_>) -> (String, String) {
    let issue = match (inputs.issue, inputs.body) {
        (Some(issue), _) => Some(issue),
        (None, Some(body)) => match crate::merge_pr::refs::closing_refs(body).as_slice() {
            [] => None,
            [one] => match u32::try_from(*one) {
                Ok(one) => Some(one),
                Err(_) => return (UNKNOWN.to_string(), UNKNOWN.to_string()),
            },
            _ => return (UNKNOWN.to_string(), UNKNOWN.to_string()),
        },
        (None, None) => None,
    };
    match issue {
        Some(issue) => super::story_and_trace(root, issue),
        None => (NONE.to_string(), UNKNOWN.to_string()),
    }
}

/// `run=`: the GitHub Actions run attempt that acted, `none` outside Actions,
/// `unknown` inside one whose coordinates are incomplete.
#[must_use]
pub fn run_field(env: impl Fn(&str) -> Option<String>) -> String {
    let get = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    let (repo, id, attempt) =
        (get("GITHUB_REPOSITORY"), get("GITHUB_RUN_ID"), get("GITHUB_RUN_ATTEMPT"));
    match (repo, id, attempt) {
        (Some(repo), Some(id), Some(attempt)) => format!("{repo}/actions/runs/{id}/{attempt}"),
        _ if get("GITHUB_ACTIONS").is_some_and(|v| v == "true") => UNKNOWN.to_string(),
        _ => NONE.to_string(),
    }
}

/// Build the PR-body record for the checkout at `root`.
#[must_use]
pub fn collect(root: &Path, inputs: &MarkerInputs<'_>) -> Marker {
    let (story, trace) = pr_story(root, inputs);
    // Outside a sweep there is no sweep id by design: `none`, not `unknown`.
    let sweep = inputs
        .sweep
        .map(str::to_string)
        .or_else(|| std::env::var("LOOM_SWEEP_ID").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| NONE.to_string());
    let host = crate::sweep_registry::host_identity();
    Marker {
        build: crate::self_update::BUILD_STAMP.to_string(),
        prompts: prompts_field(root),
        sweep,
        story,
        trace,
        host: if host == crate::sweep_registry::UNKNOWN_HOST {
            UNKNOWN.to_string()
        } else {
            host
        },
        base: base_commit(root, inputs.base_ref),
        run: run_field(|k| std::env::var(k).ok()),
        installs: None,
    }
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<loom_version> <40-hex>|unknown` from `.loom/install-metadata.json` (both
/// parts `unknown` when it is missing or unreadable); the
/// abbreviated `loom_commit` older installs record is not a full SHA, so it
/// is reported as `unknown` rather than passed off as one.
#[must_use]
pub fn prompts_field(root: &Path) -> String {
    let Some(meta) = std::fs::read_to_string(root.join(".loom/install-metadata.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return format!("{UNKNOWN} {UNKNOWN}");
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
            run: NONE.into(),
            installs: None,
        }
    }

    #[test]
    fn marker_round_trips_in_policy_key_order_and_is_one_hidden_line() {
        let line = sample().render();
        assert_eq!(
            line,
            "<!-- loom:provenance v1 build=0.19.409 059d712020000000000000000000000000000000 clean \
             prompts=0.19.409 unknown sweep=sweep-issue-9027-1790400000 story=rjwalters/loom#9027 \
             trace=4e99cc89953fb6bd604a06896b83dc81 host=loom-worker-1 base=unknown run=none -->"
        );
        assert!(!line.contains('\n'));
        assert_eq!(line.matches("-->").count(), 1);
        assert_eq!(Marker::parse(&line), Some(sample()));
        let mut with_installs = sample();
        with_installs.installs = Some("0.19.409 unknown".into());
        let line = with_installs.render();
        assert!(line.ends_with(" run=none installs=0.19.409 unknown -->"), "{line}");
        assert_eq!(Marker::parse(&line), Some(with_installs));
    }

    #[test]
    fn the_policy_example_line_parses() {
        // docs/provenance.md §PR marker (harness-ops #349), with the
        // placeholders filled in.
        let sha = "7e382af0000000000000000000000000000000aa";
        let line = format!(
            "<!-- loom:provenance v1 build=0.19.396 {sha} clean prompts=0.19.211 \
             21c0923fe515069f5f1b39827ae7c44d273e7ef1 sweep=sweep-issue-307-1790400000 \
             story=2AMLogic/harness-ops#307 trace=19d2a1429fe673430f31cff18afcc8cf \
             host=loom-worker-1 base={sha} run=none -->"
        );
        let m = Marker::parse(&line).unwrap();
        assert_eq!(m.build, format!("0.19.396 {sha} clean"));
        assert_eq!(m.story, "2AMLogic/harness-ops#307");
        assert_eq!(m.base, sha);
        assert_eq!(m.run, "none");
        assert_eq!(m.render(), line);
    }

    #[test]
    fn unknown_keys_are_skipped_not_glued_onto_the_previous_value() {
        let line = sample().render().replace(
            " run=none -->",
            " prompt_sha256=abc def run=none runtime=claude-code/2.1.x model=m -->",
        );
        let parsed = Marker::parse(&line).unwrap();
        assert_eq!(parsed.base, UNKNOWN);
        assert_eq!(parsed, sample());
    }

    #[test]
    fn unknown_parts_keep_the_field_shape() {
        let mut marker = sample();
        marker.build = UNKNOWN.into();
        marker.prompts = "only-one".into();
        let parsed = Marker::parse(&marker.render()).unwrap();
        assert_eq!(parsed.build, "unknown unknown unknown");
        assert_eq!(parsed.prompts, "unknown unknown");
        marker.build = "0.1 bad=part clean".into();
        assert_eq!(Marker::parse(&marker.render()).unwrap().build, "0.1 unknown clean");
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
        let no_run = sample().render().replace(" run=none", "");
        assert_eq!(Marker::parse(&no_run), None);
        let dup = sample()
            .render()
            .replace("base=unknown", "base=unknown build=x");
        assert_eq!(Marker::parse(&dup), None);
    }

    #[test]
    fn run_is_the_actions_attempt_or_none() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(run_field(env(&[])), "none");
        assert_eq!(
            run_field(env(&[
                ("GITHUB_ACTIONS", "true"),
                ("GITHUB_REPOSITORY", "2AMLogic/gf180-dx7"),
                ("GITHUB_RUN_ID", "20473627891"),
                ("GITHUB_RUN_ATTEMPT", "1"),
            ])),
            "2AMLogic/gf180-dx7/actions/runs/20473627891/1"
        );
        assert_eq!(run_field(env(&[("GITHUB_ACTIONS", "true")])), "unknown");
    }

    #[test]
    fn the_pr_story_follows_d32s_closing_reference_rule() {
        let dir = tempfile::tempdir().unwrap();
        let story = |body: &str| {
            pr_story(
                dir.path(),
                &MarkerInputs {
                    body: Some(body),
                    ..MarkerInputs::default()
                },
            )
        };
        assert_eq!(story("No closing reference."), ("none".into(), "unknown".into()));
        // Two issues: the PR is its own story, never the first issue's.
        assert_eq!(story("Closes #9068\nCloses #9027"), ("unknown".into(), "unknown".into()));
        // Exactly one: that issue's story (no GitHub origin here → none).
        assert_eq!(story("Fixes #7\nfixes #7"), ("none".into(), "unknown".into()));
    }

    #[test]
    fn prompts_field_requires_a_full_sha() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(prompts_field(dir.path()), "unknown unknown");
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
