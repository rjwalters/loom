//! Validated W3C version-00 context. IDs survive serialization and delivery retries.

use serde::{Deserialize, Serialize};

macro_rules! identifier {
    ($name:ident, $bytes:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.len() != $bytes * 2
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || value.bytes().all(|b| b == b'0')
                {
                    return Err(concat!("invalid ", stringify!($name)).to_string());
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
            #[must_use]
            pub fn bytes(&self) -> Vec<u8> {
                // Construction and deserialization validate every byte.
                hex::decode(&self.0).unwrap_or_default()
            }
        }
    };
}

identifier!(TraceId, 16);
identifier!(SpanId, 8);

/// A span's own context, rather than its parent's context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub flags: u8,
}

impl TraceContext {
    #[must_use]
    pub fn root(sampled: bool) -> Self {
        let trace_id = TraceId(uuid::Uuid::new_v4().simple().to_string());
        Self {
            trace_id,
            span_id: new_span_id(),
            flags: u8::from(sampled),
        }
    }

    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: new_span_id(),
            flags: self.flags,
        }
    }

    #[must_use]
    pub fn sampled(&self) -> bool {
        self.flags & 1 == 1
    }

    #[must_use]
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-{:02x}", self.trace_id.as_str(), self.span_id.as_str(), self.flags)
    }

    /// Strict version-00 parser; errors contain no supplied context or baggage.
    pub fn parse(value: &str) -> Result<Self, String> {
        let parts: Vec<_> = value.split('-').collect();
        if parts.len() != 4
            || parts[0] != "00"
            || parts[3].len() != 2
            || !parts[3]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid version-00 traceparent".to_string());
        }
        Ok(Self {
            trace_id: TraceId::try_from(parts[1].to_string())?,
            span_id: SpanId::try_from(parts[2].to_string())?,
            flags: u8::from_str_radix(parts[3], 16)
                .map_err(|_| "invalid trace flags".to_string())?,
        })
    }
}

fn new_span_id() -> SpanId {
    // UUID v4's version bits ensure these first eight bytes cannot all be zero.
    SpanId(uuid::Uuid::new_v4().simple().to_string()[..16].to_string())
}
