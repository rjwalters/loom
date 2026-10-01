//! Content-derived cache keys (#9783 step 1): the SHA-256 over the canonical
//! input snapshot. `updatedAt` is never a key component — requirements and
//! repository code change independently, so only *content* selects a key.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One requirement-bearing comment, pinned by id and revision so comment
/// edits/deletes are observable (#9784's refresh contract consumes this).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequirementComment {
    pub id: i64,
    /// Forge-side revision marker (e.g. `last_edited_at` or update count).
    pub revision: String,
    pub body: String,
}

/// The full input snapshot a cache key is derived from. Serialized
/// canonically (serde field order is declaration order and stable), hashed
/// with SHA-256.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputSnapshot {
    /// Snapshot-schema version (bump on semantic field changes).
    pub schema_version: u32,
    /// `OWNER/REPO` — repository identity is part of the key.
    pub repo: String,
    /// Issue identity within the repo.
    pub issue: u32,
    /// Canonical (whitespace-normalized) title.
    pub title: String,
    /// Canonical (whitespace-normalized) body.
    pub body: String,
    /// Explicitly selected requirement-bearing comments (id + revision +
    /// canonical body). Empty = none supplied.
    #[serde(default)]
    pub requirement_comments: Vec<RequirementComment>,
    /// The pinned immutable source revision retrieval ran against. The
    /// checkout's current HEAD is NEVER substituted for this.
    pub source_revision: String,
    /// Index identity/content manifest digest — which index answered.
    pub index_identity: String,
    /// Frozen query-policy version.
    pub query_policy_version: String,
    /// Adapter + adapter-schema version (recorded verbatim by the provider).
    pub adapter_version: String,
}

/// Normalize text for hashing: collapse all whitespace runs to single
/// spaces and trim, preserving requirement semantics (#9783 step 1: "preserve
/// requirement semantics when normalizing text").
pub fn canonical_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl InputSnapshot {
    /// Canonicalize free-text fields in place (idempotent).
    pub fn canonicalize(&mut self) {
        self.title = canonical_text(&self.title);
        self.body = canonical_text(&self.body);
        for c in &mut self.requirement_comments {
            c.body = canonical_text(&c.body);
        }
    }

    /// The content key: hex SHA-256 over the canonical JSON of the
    /// canonicalized snapshot.
    pub fn content_key(&self) -> String {
        let mut s = self.clone();
        s.canonicalize();
        let canonical = serde_json::to_vec(&s).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(&canonical);
        hex::encode(h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> InputSnapshot {
        InputSnapshot {
            schema_version: 1,
            repo: "o/r".into(),
            issue: 7,
            title: "  do   the thing ".into(),
            body: "body\nwith\nnewlines".into(),
            requirement_comments: vec![RequirementComment {
                id: 42,
                revision: "2026-01-01T00:00:00Z".into(),
                body: "must  not\nbreak".into(),
            }],
            source_revision: "abc".into(),
            index_identity: "idx-1".into(),
            query_policy_version: "qp-v1".into(),
            adapter_version: "fake-1".into(),
        }
    }

    #[test]
    fn canonicalization_is_idempotent_and_content_preserving() {
        let s = snap();
        let k1 = s.content_key();
        let k2 = s.content_key(); // repeated calls must be stable
        assert_eq!(k1, k2);
        let mut equivalent = snap();
        equivalent.title = "do the thing".into(); // same canonical form
        equivalent.body = "body with newlines".into();
        equivalent.requirement_comments[0].body = "must not break".into();
        assert_eq!(k1, equivalent.content_key(), "whitespace-only edits reuse the key");
    }

    #[test]
    fn every_dimension_selects_a_key() {
        let base = snap();
        let key_of = |s: InputSnapshot| s.content_key();
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.repo = "o/other".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.issue = 8;
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.body = "edited".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.source_revision = "def".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.index_identity = "idx-2".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.query_policy_version = "qp-v2".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.adapter_version = "fake-2".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.requirement_comments[0].body = "edited".into();
            s.content_key()
        });
        assert_ne!(key_of(base.clone()), {
            let mut s = base.clone();
            s.requirement_comments.clear();
            s.content_key()
        });
    }
}
