//! The `gh`-backed [`Source`] and [`Landing`] for the daemon's own resync
//! landing (#13487). Thin on purpose: the rules are in [`super::waiver`].

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::waiver::{self, Checks, File, Landing, Pr, Source, Waived};
use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Per-call deadline for one `gh` request.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Upstream, where the tag and the `defaults/` blobs live.
const UPSTREAM: &str = "rjwalters/loom";

/// `gh` reads and writes for one repo's resync PR.
pub struct Gh<'a> {
    pub root: &'a Path,
    pub nwo: &'a str,
}

impl Gh<'_> {
    /// One `gh` call through the facade (counted, bounded, credentialed),
    /// accounted under `op`. A read is reader-only, as every daemon read is.
    fn gh(&self, op: &'static str, intent: AccessIntent, args: &[&str]) -> Result<String, String> {
        let inv = GhInvocation::new(Operation::new(op), intent, GhTarget::None, GH_TIMEOUT)
            .args(args)
            .current_dir(self.root);
        let inv = if intent == AccessIntent::Read {
            inv.reader_only()
        } else {
            inv
        };
        match inv.run() {
            CmdOutcome::Ran(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            CmdOutcome::Ran(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            CmdOutcome::Unavailable(u) => Err(format!("gh ({op}): {u}")),
        }
    }

    fn json(&self, op: &'static str, path: &str) -> Result<Value, String> {
        serde_json::from_str(&self.gh(op, AccessIntent::Read, &["api", path])?)
            .map_err(|e| e.to_string())
    }
}

/// Percent-encode each path segment, keeping the slashes.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl Source for Gh<'_> {
    fn pr(&self, number: u32) -> Result<Pr, String> {
        let v = self.json("resync_waiver.pr", &format!("repos/{}/pulls/{number}", self.nwo))?;
        Ok(Pr {
            title: v["title"].as_str().ok_or("no title")?.to_string(),
            head_sha: v["head"]["sha"].as_str().ok_or("no head")?.to_string(),
            changed_files: usize::try_from(v["changed_files"].as_u64().ok_or("no count")?)
                .map_err(|e| e.to_string())?,
        })
    }

    fn files(&self, number: u32) -> Result<Vec<File>, String> {
        // `--jq '.[]'` instead of `--slurp`: the page walk carries `--jq`, not
        // `--slurp`, and one object per file is all the proof reads.
        let raw = self.gh(
            "resync_waiver.files",
            AccessIntent::Read,
            &[
                "api",
                "--paginate",
                "--jq",
                ".[]",
                &format!("repos/{}/pulls/{number}/files?per_page=100", self.nwo),
            ],
        )?;
        let files: Vec<Value> = serde_json::Deserializer::from_str(&raw)
            .into_iter::<Value>()
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        Ok(files
            .into_iter()
            .map(|f| File {
                filename: f["filename"].as_str().unwrap_or_default().to_string(),
                previous_filename: f["previous_filename"].as_str().map(str::to_string),
                sha: f["sha"].as_str().unwrap_or_default().to_string(),
                status: f["status"].as_str().unwrap_or_default().to_string(),
            })
            .collect())
    }

    fn metadata(&self, sha: &str) -> Result<String, String> {
        self.gh(
            "resync_waiver.metadata",
            AccessIntent::Read,
            &[
                "api",
                "-H",
                "Accept: application/vnd.github.raw",
                &format!("repos/{}/contents/.loom/install-metadata.json?ref={sha}", self.nwo),
            ],
        )
    }

    fn tag_exists(&self, tag: &str) -> Result<bool, String> {
        let v = self.json("resync_waiver.tag", &format!("repos/{UPSTREAM}/git/ref/tags/{tag}"))?;
        Ok(v["ref"].as_str() == Some(&format!("refs/tags/{tag}")))
    }

    fn upstream_blob(&self, path: &str, tag: &str) -> Result<Option<String>, String> {
        let v = self.json(
            "resync_waiver.upstream_blob",
            &format!("repos/{UPSTREAM}/contents/{}?ref={tag}", encode_path(path)),
        )?;
        Ok((v["type"].as_str() == Some("file"))
            .then(|| v["sha"].as_str().map(str::to_string))
            .flatten())
    }
}

impl Landing for Gh<'_> {
    fn checks(&self, _number: u32, head: &str) -> Checks {
        let Ok(exe) = std::env::current_exe() else {
            return Checks::NotGreen("cannot locate loom-daemon".to_string());
        };
        let out = Command::new(exe)
            .args([
                "forge",
                "wait-checks",
                head,
                "--repo",
                self.nwo,
                "--timeout",
                "0",
            ])
            .current_dir(self.root)
            .stdin(Stdio::null())
            .output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        match text.as_deref().map(str::trim) {
            Ok(t) if t.starts_with("LOOM-CHECKS-GREEN") => Checks::Green,
            Ok(t) if !t.is_empty() => Checks::NotGreen(t.to_string()),
            _ => Checks::NotGreen("no verdict from wait-checks".to_string()),
        }
    }

    fn labels(&self, number: u32) -> Result<Vec<String>, String> {
        let v = self.json(
            "resync_waiver.labels",
            &format!("repos/{}/issues/{number}/labels?per_page=100", self.nwo),
        )?;
        Ok(v.as_array()
            .ok_or("labels: not a list")?
            .iter()
            .filter_map(|l| l["name"].as_str().map(str::to_string))
            .collect())
    }

    fn relabel(&self, number: u32, add: &[&str], remove: &[&str]) -> Result<(), String> {
        for label in add {
            self.gh(
                "resync_waiver.add_label",
                AccessIntent::Write,
                &[
                    "api",
                    "-X",
                    "POST",
                    &format!("repos/{}/issues/{number}/labels", self.nwo),
                    "-f",
                    &format!("labels[]={label}"),
                ],
            )?;
        }
        for label in remove {
            self.gh(
                "resync_waiver.remove_label",
                AccessIntent::Write,
                &[
                    "api",
                    "-X",
                    "DELETE",
                    &format!(
                        "repos/{}/issues/{number}/labels/{}",
                        self.nwo,
                        label.replace(':', "%3A")
                    ),
                ],
            )?;
        }
        Ok(())
    }

    fn comment(&self, number: u32, body: &str) -> Result<(), String> {
        self.gh(
            "resync_waiver.comment",
            AccessIntent::Write,
            &[
                "api",
                "-X",
                "POST",
                &format!("repos/{}/issues/{number}/comments", self.nwo),
                "-f",
                &format!("body={body}"),
            ],
        )
        .map(|_| ())
    }

    fn merge(&self, number: u32, head: &str) -> Result<(), String> {
        let script = self.root.join(".loom/scripts/merge-pr.sh");
        if !script.is_file() {
            return Err("the repo has no .loom/scripts/merge-pr.sh".to_string());
        }
        let out = Command::new(script)
            .args([&number.to_string(), "--expected-head", head])
            .current_dir(self.root)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(err.lines().last().unwrap_or("non-zero exit").to_string())
        }
    }
}

/// The production `waive` hook.
pub fn waive(root: &Path, nwo: &str, number: u32) -> Waived {
    // The label, comment and merge below are daemon forge writes: scoped by
    // `write_scope` (#9548) here too, not only by the pass that calls this.
    if !crate::write_scope::gate_repo_with(
        root,
        nwo,
        &crate::write_scope::default_gh(),
        "resync waiver landing (#13487)",
    ) {
        return Waived::Held("the write scope does not allow this repo".to_string());
    }
    let gh = Gh { root, nwo };
    waiver::land(&gh, &gh, number)
}

/// `loom-daemon forge resync-waiver <PR> [--repo OWNER/REPO]`: the proof for
/// Champion's `resync_waiver_eval` (#13487). Exit 0 prints
/// `RESYNC_WAIVER=ELIGIBLE`, `RESYNC_WAIVER_TAG=<tag>` and one
/// `RESYNC_WAIVER_RECORD=<path>: <sha>` line per kept-set file; any other
/// outcome exits 1 with `RESYNC_WAIVER=NOT_ELIGIBLE` and the reason on
/// stderr. Callers treat any non-zero exit as `NOT ELIGIBLE`.
pub fn cli(pr: u32, repo: Option<&str>) -> i32 {
    let root = std::env::current_dir().unwrap_or_default();
    let nwo = match repo {
        Some(r) => r.to_string(),
        None => {
            let probe = Gh {
                root: &root,
                nwo: "",
            };
            match probe.gh(
                "resync_waiver.repo_view",
                AccessIntent::Read,
                &[
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "--jq",
                    ".nameWithOwner",
                ],
            ) {
                Ok(out) if out.trim().contains('/') => out.trim().to_string(),
                _ => {
                    println!("RESYNC_WAIVER=NOT_ELIGIBLE");
                    eprintln!("repo unreadable");
                    return 1;
                }
            }
        }
    };
    let gh = Gh {
        root: &root,
        nwo: &nwo,
    };
    match waiver::evaluate(&gh, pr) {
        waiver::Verdict::Eligible { tag, record, .. } => {
            println!("RESYNC_WAIVER=ELIGIBLE");
            println!("RESYNC_WAIVER_TAG={tag}");
            for (path, sha) in record {
                println!("RESYNC_WAIVER_RECORD={path}: {sha}");
            }
            0
        }
        waiver::Verdict::NotEligible(why) => {
            println!("RESYNC_WAIVER=NOT_ELIGIBLE");
            eprintln!("{why}");
            1
        }
    }
}
