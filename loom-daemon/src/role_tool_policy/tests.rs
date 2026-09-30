//! Parity tests for the per-role tool-restriction port (#8322, for #8256).
//!
//! These assert the emitted spec strings **literally** rather than deriving
//! them from the same constants the implementation uses. That is deliberate:
//! the requirement is byte-for-byte parity with `_loom_role_deny_specs()` in
//! `spawn-claude.sh`, and a test that re-derives the answer from the code
//! under test cannot detect a transcription error in either direction.
//!
//! The one place that parity is deliberately broken is the wildcard rule
//! (#8943) — see the "The ONE wildcard rule" section below, and the module
//! doc's section of the same name for why.

use super::*;

/// Build a policy directly from an allowlist, skipping the filesystem.
fn policy(role: &str, allowlist: Allowlist) -> RoleToolPolicy {
    RoleToolPolicy {
        role: role.to_string(),
        source: Some(PathBuf::from("/tmp/roles/test.json")),
        allowlist,
    }
}

fn declared(caps: &[&str]) -> Allowlist {
    Allowlist::Declared(caps.iter().map(|s| (*s).to_string()).collect())
}

// ---------------------------------------------------------------------------
// Role-name resolution (`_loom_role_policy_name()` parity)
// ---------------------------------------------------------------------------

#[test]
fn plain_role_names_pass_through() {
    for name in ["builder", "curator", "judge", "doctor", "guide"] {
        assert_eq!(resolve_role_name(name).as_deref(), Some(name));
    }
}

#[test]
fn the_three_dispatch_aliases_resolve() {
    // These three are the daemon's dispatch names. Missing one means a
    // daemon-dispatched session reaches a DIFFERENT policy than the guard
    // backstop keyed on the same LOOM_ROLE — the drift the aliases exist to
    // prevent.
    assert_eq!(resolve_role_name("development-worker").unwrap(), "builder");
    assert_eq!(resolve_role_name("pr-fixer").unwrap(), "doctor");
    assert_eq!(resolve_role_name("sweep-lifecycle").unwrap(), "builder");
}

#[test]
fn aliases_resolve_after_case_and_underscore_folding() {
    // `tr '[:upper:]_' '[:lower:]-'` runs BEFORE the alias case, so these are
    // the same three aliases arriving in the shapes a caller actually types.
    assert_eq!(resolve_role_name("Development_Worker").unwrap(), "builder");
    assert_eq!(resolve_role_name("DEVELOPMENT-WORKER").unwrap(), "builder");
    assert_eq!(resolve_role_name("PR_Fixer").unwrap(), "doctor");
    assert_eq!(resolve_role_name("Sweep_Lifecycle").unwrap(), "builder");
}

#[test]
fn case_and_underscores_fold_for_non_aliases_too() {
    assert_eq!(resolve_role_name("Builder").unwrap(), "builder");
    assert_eq!(resolve_role_name("MAIN_HEALTH").unwrap(), "main-health");
}

#[test]
fn whitespace_is_deleted_not_collapsed() {
    // `tr -d '[:space:]'` DELETES; it does not replace with a separator.
    assert_eq!(resolve_role_name("  builder \n").unwrap(), "builder");
    assert_eq!(resolve_role_name("\tBuild er\r").unwrap(), "builder");
    // Every POSIX [:space:] character, including the vertical tab.
    assert_eq!(resolve_role_name("bu\u{0b}il\u{0c}der").unwrap(), "builder");
    // Consequence worth pinning: a SPACE-separated alias is not an alias.
    assert_eq!(resolve_role_name("pr fixer").unwrap(), "prfixer");
}

#[test]
fn names_containing_a_slash_are_rejected() {
    // The value lands in a filesystem path, so a separator never gets there.
    for raw in ["../doctor", "a/b", "/etc/passwd", "roles/builder"] {
        assert_eq!(resolve_role_name(raw), None, "should reject {raw:?}");
    }
}

#[test]
fn names_containing_a_dot_are_rejected() {
    // `*.*` subsumes the leading-dot case, which is why the shell has no
    // separate `.*` pattern — pin both shapes so a refactor cannot drop one.
    for raw in ["builder.json", "..", ".", ".hidden", "a.b"] {
        assert_eq!(resolve_role_name(raw), None, "should reject {raw:?}");
    }
}

#[test]
fn empty_and_whitespace_only_names_are_rejected() {
    assert_eq!(resolve_role_name(""), None);
    assert_eq!(resolve_role_name("   \t\n"), None);
}

#[test]
fn an_alias_target_is_not_re_rejected() {
    // Regression guard on ordering: rejection runs AFTER aliasing, and no
    // alias target contains `/` or `.`, so all three must survive it.
    for raw in ["development-worker", "pr-fixer", "sweep-lifecycle"] {
        assert!(resolve_role_name(raw).is_some(), "{raw} was rejected");
    }
}

// ---------------------------------------------------------------------------
// Allowlist parsing: undeclared vs. declared-empty
// ---------------------------------------------------------------------------

#[test]
fn a_declared_empty_allowlist_is_not_undeclared() {
    // THE distinction this whole port turns on. Both render as "" once
    // joined; conflating them makes a read-only role unrestricted (or a
    // policy-less role maximally restricted), and neither failure is visible
    // from the joined string alone.
    let empty = parse_allowlist(r#"{"toolPolicy": {"allowedCapabilities": []}}"#);
    assert_eq!(empty, Allowlist::Declared(vec![]));
    assert!(empty.is_declared());

    let absent = parse_allowlist(r#"{"name": "curator"}"#);
    assert_eq!(absent, Allowlist::Undeclared);
    assert!(!absent.is_declared());

    assert_ne!(empty, absent);
}

#[test]
fn undeclared_covers_every_unreadable_shape() {
    for doc in [
        "",                         // empty file
        "not json at all",          // unparseable
        "[1, 2, 3]",                // not an object
        r#""a string""#,            // not an object
        "{}",                       // no toolPolicy
        r#"{"toolPolicy": null}"#,  // explicit null
        r#"{"toolPolicy": "yes"}"#, // scalar toolPolicy
        r#"{"toolPolicy": []}"#,    // array toolPolicy
        r#"{"toolPolicy": {}}"#,    // no allowedCapabilities
        r#"{"toolPolicy": {"allowedCapabilities": null}}"#,
        r#"{"toolPolicy": {"allowedCapabilities": "*"}}"#, // string, not array
        r#"{"toolPolicy": {"allowedCapabilities": {}}}"#,
    ] {
        assert_eq!(parse_allowlist(doc), Allowlist::Undeclared, "should be undeclared: {doc}");
    }
}

#[test]
fn non_string_elements_are_dropped_like_jqs_select() {
    let parsed = parse_allowlist(
        r#"{"toolPolicy": {"allowedCapabilities": ["cloud-cli", 7, null, {"a": 1}, "remote-shell"]}}"#,
    );
    assert_eq!(parsed, declared(&["cloud-cli", "remote-shell"]));
}

#[test]
fn other_role_json_keys_are_ignored() {
    let parsed = parse_allowlist(
        r#"{"name": "judge", "model": "opus", "toolPolicy": {"allowedCapabilities": ["forge-secrets"], "other": 1}}"#,
    );
    assert_eq!(parsed, declared(&["forge-secrets"]));
}

// ---------------------------------------------------------------------------
// Deny specs: the four capabilities, asserted literally
// ---------------------------------------------------------------------------

#[test]
fn remote_shell_specs_match_the_shell_byte_for_byte() {
    assert_eq!(
        deny_specs_for("remote-shell"),
        [
            "Bash(ssh:*)",
            "Bash(scp:*)",
            "Bash(sftp:*)",
            "Bash(ssh-add:*)",
            "Bash(ssh-agent:*)",
            "Bash(ssh-keygen:*)",
            "Bash(ssh-keyscan:*)",
            "Bash(ssh-copy-id:*)",
            "Bash(autossh:*)",
        ]
    );
}

#[test]
fn cloud_cli_specs_match_the_shell_byte_for_byte() {
    assert_eq!(
        deny_specs_for("cloud-cli"),
        [
            "Bash(aws:*)",
            "Bash(gcloud:*)",
            "Bash(az:*)",
            "Bash(doctl:*)",
            "Bash(flyctl:*)",
            "Bash(fly:*)",
            "Bash(wrangler:*)",
            "Bash(heroku:*)",
            "Bash(kubectl:*)",
            "Bash(eksctl:*)",
        ]
    );
}

#[test]
fn forge_secrets_specs_match_the_shell_byte_for_byte() {
    assert_eq!(
        deny_specs_for("forge-secrets"),
        [
            "Bash(gh secret:*)",
            "Bash(gh variable:*)",
            "Bash(gh auth token:*)",
            "Bash(gh auth login:*)",
            "Bash(gh auth refresh:*)",
            "Bash(gh auth logout:*)",
            "Bash(gh auth setup-git:*)",
        ]
    );
}

#[test]
fn forge_secrets_never_denies_gh_auth_status() {
    // Deliberate omission, not an oversight — every role runs it, so denying
    // it would break the fleet rather than restrict it.
    assert!(!deny_specs_for("forge-secrets")
        .iter()
        .any(|s| s.contains("gh auth status")));
}

#[test]
fn credential_store_specs_match_the_shell_byte_for_byte() {
    assert_eq!(
        deny_specs_for("credential-store"),
        [
            "Read(//~/.ssh/**)",
            "Edit(//~/.ssh/**)",
            "Write(//~/.ssh/**)",
            "Read(//~/.aws/**)",
            "Edit(//~/.aws/**)",
            "Write(//~/.aws/**)",
            "Read(//~/.gnupg/**)",
            "Edit(//~/.gnupg/**)",
            "Write(//~/.gnupg/**)",
            "Read(//~/.config/gh/**)",
            "Edit(//~/.config/gh/**)",
            "Write(//~/.config/gh/**)",
        ]
    );
}

#[test]
fn an_unknown_capability_has_no_specs() {
    // An unknown string in a role JSON is inert, never a wildcard.
    for name in ["", "database", "REMOTE-SHELL", "remote_shell"] {
        assert!(deny_specs_for(name).is_empty(), "{name} produced specs");
    }
}

// ---------------------------------------------------------------------------
// The four-capability matrix crossed with the four allowlist shapes
// ---------------------------------------------------------------------------

#[test]
fn undeclared_denies_nothing() {
    let p = policy("curator", Allowlist::Undeclared);
    assert!(p.denied_capabilities().is_empty());
    assert!(p.deny_specs().is_empty());
    assert!(!p.is_restricted());
}

#[test]
fn declared_empty_denies_the_entire_namespace() {
    let p = policy("curator", declared(&[]));
    assert_eq!(
        p.denied_capabilities(),
        [
            "remote-shell",
            "cloud-cli",
            "forge-secrets",
            "credential-store"
        ]
    );
    // 9 + 10 + 7 + 12 — the full spec set, in namespace order.
    let specs = p.deny_specs();
    assert_eq!(specs.len(), 38);
    assert_eq!(specs.first().copied(), Some("Bash(ssh:*)"));
    assert_eq!(specs.last().copied(), Some("Write(//~/.config/gh/**)"));
    assert!(p.is_restricted());
}

#[test]
fn wildcard_denies_nothing() {
    let p = policy("builder", declared(&["*"]));
    assert!(p.denied_capabilities().is_empty());
    assert!(p.deny_specs().is_empty());
    // …and on the Codex path it is likewise not "restricted", so no warning.
    assert!(!p.is_restricted());
}

#[test]
fn a_wildcard_alongside_named_capabilities_still_denies_nothing() {
    let p = policy("builder", declared(&["cloud-cli", "*"]));
    assert!(p.deny_specs().is_empty());
    assert!(!p.is_restricted());
}

#[test]
fn each_capability_granted_alone_denies_exactly_the_other_three() {
    for granted in CAPABILITY_NAMESPACE {
        let p = policy("role", declared(&[granted]));
        let denied = p.denied_capabilities();
        assert_eq!(denied.len(), 3, "granting {granted}");
        assert!(!denied.contains(&granted), "{granted} was denied to itself");

        // Namespace ORDER is preserved regardless of which one was granted.
        let expected: Vec<&str> = CAPABILITY_NAMESPACE
            .iter()
            .copied()
            .filter(|c| *c != granted)
            .collect();
        assert_eq!(denied, expected, "granting {granted}");

        // …and the specs are exactly those three capabilities' specs,
        // concatenated in that order.
        let expected_specs: Vec<&str> = expected
            .iter()
            .flat_map(|c| deny_specs_for(c).iter().copied())
            .collect();
        assert_eq!(p.deny_specs(), expected_specs, "granting {granted}");

        // A granted capability's specs must not appear at all.
        for spec in deny_specs_for(granted) {
            assert!(!p.deny_specs().contains(spec), "granting {granted} still emitted {spec}");
        }
        assert!(p.is_restricted(), "granting {granted}");
    }
}

#[test]
fn each_capability_withheld_alone_denies_exactly_itself() {
    for withheld in CAPABILITY_NAMESPACE {
        let granted: Vec<&str> = CAPABILITY_NAMESPACE
            .iter()
            .copied()
            .filter(|c| *c != withheld)
            .collect();
        let p = policy("role", declared(&granted));
        assert_eq!(p.denied_capabilities(), [withheld]);
        assert_eq!(p.deny_specs(), deny_specs_for(withheld).to_vec());
    }
}

#[test]
fn granting_the_whole_namespace_denies_nothing_without_a_wildcard() {
    let all: Vec<&str> = CAPABILITY_NAMESPACE.to_vec();
    let p = policy("builder", declared(&all));
    assert!(p.deny_specs().is_empty());
    // Still "restricted" on the Codex path: the declaration is an allowlist,
    // so a capability ADDED to the namespace later would be denied. That is
    // the intended fail-closed-on-capability behaviour, and the warning is
    // correct to fire.
    assert!(p.is_restricted());
}

#[test]
fn unknown_granted_names_neither_grant_nor_crash() {
    let p = policy("role", declared(&["database", "remote-shell"]));
    assert_eq!(p.denied_capabilities(), ["cloud-cli", "forge-secrets", "credential-store"]);
}

// ---------------------------------------------------------------------------
// The ONE wildcard rule: an exact `"*"` element, both paths (#8943)
//
// This section replaces the port's two-rule pinning tests. It used to hold
// `the_two_wildcard_rules_diverge_only_on_a_star_bearing_substring`, which
// asserted that `["cloud-*"]` was UNRESTRICTED on the deny-specs path (mirroring
// `spawn-claude.sh`'s substring test) while RESTRICTED on the Codex path
// (mirroring `spawn-codex.sh`'s `index("*")`). #8943 resolved that divergence in
// favour of the exact-element rule — the fail-closed one — so the replacement
// test below asserts the two paths now agree, and the old assertion is
// deliberately gone rather than relaxed.
// ---------------------------------------------------------------------------

#[test]
fn a_bare_star_is_the_wildcard_on_both_paths() {
    let p = policy("builder", declared(&["*"]));
    assert!(p.has_wildcard());
    // Unrestricted on the Codex predicate…
    assert!(!p.is_restricted());
    // …and no deny specs on the --disallowedTools path.
    assert!(p.denied_capabilities().is_empty());
    assert!(p.deny_specs().is_empty());
}

#[test]
fn the_unified_wildcard_rule_keeps_a_star_bearing_substring_restricted_on_both_paths() {
    // Replaces `the_two_wildcard_rules_diverge_only_on_a_star_bearing_substring`
    // (#8943). `cloud-*` is not a capability name and not the wildcard — it is
    // an unrecognized string, so it is INERT: it grants nothing and disarms
    // nothing, and BOTH paths say "restricted".
    let p = policy("role", declared(&["cloud-*"]));
    assert!(!p.has_wildcard(), "`cloud-*` must not count as a wildcard");

    // Codex path: restricted (unchanged from the port).
    assert!(p.is_restricted(), "codex path: restricted");

    // Deny-specs path: restricted too — this is the behaviour change. It used
    // to produce 0 specs (a silent full disarm); it now denies the whole
    // namespace, because `cloud-*` granted nothing.
    assert_eq!(
        p.denied_capabilities(),
        [
            "remote-shell",
            "cloud-cli",
            "forge-secrets",
            "credential-store"
        ],
        "deny-specs path: `cloud-*` grants nothing, so nothing is spared"
    );
    assert_eq!(p.deny_specs().len(), 38);
}

#[test]
fn an_unknown_star_bearing_capability_is_inert_not_a_glob() {
    // Every star-bearing shape that is not exactly `"*"`: a prefix glob, a
    // suffix glob, a doubled star, a real capability name with a star stuck to
    // it, and a bare star buried inside a longer token. None is a wildcard and
    // none grants a capability — so each behaves exactly like the plain unknown
    // string `"database"` does in `unknown_granted_names_neither_grant_nor_crash`.
    let baseline = policy("role", declared(&["database"]));
    for name in ["cloud-*", "*-cli", "**", "cloud-cli*", "*cloud-cli", "a*b"] {
        let p = policy("role", declared(&[name]));
        assert!(!p.has_wildcard(), "{name} was read as a wildcard");
        assert!(p.is_restricted(), "{name} disarmed the Codex path");
        assert!(deny_specs_for(name).is_empty(), "{name} resolved to a capability's specs");
        assert_eq!(
            p.denied_capabilities(),
            baseline.denied_capabilities(),
            "{name} is not inert: it differs from an ordinary unknown string"
        );
        assert_eq!(p.deny_specs(), baseline.deny_specs(), "{name}");
    }

    // …and an inert name sitting next to a real grant spares only the real one.
    let p = policy("role", declared(&["cloud-*", "cloud-cli"]));
    assert!(p.is_restricted());
    assert_eq!(p.denied_capabilities(), ["remote-shell", "forge-secrets", "credential-store"]);
}

#[test]
fn the_wildcard_constant_is_the_rule_both_paths_use() {
    // A structural guard on AC1: there is one wildcard rule, so a declaration
    // of CAPABILITY_WILDCARD alone must be the unrestricted case on both
    // paths, and `denied_capabilities()` must be empty for exactly the
    // not-`is_restricted()` cases.
    for allowlist in [
        Allowlist::Undeclared,
        declared(&[]),
        declared(&[CAPABILITY_WILDCARD]),
        declared(&["cloud-*"]),
        declared(&["cloud-cli"]),
        declared(&["database"]),
        declared(&["cloud-cli", CAPABILITY_WILDCARD]),
    ] {
        let p = policy("role", allowlist.clone());
        assert_eq!(
            p.denied_capabilities().is_empty(),
            !p.is_restricted(),
            "the two paths disagreed about {allowlist:?}"
        );
        assert_eq!(
            p.deny_specs().is_empty(),
            !p.is_restricted(),
            "the two paths disagreed about {allowlist:?}"
        );
    }
}

#[test]
fn undeclared_is_never_restricted_nor_wildcarded() {
    let p = policy("curator", Allowlist::Undeclared);
    assert!(!p.has_wildcard());
    assert!(!p.is_restricted());
}

// ---------------------------------------------------------------------------
// Candidate resolution
// ---------------------------------------------------------------------------

#[test]
fn candidate_paths_put_the_workspace_first() {
    let paths = RoleToolPolicy::candidate_paths(
        "builder",
        Some(Path::new("/ws")),
        &[PathBuf::from("/install/roles")],
    );
    assert_eq!(
        paths,
        vec![
            PathBuf::from("/ws/.loom/roles/builder.json"),
            PathBuf::from("/install/roles/builder.json"),
        ]
    );
}

#[test]
fn candidate_paths_omit_an_absent_workspace() {
    let paths = RoleToolPolicy::candidate_paths("judge", None, &[PathBuf::from("/install/roles")]);
    assert_eq!(paths, vec![PathBuf::from("/install/roles/judge.json")]);
}

#[test]
fn load_takes_the_first_readable_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    std::fs::write(&first, r#"{"toolPolicy": {"allowedCapabilities": []}}"#).unwrap();
    std::fs::write(&second, r#"{"toolPolicy": {"allowedCapabilities": ["*"]}}"#).unwrap();

    let p = RoleToolPolicy::load("curator", &[first.clone(), second]);
    assert_eq!(p.source.as_ref(), Some(&first));
    assert_eq!(p.allowlist, Allowlist::Declared(vec![]));
    assert_eq!(p.deny_specs().len(), 38);
}

#[test]
fn load_skips_a_missing_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.json");
    let present = dir.path().join("present.json");
    std::fs::write(&present, r#"{"toolPolicy": {"allowedCapabilities": ["cloud-cli"]}}"#).unwrap();

    let p = RoleToolPolicy::load("driver", &[missing, present.clone()]);
    assert_eq!(p.source.as_ref(), Some(&present));
    assert_eq!(p.denied_capabilities(), ["remote-shell", "forge-secrets", "credential-store"]);
}

#[test]
fn load_does_not_fall_through_past_a_malformed_readable_candidate() {
    // First-readable-wins, even when the JSON is garbage: consulting a
    // DIFFERENT role's file because the intended one was corrupt would apply
    // the wrong policy while reporting the right role.
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.json");
    let fallback = dir.path().join("fallback.json");
    std::fs::write(&broken, "{ this is not json").unwrap();
    std::fs::write(&fallback, r#"{"toolPolicy": {"allowedCapabilities": []}}"#).unwrap();

    let p = RoleToolPolicy::load("curator", &[broken.clone(), fallback]);
    assert_eq!(p.source.as_ref(), Some(&broken));
    assert_eq!(p.allowlist, Allowlist::Undeclared);
    assert!(p.deny_specs().is_empty());
}

#[test]
fn no_readable_candidate_is_undeclared_with_no_source() {
    let dir = tempfile::tempdir().unwrap();
    let p = RoleToolPolicy::load("ghost", &[dir.path().join("ghost.json")]);
    assert_eq!(p.source, None);
    assert_eq!(p.allowlist, Allowlist::Undeclared);
    assert!(p.deny_specs().is_empty());
    assert!(!p.is_restricted());
}

// ---------------------------------------------------------------------------
// Against the shipped role JSONs
// ---------------------------------------------------------------------------

/// Every `defaults/roles/*.json` that declares a restrictive allowlist must
/// produce a non-empty, in-namespace deny list, and every wildcard role must
/// produce none. This is the manual cross-check from the issue's test plan,
/// automated — it runs only when the repo tree is reachable from the test's
/// own manifest directory, so it never fails an out-of-tree build.
#[test]
fn shipped_role_jsons_resolve_consistently() {
    let roles_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("defaults/roles"));
    let Some(roles_dir) = roles_dir.filter(|p| p.is_dir()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&roles_dir) else {
        return;
    };

    let mut seen = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(role) = resolve_role_name(stem) else {
            panic!("shipped role file {stem}.json has a name this port rejects");
        };
        seen += 1;

        let p = RoleToolPolicy::load(&role, std::slice::from_ref(&path));
        // Whatever each role declares, the invariants hold: never a spec
        // outside the namespace, and deny-specs and restriction agree on the
        // undeclared case.
        for cap in p.denied_capabilities() {
            assert!(
                CAPABILITY_NAMESPACE.contains(&cap),
                "{role}: denied out-of-namespace capability {cap}"
            );
        }
        if !p.allowlist.is_declared() {
            assert!(p.deny_specs().is_empty(), "{role}");
            assert!(!p.is_restricted(), "{role}");
        }
    }
    assert!(seen > 0, "no shipped role JSONs were read from {roles_dir:?}");
}
