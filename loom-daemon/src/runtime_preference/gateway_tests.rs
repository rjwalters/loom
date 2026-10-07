//! #9473: a native tap whose model profile is routed through the host's LLM
//! gateway is not gated on its API-key pool, because the launch never
//! consults the pool: the gateway's virtual key replaces it. Mirrors
//! `tests::a_pooled_native_provider_is_gated_on_its_api_key_pool`.
use super::availability::{availability, CredentialSource};
use super::resolve::Tap;
use super::tests::{fixture, ClearedRuntimeEnv};
use crate::runtime_admission::resolve_and_admit;

/// Sets (or clears) process variables for one test and restores them on drop.
struct ScopedVars(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl ScopedVars {
    fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let prior = vars
            .iter()
            .map(|(key, value)| {
                let prior = std::env::var_os(key);
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
                (*key, prior)
            })
            .collect();
        Self(prior)
    }
}
impl Drop for ScopedVars {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn a_gateway_routed_native_profile_is_not_gated_on_its_api_key_pool() {
    let _env = ClearedRuntimeEnv::new();
    // The pool is the wall only while the source variable is unexported.
    let _unexported = ScopedVars::new(&[("ZAI_API_KEY", None), ("LOOM_LLM_GATEWAY_URL", None)]);
    let dir = fixture();
    let admitted = resolve_and_admit(dir.path(), "builder", Some("opencode")).unwrap();
    let tap = Tap::runtime("opencode");
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
        .unwrap();
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", false).unwrap();
    // Control: unrouted, the all-disabled pool passes the tap over.
    assert!(!availability(dir.path(), &tap, &admitted, 0).is_spawnable());

    let routed = ScopedVars::new(&[
        ("LOOM_LLM_GATEWAY_URL", Some("http://llm-gateway.example.net:8080/v1")),
        ("LOOM_LLM_GATEWAY_PROFILES", Some("zai-flash")),
        ("LOOM_LLM_GATEWAY_VK", Some("sk-bf-availability-fixture")),
    ]);
    let state = availability(dir.path(), &tap, &admitted, 0);
    drop(routed);
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);
}
