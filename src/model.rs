//! Wire types for the subset of the T3 Code contracts a chat client needs.
//! Decoding is lenient on purpose: unknown fields are ignored, optional fields
//! default, and open string unions stay `String`.

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

impl ThreadShell {
    pub fn is_running(&self) -> bool {
        matches!(self.latest_turn.as_ref().map(|t| t.state.as_str()), Some("running"))
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
pub enum ThreadItem {
    Synchronized,
    Snapshot { snapshot: ThreadDetailSnapshot },
    Event { event: Event },
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
        self.capabilities.as_ref().map(|c| c.option_descriptors.as_slice()).unwrap_or(&[])
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
