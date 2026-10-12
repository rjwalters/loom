//! The versioned journal envelope (§9.1, §11.4).
//!
//! `v` is the envelope **major**. A reader accepts its own major, ignores
//! unknown fields and unknown `kind`s, and counts a record of any other major
//! as `unknown_major` (never as corruption, never fatal).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{JournalError, MAX_ENTRY_BYTES};

/// The envelope major this build reads and writes.
pub const ENVELOPE_MAJOR: u32 = 1;

/// Who wrote a record: the crate version and the **full** commit SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterIdentity {
    pub version: String,
    pub commit_full: String,
}

impl WriterIdentity {
    /// This build's identity.
    #[must_use]
    pub fn current() -> Self {
        WriterIdentity {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            commit_full: crate::self_update::BUILT_COMMIT_FULL.to_owned(),
        }
    }
}

/// One journal record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub stream: String,
    pub seq: u64,
    pub kind: String,
    pub ts: DateTime<Utc>,
    pub writer: WriterIdentity,
    /// Natural key for replay de-duplication, if the producer has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub data: Value,
}

/// Encode `envelope` as one newline-terminated line, refusing an entry over
/// [`MAX_ENTRY_BYTES`].
pub(crate) fn encode(envelope: &Envelope) -> Result<Vec<u8>, JournalError> {
    let mut bytes = serde_json::to_vec(envelope).map_err(JournalError::Encode)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(JournalError::EntryTooLarge { bytes: bytes.len() });
    }
    Ok(bytes)
}

/// The fields every major is expected to keep: enough to classify a line
/// and to keep `seq` monotonic across a mixed-version stream.
#[derive(Deserialize)]
struct Probe {
    v: u32,
    #[serde(default)]
    seq: Option<u64>,
}

/// One complete line, classified.
#[derive(Debug)]
pub(crate) enum Decoded {
    Record(Box<Envelope>),
    UnknownMajor { v: u32, seq: Option<u64> },
    Corrupt,
}

impl Decoded {
    /// The `seq` the line carries, whatever its major.
    pub(crate) fn seq(&self) -> Option<u64> {
        match self {
            Decoded::Record(envelope) => Some(envelope.seq),
            Decoded::UnknownMajor { seq, .. } => *seq,
            Decoded::Corrupt => None,
        }
    }
}

/// Classify one complete line (newline optional).
pub(crate) fn decode(line: &[u8]) -> Decoded {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let Ok(probe) = serde_json::from_slice::<Probe>(line) else {
        return Decoded::Corrupt;
    };
    if probe.v != ENVELOPE_MAJOR {
        return Decoded::UnknownMajor {
            v: probe.v,
            seq: probe.seq,
        };
    }
    match serde_json::from_slice::<Envelope>(line) {
        Ok(envelope) => Decoded::Record(Box::new(envelope)),
        Err(_) => Decoded::Corrupt,
    }
}
