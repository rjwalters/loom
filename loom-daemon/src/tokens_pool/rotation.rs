//! Rotation cursor for one-per-account distribution of concurrent dispatches,
//! ported from `loom_tools.tokens.rotation` (issue #3909).
//!
//! A monotonic counter persisted at `.loom/tokens/.rotation_cursor` under a
//! [`super::locking::MkdirLock`]. Each call reads the counter (initializing
//! to a random offset the first time so sibling repos sharing an account
//! pool but not this cursor file de-correlate), computes `counter % modulus`,
//! and writes `counter + 1` back — see the Python docstring for the full
//! rationale.

use std::path::Path;

use super::locking::MkdirLock;
use super::rng::Rng;

/// Keep the initial random offset comfortably above any realistic account
/// count so the first modulo is uniformly distributed.
const INIT_OFFSET_BOUND: u64 = 1 << 30;

fn cursor_path(tokens_dir: &Path) -> std::path::PathBuf {
    tokens_dir.join(".rotation_cursor")
}

fn lock_path(tokens_dir: &Path) -> std::path::PathBuf {
    tokens_dir.join(".rotation_cursor.lock")
}

fn read_counter(cursor_file: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(cursor_file).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Return the next round-robin index in `[0, modulus)` and advance the
/// cursor. `modulus <= 1` returns `0` without touching the cursor file (a
/// single/empty window has nothing to rotate across).
///
/// On any lock-timeout or I/O failure this degrades gracefully to a random
/// index rather than propagating an error — selection must never hard-fail
/// on cursor bookkeeping while accounts remain available.
pub fn next_rotation_index(tokens_dir: &Path, modulus: usize, rng: &mut Rng) -> usize {
    if modulus <= 1 {
        return 0;
    }

    let cursor_file = cursor_path(tokens_dir);
    match MkdirLock::acquire(&lock_path(tokens_dir)) {
        Ok(_lock) => {
            let counter = read_counter(&cursor_file)
                .unwrap_or_else(|| rng.gen_range(INIT_OFFSET_BOUND as usize) as u64);
            let index = (counter % modulus as u64) as usize;
            let _ = std::fs::write(&cursor_file, (counter + 1).to_string());
            index
        }
        Err(_) => rng.gen_range(modulus),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modulus_one_returns_zero_without_cursor_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rng = Rng::seeded(1);
        assert_eq!(next_rotation_index(tmp.path(), 1, &mut rng), 0);
        assert!(!cursor_path(tmp.path()).exists());
    }

    #[test]
    fn modulus_zero_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rng = Rng::seeded(1);
        assert_eq!(next_rotation_index(tmp.path(), 0, &mut rng), 0);
    }

    #[test]
    fn rotates_one_per_account_across_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rng = Rng::seeded(5);
        // Pre-seed the cursor at a known value so the sequence is
        // deterministic and doesn't depend on the random initial offset.
        std::fs::write(cursor_path(tmp.path()), "0").unwrap();
        let modulus = 3;
        let indices: Vec<usize> = (0..7)
            .map(|_| next_rotation_index(tmp.path(), modulus, &mut rng))
            .collect();
        assert_eq!(indices, vec![0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn adapts_when_modulus_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rng = Rng::seeded(5);
        std::fs::write(cursor_path(tmp.path()), "4").unwrap();
        // counter=4, modulus=3 -> index 1, counter becomes 5.
        assert_eq!(next_rotation_index(tmp.path(), 3, &mut rng), 1);
        // counter=5, modulus=2 -> index 1.
        assert_eq!(next_rotation_index(tmp.path(), 2, &mut rng), 1);
    }

    #[test]
    fn malformed_cursor_falls_back_to_random_seed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(cursor_path(tmp.path()), "not-a-number").unwrap();
        let mut rng = Rng::seeded(1);
        let idx = next_rotation_index(tmp.path(), 4, &mut rng);
        assert!(idx < 4);
        // The cursor should have been rewritten to a valid integer.
        let contents = std::fs::read_to_string(cursor_path(tmp.path())).unwrap();
        assert!(contents.trim().parse::<u64>().is_ok());
    }
}
