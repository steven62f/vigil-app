use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use crate::agent::state::{AgentSnapshot, AgentState, PermissionRequestInfo, SourceTool, SubagentSnapshot, SubagentState};
use crate::pipe::protocol::HookEvent;

#[derive(Clone)]
pub struct HookSettings {
    pub show_subagents: bool,
    pub auto_cleanup_disconnected: bool,
    pub hide_notch_if_empty: bool,
}

impl Default for HookSettings {
    fn default() -> Self {
        Self { show_subagents: true, auto_cleanup_disconnected: false, hide_notch_if_empty: false }
    }
}

#[derive(Default)]
pub struct AgentStateEngine {
    pub agents: HashMap<String, AgentSnapshot>,
    pub(crate) settings: HookSettings,
    /// Story 6.31 — ensemble des `session_id` de SOUS-AGENTS Grok connus (alimenté par
    /// `SubagentStart.agent_id` = `subagentId` = session enfant). Un sous-agent Grok est une VRAIE
    /// session autonome émettant ses propres hooks sous ce `session_id` ; via l'`or_insert` des
    /// handlers, ils créeraient un agent Grok fantôme de 1er niveau. La garde en tête d'`apply_event`
    /// SUPPRIME tout event Grok dont le `session_id` est dans cet ensemble. Vide pour tout autre
    /// outil (mécanisme borné `source_tool == Grok`, orthogonal au filtre Codex par-event
    /// `is_subagent_scoped_phantom`).
    grok_subagent_sessions: HashSet<String>,
}

impl AgentStateEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_settings(&mut self, s: HookSettings) {
        self.settings = s;
    }

    pub fn set_auto_permission_label(
        &mut self,
        transcript_path: &str,
        session_id: &str,
        cwd: &str,
        parent_pid: Option<u32>,
        label: &str,
    ) {
        let now = now_ms();
        let agent = self
            .agents
            .entry(transcript_path.to_string())
            .or_insert_with(|| make_snapshot(transcript_path, session_id, cwd, parent_pid, SourceTool::Unknown, now));
        agent.last_activity = now;
        agent.last_activity_label = Some(label.to_string());
    }

    pub fn emit_snapshot(&self, app: &AppHandle) -> anyhow::Result<()> {
        let raw: Vec<AgentSnapshot> = self.agents.values().cloned().collect();
        let snapshots = crate::session_meta::enrich_snapshots(app, raw);
        app.emit("agents:snapshot", snapshots)?;

        if let Some(notch) = app.get_webview_window("notch") {
            if self.settings.hide_notch_if_empty && self.agents.is_empty() {
                let _ = notch.hide();
            } else {
                let _ = notch.show();
            }
        }
        Ok(())
    }

    pub fn apply_event(&mut self, event: &HookEvent) {
        let now = now_ms();

        // Story 6.31 — anti-fantôme Grok : SUPPRIMER les events émis par un SOUS-AGENT Grok (sa
        // propre session autonome) AVANT tout `or_insert` → zéro agent de 1er niveau créé. Bornée
        // `source_tool == Grok` (chemin Claude/Codex/Antigravity byte-identique, zéro I/O). Les
        // events `SubagentStart`/`SubagentStop` (dont le `session_id` = PARENT) sont exclus de cette
        // garde et routent le dot vers le parent.
        if self.is_grok_subagent_phantom(event) {
            return;
        }

        match event {
            HookEvent::SessionStart(p) => {
                // remove any stale interrupt flag from a previous session on the same transcript path
                let _ = std::fs::remove_file(crate::flags::interrupt_flag_path(&p.transcript_path));
                let mut snapshot = make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, p.source_tool.clone(), now);
                snapshot.model = p.model.clone();
                if let Some(src) = p
                    .terminal_source
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    snapshot.source = src.to_string();
                }
                self.agents.insert(p.transcript_path.clone(), snapshot);
            }

            HookEvent::SessionEnd(p) => {
                // Pas d'upsert : si l'agent n'existe pas, rien à faire
                self.set_disconnected(&p.transcript_path);
            }

            HookEvent::PreToolUse(p) => {
                let label = derive_activity_label(&p.tool_name, &p.tool_input);
                let source_tool = p.source_tool.clone();
                let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                    make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                });
                agent.upgrade_source_tool(source_tool);
                agent.state = AgentState::Working;
                agent.permission_request = None;
                agent.last_activity = now;
                agent.last_activity_label = Some(label);
            }

            HookEvent::PostToolUse(p) => {
                let label = derive_activity_label(&p.tool_name, &p.tool_input);
                let source_tool = p.source_tool.clone();
                let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                    make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                });
                agent.upgrade_source_tool(source_tool);
                agent.state = AgentState::Working;
                agent.permission_request = None;
                agent.last_activity = now;
                agent.last_activity_label = Some(label);
            }

            HookEvent::PostToolUseFailure(p) => {
                let source_tool = p.source_tool.clone();
                if p.is_interrupt {
                    self.clean_agent_flags(&p.transcript_path);
                    let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                        let mut s = make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now);
                        s.state = AgentState::Ended;
                        s
                    });
                    agent.upgrade_source_tool(source_tool);
                    agent.state = AgentState::Ended;
                    agent.waiting_since = None;
                    agent.permission_request = None;
                    agent.last_activity = now;
                } else {
                    let label = format!("Failed: {}", derive_activity_label(&p.tool_name, &p.tool_input));
                    let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                        make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                    });
                    agent.upgrade_source_tool(source_tool);
                    agent.last_activity = now;
                    agent.last_activity_label = Some(label);
                }
            }

            HookEvent::PermissionRequest(p) => {
                let new_state = if p.tool_name == "AskUserQuestion" {
                    AgentState::WaitingSpecific
                } else {
                    AgentState::WaitingApprove
                };
                let source_tool = p.source_tool.clone();
                self.clean_agent_flags(&p.transcript_path);
                let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                    make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                });
                agent.upgrade_source_tool(source_tool);
                agent.state = new_state;
                agent.waiting_since = Some(now);
                agent.last_activity = now;
                agent.permission_request = Some(PermissionRequestInfo {
                    tool_name: p.tool_name.clone(),
                    tool_input: p.tool_input.clone(),
                    read_only: p.read_only,
                });
            }

            HookEvent::Notification(p) => {
                let source_tool = p.source_tool.clone();
                let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                    make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                });
                agent.upgrade_source_tool(source_tool);
                agent.last_activity = now;
                if !p.message.is_empty() {
                    agent.last_activity_label = Some(p.message.clone());
                }
            }

            HookEvent::Stop(p) => {
                let source_tool = p.source_tool.clone();
                self.clean_agent_flags(&p.transcript_path);
                {
                    let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                        let mut s = make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now);
                        s.state = AgentState::Ended;
                        s
                    });
                    agent.upgrade_source_tool(source_tool);
                    agent.state = AgentState::Ended;
                    agent.waiting_since = None;
                    agent.permission_request = None;
                    agent.last_activity = now;
                }
                self.maybe_clear_subagents(&p.transcript_path);
            }

            HookEvent::StopFailure(p) => {
                let source_tool = p.source_tool.clone();
                self.clean_agent_flags(&p.transcript_path);
                {
                    let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                        let mut s = make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now);
                        s.state = AgentState::Error;
                        s
                    });
                    agent.upgrade_source_tool(source_tool);
                    agent.state = AgentState::Error;
                    agent.waiting_since = None;
                    agent.permission_request = None;
                    agent.last_activity = now;
                    if let Some(err) = &p.error_type {
                        agent.last_activity_label = Some(format!("Error: {err}"));
                    }
                }
                self.maybe_clear_subagents(&p.transcript_path);
            }

            HookEvent::UserPromptSubmit(p) => {
                let source_tool = p.source_tool.clone();
                self.clean_agent_flags(&p.transcript_path);
                let agent = self.agents.entry(p.transcript_path.clone()).or_insert_with(|| {
                    make_snapshot(&p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, source_tool.clone(), now)
                });
                agent.upgrade_source_tool(source_tool);
                agent.state = AgentState::Working;
                agent.permission_request = None;
                agent.last_activity = now;
                agent.last_activity_label = Some("Thinking\u{2026}".to_string());
            }

            HookEvent::SubagentStart(p) => {
                // Story 6.31 — anti-fantôme Grok : mémoriser la session enfant (`agent_id` =
                // `subagentId`) AVANT la garde `show_subagents`. La suppression du fantôme doit être
                // INDÉPENDANTE de la préférence d'affichage : un sous-agent Grok crée un fantôme que
                // son dot soit affiché ou non. Placer l'insertion sous le `return` ci-dessous la
                // laisserait vide quand `show_subagents == false` → les fantômes reviendraient.
                if p.source_tool == SourceTool::Grok && !p.agent_id.is_empty() {
                    self.grok_subagent_sessions.insert(p.agent_id.clone());
                    // Story 6.34 (review) — RÉCONCILIATION du fantôme précoce. La garde
                    // `is_grok_subagent_phantom` est PRÉVENTIVE : elle bloque la création mais ne
                    // nettoie pas un agent déjà créé. Sur pipe non ordonné, un event enfant
                    // (`stop_failure`/`notification`/tour) peut être traité AVANT ce `SubagentStart`
                    // (set encore vide + `summary.json` pas encore flushé) → un agent de 1er niveau
                    // fantôme a pu naître (en `Error` pour un `stop_failure`). `agent_id` (=
                    // `subagentId`) est UNIQUE à l'enfant → on évince tout agent portant ce
                    // `session_id` (jamais le parent, dont `session_id` = parent ∉ ce test).
                    self.agents.retain(|_, a| a.session_id.as_deref() != Some(p.agent_id.as_str()));
                }
                if !self.settings.show_subagents { return; }
                // Claude : transcript_path == parent (clé directe). Codex : transcript_path == le
                // sous-agent → parent résolu par session_id (story 6.14). Pas d'upsert : si le
                // parent est introuvable, on n'ajoute rien (jamais de faux agent).
                if let Some(key) = self.find_parent_key(&p.transcript_path, &p.session_id) {
                    if let Some(agent) = self.agents.get_mut(&key) {
                        agent.subagents.push(SubagentSnapshot {
                            agent_id: p.agent_id.clone(),
                            agent_type: p.agent_type.clone(),
                            state: SubagentState::Working,
                        });
                    }
                }
            }

            HookEvent::SubagentStop(p) => {
                if let Some(key) = self.find_parent_key(&p.transcript_path, &p.session_id) {
                    if let Some(agent) = self.agents.get_mut(&key) {
                        if let Some(sub) = agent.subagents.iter_mut()
                            .find(|s| s.agent_id == p.agent_id)
                        {
                            sub.state = SubagentState::Ended;
                        }
                    }
                    self.maybe_clear_subagents(&key);
                }
            }
        }
    }

    pub fn try_dismiss(&mut self, id: &str) -> bool {
        if matches!(self.agents.get(id).map(|a| &a.state), Some(AgentState::Disconnected)) {
            self.agents.remove(id);
            true
        } else {
            false
        }
    }

    pub fn set_disconnected(&mut self, transcript_path: &str) {
        if !self.agents.contains_key(transcript_path) {
            return;
        }
        self.clean_agent_flags(transcript_path);

        if self.settings.auto_cleanup_disconnected {
            self.agents.remove(transcript_path);
            return;
        }

        if let Some(agent) = self.agents.get_mut(transcript_path) {
            agent.state = AgentState::Disconnected;
            agent.waiting_since = None;
            agent.permission_request = None;
            agent.last_activity = now_ms();
            agent.subagents.clear();
        }
    }

    fn clean_agent_flags(&mut self, transcript_path: &str) {
        if let Some(agent) = self.agents.get_mut(transcript_path) {
            if agent.interrupt_pending {
                agent.interrupt_pending = false;
                let _ = std::fs::remove_file(crate::flags::interrupt_flag_path(transcript_path));
            }
        }
    }

    fn maybe_clear_subagents(&mut self, transcript_path: &str) {
        if let Some(agent) = self.agents.get_mut(transcript_path) {
            if !agent.subagents.is_empty() && agent.subagents.iter().all(|s| s.state == SubagentState::Ended) {
                agent.subagents.clear();
            }
        }
    }

    /// Résout la clé (transcript_path de l'agent PARENT) à laquelle rattacher un event sous-agent.
    /// - Claude Code : `SubagentStart.transcript_path` == transcript du PARENT → clé directe.
    /// - Codex : `SubagentStart.transcript_path` == transcript du SOUS-AGENT ; le parent partage
    ///   le même `session_id` (story 6.14) → recherche sur `session_id`.
    ///
    /// `None` si aucun agent ne correspond (event ignoré, jamais de faux agent créé).
    fn find_parent_key(&self, transcript_path: &str, session_id: &str) -> Option<String> {
        if self.agents.contains_key(transcript_path) {
            return Some(transcript_path.to_string());
        }
        if session_id.is_empty() {
            return None;
        }
        self.agents
            .iter()
            .find(|(_, a)| a.session_id.as_deref() == Some(session_id))
            .map(|(k, _)| k.clone())
    }

    /// Story 6.31 — `true` si `event` doit être SUPPRIMÉ parce qu'il émane d'un SOUS-AGENT Grok
    /// (session autonome dont les hooks créeraient un agent de 1er niveau via `or_insert`). Bornée
    /// `source_tool == Grok`. Deux discriminants :
    ///   1. `session_id ∈ grok_subagent_sessions` (ensemble alimenté par `SubagentStart.agent_id`) ;
    ///   2. repli cold-start (Vigil lancé après le spawn → set vide) : `summary.json.session_kind ==
    ///      "subagent"` sur disque (dossier dérivé du `transcript_path`). Le verdict positif est
    ///      mémorisé dans le set pour les events suivants (le set fait cache).
    ///
    /// `SubagentStart`/`SubagentStop` (dont le `session_id` = PARENT) sont EXCLUS : ils routent le
    /// dot vers le parent et ne doivent jamais être supprimés. `&mut self` pour le cache du repli.
    fn is_grok_subagent_phantom(&mut self, event: &HookEvent) -> bool {
        let (source_tool, session_id, transcript_path) = event_routing_keys(event);
        if *source_tool != SourceTool::Grok {
            return false; // chemin des autres outils byte-identique + court-circuit du re-parse
        }
        if matches!(event, HookEvent::SubagentStart(_) | HookEvent::SubagentStop(_)) {
            return false; // session_id = parent (∉ set) → jamais supprimés
        }
        if session_id.is_empty() {
            return false;
        }
        if self.grok_subagent_sessions.contains(session_id) {
            return true;
        }
        // Un `transcript_path` déjà connu comme agent de 1er niveau ne peut PAS être un sous-agent à
        // supprimer (les sous-agents n'obtiennent jamais de clé de 1er niveau). Court-circuite la
        // lecture disque sur le hot-path des sessions normales EN COURS (sinon `summary.json` serait
        // relu+parsé à CHAQUE event, le `session_id` parent n'entrant jamais dans le set) ; le
        // cold-start (transcript inconnu) continue de trancher via le disque ci-dessous.
        if self.agents.contains_key(transcript_path) {
            return false;
        }
        // Repli disque UNIQUEMENT quand ni le set ni les agents connus ne tranchent (Grok only).
        if is_grok_subagent_on_disk(transcript_path) {
            self.grok_subagent_sessions.insert(session_id.to_string());
            return true;
        }
        false
    }
}

/// Story 6.31 — extrait `(source_tool, session_id, transcript_path)` de n'importe quel event, en un
/// seul `match` exhaustif (sert à la garde anti-fantôme Grok). Miroir du `get_transcript_path` de
/// `manager.rs`, étendu au `source_tool`/`session_id`.
fn event_routing_keys(event: &HookEvent) -> (&SourceTool, &str, &str) {
    match event {
        HookEvent::SessionStart(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::SessionEnd(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::PreToolUse(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::PermissionRequest(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::PostToolUse(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::PostToolUseFailure(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::Notification(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::Stop(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::StopFailure(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::UserPromptSubmit(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::SubagentStart(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
        HookEvent::SubagentStop(p) => (&p.source_tool, &p.session_id, &p.transcript_path),
    }
}

/// Story 6.31 — repli cold-start : `true` si la session du `transcript_path` Grok est un sous-agent
/// d'après `summary.json.session_kind == "subagent"`. Le `transcript_path` Grok est
/// `<…>\<sessionId>\updates.jsonl` → le dossier de session est son `parent()`. Best-effort :
/// fichier/champ absent → `false` (jamais de panique). À n'appeler QUE pour un event Grok.
fn is_grok_subagent_on_disk(transcript_path: &str) -> bool {
    if transcript_path.is_empty() {
        return false;
    }
    let Some(session_dir) = std::path::Path::new(transcript_path).parent() else {
        return false;
    };
    crate::history::reader::read_grok_summary(session_dir)
        .session_kind
        .as_deref()
        == Some("subagent")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn make_snapshot(transcript_path: &str, session_id: &str, cwd: &str, terminal_pid: Option<u32>, source_tool: SourceTool, now: u64) -> AgentSnapshot {
    AgentSnapshot {
        transcript_path: transcript_path.to_string(),
        session_id: Some(session_id.to_string()),
        name: name_from_cwd(cwd),
        state: AgentState::Idle,
        waiting_since: None,
        last_activity: now,
        last_activity_label: None,
        terminal_pid,
        source: "Terminal".to_string(),
        permission_request: None,
        interrupt_pending: false,
        model: None,
        cwd: if cwd.is_empty() { None } else { Some(cwd.to_string()) },
        subagents: vec![],
        custom_name: None,
        description: None,
        tags: vec![],
        pinned: false,
        source_tool,
    }
}

fn name_from_cwd(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn derive_activity_label(tool_name: &str, tool_input: &serde_json::Value) -> String {
    match tool_name {
        "Read" => {
            let path = tool_input.get("file_path").and_then(|v| v.as_str()).unwrap_or("...");
            format!("Reading {path}")
        }
        "Edit" | "Write" => {
            let path = tool_input.get("file_path").and_then(|v| v.as_str()).unwrap_or("...");
            format!("Editing {path}")
        }
        "Bash" => {
            let cmd = tool_input.get("command").and_then(|v| v.as_str()).unwrap_or("...");
            format!("Running: {cmd}")
        }
        "WebFetch" => "Fetching URL...".to_string(),
        "Glob" | "Grep" => "Searching files...".to_string(),
        "TodoWrite" => "Updating tasks...".to_string(),
        other => format!("Using {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::protocol::*;

    fn make_session_start(transcript_path: &str) -> HookEvent {
        HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        })
    }

    fn make_notification(transcript_path: &str, message: &str) -> HookEvent {
        HookEvent::Notification(NotificationPayload {
            session_id: "s1".into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            message: message.into(),
            notification_type: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        })
    }

    // ── SessionStart ─────────────────────────────────────────────────────────

    #[test]
    fn session_start_cree_agent_idle() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(agent.transcript_path, "/t.jsonl");
    }

    #[test]
    fn session_start_stocke_parent_pid() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: Some(1234),
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].terminal_pid, Some(1234));
    }

    #[test]
    fn session_start_nom_depuis_cwd() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/home/user/mon-projet".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].name, "mon-projet");
    }

    // ── PreToolUse / PostToolUse ──────────────────────────────────────────────

    #[test]
    fn pre_tool_use_transition_vers_working() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "cargo test"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Working);
        assert_eq!(agent.last_activity_label.as_deref(), Some("Running: cargo test"));
    }

    #[test]
    fn pre_tool_use_sans_session_start_cree_agent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "main.rs"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Working);
        assert_eq!(agent.last_activity_label.as_deref(), Some("Reading main.rs"));
    }

    #[test]
    fn post_tool_use_transition_vers_working() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PostToolUse(PostToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "main.rs"}),
            tool_response: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Working);
        assert_eq!(agent.last_activity_label.as_deref(), Some("Reading main.rs"));
    }

    #[test]
    fn post_tool_use_sans_session_start_cree_agent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PostToolUse(PostToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_response: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Working);
    }

    // ── PermissionRequest ─────────────────────────────────────────────────────

    #[test]
    fn permission_request_outil_classique_vers_waiting_approve() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::WaitingApprove);
        assert!(engine.agents["/t.jsonl"].waiting_since.is_some());
    }

    #[test]
    fn permission_request_ask_user_question_vers_waiting_specific() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::WaitingSpecific);
    }

    #[test]
    fn permission_request_sans_session_start_cree_agent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::WaitingApprove);
        assert!(agent.waiting_since.is_some());
    }

    // ── Notification ──────────────────────────────────────────────────────────

    #[test]
    fn notification_depuis_idle_garde_etat_idle() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_notification("/t.jsonl", "Agent started"));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Idle);
    }

    #[test]
    fn notification_stocke_message_comme_label() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_notification("/t.jsonl", "Tâche terminée"));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Tâche terminée"));
    }

    #[test]
    fn notification_depuis_waiting_approve_garde_etat() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        engine.apply_event(&make_notification("/t.jsonl", "still waiting"));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::WaitingApprove);
    }

    #[test]
    fn notification_sans_session_start_cree_agent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_notification("/t.jsonl", "hello"));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(agent.last_activity_label.as_deref(), Some("hello"));
    }

    // ── Stop / StopFailure ────────────────────────────────────────────────────

    #[test]
    fn stop_transition_vers_ended() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::Stop(StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Ended);
        assert!(agent.waiting_since.is_none());
    }

    #[test]
    fn stop_sans_session_start_cree_agent_ended() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::Stop(StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Ended);
    }

    #[test]
    fn stop_failure_transition_vers_error() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::StopFailure(StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: Some("rate_limit".into()),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Error);
        assert!(agent.waiting_since.is_none());
        assert_eq!(agent.last_activity_label.as_deref(), Some("Error: rate_limit"));
    }

    #[test]
    fn stop_failure_sans_session_start_cree_agent_error() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::StopFailure(StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: Some("billing_error".into()),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Error);
        assert_eq!(agent.last_activity_label.as_deref(), Some("Error: billing_error"));
    }

    // ── SessionEnd / Disconnected ─────────────────────────────────────────────

    #[test]
    fn session_end_transition_vers_disconnected() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::SessionEnd(SessionEndPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            reason: Some("clear".into()),
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Disconnected);
        assert!(agent.waiting_since.is_none());
    }

    #[test]
    fn session_end_sans_agent_existant_ne_cree_pas() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionEnd(SessionEndPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            reason: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(!engine.agents.contains_key("/t.jsonl"));
    }

    #[test]
    fn eof_pipe_transition_vers_disconnected() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.set_disconnected("/t.jsonl");
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Disconnected);
        assert!(agent.waiting_since.is_none());
    }

    #[test]
    fn set_disconnected_ne_cree_pas_agent() {
        let mut engine = AgentStateEngine::new();
        engine.set_disconnected("/t.jsonl");
        assert!(!engine.agents.contains_key("/t.jsonl"));
    }

    // ── SessionStart sur agent existant ───────────────────────────────────────

    #[test]
    fn session_start_sur_agent_existant_le_remplace() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        // Simuler un état avancé
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Working);
        // Nouvelle session sur le même transcript_path (ex : /clear)
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s2".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(agent.session_id.as_deref(), Some("s2"));
        assert!(agent.last_activity_label.is_none());
    }

    // ── Notification — cas limites ────────────────────────────────────────────

    #[test]
    fn notification_depuis_waiting_specific_garde_etat() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        engine.apply_event(&make_notification("/t.jsonl", "ping"));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::WaitingSpecific);
    }

    #[test]
    fn notification_message_vide_ne_change_pas_label() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_notification("/t.jsonl", "premier label"));
        engine.apply_event(&make_notification("/t.jsonl", ""));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("premier label"));
    }

    // ── StopFailure sans error_type ───────────────────────────────────────────

    #[test]
    fn stop_failure_sans_error_type_ne_change_pas_label() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_notification("/t.jsonl", "label existant"));
        engine.apply_event(&HookEvent::StopFailure(StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Error);
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("label existant"));
    }

    // ── session_id stocké sur upsert ──────────────────────────────────────────

    #[test]
    fn upsert_stocke_session_id() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PostToolUse(PostToolUsePayload {
            session_id: "sess-42".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_response: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].session_id.as_deref(), Some("sess-42"));
    }

    // ── Indépendance entre agents ─────────────────────────────────────────────

    #[test]
    fn deux_agents_sont_independants() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/a.jsonl"));
        engine.apply_event(&make_session_start("/b.jsonl"));
        engine.apply_event(&HookEvent::Stop(StopPayload {
            session_id: "s1".into(),
            transcript_path: "/a.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert_eq!(engine.agents["/a.jsonl"].state, AgentState::Ended);
        assert_eq!(engine.agents["/b.jsonl"].state, AgentState::Idle);
    }

    // ── permission_request — stockage et effacement ───────────────────────────

    #[test]
    fn permission_request_stocke_tool_name_et_input() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf dist/"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        let pr = agent.permission_request.as_ref().expect("permission_request should be Some");
        assert_eq!(pr.tool_name, "Bash");
        assert_eq!(pr.tool_input["command"], "rm -rf dist/");
    }

    #[test]
    fn stop_efface_permission_request() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "main.rs"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(engine.agents["/t.jsonl"].permission_request.is_some());
        engine.apply_event(&HookEvent::Stop(StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert!(engine.agents["/t.jsonl"].permission_request.is_none());
    }

    #[test]
    fn session_start_efface_permission_request() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(engine.agents["/t.jsonl"].permission_request.is_some());
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s2".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(engine.agents["/t.jsonl"].permission_request.is_none());
    }

    // ── PostToolUseFailure ────────────────────────────────────────────────────

    #[test]
    fn post_tool_use_failure_interrupt_termine_agent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PostToolUseFailure(PostToolUseFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "npm test"}),
            error: Some("Command exited with non-zero status code 1".into()),
            is_interrupt: true,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Ended);
    }

    #[test]
    fn post_tool_use_failure_no_interrupt_garde_etat_et_met_label() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "npm test"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Working);
        engine.apply_event(&HookEvent::PostToolUseFailure(PostToolUseFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "npm test"}),
            error: Some("exit code 1".into()),
            is_interrupt: false,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Working);
        assert_eq!(
            engine.agents["/t.jsonl"].last_activity_label.as_deref(),
            Some("Failed: Running: npm test")
        );
    }

    // ── Nettoyage des flags sur transitions ───────────────────────────────────

    #[test]
    fn interrupt_flag_champs_nettoyes_sur_stop() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.agents.get_mut("/t.jsonl").unwrap().interrupt_pending = true;
        engine.apply_event(&HookEvent::Stop(StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert!(!engine.agents["/t.jsonl"].interrupt_pending);
    }

    #[test]
    fn interrupt_flag_champs_nettoyes_sur_stop_failure() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.agents.get_mut("/t.jsonl").unwrap().interrupt_pending = true;
        engine.apply_event(&HookEvent::StopFailure(StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(!engine.agents["/t.jsonl"].interrupt_pending);
    }

    #[test]
    fn flags_nettoyes_sur_permission_request() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.agents.get_mut("/t.jsonl").unwrap().interrupt_pending = true;
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            read_only: false,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(!engine.agents["/t.jsonl"].interrupt_pending);
    }

    #[test]
    fn flags_nettoyes_sur_set_disconnected() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.agents.get_mut("/t.jsonl").unwrap().interrupt_pending = true;
        engine.set_disconnected("/t.jsonl");
        assert!(!engine.agents["/t.jsonl"].interrupt_pending);
    }

    // ── derive_activity_label — toutes les branches ───────────────────────────

    #[test]
    fn label_edit() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "src/lib.rs"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Editing src/lib.rs"));
    }

    #[test]
    fn label_write() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": "out.txt"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Editing out.txt"));
    }

    #[test]
    fn label_webfetch() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "WebFetch".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Fetching URL..."));
    }

    #[test]
    fn label_glob_grep() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        for tool in ["Glob", "Grep"] {
            engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
                session_id: "s1".into(),
                transcript_path: "/t.jsonl".into(),
                cwd: "/project".into(),
                tool_name: tool.into(),
                tool_input: serde_json::json!({}),
                parent_pid: None,
                source_tool: SourceTool::Unknown,
            }));
            assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Searching files..."));
        }
    }

    #[test]
    fn label_todowrite() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "TodoWrite".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Updating tasks..."));
    }

    #[test]
    fn label_outil_inconnu() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "MonOutilMCP".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].last_activity_label.as_deref(), Some("Using MonOutilMCP"));
    }

    // ── UserPromptSubmit ──────────────────────────────────────────────────────

    #[test]
    fn user_prompt_submit_transite_vers_working() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&HookEvent::UserPromptSubmit(UserPromptSubmitPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            prompt: Some("Bonjour".into()),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.state, AgentState::Working);
        assert_eq!(agent.last_activity_label.as_deref(), Some("Thinking\u{2026}"));
        assert!(agent.permission_request.is_none());
    }

    #[test]
    fn session_start_stocke_model() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: Some("claude-opus-4-7".into()),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn terminal_source_du_payload_peuple_le_snapshot() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: Some("VS Code".into()),
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source, "VS Code");
    }

    #[test]
    fn terminal_source_absent_ou_vide_garde_defaut_terminal() {
        let mut engine = AgentStateEngine::new();
        // Absent → défaut « Terminal ».
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/a.jsonl".into(),
            cwd: "/p".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/a.jsonl"].source, "Terminal");
        // Vide → on garde aussi le défaut (pas de badge vide).
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s2".into(),
            transcript_path: "/b.jsonl".into(),
            cwd: "/p".into(),
            source: None,
            terminal_source: Some("".into()),
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/b.jsonl"].source, "Terminal");
        // Blanc-seul → trim puis défaut « Terminal » (pas de badge vide, code-review 6.38).
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s3".into(),
            transcript_path: "/c.jsonl".into(),
            cwd: "/p".into(),
            source: None,
            terminal_source: Some("   ".into()),
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/c.jsonl"].source, "Terminal");
    }

    #[test]
    fn model_absent_accepte() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert!(engine.agents["/t.jsonl"].model.is_none());
    }

    // ── SubagentStart / SubagentStop ──────────────────────────────────────────

    fn make_subagent_start(transcript_path: &str, agent_id: &str, agent_type: &str) -> HookEvent {
        HookEvent::SubagentStart(crate::pipe::protocol::SubagentStartPayload {
            session_id: "s1".into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            agent_id: agent_id.into(),
            agent_type: agent_type.into(),
            source_tool: SourceTool::Unknown,
        })
    }

    fn make_subagent_stop(transcript_path: &str, agent_id: &str, agent_type: &str) -> HookEvent {
        HookEvent::SubagentStop(crate::pipe::protocol::SubagentStopPayload {
            session_id: "s1".into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            agent_id: agent_id.into(),
            agent_type: agent_type.into(),
            agent_transcript_path: None,
            stop_hook_active: false,
            source_tool: SourceTool::Unknown,
        })
    }

    #[test]
    fn subagent_start_ajoute_dot() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.subagents.len(), 1);
        assert_eq!(agent.subagents[0].agent_id, "uuid-1");
        assert_eq!(agent.subagents[0].agent_type, "Explore");
        assert_eq!(agent.subagents[0].state, SubagentState::Working);
    }

    #[test]
    fn subagent_stop_dernier_vide_vec() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.apply_event(&make_subagent_stop("/t.jsonl", "uuid-1", "Explore"));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn subagent_stop_partiel_ne_vide_pas_vec() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-2", "claude"));
        engine.apply_event(&make_subagent_stop("/t.jsonl", "uuid-1", "Explore"));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.subagents.len(), 2);
        assert_eq!(agent.subagents[0].state, SubagentState::Ended);
        assert_eq!(agent.subagents[1].state, SubagentState::Working);
    }

    #[test]
    fn stop_vide_vec_si_tous_ended() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        // Marque Ended directement sans SubagentStop (qui viderait déjà la Vec)
        engine.agents.get_mut("/t.jsonl").unwrap().subagents[0].state = SubagentState::Ended;
        engine.apply_event(&HookEvent::Stop(crate::pipe::protocol::StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn stop_garde_vec_si_working_actifs() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.apply_event(&HookEvent::Stop(crate::pipe::protocol::StopPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
            interrupted: false,
        }));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 1);
        assert_eq!(engine.agents["/t.jsonl"].subagents[0].state, SubagentState::Working);
    }

    #[test]
    fn stop_failure_vide_vec_si_tous_ended() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.agents.get_mut("/t.jsonl").unwrap().subagents[0].state = SubagentState::Ended;
        engine.apply_event(&HookEvent::StopFailure(crate::pipe::protocol::StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn stop_failure_garde_vec_si_working_actifs() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.apply_event(&HookEvent::StopFailure(crate::pipe::protocol::StopFailurePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            error_type: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 1);
        assert_eq!(engine.agents["/t.jsonl"].subagents[0].state, SubagentState::Working);
    }

    // ── set_auto_permission_label ──────────────────────────────────────────────────
    #[test]
    fn auto_label_met_a_jour_agent_existant() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.set_auto_permission_label("/t.jsonl", "s1", "/cwd", None, "Auto-approved: Bash");
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(
            agent.last_activity_label.as_deref(),
            Some("Auto-approved: Bash")
        );
        assert_eq!(agent.state, AgentState::Idle);
    }

    #[test]
    fn auto_label_cree_agent_si_absent() {
        let mut engine = AgentStateEngine::new();
        engine.set_auto_permission_label("/new.jsonl", "s1", "/cwd", None, "Auto-denied: Read");
        assert!(engine.agents.contains_key("/new.jsonl"));
        assert_eq!(
            engine.agents["/new.jsonl"].last_activity_label.as_deref(),
            Some("Auto-denied: Read")
        );
    }

    #[test]
    fn set_disconnected_vide_vec_inconditionnellement() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.set_disconnected("/t.jsonl");
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn multiples_subagents_independants() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-2", "general-purpose"));
        engine.apply_event(&make_subagent_stop("/t.jsonl", "uuid-1", "Explore"));
        let agent = &engine.agents["/t.jsonl"];
        assert_eq!(agent.subagents.len(), 2);
        assert_eq!(agent.subagents[0].state, SubagentState::Ended);
        assert_eq!(agent.subagents[1].state, SubagentState::Working);
    }

    #[test]
    fn subagents_nettoyes_sur_stop_parent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.apply_event(&make_subagent_start("/t.jsonl", "uuid-1", "Explore"));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 1);
        engine.apply_event(&make_session_start("/t.jsonl"));
        assert_eq!(engine.agents["/t.jsonl"].subagents.len(), 0);
    }

    // ── Story 6.14 : sous-agents Codex (transcript_path du sous-agent, parent via session_id) ──

    // SubagentStart Codex : le transcript_path pointe le SOUS-AGENT (≠ parent), mais session_id
    // == parent. Le point doit se rattacher au parent, résolu par session_id.
    fn make_subagent_start_codex(sub_transcript: &str, session_id: &str, agent_id: &str) -> HookEvent {
        HookEvent::SubagentStart(crate::pipe::protocol::SubagentStartPayload {
            session_id: session_id.into(),
            transcript_path: sub_transcript.into(),
            cwd: "/project".into(),
            agent_id: agent_id.into(),
            agent_type: "worker".into(),
            source_tool: SourceTool::Codex,
        })
    }

    fn make_subagent_stop_codex(sub_transcript: &str, session_id: &str, agent_id: &str) -> HookEvent {
        HookEvent::SubagentStop(crate::pipe::protocol::SubagentStopPayload {
            session_id: session_id.into(),
            transcript_path: sub_transcript.into(),
            cwd: "/project".into(),
            agent_id: agent_id.into(),
            agent_type: "worker".into(),
            agent_transcript_path: None,
            stop_hook_active: false,
            source_tool: SourceTool::Codex,
        })
    }

    #[test]
    fn subagent_start_codex_rattache_au_parent_par_session_id() {
        let mut engine = AgentStateEngine::new();
        // make_session_start crée un parent keyé "/parent.jsonl" avec session_id "s1".
        engine.apply_event(&make_session_start("/parent.jsonl"));
        // Le SubagentStart Codex porte le transcript du SOUS-AGENT ("/sub.jsonl"), session "s1".
        engine.apply_event(&make_subagent_start_codex("/sub.jsonl", "s1", "uuid-codex-1"));
        // Aucun agent fantôme sous la clé du sous-agent.
        assert!(!engine.agents.contains_key("/sub.jsonl"));
        // Le point est rattaché au parent.
        let parent = &engine.agents["/parent.jsonl"];
        assert_eq!(parent.subagents.len(), 1);
        assert_eq!(parent.subagents[0].agent_id, "uuid-codex-1");
        assert_eq!(parent.subagents[0].state, SubagentState::Working);
    }

    #[test]
    fn subagent_stop_codex_termine_et_nettoie_par_session_id() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/parent.jsonl"));
        engine.apply_event(&make_subagent_start_codex("/sub.jsonl", "s1", "uuid-codex-1"));
        engine.apply_event(&make_subagent_stop_codex("/sub.jsonl", "s1", "uuid-codex-1"));
        // Dernier sous-agent terminé → Vec vidée (maybe_clear sur la clé parent).
        assert_eq!(engine.agents["/parent.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn subagent_start_codex_sans_parent_ne_cree_rien() {
        let mut engine = AgentStateEngine::new();
        // Aucun parent avec session "s-inconnu" → find_parent_key renvoie None → rien créé.
        engine.apply_event(&make_subagent_start_codex("/sub.jsonl", "s-inconnu", "uuid-x"));
        assert!(engine.agents.is_empty());
    }

    // ── Story 6.31 : sous-agents Grok (session autonome → suppression stateful anti-fantôme) ──

    // SubagentStart Grok : parent-side (transcript_path = parent, comme Claude) ; `agent_id` =
    // `subagentId` = SESSION ENFANT (que l'enfant portera en `session_id` sur ses propres events).
    fn make_grok_subagent_start(parent_transcript: &str, parent_sid: &str, child_sid: &str) -> HookEvent {
        HookEvent::SubagentStart(SubagentStartPayload {
            session_id: parent_sid.into(),
            transcript_path: parent_transcript.into(),
            cwd: "/project".into(),
            agent_id: child_sid.into(),
            agent_type: "Explore".into(),
            source_tool: SourceTool::Grok,
        })
    }

    fn make_grok_subagent_stop(parent_transcript: &str, parent_sid: &str, child_sid: &str) -> HookEvent {
        HookEvent::SubagentStop(SubagentStopPayload {
            session_id: parent_sid.into(),
            transcript_path: parent_transcript.into(),
            cwd: "/project".into(),
            agent_id: child_sid.into(),
            agent_type: "".into(),
            agent_transcript_path: None,
            stop_hook_active: false,
            source_tool: SourceTool::Grok,
        })
    }

    // Event de tour Grok arbitraire (session enfant = celui qui créerait le fantôme).
    fn make_grok_pretool(transcript_path: &str, session_id: &str) -> HookEvent {
        HookEvent::PreToolUse(PreToolUsePayload {
            session_id: session_id.into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            tool_name: "run_terminal_command".into(),
            tool_input: serde_json::json!({ "command": "dir" }),
            parent_pid: None,
            source_tool: SourceTool::Grok,
        })
    }

    fn make_grok_session_start(transcript_path: &str, session_id: &str) -> HookEvent {
        HookEvent::SessionStart(SessionStartPayload {
            session_id: session_id.into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Grok,
        })
    }

    #[test]
    fn grok_events_du_sous_agent_ne_creent_pas_de_fantome() {
        // AC5 : après un SubagentStart Grok (agent_id = session enfant), tout event de tour porté par
        // le session_id enfant est SUPPRIMÉ → zéro agent de 1er niveau (aucune clé enfant).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        // Le dot est bien rattaché au parent (find_parent_key clé directe comme Claude).
        assert_eq!(engine.agents["/parent.jsonl"].subagents.len(), 1);
        // Un PreToolUse du sous-agent (SON propre transcript_path + session_id enfant) → supprimé.
        engine.apply_event(&make_grok_pretool("/child.jsonl", "child-sid"));
        assert!(!engine.agents.contains_key("/child.jsonl"), "aucun agent fantôme enfant");
        assert_eq!(engine.agents.len(), 1, "seul le parent existe");
    }

    #[test]
    fn grok_events_du_parent_restent_traites() {
        // AC5 : les events du PARENT (session_id parent, ∉ set) ne sont JAMAIS supprimés.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        engine.apply_event(&make_grok_pretool("/parent.jsonl", "parent-sid"));
        let parent = &engine.agents["/parent.jsonl"];
        assert_eq!(parent.state, AgentState::Working);
        assert_eq!(parent.last_activity_label.as_deref(), Some("Using run_terminal_command"));
    }

    #[test]
    fn grok_subagent_end_marque_ended_et_nettoie() {
        // AC4 : SubagentEnd → SubagentStop interne marque Ended + maybe_clear (dernier → Vec vidée).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        engine.apply_event(&make_grok_subagent_stop("/parent.jsonl", "parent-sid", "child-sid"));
        assert_eq!(engine.agents["/parent.jsonl"].subagents.len(), 0);
    }

    #[test]
    fn grok_set_peuple_meme_si_show_subagents_desactive() {
        // AC5 + piège d'ordre : l'insertion dans le set est AVANT la garde `show_subagents`. Avec
        // l'affichage désactivé, le dot n'apparaît pas MAIS le fantôme est quand même supprimé.
        let mut engine = AgentStateEngine::new();
        engine.set_settings(HookSettings { show_subagents: false, ..Default::default() });
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        assert_eq!(engine.agents["/parent.jsonl"].subagents.len(), 0, "dot masqué (show_subagents off)");
        // Mais l'event enfant est toujours supprimé (set peuplé malgré le return de la garde).
        engine.apply_event(&make_grok_pretool("/child.jsonl", "child-sid"));
        assert!(!engine.agents.contains_key("/child.jsonl"), "fantôme supprimé même affichage off");
    }

    #[test]
    fn grok_post_tool_use_failure_subagent_supprime_par_garde() {
        // Story 6.37, AC4 : un PostToolUseFailure d'un SOUS-AGENT (session_id enfant, ∈ set) est
        // supprimé par la garde 6.31 → zéro agent fantôme, zéro label « Failed: … » parasité.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        // PostToolUseFailure du sous-agent (session_id = child-sid, ∈ set) → supprimé.
        engine.apply_event(&HookEvent::PostToolUseFailure(PostToolUseFailurePayload {
            session_id: "child-sid".into(),
            transcript_path: "/child.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "run_terminal_command".into(),
            tool_input: serde_json::json!({"command": "exit 1"}),
            error: Some("Command failed".into()),
            is_interrupt: false,
            parent_pid: None,
            source_tool: SourceTool::Grok,
        }));
        assert!(!engine.agents.contains_key("/child.jsonl"), "aucun agent fantôme sur PostToolUseFailure subagent");
        assert_eq!(engine.agents.len(), 1, "seul le parent existe");
        // Aucun agent (parent ou fantôme enfant) ne doit porter de label « Failed: … » issu de
        // l'event supprimé. Assertion porteuse : si la garde 6.31 sautait, l'event créerait un agent
        // « /child.jsonl » avec un label « Failed: … » → cette vérif échouerait (indépendante du
        // rendu exact du nom d'outil, cf. `derive_activity_label`).
        assert!(
            engine
                .agents
                .values()
                .all(|a| !a.last_activity_label.as_deref().unwrap_or_default().starts_with("Failed:")),
            "event subagent supprimé ne doit poser aucun label d'échec"
        );
    }

    #[test]
    fn grok_repli_cold_start_session_kind_supprime_sans_subagent_start() {
        // AC6 : Vigil lancé APRÈS le spawn (set vide) → repli disque `session_kind == "subagent"`.
        let dir = std::env::temp_dir().join(format!("vigil_se_grok_kind_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.json"), r#"{"session_kind":"subagent"}"#).unwrap();
        let transcript = dir.join("updates.jsonl");
        let tp = transcript.to_str().unwrap();

        let mut engine = AgentStateEngine::new();
        // AUCUN SubagentStart préalable → seul le disque tranche.
        engine.apply_event(&make_grok_pretool(tp, "child-sid-inconnu"));
        assert!(engine.agents.is_empty(), "sous-agent détecté par session_kind disque → supprimé");
        // Le verdict est mémorisé dans le set (les events suivants ne relisent pas le disque).
        let _ = std::fs::remove_file(dir.join("summary.json")); // le set doit suffire désormais
        engine.apply_event(&make_grok_pretool(tp, "child-sid-inconnu"));
        assert!(engine.agents.is_empty(), "verdict mémorisé dans le set");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_repli_session_normale_non_supprimee() {
        // AC6 : une session normale (pas de session_kind) n'est PAS supprimée par le repli.
        let dir = std::env::temp_dir().join(format!("vigil_se_grok_normal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.json"), r#"{"current_model_id":"grok-4.5"}"#).unwrap();
        let tp = dir.join("updates.jsonl");
        let tp = tp.to_str().unwrap();

        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_pretool(tp, "sid-normale"));
        assert!(engine.agents.contains_key(tp), "session normale → agent créé normalement");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_agent_connu_court_circuite_le_repli_disque() {
        // P1 (code review 2026-07-15) : un event dont le `transcript_path` est DÉJÀ un agent de 1er
        // niveau connu n'est jamais supprimé et ne relit pas le disque — même si un summary.json
        // "subagent" traînait dans son dossier. Prouve la précédence « agent connu > repli disque »
        // (sans le court-circuit, le disque trancherait "subagent" et l'event serait supprimé).
        let dir = std::env::temp_dir().join(format!("vigil_se_grok_known_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("updates.jsonl");
        let tp = transcript.to_str().unwrap();

        let mut engine = AgentStateEngine::new();
        // 1er event : session normale → agent de 1er niveau créé (pas de summary.json encore).
        engine.apply_event(&make_grok_session_start(tp, "sid-normale"));
        assert!(engine.agents.contains_key(tp), "agent de 1er niveau créé");

        // summary.json "subagent" planté APRÈS coup : le court-circuit doit l'ignorer.
        std::fs::write(dir.join("summary.json"), r#"{"session_kind":"subagent"}"#).unwrap();
        engine.apply_event(&make_grok_pretool(tp, "sid-normale"));
        assert_eq!(
            engine.agents[tp].last_activity_label.as_deref(),
            Some("Using run_terminal_command"),
            "agent connu → event traité (court-circuit avant le repli disque subagent)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suppression_grok_ne_touche_pas_les_autres_outils() {
        // AC9 : le mécanisme est borné Grok. Un event Claude/Codex avec un session_id quelconque
        // n'est jamais supprimé par is_grok_subagent_phantom (le set n'est peuplé que par Grok).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        // Un event Claude qui porterait PAR HASARD le même session_id "child-sid" n'est PAS supprimé.
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "child-sid".into(),
            transcript_path: "/claude.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            parent_pid: None,
            source_tool: SourceTool::ClaudeCode,
        }));
        assert!(engine.agents.contains_key("/claude.jsonl"), "Claude non affecté par la garde Grok");
    }

    // ── Story 6.34 : État & erreurs Grok (Notification / StopFailure) ─────────
    // Handlers socle RÉUTILISÉS INCHANGÉS (badge Error + libellé d'activité) ; la story ne fait que
    // faire ARRIVER ces events depuis Grok. Fixtures calquées sur `events-run2.jsonl`.

    fn make_grok_notification(transcript_path: &str, session_id: &str, message: &str) -> HookEvent {
        HookEvent::Notification(NotificationPayload {
            session_id: session_id.into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            message: message.into(),
            notification_type: Some("agent_error".into()),
            parent_pid: None,
            source_tool: SourceTool::Grok,
        })
    }

    fn make_grok_stop_failure(transcript_path: &str, session_id: &str, error: Option<&str>) -> HookEvent {
        HookEvent::StopFailure(StopFailurePayload {
            session_id: session_id.into(),
            transcript_path: transcript_path.into(),
            cwd: "/project".into(),
            error_type: error.map(str::to_string),
            parent_pid: None,
            source_tool: SourceTool::Grok,
        })
    }

    #[test]
    fn grok_stop_failure_parent_passe_en_error() {
        // AC4 : un StopFailure Grok du PARENT (transcript_path connu) → AgentState::Error + libellé.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_stop_failure(
            "/parent.jsonl",
            "parent-sid",
            Some("API error status 429 rate-limit"),
        ));
        let agent = &engine.agents["/parent.jsonl"];
        assert_eq!(agent.state, AgentState::Error);
        assert!(agent.waiting_since.is_none());
        assert_eq!(agent.last_activity_label.as_deref(), Some("Error: API error status 429 rate-limit"));
        // AC4 : l'état Error est surchargé par la prochaine activité réelle (parité Claude).
        engine.apply_event(&make_grok_pretool("/parent.jsonl", "parent-sid"));
        assert_eq!(engine.agents["/parent.jsonl"].state, AgentState::Working);
    }

    #[test]
    fn grok_notification_annote_le_libelle_sans_changer_l_etat() {
        // AC5 : une Notification Grok met à jour last_activity_label SANS changer l'état.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_pretool("/parent.jsonl", "parent-sid")); // état = Working
        engine.apply_event(&make_grok_notification("/parent.jsonl", "parent-sid", "Turn complete"));
        let agent = &engine.agents["/parent.jsonl"];
        assert_eq!(agent.state, AgentState::Working, "Notification n'entraîne PAS l'état Error");
        assert_eq!(agent.last_activity_label.as_deref(), Some("Turn complete"));
    }

    #[test]
    fn grok_notification_preserve_l_overlay_de_permission_6_30() {
        // AC5 (garantie porteuse « handler annotatif → overlay 6.30 PRÉSERVÉ ») : un `permission_prompt`
        // /`elicitation_dialog` en Notification arrive pendant qu'un agent attend une permission. Le
        // handler ne doit toucher NI `state`, NI `waiting_since`, NI `permission_request` — seul le
        // libellé bouge. Verrouille la réutilisation stricte du handler (un refactor qui écraserait ces
        // champs casserait ce test au lieu de passer silencieusement).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PermissionRequest(PermissionRequestPayload {
            session_id: "parent-sid".into(),
            transcript_path: "/parent.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "run_terminal_command".into(),
            tool_input: serde_json::json!({ "command": "dir" }),
            parent_pid: None,
            source_tool: SourceTool::Grok,
            read_only: false,
        }));
        let waiting_before = engine.agents["/parent.jsonl"].waiting_since;
        assert_eq!(engine.agents["/parent.jsonl"].state, AgentState::WaitingApprove);
        assert!(engine.agents["/parent.jsonl"].permission_request.is_some());
        // Notification `permission_prompt` (émise ~0,6 s après PreToolUse en mode qui prompte).
        engine.apply_event(&make_grok_notification(
            "/parent.jsonl",
            "parent-sid",
            "Tool permission requested",
        ));
        let agent = &engine.agents["/parent.jsonl"];
        assert_eq!(agent.state, AgentState::WaitingApprove, "état d'attente préservé");
        assert_eq!(agent.waiting_since, waiting_before, "waiting_since intact");
        assert!(agent.permission_request.is_some(), "overlay de permission préservé");
        assert_eq!(agent.last_activity_label.as_deref(), Some("Tool permission requested"));
    }

    #[test]
    fn grok_notification_stop_failure_du_sous_agent_supprimes() {
        // AC6 (calqué `events-run2.jsonl`) : après un SubagentStart Grok (agent_id = session enfant),
        // un Notification `agent_error` ET un StopFailure portés par le session_id ENFANT sont
        // supprimés par la garde 6.31 → aucun agent fantôme, aucun badge Error parasite.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        engine.apply_event(&make_grok_notification("/child.jsonl", "child-sid", "Rate limited 429"));
        engine.apply_event(&make_grok_stop_failure("/child.jsonl", "child-sid", Some("API error 429")));
        assert!(!engine.agents.contains_key("/child.jsonl"), "aucun agent fantôme enfant");
        assert_eq!(engine.agents.len(), 1, "seul le parent existe");
        assert_ne!(engine.agents["/parent.jsonl"].state, AgentState::Error, "pas de badge Error parasite sur le parent");
    }

    #[test]
    fn grok_stop_failure_enfant_precoce_reconcilie_par_subagent_start_tardif() {
        // Défer 1 (review 6.34) : pipe NON ORDONNÉ. Un `stop_failure` de sous-agent arrive AVANT son
        // `SubagentStart` (set vide) → la garde préventive ne le supprime pas → fantôme top-level en
        // `Error`. Le `SubagentStart` tardif doit ÉVINCER ce fantôme (réconciliation).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        // Ordre défavorable : l'event enfant précède son SubagentStart.
        engine.apply_event(&make_grok_stop_failure("/child.jsonl", "child-sid", Some("API error 429")));
        assert_eq!(engine.agents["/child.jsonl"].state, AgentState::Error, "fantôme précoce créé (garde préventive)");
        // Le SubagentStart tardif réconcilie : le fantôme enfant disparaît.
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        assert!(!engine.agents.contains_key("/child.jsonl"), "fantôme enfant évincé par le SubagentStart tardif");
        assert_eq!(engine.agents.len(), 1, "seul le parent subsiste");
        assert_ne!(engine.agents["/parent.jsonl"].state, AgentState::Error, "parent jamais évincé/altéré");
    }

    #[test]
    fn grok_stop_failure_du_parent_traite_normalement() {
        // AC6 : un StopFailure du PARENT (session_id parent ∉ set) est traité normalement (AC4).
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        engine.apply_event(&make_grok_stop_failure("/parent.jsonl", "parent-sid", Some("boom")));
        assert_eq!(engine.agents["/parent.jsonl"].state, AgentState::Error);
    }

    #[test]
    fn grok_stop_failure_claude_meme_session_id_non_supprime() {
        // AC6/AC9 non-régression : la garde est bornée Grok. Un StopFailure CLAUDE qui porterait par
        // hasard le même session_id "child-sid" qu'un sous-agent Grok n'est JAMAIS supprimé.
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_grok_session_start("/parent.jsonl", "parent-sid"));
        engine.apply_event(&make_grok_subagent_start("/parent.jsonl", "parent-sid", "child-sid"));
        engine.apply_event(&HookEvent::StopFailure(StopFailurePayload {
            session_id: "child-sid".into(),
            transcript_path: "/claude.jsonl".into(),
            cwd: "/project".into(),
            error_type: Some("rate_limit".into()),
            parent_pid: None,
            source_tool: SourceTool::ClaudeCode,
        }));
        assert!(engine.agents.contains_key("/claude.jsonl"), "Claude non affecté par la garde Grok");
        assert_eq!(engine.agents["/claude.jsonl"].state, AgentState::Error);
    }

    // ── Auto-cleanup disconnected ─────────────────────────────────────────────

    #[test]
    fn auto_cleanup_supprime_agent_a_la_deconnexion() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        engine.settings.auto_cleanup_disconnected = true;
        engine.set_disconnected("/t.jsonl");
        assert!(!engine.agents.contains_key("/t.jsonl"));
    }

    #[test]
    fn sans_auto_cleanup_agent_reste_disconnected() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        // auto_cleanup_disconnected est false par défaut
        engine.set_disconnected("/t.jsonl");
        assert!(engine.agents.contains_key("/t.jsonl"));
        assert_eq!(engine.agents["/t.jsonl"].state, AgentState::Disconnected);
    }

    #[test]
    fn dismiss_refuse_agent_non_disconnected_et_supprime_si_disconnected() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&make_session_start("/t.jsonl"));
        // Agent Idle — try_dismiss doit refuser
        assert!(!engine.try_dismiss("/t.jsonl"));
        assert!(engine.agents.contains_key("/t.jsonl"));
        // Agent Disconnected — try_dismiss doit supprimer
        engine.set_disconnected("/t.jsonl");
        assert!(engine.try_dismiss("/t.jsonl"));
        assert!(!engine.agents.contains_key("/t.jsonl"));
    }

    // ── hide_notch_if_empty ───────────────────────────────────────────────────

    #[test]
    fn hide_notch_if_empty_false_par_defaut() {
        assert!(!HookSettings::default().hide_notch_if_empty);
    }

    #[test]
    fn set_settings_applique_hide_notch_if_empty() {
        let mut engine = AgentStateEngine::new();
        engine.set_settings(HookSettings { hide_notch_if_empty: true, ..Default::default() });
        assert!(engine.settings.hide_notch_if_empty);
    }

    // ── source_tool propagation ───────────────────────────────────────────────

    #[test]
    fn source_tool_peuple_depuis_session_start_codex() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Codex,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source_tool, SourceTool::Codex);
    }

    #[test]
    fn source_tool_peuple_depuis_session_start_claude_code() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::ClaudeCode,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source_tool, SourceTool::ClaudeCode);
    }

    #[test]
    fn source_tool_unknown_si_absent() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source_tool, SourceTool::Unknown);
    }

    #[test]
    fn source_tool_preserve_sur_events_suivants() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::SessionStart(SessionStartPayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            source: None,
            terminal_source: None,
            model: None,
            parent_pid: None,
            source_tool: SourceTool::Codex,
        }));
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            parent_pid: None,
            source_tool: SourceTool::Unknown,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source_tool, SourceTool::Codex);
    }

    #[test]
    fn source_tool_upsert_depuis_pre_tool_use() {
        let mut engine = AgentStateEngine::new();
        engine.apply_event(&HookEvent::PreToolUse(PreToolUsePayload {
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/project".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "codex build"}),
            parent_pid: None,
            source_tool: SourceTool::Codex,
        }));
        assert_eq!(engine.agents["/t.jsonl"].source_tool, SourceTool::Codex);
    }
}
