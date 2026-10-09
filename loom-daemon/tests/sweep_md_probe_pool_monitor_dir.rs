//! EXECUTABLE doc-lint (issue #8849): the `/loom:sweep` Stage -1 PROBE_POOL
//! block resolves the llm-monitor (formerly claude-monitor) directory with the
//! same precedence as `loom-daemon`'s `tokens_pool::monitor_dir` resolver:
//!
//! `LOOM_LLM_MONITOR_DIR` → `LOOM_CLAUDE_MONITOR_DIR` → `~/.llm-monitor` (if a
//! directory) → `~/.claude-monitor`.
//!
//! Kept separate from `sweep_md_stage_minus_one_doc_lint.rs` because that file
//! sits near the file-size ratchet threshold. Runs the real extracted block
//! with real `bash` against synthetic layouts in a temp dir with a fake `$HOME`;
//! both override variables are always removed from the inherited environment
//! first so the runner's own values can never leak in.

use std::fs;
use std::path::Path;
use std::process::Command;

const BACKEND_DETECTION_MD: &str = "../defaults/.claude/commands/loom/sweep-backend-detection.md";
const ANCHOR: &str = "#### PROBE_POOL — does a multi-account token pool exist?";

/// The first ```bash fence after the PROBE_POOL heading.
fn probe_pool_block() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(BACKEND_DETECTION_MD);
    let content = fs::read_to_string(&path).expect("read sweep-backend-detection.md");
    let after = &content[content.find(ANCHOR).expect("PROBE_POOL heading present")..];
    let start = after
        .find("```bash\n")
        .expect("PROBE_POOL has a bash fence")
        + "```bash\n".len();
    let body = &after[start..];
    body[..body.find("\n```").expect("bash fence is closed")].to_string()
}

fn write_keys(dir: &Path, count: usize) {
    fs::create_dir_all(dir).unwrap();
    let body: String = (0..count)
        .map(|i| format!("ACCOUNT_KEY_{i}=sk-test-{i}\n"))
        .collect();
    fs::write(dir.join("accounts.env"), body).unwrap();
}

/// Runs the block in `root` (fake `$HOME` = `root/home`) with `env` applied on
/// top of an environment scrubbed of every pool-affecting variable. Returns
/// `(MONITOR_DIR, MONITOR_KEY_COUNT)`.
fn run(root: &Path, env: &[(&str, String)]) -> (String, String) {
    let script = format!(
        "{}\necho \"RESULT_MONITOR_DIR=$MONITOR_DIR\"\necho \"RESULT_MONITOR_KEY_COUNT=$MONITOR_KEY_COUNT\"\n",
        probe_pool_block()
    );
    let script_path = root.join("__probe_pool.sh");
    fs::write(&script_path, script).unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path)
        .current_dir(root)
        .env("HOME", root.join("home"));
    for var in [
        "LOOM_LLM_MONITOR_DIR",
        "LOOM_CLAUDE_MONITOR_DIR",
        "LOOM_ACCOUNTS_ENV",
    ] {
        cmd.env_remove(var);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn bash");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "PROBE_POOL block must never abort; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let grab = |key: &str| {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
            .unwrap_or_else(|| panic!("{key} not printed; stdout:\n{stdout}"))
    };
    (grab("RESULT_MONITOR_DIR"), grab("RESULT_MONITOR_KEY_COUNT"))
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

#[test]
fn llm_override_beats_legacy_override_and_both_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("home/.llm-monitor"), 1);
    write_keys(&root.join("home/.claude-monitor"), 1);
    write_keys(&root.join("legacy"), 1);
    write_keys(&root.join("new"), 2);
    let env = [
        ("LOOM_LLM_MONITOR_DIR", s(&root.join("new"))),
        ("LOOM_CLAUDE_MONITOR_DIR", s(&root.join("legacy"))),
    ];
    assert_eq!(run(root, &env), (s(&root.join("new")), "2".into()));
}

#[test]
fn legacy_override_still_honored_over_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("home/.llm-monitor"), 1);
    write_keys(&root.join("legacy"), 2);
    let env = [("LOOM_CLAUDE_MONITOR_DIR", s(&root.join("legacy")))];
    assert_eq!(run(root, &env), (s(&root.join("legacy")), "2".into()));
}

#[test]
fn blank_llm_override_falls_through_to_legacy_override() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("legacy"), 2);
    let env = [
        ("LOOM_LLM_MONITOR_DIR", String::new()),
        ("LOOM_CLAUDE_MONITOR_DIR", s(&root.join("legacy"))),
    ];
    assert_eq!(run(root, &env), (s(&root.join("legacy")), "2".into()));
}

#[test]
fn nonexistent_explicit_override_is_authoritative() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("home/.llm-monitor"), 2);
    let absent = root.join("absent");
    let env = [("LOOM_LLM_MONITOR_DIR", s(&absent))];
    assert_eq!(run(root, &env), (s(&absent), "0".into()));
}

#[test]
fn new_default_dir_preferred_without_per_file_fallback() {
    // Both defaults exist; the (empty) ~/.llm-monitor wins and the keys left
    // in ~/.claude-monitor are NOT mixed in.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("home/.llm-monitor")).unwrap();
    write_keys(&root.join("home/.claude-monitor"), 2);
    assert_eq!(run(root, &[]), (s(&root.join("home/.llm-monitor")), "0".into()));
}

#[test]
fn legacy_only_host_without_symlink_reads_claude_monitor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("home/.claude-monitor"), 2);
    assert!(!root.join("home/.llm-monitor").exists());
    assert_eq!(run(root, &[]), (s(&root.join("home/.claude-monitor")), "2".into()));
}

#[test]
fn regular_file_at_new_default_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_keys(&root.join("home/.claude-monitor"), 2);
    fs::write(root.join("home/.llm-monitor"), "not a dir").unwrap();
    assert_eq!(run(root, &[]), (s(&root.join("home/.claude-monitor")), "2".into()));
}

#[test]
fn neither_default_resolves_to_legacy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("home")).unwrap();
    assert_eq!(run(root, &[]), (s(&root.join("home/.claude-monitor")), "0".into()));
}
