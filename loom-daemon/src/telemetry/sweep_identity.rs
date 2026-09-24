use super::RepoVisibility;
use serde::{Deserialize, Serialize};

/// `sweep.identity`: the sweep launch's identity, not its child roles' models.
/// An update-only record: consumers must never create an active sweep from it.
/// No profile, credential, account, or local path crosses this boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepIdentityRecord {
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub issue: u32,
    pub sweep_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}
