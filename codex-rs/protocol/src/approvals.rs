use std::collections::HashMap;
use std::path::PathBuf;

use crate::parse_command::ParsedCommand;
use crate::protocol::FileChange;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRiskCategory {
    DataDeletion,
    DataExfiltration,
    PrivilegeEscalation,
    SystemModification,
    NetworkAccess,
    ResourceExhaustion,
    Compliance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct SandboxCommandAssessment {
    pub description: String,
    pub risk_level: SandboxRiskLevel,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_categories: Vec<SandboxRiskCategory>,
}

impl SandboxRiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl SandboxRiskCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DataDeletion => "data_deletion",
            Self::DataExfiltration => "data_exfiltration",
            Self::PrivilegeEscalation => "privilege_escalation",
            Self::SystemModification => "system_modification",
            Self::NetworkAccess => "network_access",
            Self::ResourceExhaustion => "resource_exhaustion",
            Self::Compliance => "compliance",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ExecApprovalRequestEvent {
    /// Identifier for the associated exec call, if available.
    pub call_id: String,
    /// The command to be executed.
    pub command: Vec<String>,
    /// The command's working directory.
    pub cwd: PathBuf,
    /// Optional human-readable reason for the approval (e.g. retry without sandbox).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional model-provided risk assessment describing the blocked command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<SandboxCommandAssessment>,
    pub parsed_cmd: Vec<ParsedCommand>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ApplyPatchApprovalRequestEvent {
    /// Responses API call id for the associated patch apply call, if available.
    pub call_id: String,
    pub changes: HashMap<PathBuf, FileChange>,
    /// Optional explanatory reason (e.g. request for extra write access).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When set, the agent is asking the user to allow writes under this root for the remainder of the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_root: Option<PathBuf>,
}
