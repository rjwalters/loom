//! An unprovisioned native credential is unavailable, not unobservable
//! (#11400): guarded role launches never reuse the harness's own login.
use super::availability::{availability, Availability, CredentialSource};
use super::resolve::Tap;
use super::tests::{fixture, ClearedRuntimeEnv};
use crate::role_runner::PoolHold;
use crate::runtime_admission::resolve_and_admit;
use std::path::Path;

/// Register one `zai` api-key account, enabled or not — the seat a bare
/// `opencode` (default `zai-flash`) tap needs to be selectable at all.
pub(super) fn provision_zai_pool(root: &Path, enabled: bool) {
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(root);
    if !pool.join("zai/alpha.env").exists() {
        crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
            .unwrap();
    }
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", enabled).unwrap();
}

/// [`fixture`] with an enabled `zai` seat, for tests whose metered backstop
/// tap is a bare `opencode` and must be selectable.
pub(super) fn backstop_fixture() -> tempfile::TempDir {
    let dir = fixture();
    provision_zai_pool(dir.path(), true);
    dir
}

/// A native tap whose provider is not pooled on this host, with its credential
/// variable unset, is passed over as unprovisioned (#11400): every preference
/// walk is for a role, role launches are guarded, and a guarded launch never
/// reuses the harness's own login — so it would fail at the provider on every
/// tick instead of falling through. The two ways a credential still reaches
/// it (an exported key, an external `LOOM_NATIVE_AUTH_FILE`) keep it ungated.
#[test]
#[serial_test::serial]
fn an_unpooled_native_provider_without_a_credential_is_unprovisioned() {
    let _env = ClearedRuntimeEnv::new();
    let dir = fixture();
    let admitted = resolve_and_admit(dir.path(), "builder", Some("opencode")).unwrap();
    let tap = Tap::runtime("opencode");
    // No `zai` API-key accounts are registered in this workspace.
    let state = availability(dir.path(), &tap, &admitted, 0);
    let Availability::Exhausted { hold, .. } = &state else {
        panic!("an unpooled, unexported credential must not be selectable: {state:?}");
    };
    assert_eq!(*hold, PoolHold::Unprovisioned);
    assert_eq!(
        state.source(),
        &CredentialSource::ApiKeys {
            provider: "zai".into()
        }
    );
    let summary = state.skip_reason().unwrap().summary();
    assert!(summary.contains("api_keys:zai"), "{summary}");

    // An external auth snapshot supplies the credential to a guarded launch.
    std::env::set_var("LOOM_NATIVE_AUTH_FILE", "/nonexistent/auth.json");
    let state = availability(dir.path(), &tap, &admitted, 0);
    std::env::remove_var("LOOM_NATIVE_AUTH_FILE");
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);

    // So does an exported key: the pool is never consulted.
    std::env::set_var("ZAI_API_KEY", "exported");
    let state = availability(dir.path(), &tap, &admitted, 0);
    std::env::remove_var("ZAI_API_KEY");
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);
}
