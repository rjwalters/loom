//! The CLI half of `ReadIssueState` (#8876): socket trust checks, a bounded
//! one-frame client, and [`RoutedLookup`], which selects the route once.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::standalone;
use super::{IssueStateReply, ReadOutcome};
use crate::forge_egress::policy::PolicySources;
use crate::stash_retirement::IssueStateLookup;
use crate::types::{Request, Response};

/// Bound on the whole round trip (connect is local; the read is one `gh`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(70);

/// Largest reply frame accepted.
const MAX_REPLY_BYTES: u64 = 16 * 1024;

fn effective_uid() -> u32 {
    // SAFETY: geteuid only reads the caller's effective user id.
    unsafe { libc::geteuid() }
}

/// Why a socket was not trusted. Fixed vocabulary; never echoes file content.
pub fn check_socket_trust(socket: &Path, repo_root: &Path) -> Result<(), &'static str> {
    check_socket_trust_as(socket, repo_root, effective_uid())
}

/// [`check_socket_trust`] for an explicit `uid` (tests).
pub fn check_socket_trust_as(
    socket: &Path,
    repo_root: &Path,
    uid: u32,
) -> Result<(), &'static str> {
    let meta = std::fs::symlink_metadata(socket).map_err(|_| "socket-absent")?;
    if !meta.file_type().is_socket() {
        return Err("not-a-socket");
    }
    if meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err("socket-not-owner-only");
    }
    let parent = socket.parent().ok_or("socket-untrusted-location")?;
    let parent_meta = std::fs::metadata(parent).map_err(|_| "socket-untrusted-location")?;
    if parent_meta.uid() != uid || parent_meta.mode() & 0o022 != 0 {
        return Err("socket-dir-not-owner-only");
    }
    // A socket under the checkout is checkout-controlled by construction.
    let real_parent = std::fs::canonicalize(parent).map_err(|_| "socket-untrusted-location")?;
    let real_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    if real_parent.starts_with(&real_root) {
        return Err("socket-inside-checkout");
    }
    Ok(())
}

/// Send one `ReadIssueState` frame over `socket` and parse the reply.
pub fn ask_daemon(
    socket: &Path,
    root: &Path,
    slug: &str,
    issue: u64,
    timeout: Duration,
) -> Result<IssueStateReply, String> {
    let stream =
        UnixStream::connect(socket).map_err(|e| format!("connect failed: {}", e.kind()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.kind().to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.kind().to_string())?;
    let request = Request::ReadIssueState {
        workspace_root: root.display().to_string(),
        repo: slug.to_string(),
        issue,
    };
    let mut frame = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    frame.push('\n');
    let mut writer = &stream;
    writer
        .write_all(frame.as_bytes())
        .map_err(|e| format!("write failed: {}", e.kind()))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REPLY_BYTES))
        .read_line(&mut line)
        .map_err(|e| format!("read failed: {}", e.kind()))?;
    match serde_json::from_str::<Response>(&line) {
        Ok(Response::IssueState(reply)) => Ok(reply),
        Ok(_) => Err("unexpected reply".to_string()),
        Err(_) => Err("unparseable reply (daemon too old?)".to_string()),
    }
}

/// The transport a [`RoutedLookup`] asks first.
pub trait DaemonLink {
    fn ask(&self, issue: u64) -> Result<IssueStateReply, String>;
}

/// The real link: trusted socket, a slug and a registered-root candidate.
pub struct SocketLink {
    pub socket: PathBuf,
    pub root: PathBuf,
    pub slug: String,
}

impl DaemonLink for SocketLink {
    fn ask(&self, issue: u64) -> Result<IssueStateReply, String> {
        check_socket_trust(&self.socket, &self.root).map_err(str::to_string)?;
        ask_daemon(&self.socket, &self.root, &self.slug, issue, REQUEST_TIMEOUT)
    }
}

/// The standalone side, injectable for tests.
pub trait StandaloneRead {
    fn read(&self, issue: u64) -> standalone::StandaloneRead;
}

/// Production standalone read: policy sources resolved once per run.
pub struct LocalRead {
    pub root: PathBuf,
    pub sources: PolicySources,
}

impl StandaloneRead for LocalRead {
    fn read(&self, issue: u64) -> standalone::StandaloneRead {
        standalone::read(&self.root, &self.sources, issue)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chosen {
    Daemon,
    Standalone,
}

/// An [`IssueStateLookup`] that selects its route once, on the first lookup.
///
/// - The daemon answered `Open`/`Closed`/`Unknown`: the daemon is the route
///   for the whole run. An `Unknown` from it is final; there is no retry
///   through ambient credentials.
/// - The daemon is down, untrusted, too old, or answered `NotManaged`: the
///   run uses the standalone read (itself behind the admission gate).
pub struct RoutedLookup<'a> {
    link: Option<&'a dyn DaemonLink>,
    local: &'a dyn StandaloneRead,
    chosen: Option<Chosen>,
    fallback_reason: Option<String>,
    route: String,
    cache: HashMap<u64, Option<bool>>,
}

impl<'a> RoutedLookup<'a> {
    #[must_use]
    pub fn new(link: Option<&'a dyn DaemonLink>, local: &'a dyn StandaloneRead) -> Self {
        Self {
            link,
            local,
            chosen: None,
            fallback_reason: link.is_none().then(|| "no-daemon-link".to_string()),
            route: "unresolved".to_string(),
            cache: HashMap::new(),
        }
    }

    /// The route actually used, plus why the daemon was not (for stderr).
    #[must_use]
    pub fn route_description(&self) -> String {
        match (&self.fallback_reason, self.chosen) {
            (Some(why), Some(Chosen::Standalone)) => {
                format!("{} (daemon not used: {why})", self.route)
            }
            _ => self.route.clone(),
        }
    }

    fn standalone(&mut self, issue: u64) -> Option<bool> {
        self.chosen = Some(Chosen::Standalone);
        let read = self.local.read(issue);
        self.route = read.route;
        read.outcome.is_closed()
    }

    fn query(&mut self, issue: u64) -> Option<bool> {
        if self.chosen == Some(Chosen::Standalone) {
            return self.standalone(issue);
        }
        let Some(link) = self.link else {
            return self.standalone(issue);
        };
        match link.ask(issue) {
            Ok(reply) if reply.outcome == ReadOutcome::NotManaged => {
                if self.chosen == Some(Chosen::Daemon) {
                    return None;
                }
                self.fallback_reason = Some("repo-not-managed".to_string());
                self.standalone(issue)
            }
            Ok(reply) => {
                self.chosen = Some(Chosen::Daemon);
                self.route = reply.route.clone();
                reply.outcome.is_closed()
            }
            Err(why) if self.chosen.is_none() => {
                self.fallback_reason = Some(why);
                self.standalone(issue)
            }
            Err(_) => None,
        }
    }
}

impl IssueStateLookup for RoutedLookup<'_> {
    fn is_closed(&mut self, issue: u64) -> Option<bool> {
        if let Some(cached) = self.cache.get(&issue) {
            return *cached;
        }
        let answer = self.query(issue);
        self.cache.insert(issue, answer);
        answer
    }
}
