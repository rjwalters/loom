//! Runtime-loadable model rate card (`defaults/pricing.json` -> `.loom/pricing.json`).
//!
//! Issue #8177 (ask 2 of #8060). The rate card used to exist ONLY as the
//! hardcoded cascade in [`super::resource_usage::ModelPricing::lookup`], which
//! meant every vendor price change needed a code edit, a merge, and a Loom
//! release before any consumer repo's cost telemetry stopped being wrong. The
//! same table now also ships as a JSON asset that `resync-installed.sh`
//! delivers to `.loom/pricing.json`, so a price change reaches the fleet on a
//! resync instead of on a release.
//!
//! # Failure discipline
//!
//! A pricing table that is silently wrong is worse than one that is loudly
//! missing — a zero rate or a stale default produces cost telemetry that looks
//! plausible and is not. So:
//!
//! - The asset is **all-or-nothing**. Every row is parsed and validated before
//!   any of it is used; a malformed or partial document is rejected as a
//!   [`PricingCardError`] that names the concrete failure, never partially
//!   applied.
//! - A rejected or absent asset falls back to the **compiled** card (the same
//!   rates, frozen at build time) and logs at **warn** — the same loudness
//!   discipline #8060 established for an unrecognized model id. It never falls
//!   back to zero, and it never invents a default row.
//! - The asset carries its own `verified_on` date. Past
//!   [`STALENESS_THRESHOLD_DAYS`] it is still used (a stale published rate is
//!   much closer to right than no rate at all) but its age is logged at
//!   **warn**, so "nobody has re-checked the vendor page in a quarter" is
//!   visible rather than silently trusted.
//!
//! Resolution happens once per process ([`active`]), so each of those warnings
//! is emitted at most once per daemon rather than once per costed record.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{NaiveDate, Utc};
use serde::Deserialize;

use super::resource_usage::ModelPricing;

/// The only `schema_version` this build understands.
///
/// A newer asset (a schema change, not a price change) is rejected wholesale
/// rather than read on a best-effort basis: an older daemon guessing at a
/// newer schema is exactly the "partially applied" failure this module exists
/// to prevent. Price changes do not bump this — that is the entire point.
pub const SCHEMA_VERSION: u32 = 1;

/// Age past which the asset's `verified_on` date is reported at warn level.
///
/// One quarter. Long enough that a routine release cadence never trips it,
/// short enough that a rate card nobody has re-checked against the vendor's
/// published page becomes visible well before it has drifted through several
/// model generations.
pub const STALENESS_THRESHOLD_DAYS: i64 = 90;

/// Repo-relative location of the resync-delivered asset.
pub const ASSET_REL: &str = ".loom/pricing.json";

/// Env override for the asset path.
///
/// Set to an explicit path to load from there instead of [`ASSET_REL`]; set to
/// the empty string to disable the asset entirely and pin the process to the
/// compiled card. (Both forms are used by tests, which must never depend on
/// whatever the host checkout happens to have installed.)
pub const ASSET_ENV: &str = "LOOM_PRICING_CARD";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an asset could not be used. Every variant names the concrete failure
/// and the file it came from; none of them is recoverable into a partial card.
#[derive(Debug)]
pub enum PricingCardError {
    /// The file is absent, or present and unreadable.
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The bytes are not valid JSON, or do not fit the document schema
    /// (missing field, wrong type, unknown field).
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Valid JSON of a schema this build does not understand.
    UnsupportedSchemaVersion { path: PathBuf, found: u32 },
    /// Schema-shaped but semantically invalid: names the offending field and
    /// why it was rejected.
    Invalid {
        path: PathBuf,
        field: String,
        reason: String,
    },
}

impl PricingCardError {
    /// The file this error is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Unreadable { path, .. }
            | Self::Malformed { path, .. }
            | Self::UnsupportedSchemaVersion { path, .. }
            | Self::Invalid { path, .. } => path,
        }
    }

    /// Whether this is the ordinary "no asset installed here" case, as opposed
    /// to an asset that exists and is broken.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Unreadable { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

impl std::fmt::Display for PricingCardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { path, source } => {
                write!(f, "pricing asset {} could not be read: {source}", path.display())
            }
            Self::Malformed { path, source } => {
                write!(f, "pricing asset {} is malformed: {source}", path.display())
            }
            Self::UnsupportedSchemaVersion { path, found } => write!(
                f,
                "pricing asset {} declares schema_version {found}, but this build only \
                 understands {SCHEMA_VERSION}",
                path.display()
            ),
            Self::Invalid {
                path,
                field,
                reason,
            } => write!(f, "pricing asset {} is invalid at `{field}`: {reason}", path.display()),
        }
    }
}

impl std::error::Error for PricingCardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Malformed { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Document schema (serde)
// ---------------------------------------------------------------------------

/// Which cache-read multiplier a derived row uses.
///
/// Named rather than a bare number so the Fable 5.1 / Mythos 5.1 exception is
/// stated once in `cache_multipliers` and referenced per row, instead of being
/// a magic constant repeated across rows where a typo is invisible.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CacheReadTier {
    Standard,
    Reduced,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheMultipliersDoc {
    read: f64,
    read_reduced: f64,
    write_5m: f64,
    write_1h: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplicitCacheRatesDoc {
    read_per_1k: f64,
    write_5m_per_1k: f64,
    write_1h_per_1k: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowDoc {
    id: String,
    #[serde(rename = "match")]
    match_on: Vec<String>,
    input_per_1k: f64,
    output_per_1k: f64,
    /// Derive the three cache rates from the shared multipliers. Mutually
    /// exclusive with `cache_rates`.
    cache_read: Option<CacheReadTier>,
    /// Published cache rates that are NOT a multiple of this row's base input
    /// (the OpenAI rows). Mutually exclusive with `cache_read`.
    cache_rates: Option<ExplicitCacheRatesDoc>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardDoc {
    schema_version: u32,
    verified_on: String,
    source: String,
    /// Free-text provenance/maintenance note, addressed to whoever edits the
    /// asset. Declared (and deliberately never read) so `deny_unknown_fields`
    /// does not reject an asset for carrying one.
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<String>,
    cache_multipliers: CacheMultipliersDoc,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    rows: Vec<RowDoc>,
}

// ---------------------------------------------------------------------------
// Validated runtime form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Row {
    id: String,
    match_on: Vec<String>,
    pricing: ModelPricing,
}

/// A parsed, fully validated rate card.
#[derive(Debug, Clone)]
pub struct PricingCard {
    path: PathBuf,
    verified_on: NaiveDate,
    source: String,
    aliases: BTreeMap<String, String>,
    rows: Vec<Row>,
}

impl PricingCard {
    /// Parse and validate the asset at `path`.
    ///
    /// All-or-nothing: on `Err` nothing has been applied and the caller must
    /// use the compiled card.
    pub fn load(path: &Path) -> Result<Self, PricingCardError> {
        let text =
            std::fs::read_to_string(path).map_err(|source| PricingCardError::Unreadable {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_str_at(&text, path)
    }

    /// [`Self::load`] against an in-memory document, for tests and for callers
    /// that already hold the bytes.
    pub fn from_str_at(text: &str, path: &Path) -> Result<Self, PricingCardError> {
        let invalid = |field: &str, reason: String| PricingCardError::Invalid {
            path: path.to_path_buf(),
            field: field.to_string(),
            reason,
        };

        let doc: CardDoc =
            serde_json::from_str(text).map_err(|source| PricingCardError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;

        if doc.schema_version != SCHEMA_VERSION {
            return Err(PricingCardError::UnsupportedSchemaVersion {
                path: path.to_path_buf(),
                found: doc.schema_version,
            });
        }

        let verified_on = NaiveDate::parse_from_str(&doc.verified_on, "%Y-%m-%d").map_err(|e| {
            invalid("verified_on", format!("{:?} is not a YYYY-MM-DD date: {e}", doc.verified_on))
        })?;

        for (name, value) in [
            ("read", doc.cache_multipliers.read),
            ("read_reduced", doc.cache_multipliers.read_reduced),
            ("write_5m", doc.cache_multipliers.write_5m),
            ("write_1h", doc.cache_multipliers.write_1h),
        ] {
            check_rate(&format!("cache_multipliers.{name}"), value, &invalid)?;
        }

        if doc.rows.is_empty() {
            return Err(invalid("rows", "the rate card has no rows".to_string()));
        }

        let mut rows = Vec::with_capacity(doc.rows.len());
        let mut seen_ids: Vec<&str> = Vec::with_capacity(doc.rows.len());
        for (i, row) in doc.rows.iter().enumerate() {
            let at = |suffix: &str| format!("rows[{i}].{suffix}");

            if row.id.trim().is_empty() {
                return Err(invalid(&at("id"), "row id is empty".to_string()));
            }
            if seen_ids.contains(&row.id.as_str()) {
                return Err(invalid(&at("id"), format!("duplicate row id {:?}", row.id)));
            }
            seen_ids.push(&row.id);

            if row.match_on.is_empty() {
                return Err(invalid(&at("match"), format!("row {:?} matches nothing", row.id)));
            }
            if row.match_on.iter().any(|m| m.trim().is_empty()) {
                return Err(invalid(
                    &at("match"),
                    format!("row {:?} has an empty match pattern", row.id),
                ));
            }

            check_rate(&at("input_per_1k"), row.input_per_1k, &invalid)?;
            check_rate(&at("output_per_1k"), row.output_per_1k, &invalid)?;

            let pricing = match (row.cache_read, row.cache_rates.as_ref()) {
                (Some(_), Some(_)) => {
                    return Err(invalid(
                        &at("cache_read"),
                        format!(
                            "row {:?} sets both `cache_read` and `cache_rates`; exactly one is \
                             required",
                            row.id
                        ),
                    ))
                }
                (None, None) => {
                    return Err(invalid(
                        &at("cache_read"),
                        format!(
                            "row {:?} sets neither `cache_read` nor `cache_rates`; exactly one \
                             is required",
                            row.id
                        ),
                    ))
                }
                (Some(tier), None) => {
                    let read_multiplier = match tier {
                        CacheReadTier::Standard => doc.cache_multipliers.read,
                        CacheReadTier::Reduced => doc.cache_multipliers.read_reduced,
                    };
                    ModelPricing {
                        input_cost_per_1k: row.input_per_1k,
                        output_cost_per_1k: row.output_per_1k,
                        cache_read_cost_per_1k: row.input_per_1k * read_multiplier,
                        cache_write_cost_per_1k: row.input_per_1k * doc.cache_multipliers.write_5m,
                        cache_write_1h_cost_per_1k: row.input_per_1k
                            * doc.cache_multipliers.write_1h,
                    }
                }
                (None, Some(explicit)) => {
                    check_rate(&at("cache_rates.read_per_1k"), explicit.read_per_1k, &invalid)?;
                    check_rate(
                        &at("cache_rates.write_5m_per_1k"),
                        explicit.write_5m_per_1k,
                        &invalid,
                    )?;
                    check_rate(
                        &at("cache_rates.write_1h_per_1k"),
                        explicit.write_1h_per_1k,
                        &invalid,
                    )?;
                    ModelPricing {
                        input_cost_per_1k: row.input_per_1k,
                        output_cost_per_1k: row.output_per_1k,
                        cache_read_cost_per_1k: explicit.read_per_1k,
                        cache_write_cost_per_1k: explicit.write_5m_per_1k,
                        cache_write_1h_cost_per_1k: explicit.write_1h_per_1k,
                    }
                }
            };

            rows.push(Row {
                id: row.id.clone(),
                match_on: row.match_on.iter().map(|m| m.to_lowercase()).collect(),
                pricing,
            });
        }

        for (alias, target) in &doc.aliases {
            if alias.trim().is_empty() {
                return Err(invalid("aliases", "an alias key is empty".to_string()));
            }
            let target_l = target.to_lowercase();
            if !rows
                .iter()
                .any(|r| r.match_on.iter().any(|m| target_l.contains(m.as_str())))
            {
                return Err(invalid(
                    &format!("aliases.{alias}"),
                    format!("alias target {target:?} matches no row"),
                ));
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            verified_on,
            source: doc.source,
            aliases: doc
                .aliases
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v.to_lowercase()))
                .collect(),
            rows,
        })
    }

    /// Look up `model` in this card.
    ///
    /// Same two-step contract as the compiled cascade: the id is lowercased,
    /// a bare tier alias is resolved to a concrete id, then the ORDERED rows
    /// are scanned for the first `match` substring the id contains.
    #[must_use]
    pub fn lookup(&self, model: &str) -> Option<ModelPricing> {
        let lowered = model.to_lowercase();
        let resolved: &str = self
            .aliases
            .get(&lowered)
            .map_or(lowered.as_str(), String::as_str);
        self.rows
            .iter()
            .find(|row| row.match_on.iter().any(|m| resolved.contains(m.as_str())))
            .map(|row| row.pricing.clone())
    }

    /// The id of the row `model` resolves to, for diagnostics.
    #[must_use]
    pub fn matching_row_id(&self, model: &str) -> Option<&str> {
        let lowered = model.to_lowercase();
        let resolved: &str = self
            .aliases
            .get(&lowered)
            .map_or(lowered.as_str(), String::as_str);
        self.rows
            .iter()
            .find(|row| row.match_on.iter().any(|m| resolved.contains(m.as_str())))
            .map(|row| row.id.as_str())
    }

    /// The date the rates in this card were last checked against `source`.
    #[must_use]
    pub const fn verified_on(&self) -> NaiveDate {
        self.verified_on
    }

    /// The published page the rates were verified against.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Where the card was loaded from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of rows in the cascade.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// A warn-level message when the card has not been re-verified within
    /// [`STALENESS_THRESHOLD_DAYS`], else `None`.
    #[must_use]
    pub fn staleness_warning(&self, today: NaiveDate) -> Option<String> {
        let age = (today - self.verified_on).num_days();
        (age > STALENESS_THRESHOLD_DAYS).then(|| {
            format!(
                "pricing asset {} was last verified on {} ({age} days ago, threshold \
                 {STALENESS_THRESHOLD_DAYS}); re-check {} and refresh defaults/pricing.json — \
                 cost telemetry is being computed from rates nobody has confirmed this quarter",
                self.path.display(),
                self.verified_on,
                self.source,
            )
        })
    }
}

/// Reject a rate that is negative or not finite. A NaN rate would poison every
/// downstream cost silently; a negative one would produce a negative bill.
fn check_rate<F>(field: &str, value: f64, invalid: &F) -> Result<(), PricingCardError>
where
    F: Fn(&str, String) -> PricingCardError,
{
    if !value.is_finite() {
        return Err(invalid(field, format!("{value} is not a finite number")));
    }
    if value < 0.0 {
        return Err(invalid(field, format!("{value} is negative")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Process-wide resolution
// ---------------------------------------------------------------------------

/// Resolve the asset path for this process, honoring [`ASSET_ENV`].
///
/// `None` means "the asset tier is disabled" — either `LOOM_PRICING_CARD` is
/// explicitly empty, or we are not inside a Loom repository at all (e.g. a CLI
/// helper invoked from `$HOME`), in which case there is no `.loom/` to read.
///
/// Resolution is **cwd-based**, not workspace-argument-based: pricing is a
/// vendor-global fact rather than a per-repo setting, and
/// [`ModelPricing::for_model`] is called from deep inside cost computation
/// with no repo handle to thread through. A multi-workspace daemon therefore
/// reads whichever checkout it was started in; when that is not the one you
/// want, name the file with [`ASSET_ENV`].
#[must_use]
pub fn asset_path() -> Option<PathBuf> {
    match std::env::var(ASSET_ENV) {
        Ok(p) if p.is_empty() => return None,
        Ok(p) => return Some(PathBuf::from(p)),
        Err(_) => {}
    }
    crate::repo_root::find_repo_root_from_cwd().map(|root| root.join(ASSET_REL))
}

/// Load the asset, logging the outcome exactly once (this is called from a
/// `LazyLock`). Returns `None` whenever the compiled card must be used.
fn resolve_active() -> Option<PricingCard> {
    let path = asset_path()?;
    match PricingCard::load(&path) {
        Ok(card) => {
            if let Some(msg) = card.staleness_warning(Utc::now().date_naive()) {
                log::warn!("{msg}");
            }
            log::debug!(
                "Loaded pricing card from {} ({} rows, verified {})",
                card.path().display(),
                card.row_count(),
                card.verified_on()
            );
            Some(card)
        }
        Err(e) if e.is_absent() => {
            log::warn!(
                "No pricing asset at {} — falling back to the rate card compiled into this \
                 loom-daemon build. Rates are correct as of the build, but a vendor price change \
                 will not reach this repo until `.loom/scripts/resync-installed.sh` delivers \
                 defaults/pricing.json.",
                path.display()
            );
            None
        }
        Err(e) => {
            log::warn!(
                "{e} — falling back to the rate card compiled into this loom-daemon build. The \
                 asset was NOT partially applied; fix or delete it and restart.",
            );
            None
        }
    }
}

static ACTIVE: LazyLock<Option<PricingCard>> = LazyLock::new(resolve_active);

/// The rate card in effect for this process, or `None` when the compiled card
/// is in use. Resolved (and logged) once.
#[must_use]
pub fn active() -> Option<&'static PricingCard> {
    ACTIVE.as_ref()
}

/// Absolute path to the in-repo source asset, `defaults/pricing.json`.
///
/// Test-facing: the parity test between the shipped JSON and the compiled card
/// must read the SOURCE of truth, not whatever a host checkout happens to have
/// installed under `.loom/`.
#[cfg(test)]
pub(crate) fn shipped_asset_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ always has a parent")
        .join("defaults")
        .join("pricing.json")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn shipped() -> PricingCard {
        let path = shipped_asset_path();
        PricingCard::load(&path)
            .unwrap_or_else(|e| panic!("defaults/pricing.json must be valid: {e}"))
    }

    fn doc_with(rows: &str) -> String {
        format!(
            r#"{{
              "schema_version": 1,
              "verified_on": "2026-09-18",
              "source": "https://example.invalid/pricing",
              "cache_multipliers": {{
                "read": 0.1, "read_reduced": 0.025, "write_5m": 1.25, "write_1h": 2.0
              }},
              "rows": {rows}
            }}"#
        )
    }

    fn load_doc(text: &str) -> Result<PricingCard, PricingCardError> {
        PricingCard::from_str_at(text, Path::new("/test/pricing.json"))
    }

    #[test]
    fn the_shipped_asset_parses_and_validates() {
        let card = shipped();
        assert!(card.row_count() >= 16, "got {} rows", card.row_count());
        assert!(card.source().contains("platform.claude.com"), "{}", card.source());
    }

    #[test]
    fn aliases_resolve_to_the_newest_generation() {
        let card = shipped();
        assert_eq!(card.matching_row_id("opus"), Some("opus-current"));
        assert_eq!(card.matching_row_id("sonnet"), Some("sonnet-5"));
        assert_eq!(card.matching_row_id("haiku"), Some("haiku-4-5"));
        assert_eq!(card.matching_row_id("fable"), Some("fable-mythos-5-1"));
        assert_eq!(card.matching_row_id("mythos"), Some("fable-mythos-5-1"));
    }

    #[test]
    fn the_cascade_is_ordered_not_first_wins_by_specificity() {
        // `claude-opus-4-1` contains `claude-opus-4`, so only row ORDER keeps
        // the retired row from swallowing the current one (and vice versa).
        let card = shipped();
        assert_eq!(card.matching_row_id("claude-opus-4-5"), Some("opus-current"));
        assert_eq!(card.matching_row_id("claude-opus-4-1"), Some("opus-retired"));
        assert_eq!(card.matching_row_id("claude-opus-9"), Some("opus-newest"));
    }

    #[test]
    fn an_unmatched_id_is_none_not_a_guess() {
        assert!(shipped().lookup("totally-not-a-model").is_none());
    }

    #[test]
    fn the_reduced_cache_read_multiplier_applies_only_to_the_5_1_rows() {
        let card = shipped();
        let f51 = card.lookup("claude-fable-5-1").unwrap();
        assert!((f51.cache_read_cost_per_1k - 0.000_25).abs() < f64::EPSILON);
        let m51 = card.lookup("claude-mythos-5-1").unwrap();
        assert!((m51.cache_read_cost_per_1k - 0.000_25).abs() < f64::EPSILON);
        let f5 = card.lookup("claude-fable-5").unwrap();
        assert!((f5.cache_read_cost_per_1k - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn a_missing_file_is_absent_not_malformed() {
        let err = PricingCard::load(Path::new("/nonexistent/loom/pricing.json")).unwrap_err();
        assert!(err.is_absent(), "{err}");
        assert!(err.to_string().contains("could not be read"), "{err}");
    }

    #[test]
    fn invalid_json_is_rejected_as_malformed() {
        let err = load_doc("{ not json").unwrap_err();
        assert!(matches!(err, PricingCardError::Malformed { .. }), "{err}");
        assert!(!err.is_absent());
    }

    /// AC4: a *partial* document — schema-shaped but missing a required field —
    /// must be rejected wholesale, never applied for the rows it does carry.
    #[test]
    fn a_partial_document_is_rejected_wholesale() {
        let text = r#"{
          "schema_version": 1,
          "verified_on": "2026-09-18",
          "rows": [
            {"id": "sonnet", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
             "output_per_1k": 0.01, "cache_read": "standard"}
          ]
        }"#;
        let err = load_doc(text).unwrap_err();
        assert!(matches!(err, PricingCardError::Malformed { .. }), "{err}");
        // The message names the concrete gap (serde reports the first one it
        // hits: `source`, then `cache_multipliers`), not a generic failure.
        assert!(err.to_string().contains("missing field"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard", "discount": 0.5}]"#,
        );
        let err = load_doc(&text).unwrap_err();
        assert!(matches!(err, PricingCardError::Malformed { .. }), "{err}");
        assert!(err.to_string().contains("discount"), "{err}");
    }

    #[test]
    fn a_future_schema_version_is_rejected_by_name() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard"}]"#,
        )
        .replace("\"schema_version\": 1", "\"schema_version\": 2");
        let err = load_doc(&text).unwrap_err();
        assert!(
            matches!(err, PricingCardError::UnsupportedSchemaVersion { found: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_row_with_neither_cache_form_names_the_field() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01}]"#,
        );
        let err = load_doc(&text).unwrap_err();
        match &err {
            PricingCardError::Invalid { field, reason, .. } => {
                assert_eq!(field, "rows[0].cache_read");
                assert!(reason.contains("exactly one"), "{reason}");
            }
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn a_row_with_both_cache_forms_is_rejected() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard",
                 "cache_rates": {"read_per_1k": 0.0, "write_5m_per_1k": 0.0,
                                 "write_1h_per_1k": 0.0}}]"#,
        );
        assert!(matches!(load_doc(&text), Err(PricingCardError::Invalid { .. })));
    }

    #[test]
    fn a_negative_rate_is_rejected() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": -0.002,
                 "output_per_1k": 0.01, "cache_read": "standard"}]"#,
        );
        match load_doc(&text).unwrap_err() {
            PricingCardError::Invalid { field, .. } => assert_eq!(field, "rows[0].input_per_1k"),
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn an_empty_row_set_is_rejected() {
        let err = load_doc(&doc_with("[]")).unwrap_err();
        match err {
            PricingCardError::Invalid { field, .. } => assert_eq!(field, "rows"),
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn a_duplicate_row_id_is_rejected() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-5"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard"},
                {"id": "s", "match": ["claude-sonnet-4"], "input_per_1k": 0.003,
                 "output_per_1k": 0.015, "cache_read": "standard"}]"#,
        );
        match load_doc(&text).unwrap_err() {
            PricingCardError::Invalid { field, reason, .. } => {
                assert_eq!(field, "rows[1].id");
                assert!(reason.contains("duplicate"), "{reason}");
            }
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn an_alias_pointing_at_no_row_is_rejected() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard"}]"#,
        )
        .replace("\"rows\":", "\"aliases\": {\"opus\": \"claude-opus-5\"}, \"rows\":");
        match load_doc(&text).unwrap_err() {
            PricingCardError::Invalid { field, .. } => assert_eq!(field, "aliases.opus"),
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn a_bad_verified_on_is_rejected_by_name() {
        let text = doc_with(
            r#"[{"id": "s", "match": ["claude-sonnet-"], "input_per_1k": 0.002,
                 "output_per_1k": 0.01, "cache_read": "standard"}]"#,
        )
        .replace("2026-09-18", "last Tuesday");
        match load_doc(&text).unwrap_err() {
            PricingCardError::Invalid { field, .. } => assert_eq!(field, "verified_on"),
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn staleness_is_a_warning_not_a_rejection() {
        let card = shipped();
        let verified = card.verified_on();
        assert!(card
            .staleness_warning(verified + chrono::Duration::days(STALENESS_THRESHOLD_DAYS))
            .is_none());
        let warning = card
            .staleness_warning(verified + chrono::Duration::days(STALENESS_THRESHOLD_DAYS + 1))
            .expect("past the threshold the age must be reported");
        assert!(warning.contains("last verified"), "{warning}");
        // ...and the card still prices normally while stale.
        assert!(card.lookup("claude-opus-5").is_some());
    }

    #[test]
    #[serial_test::serial(loom_pricing_card_env)]
    fn the_env_override_selects_or_disables_the_asset_tier() {
        temp_env_var(ASSET_ENV, Some(""), || assert!(asset_path().is_none()));
        temp_env_var(ASSET_ENV, Some("/tmp/x.json"), || {
            assert_eq!(asset_path(), Some(PathBuf::from("/tmp/x.json")));
        });
    }

    /// The whole resolution path end to end, as a daemon process runs it:
    /// a good asset is adopted; a corrupt one, a truncated one, and an absent
    /// one each yield `None` so the caller keeps the compiled card WHOLE.
    ///
    /// This is the automated form of "delete/corrupt the installed
    /// pricing.json and confirm the daemon falls back rather than misreporting
    /// cost" — the manual check #8177's test plan asks for.
    #[test]
    #[serial_test::serial(loom_pricing_card_env)]
    fn resolution_adopts_a_good_asset_and_rejects_a_broken_one() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("pricing.json");
        let good = std::fs::read_to_string(shipped_asset_path()).unwrap();

        std::fs::write(&installed, &good).unwrap();
        temp_env_var(ASSET_ENV, installed.to_str(), || {
            let card = resolve_active().expect("a valid asset must be adopted");
            assert_eq!(card.path(), installed.as_path());
            let opus = card
                .lookup("claude-opus-5")
                .expect("opus must price off the asset");
            assert!((opus.input_cost_per_1k - 0.005).abs() < f64::EPSILON);
        });

        // Corrupt (not JSON at all) -> compiled card, nothing partially applied.
        std::fs::write(&installed, "{ this is not json").unwrap();
        temp_env_var(ASSET_ENV, installed.to_str(), || {
            assert!(resolve_active().is_none(), "a corrupt asset must not be adopted");
        });

        // Truncated mid-document (the torn-write shape) -> same.
        std::fs::write(&installed, &good[..good.len() / 2]).unwrap();
        temp_env_var(ASSET_ENV, installed.to_str(), || {
            assert!(resolve_active().is_none(), "a truncated asset must not be adopted");
        });

        // Valid JSON, wrong shape (an array, not the document) -> same.
        std::fs::write(&installed, "[]").unwrap();
        temp_env_var(ASSET_ENV, installed.to_str(), || {
            assert!(resolve_active().is_none(), "a wrong-shaped asset must not be adopted");
        });

        // Deleted -> same.
        std::fs::remove_file(&installed).unwrap();
        temp_env_var(ASSET_ENV, installed.to_str(), || {
            assert!(resolve_active().is_none(), "an absent asset must not be adopted");
        });

        // ...and through the public entry point, prices are still correct.
        assert!(
            (ModelPricing::for_model("claude-opus-5").input_cost_per_1k - 0.005).abs()
                < f64::EPSILON,
            "the compiled fallback must still price Opus 5 at the published rate"
        );
    }

    /// Set `key` for the duration of `f`, then restore. Tests touching this
    /// variable are `#[serial]`-grouped so they cannot race each other.
    fn temp_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let previous = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match previous {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
