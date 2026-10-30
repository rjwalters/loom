//! Claim refusals report back (#12318, under #12304): a claim refused because
//! the drawn snapshot was stale tells the mirror to re-read that one item
//! (`POST /v1/mirror/recheck {repo, number}`).
//!
//! The daemon is a polite client. A recheck is fire-and-forget (the tick
//! never waits on it), deduplicated per `(repo, number)` per tick, never
//! retried, and bounded by a local token bucket at the mirror's per-caller
//! default (60 an hour), so an excess is dropped here, not at the mirror. It
//! carries no state: the mirror re-reads GitHub itself. It only runs in
//! mirror mode (`forge.mirror.discovery = worklist`), and nothing depends on
//! it having happened. It is issued from the step driver's refusal path only.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::forge_mirror::client::MirrorError;
use crate::observability::ops::discovery_recheck;

/// The mirror's per-caller default, per hour.
pub const HOURLY_LIMIT: u32 = 60;

const PATH: &str = "/v1/mirror/recheck";

/// Whether a refusal means "GitHub disagrees with what I drew". `claim_lost`
/// is a peer's earlier lease (a race), and the rest are host-side.
#[must_use]
pub fn is_stale_signal(reason: &str, detail: &str) -> bool {
    match reason {
        "claimed" => true,
        // Only when the live state differs: the item is gone or closed, not
        // a transient `gh` failure.
        "label_flip_failed" => {
            let d = detail.to_ascii_lowercase();
            d.contains("could not resolve to an issue")
                || d.contains("not found")
                || d.contains("is closed")
        }
        _ => false,
    }
}

/// Whole tokens, refilled continuously.
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn take(&mut self, now: Instant) -> bool {
        let rate = f64::from(HOURLY_LIMIT) / Duration::from_secs(3600).as_secs_f64();
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(f64::from(HOURLY_LIMIT));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// How a recheck request is sent; the answer arrives on the mirror runtime.
type Sender = dyn Fn(&str, u32, Box<dyn FnOnce(Result<(), MirrorError>) + Send>) + Send + Sync;

/// The recheck state across ticks: the hourly bucket, and the current tick's
/// already-asked items.
pub struct Recheck {
    bucket: Mutex<Bucket>,
    seen: Mutex<HashSet<(String, u32)>>,
    sender: Box<Sender>,
}

impl Default for Recheck {
    fn default() -> Self {
        Self::with_sender(Box::new(|repo, number, done| match crate::forge_mirror::active() {
            Some(active) => crate::forge_mirror::client::post_json_detached(
                &active,
                PATH,
                json!({ "repo": repo, "number": number }),
                done,
            ),
            None => done(Err(MirrorError::Transport("no mirror".into()))),
        }))
    }
}

impl Recheck {
    #[must_use]
    pub fn with_sender(sender: Box<Sender>) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                tokens: f64::from(HOURLY_LIMIT),
                last: Instant::now(),
            }),
            seen: Mutex::new(HashSet::new()),
            sender,
        }
    }

    /// A new tick: the dedup set starts empty.
    pub fn begin_tick(&self) {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Report a refusal for `(repo, number)`. Returns whether a request was
    /// issued. Never blocks on the network.
    pub fn report(&self, repo: &str, number: u32, reason: &'static str, detail: &str) -> bool {
        if !is_stale_signal(reason, detail) {
            return false;
        }
        let fresh = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((repo.to_string(), number));
        if !fresh {
            return false;
        }
        let allowed = self
            .bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(Instant::now());
        if !allowed {
            discovery_recheck::record(reason, "skipped");
            return false;
        }
        (self.sender)(
            repo,
            number,
            Box::new(move |res| {
                discovery_recheck::record(reason, if res.is_ok() { "sent" } else { "error" });
            }),
        );
        true
    }
}

/// The shared handle the driver holds.
pub type Shared = Arc<Recheck>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge_mirror::Active;
    use serial_test::serial;
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    type Calls = Arc<Mutex<Vec<(String, u32)>>>;

    fn recording() -> (Recheck, Calls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let c = calls.clone();
        let r = Recheck::with_sender(Box::new(move |repo, n, done| {
            c.lock().unwrap().push((repo.to_string(), n));
            done(Ok(()));
        }));
        (r, calls)
    }

    #[test]
    fn a_claimed_refusal_produces_exactly_one_recheck_per_tick() {
        let (r, calls) = recording();
        r.begin_tick();
        assert!(r.report("o/a", 7, "claimed", "lock held"));
        assert!(!r.report("o/a", 7, "claimed", "lock held"));
        assert!(r.report("o/a", 8, "claimed", ""));
        assert!(r.report("o/b", 7, "claimed", ""));
        assert_eq!(calls.lock().unwrap().len(), 3);
        // The next tick may ask again.
        r.begin_tick();
        assert!(r.report("o/a", 7, "claimed", ""));
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn claim_lost_and_host_side_refusals_produce_none() {
        let (r, calls) = recording();
        r.begin_tick();
        for reason in [
            "claim_lost",
            "claim_unverified",
            "admission_refused",
            "pool_exhausted",
            "no_token_pool",
            "roll_paused",
            "runtime_rejected",
            "spawn_failed",
            "workspace_commands_missing",
            "model_runtime_mismatch",
        ] {
            assert!(!r.report("o/a", 1, reason, ""), "{reason}");
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_label_flip_failure_rechecks_only_when_the_item_moved() {
        let (r, calls) = recording();
        r.begin_tick();
        assert!(!r.report("o/a", 1, "label_flip_failed", "gh issue edit failed: timeout"));
        assert!(r.report(
            "o/a",
            2,
            "label_flip_failed",
            "GraphQL: Could not resolve to an issue with the number of 2"
        ));
        assert_eq!(calls.lock().unwrap().as_slice(), &[("o/a".to_string(), 2)]);
    }

    #[test]
    fn the_hourly_limit_is_a_local_token_bucket() {
        let (r, calls) = recording();
        r.begin_tick();
        for n in 0..HOURLY_LIMIT + 10 {
            r.report("o/a", n, "claimed", "");
        }
        assert_eq!(calls.lock().unwrap().len(), HOURLY_LIMIT as usize);
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let t = Instant::now();
        let mut b = Bucket {
            tokens: 0.0,
            last: t,
        };
        assert!(!b.take(t));
        assert!(b.take(t + Duration::from_secs(61)));
    }

    #[test]
    fn a_slow_sender_does_not_extend_the_caller() {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce(Result<(), MirrorError>) + Send>>();
        let tx = Mutex::new(tx);
        let r = Recheck::with_sender(Box::new(move |_, _, done| {
            tx.lock().unwrap().send(done).unwrap();
        }));
        r.begin_tick();
        let started = Instant::now();
        assert!(r.report("o/a", 1, "claimed", ""));
        assert!(started.elapsed() < Duration::from_millis(200));
        // The answer lands later, on its own.
        rx.recv().unwrap()(Err(MirrorError::Status(429)));
    }

    /// A loopback mirror answering every POST with `status`; counts requests.
    fn mirror(status: u16) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (n_t, heads_t) = (n.clone(), heads.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = vec![0u8; 8192];
                let len = stream.read(&mut buf).unwrap_or(0);
                heads_t
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..len]).into_owned());
                n_t.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    )
                    .as_bytes(),
                );
            }
        });
        (format!("http://{addr}"), n, heads)
    }

    fn post(base: &str) -> Result<(), MirrorError> {
        let active = Active::for_test(base, Duration::from_secs(60));
        let (tx, rx) = mpsc::channel();
        crate::forge_mirror::client::post_json_detached(
            &active,
            PATH,
            json!({ "repo": "o/a", "number": 5 }),
            move |res| {
                let _ = tx.send(res);
            },
        );
        rx.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    #[test]
    #[serial]
    fn a_post_carries_the_bearer_key_and_the_item() {
        let (base, n, heads) = mirror(200);
        assert_eq!(post(&base), Ok(()));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        let head = heads.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /v1/mirror/recheck "), "{head}");
        assert!(head.contains("2ge_test_key_0123456789abcdef"));
        assert!(head.contains(r#""repo":"o/a""#) && head.contains(r#""number":5"#));
    }

    #[test]
    #[serial]
    fn a_429_and_a_403_are_errors_and_are_not_retried() {
        let (base, n, _) = mirror(429);
        assert_eq!(post(&base), Err(MirrorError::Status(429)));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(n.load(Ordering::SeqCst), 1, "no retry");
        let (base, n, _) = mirror(403);
        assert_eq!(post(&base), Err(MirrorError::Auth(403)));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(n.load(Ordering::SeqCst), 1, "no retry");
        // A dead mirror is an error too.
        let dead = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert!(matches!(post(&dead), Err(MirrorError::Transport(_))));
    }

    /// The recheck is issued from the driver's refusal path only.
    #[test]
    fn the_call_site_is_the_drivers_refusal_path_only() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = std::collections::BTreeSet::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(&src)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let text = std::fs::read_to_string(&path).unwrap();
                    if rel != "discovery/recheck.rs" && text.contains(".report(&repo,") {
                        hits.insert(rel);
                    }
                }
            }
        }
        assert_eq!(
            hits.into_iter().collect::<Vec<_>>(),
            vec!["step_dispatch/driver.rs".to_string()]
        );
    }
}
