//! `loom-daemon fleet-send` (issue #9517, epic #7810).
//!
//! Backs `defaults/scripts/fleet-send.sh` — the safehouse posting helper that
//! lifecycle role subagents (Builder / Judge / Doctor) call because their tool
//! allowlists exclude MCP tools and therefore cannot reach the session-injected
//! `safehouse_send` (issue #4199, phase 2 of #4196 / #3997). The shell version
//! was a bash+python re-implementation of the protocol in
//! [`crate::safehouse`] and called that module "the reference implementation";
//! this port deletes the copy and calls the real thing, so the daemon and the
//! role helper can never disagree about the wire format again.
//!
//! # Contract: exit 0 SILENTLY, always
//!
//! Inherited verbatim from the script and load-bearing for every caller:
//! absent env vars, an unresolvable socket, an invalid argument, or any
//! connect/hello/send failure ⇒ **exit 0 with no stdout and no stderr**.
//! "Posted" is best-effort; the room is optional, the role's work is not.
//! Every path here funnels to `Ok(())` and `main`'s exit code 0 — there is no
//! error branch anywhere in this module, by design, not by omission.
//!
//! That contract is also why the stub does NOT go through
//! `defaults/scripts/lib/script-helper.sh`: the helper's missing-daemon path is
//! an actionable loud error (exit 1), which is right for every other stub and
//! precisely wrong for the one script whose entire interface is silence. A
//! missing daemon is just the room being unavailable — the same degradation as
//! a missing socket. The stub therefore resolves through
//! `loom_resolve_self_daemon_bin` (this checkout's build in the source repo,
//! the installed daemon in a consumer repo) and discards the child's output.
//!
//! # Args: a parse failure is an invalid invocation, not a crash
//!
//! The shell parser shifted unknown arguments out and carried on ("never fail
//! a role over a typo"). clap cannot reproduce that on a subcommand —
//! `Command::ignore_errors` is a ROOT-level global setting (its docs: honored
//! from the top-level command down), and the root here is shared with every
//! other subcommand. So the port draws the line one step earlier: an unknown
//! flag fails clap's parse, and the stub swallows that (silent `exit 0`) like
//! every other failure. In practice the caller is a role prompt with a pinned
//! invocation, and a typo'd flag was ALREADY a silent no-op in the shell
//! (`--typ chat` shifted out ⇒ empty `--type` ⇒ local rejection); the only
//! delta is that an unknown flag can no longer ride along beside valid ones.
//! The hard contract — nothing printed, exit 0, the role unblocked — is
//! unchanged on every path.
//!
//! # One deliberate behavioral delta
//!
//! The shell's `--to` normalization folded case/hyphens and then sent the
//! result unvalidated; safehoused dropped unroutable lines server-side. The
//! port routes `--to` through the canonical [`crate::safehouse::normalize_to`],
//! so a `to` that would route nowhere is now rejected LOCALLY — same
//! observable outcome (nothing reaches the room, exit 0, silence), one fewer
//! dead line on the wire. Every other validation (type enum, task_id charset)
//! is unchanged and, as before, runs BEFORE the socket is opened, so an
//! invalid argument is never even a connection.

use anyhow::Result;
use loom_daemon::safehouse::{build_send_request, Envelope};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The script's closed envelope-v1 enum. Deliberately NOT
/// [`crate::safehouse::KNOWN_TYPES`], which also carries `completion` and
/// `digest`: the helper has rejected those locally for its whole life
/// (completion additionally requires a meta object the CLI cannot build), and
/// widening the script's surface is a behavior change, not a port.
const SCRIPT_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

/// How long any single socket operation may take. The shell used one
/// `settimeout(5.0)` for connect and I/O alike; the port keeps the budget and
/// spreads it over connect/read/write so no phase can silently exceed it.
const SOCKET_BUDGET: Duration = Duration::from_secs(5);

#[derive(clap::Args)]
pub(crate) struct FleetSendArgs {
    /// Task thread key (`<repo>_<issue>`, e.g. `loom_4199`, post-#4224), so
    /// role posts thread with the daemon's dispatch narration.
    #[arg(long, value_name = "ID")]
    pub(crate) task_id: Option<String>,

    /// Envelope type: chat | task | handoff | ack. The flag is `--type` —
    /// the script's invocation contract — even though the field carries
    /// safehouse's `kind` name.
    #[arg(long = "type", value_name = "TYPE")]
    pub(crate) kind: Option<String>,

    /// Body text — one concise line.
    #[arg(long, value_name = "TEXT")]
    pub(crate) body: Option<String>,

    /// Recipient: `*` (broadcast), a persona, or a `@matrix:id`.
    #[arg(long, value_name = "TO", default_value = "*")]
    pub(crate) to: String,

    /// Optional room override (the daemon routes; safehoused stamps `from`).
    #[arg(long, value_name = "NAME")]
    pub(crate) room: Option<String>,
}

impl FleetSendArgs {
    /// Never fails and never prints: `Ok(())` unconditionally, so `main`
    /// exits 0 whatever happened. See the module contract above.
    pub(crate) fn run(self) -> Result<()> {
        // Resolution mirrors the script: socket then persona, both from the
        // session env spawn-claude.sh exports; a plain shell without them is
        // the common case and the most common silent no-op.
        let socket = std::env::var("SAFEHOUSED_SOCKET")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("LOOM_SAFEHOUSE_SOCKET")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        let persona = std::env::var("SAFEHOUSE_PERSONA")
            .ok()
            .filter(|s| !s.is_empty());

        if let (Some(socket), Some(persona)) = (socket, persona) {
            if let Some(request) = self.build_request() {
                deliver(&socket, &persona, &request);
            }
        }
        Ok(())
    }

    /// Validate the invocation and build the `send` request — or `None` for
    /// "reject locally, no socket write", the script's wording. Validation
    /// runs before the connection exactly as the script's comment demanded.
    fn build_request(&self) -> Option<Value> {
        // Absent or empty body ⇒ silent no-op (the script's `[[ -z ]]`).
        let body = self.body.as_deref().filter(|b| !b.is_empty())?;
        // Closed type enum, rejected locally before anything else.
        let kind = self.kind.as_deref().filter(|k| SCRIPT_TYPES.contains(k))?;
        // Empty task_id means "absent" (the script omitted it); a present one
        // must be `[A-Za-z0-9_]` and an INVALID one rejects the whole
        // envelope locally (the script's `[[ =~ [^A-Za-z0-9_] ]] ⇒ exit 0`,
        // and test-fleet-send.sh's (d2) no-connection assertion) — dropping
        // just the task_id and sending anyway would be a silent behavior
        // change. `build_send_request` re-checks, but the script rejected it
        // locally and the port keeps that ordering.
        let task_id = match self.task_id.as_deref() {
            None | Some("") => None,
            Some(t) if t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                Some(t.to_owned())
            }
            Some(_) => return None,
        };
        // `--to ""` folded to broadcast in the shell; keep that.
        let to = if self.to.is_empty() {
            "*".to_owned()
        } else {
            self.to.clone()
        };
        let room = self.room.as_deref().filter(|r| !r.is_empty());

        let envelope = Envelope {
            to,
            kind: kind.to_owned(),
            task_id,
            body: body.to_owned(),
            meta: None,
        };
        // Canonical builder: type/task_id/`to` validation, `v: 1`, no `from`,
        // omitted `task_id`/`room` when absent — the exact shape safehoused
        // expects. A `None` here (bad `to`, mostly) is the local rejection.
        build_send_request(&envelope, 1, room).ok()
    }
}

/// The wire dance: connect, `hello` handshake, one `send`, best-effort reply
/// read, close. Every failure inside is a `return` — the caller's exit is
/// already decided. Mirrors the daemon's own client demultiplexing rule: async
/// push lines carry an `event` key and no `id`, so skip any line that has one.
fn deliver(socket: &str, persona: &str, request: &Value) {
    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(SOCKET_BUDGET));
    let _ = stream.set_write_timeout(Some(SOCKET_BUDGET));
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader);
    let mut writer = stream;

    // Mandatory first request; anything else is rejected by safehoused.
    let hello = json!({"id": 0, "op": "hello", "persona": persona});
    if write_line(&mut writer, &hello).is_err() {
        return;
    }
    if !read_reply(&mut reader).is_some_and(|r| r.get("ok") == Some(&json!(true))) {
        return;
    }

    if write_line(&mut writer, request).is_err() {
        return;
    }
    // Read the send reply so safehoused processes the line before we close —
    // best-effort, exactly like the script: the content is never inspected.
    let _ = read_reply(&mut reader);
}

/// One JSON object plus newline, flushed. `write!` + `flush` because the
/// peer's reply can only be trusted to follow a fully-sent line.
fn write_line(writer: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    writer.write_all(value.to_string().as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// First genuine reply line, skipping interleaved async push lines (an
/// `event` key and no `id`). `None` on EOF, timeout, or garbage — all
/// indistinguishable to the caller, all best-effort.
fn read_reply(reader: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if msg.get("event").is_some() && msg.get("id").is_none() {
            continue;
        }
        return Some(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::Receiver;

    /// Mock safehoused: accept one connection, optionally interleave an async
    /// push between the hello reply and the send reply, optionally reject the
    /// hello, and record every received line. Returns (socket path, record rx).
    fn mock_server(reject_hello: bool, interleave_push: bool) -> (String, Receiver<String>) {
        let dir = std::env::temp_dir().join(format!(
            "fleet-send-mock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("s.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((conn, _)) = listener.accept() {
                let mut reader = BufReader::new(&conn);
                let mut writer = &conn;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let msg: Value = serde_json::from_str(line.trim()).unwrap();
                    tx.send(line.trim().to_owned()).unwrap();
                    match msg.get("op").and_then(|o| o.as_str()) {
                        Some("hello") => {
                            let ok = !reject_hello;
                            let reply = json!({"id": 0, "ok": ok});
                            writer
                                .write_all((reply.to_string() + "\n").as_bytes())
                                .unwrap();
                            if interleave_push {
                                // An async narration push on the same
                                // connection: `event` key, no `id`.
                                let push = json!({"event": "narration", "body": "unrelated"});
                                writer
                                    .write_all((push.to_string() + "\n").as_bytes())
                                    .unwrap();
                            }
                            writer.flush().unwrap();
                        }
                        Some("send") => {
                            let reply = json!({"id": msg["id"], "ok": true});
                            writer
                                .write_all((reply.to_string() + "\n").as_bytes())
                                .unwrap();
                            writer.flush().unwrap();
                            break;
                        }
                        _ => break,
                    }
                    line.clear();
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        });
        (sock_path.to_string_lossy().to_string(), rx)
    }

    fn sample_request() -> Value {
        let args = FleetSendArgs {
            task_id: Some("loom_9517".to_owned()),
            kind: Some("handoff".to_owned()),
            body: Some("starting issue 9517".to_owned()),
            to: "*".to_owned(),
            room: None,
        };
        args.build_request().expect("sample request builds")
    }

    #[test]
    fn hello_precedes_send_and_both_carry_the_documented_fields() {
        let (sock, rx) = mock_server(false, false);
        deliver(&sock, "loom_builder_5", &sample_request());
        let hello = rx.recv_timeout(Duration::from_secs(5)).expect("hello");
        let send = rx.recv_timeout(Duration::from_secs(5)).expect("send");

        let hello: Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello["id"], 0);
        assert_eq!(hello["op"], "hello");
        assert_eq!(hello["persona"], "loom_builder_5");

        let send: Value = serde_json::from_str(&send).unwrap();
        assert_eq!(send["id"], 1);
        assert_eq!(send["op"], "send");
        assert_eq!(send["v"], 1);
        assert_eq!(send["to"], "*");
        assert_eq!(send["type"], "handoff");
        assert_eq!(send["task_id"], "loom_9517");
        assert_eq!(send["body"], "starting issue 9517");
        assert!(send.get("from").is_none(), "safehoused stamps from");
        // Serialization is compact serde_json; the byte layout is not part of
        // any contract, only the parsed shape is.
    }

    #[test]
    fn interleaved_async_pushes_are_skipped() {
        let (sock, rx) = mock_server(false, true);
        deliver(&sock, "loom_judge_1", &sample_request());
        let _hello = rx.recv_timeout(Duration::from_secs(5)).expect("hello");
        let send = rx.recv_timeout(Duration::from_secs(5)).expect("send");
        assert!(send.contains("\"op\":\"send\""));
    }

    #[test]
    fn hello_refusal_means_no_send() {
        let (sock, rx) = mock_server(true, false);
        deliver(&sock, "loom_builder_5", &sample_request());
        let _hello = rx.recv_timeout(Duration::from_secs(5)).expect("hello");
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a rejected hello must not be followed by a send"
        );
    }

    #[test]
    fn absent_socket_is_a_silent_noop() {
        let missing = std::env::temp_dir().join("fleet-send-does-not-exist.sock");
        // Must return (not panic, not block for more than the connect
        // failure) and write nothing anywhere.
        deliver(&missing.to_string_lossy(), "loom_builder_5", &sample_request());
    }

    #[test]
    fn build_request_rejects_invalid_inputs_locally() {
        let base = |kind: Option<&str>, task_id: Option<&str>, body: Option<&str>, to: &str| {
            FleetSendArgs {
                task_id: task_id.map(String::from),
                kind: kind.map(String::from),
                body: body.map(String::from),
                to: to.to_owned(),
                room: None,
            }
            .build_request()
        };

        // Closed enum: completion/digest were never the script's to send.
        assert!(base(Some("bogus"), Some("t"), Some("b"), "*").is_none());
        assert!(base(Some("completion"), Some("t"), Some("b"), "*").is_none());
        // task_id charset.
        assert!(base(Some("task"), Some("loom/9517 bad"), Some("b"), "*").is_none());
        // Empty body ⇒ no-op, not an empty envelope.
        assert!(base(Some("task"), Some("t"), Some(""), "*").is_none());
        // Invalid persona `to` — the one deliberate delta (see module doc).
        assert!(base(Some("task"), Some("t"), Some("b"), "not a persona!").is_none());
        // The happy shapes.
        assert!(base(Some("chat"), None, Some("b"), "*").is_some());
        assert!(base(Some("ack"), Some("loom_1"), Some("b"), "@robb:matrix.org").is_some());
        assert!(base(Some("task"), Some("t"), Some("b"), "Loom-Builder").is_some());
    }

    #[test]
    fn persona_to_is_normalized_like_the_daemon() {
        let args = FleetSendArgs {
            task_id: None,
            kind: Some("task".to_owned()),
            body: Some("b".to_owned()),
            to: "Loom-Builder".to_owned(),
            room: None,
        };
        let req = args.build_request().unwrap();
        assert_eq!(req["to"], "loom_builder");
    }
}
