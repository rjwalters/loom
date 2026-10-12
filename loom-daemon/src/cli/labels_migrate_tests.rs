//! Fake-forge tests for `labels migrate` (#11511). Every safety rule in the
//! module doc has a test here; the fake records every call, reads included,
//! so "no write" and "no call at all" are both assertable.

use super::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

const STAR: &str = "loom:operator-priority";
const IMPORTANT: &str = "loom:important";
const TIER: &str = "tier:maintenance";
const UNOWNED: &str = "consumer:legacy-prio";

#[derive(Default)]
struct Fake {
    live: RefCell<BTreeSet<String>>,
    /// Open items and their labels.
    items: RefCell<BTreeMap<u64, BTreeSet<String>>>,
    /// Every call, in order: `live`, `list <label> <page>`, `add #n l`,
    /// `remove #n l`, `delete l`.
    calls: RefCell<Vec<String>>,
    list_fails: bool,
    /// Labels whose usage listing errors.
    usage_fails: BTreeSet<String>,
    /// `(item, label)` removals that error.
    remove_fails: BTreeSet<(u64, String)>,
    /// `(item, label)` adds that error.
    add_fails: BTreeSet<(u64, String)>,
    /// After this many listings of `label`, a new open item carrying it
    /// appears (a concurrent labeller).
    appear: Option<(String, usize, u64)>,
    /// Calls (by index) answered with a rate limit.
    rate_limit_at: Option<usize>,
}

impl Fake {
    fn new(live: &[&str]) -> Self {
        let f = Fake::default();
        f.live
            .borrow_mut()
            .extend(live.iter().map(|s| s.to_string()));
        f
    }

    fn item(self, n: u64, labels: &[&str]) -> Self {
        self.items
            .borrow_mut()
            .insert(n, labels.iter().map(|s| s.to_string()).collect());
        self
    }

    fn log(&self, call: String) -> Result<(), ForgeError> {
        let mut calls = self.calls.borrow_mut();
        calls.push(call);
        if self.rate_limit_at == Some(calls.len() - 1) {
            return Err(ForgeError::RateLimited("API rate limit exceeded".into()));
        }
        Ok(())
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| !c.starts_with("live") && !c.starts_with("list"))
            .cloned()
            .collect()
    }

    fn labels_of(&self, n: u64) -> BTreeSet<String> {
        self.items.borrow().get(&n).cloned().unwrap_or_default()
    }
}

impl MigrateForge for Fake {
    fn live_labels(&self) -> Result<Vec<String>, ForgeError> {
        self.log("live".into())?;
        if self.list_fails {
            return Err(ForgeError::Other("boom".into()));
        }
        Ok(self.live.borrow().iter().cloned().collect())
    }

    fn open_page(&self, label: &str, page: u32) -> Result<Vec<Item>, ForgeError> {
        self.log(format!("list {label} {page}"))?;
        if self.usage_fails.contains(label) {
            return Err(ForgeError::Other("timed out".into()));
        }
        if page == 1 {
            if let Some((l, after, n)) = &self.appear {
                let lists = self
                    .calls
                    .borrow()
                    .iter()
                    .filter(|c| c.starts_with(&format!("list {l} 1")))
                    .count();
                if l == label && lists > *after {
                    self.items
                        .borrow_mut()
                        .entry(*n)
                        .or_default()
                        .insert(l.clone());
                }
            }
        }
        let all: Vec<Item> = self
            .items
            .borrow()
            .iter()
            .filter(|(_, ls)| ls.contains(label))
            .map(|(n, ls)| Item {
                number: *n,
                labels: ls.iter().cloned().collect(),
            })
            .collect();
        let start = (page as usize - 1) * PER_PAGE;
        Ok(all.into_iter().skip(start).take(PER_PAGE).collect())
    }

    fn add_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        self.log(format!("add #{number} {label}"))?;
        if self.add_fails.contains(&(number, label.to_string())) {
            return Err(ForgeError::Other("HTTP 500".into()));
        }
        self.items
            .borrow_mut()
            .entry(number)
            .or_default()
            .insert(label.into());
        Ok(())
    }

    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        self.log(format!("remove #{number} {label}"))?;
        if self.remove_fails.contains(&(number, label.to_string())) {
            return Err(ForgeError::Other("HTTP 502".into()));
        }
        let removed = self
            .items
            .borrow_mut()
            .get_mut(&number)
            .is_some_and(|ls| ls.remove(label));
        if removed {
            Ok(())
        } else {
            Err(ForgeError::NotFound("HTTP 404".into()))
        }
    }

    fn delete_label(&self, label: &str) -> Result<(), ForgeError> {
        self.log(format!("delete {label}"))?;
        if self.live.borrow_mut().remove(label) {
            Ok(())
        } else {
            Err(ForgeError::NotFound("HTTP 404".into()))
        }
    }
}

fn retired(name: &str, to: Option<&str>, owned: bool) -> RetiredLabel {
    RetiredLabel {
        name: name.into(),
        replaced_by: to.map(str::to_string),
        owned,
        retired_in: "#11103".into(),
    }
}

fn mapping() -> Vec<RetiredLabel> {
    vec![
        retired(STAR, Some(IMPORTANT), true),
        retired(TIER, None, true),
        retired(UNOWNED, Some(IMPORTANT), false),
    ]
}

fn go(f: &Fake, apply: bool) -> Report {
    run(f, "o/r", &mapping(), apply)
}

fn text(r: &Report) -> String {
    let mut s = String::new();
    r.render(&mut s);
    s
}

#[test]
fn a_dry_run_makes_no_forge_writes() {
    let f = Fake::new(&[STAR, TIER, UNOWNED, IMPORTANT])
        .item(1, &[STAR])
        .item(2, &[TIER])
        .item(3, &[UNOWNED]);
    let r = go(&f, false);
    assert!(f.writes().is_empty(), "dry run wrote: {:?}", f.writes());
    assert_eq!(r.summary.writes, 0);
    assert_eq!(r.exit, 3, "pending work is not 'done'");
    let out = text(&r);
    assert!(out.starts_with("MODE dry-run"), "{out}");
    assert!(out.contains("RELABEL #1 loom:operator-priority -> loom:important\n"), "{out}");
    assert!(out.contains("RELABEL #2 tier:maintenance -> none\n"), "{out}");
    assert!(out.contains("ADD-ONLY #3 consumer:legacy-prio +loom:important\n"), "{out}");
    assert!(out.contains("DELETE loom:operator-priority\n"), "{out}");
    assert_eq!(f.labels_of(1), BTreeSet::from([STAR.to_string()]));
}

#[test]
fn an_in_use_owned_label_is_added_before_it_is_removed_then_deleted() {
    let f = Fake::new(&[STAR, IMPORTANT]).item(7, &[STAR, "bug"]);
    let r = run(&f, "o/r", &[retired(STAR, Some(IMPORTANT), true)], true);
    assert_eq!(
        f.writes(),
        vec![
            format!("add #7 {IMPORTANT}"),
            format!("remove #7 {STAR}"),
            format!("delete {STAR}"),
        ]
    );
    assert_eq!(f.labels_of(7), BTreeSet::from(["bug".to_string(), IMPORTANT.to_string()]));
    assert_eq!((r.exit, r.summary.deleted, r.summary.relabeled), (0, 1, 1));
}

#[test]
fn an_item_already_carrying_the_target_gets_no_add() {
    let f = Fake::new(&[STAR, IMPORTANT]).item(7, &[STAR, IMPORTANT]);
    let r = run(&f, "o/r", &[retired(STAR, Some(IMPORTANT), true)], true);
    assert_eq!(f.writes(), vec![format!("remove #7 {STAR}"), format!("delete {STAR}")]);
    assert!(text(&r).contains("(already has loom:important)"));
}

#[test]
fn a_failed_removal_keeps_the_label_and_never_deletes() {
    let mut f = Fake::new(&[STAR, IMPORTANT]).item(7, &[STAR]);
    f.remove_fails.insert((7, STAR.into()));
    let r = run(&f, "o/r", &[retired(STAR, Some(IMPORTANT), true)], true);
    assert!(!f.writes().iter().any(|w| w.starts_with("delete")), "{:?}", f.writes());
    assert!(text(&r).contains("KEPT loom:operator-priority (open: #7)"), "{}", text(&r));
    assert_eq!(r.exit, 3);
}

#[test]
fn an_item_appearing_before_the_requery_keeps_the_label() {
    let mut f = Fake::new(&[TIER]).item(1, &[TIER]);
    f.appear = Some((TIER.into(), 1, 99));
    let r = run(&f, "o/r", &[retired(TIER, None, true)], true);
    assert!(!f.writes().iter().any(|w| w.starts_with("delete")));
    assert!(text(&r).contains("KEPT tier:maintenance (open: #99)"), "{}", text(&r));
    assert_eq!(r.exit, 3);
}

#[test]
fn a_failed_usage_read_keeps_the_label() {
    let mut f = Fake::new(&[TIER]).item(1, &[TIER]);
    f.usage_fails.insert(TIER.into());
    let r = run(&f, "o/r", &[retired(TIER, None, true)], true);
    assert!(f.writes().is_empty());
    assert!(text(&r).contains("KEPT tier:maintenance (usage lookup failed: timed out)"));
    assert_eq!(r.exit, 3);
}

#[test]
fn a_failed_add_leaves_the_old_label_in_place() {
    let mut f = Fake::new(&[STAR, IMPORTANT]).item(7, &[STAR]);
    f.add_fails.insert((7, IMPORTANT.into()));
    let r = run(&f, "o/r", &[retired(STAR, Some(IMPORTANT), true)], true);
    assert_eq!(f.writes(), vec![format!("add #7 {IMPORTANT}")], "no remove, no delete");
    assert_eq!(f.labels_of(7), BTreeSet::from([STAR.to_string()]));
    assert_eq!(r.exit, 3);
}

#[test]
fn more_than_one_page_is_fully_relabelled_before_any_delete() {
    let mut f = Fake::new(&[TIER]);
    for n in 1..=250 {
        f = f.item(n, &[TIER]);
    }
    let r = run(&f, "o/r", &[retired(TIER, None, true)], true);
    let writes = f.writes();
    let removes = writes.iter().filter(|w| w.starts_with("remove")).count();
    assert_eq!(removes, 250);
    assert_eq!(writes.last().unwrap(), &format!("delete {TIER}"));
    assert!(f.calls.borrow().contains(&format!("list {TIER} 3")), "read page 3");
    assert_eq!((r.exit, r.summary.relabeled), (0, 250));
}

#[test]
fn a_full_last_page_is_followed_by_an_empty_one() {
    let mut f = Fake::new(&[TIER]);
    for n in 1..=100 {
        f = f.item(n, &[TIER]);
    }
    let r = run(&f, "o/r", &[retired(TIER, None, true)], false);
    assert!(f.calls.borrow().contains(&format!("list {TIER} 2")));
    assert_eq!(r.summary.relabeled, 100);
}

#[test]
fn an_unowned_label_is_add_only_in_every_mode() {
    for apply in [false, true] {
        let f = Fake::new(&[UNOWNED, IMPORTANT]).item(3, &[UNOWNED]);
        let r = run(&f, "o/r", &[retired(UNOWNED, Some(IMPORTANT), false)], apply);
        assert!(
            !f.writes()
                .iter()
                .any(|w| w.starts_with("remove") || w.starts_with("delete")),
            "apply={apply}: {:?}",
            f.writes()
        );
        assert!(text(&r).contains("UNOWNED consumer:legacy-prio"), "{}", text(&r));
        if apply {
            assert_eq!(f.writes(), vec![format!("add #3 {IMPORTANT}")]);
            assert!(f.labels_of(3).contains(UNOWNED));
            assert_eq!(r.exit, 0);
        }
    }
}

#[test]
fn priority_high_is_owned_so_it_is_relabelled_then_deleted() {
    let reg = loom_daemon::label_registry::Registry::embedded();
    let entry = reg
        .retired
        .iter()
        .find(|r| r.name == "priority:high")
        .expect("priority:high is retired")
        .clone();
    let f = Fake::new(&["priority:high", IMPORTANT]).item(3, &["priority:high"]);
    let r = run(&f, "o/r", &[entry], true);
    assert_eq!(
        f.writes(),
        vec![
            format!("add #3 {IMPORTANT}"),
            "remove #3 priority:high".to_string(),
            "delete priority:high".to_string(),
        ]
    );
    assert_eq!(r.exit, 0);
}

#[test]
fn a_missing_replacement_is_skipped_and_never_created() {
    let f = Fake::new(&[STAR]).item(7, &[STAR]);
    let r = run(&f, "o/r", &[retired(STAR, Some(IMPORTANT), true)], true);
    assert!(f.writes().is_empty(), "{:?}", f.writes());
    assert!(!f.live.borrow().contains(IMPORTANT), "no label was created");
    assert!(text(&r).contains("SKIPPED loom:operator-priority (target loom:important missing"));
    assert_eq!(r.exit, 3);
}

#[test]
fn a_second_apply_makes_zero_writes_and_exits_zero() {
    let f = Fake::new(&[STAR, TIER, UNOWNED, IMPORTANT])
        .item(1, &[STAR])
        .item(2, &[TIER, STAR])
        .item(3, &[UNOWNED]);
    assert_eq!(go(&f, true).exit, 0);
    let before = f.writes().len();
    let r = go(&f, true);
    assert_eq!(f.writes().len(), before, "second run wrote: {:?}", &f.writes()[before..]);
    assert_eq!((r.exit, r.summary.writes), (0, 0));
    assert!(text(&r).contains("ABSENT loom:operator-priority"));
}

#[test]
fn two_retired_labels_sharing_a_target_add_it_once() {
    let other = "loom:operator-high-priority";
    let f = Fake::new(&[STAR, other, IMPORTANT]).item(5, &[STAR, other]);
    let map = [
        retired(STAR, Some(IMPORTANT), true),
        retired(other, Some(IMPORTANT), true),
    ];
    let _ = run(&f, "o/r", &map, false);
    let _ = run(&f, "o/r", &map, true);
    let adds = f.writes().iter().filter(|w| w.starts_with("add")).count();
    assert_eq!(adds, 1);
}

#[test]
fn an_unreadable_label_list_is_exit_four_with_no_writes() {
    let f = Fake {
        list_fails: true,
        ..Fake::new(&[])
    };
    let r = go(&f, true);
    assert_eq!(r.exit, 4);
    assert!(f.writes().is_empty());
}

#[test]
fn a_rate_limit_stops_the_run_and_a_rerun_converges() {
    let mut f = Fake::new(&[STAR, TIER, IMPORTANT])
        .item(1, &[STAR])
        .item(2, &[TIER]);
    // calls: live, list STAR 1, add #1, remove #1 (rate limited)
    f.rate_limit_at = Some(3);
    let r = go(&f, true);
    assert_eq!((r.exit, r.rate_limited), (4, true));
    assert!(text(&r).contains("STOPPED (rate limited"), "{}", text(&r));
    assert!(!f.calls.borrow().iter().any(|c| c.contains(TIER)), "nothing after the stop");
    f.rate_limit_at = None;
    let r = go(&f, true);
    assert_eq!(r.exit, 0, "{}", text(&r));
    assert!(f.live.borrow().iter().all(|l| l == IMPORTANT));
}

fn targets(names: &[&str]) -> Vec<Target> {
    names
        .iter()
        .map(|n| Target {
            repo: n.to_string(),
            root: PathBuf::from("/w").join(n),
        })
        .collect()
}

#[test]
fn a_refused_apply_exits_two_before_any_forge_call() {
    let calls = RefCell::new(0usize);
    let forge_for = |_: &str| -> Box<dyn MigrateForge> {
        *calls.borrow_mut() += 1;
        Box::new(Fake::new(&[STAR]))
    };
    let reports = run_targets(&targets(&["o/r"]), &mapping(), true, &forge_for, &|_| {
        Err("unmanaged repo".into())
    });
    assert_eq!(reports[0].exit, 2);
    assert_eq!(*calls.borrow(), 0, "no forge was even built");
}

#[test]
fn a_dry_run_is_not_gated() {
    let forge_for = |_: &str| -> Box<dyn MigrateForge> { Box::new(Fake::new(&[])) };
    let reports = run_targets(&targets(&["o/r"]), &mapping(), false, &forge_for, &|_| {
        Err("unmanaged repo".into())
    });
    assert_eq!(reports[0].exit, 0);
}

#[test]
fn a_rate_limit_stops_the_fleet_run() {
    let forge_for = |_: &str| -> Box<dyn MigrateForge> {
        Box::new(Fake {
            rate_limit_at: Some(0),
            ..Fake::new(&[])
        })
    };
    let reports =
        run_targets(&targets(&["o/a", "o/b"]), &mapping(), false, &forge_for, &|_| Ok(()));
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].exit, 4);
}

#[test]
fn workspaces_are_deduplicated_by_repo() {
    let (t, unresolved) = dedupe_targets(vec![
        (PathBuf::from("/a"), Some("o/r".into())),
        (PathBuf::from("/b"), Some("O/R".into())),
        (PathBuf::from("/c"), None),
        (PathBuf::from("/d"), Some("o/s".into())),
    ]);
    let repos: Vec<_> = t.iter().map(|t| t.repo.as_str()).collect();
    assert_eq!(repos, ["o/r", "o/s"]);
    assert_eq!(t[0].root, PathBuf::from("/a"));
    assert_eq!(unresolved, [PathBuf::from("/c")]);
}

#[test]
fn gh_helpers_encode_and_classify() {
    assert_eq!(enc("loom:operator-priority"), "loom%3Aoperator-priority");
    assert_eq!(enc("good first issue"), "good%20first%20issue");
    assert!(matches!(
        classify("HTTP 403: API rate limit exceeded".into()),
        ForgeError::RateLimited(_)
    ));
    assert!(matches!(classify("HTTP 429: slow down".into()), ForgeError::RateLimited(_)));
    assert!(matches!(classify("gh: Not Found (HTTP 404)".into()), ForgeError::NotFound(_)));
    assert!(matches!(classify("gh: command not found".into()), ForgeError::Other(_)));
    assert!(matches!(classify("HTTP 500".into()), ForgeError::Other(_)));
    let items = parse_items("[12,[\"a\",\"b:c\"]]\n\n[13,[]]\n").unwrap();
    assert_eq!(
        items[0],
        Item {
            number: 12,
            labels: vec!["a".into(), "b:c".into()]
        }
    );
    assert_eq!(items[1].number, 13);
    assert!(parse_items("12\n").is_err());
}

#[test]
fn the_embedded_registry_carries_the_retired_map() {
    // The embedded map is what a consumer repo (no labels.json) migrates by.
    let reg = loom_daemon::label_registry::Registry::embedded();
    assert!(reg.retired.iter().any(|r| r.name == STAR && r.owned));
    assert!(reg
        .retired
        .iter()
        .any(|r| r.name == "priority:high" && r.owned));
}

#[test]
fn json_events_are_tagged() {
    let f = Fake::new(&[TIER]).item(2, &[TIER]);
    let r = run(&f, "o/r", &[retired(TIER, None, true)], false);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["events"][0]["action"], "relabel");
    assert_eq!(v["events"][0]["item"], 2);
    assert_eq!(v["exit"], 3);
}
