use super::*;
use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;

struct Split {
    bytes: Vec<u8>,
    chunk: usize,
}
impl Read for Split {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.chunk.min(self.bytes.len()).min(out.len());
        out[..n].copy_from_slice(&self.bytes[..n]);
        self.bytes.drain(..n);
        Ok(n)
    }
}

#[test]
fn acknowledgement_does_not_change_worker_stderr_at_any_chunk_boundary() {
    let marker = ack("test-unique-invocation");
    let output = b"partial\xff stderr\nmore bytes";
    let mut input = output[..7].to_vec();
    input.extend(&marker);
    input.extend(&output[7..]);
    for chunk in 1..input.len() {
        let clean = AtomicBool::new(false);
        let mut result = Vec::new();
        host::forward_stderr(
            Split {
                bytes: input.clone(),
                chunk,
            },
            &mut result,
            None,
            &marker,
            &clean,
        )
        .unwrap();
        assert_eq!(result, output);
        assert!(clean.load(Ordering::Acquire));
    }
}

#[test]
fn another_invocation_cannot_acknowledge_cleanup() {
    let clean = AtomicBool::new(false);
    let other = ack("sibling");
    let mut result = Vec::new();
    host::forward_stderr(Cursor::new(&other), &mut result, None, &ack("ours"), &clean).unwrap();
    assert_eq!(result, other);
    assert!(!clean.load(Ordering::Acquire));
}

/// #10364: the stale-mount refusal tells the operator what is not mounted
/// where, and the account's own recreate command, on one line. It is prose:
/// the machine-readable cause is `refusal::announce`'s line.
#[test]
fn mount_stale_line_names_the_recreate_command() {
    let state = serde_json::json!({"Config": {"Labels": {"loom.workspace": "/home/u/GitHub"}}});
    let line = host::mount_stale_line("loom-codex-session-agent-3", "/home/u/GitHub/new", &state);
    assert!(line.starts_with(
        "session-exec: /home/u/GitHub/new is not mounted in loom-codex-session-agent-3"
    ));
    assert!(line.contains(
        "accounts session stop agent-3 && loom-daemon accounts session start agent-3 \
         --mount-workspace /home/u/GitHub"
    ));
    assert!(!line.contains('\n'));
    assert!(!line.starts_with('#'), "not a marker: {line}");
}

/// #10364, #10455: one `docker inspect` answers both pre-exec questions.
/// Whether the container is running fails closed, and a refusal is named
/// `SESSION_DOWN` only when docker said so: a failed inspect (no such
/// container) or an answer that is not a running container. A probe that was
/// abandoned (deadline, signal, launcher gone) says nothing about the
/// container, so it refuses unnamed, and never as a stale mount.
#[test]
fn preflight_names_a_refusal_only_when_docker_answered() {
    use host::{preflight, Preflight, Probed};
    assert_eq!(preflight(&Probed::Abandoned, "/w/repo"), Preflight::Unknown);
    assert_eq!(
        preflight(&Probed::Failed, "/w/repo"),
        Preflight::NotRunning,
        "no such container"
    );
    for answer in [
        "",
        "true",
        "false",
        "[]",
        "not json",
        r#"[{"State":{"Running":false},"Mounts":[{"Destination":"/w"}]}]"#,
        r#"[{"State":{"Running":true,"Restarting":true},"Mounts":[{"Destination":"/w"}]}]"#,
    ] {
        assert_eq!(
            preflight(&Probed::Answered(answer.into()), "/w/repo"),
            Preflight::NotRunning,
            "{answer:?}"
        );
    }
}

/// #10364: a running container is dispatched into only when one of its
/// mounts covers the workdir, component-wise; a private clone always is.
#[test]
fn preflight_reads_the_mounts_from_the_same_inspect() {
    use host::{Preflight, Probed};
    let preflight = |inspect: Option<&str>, workdir| {
        host::preflight(&Probed::Answered(inspect.unwrap().into()), workdir)
    };
    let running = |mounts: &str, labels: &str| {
        format!(
            r#"[{{"State":{{"Running":true}},"Config":{{"Labels":{labels}}},"Mounts":{mounts}}}]"#
        )
    };
    let host_mode = running(r#"[{"Type":"bind","Destination":"/w/repo"}]"#, "{}");
    assert_eq!(preflight(Some(&host_mode), "/w/repo"), Preflight::Ready);
    assert_eq!(preflight(Some(&host_mode), "/w/repo/.loom/worktrees/issue-1"), Preflight::Ready);
    for unmounted in ["/w/repository", "/w/new", "/w"] {
        assert!(
            matches!(preflight(Some(&host_mode), unmounted), Preflight::MountStale(_)),
            "{unmounted}"
        );
    }
    let private = running("[]", r#"{"loom.workspace-mode":"private-clone"}"#);
    assert_eq!(preflight(Some(&private), "/workspace/repo"), Preflight::Ready);
}
