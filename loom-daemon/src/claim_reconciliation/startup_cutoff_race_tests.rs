//! #12230: a slow startup claim pass running alongside work-finder ticks
//! yields exactly one dispatch per item.
//!
//! Since #12222 the work finder may tick while the startup pass still runs.
//! The pass's zero-evidence reclaim (`NoRecordAtStartup`, #6615) would see a
//! claim the work finder just made (no journal entry yet) as an orphan, flip
//! it back to `loom:issue`, and the next tick would dispatch it again. This
//! drives the real startup pass (`forge::reconcile_workspace`, through a fake
//! `gh`) and the real [`crate::work_finder::tick`] over one shared fake forge,
//! with the pass's listing held until the work finder's first tick has
//! claimed: the slow pass, interleaved.
//!
//! The dispatcher reports nothing in flight, as a peer host would (it cannot
//! see this host's registry): a claim flipped back to `loom:issue` is
//! dispatchable again by any host, which is the double dispatch at stake.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serial_test::serial;
use tempfile::tempdir;

use super::forge;
use crate::daemon_startup_reconciliation::set_claim_cutoff;
use crate::sweep_journal;
use crate::work_finder::{tick, WorkDispatcher, WorkItem, WorkSource};

/// GitHub's `updatedAt`: whole seconds.
fn github_time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// One file per issue, `<label> <updated_at>`; `label` is `issue` or
/// `building`.
struct Forge {
    dir: PathBuf,
}

impl Forge {
    fn set(&self, issue: u32, label: &str, at: DateTime<Utc>) {
        std::fs::write(self.dir.join(issue.to_string()), format!("{label} {}\n", github_time(at)))
            .unwrap();
    }

    fn with_label(&self, label: &str) -> Vec<u32> {
        let mut out: Vec<u32> = std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let text = std::fs::read_to_string(entry.path()).unwrap();
                (text.split_whitespace().next() == Some(label))
                    .then(|| entry.file_name().to_string_lossy().parse().unwrap())
            })
            .collect();
        out.sort_unstable();
        out
    }
}

/// The work finder's view: every `loom:issue`.
struct ForgeSource<'a>(&'a Forge);

impl WorkSource for ForgeSource<'_> {
    fn list_ready_issues(&mut self) -> anyhow::Result<Vec<WorkItem>> {
        Ok(self
            .0
            .with_label("issue")
            .into_iter()
            .map(|n| WorkItem::new(n, vec!["loom:issue".to_string()]))
            .collect())
    }
}

/// Claims (flips to `loom:building`, bumping `updatedAt`) and counts. Writes
/// no journal entry: the window before the post-spawn journal write, held
/// open for the whole test.
struct ForgeDispatcher<'a> {
    forge: &'a Forge,
    dispatches: BTreeMap<u32, usize>,
}

impl WorkDispatcher for ForgeDispatcher<'_> {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }

    fn dispatch(&mut self, issue: u32) -> anyhow::Result<bool> {
        self.forge.set(issue, "building", Utc::now());
        *self.dispatches.entry(issue).or_default() += 1;
        Ok(true)
    }
}

/// A fake `gh` over [`Forge`]: the building listing waits for `gate` (the slow
/// pass), the reclaim edit flips the file back to `issue`, and every other
/// call answers as `claim_reconciliation::tests::write_fake_gh` does.
fn fake_gh(dir: &Path, forge: &Forge, gate: &Path) -> PathBuf {
    const SCRIPT: &str = r#"#!/usr/bin/env bash
state='@STATE@'
if [[ "$*" == *'pulls?head='* ]]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'; echo '[]'; exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  if [[ "$*" == *'--add-label loom:issue'* ]]; then
    echo "issue $(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$state/$3"
  fi
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" != */comments* ]]; then
  for _ in $(seq 1 400); do [ -e '@GATE@' ] && break; sleep 0.05; done
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  rows=""
  for f in "$state"/*; do
    read -r label at < "$f"
    [ "$label" = building ] || continue
    row="{\"number\":${f##*/},\"state\":\"open\",\"labels\":[{\"name\":\"loom:building\"}],\"updated_at\":\"$at\"}"
    rows="$rows${rows:+,}$row"
  done
  echo "[$rows]"
  exit 0
fi
exit 0
"#;
    let bin = dir.join("fake-gh-race.sh");
    let script = SCRIPT
        .replace("@STATE@", &forge.dir.display().to_string())
        .replace("@GATE@", &gate.display().to_string());
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Three ready issues and one pre-restart orphan (#9, claimed three minutes
/// before the pass began, no evidence). The pass starts and lists slowly; the
/// work finder ticks once (claiming 1–3) before the listing returns, then
/// twice more after the pass. Returns the dispatch counts and the pass's
/// `(checked, reclaimed)`.
fn run(cutoff: Option<DateTime<Utc>>) -> (BTreeMap<u32, usize>, (usize, usize)) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, dir.path().join("sweeps.json"));
    let forge = Forge {
        dir: dir.path().join("forge"),
    };
    std::fs::create_dir_all(&forge.dir).unwrap();
    let start = Utc::now();
    for issue in 1..=3 {
        forge.set(issue, "issue", start - chrono::Duration::hours(1));
    }
    forge.set(9, "building", start - chrono::Duration::minutes(3));
    let gate = dir.path().join("first-tick-done");
    let gh = fake_gh(dir.path(), &forge, &gate);

    set_claim_cutoff(cutoff);
    let pass = {
        let (gh, root) = (gh.clone(), root.clone());
        std::thread::spawn(move || forge::reconcile_workspace(&gh, &root, true))
    };
    let mut dispatcher = ForgeDispatcher {
        forge: &forge,
        dispatches: BTreeMap::new(),
    };
    tick(&mut ForgeSource(&forge), &mut dispatcher, 10, false).unwrap();
    std::fs::write(&gate, "").unwrap();
    let pass_result = pass.join().unwrap();
    for _ in 0..2 {
        tick(&mut ForgeSource(&forge), &mut dispatcher, 10, false).unwrap();
    }
    set_claim_cutoff(None);
    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    (dispatcher.dispatches, pass_result)
}

#[test]
#[serial]
fn a_slow_startup_pass_beside_work_finder_ticks_dispatches_each_item_once() {
    let (dispatches, (checked, reclaimed)) = run(Some(Utc::now()));
    assert_eq!(checked, 4, "the pass saw the three fresh claims and the orphan");
    assert_eq!(reclaimed, 1, "only the pre-restart orphan is reclaimed");
    assert_eq!(
        dispatches,
        BTreeMap::from([(1, 1), (2, 1), (3, 1), (9, 1)]),
        "every item dispatched exactly once, the reclaimed orphan included"
    );
}

/// The control: with no cutoff (the pre-#12222 pass, had the work finder not
/// waited for it) the same interleaving reclaims the work finder's own fresh
/// claims and dispatches them twice. Without this the test above could pass
/// for a reason other than the cutoff.
#[test]
#[serial]
fn without_the_cutoff_the_same_interleaving_dispatches_twice() {
    let (dispatches, (_, reclaimed)) = run(None);
    assert_eq!(reclaimed, 4);
    assert_eq!(dispatches, BTreeMap::from([(1, 2), (2, 2), (3, 2), (9, 1)]));
}
