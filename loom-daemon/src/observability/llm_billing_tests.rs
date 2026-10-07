use super::*;

fn attrs(b: &LlmBilling) -> TraceAttributes {
    let mut a = TraceAttributes::new();
    b.stamp(&mut a);
    a
}

#[test]
fn oauth_pool_claude_is_subscription() {
    let a = attrs(&LlmBilling::for_runtime("claude", None, false));
    assert_eq!(a["llm.billing"], "subscription");
    assert_eq!(a["llm.credential.kind"], "oauth-pool");
    assert!(!a.contains_key("llm.provider.profile"));
}

#[test]
fn codex_seat_is_subscription() {
    let a = attrs(&LlmBilling::for_runtime("codex", None, false));
    assert_eq!(a["llm.billing"], "subscription");
    assert_eq!(a["llm.credential.kind"], "chatgpt-seat");
}

#[test]
fn metered_backstop_is_api_key() {
    let a = attrs(&LlmBilling::for_runtime("claude", Some("quick-cerebras"), true));
    assert_eq!(a["llm.billing"], "api");
    assert_eq!(a["llm.credential.kind"], "api-key");
    assert_eq!(a["llm.provider.profile"], "quick-cerebras");
}

#[test]
fn zai_coding_plan_is_subscription_and_cerebras_is_api() {
    let zai = attrs(&LlmBilling::native(Some("zai-flash"), Some("subscription"), true, "pool"));
    assert_eq!(zai["llm.billing"], "subscription");
    assert_eq!(zai["llm.credential.kind"], "api-key");
    assert_eq!(zai["llm.provider.profile"], "zai-flash");
    // Undeclared + credentialed defaults to metered.
    let cerebras = attrs(&LlmBilling::native(Some("quick-cerebras"), None, true, "env"));
    assert_eq!(cerebras["llm.billing"], "api");
    assert_eq!(cerebras["llm.credential.kind"], "api-key");
    let gateway = attrs(&LlmBilling::native(Some("x"), Some("subscription"), true, "gateway"));
    assert_eq!(gateway["llm.billing"], "api");
}

#[test]
fn local_omits_credential_kind_and_unknown_is_never_guessed() {
    let local = attrs(&LlmBilling::native(Some("lm"), Some("local"), false, "none"));
    assert_eq!(local["llm.billing"], "local");
    assert!(!local.contains_key("llm.credential.kind"));
    let unknown = attrs(&LlmBilling::native(Some("k"), None, false, "none"));
    assert_eq!(unknown["llm.billing"], "unknown");
    assert!(!unknown.contains_key("llm.credential.kind"));
    assert_eq!(
        attrs(&LlmBilling::for_runtime("mystery", None, false))["llm.billing"],
        "unknown"
    );
}

#[test]
fn parse_rejects_free_text() {
    assert!(LlmBilling::parse("sk-secret", None, None).is_none());
    let b = LlmBilling::parse("api", Some("/home/x/token"), None).unwrap();
    assert_eq!(b.credential_kind, None);
}
