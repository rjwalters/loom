//! `loom-daemon mail preflight` (#10146).
//!
//! Send-time check for `/loom:mail-send`: before either leg (loom-ui inbox,
//! Matrix) posts anything, verify that this machine is onboarded and say
//! exactly what is wrong when it is not. Exit 0 = every prerequisite holds;
//! exit 2 = at least one refusal, nothing was sent.
//!
//! The only network traffic is an authenticated no-op probe, `POST /api/inbox`
//! with `{}` (400 = key accepted, 401 = key rejected). The ingest key is read
//! from the environment or a key file, travels only in an in-process header
//! and is never printed or put on argv.
//!
//! Self-contained: URL and key resolution here is deliberately minimal; once
//! `inbox_config` (#10137) lands this should resolve through it instead.
//! Placed under [`super::script_ports::ScriptPortCommand`] for the same
//! frozen-`main.rs` reason as `eta`.

use std::path::{Path, PathBuf};

use anyhow::Result;

const DOC: &str = ".loom/docs/inbox-mail.md (section \"Operator onboarding\")";
const DEFAULT_KEY_FILE: &str = ".config/loom-ui/ingest.key";
const PROD_ENV: &str = ".config/loom-ui/prod.env";
const TELEMETRY_KEY: &str = ".loom/observability/ingest.key";
const DEFAULT_MATRIX_POST: &str = ".claude/skills/matrix-post/post.sh";
/// A reply target no thread can have: a stale build rejects the shape first.
const NIL_THREAD: &str = "00000000-0000-0000-0000-000000000000";

#[derive(clap::Subcommand)]
pub(crate) enum MailCommand {
    /// Check inbox URL, key, key acceptance and `MATRIX_POST` before sending.
    Preflight(PreflightArgs),
}

#[derive(clap::Args)]
pub(crate) struct PreflightArgs {
    /// The send is a follow-up (`REPLY_TO`): also check the dashboard build
    /// understands `replyTo`.
    #[arg(long)]
    reply_to: bool,
}

impl MailCommand {
    pub(crate) fn run(self) -> Result<()> {
        let MailCommand::Preflight(args) = self;
        let inputs = Inputs::from_env();
        match check(&inputs, args.reply_to, &HttpProber) {
            Ok(()) => {
                println!("mail preflight ok");
                Ok(())
            }
            Err(msg) => {
                eprintln!("SEND NOT ATTEMPTED — {msg}\nOperator onboarding: {DOC}");
                std::process::exit(2);
            }
        }
    }
}

/// Everything the checks read from the environment, injectable for tests.
struct Inputs {
    home: PathBuf,
    inbox_url: Option<String>,
    ui_url: Option<String>,
    key_env: Option<String>,
    key_file: Option<PathBuf>,
    matrix_post: Option<PathBuf>,
}

impl Inputs {
    fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            home: dirs::home_dir().unwrap_or_default(),
            inbox_url: var("LOOM_UI_INBOX_URL"),
            ui_url: var("LOOM_UI_URL"),
            key_env: var("LOOM_UI_INGEST_KEY"),
            key_file: var("LOOM_UI_INGEST_KEY_FILE").map(PathBuf::from),
            matrix_post: var("MATRIX_POST").map(PathBuf::from),
        }
    }
}

/// POSTs a JSON body with a bearer key; returns (status, response body).
trait Prober {
    fn post(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String>;
}

struct HttpProber;

impl Prober for HttpProber {
    fn post(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client
                .post(url)
                .bearer_auth(key)
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            let status = resp.status().as_u16();
            Ok((status, resp.text().await.unwrap_or_default()))
        })
    }
}

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `LOOM_UI_URL` from the environment, else `~/.config/loom-ui/prod.env`.
fn url_hint(i: &Inputs) -> Option<String> {
    i.ui_url.clone().or_else(|| {
        let text = std::fs::read_to_string(i.home.join(PROD_ENV)).ok()?;
        text.lines().find_map(|l| {
            let v = l
                .trim()
                .strip_prefix("export ")
                .unwrap_or(l.trim())
                .strip_prefix("LOOM_UI_URL=")?;
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            (!v.is_empty()).then(|| v.to_string())
        })
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// All checks, in order; `Err` carries the diagnosis. Nothing but the `{}`
/// probes (and, with `reply_to`, a nil-thread shape probe) is ever sent.
fn check(i: &Inputs, reply_to: bool, net: &dyn Prober) -> Result<(), String> {
    let Some(url) = i.inbox_url.as_deref() else {
        let hint = match url_hint(i) {
            Some(_) => " LOOM_UI_URL is set (or in ~/.config/loom-ui/prod.env): export \
                       LOOM_UI_INBOX_URL with that value."
                .to_string(),
            None => String::new(),
        };
        return Err(format!("missing config: LOOM_UI_INBOX_URL.{hint}"));
    };

    let key_path = i
        .key_file
        .clone()
        .unwrap_or_else(|| i.home.join(DEFAULT_KEY_FILE));
    let key = match i.key_env.clone() {
        Some(k) => k.trim().to_string(),
        None => read_trimmed(&key_path).ok_or_else(|| {
            format!(
                "missing config: no ingest key. Set LOOM_UI_INGEST_KEY, or put the key in \
                 {} (LOOM_UI_INGEST_KEY_FILE overrides) and keep it owner-readable only.",
                key_path.display()
            )
        })?,
    };
    if key.contains('"') || key.contains('\\') {
        return Err(
            "the ingest key contains a quote or backslash, which the send cannot carry".into()
        );
    }

    let post = i
        .matrix_post
        .clone()
        .unwrap_or_else(|| i.home.join(DEFAULT_MATRIX_POST));
    if !is_executable(&post) {
        return Err(format!(
            "MATRIX_POST is not an executable file ({}); set MATRIX_POST to the operator-local \
             matrix-post script. The loom-ui leg was not attempted, so nothing is half-delivered.",
            post.display()
        ));
    }

    let endpoint = format!("{}/api/inbox", url.trim_end_matches('/'));
    match net.post(&endpoint, &key, "{}") {
        Ok((400, _)) | Ok((200..=299, _)) => {}
        Ok((401, _)) => {
            let telemetry = read_trimmed(&i.home.join(TELEMETRY_KEY));
            return Err(if telemetry.as_deref() == Some(key.as_str()) {
                "the inbox rejected the key (401), and it is the daemon TELEMETRY key \
                 (~/.loom/observability/ingest.key), not a dashboard mail key. Mint a per-machine \
                 key with POST /admin/hosts (admin token) and store it in the ingest key file."
                    .into()
            } else {
                "the inbox rejected the key (401: invalid or revoked). Mint a per-machine key \
                 with POST /admin/hosts (admin token)."
                    .into()
            });
        }
        Ok((code, _)) => return Err(format!("unexpected HTTP {code} from the inbox probe")),
        Err(e) => return Err(format!("inbox unreachable: {e}")),
    }

    if reply_to {
        let body = format!(r#"{{"replyTo":"{NIL_THREAD}","body":"preflight"}}"#);
        if let Ok((400, text)) = net.post(&endpoint, &key, &body) {
            if text.contains("key is required") {
                return Err(
                    "the dashboard build predates replyTo (a follow-up was answered \
                            \"400 key is required\"); redeploy loom-ui before sending a REPLY_TO."
                        .into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    struct Fake {
        replies: Vec<Result<(u16, String), String>>,
        sent: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(replies: Vec<Result<(u16, String), String>>) -> Self {
            Self {
                replies,
                sent: RefCell::new(vec![]),
            }
        }
    }

    impl Prober for Fake {
        fn post(&self, _u: &str, _k: &str, body: &str) -> Result<(u16, String), String> {
            let n = self.sent.borrow().len();
            self.sent.borrow_mut().push(body.to_string());
            self.replies
                .get(n)
                .cloned()
                .unwrap_or(Ok((400, String::new())))
        }
    }

    fn home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let post = d.path().join(DEFAULT_MATRIX_POST);
        std::fs::create_dir_all(post.parent().unwrap()).unwrap();
        std::fs::write(&post, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&post, std::fs::Permissions::from_mode(0o755)).unwrap();
        d
    }

    fn inputs(d: &tempfile::TempDir) -> Inputs {
        Inputs {
            home: d.path().into(),
            inbox_url: Some("https://inbox.invalid/".into()),
            ui_url: None,
            key_env: Some("mail-key".into()),
            key_file: None,
            matrix_post: None,
        }
    }

    fn write(d: &tempfile::TempDir, rel: &str, s: &str) {
        let p = d.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn ok_when_probe_returns_400() {
        let d = home();
        let f = Fake::new(vec![Ok((400, "bad".into()))]);
        assert!(check(&inputs(&d), false, &f).is_ok());
        assert_eq!(*f.sent.borrow(), vec!["{}".to_string()]);
    }

    #[test]
    fn missing_url_names_var_and_hints_from_env_or_prod_env() {
        let d = home();
        let mut i = inputs(&d);
        i.inbox_url = None;
        let f = Fake::new(vec![]);
        let e = check(&i, false, &f).unwrap_err();
        assert!(e.contains("LOOM_UI_INBOX_URL") && !e.contains("LOOM_UI_URL is set"));
        i.ui_url = Some("https://x.invalid".into());
        assert!(check(&i, false, &f)
            .unwrap_err()
            .contains("LOOM_UI_URL is set"));
        i.ui_url = None;
        write(&d, PROD_ENV, "export LOOM_UI_URL=\"https://y.invalid\"\n");
        assert!(check(&i, false, &f)
            .unwrap_err()
            .contains("LOOM_UI_URL is set"));
        assert!(f.sent.borrow().is_empty());
    }

    #[test]
    fn missing_key_sends_nothing_and_key_file_is_used() {
        let d = home();
        let mut i = inputs(&d);
        i.key_env = None;
        let f = Fake::new(vec![]);
        assert!(check(&i, false, &f).unwrap_err().contains("no ingest key"));
        assert!(f.sent.borrow().is_empty());
        write(&d, DEFAULT_KEY_FILE, "file-key\n");
        assert!(check(&i, false, &f).is_ok());
    }

    #[test]
    fn missing_matrix_post_sends_nothing() {
        let d = home();
        let mut i = inputs(&d);
        i.matrix_post = Some(d.path().join("absent.sh"));
        let f = Fake::new(vec![]);
        assert!(check(&i, false, &f).unwrap_err().contains("MATRIX_POST"));
        let plain = d.path().join("plain.sh");
        std::fs::write(&plain, "x").unwrap();
        i.matrix_post = Some(plain);
        assert!(check(&i, false, &f).unwrap_err().contains("MATRIX_POST"));
        assert!(f.sent.borrow().is_empty());
    }

    #[test]
    fn rejected_key_401_sends_only_the_empty_probe() {
        let d = home();
        let f = Fake::new(vec![Ok((401, "invalid or revoked ingest key".into()))]);
        let e = check(&inputs(&d), false, &f).unwrap_err();
        assert!(e.contains("rejected") && !e.contains("TELEMETRY") && !e.contains("mail-key"));
        assert_eq!(*f.sent.borrow(), vec!["{}".to_string()]);
    }

    #[test]
    fn telemetry_key_is_named_on_401() {
        let d = home();
        write(&d, TELEMETRY_KEY, "mail-key\n");
        let f = Fake::new(vec![Ok((401, String::new()))]);
        let e = check(&inputs(&d), false, &f).unwrap_err();
        assert!(e.contains("TELEMETRY") && e.contains("POST /admin/hosts"));
        assert_eq!(*f.sent.borrow(), vec!["{}".to_string()]);
    }

    #[test]
    fn stale_deploy_reported_for_reply_to() {
        let d = home();
        let f = Fake::new(vec![
            Ok((400, String::new())),
            Ok((400, "key is required".into())),
        ]);
        let e = check(&inputs(&d), true, &f).unwrap_err();
        assert!(e.contains("predates replyTo"));
        assert!(f
            .sent
            .borrow()
            .iter()
            .all(|b| !b.contains("mail") || b.contains("preflight")));
        // A current build answers the nil thread with 404: fine.
        let f = Fake::new(vec![Ok((400, String::new())), Ok((404, "unknown".into()))]);
        assert!(check(&inputs(&d), true, &f).is_ok());
    }

    #[test]
    fn unreachable_inbox_is_a_refusal() {
        let d = home();
        let f = Fake::new(vec![Err("connect refused".into())]);
        assert!(check(&inputs(&d), false, &f)
            .unwrap_err()
            .contains("unreachable"));
    }
}
