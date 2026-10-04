//! Operator-inbox mail for star-liveness escalations (#10169).
//!
//! A starred issue that needs an operator already gets a forge comment and a
//! Matrix notice. This adds the third leg: one mail to the operator inbox,
//! driven from the [`Notice`] stream. The ledger's `Outcome::Posted` guard
//! already makes a notice once-per-cause fleet-wide, so this module adds **no
//! second dedupe**: `resolved == false` sends, `resolved == true` resolves
//! the same key.
//!
//! Wire format mirrors the bash `inbox_mail` helper in
//! `defaults/docs/inbox-mail.md`: `POST ${LOOM_UI_INBOX_URL}/api/inbox` with
//! `Authorization: Bearer $LOOM_UI_INGEST_KEY`; send body
//! `{key, body, who, severity, title}`, resolve body `{key, resolve: true}`.
//! Either env var unset is a no-op. Failures are logged and swallowed; a mail
//! failure never fails a pass. The bearer key never appears in logs or argv.
//!
//! Limitation: `resolve_cleared` only resolves keys this process announced, so
//! after a daemon restart a still-open mail whose cause then clears is not
//! resolved by this path.

use std::time::Duration;

use super::escalate::Notice;
use crate::types::AskKind;

/// Env: inbox base URL.
pub const INBOX_URL_ENV: &str = "LOOM_UI_INBOX_URL";
/// Env: inbox bearer ingest key.
pub const INGEST_KEY_ENV: &str = "LOOM_UI_INGEST_KEY";

/// Kinds that do not mail: Champion already mails `MergeRiskHold`
/// (`mail-<repo>-crithold-pr-N`), and `BlockedUnnamed` is becoming
/// self-resolving (#10151).
pub const SKIP_KINDS: &[AskKind] = &[AskKind::MergeRiskHold, AskKind::BlockedUnnamed];

/// Destination for operator mail. Implementations swallow their own errors.
pub trait MailSink {
    fn send(&self, key: &str, title: &str, body: &str);
    fn resolve(&self, key: &str);
}

/// Sink used when the inbox is unconfigured: does nothing.
pub struct NoopMailSink;

impl MailSink for NoopMailSink {
    fn send(&self, _key: &str, _title: &str, _body: &str) {}
    fn resolve(&self, _key: &str) {}
}

/// HTTP sink for the loom-ui inbox.
pub struct HttpMailSink {
    endpoint: String,
    ingest_key: String,
    who: String,
    client: reqwest::Client,
    rt: tokio::runtime::Runtime,
}

impl HttpMailSink {
    fn new(base: &str, ingest_key: String, who: String) -> Option<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .ok()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(Self {
            endpoint: format!("{}/api/inbox", base.trim_end_matches('/')),
            ingest_key,
            who,
            client,
            rt,
        })
    }

    fn post(&self, what: &str, payload: &serde_json::Value) {
        let req = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.ingest_key)
            .json(payload);
        match self.rt.block_on(req.send()) {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => log::warn!(
                "star_liveness: inbox mail {what} failed (HTTP {}) - continuing",
                r.status()
            ),
            // Not `{e}` verbatim paths of the URL are fine; the key is a header.
            Err(e) => log::warn!("star_liveness: inbox mail {what} failed ({e}) - continuing"),
        }
    }
}

impl MailSink for HttpMailSink {
    fn send(&self, key: &str, title: &str, body: &str) {
        self.post(
            "send",
            &serde_json::json!({
                "key": key, "body": body, "who": self.who,
                "severity": "normal", "title": title,
            }),
        );
    }

    fn resolve(&self, key: &str) {
        self.post("resolve", &serde_json::json!({"key": key, "resolve": true}));
    }
}

/// Build the sink from the environment; no-op when either var is unset/empty.
#[must_use]
pub fn sink_from_env(who: &str) -> Box<dyn MailSink> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (get(INBOX_URL_ENV), get(INGEST_KEY_ENV)) {
        (Some(url), Some(key)) => match HttpMailSink::new(&url, key, who.to_string()) {
            Some(s) => Box::new(s),
            None => {
                log::warn!("star_liveness: could not build the inbox mail client; mail disabled");
                Box::new(NoopMailSink)
            }
        },
        _ => {
            log::debug!("star_liveness: inbox mail unconfigured; escalations will not mail");
            Box::new(NoopMailSink)
        }
    }
}

/// `mail-<repo>-starliveness-<issue>-<ask key>`, restricted to
/// `[A-Za-z0-9._-]` and capped at 200 chars.
#[must_use]
pub fn mail_key(notice: &Notice) -> String {
    let raw = format!("mail-{}-starliveness-{}-{}", notice.repo, notice.issue, notice.key);
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(200)
        .collect()
}

/// Drive `sink` from one pass's notices.
pub fn dispatch_notices(sink: &dyn MailSink, notices: &[Notice]) {
    for n in notices {
        if SKIP_KINDS.contains(&n.kind) {
            continue;
        }
        let key = mail_key(n);
        if n.resolved {
            sink.resolve(&key);
        } else {
            let title = format!("Operator needed: {}#{}", n.repo, n.issue);
            let body = format!(
                "{}\n\n{}\n\nWhat to do: act on the ask above; this mail resolves \
                 itself when the cause clears.",
                n.url, n.text
            );
            sink.send(&key, &title, &body);
        }
    }
}
