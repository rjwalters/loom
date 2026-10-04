//! `LOOM_RUNTIME` + `LOOM_CODEX_PROFILE_ROOT` isolation for the dispatch tests
//! (extracted from `tests.rs` for #9964).

/// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
/// scope of a test and restores whatever value (if any) it previously
/// had — including across a mid-test assertion panic, since Rust
/// unwinds through `Drop`. Some host/dev-container shells export
/// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
/// without this guard that ambient value silently outranks the
/// `runtimes.default` config precedence this test exercises (#4739).
///
/// It also points `LOOM_CODEX_PROFILE_ROOT` at an empty tempdir (#9964): a
/// codex-admitted role resolves the profile root, and these tests must never
/// read the host's real `~/.loom/codex-profiles`.
pub(super) struct ClearedLoomRuntimeEnv(
    Option<String>,
    #[allow(dead_code)] crate::tokens_pool::profile_root_env::ProfileRootEnv,
    #[allow(dead_code)] tempfile::TempDir,
);

impl ClearedLoomRuntimeEnv {
    pub(super) fn new() -> Self {
        let prior = std::env::var("LOOM_RUNTIME").ok();
        std::env::remove_var("LOOM_RUNTIME");
        let profiles = tempfile::tempdir().unwrap();
        let profile_root =
            crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());
        Self(prior, profile_root, profiles)
    }
}

impl Drop for ClearedLoomRuntimeEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_RUNTIME", v),
            None => std::env::remove_var("LOOM_RUNTIME"),
        }
    }
}
