use std::sync::{Arc, Mutex};
#[cfg(debug_assertions)]
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::command;
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tauri::State;
use tokio::task::JoinHandle;
#[cfg(debug_assertions)]
use crate::agent::state::{AgentSnapshot, AgentState};
use crate::agent::state_engine::AgentStateEngine;
use crate::agent::state::AgentMessage;
use crate::config;
use crate::pipe::manager::PendingApprovals;
use crate::pipe::protocol::{ALLOW_RESPONSE, DENY_RESPONSE};
use crate::SETTINGS_STORE;

#[cfg(debug_assertions)]
#[command]
pub async fn emit_test_snapshot(
    app: AppHandle,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;

    let snapshot = AgentSnapshot {
        transcript_path: "/test/vigil-pipeline-test.jsonl".to_string(),
        session_id: Some("test-session-1.3".to_string()),
        name: "vigil-test".to_string(),
        state: AgentState::Working,
        waiting_since: None,
        last_activity: now,
        last_activity_label: Some("Testing pipeline IPC".to_string()),
        terminal_pid: None,
        source: "Terminal".to_string(),
        permission_request: None,
        interrupt_pending: false,
        model: None,
        cwd: None,
        subagents: vec![],
        custom_name: None,
        description: None,
        tags: vec![],
        pinned: false,
        source_tool: crate::agent::state::SourceTool::Unknown,
    };

    let mut eng = engine.lock().map_err(|e| e.to_string())?;
    eng.agents.insert(snapshot.transcript_path.clone(), snapshot);
    eng.emit_snapshot(&app).map_err(|e| e.to_string())
}


#[command]
pub async fn approve_agent(
    id: String,
    approvals: State<'_, Mutex<PendingApprovals>>,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    {
        let mut pa = approvals.lock().map_err(|e| e.to_string())?;
        if !pa.resolve(&id, ALLOW_RESPONSE.to_string()) {
            return Err("approve_failed".to_string());
        }
    }
    // Story 8.3 — le message déjà reçu au téléphone ne doit plus porter de boutons. Strictement
    // fire-and-forget : approuver depuis la Vue Détail ne peut PAS échouer parce que le réseau
    // est tombé.
    notify.request_resolved(&id, RequestOutcome::ApprovedAtDesk);
    Ok(())
}

#[command]
pub async fn deny_agent(
    id: String,
    approvals: State<'_, Mutex<PendingApprovals>>,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    {
        let mut pa = approvals.lock().map_err(|e| e.to_string())?;
        if !pa.resolve(&id, DENY_RESPONSE.to_string()) {
            return Err("deny_failed".to_string());
        }
    }
    notify.request_resolved(&id, RequestOutcome::DeniedAtDesk);
    Ok(())
}

// response_json : JSON complet hookSpecificOutput construit par le frontend
#[command]
pub async fn send_message(
    id: String,
    response_json: String,
    approvals: State<'_, Mutex<PendingApprovals>>,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(&response_json)
        .map_err(|_| "send_message_failed".to_string())?;
    {
        let mut pa = approvals.lock().map_err(|e| e.to_string())?;
        if !pa.resolve(&id, response_json) {
            return Err("send_message_failed".to_string());
        }
    }
    // Story 8.4 — répondre à une question depuis la Vue Détail doit retirer le clavier mobile
    // (approve/deny le faisaient déjà en 8.3 ; `send_message` était le trou).
    notify.request_resolved(&id, RequestOutcome::AnsweredAtDesk);
    Ok(())
}

#[command]
pub async fn focus_terminal(pid: u32) -> Result<(), String> {
    focus_terminal_impl(pid)
}

#[cfg(target_os = "windows")]
fn focus_terminal_impl(target_pid: u32) -> Result<(), String> {
    use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
        PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    struct FindState { target_pid: u32, result: HWND }

    unsafe extern "system" fn find_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let s = &mut *(lparam.0 as *mut FindState);
        let mut wpid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
        if wpid == s.target_pid && IsWindowVisible(hwnd).as_bool() {
            s.result = hwnd;
            BOOL(0)
        } else {
            BOOL(1)
        }
    }

    fn visible_window(pid: u32) -> Option<HWND> {
        let mut state = FindState { target_pid: pid, result: HWND(std::ptr::null_mut()) };
        unsafe {
            let _ = EnumWindows(Some(find_callback), LPARAM(&mut state as *mut FindState as isize));
        }
        if !state.result.0.is_null() { Some(state.result) } else { None }
    }

    fn parent_pid(pid: u32) -> Option<u32> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = None;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    if entry.th32ProcessID == pid {
                        found = Some(entry.th32ParentProcessID);
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            found
        }
    }

    let mut pid = target_pid;
    for _ in 0..6 {
        if let Some(hwnd) = visible_window(pid) {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            return Ok(());
        }
        match parent_pid(pid) {
            Some(p) if p > 4 && p != pid => pid = p,
            _ => break,
        }
    }

    Err("focus_terminal_not_found".to_string())
}

#[cfg(not(target_os = "windows"))]
fn focus_terminal_impl(_pid: u32) -> Result<(), String> {
    Ok(())
}

#[command]
pub async fn dismiss_agent(
    id: String,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut eng = engine.lock().map_err(|e| e.to_string())?;
    if eng.try_dismiss(&id) {
        eng.emit_snapshot(&app).map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Préconditions d'`interrupt_agent`, isolées ici pour être testables (la commande elle-même exige
/// `State`/`AppHandle`). Évaluées AVANT toute écriture de flag ou mutation d'état :
/// agent connu et `Working` (4.5), puis outil dont le bridge consomme réellement le flag (6.33).
fn check_interrupt_allowed(agent: Option<&crate::agent::state::AgentSnapshot>) -> Result<(), String> {
    let agent = agent.ok_or("interrupt_failed")?;
    if !matches!(agent.state, crate::agent::state::AgentState::Working) {
        return Err("interrupt_failed".to_string());
    }
    if !crate::flags::interrupt_supported(&agent.source_tool) {
        return Err("interrupt_unsupported".to_string());
    }
    Ok(())
}

#[command]
pub async fn interrupt_agent(
    id: String,
    reason: Option<String>,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
    app: AppHandle,
) -> Result<(), String> {
    let path = crate::flags::interrupt_flag_path(&id);
    let content = reason.unwrap_or_default();
    let mut eng = engine.lock().map_err(|e| e.to_string())?;
    check_interrupt_allowed(eng.agents.get(&id))?;
    eng.agents.get_mut(&id).unwrap().interrupt_pending = true;
    if std::fs::write(&path, content.as_bytes()).is_err() {
        eng.agents.get_mut(&id).unwrap().interrupt_pending = false;
        return Err("interrupt_failed".to_string());
    }
    eng.emit_snapshot(&app).map_err(|e| e.to_string())
}

#[command]
pub async fn cancel_interrupt(
    id: String,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
    app: AppHandle,
) -> Result<(), String> {
    let _ = std::fs::remove_file(crate::flags::interrupt_flag_path(&id));
    let mut eng = engine.lock().map_err(|e| e.to_string())?;
    if let Some(agent) = eng.agents.get_mut(&id) {
        agent.interrupt_pending = false;
    }
    eng.emit_snapshot(&app).map_err(|e| e.to_string())
}


#[command]
pub async fn get_conversation(id: String, limit: u32, tool: String) -> Result<Vec<AgentMessage>, String> {
    let messages = crate::jsonl::reader::JsonlReader::read_tail(&id, limit as usize, &tool);
    Ok(messages)
}

/// Lit **on-demand** le modèle d'un agent VIVANT dont l'`AgentSnapshot.model` est `undefined` (aucune
/// injection au `session_start`). Le badge ne s'affiche QUE dans `DetailHeader` → une lecture backend
/// à l'ouverture de la Vue Détail suffit. Dispatch par outil :
/// - `"grok"` (6.29) : dérive le dossier de session (parent d'`updates.jsonl`) et lit
///   `summary.json`.`current_model_id` via le helper réutilisé `read_grok_summary`.
/// - `"kimi"` (9.4) : lit le dernier `llm.request`.`model` du `wire.jsonl` (= le `transcript_path`).
/// - autres outils : portent déjà `model` dans `AgentSnapshot` → `None`.
/// Tout échec (chemin sans parent, fichier/champ absent) dégrade en `None`.
#[command]
pub async fn get_agent_model(transcript_path: String, tool: String) -> Result<Option<String>, String> {
    match tool.as_str() {
        "grok" => {
            let Some(session_dir) = std::path::Path::new(&transcript_path).parent() else {
                return Ok(None);
            };
            Ok(crate::history::reader::read_grok_summary(session_dir).model)
        }
        "kimi" => Ok(crate::jsonl::reader::read_kimi_model(&transcript_path)),
        _ => Ok(None),
    }
}

#[command]
pub async fn get_agents_snapshot(
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
    app: AppHandle,
) -> Result<Vec<crate::agent::state::AgentSnapshot>, String> {
    let raw: Vec<crate::agent::state::AgentSnapshot> = {
        let eng = engine.lock().map_err(|e| e.to_string())?;
        eng.agents.values().cloned().collect()
    };
    Ok(crate::session_meta::enrich_snapshots(&app, raw))
}

#[command]
pub async fn get_history_grouped(
    tool: String,
    app: AppHandle,
) -> Result<Vec<crate::history::reader::ProjectGroup>, String> {
    let groups = crate::history::reader::HistoryReader::get_all_grouped(&tool);
    Ok(crate::session_meta::enrich_groups(&app, groups))
}

#[command]
pub async fn show_history_window(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("panel") {
        let _ = win.emit("vigil:navigate", serde_json::json!({ "page": "history" }));
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[command]
pub async fn resume_session(
    transcript_path: String,
    cwd: String,
    tool: Option<String>,
    session_id: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let preferred = app.store(SETTINGS_STORE).ok()
        .and_then(|s| s.get("preferredTerminal"))
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "wt.exe".to_string());

    let terminal = resolve_terminal(&preferred);
    // `tool` absent ⇒ Claude (rétro-compat) ; chemin Claude strictement inchangé.
    let tool = tool.as_deref().unwrap_or("claude_code");
    launch_resume(&terminal, &cwd, tool, &transcript_path, session_id.as_deref())
        .map_err(|_| "resume_failed".to_string())
}

fn find_in_path(exe: &str) -> bool {
    std::process::Command::new("where")
        .arg(exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn resolve_terminal(preferred: &str) -> String {
    if !preferred.is_empty() && find_in_path(preferred) {
        return preferred.to_string();
    }
    for candidate in &["wt.exe", "powershell.exe"] {
        if find_in_path(candidate) {
            return candidate.to_string();
        }
    }
    "cmd.exe".to_string()
}

/// Commande de reprise propre à l'outil, avec le style de quoting passé en paramètre.
/// - Claude : `claude --resume <transcript_path>` (reprend par CHEMIN de fichier) — INCHANGÉ.
/// - Codex : `codex resume <session_id>` (reprend par IDENTIFIANT de session, cf. spike 6.11).
///   Le session_id est lu depuis le contenu d'un `.jsonl` → on le passe par `quote` (défense en
///   profondeur, jamais d'interpolation brute dans le shell). Sans session_id exploitable, on NE
///   retombe PAS sur `claude --resume` (mauvais outil sur un transcript Codex) : on renvoie `None`,
///   et l'appelant remonte une erreur explicite (cas garanti improbable par le spike).
fn resume_command(
    tool: &str,
    transcript_path: &str,
    session_id: Option<&str>,
    quote: impl Fn(&str) -> String,
) -> Option<String> {
    match tool {
        "codex" => session_id
            .filter(|s| !s.is_empty())
            .map(|sid| format!("codex resume {}", quote(sid))),
        // Antigravity — reprise par identifiant (miroir du bras Codex) : `agy --conversation <id>`.
        // Le conversationId (= session_id) passe par `quote` (jamais d'interpolation shell brute).
        // Sans id exploitable → None → erreur explicite (pas de commande hasardeuse, garde-fou 6.11).
        "antigravity" => session_id
            .filter(|s| !s.is_empty())
            .map(|sid| format!("agy --conversation {}", quote(sid))),
        // Grok Build — reprise par identifiant (miroir Codex/Antigravity) : `grok --resume <id>`
        // (syntaxe validée au spike T0 : `grok --help` → `-r, --resume [<SESSION_ID>]`). Le
        // sessionId passe par `quote` (jamais d'interpolation shell brute). Sans id → None → erreur
        // explicite (garde-fou 6.11).
        "grok" => session_id
            .filter(|s| !s.is_empty())
            .map(|sid| format!("grok --resume {}", quote(sid))),
        // Kimi CLI — reprise par identifiant (miroir Codex/Antigravity/Grok) : `kimi --session <id>`
        // (syntaxe validée au spike T0 9.4 : `kimi --help` sur le binaire 0.28.1 → `-S, --session
        // [id]` = « With ID: resume that session »). Le `--resume` de 9.2 était un `None` DÉLIBÉRÉ
        // (pas d'invention de syntaxe) ; le spike a confirmé la forme non interactive. Le sessionId
        // (= `session_<uuid>`, le nom de dossier / la clé de l'index) passe par `quote` (jamais
        // d'interpolation shell brute). Sans id exploitable → None → `missing_resume_target` (pas de
        // commande hasardeuse, garde-fou 6.11).
        "kimi" => session_id
            .filter(|s| !s.is_empty())
            .map(|sid| format!("kimi --session {}", quote(sid))),
        _ => Some(format!("claude --resume {}", quote(transcript_path))),
    }
}

fn launch_resume(
    terminal: &str,
    cwd: &str,
    tool: &str,
    transcript_path: &str,
    session_id: Option<&str>,
) -> std::io::Result<()> {
    // Onglet Codex sans session_id exploitable → `resume_command` renvoie None : on remonte une
    // erreur explicite plutôt que de lancer une commande hasardeuse (cf. arbitrage review 6.11).
    let missing_id = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing_resume_target");
    if terminal.contains("wt") {
        let cmd = resume_command(tool, transcript_path, session_id, |s| format!("\"{}\"", s))
            .ok_or_else(missing_id)?;
        std::process::Command::new("wt.exe")
            .args(["-d", cwd, "--", "cmd", "/k", &cmd])
            .spawn()?;
    } else if terminal.contains("powershell") {
        let cmd = resume_command(tool, transcript_path, session_id,
            |s| format!("'{}'", s.replace('\'', "''")))
            .ok_or_else(missing_id)?;
        std::process::Command::new("powershell.exe")
            .args(["-NoExit", "-Command",
                &format!("Set-Location '{}'; {}", cwd.replace('\'', "''"), cmd)])
            .spawn()?;
    } else {
        let cmd = resume_command(tool, transcript_path, session_id, |s| format!("\"{}\"", s))
            .ok_or_else(missing_id)?;
        std::process::Command::new("cmd.exe")
            .args(["/k", &format!("cd /d \"{}\" && {}", cwd, cmd)])
            .spawn()?;
    }
    Ok(())
}

use tauri_plugin_store::StoreExt;
use crate::permissions::{
    tool_name_to_key, AgentPermissionOverrides, ToolPermissionAction, ToolPermissionKey,
};
use crate::agent::state::SourceTool;

#[command]
pub async fn get_agent_rules(
    transcript_path: String,
    app: AppHandle,
) -> Result<AgentPermissionOverrides, String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let key = format!("agentRules:{}", transcript_path);
    let overrides: AgentPermissionOverrides = store
        .get(&key)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    Ok(overrides)
}

#[command]
pub async fn set_agent_rule(
    transcript_path: String,
    key: ToolPermissionKey,
    action: ToolPermissionAction,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = format!("agentRules:{}", transcript_path);
    let mut overrides: AgentPermissionOverrides = store
        .get(&store_key)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    overrides.insert(key, action);
    store.set(store_key, serde_json::to_value(overrides).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())
}

#[command]
pub async fn set_all_agent_rules(
    transcript_path: String,
    action: ToolPermissionAction,
    app: AppHandle,
) -> Result<(), String> {
    use crate::permissions::ToolPermissionKey::*;
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = format!("agentRules:{}", transcript_path);
    let overrides: AgentPermissionOverrides =
        [Bash, EditWrite, Read, WebFetch, ReadOnly, Mcp, Subagent, Other]
            .into_iter()
            .map(|k| (k, action))
            .collect();
    store.set(store_key, serde_json::to_value(overrides).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())
}

/// Story 6.35 — « Always approve » (PORTÉE AGENT, PAS globale). Approuve la demande courante ET
/// passe la catégorie concernée en `Approve` pour CET agent (règle par-agent `set_agent_rule`).
/// La catégorie est dérivée côté Rust via le MÊME `tool_name_to_key` (source unique de vérité)
/// depuis le `source_tool`+`tool_name` que le backend a lui-même produits dans le snapshot.
/// Ne touche JAMAIS les réglages globaux (`toolPermissions`) — pour ça, la fenêtre Settings.
#[command]
pub async fn always_approve_agent(
    transcript_path: String,
    source_tool: SourceTool,
    tool_name: String,
    approvals: State<'_, Mutex<PendingApprovals>>,
    notify: State<'_, Arc<NotifyManager>>,
    app: AppHandle,
) -> Result<(), String> {
    // 1. Dériver la catégorie (même fonction que l'auto-décision manager.rs).
    let key = tool_name_to_key(source_tool, &tool_name);

    // 2. Approuver la demande courante D'ABORD (débloque vigil-grok bypass immédiatement).
    //    Story 6.35 (review) — atomicité : si `resolve` échoue (demande déjà résolue/périmée),
    //    on retourne AVANT d'écrire la règle par-agent → pas de changement de politique silencieux
    //    derrière un `approve_failed` affiché à l'utilisateur. Le lock est relâché (portée) avant
    //    l'I/O du store.
    {
        let mut pa = approvals.lock().map_err(|e| e.to_string())?;
        if !pa.resolve(&transcript_path, ALLOW_RESPONSE.to_string()) {
            return Err("approve_failed".to_string());
        }
    }
    // Story 8.3 — désarmement du message mobile (fire-and-forget), APRÈS la résolution réussie et
    // AVANT l'écriture de la règle : il ne peut ni échouer, ni retarder l'une ou l'autre.
    notify.request_resolved(&transcript_path, RequestOutcome::ApprovedAtDesk);

    // 3. Écrire la règle par-agent Approve (miroir exact de set_agent_rule) : les suivantes de la
    //    catégorie seront auto-approuvées par resolve_permission sans overlay.
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = format!("agentRules:{}", transcript_path);
    let mut overrides: AgentPermissionOverrides = store
        .get(&store_key)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    overrides.insert(key, ToolPermissionAction::Approve);
    store.set(store_key, serde_json::to_value(overrides).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())?;

    Ok(())
}

#[command]
pub async fn reset_agent_rules(
    transcript_path: String,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = format!("agentRules:{}", transcript_path);
    store.delete(store_key);
    store.save().map_err(|e| e.to_string())
}

use crate::session_meta::{SessionMetadata, session_meta_key, validate as validate_session_metadata};

#[command]
pub async fn get_session_metadata(
    transcript_path: String,
    app: AppHandle,
) -> Result<Option<SessionMetadata>, String> {
    if transcript_path.is_empty() {
        return Err("transcript_path_empty".to_string());
    }
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let key = session_meta_key(&transcript_path);
    let meta = store
        .get(&key)
        .and_then(|v| serde_json::from_value::<SessionMetadata>(v).ok());
    Ok(meta)
}

#[command]
pub async fn set_session_metadata(
    transcript_path: String,
    metadata: SessionMetadata,
    app: AppHandle,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
) -> Result<(), String> {
    if transcript_path.is_empty() {
        return Err("transcript_path_empty".to_string());
    }
    validate_session_metadata(&metadata)?;
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let key = session_meta_key(&transcript_path);
    store.set(key, serde_json::to_value(&metadata).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())?;

    match engine.lock() {
        Ok(eng) => {
            if let Err(e) = eng.emit_snapshot(&app) {
                eprintln!("[vigil] set_session_metadata: emit_snapshot failed: {e}");
            }
        }
        Err(e) => {
            eprintln!("[vigil] set_session_metadata: engine mutex poisoned: {e}");
        }
    }
    Ok(())
}

pub type ConversationWatcherState = Mutex<Option<JoinHandle<()>>>;

use crate::stats::LocalStats;
use crate::usage::manager::{FetchSource, UsageManager, UsageState};

#[command]
pub async fn get_usage_state(
    tool: Option<String>,
    usage: State<'_, Arc<UsageManager>>,
) -> Result<UsageState, String> {
    // `None`/absent → Claude (contrat byte-identique : Notch/UsageBar appellent sans argument).
    Ok(usage.snapshot(tool.as_deref().unwrap_or("claude_code")))
}

#[command]
pub async fn refresh_usage(
    tool: Option<String>,
    usage: State<'_, Arc<UsageManager>>,
) -> Result<(), String> {
    usage.refresh(tool.as_deref().unwrap_or("claude_code"), FetchSource::Manual);
    Ok(())
}

#[command]
pub async fn set_usage_tracking_enabled(
    enabled: bool,
    usage: State<'_, Arc<UsageManager>>,
) -> Result<UsageState, String> {
    Ok(usage.set_enabled(enabled))
}

#[command]
pub fn get_local_stats(tool: String) -> Result<LocalStats, String> {
    Ok(crate::stats::reader::get_local_stats(&tool))
}

/// Stats d'un agent pour le pied de la Vue Détail. Dispatch par `tool` (patron `get_agent_model`,
/// 6.29) : `"grok"` parse le `turn_completed.usage` de son `updates.jsonl` (6.32) ; `"kimi"` parse
/// les `usage.record` de son `wire.jsonl` (9.4) ; tout autre outil suit le chemin Claude,
/// byte-identique (Codex reste sur ce chemin = zéros pré-existants, hors scope).
#[command]
pub fn get_agent_stats(transcript_path: String, tool: String) -> Result<crate::stats::AgentStats, String> {
    match tool.as_str() {
        "grok" => crate::stats::agent::get_agent_stats_grok(&transcript_path),
        "kimi" => crate::stats::agent::get_agent_stats_kimi(&transcript_path),
        _ => crate::stats::agent::get_agent_stats_claude(&transcript_path),
    }
}

#[command]
pub async fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    config::set_autostart(&app, enabled).map_err(|e| e.to_string())
}

#[command]
pub async fn get_tools_status(app: AppHandle) -> Result<config::ToolsStatus, String> {
    Ok(config::detect_installed_tools(&app))
}

#[command]
pub async fn set_tool_enabled(
    app: AppHandle,
    tool: String,
    enabled: bool,
) -> Result<(), String> {
    let result = if enabled {
        config::install_hooks_for_tool(&app, &tool)
    } else {
        config::uninstall_hooks_for_tool(&app, &tool)
    };
    result.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("hook_settings_locked") {
            "hook_settings_locked".to_string()
        } else if msg.contains("hook_settings_unreadable") {
            "hook_settings_unreadable".to_string()
        } else if msg.contains("outil inconnu") {
            "hook_tool_unknown".to_string()
        } else {
            "hook_install_failed".to_string()
        }
    })
}

#[command]
pub async fn get_tool_hook_diff(
    app: AppHandle,
    tool: String,
    action: String,
) -> Result<config::ToolHookDiff, String> {
    config::get_tool_hook_diff(&app, &tool, &action).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("hook_settings_locked") {
            "hook_settings_locked".to_string()
        } else if msg.contains("hook_settings_unreadable") {
            "hook_settings_unreadable".to_string()
        } else if msg.contains("outil inconnu") {
            "hook_tool_unknown".to_string()
        } else {
            "hook_install_failed".to_string()
        }
    })
}

#[command]
pub async fn get_installation_required(app: AppHandle) -> Result<bool, String> {
    Ok(config::check_installation_required_sync(&app))
}

#[command]
pub fn set_hide_notch_if_empty(
    enabled: bool,
    app: AppHandle,
    engine: State<'_, Arc<Mutex<AgentStateEngine>>>,
) -> Result<(), String> {
    let mut eng = engine.lock().map_err(|e| e.to_string())?;
    eng.settings.hide_notch_if_empty = enabled;
    let is_empty = eng.agents.is_empty();
    drop(eng);
    if let Some(notch) = app.get_webview_window("notch") {
        if enabled && is_empty {
            let _ = notch.hide();
        } else {
            let _ = notch.show();
        }
    }
    Ok(())
}

#[command]
pub async fn start_conversation_watch(
    app: AppHandle,
    path: String,
    watcher: State<'_, ConversationWatcherState>,
) -> Result<(), String> {
    use std::time::{Duration, SystemTime};
    use tauri::Emitter;

    // Annuler le watcher précédent s'il existe
    {
        let mut guard = watcher.lock().map_err(|e| e.to_string())?;
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    let app_clone = app.clone();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut last_modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        loop {
            interval.tick().await;
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(modified) = meta.modified() {
                    if modified != last_modified {
                        last_modified = modified;
                        let _ = app_clone.emit_to("notch", "conversation:updated", ());
                    }
                }
            }
        }
    });

    let mut guard = watcher.lock().map_err(|e| e.to_string())?;
    *guard = Some(handle);
    Ok(())
}

#[command]
pub async fn stop_conversation_watch(
    watcher: State<'_, ConversationWatcherState>,
) -> Result<(), String> {
    let mut guard = watcher.lock().map_err(|e| e.to_string())?;
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    Ok(())
}

// ── Notifications mobiles (Story 8.1) ────────────────────────────────────────────
//
// ⚠️ Ces commandes sont le SEUL écrivain de la clé `notifSettings` : le front ne passe jamais
// par `saveSettings()` pour elle. Raison : `saveSettings` écrit la valeur d'une clé EN BLOC, donc
// un écrit côté front écraserait l'objet entier et perdrait l'identifiant de destinataire produit
// par l'appairage côté Rust. Un seul écrivain = zéro course.
//
// Aucune de ces commandes ne nomme un canal particulier : tout passe par `ActiveChannel`.

use crate::notify::manager::NotifyManager;
use crate::notify::{
    agent_notifs_key, read_agent_notif_overrides_from, read_notif_settings, secret,
    write_notif_settings, ActiveChannel, AgentNotifOverrides, ChannelIdentity, MobileNotif,
    NotifEvents, NotifKind, NotifSettings, PairedRecipient, RequestOutcome, ALL_NOTIF_KINDS,
};

/// Vue des réglages exposée au front. `has_token` remplace le secret : sa présence suffit à
/// piloter l'assistant d'appairage, et le secret lui-même ne quitte jamais le coffre de l'OS.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifSettingsDto {
    pub enabled: bool,
    pub events: NotifEvents,
    pub chat_id: Option<String>,
    pub bot_username: Option<String>,
    pub has_token: bool,
    pub last_error: Option<String>,
}

fn to_dto(settings: NotifSettings, last_error: Option<String>) -> NotifSettingsDto {
    NotifSettingsDto {
        enabled: settings.enabled,
        events: settings.events,
        chat_id: settings.chat_id,
        bot_username: settings.bot_username,
        has_token: secret::get_token(ActiveChannel::secret_account()).is_some(),
        last_error,
    }
}

#[command]
pub async fn notify_get_settings(
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<NotifSettingsDto, String> {
    Ok(to_dto(read_notif_settings(&app), notify.last_error()))
}

/// N'écrit QUE l'interrupteur maître et les cases d'événements : `chat_id`/`bot_username` sont
/// relus puis réécrits tels quels, donc un enregistrement de réglages ne peut pas casser
/// l'appairage.
#[command]
pub async fn notify_set_settings(
    enabled: bool,
    events: NotifEvents,
    app: AppHandle,
) -> Result<(), String> {
    let mut settings = read_notif_settings(&app);
    settings.enabled = enabled;
    settings.events = events;
    write_notif_settings(&app, &settings)
}

/// Valide le secret candidat **avant** de l'écrire : un jeton invalide n'est jamais stocké.
/// Mémorise au passage le nom du bot (affiché « Connected ✓ @monbot »).
#[command]
pub async fn notify_set_token(
    token: String,
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<ChannelIdentity, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("empty_token".to_string());
    }
    // Refus AVANT toute requête : le secret est interpolé dans le chemin de l'URL, un collage
    // élargi (URL BotFather complète) redirigerait la requête au lieu d'échouer lisiblement.
    if !ActiveChannel::token_is_well_formed(&token) {
        return Err("invalid_token".to_string());
    }
    let identity = ActiveChannel::for_candidate_token(token.clone()).validate().await?;

    // ⚠️ ORDRE : le store d'abord, le coffre ensuite. L'inverse laisse, si l'écriture du store
    // échoue (store verrouillé, disque plein), le jeton du NOUVEAU bot cohabiter avec le
    // destinataire de l'ANCIEN — l'UI affiche « Connected ✓ @ancien » et tout envoi rend
    // `api_error_400`, sans issue autre qu'un Disconnect manuel. En écrivant le store d'abord,
    // un échec du coffre laisse au pire un appairage incomplet, que l'assistant sait reprendre.
    let mut settings = read_notif_settings(&app);
    settings.bot_username = Some(identity.username.clone());
    // Un changement de bot invalide le destinataire précédent : il faut ré-appairer.
    settings.chat_id = None;
    write_notif_settings(&app, &settings)?;
    secret::set_token(ActiveChannel::secret_account(), &token)?;
    notify.clear_error();
    // Même raison que le `chat_id = None` ci-dessus : les `update_id` sont propres à un bot.
    // Garder l'offset de l'ancien filtrerait toutes les updates du nouveau — plus aucun tap ne
    // remonterait, sans erreur affichée, jusqu'au redémarrage du processus (revue 8.5).
    notify.reset_action_cursor();
    Ok(identity)
}

/// Attend que l'utilisateur se manifeste auprès de son bot (~60 s max) et rend le destinataire
/// capté — **sans le mémoriser**. Opération exclusive : l'UI désactive le bouton pendant l'attente.
///
/// ⚠️ La capture n'est PAS une preuve d'identité : le canal ne garantit pas que l'expéditeur est
/// bien l'utilisateur (n'importe qui connaissant le nom du bot peut lui écrire, et la file
/// d'updates couvre 24 h). C'est pourquoi le résultat passe par un écran de confirmation avant
/// d'être persisté par `notify_confirm_pairing`.
///
/// ⚠️ Story 8.3 — exclusion mutuelle **explicite** avec la boucle retour : les deux font un
/// long-poll exclusif sur le même bot, et deux écoutes concurrentes se cassent mutuellement.
/// En pratique on n'appaire jamais pendant qu'un agent attend une permission, mais la garde ne
/// doit pas être *déduite* de cet usage. Elle protège aussi de l'effet de bord inverse : la
/// boucle retour confirme un offset, ce que `pair()` ne consomme exprès jamais.
///
/// La garde est **RAII** (`PairingGuard`) : si le futur est annulé (webview rechargée, fenêtre
/// Settings fermée pendant l'attente du `/start`) ou panique, le drapeau redescend tout seul.
/// Sans ça il restait levé pour la durée du processus et plus aucun tap n'était jamais reçu.
#[command]
pub async fn notify_start_pairing(
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<PairedRecipient, String> {
    let _ = &app;
    let token = secret::get_token(ActiveChannel::secret_account()).ok_or("missing_token")?;
    // Garde tenue jusqu'à la fin de la fonction (y compris sur `?` / cancel / panic).
    let _pairing = notify
        .shared()
        .try_begin_pairing()
        .ok_or_else(|| "busy_receiving".to_string())?;
    ActiveChannel::pair(&token).await
}

/// Mémorise le destinataire APRÈS confirmation visuelle par l'utilisateur.
///
/// Rallume l'interrupteur maître au passage : quelqu'un qui vient de créer un bot, de coller son
/// jeton et de confirmer son destinataire veut manifestement recevoir des notifications. Sans ça,
/// l'appairage se termine sur un système muet — et comme `notify_send_test` contourne
/// volontairement le filtrage, le bouton de test réussit quand même, fabriquant une **fausse
/// confiance** (constaté en conditions réelles le 2026-07-19 : Disconnect puis ré-appairage
/// laissaient `enabled: false`, test OK, aucune notification réelle).
#[command]
pub async fn notify_confirm_pairing(
    chat_id: String,
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    if chat_id.trim().is_empty() {
        return Err("missing_recipient".to_string());
    }
    let mut settings = read_notif_settings(&app);
    settings.chat_id = Some(chat_id);
    settings.enabled = true;
    write_notif_settings(&app, &settings)?;
    notify.clear_error();
    Ok(())
}

/// Bout-en-bout : construit une vraie notification et l'envoie par le vrai chemin.
/// Volontairement NON filtrée par `should_send` — c'est une action explicite de l'utilisateur,
/// elle doit marcher avant même d'avoir activé l'interrupteur maître.
#[command]
pub async fn notify_send_test(
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    let settings = read_notif_settings(&app);
    let channel = ActiveChannel::active(&settings).ok_or("not_paired")?;
    let result = channel
        .send(&MobileNotif {
            kind: NotifKind::Ended,
            agent_label: "Vigil".to_string(),
            tool: crate::agent::state::SourceTool::Unknown,
            summary: Some("Test notification".to_string()),
            // Aucun bouton : un test n'a aucune demande à approuver (Story 8.3).
            request_id: None,
            keyboard: None,
            body_extra: None,
        })
        .await;
    if result.is_ok() {
        notify.clear_error();
    }
    // La référence de message rendue par l'envoi n'a d'intérêt que pour une demande armée.
    result.map(|_| ())
}

/// Retire le secret du coffre de l'OS ET l'appairage du store, et coupe l'interrupteur maître :
/// après ça, plus aucune I/O sortante n'est possible.
#[command]
pub async fn notify_disconnect(
    app: AppHandle,
    notify: State<'_, Arc<NotifyManager>>,
) -> Result<(), String> {
    // ⚠️ ORDRE : couper l'appairage dans le store D'ABORD. Si `delete_token` sortait en premier
    // et échouait (coffre indisponible, stratégie d'entreprise), le store resterait intact —
    // `enabled: true`, destinataire mémorisé — et la promesse « plus aucune I/O sortante » serait
    // fausse précisément dans le cas d'échec. Ici, même si le coffre refuse, plus rien ne part
    // (`ActiveChannel::active` exige un destinataire).
    let mut settings = read_notif_settings(&app);
    settings.enabled = false;
    settings.chat_id = None;
    settings.bot_username = None;
    // Les cases d'événements sont des PRÉFÉRENCES, pas de l'appairage : les réinitialiser ferait
    // silencieusement perdre à l'utilisateur ses réglages (« Ended » décoché, « Error » coché) à
    // chaque déconnexion. AC2 ne demande que le retrait du secret, du destinataire et du bot.
    write_notif_settings(&app, &settings)?;
    secret::delete_token(ActiveChannel::secret_account())?;
    notify.clear_error();
    // Défensif : le prochain `notify_set_token` remettra de toute façon le curseur à zéro, mais
    // laisser derrière soi un offset qui ne décrit plus aucun bot joignable n'a aucun sens.
    notify.reset_action_cursor();
    Ok(())
}

/// Ouvre une étape de l'assistant dans le navigateur. L'URL est construite **côté Rust** : le
/// front ne dispose d'aucun ouvreur d'URL générique.
#[command]
pub async fn notify_open_link(kind: String, app: AppHandle) -> Result<(), String> {
    let settings = read_notif_settings(&app);
    let url = ActiveChannel::link_url(&kind, settings.bot_username.as_deref())
        .ok_or("unknown_link")?;
    open::that(&url).map_err(|e| e.to_string())
}

// ── Surcharges de notification par agent (Story 8.2) ─────────────────────────────
//
// Miroir des commandes `*_agent_rules` ci-dessus, sur la clé de store
// `agentNotifs:{transcript_path}` — même store, même convention, même modèle mental.
//
// Deux écarts par rapport au patron, tous deux issus de la revue 8.2 :
//  1. `clear_agent_notif_rule` n'a PAS de pendant côté permissions. Il le faut ici : le `select`
//     des permissions a un 3ᵉ état (`manual`) qui laisse une porte de sortie visuelle, une case à
//     cocher n'en a que 2 — sans cette commande, recocher une ligne la fige en surcharge `agent`
//     au lieu de la rendre à l'héritage.
//  2. Les lectures passent par `read_agent_notif_overrides_from` (décodage clé par clé) et non par
//     un `from_value` en bloc : une clé illisible ne doit pas effacer les autres surcharges.
//
// Pas d'équivalent d'`always_approve_agent` : il n'a aucun pendant fonctionnel côté
// notifications (il n'y a pas de « demande en cours » à résoudre).

#[command]
pub async fn get_agent_notif_rules(
    transcript_path: String,
    app: AppHandle,
) -> Result<AgentNotifOverrides, String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    Ok(read_agent_notif_overrides_from(store.get(agent_notifs_key(&transcript_path))))
}

/// Lecture-modification-écriture, **jamais** un `set` en bloc : écraser la clé avec un objet
/// partiel a déjà coûté trois régressions à ce codebase (`mascotByTool`, `toolPermissions`,
/// `stateSettings`).
#[command]
pub async fn set_agent_notif_rule(
    transcript_path: String,
    kind: NotifKind,
    enabled: bool,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = agent_notifs_key(&transcript_path);
    let mut overrides = read_agent_notif_overrides_from(store.get(&store_key));
    overrides.insert(kind, enabled);
    store.set(store_key, serde_json::to_value(overrides).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())
}

/// Retire UNE ligne des surcharges : cette ligne redevient héritée du global (badge `global`),
/// sans toucher aux deux autres. C'est le pendant unitaire de `reset_agent_notif_rules`.
///
/// Si la clé de store se retrouve vide, elle est SUPPRIMÉE plutôt que laissée à `{}` : un objet
/// vide et une clé absente valent la même chose à la lecture, autant ne pas laisser de résidu.
#[command]
pub async fn clear_agent_notif_rule(
    transcript_path: String,
    kind: NotifKind,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let store_key = agent_notifs_key(&transcript_path);
    let mut overrides = read_agent_notif_overrides_from(store.get(&store_key));
    overrides.remove(&kind);
    if overrides.is_empty() {
        store.delete(store_key);
    } else {
        store.set(store_key, serde_json::to_value(overrides).map_err(|e| e.to_string())?);
    }
    store.save().map_err(|e| e.to_string())
}

/// Écrase la clé entière : « tout activer / tout couper » est un état complet, pas un ajout.
/// **Data-driven** sur `ALL_NOTIF_KINDS` — un 4ᵉ type d'événement sera couvert sans toucher ici.
#[command]
pub async fn set_all_agent_notif_rules(
    transcript_path: String,
    enabled: bool,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    let overrides: AgentNotifOverrides =
        ALL_NOTIF_KINDS.into_iter().map(|k| (k, enabled)).collect();
    store.set(
        agent_notifs_key(&transcript_path),
        serde_json::to_value(overrides).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())
}

/// SUPPRIME la clé (et non « écrit false partout ») : l'absence de clé est ce qui rétablit
/// l'héritage du réglage global.
#[command]
pub async fn reset_agent_notif_rules(
    transcript_path: String,
    app: AppHandle,
) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    store.delete(agent_notifs_key(&transcript_path));
    store.save().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture : `AgentSnapshot` minimal, `state` et `source_tool` passés en clair (désérialisés
    /// vers leurs variants réels — `rename_all = "snake_case"`, cf. `agent::state`).
    fn snapshot(state: &str, source_tool: &str) -> crate::agent::state::AgentSnapshot {
        serde_json::from_value(serde_json::json!({
            "transcriptPath": "/t.jsonl",
            "name": "t",
            "state": state,
            "lastActivity": 0,
            "source": "terminal",
            "sourceTool": source_tool,
        }))
        .unwrap()
    }

    /// T3 (6.33) : garde de `interrupt_agent`. `check_interrupt_allowed` est la PREMIÈRE chose
    /// qu'exécute la commande, avant toute écriture de flag ou mutation d'`interrupt_pending` —
    /// un `Err` ici garantit donc zéro flag orphelin.
    #[test]
    fn interrupt_refuse_les_outils_qui_ne_consomment_pas_le_flag() {
        for tool in ["codex", "grok", "antigravity", "kimi", "unknown"] {
            assert_eq!(
                check_interrupt_allowed(Some(&snapshot("working", tool))),
                Err("interrupt_unsupported".to_string()),
                "{tool} ne consomme pas le flag → refus sans écriture",
            );
        }
    }

    /// Story 9.4 — la vraie commande de reprise Kimi remplace le `None` délibéré de 9.2 :
    /// `kimi --session <id>` (syntaxe confirmée au spike T0 `kimi --help` 0.28.1 : `-S, --session
    /// [id]`). Reprise PAR IDENTIFIANT (miroir Codex/Antigravity/Grok), `session_id` par `quote`.
    /// Sans id exploitable → None (garde-fou 6.11, jamais `claude --resume` sur un transcript Kimi).
    #[test]
    fn resume_command_kimi_reprend_par_session_id() {
        let quote = |s: &str| format!("\"{}\"", s);
        assert_eq!(
            resume_command("kimi", "C:\\k\\wire.jsonl", Some("session_abc"), quote),
            Some("kimi --session \"session_abc\"".to_string()),
        );
        // Sans session_id → None (pas de commande hasardeuse, jamais de repli Claude).
        assert_eq!(resume_command("kimi", "C:\\k\\wire.jsonl", None, quote), None);
        assert_eq!(resume_command("kimi", "C:\\k\\wire.jsonl", Some(""), quote), None);

        // Non-régression : les 4 autres outils sont INCHANGÉS.
        assert_eq!(
            resume_command("claude_code", "C:\\t.jsonl", None, quote),
            Some("claude --resume \"C:\\t.jsonl\"".to_string()),
        );
        assert_eq!(
            resume_command("codex", "C:\\t.jsonl", Some("sid"), quote),
            Some("codex resume \"sid\"".to_string()),
        );
        assert_eq!(
            resume_command("grok", "C:\\t.jsonl", Some("sid"), quote),
            Some("grok --resume \"sid\"".to_string()),
        );
        assert_eq!(
            resume_command("antigravity", "C:\\t.jsonl", Some("sid"), quote),
            Some("agy --conversation \"sid\"".to_string()),
        );
    }

    #[test]
    fn interrupt_autorise_claude_working() {
        assert_eq!(check_interrupt_allowed(Some(&snapshot("working", "claude_code"))), Ok(()));
    }

    #[test]
    fn interrupt_refuse_hors_working_et_agent_inconnu() {
        // Garde 4.5 inchangée, et évaluée AVANT celle de l'outil (même erreur qu'avant 6.33).
        assert_eq!(
            check_interrupt_allowed(Some(&snapshot("idle", "claude_code"))),
            Err("interrupt_failed".to_string()),
        );
        assert_eq!(
            check_interrupt_allowed(Some(&snapshot("idle", "grok"))),
            Err("interrupt_failed".to_string()),
        );
        assert_eq!(check_interrupt_allowed(None), Err("interrupt_failed".to_string()));
    }

    /// T5 (6.29) : lecture modèle on-demand `get_agent_model`. `"grok"` lit
    /// `summary.json`.`current_model_id` (parent d'`updates.jsonl`) ; `tool != "grok"` → `None`
    /// (les autres outils portent déjà `model` dans `AgentSnapshot`) ; `summary.json` absent → `None`.
    #[tokio::test]
    async fn get_agent_model_dispatch_et_degradation() {
        let dir = std::env::temp_dir().join(format!("vigil_getmodel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let updates = dir.join("updates.jsonl");
        std::fs::write(&updates, "{}\n").unwrap();
        std::fs::write(dir.join("summary.json"), r#"{"current_model_id":"grok-4.5"}"#).unwrap();
        let path = updates.to_str().unwrap().to_string();

        // grok → lit le modèle depuis summary.json
        assert_eq!(
            get_agent_model(path.clone(), "grok".into()).await.unwrap().as_deref(),
            Some("grok-4.5")
        );
        // outil ≠ grok → None (aucune lecture disque)
        assert_eq!(get_agent_model(path.clone(), "codex".into()).await.unwrap(), None);
        // summary.json absent → None (dégradation propre, jamais de panique)
        std::fs::remove_file(dir.join("summary.json")).unwrap();
        assert_eq!(get_agent_model(path, "grok".into()).await.unwrap(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn get_agent_model_chemin_vide_none() {
        // transcript_path vide/sans parent → Ok(None), jamais de panique (AC6).
        assert_eq!(get_agent_model(String::new(), "grok".into()).await.unwrap(), None);
    }

    /// T3 (6.32) : `get_agent_stats` est tool-aware. `"grok"` parse `updates.jsonl` (turn_completed) ;
    /// tout autre outil suit le chemin Claude (byte-identique). Erreur propre si fichier absent.
    #[test]
    fn get_agent_stats_dispatch_grok() {
        let dir = std::env::temp_dir().join(format!("vigil_getstats_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let updates = dir.join("updates.jsonl");
        std::fs::write(&updates, "{\"timestamp\":1783943155,\"params\":{\"update\":{\"sessionUpdate\":\"turn_completed\",\"usage\":{\"inputTokens\":1000,\"outputTokens\":100,\"cachedReadTokens\":600}}}}\n").unwrap();
        let path = updates.to_str().unwrap().to_string();

        let stats = get_agent_stats(path, "grok".into()).unwrap();
        assert_eq!(stats.input_tokens, 400, "1000 - 600 caché");
        assert_eq!(stats.output_tokens, 100);
        assert_eq!(stats.cache_tokens, 600);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
