//! Resolve the loom-ui inbox URL and ingest-key *file* for `mail-send` /
//! `inbox_mail` the way the daemon resolves its own observability config
//! (#10137), and report when a mail-meant host cannot send.
//!
//! Precedence for the URL: `$LOOM_UI_INBOX_URL`, else the origin of the
//! observability endpoint (`https://<dashboard>/ingest` -> `https://<dashboard>`),
//! refusing reserved placeholder hosts and loopback (an edge collector is not
//! the inbox). The key itself is `$LOOM_UI_INGEST_KEY` (honoured by the shell
//! consumers), else the file [`crate::observability::resolve_ingest_key_file`]
//! names. **This module never returns or prints the key value** -- it only
//! checks the file is present, readable and non-empty.

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

/// Origin (`scheme://host[:port]`) of `endpoint`, or `None` when it is not
/// http(s), has no host, is a reserved placeholder host or is loopback.
#[must_use]
pub fn derive_inbox_url(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint)
        .ok()
        .filter(|u| u.host_str().is_some())
        .or_else(|| {
            reqwest::Url::parse(&format!("https://{}", endpoint.trim_start_matches("//"))).ok()
        })?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if reserved_placeholder_host(endpoint).is_some() || is_loopback_endpoint(endpoint) {
        return None;
    }
    let host = url.host_str()?;
    Some(match url.port() {
        Some(p) => format!("{}://{}:{}", url.scheme(), host, p),
        None => format!("{}://{}", url.scheme(), host),
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
            "inbox URL: set LOOM_UI_INBOX_URL, or configure observability.endpoint \
             (https://<dashboard>/ingest)"
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
                "ingest key: {f} is {why} -- place this host's key there (mode 600; \
                 loom-ui docs/deploy-runbook.md section 8)"
            )),
            _ => r.missing.push(
                "ingest key: set LOOM_UI_INGEST_KEY or place the key at \
                 ~/.loom/observability/ingest.key (mode 600; loom-ui docs/deploy-runbook.md \
                 section 8)"
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

/// Resolve against the real environment and `root`'s resolved config.
#[must_use]
pub fn resolve(root: &Path) -> InboxResolution {
    let config: ObservabilityConfig = observability::read_config(root);
    let key_file = observability::resolve_ingest_key_file(&config);
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
    fn derives_origin_from_endpoint() {
        for e in [
            "https://dash.acme.dev/ingest",
            "https://dash.acme.dev/ingest/",
            "https://dash.acme.dev",
            "dash.acme.dev/ingest",
        ] {
            assert_eq!(derive_inbox_url(e).as_deref(), Some("https://dash.acme.dev"), "{e}");
        }
        assert_eq!(
            derive_inbox_url("http://dash.acme.dev:8443/x").as_deref(),
            Some("http://dash.acme.dev:8443")
        );
    }

    #[test]
    fn refuses_placeholder_loopback_and_non_http() {
        assert_eq!(derive_inbox_url("https://collector.example.com/ingest"), None);
        assert_eq!(derive_inbox_url("http://127.0.0.1:4318"), None);
        assert_eq!(derive_inbox_url("ftp://dash.acme.dev/x"), None);
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
        assert!(r.missing[0].contains("/k is missing") && r.missing[0].contains("deploy-runbook"));
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
