//! The `gh`-backed [`Source`] and [`Landing`] for the daemon's own resync
//! landing (#13487). Thin on purpose: the rules are in [`super::waiver`].

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use super::waiver::{self, Checks, File, Landing, Pr, Source, Waived};

/// Upstream, where the tag and the `defaults/` blobs live.
const UPSTREAM: &str = "rjwalters/loom";

/// `gh` reads and writes for one repo's resync PR.
pub struct Gh<'a> {
    pub root: &'a Path,
    pub nwo: &'a str,
}

impl Gh<'_> {
    fn gh(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(crate::gh_invocation::gh_bin())
            .args(args)
            .current_dir(self.root)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("gh: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn json(&self, path: &str) -> Result<Value, String> {
        serde_json::from_str(&self.gh(&["api", path])?).map_err(|e| e.to_string())
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
        let v = self.json(&format!("repos/{}/pulls/{number}", self.nwo))?;
        Ok(Pr {
            title: v["title"].as_str().ok_or("no title")?.to_string(),
            head_sha: v["head"]["sha"].as_str().ok_or("no head")?.to_string(),
            changed_files: usize::try_from(v["changed_files"].as_u64().ok_or("no count")?)
                .map_err(|e| e.to_string())?,
        })
    }

    fn files(&self, number: u32) -> Result<Vec<File>, String> {
        let raw = self.gh(&[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{}/pulls/{number}/files?per_page=100", self.nwo),
        ])?;
        let pages: Vec<Vec<Value>> = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        Ok(pages
            .into_iter()
            .flatten()
            .map(|f| File {
                filename: f["filename"].as_str().unwrap_or_default().to_string(),
                previous_filename: f["previous_filename"].as_str().map(str::to_string),
                sha: f["sha"].as_str().unwrap_or_default().to_string(),
                status: f["status"].as_str().unwrap_or_default().to_string(),
            })
            .collect())
    }

    fn metadata(&self, sha: &str) -> Result<String, String> {
        self.gh(&[
            "api",
            "-H",
            "Accept: application/vnd.github.raw",
            &format!("repos/{}/contents/.loom/install-metadata.json?ref={sha}", self.nwo),
        ])
    }

    fn tag_exists(&self, tag: &str) -> Result<bool, String> {
        let v = self.json(&format!("repos/{UPSTREAM}/git/ref/tags/{tag}"))?;
        Ok(v["ref"].as_str() == Some(&format!("refs/tags/{tag}")))
    }

    fn upstream_blob(&self, path: &str, tag: &str) -> Result<Option<String>, String> {
        let v = self.json(&format!("repos/{UPSTREAM}/contents/{}?ref={tag}", encode_path(path)))?;
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

    fn relabel(&self, number: u32, add: &[&str], remove: &[&str]) -> Result<(), String> {
        for label in add {
            self.gh(&[
                "api",
                "-X",
                "POST",
                &format!("repos/{}/issues/{number}/labels", self.nwo),
                "-f",
                &format!("labels[]={label}"),
            ])?;
        }
        for label in remove {
            self.gh(&[
                "api",
                "-X",
                "DELETE",
                &format!("repos/{}/issues/{number}/labels/{}", self.nwo, label.replace(':', "%3A")),
            ])?;
        }
        Ok(())
    }

    fn comment(&self, number: u32, body: &str) -> Result<(), String> {
        self.gh(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/issues/{number}/comments", self.nwo),
            "-f",
            &format!("body={body}"),
        ])
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
            match probe.gh(&[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ]) {
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
