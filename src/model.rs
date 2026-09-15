//! Wire types for the subset of the T3 Code contracts a chat client needs.
//! Decoding is lenient on purpose: unknown fields are ignored, optional fields
//! default, and open string unions stay `String`.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Id = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub instance_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSelection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: Id,
    pub title: String,
    pub workspace_root: String,
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestTurn {
    pub turn_id: Id,
    pub state: String,
    #[serde(default)]
    pub requested_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub status: String,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub active_turn_id: Option<Id>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProgress {
    pub step: String,
    pub completed_steps: u32,
    pub total_steps: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadShell {
    pub id: Id,
    pub project_id: Id,
    pub title: String,
    pub model_selection: ModelSelection,
    #[serde(default = "default_runtime_mode")]
    pub runtime_mode: String,
    #[serde(default = "default_interaction_mode")]
    pub interaction_mode: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub latest_turn: Option<LatestTurn>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(default)]
    pub pin_order_key: Option<String>,
    #[serde(default)]
    pub settled_at: Option<String>,
    #[serde(default)]
    pub settled_override: Option<String>,
    #[serde(default)]
    pub unsettled_at: Option<String>,
    #[serde(default)]
    pub snoozed_until: Option<String>,
    #[serde(default)]
    pub has_actionable_proposed_plan: bool,
    /// Native background work alive after the turn settled: "working" or "monitoring".
    #[serde(default)]
    pub background_liveness: Option<String>,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub latest_user_message_at: Option<String>,
    #[serde(default)]
    pub has_pending_approvals: bool,
    #[serde(default)]
    pub has_pending_user_input: bool,
    #[serde(default)]
    pub plan_progress: Option<PlanProgress>,
}

fn default_runtime_mode() -> String {
    "full-access".to_string()
}

fn default_interaction_mode() -> String {
    "default".to_string()
}

/// Sidebar status for a thread, ordered by priority (first match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    Approval,
    Question,
    Working,
    Failed,
    Monitoring,
    PlanReady,
    Done,
    Idle,
}

impl ThreadStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
            Self::Working => "working",
            Self::Failed => "failed",
            Self::Monitoring => "monitoring",
            Self::PlanReady => "plan ready",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }

    /// Whether the status deserves attention in the list (everything but the resting states).
    pub fn is_notable(self) -> bool {
        !matches!(self, Self::Done | Self::Idle)
    }
}

impl ThreadShell {
    /// A turn is in flight, or the provider session is starting up for one.
    pub fn is_running(&self) -> bool {
        if matches!(
            self.latest_turn.as_ref().map(|t| t.state.as_str()),
            Some("running")
        ) {
            return true;
        }
        matches!(
            self.session.as_ref().map(|s| s.status.as_str()),
            Some("running") | Some("starting")
        )
    }

    pub fn is_settled(&self) -> bool {
        self.settled_at.is_some()
    }

    /// `now` is an RFC 3339 timestamp; ISO-8601 strings in UTC compare lexicographically.
    pub fn is_snoozed(&self, now: &str) -> bool {
        self.snoozed_until
            .as_deref()
            .is_some_and(|until| until > now)
    }

    /// Mirrors the desktop sidebar's status resolution order.
    pub fn status(&self) -> ThreadStatus {
        if self.has_pending_approvals {
            return ThreadStatus::Approval;
        }
        if self.has_pending_user_input {
            return ThreadStatus::Question;
        }
        if self.is_running() {
            return ThreadStatus::Working;
        }
        let session_status = self.session.as_ref().map(|s| s.status.as_str());
        if session_status == Some("error") {
            return ThreadStatus::Failed;
        }
        match self.background_liveness.as_deref() {
            Some("working") => return ThreadStatus::Working,
            Some("monitoring") => return ThreadStatus::Monitoring,
            _ => {}
        }
        if self.interaction_mode == "plan" && self.has_actionable_proposed_plan {
            return ThreadStatus::PlanReady;
        }
        match self.latest_turn.as_ref().map(|t| t.state.as_str()) {
            Some("error") => ThreadStatus::Failed,
            Some("completed") => ThreadStatus::Done,
            _ => ThreadStatus::Idle,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Id,
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub id: Id,
    #[serde(default)]
    pub tone: String,
    pub kind: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub created_at: String,
}

impl Activity {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedPlan {
    pub id: Id,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub plan_markdown: String,
    #[serde(default)]
    pub implemented_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub shell: ThreadShell,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub activities: Vec<Activity>,
    #[serde(default)]
    pub proposed_plans: Vec<ProposedPlan>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailPage {
    #[serde(default)]
    pub before_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailSnapshot {
    pub snapshot_sequence: u64,
    pub thread: ThreadDetail,
    #[serde(default)]
    pub page: Option<ThreadDetailPage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    pub snapshot_sequence: u64,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub threads: Vec<ThreadShell>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum ShellItem {
    Synchronized,
    Snapshot {
        snapshot: ShellSnapshot,
    },
    ProjectUpserted {
        sequence: u64,
        project: Project,
    },
    ProjectRemoved {
        sequence: u64,
        #[serde(rename = "projectId")]
        project_id: Id,
    },
    ThreadUpserted {
        sequence: u64,
        thread: ThreadShell,
    },
    ThreadRemoved {
        sequence: u64,
        #[serde(rename = "threadId")]
        thread_id: Id,
    },
    #[serde(other)]
    Unknown,
}

/// A persisted orchestration event. The payload is interpreted by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub sequence: u64,
    #[serde(default)]
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub occurred_at: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum ThreadItem {
    Synchronized,
    Snapshot {
        snapshot: ThreadDetailSnapshot,
    },
    Event {
        event: Event,
    },
    #[serde(other)]
    Unknown,
}

// ── Server config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub thread_snapshot_pagination: bool,
    #[serde(default)]
    pub thread_resume_completion_marker: bool,
    #[serde(default)]
    pub shell_resume_completion_marker: bool,
    #[serde(default)]
    pub settings: ServerSettings,
    #[serde(default)]
    pub environment: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub default_runtime_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub instance_id: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub auth: ProviderAuth,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub slash_commands: Vec<SlashCommand>,
}

impl Provider {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.instance_id)
    }

    pub fn is_usable(&self) -> bool {
        self.enabled
            && self.installed
            && self.availability.as_deref() != Some("unavailable")
            && self.status != "disabled"
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderAuth {
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub short_name: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_legacy: bool,
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
}

impl Model {
    pub fn option_descriptors(&self) -> &[OptionDescriptor] {
        self.capabilities
            .as_ref()
            .map(|c| c.option_descriptors.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    #[serde(default)]
    pub option_descriptors: Vec<OptionDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionDescriptor {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<OptionChoice>,
    #[serde(default)]
    pub current_value: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptionChoice {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_item_unknown_kind_is_tolerated() {
        let item: ShellItem = serde_json::from_str(r#"{"kind":"something-new","x":1}"#).unwrap();
        assert!(matches!(item, ShellItem::Unknown));
    }

    #[test]
    fn thread_detail_flattens_shell_fields() {
        let json = r#"{"id":"t","projectId":"p","title":"T","modelSelection":{"instanceId":"claudeAgent","model":"m"},
            "runtimeMode":"full-access","latestTurn":null,"session":null,"messages":[],"activities":[]}"#;
        let detail: ThreadDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.shell.title, "T");
        assert!(!detail.shell.has_pending_approvals);
    }
}
