//! Rendering the launchd plist and the `systemd --user` unit, plus the textual
//! readers and writers that operate on an already-installed one.
//!
//! Every renderer here is **pure string rendering** and safe to call on any
//! platform — `--print-plist` on a Linux host and `--print-unit` on a Mac are
//! both supported, and the retained suite depends on exactly that (it exercises
//! the launchd half read-only through `--print-plist` because the real install
//! branch is Darwin-gated with no force seam).
//!
//! The readers are deliberately **not** parsers. `extract_plist_env_keys` is an
//! awk line scan, not a plist parse, because every plist it reads is one this
//! file rendered and therefore has the exact two-line `<key>`/`<string>` shape.
//! Substituting `plutil`/an XML parser would accept shapes the shell rejected
//! and reject shapes it accepted, in a code path whose job is to decide whether
//! an operator's `LOOM_WORK_FINDER=1` survives a re-render.

use std::path::Path;

use super::envh::{is_session_scoped_env_key, xml_escape};
use super::out;
use super::unescape::{expand_awk_v, expand_printf_b};

/// `render_launchd_plist <label> <daemon_bin> <workdir> <log_path>`.
///
/// `LOOM_DAEMON_SUPERVISOR=launchd` is hardcoded rather than harvested so it
/// lands in *every* rendered plist (and so it is absent from the nohup path,
/// which renders none) — that is what lets the daemon prove it is supervised
/// before exiting for a restart (#4054).
#[must_use]
pub fn launchd_plist(
    label: &str,
    bin: &str,
    workdir: &str,
    log_path: &str,
    plist_path_value: &str,
    home: &str,
    forwarded: &[(String, String)],
) -> String {
    let mut env_entries = String::new();
    env_entries.push_str(&format!(
        "        <key>PATH</key>\\n        <string>{}</string>\\n",
        xml_escape(plist_path_value)
    ));
    env_entries.push_str(&format!(
        "        <key>HOME</key>\\n        <string>{}</string>\\n",
        xml_escape(home)
    ));
    env_entries.push_str(
        "        <key>LOOM_DAEMON_SUPERVISOR</key>\\n        <string>launchd</string>\\n",
    );

    for (key, value) in forwarded {
        if key == "LOOM_DAEMON_SUPERVISOR" {
            continue;
        }
        if is_session_scoped_env_key(key) {
            continue;
        }
        env_entries.push_str(&format!(
            "        <key>{}</key>\\n        <string>{}</string>\\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }

    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    s.push_str(&format!("    <key>Label</key>\n    <string>{}</string>\n", xml_escape(label)));
    s.push_str(&format!(
        "    <key>ProgramArguments</key>\n    <array>\n        <string>{}</string>\n    </array>\n",
        xml_escape(bin)
    ));
    s.push_str(&format!(
        "    <key>WorkingDirectory</key>\n    <string>{}</string>\n",
        xml_escape(workdir)
    ));
    s.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
    // `printf '%b'` — see `unescape.rs` for why this is not a plain push.
    s.push_str(&expand_printf_b(&env_entries));
    s.push_str("    </dict>\n");
    s.push_str("    <key>RunAtLoad</key>\n    <true/>\n");
    s.push_str(
        "    <key>KeepAlive</key>\n    <dict>\n        <key>SuccessfulExit</key>\n        <true/>\n    </dict>\n",
    );
    s.push_str("    <key>ProcessType</key>\n    <string>Background</string>\n");
    s.push_str(&format!(
        "    <key>StandardOutPath</key>\n    <string>{}</string>\n",
        xml_escape(log_path)
    ));
    s.push_str(&format!(
        "    <key>StandardErrorPath</key>\n    <string>{}</string>\n",
        xml_escape(log_path)
    ));
    s.push_str("</dict>\n</plist>\n");
    s
}

/// `render_systemd_unit <daemon_bin> <workdir> <log_path>` — the Linux mirror.
///
/// `Restart=on-success` + `KillMode=mixed` + `TimeoutStopSec=20` +
/// `SuccessExitStatus=143 130` / `RestartPreventExitStatus=143 130` are each
/// load-bearing and each has an incident behind it; the shell's comments at
/// their printf sites carry the full rationale and are not repeated here.
#[must_use]
pub fn systemd_unit(
    bin: &str,
    workdir: &str,
    log_path: &str,
    unit_path_value: &str,
    home: &str,
    forwarded: &[(String, String)],
) -> String {
    let mut env_lines = String::new();
    env_lines.push_str(&format!("Environment=PATH={unit_path_value}\\n"));
    env_lines.push_str(&format!("Environment=HOME={home}\\n"));
    env_lines.push_str("Environment=LOOM_DAEMON_SUPERVISOR=systemd\\n");

    for (key, value) in forwarded {
        if key == "LOOM_DAEMON_SUPERVISOR" {
            continue;
        }
        if is_session_scoped_env_key(key) {
            continue;
        }
        // The shell appended `$line`, i.e. the ORIGINAL `KEY=VALUE` text, not a
        // re-joined pair. They are the same string because the split kept every
        // later `=` in the value.
        env_lines.push_str(&format!("Environment={key}={value}\\n"));
    }

    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str("Description=Loom autonomous daemon (loom-daemon)\n");
    s.push_str("After=network-online.target\n");
    s.push_str("Wants=network-online.target\n");
    s.push('\n');
    s.push_str("[Service]\n");
    s.push_str("Type=simple\n");
    s.push_str(&format!("WorkingDirectory={workdir}\n"));
    s.push_str(&format!("ExecStart={bin}\n"));
    s.push_str("Restart=on-success\n");
    s.push_str("KillMode=mixed\n");
    s.push_str("TimeoutStopSec=20\n");
    s.push_str("SuccessExitStatus=143 130\n");
    s.push_str("RestartPreventExitStatus=143 130\n");
    s.push_str(&expand_printf_b(&env_lines));
    s.push_str(&format!("StandardOutput=append:{log_path}\n"));
    s.push_str(&format!("StandardError=append:{log_path}\n"));
    s.push('\n');
    s.push_str("[Install]\n");
    s.push_str("WantedBy=default.target\n");
    s
}

/// `render_watchdog_plist <label> <script> <workdir> <log_path> <interval>`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn watchdog_plist(
    label: &str,
    script: &str,
    workdir: &str,
    log_path: &str,
    interval: &str,
    plist_path_value: &str,
    home: &str,
    intent_marker: &str,
    socket_path: &str,
    pid_file: &str,
    launchd_label: &str,
) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    s.push_str(&format!("    <key>Label</key>\n    <string>{}</string>\n", xml_escape(label)));
    s.push_str(&format!(
        "    <key>ProgramArguments</key>\n    <array>\n        <string>/bin/bash</string>\n        <string>{}</string>\n    </array>\n",
        xml_escape(script)
    ));
    s.push_str(&format!(
        "    <key>WorkingDirectory</key>\n    <string>{}</string>\n",
        xml_escape(workdir)
    ));
    s.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
    s.push_str(&format!(
        "        <key>PATH</key>\n        <string>{}</string>\n",
        xml_escape(plist_path_value)
    ));
    s.push_str(&format!(
        "        <key>HOME</key>\n        <string>{}</string>\n",
        xml_escape(home)
    ));
    s.push_str(&format!(
        "        <key>LOOM_AUTONOMY_MARKER</key>\n        <string>{}</string>\n",
        xml_escape(intent_marker)
    ));
    s.push_str(&format!(
        "        <key>LOOM_SOCKET_PATH</key>\n        <string>{}</string>\n",
        xml_escape(socket_path)
    ));
    s.push_str(&format!(
        "        <key>LOOM_PID_FILE</key>\n        <string>{}</string>\n",
        xml_escape(pid_file)
    ));
    s.push_str(&format!(
        "        <key>LOOM_LAUNCHD_LABEL</key>\n        <string>{}</string>\n",
        xml_escape(launchd_label)
    ));
    s.push_str("    </dict>\n");
    s.push_str("    <key>RunAtLoad</key>\n    <true/>\n");
    s.push_str(&format!("    <key>StartInterval</key>\n    <integer>{interval}</integer>\n"));
    s.push_str("    <key>ProcessType</key>\n    <string>Background</string>\n");
    s.push_str(&format!(
        "    <key>StandardOutPath</key>\n    <string>{}</string>\n",
        xml_escape(log_path)
    ));
    s.push_str(&format!(
        "    <key>StandardErrorPath</key>\n    <string>{}</string>\n",
        xml_escape(log_path)
    ));
    s.push_str("</dict>\n</plist>\n");
    s
}

/// `render_systemd_watchdog_service <script> <workdir> <log_path>`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn systemd_watchdog_service(
    script: &str,
    workdir: &str,
    log_path: &str,
    plist_path_value: &str,
    home: &str,
    intent_marker: &str,
    socket_path: &str,
    pid_file: &str,
) -> String {
    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str("Description=Loom daemon autonomy-loss watchdog (loom-daemon-watchdog)\n");
    s.push('\n');
    s.push_str("[Service]\n");
    s.push_str("Type=oneshot\n");
    s.push_str(&format!("WorkingDirectory={workdir}\n"));
    s.push_str(&format!("ExecStart=/bin/bash {script}\n"));
    s.push_str(&format!("Environment=PATH={plist_path_value}\n"));
    s.push_str(&format!("Environment=HOME={home}\n"));
    s.push_str(&format!("Environment=LOOM_AUTONOMY_MARKER={intent_marker}\n"));
    s.push_str(&format!("Environment=LOOM_SOCKET_PATH={socket_path}\n"));
    s.push_str(&format!("Environment=LOOM_PID_FILE={pid_file}\n"));
    s.push_str("Environment=LOOM_DAEMON_LAUNCHD=0\n");
    s.push_str(&format!("StandardOutput=append:{log_path}\n"));
    s.push_str(&format!("StandardError=append:{log_path}\n"));
    s
}

/// `render_systemd_watchdog_timer <service_unit_name> <interval_secs>`.
#[must_use]
pub fn systemd_watchdog_timer(service_unit: &str, interval: &str) -> String {
    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str("Description=Loom daemon autonomy-loss watchdog timer (loom-daemon-watchdog)\n");
    s.push('\n');
    s.push_str("[Timer]\n");
    s.push_str(&format!("OnBootSec={interval}\n"));
    s.push_str(&format!("OnUnitActiveSec={interval}\n"));
    s.push_str(&format!("Unit={service_unit}\n"));
    s.push_str("Persistent=false\n");
    s.push('\n');
    s.push_str("[Install]\n");
    s.push_str("WantedBy=timers.target\n");
    s
}

// ---------------------------------------------------------------------------
// Textual readers over an already-installed plist / unit
// ---------------------------------------------------------------------------

/// `extract_plist_path_value <plist_file>` — the `<string>` on the line after
/// `<key>PATH</key>`.
///
/// The awk match is a substring test, which is why it does **not** also fire on
/// `<key>LOOM_DAEMON_PATH</key>`: that line does not contain `<key>PATH</key>`.
#[must_use]
pub fn plist_path_value(text: &str) -> Option<String> {
    let mut want = false;
    for line in text.lines() {
        if want {
            let l = line.trim_start_matches([' ', '\t']);
            let l = l.strip_prefix("<string>").unwrap_or(l);
            let l = l.trim_end_matches([' ', '\t']);
            let l = l.strip_suffix("</string>").unwrap_or(l);
            return Some(l.to_string());
        }
        if line.contains("<key>PATH</key>") {
            want = true;
        }
    }
    None
}

/// `extract_plist_env_keys <plist_file>` — every key inside the
/// `EnvironmentVariables` dict, in file order.
#[must_use]
pub fn plist_env_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut in_env = false;
    for line in text.lines() {
        if !in_env {
            if line.contains("<key>EnvironmentVariables</key>") {
                in_env = true;
            }
            continue;
        }
        if line.contains("</dict>") {
            break;
        }
        if line.contains("<key>") {
            let l = line.trim_start_matches([' ', '\t']);
            let Some(l) = l.strip_prefix("<key>") else {
                continue;
            };
            // `sub(/<\/key>.*$/, "")` removes from the FIRST `</key>` onward.
            let l = l.split("</key>").next().unwrap_or(l);
            keys.push(l.to_string());
        }
    }
    keys
}

/// `extract_plist_env_value <plist_file> <key>`.
#[must_use]
pub fn plist_env_value(text: &str, want_key: &str) -> Option<String> {
    let mut in_env = false;
    let mut found = false;
    for line in text.lines() {
        if !in_env {
            if line.contains("<key>EnvironmentVariables</key>") {
                in_env = true;
            }
            continue;
        }
        if line.contains("</dict>") {
            break;
        }
        // The awk rules are evaluated in source order: the `<string>` rule
        // comes BEFORE the `<key>` rule, so a line is only read as a value when
        // the previous `<key>` matched.
        if found && line.contains("<string>") {
            let l = line.trim_start_matches([' ', '\t']);
            let l = l.strip_prefix("<string>").unwrap_or(l);
            let l = l.trim_end_matches([' ', '\t']);
            let l = l.strip_suffix("</string>").unwrap_or(l);
            return Some(l.to_string());
        }
        if line.contains("<key>") {
            let l = line.trim_start_matches([' ', '\t']);
            let l = l.strip_prefix("<key>").unwrap_or(l);
            let l = l.split("</key>").next().unwrap_or(l);
            found = l == want_key;
        }
    }
    None
}

/// `extract_systemd_env_keys <unit_file>` —
/// `sed -n 's/^Environment=\([^=]*\)=.*/\1/p'`.
///
/// A bare `Environment=FOO` with no value does not match: the pattern requires
/// a second `=`.
#[must_use]
pub fn systemd_env_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Environment=")?;
            let eq = rest.find('=')?;
            Some(rest[..eq].to_string())
        })
        .collect()
}

/// `extract_systemd_env_value <unit_file> <key>` — the first matching line.
///
/// The shell interpolated `$want_key` into a `sed` BRE unescaped, so a key
/// containing a regex metacharacter would have matched more than itself. Every
/// key that reaches here came from an `Environment=` line whose name the
/// renderers restrict to `[A-Za-z0-9_]`, an alphabet in which a BRE and a
/// literal coincide — so this matches literally, and the divergence is confined
/// to a hand-edited unit with a metacharacter in an env-var name.
#[must_use]
pub fn systemd_env_value(text: &str, want_key: &str) -> Option<String> {
    let prefix = format!("Environment={want_key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(ToString::to_string))
}

// ---------------------------------------------------------------------------
// Carry-forward injection (#5344)
// ---------------------------------------------------------------------------

/// `inject_one_plist_env_entry <file> <key> <value>` — insert before the
/// `</dict>` that closes `EnvironmentVariables`.
#[must_use]
pub fn inject_plist_env_entry(text: &str, key: &str, value: &str) -> String {
    // `awk -v k=… -v v=…` expands escape sequences in the assignment.
    let k = expand_awk_v(&xml_escape(key));
    let v = expand_awk_v(&xml_escape(value));
    let mut out = String::new();
    let mut in_env = false;
    let mut injected = false;
    for line in text.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        if !in_env && bare.contains("<key>EnvironmentVariables</key>") {
            in_env = true;
            out.push_str(line);
            continue;
        }
        if in_env && !injected && bare.contains("</dict>") {
            out.push_str(&format!("        <key>{k}</key>\n        <string>{v}</string>\n"));
            injected = true;
        }
        out.push_str(line);
    }
    out
}

/// `inject_one_systemd_env_entry <file> <key> <value>` — append after the last
/// existing `Environment=` line, else right after `[Service]`.
///
/// When neither exists the shell's `${last_line:-0}` made the insert line 0,
/// which no record number equals, so nothing was inserted at all. Preserved.
#[must_use]
pub fn inject_systemd_env_entry(text: &str, key: &str, value: &str) -> String {
    let ins = expand_awk_v(&format!("Environment={key}={value}"));
    let lines: Vec<&str> = text.lines().collect();
    let target = lines
        .iter()
        .rposition(|l| l.starts_with("Environment="))
        .or_else(|| lines.iter().position(|l| l.starts_with("[Service]")));
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push('\n');
        if Some(i) == target {
            out.push_str(&ins);
            out.push('\n');
        }
    }
    out
}

/// `warn_dropped_env_keys <old_file> <new_file> …` — compare the env KEY sets
/// and, by default, carry the installed values forward so a re-render can only
/// widen or match (#4522 detection, #5344 merge).
///
/// `--force-env` is checked **after** the dropped set is computed, not as an
/// early return, so the merge and the warning share one definition of "what
/// would be dropped".
pub struct MergeResult {
    /// The (possibly widened) replacement text.
    pub new_text: String,
}

/// The mechanism-specific half of [`warn_dropped_env_keys`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Launchd,
    Systemd,
}

impl Mechanism {
    fn keys(self, text: &str) -> Vec<String> {
        match self {
            Mechanism::Launchd => plist_env_keys(text),
            Mechanism::Systemd => systemd_env_keys(text),
        }
    }
    fn value(self, text: &str, key: &str) -> Option<String> {
        match self {
            Mechanism::Launchd => plist_env_value(text, key),
            Mechanism::Systemd => systemd_env_value(text, key),
        }
    }
    fn inject(self, text: &str, key: &str, value: &str) -> String {
        match self {
            Mechanism::Launchd => inject_plist_env_entry(text, key, value),
            Mechanism::Systemd => inject_systemd_env_entry(text, key, value),
        }
    }
}

/// Returns the replacement text after any carry-forward merge, and emits the
/// notices to stderr.
#[must_use]
pub fn warn_dropped_env_keys(
    old_file: &Path,
    new_file_label: &str,
    new_text: &str,
    mech: Mechanism,
    force_env: bool,
) -> MergeResult {
    let mut result = MergeResult {
        new_text: new_text.to_string(),
    };
    // A missing old file is a first-ever install, not a drop.
    let Ok(old_text) = std::fs::read_to_string(old_file) else {
        return result;
    };

    let old_keys = mech.keys(&old_text);
    if old_keys.is_empty() {
        return result;
    }
    let new_keys = mech.keys(new_text);

    let mut dropped: Vec<String> = Vec::new();
    let mut purged: Vec<String> = Vec::new();
    for k in old_keys {
        if k.is_empty() {
            continue;
        }
        if new_keys.iter().any(|nk| nk == &k) {
            continue;
        }
        // #6568: an agent-session key the INSTALLED file carries is the
        // corruption this merge must NOT preserve — without this branch the
        // renderers' strip and this merge would fight forever and the poisoning
        // would be self-healing-proof.
        if is_session_scoped_env_key(&k) {
            purged.push(k);
            continue;
        }
        dropped.push(k);
    }

    if !purged.is_empty() {
        out::warn("");
        out::warn(&format!(
            "NOTICE: purging {} AGENT-SESSION env key(s) carried by the installed {} (#6568):",
            purged.len(),
            old_file.display()
        ));
        for k in &purged {
            out::warn(&format!(
                "  - {k} (per-invocation session state, never durable daemon config)"
            ));
        }
        out::warn(&format!(
            "These are deliberately NOT carried forward into {}. Their presence means a daemon",
            new_file_label
        ));
        out::warn(
            "config was once written from an agent session's environment -- see .loom/docs/daemon-reference.md.",
        );
    }

    if dropped.is_empty() {
        return result;
    }
    if force_env {
        return result;
    }

    out::warn("");
    out::warn(&format!(
        "WARNING: re-rendering {} drops {} env key(s) present in the installed {}:",
        new_file_label,
        dropped.len(),
        old_file.display()
    ));
    for k in &dropped {
        if k.starts_with("LOOM_SAFEHOUSE_") {
            out::warn(&format!(
                "  - {k} (config-tier equivalent: the \"safehouse\" block in .loom/config.json + --from-config, #4353)"
            ));
        } else {
            out::warn(&format!("  - {k}"));
        }
    }
    out::warn(
        "This usually means this invocation ran without the operator's exported env (a watchdog / automated re-render / a bare re-run from a different shell).",
    );
    out::warn(&format!(
        "Carrying the installed value(s) of the key(s) above forward into {} so this invocation does not silently narrow it. Pass --force-env to acknowledge an intentional narrowing and actually drop them instead.",
        new_file_label
    ));

    for k in &dropped {
        let Some(v) = mech.value(&old_text, k) else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        result.new_text = mech.inject(&result.new_text, k, &v);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn the_supervisor_key_is_never_duplicated() {
        let p = launchd_plist(
            "lbl",
            "/bin/d",
            "/w",
            "/l",
            "/usr/bin",
            "/home/u",
            &pairs(&[
                ("LOOM_DAEMON_SUPERVISOR", "bogus"),
                ("LOOM_WORK_FINDER", "1"),
            ]),
        );
        assert_eq!(p.matches("<key>LOOM_DAEMON_SUPERVISOR</key>").count(), 1);
        assert!(p.contains("<string>launchd</string>"));
        assert!(!p.contains("<string>bogus</string>"));
    }

    #[test]
    fn session_keys_are_stripped_from_both_renderers() {
        let forwarded = pairs(&[
            ("LOOM_SWEEP_CLAIM_OWNED", "6388"),
            ("LOOM_ROLE", "sweep-lifecycle"),
            ("LOOM_RUNTIME", "claude"),
            ("LOOM_TERMINAL_ID", "t1"),
            ("LOOM_WORK_FINDER", "1"),
        ]);
        let p = launchd_plist("l", "/b", "/w", "/l", "/p", "/h", &forwarded);
        let u = systemd_unit("/b", "/w", "/l", "/p", "/h", &forwarded);
        for k in [
            "LOOM_SWEEP_CLAIM_OWNED",
            "LOOM_ROLE",
            "LOOM_RUNTIME",
            "LOOM_TERMINAL_ID",
        ] {
            assert!(!p.contains(k), "plist leaked {k}");
            assert!(!u.contains(k), "unit leaked {k}");
        }
        assert!(p.contains("LOOM_WORK_FINDER"));
        assert!(u.contains("Environment=LOOM_WORK_FINDER=1"));
    }

    #[test]
    fn plist_path_value_ignores_a_key_that_merely_ends_in_path() {
        let text = "    <key>LOOM_DAEMON_PATH</key>\n    <string>/wrong</string>\n    <key>PATH</key>\n    <string>/right</string>\n";
        assert_eq!(plist_path_value(text).as_deref(), Some("/right"));
    }

    #[test]
    fn systemd_env_keys_needs_a_second_equals() {
        let text = "Environment=FOO\nEnvironment=BAR=1\nExecStart=/x\n";
        assert_eq!(systemd_env_keys(text), vec!["BAR".to_string()]);
    }

    #[test]
    fn a_dropped_key_is_carried_forward_into_the_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.plist");
        std::fs::write(
            &old,
            launchd_plist(
                "l",
                "/b",
                "/w",
                "/lg",
                "/p",
                "/h",
                &pairs(&[("LOOM_SAFEHOUSE_ENABLED", "1")]),
            ),
        )
        .expect("write");
        let new = launchd_plist("l", "/b", "/w", "/lg", "/p", "/h", &[]);
        let merged = warn_dropped_env_keys(&old, "/tmp/new.plist", &new, Mechanism::Launchd, false);
        assert!(merged
            .new_text
            .contains("<key>LOOM_SAFEHOUSE_ENABLED</key>"));
        assert!(merged.new_text.contains("<string>1</string>"));
    }

    #[test]
    fn force_env_actually_drops_rather_than_merely_quieting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.service");
        std::fs::write(
            &old,
            systemd_unit("/b", "/w", "/lg", "/p", "/h", &pairs(&[("LOOM_SAFEHOUSE_ENABLED", "1")])),
        )
        .expect("write");
        let new = systemd_unit("/b", "/w", "/lg", "/p", "/h", &[]);
        let merged =
            warn_dropped_env_keys(&old, "/tmp/new.service", &new, Mechanism::Systemd, true);
        assert!(!merged.new_text.contains("LOOM_SAFEHOUSE_ENABLED"));
    }

    #[test]
    fn a_session_key_in_the_installed_file_is_purged_not_carried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.service");
        // An already-poisoned unit: the 2026-08-17 shape.
        let mut poisoned = systemd_unit("/b", "/w", "/lg", "/p", "/h", &[]);
        poisoned = poisoned.replace(
            "Environment=LOOM_DAEMON_SUPERVISOR=systemd\n",
            "Environment=LOOM_DAEMON_SUPERVISOR=systemd\nEnvironment=LOOM_SWEEP_CLAIM_OWNED=6388\n",
        );
        std::fs::write(&old, &poisoned).expect("write");
        let new = systemd_unit("/b", "/w", "/lg", "/p", "/h", &[]);
        let merged =
            warn_dropped_env_keys(&old, "/tmp/new.service", &new, Mechanism::Systemd, false);
        assert!(
            !merged.new_text.contains("LOOM_SWEEP_CLAIM_OWNED"),
            "the merge must not re-inject what the renderer just stripped"
        );
    }
}
