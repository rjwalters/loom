//! Source scan: nothing writes the daemon binary in place (#10983).
//!
//! Every writer of the installed `loom-daemon` must publish by rename, through
//! [`super::stage`] and [`super::publish`]. This scan fails CI when a new
//! in-place writer appears in production shell or in this module tree.
//!
//! # The writers, all of them
//!
//! | Writer | How it publishes |
//! |---|---|
//! | `daemon_update::provision::install_to` (the `LOOM_DAEMON_BIN` override) | `stage` + `publish`, in process |
//! | `provision_machine_daemon` in `scripts/install/provision-daemon.sh` (the machine-level path: `daemon-update`, `install.sh`) | `loom-daemon install-binary stage` / `publish` |
//!
//! Lines the detector matches that are NOT writers of the installed daemon
//! are listed, with the reason, in [`SHELL_ALLOWED`] and [`RUST_ALLOWED`]. A
//! new match needs either a fix or an entry there that a reviewer can read.
//!
//! # What the detector sees
//!
//! Shell: a write command (`install`, `cp`, `mv`, `ln`, `rsync`, `ditto`,
//! `dd of=`, `tee`, `truncate`) or a `>` redirect whose DESTINATION is a
//! daemon-binary path. A destination is recognised by spelling: `$dest_bin`,
//! `$PROVISIONED_DAEMON_BIN`, `$PROVISION_TARGET`, `$LOOM_DAEMON_BIN`, or any
//! word ending in `/loom-daemon`. A path held in a variable under another
//! name is not seen; this is a ratchet on the known spellings, not a proof.
//!
//! Rust (`daemon_update/` only, the module that owns the destination): any
//! `File::create`, `fs::write`, `fs::copy`, `hard_link` or `.truncate(true)`
//! outside test code.

use std::path::{Path, PathBuf};

use regex::Regex;

/// Production shell that is scanned: every `*.sh` under these, minus tests.
const SHELL_ROOTS: &[&str] = &["scripts", "defaults/scripts", "install.sh"];

/// `(file, text the line contains, why it is not an in-place write of the
/// installed daemon)`.
const SHELL_ALLOWED: &[(&str, &str, &str)] = &[
    (
        "install.sh",
        r#"cp -f "$dest_bin" "$release_dir/loom-daemon""#,
        "reads the installed binary; the destination is the build-output cache under target/release, which nothing executes as the daemon",
    ),
    (
        "scripts/install/provision-dispatcher.sh",
        r#"if install -m 755 "$src" "$dest_bin""#,
        "`$dest_bin` here is the `loom` dispatcher script (~/.local/bin/loom), not the daemon binary",
    ),
    (
        "scripts/install/provision-dispatcher.sh",
        r#"cp -f "$src" "$dest_bin""#,
        "`$dest_bin` here is the `loom` dispatcher script (~/.local/bin/loom), not the daemon binary",
    ),
];

/// `(file under daemon_update/, text the line contains, why it is allowed)`.
const RUST_ALLOWED: &[(&str, &str, &str)] = &[(
    "rebuild.rs",
    "std::fs::File::create(&log)",
    "the cargo build log, a scratch file",
)];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn collect(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    if dir.is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

fn is_test_path(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.split('/').any(|part| part == "tests")
        || name.starts_with("test-")
        || name.starts_with("test_")
        || name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name == "writer_scan.rs"
}

/// A daemon-binary destination, by spelling.
const DEST: &str = r#"["']?(?:\$\{?(?:dest_bin|PROVISIONED_DAEMON_BIN|PROVISION_TARGET|LOOM_DAEMON_BIN)(?::-[^}]*)?\}?|[^\s"';&|]*/loom-daemon)["']?"#;

fn shell_detectors() -> Vec<Regex> {
    // A write command whose LAST argument is the destination.
    let command = format!(
        r"(?:^|[;&|({{!]|\b(?:if|then|do|else|sudo)\s)\s*(?:install|cp|mv|ln|rsync|ditto|tee|truncate)\s[^;&|]*?\s{DEST}\s*(?:\d?>\S*\s*)*(?:$|[;&|)}}\\])"
    );
    // Not `2>`, `&>`, `<>`, or the `->` / `=>` arrows of a message.
    let redirect = format!(r"(?:^|[^<>&\d=-])>>?\s*{DEST}(?:\s|$|[;&|)}}])");
    let dd = format!(r"\bof={DEST}");
    [command, redirect, dd]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
}

/// Is this shell line a write onto a daemon-binary destination?
fn shell_line_writes_the_daemon(detectors: &[Regex], line: &str) -> bool {
    let code = line.trim_start();
    !code.starts_with('#') && detectors.iter().any(|re| re.is_match(code))
}

fn allowed(table: &[(&str, &str, &str)], file: &str, line: &str) -> bool {
    table
        .iter()
        .any(|(f, text, _why)| file.ends_with(f) && line.contains(text))
}

/// The detector recognises the writes this issue removed, and does not fire
/// on the lines that replaced them. Without this the scan below could rot
/// into matching nothing and stay green.
#[test]
fn the_shell_detector_recognises_in_place_writes() {
    let d = shell_detectors();
    for bad in [
        r#"  if install -m 755 "$src_bin" "$dest_bin" 2>/dev/null || \"#,
        r#"     { cp -f "$src_bin" "$dest_bin" 2>/dev/null && chmod 755 "$dest_bin" 2>/dev/null; }; then"#,
        r#"        if cp -f "$preupdate_backup" "$dest_bin" 2>/dev/null && chmod 755 "$dest_bin" 2>/dev/null; then"#,
        r#"cp "$NEW_BIN" "$HOME/.local/bin/loom-daemon""#,
        r#"install -m 0755 target/release/loom-daemon "${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}/loom-daemon""#,
        r#"mv -f "$tmp" "${dest_bin}""#,
        r#"cat "$src" > "$dest_bin""#,
        r#"curl -fsSL "$url" >"$PROVISION_TARGET""#,
        r#"ln -sf "$built" "$LOOM_DAEMON_BIN""#,
        r#"dd if="$src" of="$dest_dir/loom-daemon" bs=1m"#,
        r#"sudo cp new /usr/local/bin/loom-daemon"#,
        r#"    if install -m 755 "$src" "$dest_bin" 2>/dev/null \"#,
    ] {
        assert!(shell_line_writes_the_daemon(&d, bad), "not detected: {bad}");
    }
    for fine in [
        r#"  if ! "$helper" install-binary publish "$staged" "$dest_bin"; then"#,
        r#"    stage_out="$("$helper" install-binary stage "$src_bin" "$dest_bin" 2>&1)" || continue"#,
        r#"  if mv -f "$bad_bin" "$quarantine_path" 2>/dev/null; then"#,
        r#"  # cp -f "$src_bin" "$dest_bin" used to live here"#,
        r#"  dest_ver=$("$dest_bin" --version 2>/dev/null || echo "unknown")"#,
        r#"  cp "$bin" "$tmp" 2>/dev/null || {"#,
        r#"  "${LOOM_DAEMON_BIN:-loom-daemon}" forge trusted-comments > "$out""#,
        r#"cp "$DAEMON_BIN" "$TARGET_DIR/release/loom-daemon-aarch64-apple-darwin""#,
        r#"  _pmd_warn "set LOOM_DAEMON_BIN=$src_bin in the consumer env to run the daemon""#,
        r#"  [[ -x "$dest_bin" ]] && chmod 755 "$other""#,
        r#"  _pdisp_ok "installed loom dispatcher -> $dest_bin""#,
    ] {
        assert!(!shell_line_writes_the_daemon(&d, fine), "false positive: {fine}");
    }
}

/// Acceptance 2, the shell half: no production script writes the daemon
/// binary in place.
#[test]
fn no_production_shell_writes_the_daemon_binary_in_place() {
    let root = repo_root();
    let detectors = shell_detectors();
    let mut files = Vec::new();
    for r in SHELL_ROOTS {
        collect(&root.join(r), "sh", &mut files);
    }
    let mut scanned = 0;
    let mut found = Vec::new();
    let mut used = vec![false; SHELL_ALLOWED.len()];
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&rel) {
            continue;
        }
        scanned += 1;
        let text = std::fs::read_to_string(file).unwrap_or_default();
        for (n, line) in text.lines().enumerate() {
            if !shell_line_writes_the_daemon(&detectors, line) {
                continue;
            }
            match SHELL_ALLOWED
                .iter()
                .position(|(f, t, _)| rel.ends_with(f) && line.contains(t))
            {
                Some(i) => used[i] = true,
                None => found.push(format!("{rel}:{}: {}", n + 1, line.trim())),
            }
        }
    }
    assert!(scanned > 50, "the scan found only {scanned} scripts; is the repo root wrong?");
    assert!(
        found.is_empty(),
        "in-place write(s) of the daemon binary. Publish through `loom-daemon install-binary stage|publish` (daemon_update::provision), or add the line to SHELL_ALLOWED with the reason it is not one:\n{}",
        found.join("\n")
    );
    for (i, hit) in used.iter().enumerate() {
        assert!(
            *hit,
            "SHELL_ALLOWED entry no longer matches anything, remove it: {:?}",
            SHELL_ALLOWED[i]
        );
    }
}

/// The machine-level script stages and publishes through the helper, and no
/// longer keeps its backup in `/tmp`.
#[test]
fn the_provision_script_publishes_through_the_helper() {
    let text =
        std::fs::read_to_string(repo_root().join("scripts/install/provision-daemon.sh")).unwrap();
    let code: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(code.contains(r#"install-binary stage "$src_bin" "$dest_bin""#));
    assert!(code.contains(r#"install-binary publish "$staged" "$dest_bin""#));
    assert!(!code.contains("loom-daemon-preupdate"), "the /tmp backup is back");
    let stage = code.find("install-binary stage").unwrap();
    let load = code
        .find(r#"_pmd_verify_binary_loadable "$staged""#)
        .unwrap();
    let sign = code.find(r#"sign_daemon_binary "$staged""#).unwrap();
    let publish = code.find("install-binary publish").unwrap();
    assert!(
        stage < load && load < sign && sign < publish,
        "the loadability check and signing must run on the staged file, before the publish"
    );
}

/// Acceptance 2, the Rust half: nothing in `daemon_update/` creates,
/// truncates or copies over a file except the allowed scratch writes. The
/// staged file is created with `create_new` and everything else is a rename.
#[test]
fn no_daemon_update_code_writes_a_file_in_place() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon_update");
    let forbidden =
        Regex::new(r"File::create\(|fs::write\(|fs::copy\(|hard_link\(|\.truncate\(true\)")
            .unwrap();
    // An inline `#[cfg(test)] mod … {` and everything after it is test code.
    let inline_tests = Regex::new(r"#\[cfg\(test\)\](?:\s*#\[[^\]]*\])*\s*mod\s+\w+\s*\{").unwrap();
    let mut files = Vec::new();
    collect(&dir, "rs", &mut files);
    let mut scanned = 0;
    let mut found = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&dir)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&rel) {
            continue;
        }
        scanned += 1;
        let text = std::fs::read_to_string(file).unwrap();
        let production = inline_tests
            .find(&text)
            .map_or(text.as_str(), |m| &text[..m.start()]);
        for (n, line) in production.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !forbidden.is_match(code) {
                continue;
            }
            if !allowed(RUST_ALLOWED, &rel, line) {
                found.push(format!("daemon_update/{rel}:{}: {code}", n + 1));
            }
        }
    }
    assert!(scanned >= 15, "the scan found only {scanned} files");
    assert!(
        found.is_empty(),
        "file write(s) in daemon_update that are not a create_new + rename. Use provision::stage/publish, or add the line to RUST_ALLOWED with the reason:\n{}",
        found.join("\n")
    );

    let provision = std::fs::read_to_string(dir.join("provision.rs")).unwrap();
    assert!(provision.contains("create_new(true)"), "staging must refuse an existing file");
    assert!(
        provision.contains("std::fs::rename(staged, dest)"),
        "the publish must be a rename"
    );
}
