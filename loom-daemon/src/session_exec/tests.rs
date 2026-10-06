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

/// #10455: `SESSION_DOWN` is announced only when docker answered the
/// running-probe. A probe that was abandoned (deadline, signal, launcher gone)
/// says nothing about the container.
#[test]
fn only_an_answered_or_failed_running_probe_says_not_running() {
    use host::Probed;
    assert!(!Probed::Answered("true".into()).says_not_running());
    assert!(Probed::Answered("false".into()).says_not_running());
    assert!(Probed::Answered(String::new()).says_not_running());
    assert!(Probed::Failed.says_not_running(), "no such container");
    assert!(!Probed::Abandoned.says_not_running(), "the container may be running");
}
