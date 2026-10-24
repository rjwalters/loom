//! The #11951 divergence class's replay: the systemd unit stopped carrying
//! the dispatcher's autonomy env on purpose. See
//! `Divergence::SystemdAutonomyEnv11951` in the parent for the class itself.

use super::Answer;

/// The keys #11951 removed from the unit, written out literally rather than
/// imported from the renderer, so a later change cannot ride in under this
/// class.
const UNIT_AUTONOMY_KEYS_11951: [&str; 4] = [
    "LOOM_WORK_FINDER",
    "LOOM_MAIN_HEALTH_GATE",
    "LOOM_FORCE_SCOPE",
    "LOOM_GUARD_DECISION_LOG",
];

/// The shell's answer with exactly the #11951 edit applied: a unit document
/// loses its autonomy `Environment=` lines, and the carry-forward warning
/// loses those keys (the whole warning when none are left). A plist document
/// is returned unchanged.
pub(super) fn replay_11951(shell: &Answer) -> Answer {
    if !shell.stdout.contains("[Service]") {
        return shell.clone();
    }
    let is_autonomy_line = |l: &str| {
        UNIT_AUTONOMY_KEYS_11951
            .iter()
            .any(|k| l.starts_with(&format!("Environment={k}=")))
    };
    let stdout: String = shell
        .stdout
        .split_inclusive('\n')
        .filter(|l| !is_autonomy_line(l))
        .collect();
    Answer {
        stdout,
        stderr: replay_11951_dropped_warning(&shell.stderr),
        rc: shell.rc,
    }
}

/// The dropped-key warning with the autonomy keys taken out of its list and its
/// count, or removed whole (with its leading blank line and its two trailing
/// lines) when they were all it listed.
fn replay_11951_dropped_warning(stderr: &str) -> String {
    static HEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let head = HEAD.get_or_init(|| {
        regex::Regex::new(
            r"^WARNING: re-rendering (.*) drops (\d+) env key\(s\) present in the installed (.*):$",
        )
        .expect("static warning pattern")
    });
    let lines: Vec<&str> = stderr.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(caps) = head.captures(lines[i]) else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut j = i + 1;
        let mut kept: Vec<&str> = Vec::new();
        while j < lines.len() && lines[j].starts_with("  - ") {
            let key = &lines[j]["  - ".len()..];
            if !UNIT_AUTONOMY_KEYS_11951.contains(&key) {
                kept.push(lines[j]);
            }
            j += 1;
        }
        if kept.is_empty() {
            // The blank line before the header, and the two explanation lines.
            if out.last().is_some_and(String::is_empty) {
                out.pop();
            }
            i = (j + 2).min(lines.len());
            continue;
        }
        out.push(format!(
            "WARNING: re-rendering {} drops {} env key(s) present in the installed {}:",
            &caps[1],
            kept.len(),
            &caps[3]
        ));
        out.extend(kept.iter().map(ToString::to_string));
        i = j;
    }
    out.join("\n")
}

/// The port's stderr without its `(#11951)` advisory blocks: a blank line, a
/// `NOTICE (#11951): ` or `WARNING (#11951): ` header, and the two-space
/// indented lines that follow it. Nothing else is removed.
pub(super) fn strip_11951_blocks(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.split('\n').collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("NOTICE (#11951): ") || l.starts_with("WARNING (#11951): ") {
            if out.last().is_some_and(|p| p.is_empty()) {
                out.pop();
            }
            i += 1;
            while i < lines.len() && lines[i].starts_with("  ") {
                i += 1;
            }
            continue;
        }
        out.push(l);
        i += 1;
    }
    out.join("\n")
}
