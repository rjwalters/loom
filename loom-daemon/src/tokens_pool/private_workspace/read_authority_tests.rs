//! Hermetic tests for the read-authority proof contract (#14716).
//!
//! Every provider route — GitHub's paired scoped mint and identity read,
//! Gitea's repository identity, authenticated principal and Git-over-HTTPS
//! route — is scripted through the [`ProviderHttp`] / [`ScopedMinter`]
//! seams, so the fixtures record the exact URL and the exact credential
//! each leg used. That record is what proves credential/route propagation:
//! identity, code-read verification and the Git read must all have used the
//! same selected credential and canonical route, with no ambient fallback.
//!
//! Storage, binds, borrower tracking and idle clone migration belong to
//! #14680; these tests cover only the read-authority contract — the
//! decisions, the refusal vocabulary, and the legacy path staying
//! untouched in every refusal case.
use super::read_authority::*;
use super::*;
use crate::credential_preflight::{Minted, ScopedIssuance};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const GHS_A: &str = "ghs_scopedAAAA TESTONLY";
const GHS_B: &str = "ghs_scopedBBBB TESTONLY";
const GITEA_TOKEN: &str = "gtea_selected TESTONLY";

/// A fixed, readable wall clock: 2027-01-15-ish.
fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// The scripted provider transport: exact URL → canned answer, recording
/// every `(url, credential)` pair it served, so a test can prove the same
/// selected credential reached every leg of one verification.
#[derive(Default)]
struct FakeHttp {
    routes: Vec<(String, ProbeAnswer)>,
    seen: Mutex<Vec<(String, String)>>,
}

impl FakeHttp {
    /// Script `url`. Setting a URL twice REPLACES its answer, so a test can
    /// take the healthy fixture and flip exactly one leg.
    fn route(mut self, url: &str, answer: ProbeAnswer) -> Self {
        self.routes.retain(|(route, _)| route != url);
        self.routes.push((url.to_owned(), answer));
        self
    }

    fn saw(&self, url: &str, credential: &str) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|(u, c)| u == url && c == credential)
    }

    fn credentials_on(&self, url: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u == url)
            .map(|(_, c)| c.clone())
            .collect()
    }
}

impl ProviderHttp for FakeHttp {
    fn get(
        &self,
        url: &str,
        _scheme: AuthScheme,
        _accept: &str,
        credential: &Credential,
    ) -> ProbeAnswer {
        self.seen
            .lock()
            .unwrap()
            .push((url.to_owned(), credential.value().to_owned()));
        self.routes
            .iter()
            .find(|(route, _)| route == url)
            .map(|(_, answer)| answer.clone())
            .unwrap_or_else(|| ProbeAnswer::Transport(format!("no scripted route for {url}")))
    }
}

/// The scripted issuer, recording what it was asked to mint.
struct FakeMinter {
    outcome: ScopedOutcome,
    seen: Mutex<Vec<String>>,
}

impl FakeMinter {
    fn minting(outcome: ScopedOutcome) -> Self {
        Self {
            outcome,
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl ScopedMinter for FakeMinter {
    fn mint_scoped(&self, owner_repo: &str) -> ScopedOutcome {
        self.seen.lock().unwrap().push(owner_repo.to_owned());
        match &self.outcome {
            ScopedOutcome::Minted(minted) => ScopedOutcome::Minted(minted.clone()),
            ScopedOutcome::NotConfigured => ScopedOutcome::NotConfigured,
            ScopedOutcome::Refused(reason) => ScopedOutcome::Refused(reason.clone()),
        }
    }
}

/// One paired scoped mint: `token` for exactly `repository_id`, Contents
/// read, expiring 2035-01-01 (epoch 2051222400).
fn scoped_mint(token: &str, repository_id: u64, installation: &str) -> Minted {
    Minted::read_authority(
        token.to_owned(),
        "4486636".to_owned(),
        installation.to_owned(),
        "acme".to_owned(),
        "2035-01-01T00:00:00Z".to_owned(),
        ScopedIssuance {
            repository_ids: vec![repository_id.to_string()],
            permissions: [("contents".to_owned(), "read".to_owned())]
                .into_iter()
                .collect(),
        },
    )
}

fn github_request() -> VerificationRequest {
    VerificationRequest {
        url: "https://github.com/acme/widget".to_owned(),
        github_host: None,
        gitea_credential: None,
    }
}

fn gitea_request() -> VerificationRequest {
    VerificationRequest {
        url: "https://gitea.example.com:3000/base/acme/widget".to_owned(),
        github_host: None,
        gitea_credential: Some(Credential::new(GITEA_TOKEN.to_owned())),
    }
}

/// All three Gitea legs for a healthy private instance, so single-leg
/// failure tests can override exactly one route.
fn gitea_http() -> FakeHttp {
    FakeHttp::default()
        .route(
            "https://gitea.example.com:3000/base/api/v1/repos/acme/widget",
            ProbeAnswer::Ok(
                r#"{"id": 42, "full_name": "acme/widget", "permissions": {"admin": false, "pull": true, "push": false}}"#.to_owned(),
            ),
        )
        .route(
            "https://gitea.example.com:3000/base/api/v1/user",
            ProbeAnswer::Ok(r#"{"id": 7, "login": "acme-bot"}"#.to_owned()),
        )
        .route(
            "https://gitea.example.com:3000/base/acme/widget.git/info/refs?service=git-upload-pack",
            ProbeAnswer::Ok("001e# service=git-upload-pack\n0000".to_owned()),
        )
}

/// Run one verification and demand a proof out of it.
fn proved(
    request: &VerificationRequest,
    http: &dyn ProviderHttp,
    minter: &dyn ScopedMinter,
    now: SystemTime,
) -> ReadProof {
    match verify(request, http, minter, now) {
        Verification::Proved(proof) => proof,
        Verification::Unproved(reason) => panic!("expected a proof, got {reason:?}"),
    }
}

/// Run one verification and demand its refusal.
fn unproved(
    request: &VerificationRequest,
    http: &dyn ProviderHttp,
    minter: &dyn ScopedMinter,
    now: SystemTime,
) -> Unproved {
    match verify(request, http, minter, now) {
        Verification::Unproved(reason) => reason,
        Verification::Proved(_) => panic!("expected a refusal, got a proof"),
    }
}

fn github_grant(repository_id: &str, installation: &str) -> ReadGrant {
    ReadGrant {
        identity: RepositoryIdentity {
            provider: Provider::GitHub,
            base: ForgeBase::of_repository_url("https://github.com/acme/widget").unwrap(),
            repository_id: repository_id.to_owned(),
        },
        scope: ReadScope::GitHubContentsRead,
        principal: Principal::GitHubApp {
            app_id: "4486636".to_owned(),
            installation_id: installation.to_owned(),
        },
    }
}

// --- GitHub verification -------------------------------------------------

#[test]
fn github_proves_a_paired_scoped_mint_and_reads_identity_with_the_same_credential() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439, "private": true}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    assert_eq!(proof.grant(), &github_grant("907216439", "151241341"));
    assert!(proof.expires_at().is_some(), "the paired expiry travels with the proof");
    // Credential/route propagation: the identity read used the EXACT minted
    // credential on the canonical API route — nothing ambient, nothing else.
    assert!(http.saw("https://api.github.com/repos/acme/widget", GHS_A));
    assert_eq!(
        minter.seen.lock().unwrap().as_slice(),
        ["acme/widget"],
        "the issuer was asked for exactly this repository"
    );
    // The proof's own Debug never carries the credential.
    assert!(!format!("{proof:?}").contains(GHS_A));
}

#[test]
fn github_refuses_a_mint_whose_scope_is_not_contents_read() {
    // Two different scopes can sit on one URL; only Contents read binds.
    let mut minted = scoped_mint(GHS_A, 907216439, "151241341");
    minted.scoped = Some(ScopedIssuance {
        repository_ids: vec!["907216439".to_owned()],
        permissions: [("metadata".to_owned(), "read".to_owned())]
            .into_iter()
            .collect(),
    });
    let http = FakeHttp::default();
    let minter = FakeMinter::minting(ScopedOutcome::Minted(minted));
    assert_eq!(
        unproved(&github_request(), &http, &minter, at(1_800_000_000)),
        Unproved::ScopeUnavailable(
            "the issuer did not bind Contents read to this credential".to_owned()
        )
    );
    assert!(
        http.seen.lock().unwrap().is_empty(),
        "a credential without code read is never used against the forge"
    );
}

#[test]
fn github_refuses_evidence_that_does_not_pair_with_one_repository() {
    let mut minted = scoped_mint(GHS_A, 907216439, "151241341");
    minted.scoped = Some(ScopedIssuance {
        repository_ids: vec!["907216439".to_owned(), "907216440".to_owned()],
        permissions: [("contents".to_owned(), "read".to_owned())]
            .into_iter()
            .collect(),
    });
    let minter = FakeMinter::minting(ScopedOutcome::Minted(minted));
    let reason = unproved(&github_request(), &FakeHttp::default(), &minter, at(1_800_000_000));
    assert!(matches!(reason, Unproved::BadEvidence(_)), "{reason:?}");
}

#[test]
fn github_refuses_a_reused_slug_whose_repository_id_moved() {
    // The issuer bound id 907216439; the slug now answers 111 — a
    // replacement repository cannot inherit the store.
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 111}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    assert_eq!(
        unproved(&github_request(), &http, &minter, at(1_800_000_000)),
        Unproved::IdentityMismatch
    );
}

#[test]
fn github_refuses_an_expired_mint() {
    // The paired expiry (2035) is past `now` (2036).
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    assert_eq!(
        unproved(&github_request(), &FakeHttp::default(), &minter, at(2_100_000_000)),
        Unproved::Expired
    );
}

#[test]
fn github_refuses_a_revoked_or_narrow_credential() {
    for (answer, expected) in [
        (ProbeAnswer::Status(401), Unproved::NotAuthenticated),
        (ProbeAnswer::Status(403), Unproved::NotAuthenticated),
        (ProbeAnswer::Status(404), Unproved::NotFound),
    ] {
        let http = FakeHttp::default().route("https://api.github.com/repos/acme/widget", answer);
        let minter =
            FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
        assert_eq!(unproved(&github_request(), &http, &minter, at(1_800_000_000)), expected);
    }
}

#[test]
fn github_refuses_a_redirect_instead_of_following_it() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Redirect("https://malicious.example/repo".to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    assert_eq!(
        unproved(&github_request(), &http, &minter, at(1_800_000_000)),
        Unproved::Redirect("https://malicious.example/repo".to_owned())
    );
    // And it never followed the redirect: only the canonical URL was hit.
    assert_eq!(http.seen.lock().unwrap().len(), 1);
}

#[test]
fn github_without_an_authorized_issuer_stays_unproved() {
    for outcome in [
        ScopedOutcome::NotConfigured,
        ScopedOutcome::Refused("installation grants no Contents read".to_owned()),
    ] {
        let minter = FakeMinter::minting(outcome);
        let reason = unproved(&github_request(), &FakeHttp::default(), &minter, at(1_800_000_000));
        assert!(matches!(reason, Unproved::IssuerUnavailable(_)), "{reason:?}");
    }
}

// --- Gitea verification --------------------------------------------------

#[test]
fn gitea_proves_identity_principal_and_code_read_with_the_same_selected_credential() {
    let http = gitea_http();
    let proof = proved(
        &gitea_request(),
        &http,
        &FakeMinter::minting(ScopedOutcome::NotConfigured),
        at(1_800_000_000),
    );
    assert_eq!(
        proof.grant(),
        &ReadGrant {
            identity: RepositoryIdentity {
                provider: Provider::Gitea,
                base: ForgeBase::of_repository_url(
                    "https://gitea.example.com:3000/base/acme/widget"
                )
                .unwrap(),
                repository_id: "42".to_owned(),
            },
            scope: ReadScope::GiteaCodeRead,
            principal: Principal::GiteaUser {
                user_id: "7".to_owned()
            },
        }
    );
    // No durable expiry: the proof holds only for this in-flight operation.
    assert_eq!(proof.expires_at(), None);
    // Credential/route propagation: ALL THREE legs — identity, principal,
    // the exact private Git-over-HTTPS route — used the same selected
    // credential on canonical instance URLs (subpath and port included).
    for url in [
        "https://gitea.example.com:3000/base/api/v1/repos/acme/widget",
        "https://gitea.example.com:3000/base/api/v1/user",
        "https://gitea.example.com:3000/base/acme/widget.git/info/refs?service=git-upload-pack",
    ] {
        assert_eq!(http.credentials_on(url), [GITEA_TOKEN.to_owned()], "on {url}");
    }
    assert!(!format!("{proof:?}").contains(GITEA_TOKEN));
}

#[test]
fn gitea_refuses_an_origin_with_no_selected_credential() {
    let request = VerificationRequest {
        url: "https://gitea.example.com:3000/base/acme/widget".to_owned(),
        github_host: None,
        gitea_credential: None,
    };
    // Unknown credential provenance is a refusal, never an ambient pickup.
    assert_eq!(
        unproved(
            &request,
            &gitea_http(),
            &FakeMinter::minting(ScopedOutcome::NotConfigured),
            at(1_800_000_000)
        ),
        Unproved::NoCredential
    );
}

#[test]
fn gitea_refuses_a_metadata_only_scope() {
    let http = FakeHttp::default().route(
        "https://gitea.example.com:3000/base/api/v1/repos/acme/widget",
        ProbeAnswer::Ok(
            r#"{"id": 42, "permissions": {"admin": true, "pull": false, "push": true}}"#.to_owned(),
        ),
    );
    assert_eq!(
        unproved(
            &gitea_request(),
            &http,
            &FakeMinter::minting(ScopedOutcome::NotConfigured),
            at(1_800_000_000)
        ),
        Unproved::ScopeUnavailable(
            "the credential has no effective Code read on this repository".to_owned()
        )
    );
    assert!(
        http.seen.lock().unwrap().len() == 1,
        "a metadata-only credential is never taken to the Git route"
    );
}

#[test]
fn gitea_refuses_when_the_git_route_denies_the_credential() {
    for (answer, expected) in [
        (ProbeAnswer::Status(401), Unproved::NotAuthenticated),
        (ProbeAnswer::Status(403), Unproved::NotAuthenticated),
        (
            ProbeAnswer::Redirect(
                "https://other.example/base/acme/widget.git/info/refs".to_owned(),
            ),
            Unproved::Redirect("https://other.example/base/acme/widget.git/info/refs".to_owned()),
        ),
        (
            ProbeAnswer::Ok("plain text, not the smart advertisement".to_owned()),
            Unproved::BadEvidence(
                "the route answered, but the evidence does not pair: 39 bytes, not usable"
                    .to_owned(),
            ),
        ),
        (ProbeAnswer::Status(404), Unproved::NotFound),
    ] {
        let http = gitea_http().route(
            "https://gitea.example.com:3000/base/acme/widget.git/info/refs?service=git-upload-pack",
            answer,
        );
        assert_eq!(
            unproved(
                &gitea_request(),
                &http,
                &FakeMinter::minting(ScopedOutcome::NotConfigured),
                at(1_800_000_000)
            ),
            expected
        );
    }
}

#[test]
fn gitea_refuses_an_instance_that_does_not_answer() {
    // No scripted route at all: an unsupported or unreachable provider is
    // unproved, never guessed.
    let reason = unproved(
        &gitea_request(),
        &FakeHttp::default(),
        &FakeMinter::minting(ScopedOutcome::NotConfigured),
        at(1_800_000_000),
    );
    assert!(matches!(reason, Unproved::Transport(_)), "{reason:?}");
}

#[test]
fn an_origin_that_is_not_an_owner_name_url_is_unsupported() {
    for url in [
        "https://github.com/acme",
        "not a url",
        "ssh://git@github.com/acme/widget",
    ] {
        let request = VerificationRequest {
            url: url.to_owned(),
            github_host: None,
            gitea_credential: None,
        };
        let reason = unproved(
            &request,
            &FakeHttp::default(),
            &FakeMinter::minting(ScopedOutcome::NotConfigured),
            at(1_800_000_000),
        );
        assert!(matches!(reason, Unproved::UnsupportedUrl(_)), "{url}: {reason:?}");
    }
}

// --- Admission ------------------------------------------------------------

#[test]
fn admission_is_fail_closed_without_a_proof_or_a_pinned_grant() {
    let pinned = github_grant("907216439", "151241341");
    assert_eq!(
        admit_borrower(None, Some(&pinned), at(1_800_000_000)),
        Admission::Refused {
            reason: AdmissionRefusal::NoProof
        }
    );
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    assert_eq!(
        admit_borrower(Some(&proof), None, at(1_800_000_000)),
        Admission::Refused {
            reason: AdmissionRefusal::StoreNotProved
        }
    );
}

#[test]
fn admission_accepts_rotation_within_the_same_authority() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let old = proved(
        &github_request(),
        &http,
        &FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341"))),
        at(1_800_000_000),
    );
    let fresh = proved(
        &github_request(),
        &http,
        &FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_B, 907216439, "151241341"))),
        at(1_800_000_100),
    );
    let pinned = old.grant().clone();
    assert_eq!(
        admit_borrower(Some(&fresh), Some(&pinned), at(1_800_000_100)),
        Admission::Admitted {
            grant: pinned.clone()
        }
    );
    // Two credentials exercised the same canonical route.
    assert_eq!(
        http.credentials_on("https://api.github.com/repos/acme/widget")
            .len(),
        2
    );
}

#[test]
fn admission_refuses_a_replacement_repository_at_the_same_slug() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    // The store pinned the repository the slug used to name.
    let pinned = github_grant("907216440", "151241341");
    assert_eq!(
        admit_borrower(Some(&proof), Some(&pinned), at(1_800_000_000)),
        Admission::Refused {
            reason: AdmissionRefusal::Authority(AuthorityGap::DifferentRepository)
        }
    );
}

#[test]
fn admission_refuses_a_different_principal_at_the_same_url() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "99000999")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    let pinned = github_grant("907216439", "151241341");
    assert_eq!(
        admit_borrower(Some(&proof), Some(&pinned), at(1_800_000_000)),
        Admission::Refused {
            reason: AdmissionRefusal::Authority(AuthorityGap::DifferentAuthority)
        }
    );
}

#[test]
fn admission_refuses_an_incompatible_scope_on_one_url() {
    let proof_grant = ReadGrant {
        identity: github_grant("907216439", "151241341").identity,
        scope: ReadScope::GiteaCodeRead,
        principal: github_grant("907216439", "151241341").principal,
    };
    let pinned = github_grant("907216439", "151241341");
    assert!(!proof_grant.same_authority(&pinned));
    // The gap is classified through the decision, using a proof-shaped pair:
    // identity equal, scope different.
    let mut incoming = pinned.clone();
    incoming.scope = ReadScope::GiteaCodeRead;
    assert_eq!(authority_gap(&incoming, &pinned), Some(AuthorityGap::IncompatibleScope));
    // And the symmetric case: identity differs first, even when all three
    // fields differ (a reused slug reads as a replacement repository).
    let mut replaced = incoming.clone();
    replaced.identity.repository_id = "907216440".to_owned();
    assert_eq!(authority_gap(&replaced, &pinned), Some(AuthorityGap::DifferentRepository));
}

#[test]
fn admission_refuses_an_expired_proof() {
    // Paired expiry (2035) is past `now` (2036): the mint refuses earlier,
    // so build the expired case from the decision's own clock.
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    let pinned = proof.grant().clone();
    assert_eq!(
        admit_borrower(Some(&proof), Some(&pinned), at(2_100_000_000)),
        Admission::Refused {
            reason: AdmissionRefusal::Expired
        }
    );
}

// --- Publication ----------------------------------------------------------

/// A proof and the matching pinned grant for publication tests.
fn proved_store(now: SystemTime) -> (ReadProof, ReadGrant) {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, now);
    let pinned = proof.grant().clone();
    (proof, pinned)
}

#[test]
fn publication_requires_a_fresh_matching_proof() {
    let (proof, pinned) = proved_store(at(1_800_000_000));
    let borrowers = [BorrowerAuthority::Pinned(pinned.clone())];
    assert_eq!(
        publish(None, &pinned, &borrowers, at(1_800_000_000)),
        Publication::Withheld {
            reason: PublicationRefusal::NoProof
        }
    );
    assert_eq!(
        publish(Some(&proof), &pinned, &borrowers, at(2_100_000_000)),
        Publication::Withheld {
            reason: PublicationRefusal::Expired
        }
    );
    // A proof for a different repository is a mismatch, never a passthrough.
    let other = github_grant("907216440", "151241341");
    assert_eq!(
        publish(Some(&proof), &other, &borrowers, at(1_800_000_000)),
        Publication::Withheld {
            reason: PublicationRefusal::Authority(AuthorityGap::DifferentRepository)
        }
    );
}

#[test]
fn publication_withholds_on_any_borrowers_missing_or_lost_authority() {
    let (proof, pinned) = proved_store(at(1_800_000_000));
    let healthy = [BorrowerAuthority::Pinned(pinned.clone())];
    assert_eq!(
        publish(Some(&proof), &pinned, &healthy, at(1_800_000_000)),
        Publication::Publish {
            grant: pinned.clone()
        }
    );
    // A borrower whose state cannot be read is missing, not fine.
    let unreadable = [
        BorrowerAuthority::Pinned(pinned.clone()),
        BorrowerAuthority::Unknown,
    ];
    assert_eq!(
        publish(Some(&proof), &pinned, &unreadable, at(1_800_000_000)),
        Publication::Withheld {
            reason: PublicationRefusal::BorrowerStateMissing
        }
    );
    // A borrower that lost the grant withholds — for everyone, in the same
    // decision: new bytes stop flowing to an unverified set.
    let mut drifted = pinned.clone();
    drifted.principal = Principal::GitHubApp {
        app_id: "4486636".to_owned(),
        installation_id: "99000999".to_owned(),
    };
    let lost = [BorrowerAuthority::Pinned(drifted)];
    assert_eq!(
        publish(Some(&proof), &pinned, &lost, at(1_800_000_000)),
        Publication::Withheld {
            reason: PublicationRefusal::BorrowerAuthorityLost
        }
    );
    // Withholding is per-decision: the pinned authority and the previous
    // object set survive it, so a later decision for a healthy set — or a
    // re-attached borrower after a fresh proof — publishes again.
    assert_eq!(
        publish(Some(&proof), &pinned, &healthy, at(1_800_000_000)),
        Publication::Publish { grant: pinned }
    );
}

#[test]
fn publication_accepts_rotation_and_an_empty_borrower_set() {
    let (proof, pinned) = proved_store(at(1_800_000_000));
    assert_eq!(
        publish(Some(&proof), &pinned, &[], at(1_800_000_000)),
        Publication::Publish { grant: pinned }
    );
}

// --- Scoped-envelope parsing ---------------------------------------------

#[test]
fn the_scoped_envelope_pairs_the_token_with_its_evidence() {
    let outcome = parse_scoped_envelope(
        r#"{"status":"ok","token":"ghs_envelope","installation_id":"151241341","app_id":"4486636","expires_at":"2035-01-01T00:00:00Z","repository_ids":[907216439],"permissions":{"contents":"read"}}"#,
        "Acme/Widget",
    );
    let ScopedOutcome::Minted(minted) = outcome else {
        panic!("expected a paired mint");
    };
    assert_eq!(minted.token, "ghs_envelope");
    assert_eq!(minted.owner, "acme", "the owner is normalized, not echoed");
    assert_eq!(minted.scoped.as_ref().unwrap().repository_ids, ["907216439".to_owned()]);
    assert_eq!(
        minted
            .scoped
            .as_ref()
            .unwrap()
            .permissions
            .get("contents")
            .map(String::as_str),
        Some("read")
    );
}

#[test]
fn an_unpaired_or_failing_envelope_is_refused() {
    for (stdout, want_not_configured) in [
        (r#"{"status":"not_configured"}"#, true),
        (r#"{"status":"error","message":"no Contents read"}"#, false),
        (
            r#"{"status":"ok","token":"ghs_x","installation_id":"1","app_id":"2","expires_at":"2035-01-01T00:00:00Z"}"#,
            false,
        ),
        (
            r#"{"status":"ok","token":"ghs_x","installation_id":"1","app_id":"2","expires_at":"2035-01-01T00:00:00Z","repository_ids":["not-numeric"],"permissions":{"contents":"read"}}"#,
            false,
        ),
        ("not json at all", false),
    ] {
        match parse_scoped_envelope(stdout, "acme/widget") {
            ScopedOutcome::NotConfigured if want_not_configured => {}
            ScopedOutcome::Refused(reason) => {
                assert!(!reason.contains("ghs_"), "the refusal never quotes the token: {reason}")
            }
            other => panic!(
                "expected a refusal for {stdout}, got token present: {}",
                other_token_present(&other)
            ),
        }
    }
}

/// Whether a parsed outcome carries a token (test reporting only).
fn other_token_present(outcome: &ScopedOutcome) -> bool {
    matches!(outcome, ScopedOutcome::Minted(_))
}

// --- The proved reader and the legacy path --------------------------------

#[test]
fn a_proved_reader_uses_the_exact_route_and_keeps_the_credential_out_of_argv() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    let reader = forge_auth::ProvedReader::new(&proof, "https://github.com/acme/widget")
        .expect("the proof's base is the URL's base");
    assert_eq!(reader.url(), "https://github.com/acme/widget");
    let env = reader.fetch_env();
    let value_of = |key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("missing {key}"))
    };
    // Ambient sources are disabled: no helpers, no system/global config
    // (so no URL rewriting), no prompts, redirects refused.
    assert_eq!(value_of("GIT_CONFIG_COUNT"), "3");
    assert_eq!(value_of("GIT_CONFIG_KEY_0"), "credential.helper");
    assert_eq!(value_of("GIT_CONFIG_VALUE_0"), "", "the helper is cleared");
    assert_eq!(value_of("GIT_CONFIG_VALUE_1"), "false", "redirects refused");
    assert_eq!(value_of("GIT_CONFIG_NOSYSTEM"), "1");
    assert_eq!(value_of("GIT_CONFIG_GLOBAL"), "/dev/null");
    assert_eq!(value_of("GIT_TERMINAL_PROMPT"), "0");
    // The credential rides the child's ENVIRONMENT as the extra header —
    // never as a bare value an argv or a log could quote.
    let header = value_of("GIT_CONFIG_VALUE_2");
    assert!(header.starts_with("Authorization: Basic "), "{header}");
    for (key, value) in &env {
        assert!(!value.contains(GHS_A), "the raw credential never appears at {key}");
    }
}

#[test]
fn a_proof_for_another_origin_cannot_authorize_a_fetch() {
    let http = FakeHttp::default().route(
        "https://api.github.com/repos/acme/widget",
        ProbeAnswer::Ok(r#"{"id": 907216439}"#.to_owned()),
    );
    let minter =
        FakeMinter::minting(ScopedOutcome::Minted(scoped_mint(GHS_A, 907216439, "151241341")));
    let proof = proved(&github_request(), &http, &minter, at(1_800_000_000));
    assert!(forge_auth::ProvedReader::new(&proof, "https://github.com/acme/other").is_none());
    assert!(forge_auth::ProvedReader::new(
        &proof,
        "https://gitea.example.com:3000/base/acme/widget"
    )
    .is_none());
}

#[test]
fn an_unproved_route_keeps_the_legacy_exec_auth_path() {
    // The refusal vocabulary is built for the legacy fallback: every
    // Unproved leaves ExecAuth's ambient resolution untouched, and a
    // proved reader simply never comes into existence.
    let reason = unproved(
        &gitea_request(),
        &FakeHttp::default(),
        &FakeMinter::minting(ScopedOutcome::NotConfigured),
        at(1_800_000_000),
    );
    assert!(matches!(reason, Unproved::Transport(_)), "{reason:?}");
    let empty = tempfile::tempdir().unwrap();
    let vars: std::collections::HashMap<String, std::ffi::OsString> =
        [("GH_TOKEN", "ambient-writer-token")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), std::ffi::OsString::from(*v)))
            .collect();
    let auth = forge_auth::ExecAuth::resolve_with(
        empty.path(),
        "https://gitea.example.com:3000/base/acme/widget",
        &|name| vars.get(name).cloned(),
        &|_| None,
    );
    assert!(
        auth.names().contains(&"GH_TOKEN"),
        "the legacy path still forwards ambient credentials"
    );
}
