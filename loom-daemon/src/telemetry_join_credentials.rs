//! Credential shapes for telemetry-join. Values stay in memory or owner-only
//! files and never implement revealing Debug/Display.
use super::{JoinError, DEFAULT_SSM_PARAM};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;
const CREDENTIAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Credential {
    Bearer(String),
    Headers(String),
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential([redacted])")
    }
}
impl Credential {
    pub(super) fn headers(&self) -> Result<String, JoinError> {
        match self {
            Self::Bearer(token) => {
                if token.is_empty() || token.bytes().any(|b| !b.is_ascii_graphic()) {
                    return Err(JoinError::Credential(
                        "bearer credential has an invalid value shape".into(),
                    ));
                }
                Ok(format!("Authorization: Bearer {token}\n"))
            }
            Self::Headers(text) => Ok(text.clone()),
        }
    }
}

#[derive(Debug)]
struct AccessPairSource {
    prefix: String,
}
impl CredentialSource for AccessPairSource {
    fn credential(&self) -> Result<Credential, JoinError> {
        let fetch = |suffix: &str| -> Result<String, JoinError> {
            match (SsmSource {
                parameter: format!("{}/{suffix}", self.prefix),
            })
            .credential()?
            {
                Credential::Bearer(value) => Ok(value),
                Credential::Headers(_) => unreachable!(),
            }
        };
        let id = fetch("client_id")?;
        let secret = fetch("client_secret")?;
        // A pair is two single-line values, never an arbitrary header fragment.
        if [&id, &secret]
            .iter()
            .any(|v| v.bytes().any(|b| !b.is_ascii_graphic()))
        {
            return Err(JoinError::Credential("Access pair has an invalid value shape".into()));
        }
        Ok(Credential::Headers(format!(
            "CF-Access-Client-Id: {id}\nCF-Access-Client-Secret: {secret}\n"
        )))
    }
}

#[derive(Debug)]
struct HeaderFileSource {
    path: PathBuf,
}
impl CredentialSource for HeaderFileSource {
    fn credential(&self) -> Result<Credential, JoinError> {
        super::ensure_external_path(&self.path)?;
        use std::io::Read;
        let mut file = std::fs::File::open(&self.path)
            .map_err(|_| JoinError::Credential("cannot open declared headers file".into()))?;
        let meta = file
            .metadata()
            .map_err(|_| JoinError::Credential("cannot stat headers file".into()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // SAFETY: geteuid only reads the caller's effective user id.
            if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
                return Err(JoinError::Credential("headers source must be owner-only".into()));
            }
        }
        if !meta.is_file() {
            return Err(JoinError::Credential("headers source must be a regular file".into()));
        }
        let max = crate::observability::request_headers::MAX_HEADERS_FILE_BYTES;
        let mut text = String::new();
        file.by_ref()
            .take(max + 1)
            .read_to_string(&mut text)
            .map_err(|_| JoinError::Credential("cannot read declared headers file".into()))?;
        if text.len() as u64 > max {
            return Err(JoinError::Credential("headers source is too large".into()));
        }
        Ok(Credential::Headers(text))
    }
}

/// A credential is fetchable from its declared store.
pub trait CredentialSource: fmt::Debug {
    /// The declared credential shape for this host. Never logged.
    ///
    /// # Errors
    ///
    /// [`JoinError::Credential`] when the store cannot be reached or the
    /// value is empty.
    fn credential(&self) -> Result<Credential, JoinError>;
}

/// `ssm://<parameter>`: shell out to the AWS CLI (`aws ssm get-parameter
/// --with-decryption`). No SDK dependency exists in this workspace, and the
/// token never appears in a command line — only the parameter *name* does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsmSource {
    /// The SSM parameter's full name.
    pub parameter: String,
}

impl CredentialSource for SsmSource {
    fn credential(&self) -> Result<Credential, JoinError> {
        let mut cmd = std::process::Command::new("aws");
        cmd.args([
            "ssm",
            "get-parameter",
            "--name",
            &self.parameter,
            "--with-decryption",
            "--query",
            "Parameter.Value",
            "--output",
            "text",
        ]);
        let output =
            run_credential_command(cmd, &format!("aws ssm get-parameter {}", self.parameter))?;
        finish_credential(output, "aws ssm get-parameter").map(Credential::Bearer)
    }
}

/// One completed credential subprocess: stdout / first stderr line, and
/// whether it exited zero. A deadline hit is a failure naming the tool.
pub(super) struct CommandOutput {
    pub(super) ok: bool,
    pub(super) stdout: String,
}

/// Run a credential fetch under [`CREDENTIAL_COMMAND_TIMEOUT`], mapping every
/// failure (spawn, collect, deadline) to [`JoinError::Credential`].
fn run_credential_command(
    cmd: std::process::Command,
    tool: &str,
) -> Result<CommandOutput, JoinError> {
    match crate::proc_exec::run_bounded(cmd, CREDENTIAL_COMMAND_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(output)) => Ok(CommandOutput {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        }),
        Ok(crate::proc_exec::Completion::TimedOut { .. }) => {
            Err(JoinError::Credential(format!("{tool} timed out")))
        }
        Err(e) => Err(JoinError::Credential(format!("{tool}: {e}"))),
    }
}

/// Shared tail of a credential fetch: non-empty stdout on exit zero, a
/// reason that names the tool and its first stderr line — never the value.
pub(super) fn finish_credential(output: CommandOutput, tool: &str) -> Result<String, JoinError> {
    if !output.ok {
        return Err(JoinError::Credential(format!(
            "{tool} failed (credential subprocess exited non-zero)"
        )));
    }
    let token = output.stdout;
    if token.is_empty() {
        return Err(JoinError::Credential(format!("{tool} returned an empty value")));
    }
    Ok(token)
}

/// `age-file:<path>`: decrypt with `age -d` (optionally `-i <identity>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeFileSource {
    /// The encrypted file's path.
    pub path: PathBuf,
    /// Identity file passed as `-i`, when the passphrase is held in one.
    pub identity: Option<PathBuf>,
}

impl CredentialSource for AgeFileSource {
    fn credential(&self) -> Result<Credential, JoinError> {
        super::ensure_external_path(&self.path)?;
        let mut cmd = std::process::Command::new("age");
        cmd.arg("--decrypt");
        if let Some(identity) = &self.identity {
            super::ensure_external_path(identity)?;
            cmd.args(["-i", &identity.display().to_string()]);
        }
        cmd.arg(&self.path);
        let output = run_credential_command(cmd, &format!("age -d {}", self.path.display()))?;
        finish_credential(output, "age -d").map(Credential::Bearer)
    }
}

/// `file:<path>`: an already-decrypted plaintext file (e.g. a
/// chezmoi-rendered secret), read directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    /// The file's path.
    pub path: PathBuf,
}

impl CredentialSource for FileSource {
    fn credential(&self) -> Result<Credential, JoinError> {
        let Credential::Headers(text) = (HeaderFileSource {
            path: self.path.clone(),
        })
        .credential()?
        else {
            unreachable!()
        };
        let token = text.trim().to_owned();
        if token.is_empty() {
            return Err(JoinError::Credential("scalar credential file is empty".into()));
        }
        Ok(Credential::Bearer(token))
    }
}

/// A parsed `--credential-source` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSpec {
    /// An existing Access service-token pair below this SSM prefix.
    SsmAccess { prefix: String },
    /// An already provisioned owner-only header file.
    HeadersFile { path: PathBuf },
    /// `ssm://<parameter>`.
    Ssm { parameter: String },
    /// `age-file:<path>`.
    AgeFile { path: PathBuf },
    /// `file:<path>`.
    File { path: PathBuf },
}

/// The default Access-pair spec for `host`: `ssm-access://{DEFAULT_SSM_PARAM}/{host}`.
#[must_use]
pub fn default_credential_spec(host: &str) -> String {
    format!("ssm-access://{DEFAULT_SSM_PARAM}/{host}")
}

/// Parse a `--credential-source` value.
///
/// # Errors
///
/// [`JoinError::Credential`] naming the accepted forms when the spec does not
/// match one.
pub fn parse_credential_spec(spec: &str) -> Result<CredentialSpec, JoinError> {
    for (scheme, access) in [("ssm-access://", true), ("headers-file:", false)] {
        if let Some(value) = spec.strip_prefix(scheme) {
            let value = value.trim();
            if value.is_empty() {
                return Err(JoinError::Credential("credential source needs a path".into()));
            }
            return Ok(if access {
                CredentialSpec::SsmAccess {
                    prefix: value.trim_end_matches('/').into(),
                }
            } else {
                CredentialSpec::HeadersFile { path: value.into() }
            });
        }
    }
    if let Some(parameter) = spec.strip_prefix("ssm://") {
        let parameter = parameter.trim();
        if parameter.is_empty() {
            return Err(JoinError::Credential(
                "ssm:// needs a parameter name (ssm:///path/to/parameter)".to_string(),
            ));
        }
        return Ok(CredentialSpec::Ssm {
            parameter: parameter.to_string(),
        });
    }
    if let Some(path) = spec.strip_prefix("age-file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(JoinError::Credential(
                "age-file: needs a path (age-file:/path/to/encrypted)".to_string(),
            ));
        }
        return Ok(CredentialSpec::AgeFile {
            path: PathBuf::from(path),
        });
    }
    if let Some(path) = spec.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(JoinError::Credential(
                "file: needs a path (file:/path/to/plaintext)".to_string(),
            ));
        }
        return Ok(CredentialSpec::File {
            path: PathBuf::from(path),
        });
    }
    Err(JoinError::Credential(
        "unknown credential source; use ssm-access://<prefix>, headers-file:<path>, ssm://<parameter>, age-file:<path> or \
         file:<path>".into()
    ))
}

/// Build the live source for a parsed spec.
///
/// # Errors
///
/// A bad spec is [`JoinError::Credential`].
pub fn credential_source<'a>(
    spec: &'a CredentialSpec,
    age_identity: Option<&'a Path>,
) -> Box<dyn CredentialSource + 'a> {
    match spec {
        CredentialSpec::SsmAccess { prefix } => Box::new(AccessPairSource {
            prefix: prefix.clone(),
        }),
        CredentialSpec::HeadersFile { path } => Box::new(HeaderFileSource { path: path.clone() }),
        CredentialSpec::Ssm { parameter } => Box::new(SsmSource {
            parameter: parameter.clone(),
        }),
        CredentialSpec::AgeFile { path } => Box::new(AgeFileSource {
            path: path.clone(),
            identity: age_identity.map(Path::to_path_buf),
        }),
        CredentialSpec::File { path } => Box::new(FileSource { path: path.clone() }),
    }
}
