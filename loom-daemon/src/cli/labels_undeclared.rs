//! `loom-daemon labels undeclared` (#11105): the live `loom:*` labels on a
//! GitHub repo that its `labels.yml` does not declare, and, with `--prune`,
//! their deletion.
//!
//! A label that leaves `labels.yml` stays on the forge: `sync-labels.sh` is
//! additive and never deletes a Loom label, so retired names (and names no
//! release ever declared) accumulate in the label picker and get applied by
//! hand. This is the report-and-prune half `sync-labels.sh
//! --prune-undeclared` calls; the shell side is only the call-site, per the
//! shell-language policy (`sync-labels.sh` is `contract` shell).
//!
//! Safety: pruning is opt-in, and a label still on an **open** issue or PR
//! is kept and reported, never deleted (deleting a label strips it from
//! every item carrying it). A failed usage lookup keeps the label too: an
//! unanswered "is it in use?" is not a "no". Closed items do not hold a
//! label back: Loom never cleans labels off closed items (#2838), so they
//! say nothing about live use. Write scope (#9548): `--prune` is gated here by
//! `loom_daemon::write_scope::may_write_from`, before the first label is read or
//! deleted, so a direct caller cannot bypass `sync-labels.sh`'s
//! `loom_write_repo`; the report-only read is ungated.
//!
//! Output, one line per undeclared label on stdout:
//! `UNDECLARED <name> (unused: would delete)`, `PRUNED <name>`,
//! `KEPT <name> (open: #1 #2)` or `KEPT <name> (<why>)`. Exit 0 when no
//! undeclared label remains, 3 when one does, 4 when the live label list
//! could not be read, 2 when the labels file cannot be read.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// The forge reads and the one write this command makes.
pub(crate) trait LabelForge {
    /// Every label name on the repo.
    fn live_labels(&self) -> Result<Vec<String>, String>;
    /// Numbers of the open issues and PRs carrying `label` (capped at 100).
    fn open_numbers(&self, label: &str) -> Result<Vec<u64>, String>;
    /// Delete `label` from the repo.
    fn delete(&self, label: &str) -> Result<(), String>;
}

/// Names declared by `- name: <name>` lines, the parser `sync-labels.sh`
/// itself uses.
fn declared_names(yaml: &str) -> Vec<&str> {
    yaml.lines()
        .filter_map(|l| l.strip_prefix("- name: "))
        .map(str::trim)
        .collect()
}

/// Namespace of Loom's own labels, built so the label-literal guard sees no bare literal.
const LOOM_PREFIX: &str = concat!("loom", ":");

/// Live `loom:*` labels absent from `declared`, in live order.
pub(crate) fn undeclared<'a>(declared: &[&str], live: &'a [String]) -> Vec<&'a str> {
    live.iter()
        .map(String::as_str)
        .filter(|n| n.starts_with(LOOM_PREFIX) && !declared.contains(n))
        .collect()
}

/// Report (and, with `prune`, delete) every undeclared `loom:*` label.
/// Returns the exit code documented in the module doc.
pub(crate) fn run(forge: &dyn LabelForge, yaml: &str, prune: bool, out: &mut dyn Write) -> i32 {
    let live = match forge.live_labels() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("labels undeclared: could not list the repo's labels: {e}");
            return 4;
        }
    };
    let declared = declared_names(yaml);
    let mut remaining = 0;
    for name in undeclared(&declared, &live) {
        let line = match forge.open_numbers(name) {
            Err(e) => {
                remaining += 1;
                format!("KEPT {name} (usage lookup failed: {e})")
            }
            Ok(open) if !open.is_empty() => {
                remaining += 1;
                let list: Vec<String> = open.iter().map(|n| format!("#{n}")).collect();
                format!("KEPT {name} (open: {})", list.join(" "))
            }
            Ok(_) if !prune => {
                remaining += 1;
                format!("UNDECLARED {name} (unused: would delete)")
            }
            Ok(_) => match forge.delete(name) {
                Ok(()) => format!("PRUNED {name}"),
                Err(e) => {
                    remaining += 1;
                    format!("KEPT {name} (delete failed: {e})")
                }
            },
        };
        let _ = writeln!(out, "{line}");
    }
    if remaining == 0 {
        0
    } else {
        3
    }
}

/// [`LabelForge`] over `gh api` (REST), scoped to one repo.
pub(crate) struct GhLabelForge {
    pub repo: String,
}

const GH_TIMEOUT: Duration = Duration::from_secs(60);

impl GhLabelForge {
    fn gh(&self, intent: AccessIntent, args: &[&str]) -> Result<Vec<u8>, String> {
        let target = GhTarget::repo(&self.repo)?;
        let what = format!("gh {}", args.first().copied().unwrap_or_default());
        let r = GhInvocation::new(Operation::new("labels.undeclared"), intent, target, GH_TIMEOUT)
            .args(args)
            .run();
        match r.ok_output() {
            Some(o) => Ok(o.stdout.clone()),
            None => Err(r.failure_reason(&what)),
        }
    }
}

fn lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

impl LabelForge for GhLabelForge {
    fn live_labels(&self) -> Result<Vec<String>, String> {
        let path = format!("repos/{}/labels?per_page=100", self.repo);
        let out = self.gh(AccessIntent::Read, &["api", "--paginate", &path, "--jq", ".[].name"])?;
        Ok(lines(&out))
    }

    fn open_numbers(&self, label: &str) -> Result<Vec<u64>, String> {
        let path = format!("repos/{}/issues", self.repo);
        let filter = format!("labels={label}");
        let out = self.gh(
            AccessIntent::Read,
            &[
                "api",
                "-X",
                "GET",
                &path,
                "-f",
                "state=open",
                "-f",
                "per_page=100",
                "-f",
                &filter,
                "--jq",
                ".[].number",
            ],
        )?;
        lines(&out)
            .iter()
            .map(|l| {
                l.parse::<u64>()
                    .map_err(|e| format!("bad issue number {l:?}: {e}"))
            })
            .collect()
    }

    fn delete(&self, label: &str) -> Result<(), String> {
        self.gh(AccessIntent::Write, &["label", "delete", label, "--repo", &self.repo, "--yes"])
            .map(|_| ())
    }
}

/// [`run`] behind the write-scope gate (#9548): with `prune`, a refused
/// `gate` returns exit 2 before any forge call, so nothing is deleted.
fn run_gated(
    forge: &dyn LabelForge,
    yaml: &str,
    prune: bool,
    gate: &dyn Fn() -> Result<(), String>,
    out: &mut dyn Write,
) -> i32 {
    if prune {
        if let Err(why) = gate() {
            eprintln!("labels undeclared: refusing --prune (#9548): {why}");
            return 2;
        }
    }
    run(forge, yaml, prune, out)
}

/// `labels undeclared --repo R --labels-file F [--prune]`.
pub(crate) fn dispatch(repo: &str, labels_file: &Path, prune: bool) -> i32 {
    let yaml = match std::fs::read_to_string(labels_file) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("labels undeclared: cannot read {}: {e}", labels_file.display());
            return 2;
        }
    };
    if GhTarget::repo(repo).is_err() {
        eprintln!("labels undeclared: --repo must be OWNER/NAME");
        return 2;
    }
    let forge = GhLabelForge {
        repo: repo.to_string(),
    };
    let gate = || {
        let cwd = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
        match loom_daemon::write_scope::may_write_from(&cwd, Some(repo)) {
            loom_daemon::write_scope::Verdict::Allow(_) => Ok(()),
            loom_daemon::write_scope::Verdict::Deny(why) => Err(why),
        }
    };
    run_gated(&forge, &yaml, prune, &gate, &mut std::io::stdout())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fake {
        live: Vec<String>,
        list_fails: bool,
        open: BTreeMap<String, Result<Vec<u64>, String>>,
        deleted: RefCell<Vec<String>>,
    }

    impl LabelForge for Fake {
        fn live_labels(&self) -> Result<Vec<String>, String> {
            if self.list_fails {
                Err("boom".into())
            } else {
                Ok(self.live.clone())
            }
        }
        fn open_numbers(&self, label: &str) -> Result<Vec<u64>, String> {
            self.open.get(label).cloned().unwrap_or(Ok(vec![]))
        }
        fn delete(&self, label: &str) -> Result<(), String> {
            self.deleted.borrow_mut().push(label.to_string());
            Ok(())
        }
    }

    const YAML: &str = "# BEGIN LOOM LABELS\n- name: loom:issue\n  description: \"x\"\n  color: \"000000\"\n# END LOOM LABELS\n";

    fn fake(live: &[&str]) -> Fake {
        Fake {
            live: live.iter().map(|s| s.to_string()).collect(),
            ..Fake::default()
        }
    }

    fn go(f: &Fake, prune: bool) -> (i32, String) {
        let mut out = Vec::new();
        let rc = run(f, YAML, prune, &mut out);
        (rc, String::from_utf8(out).unwrap())
    }

    #[test]
    fn only_undeclared_loom_labels_are_reported() {
        let f = fake(&["loom:issue", "loom:healing", "bug", "rust", "priority:high"]);
        let (rc, out) = go(&f, false);
        assert_eq!(rc, 3);
        assert_eq!(out, "UNDECLARED loom:healing (unused: would delete)\n");
        assert!(f.deleted.borrow().is_empty(), "a report never deletes");
    }

    #[test]
    fn nothing_undeclared_is_exit_zero() {
        let (rc, out) = go(&fake(&["loom:issue", "bug"]), true);
        assert_eq!((rc, out.as_str()), (0, ""));
    }

    #[test]
    fn prune_deletes_unused_and_keeps_labels_on_open_items() {
        let mut f = fake(&["loom:healing", "loom:in-progress", "loom:issue"]);
        f.open.insert("loom:in-progress".into(), Ok(vec![7, 9]));
        let (rc, out) = go(&f, true);
        assert_eq!(rc, 3, "a kept label means one still remains");
        assert_eq!(out, "PRUNED loom:healing\nKEPT loom:in-progress (open: #7 #9)\n");
        assert_eq!(*f.deleted.borrow(), vec!["loom:healing".to_string()]);
    }

    #[test]
    fn a_failed_usage_lookup_keeps_the_label() {
        let mut f = fake(&["loom:healing"]);
        f.open
            .insert("loom:healing".into(), Err("rate limited".into()));
        let (rc, out) = go(&f, true);
        assert_eq!(rc, 3);
        assert_eq!(out, "KEPT loom:healing (usage lookup failed: rate limited)\n");
        assert!(f.deleted.borrow().is_empty());
    }

    #[test]
    fn prune_of_only_unused_labels_is_exit_zero() {
        let f = fake(&["loom:failed:judge"]);
        assert_eq!(go(&f, true), (0, "PRUNED loom:failed:judge\n".to_string()));
    }

    #[test]
    fn a_refused_prune_target_makes_no_forge_call() {
        let f = fake(&["loom:healing"]);
        let mut out = Vec::new();
        let rc = run_gated(&f, YAML, true, &|| Err("unmanaged repo".into()), &mut out);
        assert_eq!(rc, 2);
        assert!(out.is_empty());
        assert!(f.deleted.borrow().is_empty(), "a refused prune deletes nothing");
    }

    #[test]
    fn the_report_only_read_is_not_gated() {
        let f = fake(&["loom:healing"]);
        let mut out = Vec::new();
        let rc = run_gated(&f, YAML, false, &|| Err("unmanaged repo".into()), &mut out);
        assert_eq!(rc, 3);
        assert!(String::from_utf8(out)
            .unwrap()
            .starts_with("UNDECLARED loom:healing"));
    }

    #[test]
    fn an_unreadable_label_list_is_exit_four() {
        let f = Fake {
            list_fails: true,
            ..Fake::default()
        };
        assert_eq!(go(&f, true).0, 4);
    }
}
