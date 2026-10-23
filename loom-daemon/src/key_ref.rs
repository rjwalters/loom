//! Where a service key is read from: `file:<path>` or `ssm://<parameter>`
//! (2AMLogic/2am#4367). A [`KeyRef`] holds a location, never a key; a key is
//! read into memory and its errors name the source, never the value. Shared
//! by `forge.gateway` and `forge.mirror`.

use std::path::PathBuf;

use crate::aws_ssm::AwsAccess;

/// Where a key is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRef {
    /// `ssm://<parameter>`.
    Ssm(String),
    /// `file:<path>`.
    File(PathBuf),
}

impl KeyRef {
    /// Parse `ssm://<parameter>` or `file:<path>`.
    ///
    /// # Errors
    ///
    /// Any other shape, or an empty location.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if let Some(p) = spec.strip_prefix("ssm://") {
            let p = p.trim();
            return if p.is_empty() {
                Err("ssm:// needs a parameter name".into())
            } else {
                Ok(Self::Ssm(p.to_string()))
            };
        }
        if let Some(p) = spec.strip_prefix("file:") {
            let p = p.trim();
            return if p.is_empty() {
                Err("file: needs a path".into())
            } else {
                Ok(Self::File(PathBuf::from(p)))
            };
        }
        Err("use file:<path> or ssm://<parameter>".into())
    }

    /// Read the key; `config_key` names the block in the errors.
    ///
    /// # Errors
    ///
    /// When the source cannot be read, or holds no plausible key.
    pub fn read(&self, aws: &AwsAccess<'_>, config_key: &str) -> Result<String, String> {
        use crate::telemetry_join::{credential_source, Credential, CredentialSpec};
        let fetched = match self {
            Self::Ssm(p) => crate::aws_ssm::ssm_get(p, aws, config_key)?,
            Self::File(p) => {
                match credential_source(&CredentialSpec::File { path: p.clone() }, None)
                    .credential()
                {
                    Ok(Credential::Bearer(key)) => key,
                    Ok(_) => return Err("the source does not hold a single-line key".into()),
                    Err(e) => return Err(e.to_string()),
                }
            }
        };
        if valid_key(&fetched) {
            Ok(fetched)
        } else {
            Err("the source does not hold a single-line key".into())
        }
    }
}

/// A key is one token of URL-safe characters: anything else could not be
/// written into a header or a `hosts.yml` as-is.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (16..=512).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=".contains(&b))
}

/// The host of a service `origin`: `https://host` or a bare `host`, no path,
/// port or credentials, and never GitHub itself (that would send the
/// service's key to GitHub). `what` names the service in the refusal.
///
/// # Errors
///
/// A reason the origin is refused.
pub fn https_host(origin: &str, what: &str) -> Result<String, String> {
    let o = origin.trim();
    let host = match o.split_once("://") {
        Some(("https", rest)) => rest,
        Some((scheme, _)) => return Err(format!("origin must be https, not {scheme}")),
        None => o,
    };
    let host = host.strip_suffix('/').unwrap_or(host).to_ascii_lowercase();
    // `github.com.` is GitHub too: compare and store without the root dot.
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    let ok = host.contains('.')
        && !host.starts_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !ok {
        return Err("origin must be a bare host name or https://<host>".into());
    }
    if host == "github.com" || host.ends_with(".github.com") {
        return Err(format!("origin must be {what}, not GitHub"));
    }
    Ok(host)
}
