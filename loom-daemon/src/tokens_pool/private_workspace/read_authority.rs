//! A host-owned proof that a retained mirror store belongs to the same
//! repository and read authority as its borrowers (#14716).
//!
//! A host mirror (#14680 owns the store, pack staging and borrower
//! tracking) retains Git objects across ref changes and publishes new packs
//! into a read-only bind that existing seats can still read. Admission and
//! refresh need proof that this retained store belongs to the same
//! repository and read authority as its borrowers — owner or path equality,
//! a cached role, a token prefix or a mutable accounting sidecar cannot
//! supply it.
//!
//! The contract is deliberately split in two:
//!
//! * [`ReadGrant`] — the STABLE authority a store pins: provider, canonical
//!   forge base (instance and base path, so provider-issued repository IDs
//!   stay namespaced), the approved code-read scope and the stable
//!   principal. Secret-free and serializable on purpose: this is the public
//!   record the two lifecycle decisions compare.
//! * [`ReadProof`] — one verified credential instance carrying that grant.
//!   The exact credential travels privately from verification ([`verify`])
//!   through the mirror fetch (`forge_auth::ProvedReader`), never in argv,
//!   logs or serialized state.
//!
//! Verification is fail-closed: anything but affirmative, matching evidence
//! from the explicitly selected route is [`Verification::Unproved`], and the
//! caller stays on the legacy private-clone path — which this module never
//! modifies, exactly as seat writer selection stays untouched.
//!
//! The two lifecycle decisions are separate on purpose ([`admit_borrower`]
//! for a new borrower, [`publish`] for fresh objects to existing
//! borrowers): an unverifiable, expired, revoked or incompatible-scope
//! authority refuses the affected operation only, and a withheld
//! publication leaves the previous object set and running seat work usable
//! — the store keeps serving the bytes it already holds (#14680).
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The forge provider a canonical origin claims (#14716).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    /// GitHub (github.com or a configured `GH_HOST` Enterprise host).
    GitHub,
    /// A Gitea instance — probed as such; a base that is neither is refused
    /// by verification, never guessed.
    Gitea,
}

impl Provider {
    /// The provider a canonical forge base claims: the configured GitHub
    /// host (default `github.com`) is GitHub; every other base is verified
    /// as a Gitea instance and refused when it is not one.
    #[must_use]
    pub fn of_base(base: &ForgeBase, github_host: &str) -> Self {
        if base.authority().eq_ignore_ascii_case(github_host) {
            Self::GitHub
        } else {
            Self::Gitea
        }
    }

    /// The Authorization scheme the provider's authenticated routes expect.
    #[must_use]
    pub fn auth_scheme(self) -> AuthScheme {
        match self {
            Self::GitHub => AuthScheme::Bearer,
            Self::Gitea => AuthScheme::Token,
        }
    }
}

/// How a credential is presented on an authenticated route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer <token>` (GitHub).
    Bearer,
    /// `Authorization: token <token>` (Gitea).
    Token,
}

impl AuthScheme {
    /// The header value prefix for this scheme.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Bearer => "Bearer",
            Self::Token => "token",
        }
    }
}

/// The canonical HTTPS forge base of an origin: scheme, authority (host and
/// explicit port) and configured subpath — everything but the final
/// `owner/name` pair (#14716). Repository IDs are namespaced by it: another
/// port or subpath is another instance whose numeric ID 42 names a
/// different repository.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForgeBase(String);

impl ForgeBase {
    /// The base of a canonical credential-free repository URL, lowercased
    /// so two spellings of one origin share it. `None` for anything
    /// [`super::identify`] refuses.
    #[must_use]
    pub fn of_repository_url(value: &str) -> Option<Self> {
        let id = super::identify(value).ok()?;
        // identify's url is `https://authority/[subpath/]owner/name`: the
        // base is everything but the trailing owner/name pair.
        let without_name = id.url.rsplit_once('/')?.0;
        let base = without_name.rsplit_once('/')?.0;
        Some(Self(base.to_ascii_lowercase()))
    }

    /// The `host[:port]` authority of this base.
    #[must_use]
    pub fn authority(&self) -> &str {
        self.0
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or("")
    }

    /// The canonical URL of the repository's Git-over-HTTPS route.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A provider-issued repository identity, namespaced by forge instance:
/// never a bare numeric ID, which two instances can both issue (#14716).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    pub provider: Provider,
    pub base: ForgeBase,
    /// The provider-issued numeric repository ID, as a string.
    pub repository_id: String,
}

/// The approved repository code-read scope one grant carries: what the
/// credential must be able to do, and exactly what it was proved to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadScope {
    /// A GitHub App installation token issued for this one repository with
    /// Contents read (a metadata-only or write-only credential does not
    /// carry it).
    GitHubContentsRead,
    /// Effective Code read on the repository's private Git-over-HTTPS
    /// route, verified with the same credential that reads it.
    GiteaCodeRead,
}

/// The stable principal behind a grant: it survives credential rotation
/// within one authority, and a different principal at the same URL cannot
/// inherit an existing binding (#14716).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Principal {
    /// The App installation that issued the credential: stable across
    /// token rotation; a different installation is a different authority
    /// even for the same repository.
    GitHubApp {
        app_id: String,
        installation_id: String,
    },
    /// The authenticated Gitea principal by immutable numeric user ID — a
    /// mutable username never qualifies.
    GiteaUser { user_id: String },
}

/// The stable read authority a retained store pins (#14716): identity,
/// approved scope and principal. The public, secret-free half of the proof
/// contract — the credential never travels with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadGrant {
    pub identity: RepositoryIdentity,
    pub scope: ReadScope,
    pub principal: Principal,
}

impl ReadGrant {
    /// Whether two grants state ONE authority: same repository identity,
    /// same approved scope, same principal. Credential lifetime is
    /// deliberately absent — rotation (a fresh credential under the same
    /// authority) compares equal.
    #[must_use]
    pub fn same_authority(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.scope == other.scope
            && self.principal == other.principal
    }
}

/// The exact credential one proof verified (#14716). Deliberately not
/// `Debug`/`Clone`/`Serialize`: the value must not be printable, copyable
/// or persistable by accident — only the private transport carries it.
pub struct Credential(String);

impl Credential {
    /// Wrap a selected credential value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The credential value, visible only inside the private-workspace
    /// module tree: the transport that must carry it, and nothing else.
    pub(super) fn value(&self) -> &str {
        &self.0
    }

    /// Copy for the proof that will outlive this borrow. Not `Clone`: the
    /// duplicate is a deliberate act, not an accident of a derive.
    pub(super) fn duplicate(&self) -> Self {
        Self(self.0.clone())
    }
}

/// One verified read authority: the stable grant plus the exact private
/// credential the verification exercised (#14716). The credential is
/// carried from verification through the fetch; [`fmt::Debug`] redacts it.
pub struct ReadProof {
    grant: ReadGrant,
    /// The canonical credential-free repository URL this proof verified:
    /// the exact proved route a fetch must reuse (#14716) — the same forge
    /// base with a different owner/name is NOT this proof's route.
    origin: String,
    credential: Credential,
    /// The credential's lifetime, when the provider supplies one. `None`
    /// means no durable expiry: the proof holds only for the bounded
    /// in-flight operation it was verified for, and publication must
    /// reverify against the forge first.
    expires_at: Option<SystemTime>,
}

impl ReadProof {
    /// The grant this proof verified.
    #[must_use]
    pub fn grant(&self) -> &ReadGrant {
        &self.grant
    }

    /// The exact canonical route this proof verified: the credential-free
    /// repository URL a mirror fetch must reuse — no redirect, no other
    /// spelling of another repository (#14716).
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The exact credential, for the mirror fetch's private transport only.
    pub(super) fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The credential's lifetime; `None` = in-flight only (see the field).
    #[must_use]
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }

    /// Whether the credential's lifetime still covers `now`. A proof with
    /// no durable expiry is in-flight only and never time-checked: the
    /// operation that verified it must be the one that uses it.
    #[must_use]
    fn live_at(&self, now: SystemTime) -> bool {
        self.expires_at.is_none_or(|exp| exp > now)
    }
}

impl fmt::Debug for ReadProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadProof")
            .field("grant", &self.grant)
            .field("credential", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// How an incoming authority differs from a pinned one (#14716).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorityGap {
    /// A different repository identity — including a reused slug whose
    /// provider-issued ID names a replacement repository.
    DifferentRepository,
    /// A credential whose approved scope is not the one the store pinned
    /// (two different scopes can sit on one URL; only the pinned one binds).
    IncompatibleScope,
    /// A different principal at the same repository: another App
    /// installation or Gitea principal cannot inherit the binding on URL
    /// equality alone.
    DifferentAuthority,
}

/// Why a new borrower was refused admission to a retained store (#14716).
/// Every variant is fail-closed: the refusal names the operation it
/// stopped, never a downgrade to a weaker check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// No proof accompanied the attach.
    NoProof,
    /// The store pins no grant: there is no proved authority to inherit.
    StoreNotProved,
    /// The proof's credential lifetime is over.
    Expired,
    /// The proof's authority differs from the pinned grant.
    Authority(AuthorityGap),
}

/// The admission decision for a NEW borrower against a store's pinned
/// authority (#14716).
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// The borrower may attach; the pinned authority stays as it is.
    Admitted { grant: ReadGrant },
    /// Refused; the retained store and its previous object set stay usable
    /// by already-attached borrowers.
    Refused { reason: AdmissionRefusal },
}

/// The authority state of one attached borrower as the store reports it
/// (#14716).
#[derive(Debug, PartialEq, Eq)]
pub enum BorrowerAuthority {
    /// The borrower still pins exactly this grant.
    Pinned(ReadGrant),
    /// The store could not read the borrower's current state: treated as
    /// lost, never as fine.
    Unknown,
}

/// Why fresh publication to existing borrowers was withheld (#14716).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicationRefusal {
    /// No fresh proof accompanied the selected fetch.
    NoProof,
    /// The fetch proof's credential lifetime is over.
    Expired,
    /// The fetch proof does not match the store's pinned authority.
    Authority(AuthorityGap),
    /// An attached borrower's current state could not be read.
    BorrowerStateMissing,
    /// An attached borrower no longer holds the pinned authority.
    BorrowerAuthorityLost,
}

/// The publication decision for fresh objects to EXISTING borrowers
/// (#14716). `Withheld` never touches what the store already holds: the
/// previous object set and running seat work stay usable; only new bytes
/// stop flowing to an unverified borrower.
#[derive(Debug, PartialEq, Eq)]
pub enum Publication {
    /// New objects may be published under the pinned authority.
    Publish { grant: ReadGrant },
    /// Withheld.
    Withheld { reason: PublicationRefusal },
}

/// Classify how `incoming` differs from `pinned`, if it does. Repository
/// identity first: a reused slug must read as a replacement repository, not
/// as a scope or principal gap. Visible to the private-workspace module
/// tree so its classification order is testable beside the decisions.
pub(super) fn authority_gap(incoming: &ReadGrant, pinned: &ReadGrant) -> Option<AuthorityGap> {
    if incoming.identity != pinned.identity {
        return Some(AuthorityGap::DifferentRepository);
    }
    if incoming.scope != pinned.scope {
        return Some(AuthorityGap::IncompatibleScope);
    }
    if incoming.principal != pinned.principal {
        return Some(AuthorityGap::DifferentAuthority);
    }
    None
}

/// Whether a new borrower may attach to a retained store (#14716).
///
/// Fail-closed on every leg: the borrower must present a live proof, the
/// store must already pin a proved authority, and the proof's grant must be
/// the SAME authority (identity, scope, principal — rotation compares
/// equal because lifetime is not part of a grant).
#[must_use]
pub fn admit_borrower(
    proof: Option<&ReadProof>,
    pinned: Option<&ReadGrant>,
    now: SystemTime,
) -> Admission {
    let Some(proof) = proof else {
        return Admission::Refused {
            reason: AdmissionRefusal::NoProof,
        };
    };
    let Some(pinned) = pinned else {
        return Admission::Refused {
            reason: AdmissionRefusal::StoreNotProved,
        };
    };
    if !proof.live_at(now) {
        return Admission::Refused {
            reason: AdmissionRefusal::Expired,
        };
    }
    if let Some(gap) = authority_gap(proof.grant(), pinned) {
        return Admission::Refused {
            reason: AdmissionRefusal::Authority(gap),
        };
    }
    Admission::Admitted {
        grant: pinned.clone(),
    }
}

/// Whether fresh objects may be published to a store's EXISTING borrowers
/// (#14716).
///
/// Requires a fresh proof for the selected fetch that still matches the
/// pinned authority, and matching current authority for EVERY attached
/// borrower in the same decision. A borrower whose state is unreadable is
/// missing, not fine. `Withheld` stops only the new bytes: previously
/// published objects stay available to running seats.
#[must_use]
pub fn publish(
    proof: Option<&ReadProof>,
    pinned: &ReadGrant,
    borrowers: &[BorrowerAuthority],
    now: SystemTime,
) -> Publication {
    let Some(proof) = proof else {
        return Publication::Withheld {
            reason: PublicationRefusal::NoProof,
        };
    };
    if !proof.live_at(now) {
        return Publication::Withheld {
            reason: PublicationRefusal::Expired,
        };
    }
    if let Some(gap) = authority_gap(proof.grant(), pinned) {
        return Publication::Withheld {
            reason: PublicationRefusal::Authority(gap),
        };
    }
    for borrower in borrowers {
        match borrower {
            BorrowerAuthority::Unknown => {
                return Publication::Withheld {
                    reason: PublicationRefusal::BorrowerStateMissing,
                };
            }
            BorrowerAuthority::Pinned(grant) if !grant.same_authority(pinned) => {
                return Publication::Withheld {
                    reason: PublicationRefusal::BorrowerAuthorityLost,
                };
            }
            BorrowerAuthority::Pinned(_) => {}
        }
    }
    Publication::Publish {
        grant: pinned.clone(),
    }
}

/// What one authenticated provider exchange answered. Anything but
/// [`ProbeAnswer::Ok`] refuses the leg it served (#14716): a redirect is a
/// redirect, never followed; an indeterminate reply is not a pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// 2xx, with this body.
    Ok(String),
    /// Any other HTTP status.
    Status(u16),
    /// 3xx redirecting toward `location` — never followed.
    Redirect(String),
    /// No dependable HTTP answer at all.
    Transport(String),
}

/// One authenticated GET against a forge route (#14716). Production shells
/// `curl` with the Authorization header on stdin (never argv) and never
/// follows redirects; tests script a fake so every provider route is
/// hermetic.
pub trait ProviderHttp {
    /// GET `url` authenticated with `credential` under `scheme`, sending
    /// `accept`. Redirects are never followed: a 3xx is reported as
    /// [`ProbeAnswer::Redirect`].
    fn get(
        &self,
        url: &str,
        scheme: AuthScheme,
        accept: &str,
        credential: &Credential,
    ) -> ProbeAnswer;
}

/// GitHub narrow issuance (#14716): the existing authorized App issuer,
/// asked for ONE repository with Contents read. The token and the scope
/// bound to it at mint arrive PAIRED in one in-memory value — a scoped
/// result never enters the writer token cache or the accounting sidecar
/// path (`credential_preflight::minted`).
pub trait ScopedMinter {
    /// Mint a fresh read credential for `owner_repo` with its paired
    /// issuance evidence. Always fresh: a scoped mint is per-operation and
    /// is never read from or written to any cache.
    fn mint_scoped(&self, owner_repo: &str) -> ScopedOutcome;
}

/// The outcome of one scoped-mint request (#14716).
pub enum ScopedOutcome {
    /// The paired mint: token and the evidence the issuer bound to it.
    Minted(crate::credential_preflight::Minted),
    /// No authorized issuer is configured: the legacy path applies.
    NotConfigured,
    /// The issuer refused or could not mint (revoked key, installation
    /// without Contents read, network failure, …).
    Refused(String),
}

/// The origin and credential selection one verification acts on (#14716).
///
/// The credential is selected BY NAME by the caller (a configured Gitea
/// token; on GitHub the issuer mints the credential itself), never picked
/// up ambiently: an operation with no selected credential and no issuer is
/// [`Unproved::NoCredential`], not a fallback to whatever the environment
/// happens to carry.
pub struct VerificationRequest {
    /// The registered canonical credential-free HTTPS repository URL.
    pub url: String,
    /// The configured GitHub host (`GH_HOST`); default `github.com`.
    pub github_host: Option<String>,
    /// The exact credential selected for a Gitea route. Ignored on GitHub.
    pub gitea_credential: Option<Credential>,
}

/// Why a read authority could not be proved (#14716). Every variant is
/// fail-closed — the caller stays on the legacy private-clone path, which
/// this module leaves untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unproved {
    /// The origin is not a canonical `https://host/[subpath/]owner/name`.
    UnsupportedUrl(String),
    /// No credential was selected and no issuer minted one.
    NoCredential,
    /// No dependable HTTP answer from the forge.
    Transport(String),
    /// The route redirected toward another origin.
    Redirect(String),
    /// The credential is revoked, invalid or too narrow (401/403).
    NotAuthenticated,
    /// The repository is invisible to this credential (404: gone, private
    /// or replaced under the same slug).
    NotFound,
    /// The credential cannot be tied to the same resource ID.
    IdentityMismatch,
    /// The provider offers no code read for this credential (metadata-only
    /// scope, disabled unit, narrowed token).
    ScopeUnavailable(String),
    /// The credential's lifetime is over.
    Expired,
    /// The authorized App issuer is unavailable or refused the narrow
    /// issuance.
    IssuerUnavailable(String),
    /// The provider answered, but the evidence is malformed or does not
    /// pair with the credential it describes.
    BadEvidence(String),
}

/// The result of one verification (#14716).
pub enum Verification {
    /// Proved: the grant holds and the exact credential exercised it.
    Proved(ReadProof),
    /// Unproved; use the legacy private-clone path.
    Unproved(Unproved),
}

/// Prove the read authority of `request`'s origin for one in-flight
/// operation (#14716).
///
/// GitHub: the authorized App issuer mints a credential for exactly one
/// repository with Contents read, and the same credential must read the
/// repository back with the issuer-bound numeric ID. Gitea: the selected
/// credential must resolve the repository's numeric ID, authenticate a
/// stable principal and exercise effective Code read on the exact private
/// Git-over-HTTPS route. Every route uses the explicitly selected
/// credential and canonical origin; redirects refuse.
pub fn verify(
    request: &VerificationRequest,
    http: &dyn ProviderHttp,
    minter: &dyn ScopedMinter,
    now: SystemTime,
) -> Verification {
    let Some(base) = ForgeBase::of_repository_url(&request.url) else {
        return Verification::Unproved(Unproved::UnsupportedUrl(
            "a read proof needs an https://host/[subpath/]owner/name origin".into(),
        ));
    };
    // The canonical spelling the proof pins: identify's normalized URL.
    let Ok(canonical) = super::identify(&request.url).map(|id| id.url) else {
        return Verification::Unproved(Unproved::UnsupportedUrl(
            "a read proof needs an https://host/[subpath/]owner/name origin".into(),
        ));
    };
    let Some((owner, name)) = owner_name(&request.url) else {
        return Verification::Unproved(Unproved::UnsupportedUrl(
            "the origin does not name an owner/repository".into(),
        ));
    };
    let github_host = request
        .github_host
        .as_deref()
        .filter(|host| !host.is_empty())
        .unwrap_or("github.com");
    match Provider::of_base(&base, github_host) {
        Provider::GitHub => verify_github(&base, &canonical, &owner, &name, http, minter, now),
        Provider::Gitea => match &request.gitea_credential {
            Some(credential) => verify_gitea(&base, &canonical, &owner, &name, credential, http),
            None => Verification::Unproved(Unproved::NoCredential),
        },
    }
}

/// The `owner` and repository `name` of a canonical URL.
fn owner_name(url: &str) -> Option<(String, String)> {
    let path = reqwest::Url::parse(url).ok()?.path().to_string();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let [.., owner, name] = segments[..] else {
        return None;
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_owned(), name.to_owned()))
}

/// The API base a GitHub forge serves repository metadata from: `github.com`
/// serves from `api.github.com`, an Enterprise host from its own `/api/v3`.
fn github_api_base(base: &ForgeBase) -> String {
    if base.authority().eq_ignore_ascii_case("github.com") {
        "https://api.github.com".to_owned()
    } else {
        format!("https://{}/api/v3", base.authority())
    }
}

/// Classify one probe answer against the fail-closed vocabulary.
fn refused_by(answer: ProbeAnswer) -> Unproved {
    match answer {
        ProbeAnswer::Ok(body) => Unproved::BadEvidence(format!(
            "the route answered, but the evidence does not pair: {} bytes, not usable",
            body.len()
        )),
        ProbeAnswer::Status(401 | 403) => Unproved::NotAuthenticated,
        ProbeAnswer::Status(404) => Unproved::NotFound,
        ProbeAnswer::Status(code) => Unproved::Transport(format!("the forge answered HTTP {code}")),
        ProbeAnswer::Redirect(to) => Unproved::Redirect(to),
        ProbeAnswer::Transport(e) => Unproved::Transport(e),
    }
}

fn verify_github(
    base: &ForgeBase,
    canonical: &str,
    owner: &str,
    name: &str,
    http: &dyn ProviderHttp,
    minter: &dyn ScopedMinter,
    now: SystemTime,
) -> Verification {
    let nwo = format!("{owner}/{name}");
    let minted = match minter.mint_scoped(&nwo) {
        ScopedOutcome::Minted(minted) => minted,
        ScopedOutcome::NotConfigured => {
            return Verification::Unproved(Unproved::IssuerUnavailable(
                "no authorized GitHub App issuer is configured".into(),
            ))
        }
        ScopedOutcome::Refused(reason) => {
            return Verification::Unproved(Unproved::IssuerUnavailable(reason))
        }
    };
    let Some(scoped) = minted.scoped.clone() else {
        return Verification::Unproved(Unproved::BadEvidence(
            "the scoped mint carried no issuance evidence".into(),
        ));
    };
    if scoped.repository_ids.len() != 1 {
        return Verification::Unproved(Unproved::BadEvidence(format!(
            "a read proof needs exactly one repository id; the issuer bound {}",
            scoped.repository_ids.len()
        )));
    }
    if scoped.permissions.get("contents").map(String::as_str) != Some("read") {
        return Verification::Unproved(Unproved::ScopeUnavailable(
            "the issuer did not bind Contents read to this credential".into(),
        ));
    }
    let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(&minted.expires_at) else {
        return Verification::Unproved(Unproved::BadEvidence(
            "the scoped mint carried no parseable expiry".into(),
        ));
    };
    let expires_at: SystemTime = expiry.with_timezone(&chrono::Utc).into();
    if expires_at <= now {
        return Verification::Unproved(Unproved::Expired);
    }
    // Identity and authenticated private Contents access, with the exact
    // minted credential: the resource must answer with the SAME numeric ID
    // the issuer bound to it. A reused slug whose ID moved, or a token that
    // cannot read the repository privately, refuses here.
    let repository_id = scoped.repository_ids[0].clone();
    let credential = Credential::new(minted.token.clone());
    let url = format!("{}/repos/{owner}/{name}", github_api_base(base));
    let answer = http.get(&url, AuthScheme::Bearer, "application/vnd.github+json", &credential);
    match answer {
        ProbeAnswer::Ok(body) => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return Verification::Unproved(Unproved::BadEvidence(
                    "the repository metadata answer was not JSON".into(),
                ));
            };
            match value.get("id").and_then(serde_json::Value::as_u64) {
                Some(id) if id.to_string() == repository_id => {}
                Some(_) => return Verification::Unproved(Unproved::IdentityMismatch),
                None => {
                    return Verification::Unproved(Unproved::BadEvidence(
                        "the repository metadata answer named no numeric id".into(),
                    ))
                }
            }
        }
        otherwise => return Verification::Unproved(refused_by(otherwise)),
    }
    let grant = ReadGrant {
        identity: RepositoryIdentity {
            provider: Provider::GitHub,
            base: base.clone(),
            repository_id,
        },
        scope: ReadScope::GitHubContentsRead,
        principal: Principal::GitHubApp {
            app_id: minted.app_id,
            installation_id: minted.installation_id,
        },
    };
    Verification::Proved(ReadProof {
        grant,
        origin: canonical.to_owned(),
        credential,
        expires_at: Some(expires_at),
    })
}

fn verify_gitea(
    base: &ForgeBase,
    canonical: &str,
    owner: &str,
    name: &str,
    credential: &Credential,
    http: &dyn ProviderHttp,
) -> Verification {
    let scheme = AuthScheme::Token;
    // 1. Identity: the instance-issued numeric repository ID, with the
    //    credential's EFFECTIVE permission on it (Code read is a distinct
    //    unit; a metadata-only answer does not qualify).
    let repo_url = format!("{}/api/v1/repos/{owner}/{name}", base.as_str());
    let repository_id = match http.get(&repo_url, scheme, "application/json", credential) {
        ProbeAnswer::Ok(body) => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return Verification::Unproved(Unproved::BadEvidence(
                    "the repository answer was not JSON".into(),
                ));
            };
            let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
                return Verification::Unproved(Unproved::BadEvidence(
                    "the repository answer named no numeric id".into(),
                ));
            };
            let code_read = value
                .get("permissions")
                .and_then(|p| p.get("pull"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !code_read {
                return Verification::Unproved(Unproved::ScopeUnavailable(
                    "the credential has no effective Code read on this repository".into(),
                ));
            }
            id.to_string()
        }
        otherwise => return Verification::Unproved(refused_by(otherwise)),
    };
    // 2. Principal: the AUTHENTICATED user by immutable numeric ID — a
    //    mutable username never qualifies.
    let user_url = format!("{}/api/v1/user", base.as_str());
    let user_id = match http.get(&user_url, scheme, "application/json", credential) {
        ProbeAnswer::Ok(body) => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return Verification::Unproved(Unproved::BadEvidence(
                    "the principal answer was not JSON".into(),
                ));
            };
            match value.get("id").and_then(serde_json::Value::as_u64) {
                Some(id) => id.to_string(),
                None => {
                    return Verification::Unproved(Unproved::BadEvidence(
                        "the principal answer named no numeric id".into(),
                    ))
                }
            }
        }
        otherwise => return Verification::Unproved(refused_by(otherwise)),
    };
    // 3. Effective Code read on the exact private Git-over-HTTPS route, with
    //    the same credential: the smart upload-pack advertisement must come
    //    back. This is the route the mirror fetch will use.
    let refs_url =
        format!("{}/{owner}/{name}.git/info/refs?service=git-upload-pack", base.as_str());
    match http.get(&refs_url, scheme, "*/*", credential) {
        ProbeAnswer::Ok(body) if body.starts_with("001e# service=git-upload-pack") => {}
        otherwise => return Verification::Unproved(refused_by(otherwise)),
    }
    // Gitea supplies no durable per-token expiry: the proof holds only for
    // this in-flight operation, and publication must reverify.
    let grant = ReadGrant {
        identity: RepositoryIdentity {
            provider: Provider::Gitea,
            base: base.clone(),
            repository_id,
        },
        scope: ReadScope::GiteaCodeRead,
        principal: Principal::GiteaUser { user_id },
    };
    Verification::Proved(ReadProof {
        grant,
        origin: canonical.to_owned(),
        credential: credential.duplicate(),
        expires_at: None,
    })
}

/// Parse `github-app-token.sh scoped-token`'s JSON envelope into the paired
/// mint (#14716). Split from I/O so it is unit-testable without a
/// subprocess. The evidence must arrive WITH the token it describes; an
/// incomplete or unpaired envelope is refused, never patched up.
pub(crate) fn parse_scoped_envelope(stdout: &str, owner_repo: &str) -> ScopedOutcome {
    let parsed: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(parsed) => parsed,
        Err(e) => {
            return ScopedOutcome::Refused(format!(
                "could not parse github-app-token.sh scoped-token output ({e})"
            ))
        }
    };
    let status = parsed
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match status {
        "not_configured" => ScopedOutcome::NotConfigured,
        "ok" => {
            let field = |key: &str| {
                parsed
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let token = field("token");
            let installation_id = field("installation_id");
            let app_id = field("app_id");
            let expires_at = field("expires_at");
            let ids = parsed
                .get("repository_ids")
                .and_then(serde_json::Value::as_array);
            let Some(ids) = ids else {
                return ScopedOutcome::Refused(
                    "the scoped mint envelope named no repository ids".into(),
                );
            };
            let repository_ids: Vec<String> = ids
                .iter()
                .filter_map(|value| value.as_u64().map(|id| id.to_string()))
                .collect();
            if token.is_empty()
                || installation_id.is_empty()
                || repository_ids.len() != ids.len()
                || repository_ids.is_empty()
            {
                return ScopedOutcome::Refused(
                    "the scoped mint envelope is incomplete (token, installation or repository \
                     ids)"
                        .into(),
                );
            }
            let mut permissions = BTreeMap::new();
            if let Some(map) = parsed
                .get("permissions")
                .and_then(serde_json::Value::as_object)
            {
                for (key, value) in map {
                    if let Some(level) = value.as_str() {
                        permissions.insert(key.clone(), level.to_owned());
                    }
                }
            }
            let owner = crate::credential_preflight::owner_of_nwo(owner_repo).to_ascii_lowercase();
            ScopedOutcome::Minted(crate::credential_preflight::Minted::read_authority(
                token,
                app_id,
                installation_id,
                owner,
                expires_at,
                crate::credential_preflight::ScopedIssuance {
                    repository_ids,
                    permissions,
                },
            ))
        }
        _ => {
            let message = parsed
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("github-app-token.sh reported an unrecognized status");
            ScopedOutcome::Refused(message.to_owned())
        }
    }
}

/// The live provider transport (#14716): `curl` with the Authorization
/// header on stdin (never argv), bounded `--max-time`, redirects NEVER
/// followed — `curl` reports one through `%{redirect_url}` instead.
pub struct LiveProviderHttp {
    /// Positive bound per call; `0` would disable curl's limit entirely.
    pub timeout: Duration,
}

impl ProviderHttp for LiveProviderHttp {
    fn get(
        &self,
        url: &str,
        scheme: AuthScheme,
        accept: &str,
        credential: &Credential,
    ) -> ProbeAnswer {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        if self.timeout.is_zero() {
            return ProbeAnswer::Transport(
                "timeout must be positive (0 would disable the per-call bound)".into(),
            );
        }
        let mut cmd = Command::new("curl");
        cmd.arg("-q")
            .arg("--globoff")
            .arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg(self.timeout.as_secs_f64().to_string())
            .arg("-o")
            .arg("-")
            .arg("-w")
            .arg("\n%{http_code} %{redirect_url}")
            .arg("-X")
            .arg("GET")
            .arg("-H")
            .arg("@-")
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return ProbeAnswer::Transport(format!("spawn curl: {e}")),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(
                format!(
                    "Authorization: {} {}\nAccept: {}\n",
                    scheme.prefix(),
                    credential.value(),
                    accept
                )
                .as_bytes(),
            );
        }
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(e) => return ProbeAnswer::Transport(format!("curl wait: {e}")),
        };
        if !output.status.success() {
            return ProbeAnswer::Transport(format!(
                "curl exit {}: body withheld",
                output.status.code().unwrap_or(-1)
            ));
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let Some((body, tail)) = raw.rsplit_once('\n') else {
            return ProbeAnswer::Transport("unparseable curl reply".into());
        };
        let Some((code, redirect)) = tail.trim().split_once(' ') else {
            return ProbeAnswer::Transport(format!("unparseable status line from curl: {tail:?}"));
        };
        let Ok(code) = code.parse::<u16>() else {
            return ProbeAnswer::Transport(format!("unparseable status line from curl: {tail:?}"));
        };
        if (300..400).contains(&code) {
            return ProbeAnswer::Redirect(redirect.trim().to_owned());
        }
        if (200..300).contains(&code) {
            ProbeAnswer::Ok(body.to_owned())
        } else {
            ProbeAnswer::Status(code)
        }
    }
}

/// The scoped minter against the shipped issuer boundary (#14716):
/// `github-app-token.sh scoped-token OWNER/REPO` — a narrow issuance
/// request (one repository, Contents read) whose envelope carries the token
/// and the evidence the issuer bound to it. The ordinary `get-token`
/// writer mint is untouched.
pub struct ScriptScopedMinter {
    pub script_path: PathBuf,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ScopedMinter for ScriptScopedMinter {
    fn mint_scoped(&self, owner_repo: &str) -> ScopedOutcome {
        let script = self.script_path.to_string_lossy().to_string();
        match crate::main_health_gate::run_capture_with_timeout(
            "bash",
            &[&script, "scoped-token", owner_repo],
            &self.cwd,
            self.timeout,
        ) {
            Ok(stdout) => parse_scoped_envelope(&stdout, owner_repo),
            Err(e) => ScopedOutcome::Refused(format!(
                "could not run github-app-token.sh scoped-token ({e})"
            )),
        }
    }
}
