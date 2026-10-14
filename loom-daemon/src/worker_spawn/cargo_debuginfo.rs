//! Debuginfo cap for the cargo builds a Loom-spawned worker runs (#11190,
//! epic #11189).
//!
//! Cargo's default `dev`/`test` profile emits full DWARF (`debug = 2`). In a
//! workspace whose `tests/*.rs` files are each their own crate linking the
//! whole library, every integration-test binary carries a full copy of it: on
//! loom-worker-1 (2026-10-09) one sweep's run dir reached 26 GB, a typical test
//! binary was 438 MB of which 425 MB was `.debug_*` sections. Capping both
//! profiles at `line-tables-only` keeps `file:line` in panic backtraces and
//! drops nearly all of the rest.
//!
//! The cap is two environment variables set at the dispatcher's env seam
//! (`worker_spawn::run`, beside `CARGO_INCREMENTAL=0`), so it reaches every
//! `cargo` the agent runs and nothing outside a spawn. It never overrides a
//! choice someone already made, because a `CARGO_PROFILE_*` variable outranks
//! every `[profile]` table cargo reads:
//!
//! * a variable already in the spawning environment is left alone;
//! * a `debug` key the repo (or a cargo config the build would read) sets for
//!   that profile is left alone;
//! * `cargo.debuginfo` in `.loom/config.json` (env `LOOM_CARGO_DEBUGINFO`)
//!   picks the level, and `"full"` / `"inherit"` / `false` turn the cap off.
//!
//! `test` inherits `debug` from `dev`, so a `dev` choice made anywhere (ambient
//! or repo) also suppresses the `test` variable — injecting it would override
//! the inherited value for exactly the binaries this exists to shrink.

use std::path::{Path, PathBuf};

/// Config key choosing the level (or turning the cap off).
pub const CONFIG_KEY: &str = "cargo.debuginfo";
/// Env override for [`CONFIG_KEY`]. Precedence: env > config > default.
pub const ENV_OVERRIDE: &str = "LOOM_CARGO_DEBUGINFO";
/// The level used when nothing chooses one.
pub const DEFAULT_LEVEL: &str = "line-tables-only";
/// Cargo's env form of `[profile.dev] debug`.
pub const DEV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
/// Cargo's env form of `[profile.test] debug`.
pub const TEST_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";

/// Settings that turn the cap off and leave cargo's own resolution alone.
/// `false` is the JSON bool `cargo.debuginfo: false` (read as its string
/// form): someone who writes it means "no cap", and it reads the same way as
/// `LOOM_CARGO_DEBUGINFO=false`.
const OPT_OUT: [&str; 3] = ["full", "inherit", "false"];
/// Levels that may be injected: every cargo `debug` value below full.
const LEVELS: [&str; 6] = [
    "none",
    "line-directives-only",
    "line-tables-only",
    "limited",
    "0",
    "1",
];

/// Everything the decision depends on, gathered by the caller so the rule is
/// testable without process-global env or a real cargo home.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `LOOM_CARGO_DEBUGINFO`, else `cargo.debuginfo`, else `None`.
    pub setting: Option<String>,
    /// `CARGO_PROFILE_DEV_DEBUG` is already set (non-empty) in the env.
    pub ambient_dev: bool,
    /// `CARGO_PROFILE_TEST_DEBUG` is already set (non-empty) in the env.
    pub ambient_test: bool,
    /// The repo / a cargo config sets `profile.dev.debug`.
    pub explicit_dev: bool,
    /// The repo / a cargo config sets `profile.test.debug`.
    pub explicit_test: bool,
}

/// What to inject, plus one log line saying why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// `(variable, value)` pairs to set on the worker.
    pub vars: Vec<(&'static str, String)>,
    /// `# LOOM_CARGO_DEBUGINFO …` marker for the worker log.
    pub marker: String,
}

/// The rule, pure.
#[must_use]
pub fn decide(inputs: &Inputs) -> Decision {
    let raw = inputs
        .setting
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (level, note) = match raw {
        None => (Some(DEFAULT_LEVEL), String::new()),
        Some(s) if OPT_OUT.contains(&s) => (None, format!(" setting={s}")),
        Some(s) if LEVELS.contains(&s) => (Some(s), format!(" setting={s}")),
        Some(s) => (Some(DEFAULT_LEVEL), format!(" setting={s:?} unrecognized")),
    };
    let Some(level) = level else {
        return Decision {
            vars: Vec::new(),
            marker: format!("# LOOM_CARGO_DEBUGINFO off{note} (#11190)"),
        };
    };
    let dev_chosen = inputs.ambient_dev || inputs.explicit_dev;
    let skip = |ambient: bool, explicit: bool, inherited: bool| {
        if ambient {
            Some("ambient")
        } else if explicit {
            Some("repo-profile")
        } else if inherited {
            Some("inherits-dev")
        } else {
            None
        }
    };
    let mut vars = Vec::new();
    let mut parts = Vec::new();
    for (var, label, why) in [
        (DEV_VAR, "dev", skip(inputs.ambient_dev, inputs.explicit_dev, false)),
        (TEST_VAR, "test", skip(inputs.ambient_test, inputs.explicit_test, dev_chosen)),
    ] {
        match why {
            Some(why) => parts.push(format!("{label}=kept({why})")),
            None => {
                vars.push((var, level.to_string()));
                parts.push(format!("{label}={level}"));
            }
        }
    }
    Decision {
        vars,
        marker: format!("# LOOM_CARGO_DEBUGINFO {}{note} (#11190)", parts.join(" ")),
    }
}

/// Whether `name` is one of the two cap variables and `value` is empty.
///
/// Cargo does not read an empty `CARGO_PROFILE_*_DEBUG` as unset: it fails the
/// whole build (`invalid value: string ""`). [`inputs_for`] already treats an
/// empty variable as not ambient, so the seam overwrites it whenever it injects
/// — but when it injects nothing (an opt-out, a repo profile, `test` inheriting
/// `dev`) the empty value would be inherited as is. Every boundary a worker's
/// env crosses uses this to make such a variable unset instead.
#[must_use]
pub fn is_empty_cap_var(name: &str, value: &std::ffi::OsStr) -> bool {
    (name == DEV_VAR || name == TEST_VAR) && value.is_empty()
}

/// Apply `decision` to the worker's `command`: unset a cap variable that
/// `ambient` reports as set but empty (see [`is_empty_cap_var`]), then set the
/// injected ones. In that order, so an injected value replaces the removal.
pub fn apply(
    command: &mut std::process::Command,
    decision: &Decision,
    ambient: impl Fn(&str) -> Option<std::ffi::OsString>,
) {
    for var in [DEV_VAR, TEST_VAR] {
        if ambient(var).is_some_and(|value| is_empty_cap_var(var, &value)) {
            command.env_remove(var);
        }
    }
    command.envs(decision.vars.iter().map(|(k, v)| (*k, v.as_str())));
}

/// The production inputs for a worker whose repository is `root`.
#[must_use]
pub fn inputs_for(root: &Path) -> Inputs {
    let nonempty = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let setting = std::env::var(ENV_OVERRIDE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let config = crate::config_resolver::resolve_effective_config(root);
            match crate::config_resolver::get_path(&config, CONFIG_KEY) {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(other) if !other.is_null() => Some(other.to_string()),
                _ => None,
            }
        });
    let (explicit_dev, explicit_test) = explicit_profiles(root, cargo_home().as_deref());
    Inputs {
        setting,
        ambient_dev: nonempty(DEV_VAR),
        ambient_test: nonempty(TEST_VAR),
        explicit_dev,
        explicit_test,
    }
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
}

/// Whether `profile.dev.debug` / `profile.test.debug` is set by the repo's
/// root `Cargo.toml` or by any cargo config file a build under `root` reads
/// (`.cargo/config{,.toml}` in `root` and its ancestors, then `cargo_home`).
/// Unreadable or malformed files contribute nothing.
#[must_use]
pub fn explicit_profiles(root: &Path, cargo_home: Option<&Path>) -> (bool, bool) {
    let mut files = vec![root.join("Cargo.toml")];
    for dir in root.ancestors() {
        files.push(dir.join(".cargo/config.toml"));
        files.push(dir.join(".cargo/config"));
    }
    if let Some(home) = cargo_home {
        files.push(home.join("config.toml"));
        files.push(home.join("config"));
    }
    let (mut dev, mut test) = (false, false);
    for file in files {
        let Some(table) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
        else {
            continue;
        };
        let sets = |profile: &str| {
            table
                .get("profile")
                .and_then(|p| p.get(profile))
                .is_some_and(|p| p.get("debug").is_some())
        };
        dev |= sets("dev");
        test |= sets("test");
    }
    (dev, test)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(inputs: &Inputs) -> Vec<(&'static str, String)> {
        decide(inputs).vars
    }

    #[test]
    fn default_caps_both_profiles() {
        let d = decide(&Inputs::default());
        assert_eq!(
            d.vars,
            vec![
                (DEV_VAR, DEFAULT_LEVEL.to_string()),
                (TEST_VAR, DEFAULT_LEVEL.to_string())
            ]
        );
        assert!(
            d.marker
                .contains("dev=line-tables-only test=line-tables-only"),
            "{}",
            d.marker
        );
    }

    #[test]
    fn opt_out_injects_nothing() {
        for s in ["full", "inherit", " full ", "false"] {
            let d = decide(&Inputs {
                setting: Some(s.into()),
                ..Inputs::default()
            });
            assert!(d.vars.is_empty(), "{s}");
            assert!(d.marker.contains(" off "), "{}", d.marker);
        }
    }

    #[test]
    fn a_chosen_level_is_injected_and_garbage_falls_back() {
        let limited = Inputs {
            setting: Some("limited".into()),
            ..Inputs::default()
        };
        assert_eq!(vars(&limited)[0], (DEV_VAR, "limited".to_string()));
        let bad = decide(&Inputs {
            setting: Some("verbose".into()),
            ..Inputs::default()
        });
        assert_eq!(bad.vars[1], (TEST_VAR, DEFAULT_LEVEL.to_string()));
        assert!(bad.marker.contains("unrecognized"), "{}", bad.marker);
    }

    #[test]
    fn ambient_dev_wins_and_test_inherits_it() {
        let d = decide(&Inputs {
            ambient_dev: true,
            ..Inputs::default()
        });
        assert!(d.vars.is_empty(), "{d:?}");
        assert!(
            d.marker
                .contains("dev=kept(ambient) test=kept(inherits-dev)"),
            "{}",
            d.marker
        );
    }

    #[test]
    fn ambient_test_only_keeps_test() {
        let v = vars(&Inputs {
            ambient_test: true,
            ..Inputs::default()
        });
        assert_eq!(v, vec![(DEV_VAR, DEFAULT_LEVEL.to_string())]);
    }

    #[test]
    fn explicit_repo_profiles_are_respected() {
        assert!(vars(&Inputs {
            explicit_dev: true,
            ..Inputs::default()
        })
        .is_empty());
        let v = vars(&Inputs {
            explicit_test: true,
            ..Inputs::default()
        });
        assert_eq!(v, vec![(DEV_VAR, DEFAULT_LEVEL.to_string())]);
    }

    /// What `apply` leaves on a command, as `(name, Some(value) | None)`
    /// where `None` is an explicit removal.
    fn applied(inputs: &Inputs, ambient: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        let mut command = std::process::Command::new("true");
        apply(&mut command, &decide(inputs), |k| {
            ambient
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, value)| std::ffi::OsString::from(value))
        });
        command
            .get_envs()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
            })
            .collect()
    }

    #[test]
    fn a_set_but_empty_variable_never_reaches_cargo() {
        // Cargo rejects `CARGO_PROFILE_DEV_DEBUG=` outright, so an empty value
        // the seam does not overwrite must be removed, not inherited.
        let both_empty = [(DEV_VAR, ""), (TEST_VAR, "")];
        let removed = vec![(DEV_VAR.to_string(), None), (TEST_VAR.to_string(), None)];
        // Opt-out: nothing is injected, both empties are unset.
        let off = Inputs {
            setting: Some("full".into()),
            ..Inputs::default()
        };
        assert_eq!(applied(&off, &both_empty), removed);
        // A repo profile for `dev` (which `test` inherits): same.
        let repo = Inputs {
            explicit_dev: true,
            ..Inputs::default()
        };
        assert_eq!(applied(&repo, &both_empty), removed);
        // `test` kept for the repo's own profile: dev is injected over the
        // empty value, the empty test variable is unset.
        let repo_test = Inputs {
            explicit_test: true,
            ..Inputs::default()
        };
        assert_eq!(
            applied(&repo_test, &both_empty),
            vec![
                (DEV_VAR.to_string(), Some(DEFAULT_LEVEL.to_string())),
                (TEST_VAR.to_string(), None)
            ]
        );
        // The default case injects both, replacing the removals.
        assert_eq!(
            applied(&Inputs::default(), &both_empty),
            vec![
                (DEV_VAR.to_string(), Some(DEFAULT_LEVEL.to_string())),
                (TEST_VAR.to_string(), Some(DEFAULT_LEVEL.to_string()))
            ]
        );
        // A real ambient value and an unset variable are both left alone.
        let ambient = Inputs {
            ambient_dev: true,
            ..Inputs::default()
        };
        assert!(applied(&ambient, &[(DEV_VAR, "full")]).is_empty());
        assert!(applied(&off, &[]).is_empty());
        // Only the two cap variables are ever treated this way.
        let empty = std::ffi::OsStr::new("");
        assert!(is_empty_cap_var(DEV_VAR, empty) && is_empty_cap_var(TEST_VAR, empty));
        assert!(!is_empty_cap_var(DEV_VAR, std::ffi::OsStr::new("0")));
        assert!(!is_empty_cap_var("CARGO_TARGET_DIR", empty));
    }

    #[test]
    fn explicit_profiles_reads_manifest_and_cargo_configs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("repo");
        let home = d.path().join("cargo-home");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (false, false));

        std::fs::write(root.join("Cargo.toml"), "[workspace]\n[profile.dev]\nopt-level = 1\n")
            .unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (false, false), "no debug key");

        std::fs::write(root.join("Cargo.toml"), "[profile.test]\ndebug = 2\n").unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (false, true));

        std::fs::write(root.join(".cargo/config.toml"), "profile.dev.debug = true\n").unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (true, true));

        std::fs::remove_file(root.join(".cargo/config.toml")).unwrap();
        std::fs::remove_file(root.join("Cargo.toml")).unwrap();
        std::fs::write(home.join("config.toml"), "[profile.dev]\ndebug = \"full\"\n").unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (true, false), "cargo home");

        std::fs::write(home.join("config.toml"), "not = [valid toml").unwrap();
        assert_eq!(explicit_profiles(&root, Some(&home)), (false, false), "malformed");
    }
}
