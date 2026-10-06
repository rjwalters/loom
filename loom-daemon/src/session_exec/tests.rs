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

/// #10364: the stale-mount refusal starts with the marker spawn-codex.sh
/// matches, exits 78, and names the account's own recreate command.
#[test]
fn mount_stale_line_carries_the_marker_and_the_recreate_command() {
    let state = serde_json::json!({"Config": {"Labels": {"loom.workspace": "/home/u/GitHub"}}});
    let line = host::mount_stale_line("loom-codex-session-agent-3", "/home/u/GitHub/new", &state);
    assert!(line.starts_with("# LOOM_SESSION_MOUNT_STALE container=loom-codex-session-agent-3 "));
    assert!(line.contains("workdir=/home/u/GitHub/new"));
    assert!(line.contains(
        "accounts session stop agent-3 && loom-daemon accounts session start agent-3 \
         --mount-workspace /home/u/GitHub"
    ));
    assert!(!line.contains('\n'));
    assert_eq!(host::MOUNT_STALE_EXIT, 78);
}
