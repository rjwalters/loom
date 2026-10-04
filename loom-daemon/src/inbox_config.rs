//! Resolve the loom-ui inbox URL and ingest-key *file* for `mail-send` /
//! `inbox_mail` the way the daemon resolves its own observability config
//! (#10137), and report when a mail-meant host cannot send.
//!
//! Precedence for the URL: `$LOOM_UI_INBOX_URL`, else the origin of the
//! observability endpoint -- but **only** an `https` endpoint whose path is
//! `/ingest` (`https://<dashboard>/ingest` -> `https://<dashboard>`), i.e. a
//! daemon exporting straight to the dashboard. Any other endpoint (http, a
//! collector port, a bare origin, another path), a reserved placeholder host
//! or loopback is an edge collector, not the inbox, and leaves the URL
//! unresolved.
//!
//! The key itself is `$LOOM_UI_INGEST_KEY` (honoured by the shell consumers),
//! else the first key *file* of: `$LOOM_UI_INGEST_KEY_FILE`, then
//! `~/.config/loom-ui/ingest.key` (when it exists) -- the per-host dashboard
//! key -- and only then the telemetry tiers
//! [`crate::observability::resolve_ingest_key_file`] names (a telemetry key
//! is the dashboard key only on hosts exporting directly to `/ingest`).
//! **This module never returns or prints the key value** -- it only checks
//! the file is present, readable and non-empty.

use std::path::Path;

use serde::Serialize;

use crate::observability::{
    self,
    endpoint_policy::{is_loopback_endpoint, reserved_placeholder_host},
    ObservabilityConfig,
};

/// Env var carrying the inbox base URL (wins over the derived one).
pub const INBOX_URL_ENV: &str = "LOOM_UI_INBOX_URL";
/// Env var carrying the ingest key itself (wins over the key file).
pub const INGEST_KEY_ENV: &str = "LOOM_UI_INGEST_KEY";
/// Env var naming the dashboard ingest-key *file* (wins over the default
/// `~/.config/loom-ui/ingest.key` and every telemetry tier).
pub const INGEST_KEY_FILE_ENV: &str = "LOOM_UI_INGEST_KEY_FILE";

/// Raw, already-read inputs; keeps [`resolve_from`] pure.
#[derive(Debug, Clone, Default)]
pub struct InboxInputs {
    pub inbox_url_env: Option<String>,
    pub ingest_key_env_set: bool,
    pub endpoint: Option<String>,
    pub key_file: Option<String>,
    /// `Ok(())` when the key file is present, readable and non-empty.
    pub key_file_check: Option<Result<(), String>>,
}

/// The non-secret outcome of resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InboxResolution {
    pub url: Option<String>,
    /// `env` or `endpoint`.
    pub url_source: Option<String>,
    /// Path of a usable key file (never the key).
    pub key_file: Option<String>,
    /// `env` or `file`.
    pub key_source: Option<String>,
    /// Does this host look meant to send mail (endpoint or inbox URL set)?
    pub mail_meant: bool,
    /// Human-readable names of what is unresolved, each with its fix.
    pub missing: Vec<String>,
}

/// Origin (`https://host[:port]`) of `endpoint`, or `None` unless it is an
/// `https` URL whose path is `/ingest` (trailing slash tolerated) on a host
/// that is neither a reserved placeholder nor loopback. A schemeless
/// `host/ingest` is read as `https`. Everything else -- `http`, a collector
/// port such as `:4318`, a bare origin, any other path -- is a collector
/// endpoint, not the dashboard, and is refused.
#[must_use]
pub fn derive_inbox_url(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint)
        .ok()
        .filter(|u| u.host_str().is_some())
        .or_else(|| {
            reqwest::Url::parse(&format!("https://{}", endpoint.trim_start_matches("//"))).ok()
        })?;
    if url.scheme() != "https" || url.path().trim_end_matches('/') != "/ingest" {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    if reserved_placeholder_host(endpoint).is_some() || is_loopback_endpoint(endpoint) {
        return None;
    }
    let host = url.host_str()?;
    Some(match url.port() {
        Some(p) => format!("https://{host}:{p}"),
        None => format!("https://{host}"),
    })
}

/// Pure resolution over already-read inputs.
#[must_use]
pub fn resolve_from(i: &InboxInputs) -> InboxResolution {
    let mut r = InboxResolution::default();
    let env_url = i.inbox_url_env.as_deref().filter(|s| !s.is_empty());
    r.mail_meant = env_url.is_some() || i.endpoint.is_some();
    if let Some(u) = env_url {
        r.url = Some(u.trim_end_matches('/').to_string());
        r.url_source = Some("env".into());
    } else if let Some(u) = i.endpoint.as_deref().and_then(derive_inbox_url) {
        r.url = Some(u);
        r.url_source = Some("endpoint".into());
    }
    if r.url.is_none() {
        r.missing.push(
            "inbox URL: set LOOM_UI_INBOX_URL (derived only from an https \
             observability.endpoint ending in /ingest -- never a collector or loopback)"
                .into(),
        );
    }
    if i.ingest_key_env_set {
        r.key_source = Some("env".into());
    } else {
        match (&i.key_file, &i.key_file_check) {
            (Some(f), Some(Ok(()))) => {
                r.key_file = Some(f.clone());
                r.key_source = Some("file".into());
            }
            (Some(f), Some(Err(why))) => r.missing.push(format!(
                "ingest key: {f} is {why} -- place this host's dashboard key at \
                 ~/.config/loom-ui/ingest.key (or point LOOM_UI_INGEST_KEY_FILE at it; \
                 mode 600; loom-ui docs/operator-mail-onboarding.md)"
            )),
            _ => r.missing.push(
                "ingest key: place this host's dashboard key at ~/.config/loom-ui/ingest.key \
                 (or set LOOM_UI_INGEST_KEY_FILE / LOOM_UI_INGEST_KEY; mode 600; \
                 loom-ui docs/operator-mail-onboarding.md)"
                    .into(),
            ),
        }
    }
    r
}

fn check_key_file(path: &str) -> Result<(), String> {
    match std::fs::read_to_string(Path::new(path)) {
        Ok(s) if s.trim().is_empty() => Err("empty".into()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("missing".into()),
        Err(_) => Err("unreadable".into()),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// `<home>/.config/loom-ui/ingest.key` when that file exists, else `None`.
/// Pure over `home`, so unit-testable without touching the real `$HOME`.
fn ui_key_file_under(home: &Path) -> Option<String> {
    let p = home.join(".config").join("loom-ui").join("ingest.key");
    p.exists().then(|| p.to_string_lossy().to_string())
}

/// The conventional per-host dashboard key file, when present.
#[cfg(not(test))]
fn default_ui_key_file() -> Option<String> {
    dirs::home_dir().and_then(|h| ui_key_file_under(&h))
}

/// Hermetic under `cfg(test)` for the same reason as
/// `observability::default_ingest_key_file`: other tests mutate `$HOME`, and
/// a real ambient key must never leak into a resolution under test.
#[cfg(test)]
fn default_ui_key_file() -> Option<String> {
    None
}

/// Key-file precedence (env value excluded; that is shell-side):
/// `$LOOM_UI_INGEST_KEY_FILE`, then `~/.config/loom-ui/ingest.key` (if
/// present), then the telemetry tiers. `telemetry` is only evaluated when
/// both dashboard tiers are absent.
fn pick_key_file(
    ui_env: Option<String>,
    ui_default: Option<String>,
    telemetry: impl FnOnce() -> Option<String>,
) -> Option<String> {
    ui_env.or(ui_default).or_else(telemetry)
}

/// Resolve against the real environment and `root`'s resolved config.
#[must_use]
pub fn resolve(root: &Path) -> InboxResolution {
    let config: ObservabilityConfig = observability::read_config(root);
    let key_file = pick_key_file(env_nonempty(INGEST_KEY_FILE_ENV), default_ui_key_file(), || {
        observability::resolve_ingest_key_file(&config)
    });
    resolve_from(&InboxInputs {
        inbox_url_env: env_nonempty(INBOX_URL_ENV),
        ingest_key_env_set: env_nonempty(INGEST_KEY_ENV).is_some(),
        endpoint: observability::resolve_endpoint(&config),
        key_file_check: key_file.as_deref().map(check_key_file),
        key_file,
    })
}

/// Machine-greppable lines for the shell consumers. Never contains the key.
#[must_use]
pub fn render_lines(r: &InboxResolution) -> String {
    let mut out = String::new();
    if let Some(u) = &r.url {
        out.push_str(&format!("url={u}\n"));
    }
    if let Some(f) = &r.key_file {
        out.push_str(&format!("key_file={f}\n"));
    }
    for m in &r.missing {
        out.push_str(&format!("missing={m}\n"));
    }
    out
}

/// Health input: only present on a mail-meant host that cannot send.
#[must_use]
pub fn collect_health(root: &Path) -> Option<InboxResolution> {
    let r = resolve(root);
    (r.mail_meant && !r.missing.is_empty()).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> InboxInputs {
        InboxInputs {
            endpoint: Some("https://dash.acme.dev/ingest".into()),
            key_file: Some("/k".into()),
            key_file_check: Some(Ok(())),
            ..Default::default()
        }
    }

    #[test]
    fn derives_origin_only_from_https_ingest_endpoint() {
        for e in [
            "https://dash.acme.dev/ingest",
            "https://dash.acme.dev/ingest/",
            "dash.acme.dev/ingest",
        ] {
            assert_eq!(derive_inbox_url(e).as_deref(), Some("https://dash.acme.dev"), "{e}");
        }
        assert_eq!(
            derive_inbox_url("https://dash.acme.dev:8443/ingest").as_deref(),
            Some("https://dash.acme.dev:8443")
        );
    }

    #[test]
    fn refuses_placeholder_loopback_and_non_http() {
        assert_eq!(derive_inbox_url("https://collector.example.com/ingest"), None);
        assert_eq!(derive_inbox_url("http://127.0.0.1:4318"), None);
        assert_eq!(derive_inbox_url("https://127.0.0.1/ingest"), None);
        assert_eq!(derive_inbox_url("ftp://dash.acme.dev/x"), None);
    }

    /// The #10137 builder caution: only a direct-to-dashboard `https .../ingest`
    /// endpoint is the inbox; http, other paths, bare origins and collector
    /// ports are refused.
    #[test]
    fn refuses_http_bare_origin_other_paths_and_collectors() {
        for e in [
            "http://dash.acme.dev:8443/x",
            "http://dash.acme.dev/ingest",
            "https://dash.acme.dev",
            "https://dash.acme.dev/",
            "https://dash.acme.dev/x",
            "https://dash.acme.dev/v1/traces",
            "https://dash.acme.dev/ingest/extra",
            "https://dash.acme.dev/ingest?x=1",
            "dash.acme.dev",
            "http://10.1.2.3:4318",
            "https://10.1.2.3:4318",
        ] {
            assert_eq!(derive_inbox_url(e), None, "{e}");
        }
        let r = resolve_from(&InboxInputs {
            endpoint: Some("http://10.1.2.3:4318".into()),
            ..inputs()
        });
        assert_eq!(r.url, None);
        assert!(r.mail_meant);
        assert!(r.missing[0].starts_with("inbox URL"), "{:?}", r.missing);
    }

    #[test]
    fn dashboard_key_tiers_precede_telemetry_tiers() {
        let tel = || Some("/telemetry.key".to_string());
        assert_eq!(
            pick_key_file(Some("/env.key".into()), Some("/ui.key".into()), tel).as_deref(),
            Some("/env.key")
        );
        assert_eq!(pick_key_file(None, Some("/ui.key".into()), tel).as_deref(), Some("/ui.key"));
        assert_eq!(pick_key_file(None, None, tel).as_deref(), Some("/telemetry.key"));
        assert_eq!(pick_key_file(None, None, || None), None);
        let mut called = false;
        let _ = pick_key_file(None, Some("/ui.key".into()), || {
            called = true;
            None
        });
        assert!(!called, "telemetry tiers not consulted when a dashboard tier resolves");
    }

    #[test]
    fn ui_key_file_only_when_present() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(ui_key_file_under(d.path()), None);
        let dir = d.path().join(".config/loom-ui");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ingest.key"), "K\n").unwrap();
        assert_eq!(ui_key_file_under(d.path()).as_deref(), dir.join("ingest.key").to_str());
    }

    #[test]
    fn missing_key_names_the_dashboard_key_location() {
        let r = resolve_from(&InboxInputs {
            endpoint: Some("https://dash.acme.dev/ingest".into()),
            ..Default::default()
        });
        assert_eq!(r.missing.len(), 1);
        assert!(r.missing[0].contains("~/.config/loom-ui/ingest.key"), "{:?}", r.missing);
        assert!(r.missing[0].contains("LOOM_UI_INGEST_KEY_FILE"), "{:?}", r.missing);
    }

    #[test]
    fn file_and_endpoint_fallback_resolves() {
        let r = resolve_from(&inputs());
        assert_eq!(r.url.as_deref(), Some("https://dash.acme.dev"));
        assert_eq!(r.url_source.as_deref(), Some("endpoint"));
        assert_eq!(r.key_file.as_deref(), Some("/k"));
        assert!(r.missing.is_empty());
        assert_eq!(render_lines(&r), "url=https://dash.acme.dev\nkey_file=/k\n");
    }

    #[test]
    fn env_url_wins_over_endpoint_and_env_key_skips_file() {
        let mut i = inputs();
        i.inbox_url_env = Some("http://inbox.test/".into());
        i.ingest_key_env_set = true;
        let r = resolve_from(&i);
        assert_eq!(r.url.as_deref(), Some("http://inbox.test"));
        assert_eq!(r.url_source.as_deref(), Some("env"));
        assert_eq!(r.key_source.as_deref(), Some("env"));
        assert_eq!(r.key_file, None);
    }

    #[test]
    fn missing_items_are_named_with_the_fix() {
        let r = resolve_from(&InboxInputs {
            endpoint: Some("https://dash.acme.dev/ingest".into()),
            key_file: Some("/k".into()),
            key_file_check: Some(Err("missing".into())),
            ..Default::default()
        });
        assert!(r.mail_meant);
        assert_eq!(r.missing.len(), 1);
        assert!(r.missing[0].contains("/k is missing"), "{:?}", r.missing);
        assert!(r.missing[0].contains("~/.config/loom-ui/ingest.key"), "{:?}", r.missing);
        let none = resolve_from(&InboxInputs::default());
        assert!(!none.mail_meant);
        assert_eq!(none.missing.len(), 2);
    }

    /// End-to-end through `resolve(root)`: the `observability.ingestKeyFile`
    /// tier and the endpoint-derived URL resolve from a real config file, and
    /// the rendered lines never carry the key. Bare `#[serial]` shares the
    /// lock with the observability env-var tests.
    #[test]
    #[serial_test::serial]
    fn resolve_reads_config_tier_and_never_renders_the_key() {
        for v in [
            INBOX_URL_ENV,
            INGEST_KEY_ENV,
            INGEST_KEY_FILE_ENV,
            observability::ENDPOINT_ENV,
            observability::INGEST_KEY_FILE_ENV,
        ] {
            std::env::remove_var(v);
        }
        let d = tempfile::tempdir().unwrap();
        let key = d.path().join("ingest.key");
        std::fs::write(&key, "SECRETKEY\n").unwrap();
        std::fs::create_dir_all(d.path().join(".loom")).unwrap();
        let write_cfg = |key_path: &Path| {
            let cfg = serde_json::json!({"observability": {
                "endpoint": "https://dash.acme.dev/ingest",
                "ingestKeyFile": key_path.to_str().unwrap()}});
            std::fs::write(d.path().join(".loom/config.json"), cfg.to_string()).unwrap();
        };
        write_cfg(&key);
        let r = resolve(d.path());
        assert_eq!(r.url.as_deref(), Some("https://dash.acme.dev"));
        assert_eq!(r.key_file.as_deref(), key.to_str());
        assert!(r.missing.is_empty(), "{:?}", r.missing);
        assert!(!render_lines(&r).contains("SECRETKEY"));
        assert!(collect_health(d.path()).is_none(), "resolved host: no health line");

        let gone = d.path().join("absent.key");
        write_cfg(&gone);
        let h = collect_health(d.path()).expect("mail-meant + unresolved key => health input");
        assert!(h.missing[0].contains(gone.to_str().unwrap()), "{:?}", h.missing);

        // $LOOM_UI_INGEST_KEY_FILE outranks the telemetry config tier.
        let ui = d.path().join("ui.key");
        std::fs::write(&ui, "UIKEY\n").unwrap();
        write_cfg(&key);
        std::env::set_var(INGEST_KEY_FILE_ENV, &ui);
        let r = resolve(d.path());
        std::env::remove_var(INGEST_KEY_FILE_ENV);
        assert_eq!(r.key_file.as_deref(), ui.to_str());
        assert!(!render_lines(&r).contains("UIKEY"));
    }

    #[test]
    fn key_file_check_never_surfaces_content() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ingest.key");
        assert_eq!(check_key_file(p.to_str().unwrap()), Err("missing".into()));
        std::fs::write(&p, "  \n").unwrap();
        assert_eq!(check_key_file(p.to_str().unwrap()), Err("empty".into()));
        std::fs::write(&p, "SECRETKEY\n").unwrap();
        assert_eq!(check_key_file(p.to_str().unwrap()), Ok(()));
    }
}
