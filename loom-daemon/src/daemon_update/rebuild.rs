//! The source-build path: find `cargo`, rebuild, locate what cargo actually
//! produced (#6160), refuse to provision a binary that cannot export OTLP
//! (#10700), and refuse to provision a binary stamped with the wrong commit
//! (#4053).
//!
//! Locating the artifact from cargo's OWN build output rather than probing two
//! hardcoded `target/release/` paths is the whole point of the `#6160` shape.
//! The old probe was a SILENT no-op on any host where cargo's output directory
//! is redirected (`CARGO_TARGET_DIR`, or `~/.cargo/config.toml`'s
//! `build.target-dir`, e.g. after an ENOSPC-driven move off the internal
//! volume) — the build fully succeeded, this script never found the binary it
//! built, never provisioned it, and left the fleet host on its stale binary
//! while reporting a rebuild. This fleet runs exactly that redirect.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::out;
use super::util;

/// `cargo` was not found anywhere this script knows to look.
pub struct CargoMissing;

/// Make `cargo` resolvable, extending `$PATH` the same three ways the script
/// did, in the same order.
///
/// Non-interactive SSH sessions (the fleet remote-update path, #4695) do not
/// source a login shell's profile, so a rustup-installed cargo at the default
/// `~/.cargo/bin` is invisible to `command -v cargo` even though it IS
/// installed.
pub fn ensure_cargo_on_path() -> Result<(), CargoMissing> {
    if util::have("cargo") {
        return Ok(());
    }

    // The shell `source`d `~/.cargo/env`, rustup's canonical PATH-setup
    // snippet. That file's entire body is a guarded
    // `export PATH="$HOME/.cargo/bin:$PATH"`, so its effect is applied
    // directly here — a port cannot `source` a shell script, and re-execing a
    // shell to do it would change what `$PATH` the rebuild below inherits.
    let cargo_bin = util::home().join(".cargo/bin");
    if util::home().join(".cargo/env").is_file() || util::is_executable(&cargo_bin.join("cargo")) {
        prepend_path(&cargo_bin);
    }
    if util::have("cargo") {
        return Ok(());
    }

    // Finally the FULL shared canonical PATH superset (#4831 — the same set
    // `resolve_plist_path()` renders and `fleet add-worker`'s provisioning
    // uses), in case cargo came from Homebrew or another non-rustup path.
    let canonical = crate::daemon_start::paths::canonical_daemon_path();
    if !canonical.is_empty() {
        let current = std::env::var("PATH").unwrap_or_default();
        // SAFETY: single-threaded at this point in the run; the shell's own
        // `export PATH=` had exactly this scope.
        unsafe {
            std::env::set_var("PATH", format!("{canonical}:{current}"));
        }
    }
    if util::have("cargo") {
        return Ok(());
    }

    out::err("cargo not found on PATH (checked $HOME/.cargo/bin and the shared canonical PATH too, see lib/canonical-daemon-path.sh) — cannot rebuild loom-daemon. Install Rust via rustup: https://rustup.rs");
    Err(CargoMissing)
}

fn prepend_path(dir: &Path) {
    let current = std::env::var("PATH").unwrap_or_default();
    let dir_s = dir.display().to_string();
    // `~/.cargo/env`'s own `case ":${PATH}:" in *:"$dir":*` dedupe guard.
    if format!(":{current}:").contains(&format!(":{dir_s}:")) {
        return;
    }
    // SAFETY: single-threaded at this point in the run.
    unsafe {
        std::env::set_var("PATH", format!("{dir_s}:{current}"));
    }
}

/// The freshly-built binary and its embedded commit.
pub struct Built {
    pub bin: PathBuf,
    pub commit: String,
}

/// Why the rebuild did not produce a usable binary.
pub enum BuildFailure {
    /// `cargo build --release` itself failed (exit 1).
    Compile,
    /// The build reported success but no executable could be located (exit 1,
    /// #6160) — a build that reports success must NEVER be silently treated as
    /// a no-op success.
    ArtifactNotFound,
    /// The built binary failed its `telemetry-capabilities --require-otlp`
    /// probe (exit 1, #10700): it cannot export OTLP, so installing it would
    /// silently drop every configured `otlp` sink.
    MissingOtlp,
}

/// The `cargo build` arguments the source path runs in `daemon_dir`.
///
/// `--features otlp` is passed whenever the source's own manifest declares an
/// `otlp` feature (#10700). On current source that feature is a no-op alias
/// (the exporter is compiled unconditionally), but a roll to an older tag
/// between #4879 and #10700 still gates the exporter behind it — and a tag
/// that predates #4879 has no such feature, where cargo would hard-error on
/// the unknown name, so the flag is omitted (with a WARN from [`rebuild`]).
#[must_use]
pub fn build_args(daemon_dir: &Path) -> Vec<&'static str> {
    let mut args = vec!["build", "--release"];
    if manifest_declares_otlp(daemon_dir) {
        args.extend(["--features", "otlp"]);
    }
    args.push("--message-format=json-render-diagnostics");
    args
}

fn manifest_declares_otlp(daemon_dir: &Path) -> bool {
    std::fs::read_to_string(daemon_dir.join("Cargo.toml"))
        .is_ok_and(|text| declares_otlp_feature(&text))
}

/// Does this `Cargo.toml` text declare an `otlp` key in its `[features]`
/// table? A line scan, not a TOML parse: the only shapes this crate has ever
/// written are `otlp = [...]` lines directly under `[features]`.
fn declares_otlp_feature(manifest: &str) -> bool {
    let mut in_features = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_features = line == "[features]";
        } else if in_features {
            if let Some(rest) = line.strip_prefix("otlp") {
                if rest.trim_start().starts_with('=') {
                    return true;
                }
            }
        }
    }
    false
}

/// What the post-build OTLP probe's exit status means for the swap.
#[derive(Debug, PartialEq, Eq)]
pub enum OtlpProbe {
    /// Exit 0: the binary exports OTLP.
    Proceed,
    /// Exit 2: clap's usage error — a binary too old to have
    /// `telemetry-capabilities` (it predates #8535). Its OTLP support then
    /// rests on `--features otlp` having been passed; warn and proceed.
    TooOldToAsk,
    /// Any other outcome (exit 1 = "built without the otlp feature", a signal,
    /// or a binary that would not run): refuse the swap.
    Refuse,
}

/// Classify the probe's exit code (`None` = killed by a signal, or never ran).
#[must_use]
pub fn classify_otlp_probe(code: Option<i32>) -> OtlpProbe {
    match code {
        Some(0) => OtlpProbe::Proceed,
        Some(2) => OtlpProbe::TooOldToAsk,
        _ => OtlpProbe::Refuse,
    }
}

/// Run `<new_bin> telemetry-capabilities --require-otlp` and decide (#10700).
fn probe_otlp(new_bin: &Path) -> Result<(), BuildFailure> {
    let output = Command::new(new_bin)
        .args(["telemetry-capabilities", "--require-otlp"])
        .stdin(Stdio::null())
        .output();
    let (code, detail) = match &output {
        Ok(o) => (
            o.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout).trim(),
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Err(e) => (None, e.to_string()),
    };
    match classify_otlp_probe(code) {
        OtlpProbe::Proceed => {
            out::ok("OTLP probe: the freshly-built binary exports OTLP.");
            Ok(())
        }
        OtlpProbe::TooOldToAsk => {
            out::warn("OTLP probe: the freshly-built binary has no 'telemetry-capabilities' subcommand (it predates #8535) — proceeding on the strength of the build's --features otlp.");
            Ok(())
        }
        OtlpProbe::Refuse => {
            out::err(&format!(
                "OTLP probe FAILED: '{} telemetry-capabilities --require-otlp' exited {} ({detail}).",
                new_bin.display(),
                code.map_or_else(|| "without a status".to_string(), |c| c.to_string())
            ));
            out::err("Refusing to provision a loom-daemon that cannot export OTLP (#10700) — the running daemon (if any) was left untouched.");
            Err(BuildFailure::MissingOtlp)
        }
    }
}

/// `cargo build --release --features otlp --message-format=json-render-diagnostics`
/// (see [`build_args`]), then locate the executable it reported and probe it
/// for OTLP export (#10700).
///
/// `--message-format=json-render-diagnostics` keeps rustc's human-readable
/// errors and warnings on stderr — unchanged from a plain `cargo build`, still
/// visible to the operator — while emitting cargo's structured build JSON,
/// including the artifact's REAL on-disk path, to stdout. That is exact and
/// survives `CARGO_TARGET_DIR`, `--target-dir`, workspace layouts and
/// per-profile directories, unlike guessing at candidate paths.
pub fn rebuild(daemon_dir: &Path, repo_root: &Path) -> Result<PathBuf, BuildFailure> {
    out::say("");
    out::say("Rebuilding loom-daemon (cargo build --release)...");

    let log = super::scratch_file("loom-daemon-build-json");
    let Ok(log_handle) = std::fs::File::create(&log) else {
        out::err("cargo build --release failed — the running daemon (if any) was left untouched.");
        return Err(BuildFailure::Compile);
    };

    let args = build_args(daemon_dir);
    if !args.contains(&"otlp") {
        out::warn("This source's loom-daemon/Cargo.toml declares no `otlp` feature (a tag older than #4879) — building without --features otlp; the post-build probe decides whether the result may be installed.");
    }
    let status = Command::new("cargo")
        .args(&args)
        .current_dir(daemon_dir)
        .stdout(Stdio::from(log_handle))
        .status();
    if !status.is_ok_and(|s| s.success()) {
        out::err("cargo build --release failed — the running daemon (if any) was left untouched.");
        return Err(BuildFailure::Compile);
    }

    let json = std::fs::read_to_string(&log).unwrap_or_default();
    let mut new_bin = last_executable(&json).map(PathBuf::from);
    if !new_bin.as_deref().is_some_and(util::is_executable) {
        new_bin = fallback_candidates(daemon_dir, repo_root)
            .into_iter()
            .find(|c| util::is_executable(c));
    }

    let Some(new_bin) = new_bin.filter(|b| util::is_executable(b)) else {
        out::err(&format!(
            "cargo build --release reported success but no loom-daemon executable could be located -- checked cargo's own build JSON, cargo metadata's target_directory, $CARGO_TARGET_DIR, and the conventional {}/target/release and {}/target/release paths.",
            daemon_dir.display(),
            repo_root.display()
        ));
        out::err("Refusing to report a successful update when the built artifact cannot be found (#6160).");
        return Err(BuildFailure::ArtifactNotFound);
    };
    out::ok(&format!("Build succeeded: {}", new_bin.display()));
    probe_otlp(&new_bin)?;
    Ok(new_bin)
}

/// `grep -o '"executable":"[^"]*/loom-daemon"' | tail -n1 | sed …`
///
/// The LAST such field is the `loom-daemon` BIN target's own
/// compiler-artifact message; every earlier one (its library target, its build
/// script, every dependency) reports `executable:null` and is skipped
/// automatically, because the pattern requires a quoted value ending in
/// `/loom-daemon`.
///
/// Parsed textually, not with a JSON parser, for the same reason the shell
/// used `grep`: a JSON parse would succeed on shapes the scan misses and vice
/// versa, and the scan is the behaviour the retained suite's fake cargo emits
/// against.
fn last_executable(json: &str) -> Option<String> {
    const KEY: &str = "\"executable\":\"";
    let mut found = None;
    let mut from = 0;
    while let Some(rel) = json[from..].find(KEY) {
        let start = from + rel + KEY.len();
        match json[start..].find('"') {
            Some(len) => {
                let value = &json[start..start + len];
                if value.ends_with("/loom-daemon") {
                    found = Some(value.to_string());
                }
                from = start + len + 1;
            }
            None => break,
        }
    }
    found
}

/// The belt-and-braces fallbacks, in the shell's order: `cargo metadata`'s own
/// `target_directory` (which resolves the SAME redirect the build used), then
/// `$CARGO_TARGET_DIR` directly, then the pre-#6160 hardcoded candidates
/// (still correct on an unredirected host).
fn fallback_candidates(daemon_dir: &Path, repo_root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(meta) = cargo_metadata_target_dir(daemon_dir) {
        candidates.push(PathBuf::from(meta).join("release/loom-daemon"));
    }
    if let Some(dir) = util::env_non_empty("CARGO_TARGET_DIR") {
        candidates.push(PathBuf::from(dir).join("release/loom-daemon"));
    }
    candidates.push(daemon_dir.join("target/release/loom-daemon"));
    candidates.push(repo_root.join("target/release/loom-daemon"));
    candidates
}

fn cargo_metadata_target_dir(daemon_dir: &Path) -> Option<String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(daemon_dir)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    const KEY: &str = "\"target_directory\":\"";
    let start = text.find(KEY)? + KEY.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The built-commit verification (#4053) — exits **4** on failure.
///
/// A rebuild can succeed (exit 0) yet bake in a STALE `LOOM_DAEMON_GIT_COMMIT`
/// — the exact hazard this script exists to close (a `build.rs` watch-set bug
/// that lets `--version` report the old commit). Provisioning such a binary
/// would "report success while shipping nothing" and, worse, turn any
/// auto-update loop that trusts the baked commit into an infinite
/// rebuild-still-stale retry. So this asserts built commit == source HEAD
/// BEFORE provisioning, and exit 4 marks it as a build-system defect that
/// retrying cannot fix — distinct from the compile failure above.
#[must_use]
pub fn verify_built_commit(new_bin: &Path, source_commit: &str) -> String {
    let built_version_output = util::version_output(new_bin);
    let built_commit = util::extract_commit(&built_version_output);
    if source_commit == "unknown" {
        out::warn("Source HEAD is unknown (no .git?) — skipping built-commit verification (tarball build).");
    } else if built_commit.is_empty() {
        let shown = if built_version_output.is_empty() {
            "<empty>"
        } else {
            built_version_output.as_str()
        };
        out::err(&format!(
            "Build verification FAILED: the freshly-built binary reports no commit in --version output ('{shown}')."
        ));
        out::err("Refusing to provision a binary that cannot prove what it was built from. This is a build-system defect, not a compile failure.");
        super::exit(4);
    } else if built_commit != source_commit {
        out::err(&format!(
            "Build verification FAILED: the freshly-built binary embeds commit '{built_commit}' but source HEAD is '{source_commit}'."
        ));
        out::err("A successful build produced a binary stamped with the WRONG commit (a stale baked-in commit — e.g. a build.rs watch-set bug). Retrying will not fix it; refusing to provision (#4053).");
        super::exit(4);
    } else {
        out::ok(&format!(
            "Build verification: freshly-built binary embeds source HEAD commit ({built_commit})."
        ));
    }
    built_commit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_non_null_executable_field_wins() {
        // Shaped like real `cargo build --message-format=json` output: the
        // library target and the build script report null, the bin target
        // reports the real (possibly redirected) absolute path.
        let stream = concat!(
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"]},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"build-script-build"},"executable":"/x/release/build/foo"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"loom-daemon"},"executable":"/Volumes/Stripe/cargo-target/release/loom-daemon"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        assert_eq!(
            last_executable(stream).as_deref(),
            Some("/Volumes/Stripe/cargo-target/release/loom-daemon")
        );
    }

    #[test]
    fn the_otlp_probe_proceeds_only_on_zero_and_warns_on_a_usage_error() {
        assert_eq!(classify_otlp_probe(Some(0)), OtlpProbe::Proceed);
        assert_eq!(classify_otlp_probe(Some(2)), OtlpProbe::TooOldToAsk);
        assert_eq!(classify_otlp_probe(Some(1)), OtlpProbe::Refuse);
        assert_eq!(classify_otlp_probe(Some(101)), OtlpProbe::Refuse);
        assert_eq!(classify_otlp_probe(None), OtlpProbe::Refuse);
    }

    #[cfg(unix)]
    fn stub_binary(dir: &Path, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("loom-daemon");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{{\"otlp\":false}}'\nexit {exit_code}\n"))
            .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn the_otlp_probe_refuses_the_swap_for_a_binary_that_cannot_export() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(probe_otlp(&stub_binary(tmp.path(), 0)).is_ok());
        assert!(probe_otlp(&stub_binary(tmp.path(), 2)).is_ok());
        assert!(matches!(
            probe_otlp(&stub_binary(tmp.path(), 1)),
            Err(BuildFailure::MissingOtlp)
        ));
        assert!(matches!(
            probe_otlp(&tmp.path().join("no-such-binary")),
            Err(BuildFailure::MissingOtlp)
        ));
    }

    #[test]
    fn otlp_is_requested_only_when_the_manifest_declares_it() {
        let current = "[package]\nname = \"loom-daemon\"\n\n[features]\n# no-op alias\notlp = []\n";
        let gated = "[features]\notlp = [\"dep:opentelemetry-proto\", \"dep:prost\"]\n";
        let before = "[package]\nname = \"loom-daemon\"\n[dependencies]\notlp-ish = \"1\"\n";
        let elsewhere = "[dependencies]\notlp = \"1\"\n[features]\nfast = []\n";
        assert!(declares_otlp_feature(current));
        assert!(declares_otlp_feature(gated));
        assert!(!declares_otlp_feature(before));
        assert!(!declares_otlp_feature(elsewhere));

        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            build_args(tmp.path()),
            [
                "build",
                "--release",
                "--message-format=json-render-diagnostics"
            ],
            "no manifest at all: no feature flag cargo could reject"
        );
        std::fs::write(tmp.path().join("Cargo.toml"), current).unwrap();
        assert_eq!(
            build_args(tmp.path()),
            [
                "build",
                "--release",
                "--features",
                "otlp",
                "--message-format=json-render-diagnostics"
            ]
        );
    }

    /// This crate's own manifest keeps the alias, so a source roll of THIS
    /// tree passes `--features otlp` — and every external caller that does
    /// keeps working (cargo hard-errors on an unknown feature name).
    #[test]
    fn this_crate_still_declares_the_otlp_alias() {
        assert!(declares_otlp_feature(include_str!("../../Cargo.toml")));
    }

    #[test]
    fn a_null_executable_never_matches() {
        let stream = r#"{"executable":null}{"executable":"/x/other-binary"}"#;
        assert_eq!(last_executable(stream), None);
    }
}
