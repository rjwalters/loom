//! The role claim breadcrumb (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §4 "Role run", §9).
//!
//! A scheduled role takes its claim with a label it adds itself, from inside
//! its session: Judge adds `loom:reviewing` to a PR, Doctor `loom:treating`,
//! Curator `loom:curating` to an issue. The daemon never sees that, so before
//! #10832 a role run a roll had to requeue left its claim label behind until
//! the role's own staleness rule (30 to 60 minutes) released it.
//!
//! The pause hook already runs for every tool call of a daemon-dispatched
//! agent. It now also recognises the one command that takes a claim (`gh pr
//! edit <n> --add-label <claim label>`, or `gh issue edit …`) and records it
//! next to the item's other pause state as `claim.json`. A requeue reads that
//! file and releases exactly the label it names; a resume carries it to the
//! resumed run's item. The matching `--remove-label` clears it.
//!
//! **When it is written.** On the post-tool-use event, so only a claim the
//! forge accepted is recorded. A runtime with no post event (Codex, whose
//! managed hook is pre-tool-use only, [`super::LEDGER_ENV`] `=0`) records on
//! pre-tool-use instead.
//!
//! **What it deliberately does not do.** It parses the command an agent ran; it
//! runs nothing. A command it cannot read (a number held in a variable it
//! cannot resolve, a label set built elsewhere) leaves no breadcrumb, and the
//! role's staleness rule still releases that claim, exactly as before.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name inside an item's pause state dir.
pub const CLAIM_FILE: &str = "claim.json";

/// The labels a scheduled role claims work with.
pub const ROLE_CLAIM_LABELS: &[&str] = &["loom:reviewing", "loom:treating", "loom:curating"];

/// One recorded claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimBreadcrumb {
    /// The claim label (`loom:reviewing`, …).
    pub label: String,
    /// `pr` or `issue`: what the label is on.
    pub on: String,
    /// The PR or issue number.
    pub number: u32,
    /// When the claim was recorded.
    #[serde(default)]
    pub at: Option<String>,
}

impl ClaimBreadcrumb {
    /// The manifest's `claim` value for this breadcrumb.
    #[must_use]
    pub fn manifest_value(&self) -> serde_json::Value {
        serde_json::json!({ "label": self.label, "on": self.on, "number": self.number })
    }

    /// Read a manifest `claim` value back. `None` for a sweep's claim (which
    /// carries no number) and for anything malformed.
    #[must_use]
    pub fn from_manifest(value: &serde_json::Value) -> Option<Self> {
        let label = value.get("label")?.as_str()?.to_string();
        let on = value.get("on")?.as_str()?.to_string();
        let number = u32::try_from(value.get("number")?.as_u64()?).ok()?;
        (ROLE_CLAIM_LABELS.contains(&label.as_str()) && matches!(on.as_str(), "pr" | "issue"))
            .then_some(ClaimBreadcrumb {
                label,
                on,
                number,
                at: None,
            })
    }
}

/// What one command does to a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimEdit {
    /// It takes the claim.
    Take(ClaimBreadcrumb),
    /// It releases `label` on `number`.
    Release { label: String, number: u32 },
}

fn unquote(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'')
}

/// Resolve `$NAME` / `${NAME}` against the `NAME=<digits>` assignments seen so
/// far in the same command string, and parse the result as a number.
fn number_of(token: &str, vars: &[(String, u32)]) -> Option<u32> {
    let token = unquote(token);
    if let Some(name) = token.strip_prefix('$') {
        let name = name.trim_start_matches('{').trim_end_matches('}');
        return vars.iter().rev().find(|(n, _)| n == name).map(|(_, v)| *v);
    }
    token.trim_start_matches('#').parse().ok()
}

/// The claim edits in one shell command string, in order. Recognises
/// `gh pr edit <n> … --add-label <labels>` and `--remove-label`, for a PR or an
/// issue, where a label is one of [`ROLE_CLAIM_LABELS`]. Anything else is
/// ignored.
#[must_use]
pub fn parse_claim_edits(command: &str) -> Vec<ClaimEdit> {
    let mut edits = Vec::new();
    let mut vars: Vec<(String, u32)> = Vec::new();
    // One simple command at a time: an `&&`, `;`, `|` or newline ends it.
    for segment in command.split(['\n', ';', '|', '&']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        for token in &tokens {
            if let Some((name, value)) = token.split_once('=') {
                let simple =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if let (true, Ok(n)) = (simple, unquote(value).parse::<u32>()) {
                    vars.push((name.to_string(), n));
                }
            }
        }
        let Some(gh) = tokens.iter().position(|t| *t == "gh" || t.ends_with("/gh")) else {
            continue;
        };
        let rest = &tokens[gh + 1..];
        let Some(on) = rest
            .first()
            .copied()
            .filter(|t| matches!(*t, "pr" | "issue"))
        else {
            continue;
        };
        if rest.get(1).copied() != Some("edit") {
            continue;
        }
        let mut number = None;
        let mut takes: Vec<String> = Vec::new();
        let mut releases: Vec<String> = Vec::new();
        let mut i = 2;
        while i < rest.len() {
            let token = rest[i];
            let (flag, inline) = match token.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v)),
                _ => (token, None),
            };
            let target = match flag {
                "--add-label" => Some(&mut takes),
                "--remove-label" => Some(&mut releases),
                _ => None,
            };
            if let Some(target) = target {
                let value = inline.or_else(|| {
                    i += 1;
                    rest.get(i).copied()
                });
                if let Some(value) = value {
                    target.extend(
                        unquote(value)
                            .split(',')
                            .map(|l| unquote(l.trim()).to_string())
                            .filter(|l| ROLE_CLAIM_LABELS.contains(&l.as_str())),
                    );
                }
            } else if flag.starts_with('-') {
                // A flag with a value this does not care about (`-R o/r`,
                // `--repo o/r`): skip the value when it is a separate token.
                if inline.is_none() && matches!(flag, "-R" | "--repo") {
                    i += 1;
                }
            } else if number.is_none() {
                number = number_of(token, &vars);
            }
            i += 1;
        }
        let Some(number) = number else { continue };
        edits.extend(
            releases
                .into_iter()
                .map(|label| ClaimEdit::Release { label, number }),
        );
        edits.extend(takes.into_iter().map(|label| {
            ClaimEdit::Take(ClaimBreadcrumb {
                label,
                on: on.to_string(),
                number,
                at: None,
            })
        }));
    }
    edits
}

/// The item's recorded claim, if any.
#[must_use]
pub fn read(item_dir: &Path) -> Option<ClaimBreadcrumb> {
    serde_json::from_str(&std::fs::read_to_string(item_dir.join(CLAIM_FILE)).ok()?).ok()
}

/// Record `claim` for the item.
///
/// # Errors
/// When the file cannot be written.
pub fn write(item_dir: &Path, claim: &ClaimBreadcrumb) -> std::io::Result<()> {
    let body = serde_json::to_vec(claim).map_err(std::io::Error::other)?;
    super::write_atomic(&item_dir.join(CLAIM_FILE), &body)
}

/// Forget the item's claim.
pub fn clear(item_dir: &Path) {
    let _ = std::fs::remove_file(item_dir.join(CLAIM_FILE));
}

/// Carry a paused run's claim to the item dir of the run that resumes it.
pub fn carry(from_item_dir: &Path, to_item_dir: &Path) {
    if let Some(claim) = read(from_item_dir) {
        let _ = write(to_item_dir, &claim);
    }
}

fn command_of(payload: &serde_json::Value) -> Option<String> {
    let input = payload.get("tool_input")?;
    ["command", "cmd"].iter().find_map(|k| match input.get(*k) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(parts)) => Some(
            parts
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    })
}

/// The pause hook's breadcrumb step for one event: apply the claim edits of a
/// shell command the agent ran. `ledger` is whether this runtime delivers
/// post-tool-use events (see the module doc). Never fails.
pub fn observe(item_dir: &Path, event: &str, payload: &serde_json::Value, ledger: bool) {
    let commit = event == "PostToolUse" || (event == "PreToolUse" && !ledger);
    if !commit {
        return;
    }
    let Some(command) = command_of(payload) else {
        return;
    };
    // The cheap test first: almost no command names a claim label.
    let names_a_claim = ROLE_CLAIM_LABELS.iter().any(|l| command.contains(l));
    if !names_a_claim && !command.contains("post-verdict.sh") {
        return;
    }
    for edit in parse_claim_edits(&command) {
        match edit {
            ClaimEdit::Take(mut claim) => {
                claim.at =
                    Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                let _ = write(item_dir, &claim);
            }
            ClaimEdit::Release { label, number } => {
                if read(item_dir).is_some_and(|c| c.label == label && c.number == number) {
                    clear(item_dir);
                }
            }
        }
    }
    // `post-verdict.sh` strips `loom:reviewing` itself when it posts a verdict.
    if command.contains("post-verdict.sh")
        && read(item_dir).is_some_and(|c| c.label == "loom:reviewing")
    {
        clear(item_dir);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "claim_breadcrumb_tests.rs"]
mod tests;
