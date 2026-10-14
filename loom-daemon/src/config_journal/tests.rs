use std::path::Path;

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use serial_test::serial;

use super::*;
use crate::fleet_sync::{ConfigPass, FleetSyncStatus, RosterPass};

const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new(enable: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SOCKET_PATH", dir.path().join("loom-daemon.sock"));
        std::env::set_var(ROOT_ENV, dir.path().join("journal"));
        std::env::set_var(LOCAL_ENV_GUARD, "");
        if enable {
            std::env::set_var(ENABLE_ENV, "1");
        } else {
            std::env::remove_var(ENABLE_ENV);
        }
        Env { dir }
    }
    fn root(&self) -> std::path::PathBuf {
        self.dir.path().join("journal")
    }
    fn records(&self) -> Vec<crate::journal::ReadRecord> {
        let s = Journal::open(self.root()).stream(STREAM).unwrap();
        s.reader(Position::START).map(Result::unwrap).collect()
    }
    fn raw(&self) -> String {
        let mut out = String::new();
        let dir = self.root().join(STREAM);
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Ok(t) = std::fs::read_to_string(e.path()) {
                    out.push_str(&t);
                }
            }
        }
        out
    }
}

// Disable the machine tier so the host's real defaults never leak in.
const LOCAL_ENV_GUARD: &str = crate::config_resolver::PRIVATE_DEFAULTS_ENV;

impl Drop for Env {
    fn drop(&mut self) {
        for k in [ENABLE_ENV, ROOT_ENV, "LOOM_SOCKET_PATH", LOCAL_ENV_GUARD] {
            std::env::remove_var(k);
        }
    }
}

fn workspace(config: &Value) -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    let p = ws.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, config.to_string()).unwrap();
    ws
}

fn status(pass: &str, error: Option<&str>) -> FleetSyncStatus {
    FleetSyncStatus {
        repo: "o/r".into(),
        reference: "main".into(),
        host: "h1".into(),
        pass: pass.into(),
        at: Utc.with_ymd_and_hms(2026, 10, 11, 1, 2, 3).unwrap(),
        interval_secs: 300,
        auto_apply: false,
        config: ConfigPass {
            commit: Some("abc123".into()),
            error: error.map(str::to_string),
            ..ConfigPass::default()
        },
        roster: RosterPass::default(),
        state: Default::default(),
        enforced: crate::fleet_state::Enforcement::Proceed,
        floor: Default::default(),
        workspaces: Default::default(),
        checkouts: Vec::new(),
        checkouts_note: None,
        repo_config: Default::default(),
    }
}

#[test]
#[serial]
fn disabled_is_a_no_op() {
    let env = Env::new(false);
    let ws = workspace(&json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}}));
    record_resolved(ws.path());
    append_rendered("local", Path::new("/x"), None, Some("c"), &[]);
    record_pending_restart(&["a".into()], 1, Utc::now());
    record_pending_cleared();
    record_sync(&status("startup", None));
    record_sync_cleared();
    assert!(!env.root().exists(), "disabled mode must not touch the disk");
}

#[test]
#[serial]
fn resolved_hash_repeats_append_nothing_and_body_is_once_per_hash() {
    let env = Env::new(true);
    let ws = workspace(&json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}}));
    record_resolved(ws.path());
    record_resolved(ws.path());
    let recs = env.records();
    assert_eq!(recs.len(), 1);
    let d = &recs[0].envelope.data;
    assert_eq!(recs[0].envelope.kind, KIND_RESOLVED);
    assert_eq!(d["effective_hash"].as_str().unwrap().len(), 12);
    assert!(d["body"].is_object());
    assert!(d["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["sha256"] == "absent"));

    // Live reload of an unrelated key: same planner hash, new tier sha, no body.
    std::fs::write(
        ws.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
        json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}, "other": 1}).to_string(),
    )
    .unwrap();
    record_resolved(ws.path());
    let recs = env.records();
    assert_eq!(recs.len(), 2);
    assert!(recs[1].envelope.data.get("body").is_none());
    assert_eq!(recs[1].envelope.data["effective_hash"], recs[0].envelope.data["effective_hash"]);

    // A planner-relevant change is a new hash and carries its body.
    std::fs::write(
        ws.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
        json!({"autonomous": {"workFinder": {"maxConcurrent": 5}}}).to_string(),
    )
    .unwrap();
    record_resolved(ws.path());
    let recs = env.records();
    assert_eq!(recs.len(), 3);
    assert!(recs[2].envelope.data["body"].is_object());
}

fn set_config(ws: &tempfile::TempDir, config: &Value) {
    std::fs::write(ws.path().join(crate::config_resolver::LEGACY_CONFIG_REL), config.to_string())
        .unwrap();
}

/// `(effective_hash, tiers_digest)` of each `config.resolved` record.
fn resolved_states(env: &Env) -> Vec<(String, String)> {
    env.records()
        .iter()
        .filter(|r| r.envelope.kind == KIND_RESOLVED)
        .map(|r| {
            let d = &r.envelope.data;
            (
                d["effective_hash"].as_str().unwrap().into(),
                d["tiers_digest"].as_str().unwrap().into(),
            )
        })
        .collect()
}

#[test]
#[serial]
fn resolved_records_a_return_to_an_earlier_hash() {
    let env = Env::new(true);
    let a = json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}});
    let b = json!({"autonomous": {"workFinder": {"maxConcurrent": 5}}});
    let ws = workspace(&a);
    record_resolved(ws.path());
    set_config(&ws, &b);
    record_resolved(ws.path());
    set_config(&ws, &a);
    record_resolved(ws.path()); // A -> B -> A: the return is a transition
    record_resolved(ws.path()); // consecutive identical state: nothing
    let states = resolved_states(&env);
    assert_eq!(states.len(), 3, "{states:?}");
    assert_eq!(states[2], states[0], "newest record must be the current state A");
    assert_ne!(states[1].0, states[0].0);
    let recs = env.records();
    assert!(recs[0].envelope.data["body"].is_object());
    assert!(recs[1].envelope.data["body"].is_object());
    assert!(recs[2].envelope.data.get("body").is_none(), "body is stored once per hash");
}

#[test]
#[serial]
fn resolved_records_a_return_to_earlier_tiers_under_one_hash() {
    let env = Env::new(true);
    let x = json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}});
    let y = json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}, "other": 1});
    let ws = workspace(&x);
    record_resolved(ws.path());
    set_config(&ws, &y);
    record_resolved(ws.path());
    set_config(&ws, &x);
    record_resolved(ws.path()); // X -> Y -> X under one hash
    record_resolved(ws.path());
    let states = resolved_states(&env);
    assert_eq!(states.len(), 3, "{states:?}");
    assert!(states.iter().all(|s| s.0 == states[0].0), "one planner hash");
    assert_ne!(states[1].1, states[0].1);
    assert_eq!(states[2], states[0], "newest record must be the current tiers X");
}

#[test]
#[serial]
fn resolved_state_is_scoped_per_workspace() {
    let env = Env::new(true);
    let cfg = json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}});
    let (w1, w2) = (workspace(&cfg), workspace(&cfg));
    record_resolved(w1.path());
    record_resolved(w2.path()); // same config, other workspace: its own state
    record_resolved(w1.path());
    record_resolved(w2.path());
    let recs = env.records();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].envelope.data["workspace"], w1.path().display().to_string());
    assert_eq!(recs[1].envelope.data["workspace"], w2.path().display().to_string());
    assert!(recs[1].envelope.data.get("body").is_none(), "hash body already stored");
}

#[test]
#[serial]
fn credentials_never_reach_the_journal() {
    let env = Env::new(true);
    let ws = workspace(&json!({
        "autonomous": {"workFinder": {"maxConcurrent": 2}},
        "forge": {"apiToken": SECRET, "note": format!("see {SECRET}")},
        "password": "hunter2-hunter2",
        "remote": "https://bot:urlsecret42@git.example.com/o/r.git",
    }));
    record_resolved(ws.path());
    record_sync(&status("timer", Some(&format!("auth failed with {SECRET}"))));
    let raw = env.raw();
    assert!(raw.contains("config.resolved") && raw.contains("fleet.sync"));
    assert!(!raw.contains(SECRET), "token leaked into the journal");
    assert!(!raw.contains("hunter2"), "password leaked into the journal");
    assert!(!raw.contains("urlsecret42"), "URL userinfo leaked into the journal");
    assert!(raw.contains(REDACTED));
    assert_eq!(
        redact_userinfo("see ssh://u:p@h/x and http://h/y"),
        "see ssh://<redacted>@h/x and http://h/y"
    );
}

#[test]
#[serial]
fn render_and_restart_lifecycle_projects_without_divergence() {
    let env = Env::new(true);
    let loom_dir = env.dir.path();
    let keys = vec!["daemon.x".to_string()];
    append_rendered(
        "machine",
        Path::new("/m/defaults.json"),
        Some(Path::new("/m/b")),
        Some("c0ffee"),
        &keys,
    );
    crate::fleet_store::pending_restart::record(keys.clone(), 4242, Utc::now()).unwrap();
    let recs = env.records();
    assert_eq!(recs[0].envelope.data["store_commit"], "c0ffee");
    assert_eq!(recs[0].envelope.data["backup"], "/m/b");
    assert_eq!(recs[1].envelope.data["daemon_pid_at_render"], 4242);
    assert!(divergence(&env.root(), loom_dir).is_clean());

    // A restarted daemon reconciles (clears) the marker: still no divergence.
    assert!(crate::fleet_store::pending_restart::reconcile(Some(9999)).is_none());
    assert_eq!(env.records().last().unwrap().envelope.data["event"], "pending_cleared");
    assert!(divergence(&env.root(), loom_dir).is_clean());

    // A legacy marker the journal does not know about is a divergence.
    crate::fleet_store::pending_restart::record(keys, 1, Utc::now()).unwrap();
    std::fs::write(
        loom_dir.join(crate::fleet_store::pending_restart::FILENAME),
        json!({"paths": ["other"], "observedPid": 1, "renderedAt": Utc::now()}).to_string(),
    )
    .unwrap();
    let report = divergence(&env.root(), loom_dir);
    assert_eq!(report.total, 1);
    assert_eq!(report.divergences[0].field, "paths");
}

#[test]
#[serial]
fn sync_records_rebuild_the_legacy_snapshot_and_dedupe_unchanged_passes() {
    let env = Env::new(true);
    crate::fleet_sync::publish(&status("startup", None));
    let mut later = status("startup", None);
    later.at = Utc.with_ymd_and_hms(2026, 10, 11, 1, 7, 3).unwrap();
    crate::fleet_sync::publish(&later); // same state, later `at`: not re-journaled
    assert_eq!(env.records().len(), 1);
    // The legacy file now carries the later `at`; that is not a divergence.
    let r = divergence(&env.root(), env.dir.path());
    assert!(r.is_clean(), "{r:?}");

    // A genuine state difference in the legacy file IS reported.
    let path = env.dir.path().join(crate::fleet_sync::STATUS_FILENAME);
    let mut legacy: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    legacy["pass"] = json!("manual");
    std::fs::write(&path, legacy.to_string()).unwrap();
    let r = divergence(&env.root(), env.dir.path());
    assert_eq!(r.total, 1, "{r:?}");
    assert_eq!(r.divergences[0].field, "pass");

    crate::fleet_sync::publish(&status("timer", Some("boom")));
    let recs = env.records();
    assert_eq!(recs.len(), 2);
    let st = &recs[1].envelope.data["status"];
    assert_eq!(st["pass"], "timer");
    assert_eq!(st["config"]["error"], "boom");
    assert_eq!(st["repo"], "o/r");
    assert!(divergence(&env.root(), env.dir.path()).is_clean());

    crate::fleet_sync::clear_status();
    assert_eq!(env.records().last().unwrap().envelope.data["event"], "cleared");
    assert!(divergence(&env.root(), env.dir.path()).is_clean());
}

#[test]
#[serial]
fn shadow_failure_never_fails_the_legacy_write() {
    let env = Env::new(true);
    // The journal root is a regular file: every shadow append fails.
    std::fs::write(env.root(), "not a directory").unwrap();
    crate::fleet_store::pending_restart::record(vec!["k".into()], 7, Utc::now()).unwrap();
    assert!(crate::fleet_store::pending_restart::probe().is_some());
    crate::fleet_sync::publish(&status("startup", None));
    assert!(crate::fleet_sync::probe_status().is_some());
}

#[test]
#[serial]
fn torn_tail_and_newer_schema_do_not_break_projection_or_appends() {
    let env = Env::new(true);
    crate::fleet_store::pending_restart::record(vec!["k".into()], 7, Utc::now()).unwrap();
    // A record from a future writer: skipped by the projection.
    let s = Journal::open(env.root()).stream(STREAM).unwrap();
    s.append(KIND_RENDERED, None, json!({"schema": 99, "event": "pending_cleared"}))
        .unwrap();
    let report = divergence(&env.root(), env.dir.path());
    assert!(report.is_clean());
    assert_eq!(report.skipped_newer_schema, 1);

    // Simulate a crash mid-append: an unterminated line at the tail.
    let seg = std::fs::read_dir(env.root().join(STREAM))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .unwrap();
    let mut text = std::fs::read_to_string(&seg).unwrap();
    text.push_str("{\"v\":1,\"stream\":\"conf");
    std::fs::write(&seg, text).unwrap();
    assert!(divergence(&env.root(), env.dir.path()).is_clean());
    crate::fleet_store::pending_restart::clear(); // next writer repairs the tail
    let report = crate::journal::verify(&env.root(), &crate::journal::VerifyOptions::default());
    assert!(report.ok, "{report:?}");
}

#[test]
#[serial]
fn divergence_report_is_bounded() {
    let env = Env::new(true);
    let many: Map<String, Value> = (0..200).map(|i| (format!("f{i}"), json!(i))).collect();
    let s = Journal::open(env.root()).stream(STREAM).unwrap();
    s.append(
        KIND_SYNC,
        None,
        json!({"schema": 1, "event": "published", "status": Value::Object(many)}),
    )
    .unwrap();
    std::fs::write(env.dir.path().join(crate::fleet_sync::STATUS_FILENAME), "{\"z\": 1}").unwrap();
    let r = divergence(&env.root(), env.dir.path());
    assert_eq!(r.divergences.len(), MAX_DIVERGENCES);
    assert!(r.truncated && r.total > MAX_DIVERGENCES);
}
