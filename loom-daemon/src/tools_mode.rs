//! Mode resolution for the `loom-tools` binary (#12938, epic #12708; design
//! `docs/design/loom-tools-cli-boundary.md` §7.3).
//!
//! `loom-tools` must not silently inherit a fleet session's routing: a shell
//! that carries `LOOM_GH_GATEWAY_DIR` / `LOOM_GH_READER_ROOT` would otherwise
//! have its forge reads shed by [`crate::forge_gateway::enable_cli_reads`]
//! (`CLI_READER_ONLY`). [`resolve`] is the pure table; the bin applies the
//! result and only then, on the fleet route, enables the reader routing.
//! `loom-daemon <name>` never calls this: it ignores `mode`.

use std::ffi::OsString;
use std::path::Path;

use crate::forge_gateway::DIR_ENV as GATEWAY_DIR_ENV;
use crate::forge_identity::FRONT_ROOT_ENV as READER_ROOT_ENV;

/// Environment override for the config key (`env > config > default`).
pub const MODE_ENV: &str = "LOOM_MODE";

/// The accepted `mode` values, in the order error text names them.
pub const MODES: [&str; 3] = ["fleet", "local-daemon", "session"];

/// How the tools process routes forge calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The user's own `gh` / Gitea credential: no gateway, no readers.
    Local,
    /// The fleet gateway / reader pool the dispatcher exported.
    Fleet,
}

/// A resolved route plus the one-time stderr notes to print.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    pub route: Route,
    /// Inherited routing variables the tools path ignores (local routes only).
    pub ignored_vars: Vec<&'static str>,
    /// Warnings for stderr, already worded.
    pub notes: Vec<String>,
}

/// A `mode` value and where it came from (`LOOM_MODE`, or a config tier file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced {
    pub value: String,
    pub source: String,
}

/// Everything [`resolve`] reads, injected so the table is testable.
pub struct Inputs {
    /// `LOOM_MODE`, when set and non-empty.
    pub env_mode: Option<String>,
    /// The `mode` key of the resolved config, with its tier file.
    pub config_mode: Option<Sourced>,
    /// `LOOM_GH_GATEWAY_DIR`.
    pub gateway_dir: Option<OsString>,
    /// `LOOM_GH_READER_ROOT`.
    pub reader_root: Option<OsString>,
}

fn non_empty(v: &Option<OsString>) -> Option<&OsString> {
    v.as_ref().filter(|v| !v.is_empty())
}

fn accepted() -> String {
    MODES.join(", ")
}

/// Resolve the route per the §7.3 table. `Err` is the actionable error text.
pub fn resolve(inputs: &Inputs) -> Result<Resolution, String> {
    let gateway = non_empty(&inputs.gateway_dir);
    let reader = non_empty(&inputs.reader_root);
    let inherited = gateway.is_some() || reader.is_some();
    let mut named: Vec<&'static str> = Vec::new();
    if gateway.is_some() {
        named.push(GATEWAY_DIR_ENV);
    }
    if reader.is_some() {
        named.push(READER_ROOT_ENV);
    }
    let named_list = named.join(" and ");

    let mut notes = Vec::new();
    // env wins over the tier; show both when they disagree.
    let env_mode = inputs
        .env_mode
        .as_ref()
        .filter(|v| !v.is_empty())
        .map(|v| Sourced {
            value: v.clone(),
            source: MODE_ENV.to_string(),
        });
    let chosen = match (&env_mode, &inputs.config_mode) {
        (Some(e), Some(c)) if e.value != c.value => {
            notes.push(format!(
                "loom-tools: mode is {:?} from {} but {:?} from {}; {} wins",
                e.value, e.source, c.value, c.source, e.source
            ));
            Some(e.clone())
        }
        (Some(e), _) => Some(e.clone()),
        (None, c) => c.clone(),
    };

    let Some(chosen) = chosen else {
        return Err(if inherited {
            format!(
                "loom-tools: `mode` is unset but {named_list} {} set in this environment. A fleet \
                 session's environment leaked into this shell, or `mode` was never rendered to \
                 this host; not guessing between session and fleet routing. Fix: set \
                 `{MODE_ENV}=session` (or `mode` in .loom-local/local.json) to use your own \
                 credential, or `unset {}` to drop the inherited routing. Inspect with `loom-tools \
                 config get mode`.",
                if named.len() > 1 { "are" } else { "is" },
                named.join(" "),
            )
        } else {
            format!(
                "loom-tools: `mode` is unset. Set it to one of: {} (env `{MODE_ENV}` or the `mode` \
                 key in .loom-local/local.json). Inspect with `loom-tools config get mode`.",
                accepted()
            )
        });
    };

    match chosen.value.as_str() {
        "session" | "local-daemon" => {
            if inherited {
                notes.push(format!(
                    "loom-tools: ignoring inherited {named_list} (mode is {:?} from {}); they \
                     came from a parent fleet session and are not used for routing",
                    chosen.value, chosen.source
                ));
            }
            Ok(Resolution {
                route: Route::Local,
                ignored_vars: named,
                notes,
            })
        }
        "fleet" => {
            if !inherited {
                return Err(format!(
                    "loom-tools: mode is \"fleet\" (from {}) but neither {GATEWAY_DIR_ENV} nor \
                     {READER_ROOT_ENV} is set; no fleet route is configured",
                    chosen.source
                ));
            }
            if let Some(dir) = gateway {
                if !Path::new(dir).is_absolute() || !Path::new(dir).is_dir() {
                    return Err(format!(
                        "loom-tools: mode is \"fleet\" but {GATEWAY_DIR_ENV}={dir:?} is not an \
                         existing absolute directory"
                    ));
                }
            }
            if let Some(root) = reader {
                let root = Path::new(root);
                if !root.is_absolute() || !root.join(".loom").is_dir() {
                    return Err(format!(
                        "loom-tools: mode is \"fleet\" but {READER_ROOT_ENV}={root:?} is not an \
                         absolute path with a .loom/ directory"
                    ));
                }
            }
            Ok(Resolution {
                route: Route::Fleet,
                ignored_vars: Vec::new(),
                notes,
            })
        }
        other => Err(format!(
            "loom-tools: invalid mode {other:?} from {}; accepted values: {}",
            chosen.source,
            accepted()
        )),
    }
}

/// Gather the real inputs: `LOOM_MODE`, the `mode` config key under `repo_root`
/// and the two inherited routing variables.
pub fn inputs_from_env(repo_root: &Path) -> Inputs {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let config_mode = crate::config_resolver::get_path(&effective, "mode")
        .filter(|v| !v.is_null())
        .map(|v| Sourced {
            // A non-string stays visible so `resolve` reports it as invalid.
            value: v.as_str().map_or_else(|| v.to_string(), str::to_string),
            source: crate::config_resolver::source_of(repo_root, "mode")
                .map_or_else(|| "config".to_string(), |p| p.display().to_string()),
        });
    Inputs {
        env_mode: std::env::var(MODE_ENV).ok(),
        config_mode,
        gateway_dir: std::env::var_os(GATEWAY_DIR_ENV),
        reader_root: std::env::var_os(READER_ROOT_ENV),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: &str) -> Option<Sourced> {
        Some(Sourced {
            value: v.into(),
            source: "local.json".into(),
        })
    }

    fn inp(mode: Option<&str>, gw: Option<&Path>, rr: Option<&Path>) -> Inputs {
        Inputs {
            env_mode: None,
            config_mode: mode.and_then(cfg),
            gateway_dir: gw.map(|p| p.as_os_str().to_owned()),
            reader_root: rr.map(|p| p.as_os_str().to_owned()),
        }
    }

    fn fleet_dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        let gw = tempfile::tempdir().unwrap();
        let rr = tempfile::tempdir().unwrap();
        std::fs::create_dir(rr.path().join(".loom")).unwrap();
        (gw, rr)
    }

    #[test]
    fn session_without_routing_is_local() {
        let r = resolve(&inp(Some("session"), None, None)).unwrap();
        assert_eq!(r.route, Route::Local);
        assert!(r.notes.is_empty() && r.ignored_vars.is_empty());
    }

    #[test]
    fn local_daemon_without_routing_is_local() {
        assert_eq!(
            resolve(&inp(Some("local-daemon"), None, None))
                .unwrap()
                .route,
            Route::Local
        );
    }

    #[test]
    fn session_or_local_with_inherited_vars_ignores_them_and_warns() {
        let (gw, rr) = fleet_dirs();
        for mode in ["session", "local-daemon"] {
            let r = resolve(&inp(Some(mode), Some(gw.path()), Some(rr.path()))).unwrap();
            assert_eq!(r.route, Route::Local);
            assert_eq!(r.ignored_vars, vec![GATEWAY_DIR_ENV, READER_ROOT_ENV]);
            assert_eq!(r.notes.len(), 1);
            assert!(r.notes[0].contains(GATEWAY_DIR_ENV) && r.notes[0].contains(READER_ROOT_ENV));
        }
    }

    #[test]
    fn fleet_with_valid_routing_is_fleet() {
        let (gw, rr) = fleet_dirs();
        assert_eq!(
            resolve(&inp(Some("fleet"), Some(gw.path()), None))
                .unwrap()
                .route,
            Route::Fleet
        );
        assert_eq!(
            resolve(&inp(Some("fleet"), None, Some(rr.path())))
                .unwrap()
                .route,
            Route::Fleet
        );
    }

    #[test]
    fn fleet_with_invalid_routing_is_an_error_not_a_fallback() {
        let missing = Path::new("/nonexistent/loom-tools-test");
        let e = resolve(&inp(Some("fleet"), Some(missing), None)).unwrap_err();
        assert!(e.contains(GATEWAY_DIR_ENV), "{e}");
        let bare = tempfile::tempdir().unwrap(); // no .loom/
        let e = resolve(&inp(Some("fleet"), None, Some(bare.path()))).unwrap_err();
        assert!(e.contains(READER_ROOT_ENV), "{e}");
    }

    #[test]
    fn fleet_without_routing_is_an_error() {
        let e = resolve(&inp(Some("fleet"), None, None)).unwrap_err();
        assert!(e.contains("no fleet route"), "{e}");
    }

    #[test]
    fn unset_without_routing_names_values_and_config_get() {
        let e = resolve(&inp(None, None, None)).unwrap_err();
        for m in MODES {
            assert!(e.contains(m), "{e}");
        }
        assert!(e.contains("loom-tools config get mode"), "{e}");
    }

    #[test]
    fn unset_with_fleet_variables_is_an_error_naming_them_and_both_fixes() {
        let (gw, rr) = fleet_dirs();
        let e = resolve(&inp(None, Some(gw.path()), Some(rr.path()))).unwrap_err();
        assert!(e.contains(GATEWAY_DIR_ENV) && e.contains(READER_ROOT_ENV), "{e}");
        assert!(e.contains("leaked") && e.contains("never rendered"), "{e}");
        assert!(e.contains("LOOM_MODE=session") && e.contains("unset "), "{e}");
        let e = resolve(&inp(None, Some(gw.path()), None)).unwrap_err();
        assert!(e.contains(GATEWAY_DIR_ENV) && !e.contains(READER_ROOT_ENV), "{e}");
    }

    #[test]
    fn empty_routing_variables_do_not_count() {
        let i = Inputs {
            env_mode: None,
            config_mode: cfg("session"),
            gateway_dir: Some(OsString::new()),
            reader_root: Some(OsString::new()),
        };
        let r = resolve(&i).unwrap();
        assert!(r.notes.is_empty());
    }

    #[test]
    fn invalid_value_names_accepted_values_and_source() {
        let e = resolve(&inp(Some("bogus"), None, None)).unwrap_err();
        assert!(e.contains("bogus") && e.contains("local.json"), "{e}");
        for m in MODES {
            assert!(e.contains(m), "{e}");
        }
    }

    #[test]
    fn env_overrides_config_and_both_are_printed() {
        let i = Inputs {
            env_mode: Some("session".into()),
            config_mode: cfg("fleet"),
            gateway_dir: None,
            reader_root: None,
        };
        let r = resolve(&i).unwrap();
        assert_eq!(r.route, Route::Local);
        assert!(r.notes[0].contains("LOOM_MODE") && r.notes[0].contains("local.json"));
    }

    #[test]
    fn env_alone_is_enough_and_empty_env_is_unset() {
        let i = Inputs {
            env_mode: Some("session".into()),
            config_mode: None,
            gateway_dir: None,
            reader_root: None,
        };
        assert_eq!(resolve(&i).unwrap().route, Route::Local);
        let i = Inputs {
            env_mode: Some(String::new()),
            ..i
        };
        assert!(resolve(&i).is_err());
    }
}
