use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubagentState {
    Working,
    Ended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSnapshot {
    pub agent_id: String,
    pub agent_type: String,
    pub state: SubagentState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestInfo {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    /// Story 6.30 (Grok) — `true` = affichage lecture-seule (permission Grok en mode ask : pas de
    /// boutons, invite « réponds dans le terminal »). `false` (défaut) = overlay actionnable.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentState {
    Idle,
    Working,
    WaitingApprove,
    WaitingSpecific,
    Ended,
    Error,
    Disconnected,
    Unknown,
}


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceTool {
    ClaudeCode,
    Codex,
    Grok,
    Antigravity,
    /// Story 9.1 — Kimi CLI (Moonshot). ⚠️ Sans ce variant, `source_tool:"kimi"` retomberait EN
    /// SILENCE sur `Unknown` via `#[serde(other)]` : aucun test rouge, aucun badge, `tool_label`
    /// « Agent ». Le filet réel est ailleurs — les deux `match` EXHAUSTIFS
    /// (`flags::interrupt_supported`, `notify::tool_label`) cassent le build tant qu'ils ne sont pas
    /// complétés.
    Kimi,
    #[serde(other)]
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub transcript_path: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub name: String,
    pub state: AgentState,
    #[serde(default)]
    pub waiting_since: Option<u64>,
    pub last_activity: u64,
    #[serde(default)]
    pub last_activity_label: Option<String>,
    #[serde(default)]
    pub terminal_pid: Option<u32>,
    /// Libellé convivial de l'hôte d'exécution (« VS Code », « Windows Terminal », « PowerShell »,
    /// « Cursor »…, ou nom nettoyé d'un hôte inconnu). Défaut « Terminal ». Rempli au `SessionStart`
    /// depuis le hook (`terminal_source`), qui l'a déduit de l'arbre de process.
    pub source: String,
    #[serde(default)]
    pub permission_request: Option<PermissionRequestInfo>,
    #[serde(default)]
    pub interrupt_pending: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub subagents: Vec<SubagentSnapshot>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub source_tool: SourceTool,
}

impl AgentSnapshot {
    pub fn upgrade_source_tool(&mut self, tool: SourceTool) {
        if self.source_tool == SourceTool::Unknown && tool != SourceTool::Unknown {
            self.source_tool = tool;
        }
    }
}
