use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub transcript_path: String,
    pub cwd: Option<String>,
    pub started_at: u64,
    pub last_active_at: u64,
    pub model: Option<String>,
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGroup {
    pub project_name: String,
    pub project_path: String,
    pub last_activity: u64,
    pub session_count: usize,
    pub sessions: Vec<SessionSummary>,
}

pub struct HistoryReader;

fn claude_projects_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| PathBuf::from(home).join(".claude").join("projects"))
}

/// Répertoire des sessions Codex (`~/.codex/sessions`). Même modèle USERPROFILE/HOME que
/// `claude_projects_dir`. Re-déclaré ici plutôt que partagé avec `stats::reader` pour ne pas
/// coupler les modules (convention du projet, cf. `claude_projects_dir` déjà dupliqué).
fn codex_sessions_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| PathBuf::from(home).join(".codex").join("sessions"))
}

/// Racine des conversations Antigravity (`~/.gemini/antigravity-cli/brain`). Un sous-dossier par
/// conversation (nom = `conversationId`). Même modèle USERPROFILE/HOME que `claude_projects_dir`/
/// `codex_sessions_dir`, re-déclaré localement (convention du projet, ne PAS coupler les modules).
fn antigravity_brain_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| {
            PathBuf::from(home)
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
        })
}

/// Log JSONL de l'historique Antigravity (`~/.gemini/antigravity-cli/history.jsonl`) : source du
/// `workspace` (cwd), absent des transcripts.
fn antigravity_history_file() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| {
            PathBuf::from(home)
                .join(".gemini")
                .join("antigravity-cli")
                .join("history.jsonl")
        })
}

fn scan_jsonl_files(dir: &Path, results: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Un seul niveau de descente — on collecte les .jsonl du sous-dossier sans récursion
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub_entry in sub_entries.flatten() {
                    let sub_path = sub_entry.path();
                    if sub_path.is_file() && sub_path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        results.push(sub_path);
                    }
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            results.push(path);
        }
    }
}

/// Scan récursif borné pour l'arborescence Codex `~/.codex/sessions/YYYY/MM/DD/*.jsonl`.
/// `depth`/`max_depth` bornent la descente (profondeur 4 = year/month/day/file suffit).
/// Distinct de `scan_jsonl_files` (1 niveau, chemin Claude inchangé) et volontairement NON
/// partagé avec `stats::reader::scan_jsonl_files_recursive` pour ne pas coupler les modules.
fn scan_jsonl_files_recursive(dir: &Path, results: &mut Vec<PathBuf>, depth: u32, max_depth: u32) {
    if depth > max_depth { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_jsonl_files_recursive(&path, results, depth + 1, max_depth);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            results.push(path);
        }
    }
}

fn mtime_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

fn read_head(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else { return vec![] };
    BufReader::new(file)
        .lines()
        .take(max_lines)
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .collect()
}

fn parse_session(path: &Path) -> Option<SessionSummary> {
    let lines = read_head(path, 20);
    if lines.is_empty() {
        return None;
    }

    let transcript_path = path.to_string_lossy().into_owned();

    // started_at = timestamp de la première ligne parseable
    let started_at = lines
        .iter()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let ts = v.get("timestamp")?.as_str()?;
            let ms = iso8601_to_unix_ms(ts);
            if ms > 0 { Some(ms) } else { None }
        })
        .unwrap_or(0);

    // Extraire session_id, cwd, model depuis la ligne "type":"system"
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut first_user_message: Option<String> = None;

    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };

        let type_str = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // cwd et sessionId sont des champs top-level sur toutes les lignes
        if session_id.is_none() {
            session_id = v.get("sessionId").and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }

        // model est dans message.model sur les lignes assistant
        if model.is_none() && type_str == "assistant" {
            model = v.get("message")
                .and_then(|m| m.get("model"))
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }

        if type_str == "user" && first_user_message.is_none() {
            if let Some(text) = extract_user_text(&v) {
                if !text.is_empty() {
                    first_user_message = Some(truncate_60(&clean_user_text(&text)));
                }
            }
        }
    }

    Some(SessionSummary {
        session_id,
        transcript_path,
        cwd,
        started_at,
        last_active_at: 0,
        model,
        first_user_message,
        custom_name: None,
        description: None,
        tags: Vec::new(),
        pinned: false,
    })
}

/// Parser de **résumé** Codex CLI (schéma confirmé par spike 2026-06-02 sur sessions réelles).
/// Lit `cwd`/`session_id`/`started_at` depuis `session_meta`, le modèle via `turn_context`,
/// et le 1er message humain via les `response_item`/`message`/`role:"user"` (en excluant les
/// messages synthétiques). `last_active_at` reste à 0 ici (rempli par l'appelant via `mtime_ms`,
/// comme pour Claude). Dégrade proprement : champ absent → `None`/`0`, ligne illisible → skip.
fn parse_codex_summary(path: &Path) -> Option<SessionSummary> {
    // 60 lignes suffisent largement : le 1er message humain et le 1er turn_context arrivent
    // en tête de session (juste après les blocs synthétiques developer/environment_context).
    let lines = read_head(path, 60);
    if lines.is_empty() {
        return None;
    }

    let transcript_path = path.to_string_lossy().into_owned();

    // started_at = 1er timestamp parseable (généralement la ligne session_meta).
    let started_at = lines
        .iter()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let ts = v.get("timestamp")?.as_str()?;
            let ms = iso8601_to_unix_ms(ts);
            if ms > 0 { Some(ms) } else { None }
        })
        .unwrap_or(0);

    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut first_user_message: Option<String> = None;

    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let type_str = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let payload = v.get("payload");

        match type_str {
            // Métadonnées de session : id + cwd (1ère ligne du rollout).
            "session_meta" => {
                if session_id.is_none() {
                    session_id = payload
                        .and_then(|p| p.get("id"))
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                }
                if cwd.is_none() {
                    cwd = payload
                        .and_then(|p| p.get("cwd"))
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                }
            }
            // Modèle courant : 1ère valeur non vide rencontrée.
            "turn_context" if model.is_none() => {
                model = payload
                    .and_then(|p| p.get("model"))
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            // 1er vrai message humain (hors blocs synthétiques).
            "response_item" if first_user_message.is_none() => {
                if let Some(text) = extract_codex_user_text(payload) {
                    first_user_message = Some(truncate_60(&text));
                }
            }
            _ => {}
        }
    }

    Some(SessionSummary {
        session_id,
        transcript_path,
        cwd,
        started_at,
        last_active_at: 0,
        model,
        first_user_message,
        custom_name: None,
        description: None,
        tags: Vec::new(),
        pinned: false,
    })
}

/// Extrait le texte d'un message utilisateur Codex réel depuis un `payload` `response_item`,
/// en filtrant les messages synthétiques :
/// - `role:"developer"` (instructions de permissions) → ignoré
/// - `role:"user"` dont le texte commence par `<` (`<environment_context>`, `<turn_aborted>`, …)
///   → wrapper synthétique injecté, ignoré
///
/// Renvoie `None` si ce n'est pas un message utilisateur exploitable.
fn extract_codex_user_text(payload: Option<&serde_json::Value>) -> Option<String> {
    let payload = payload?;
    if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    // content = [{"type":"input_text","text":"…"}]
    let content = payload.get("content").and_then(|c| c.as_array())?;
    for block in content {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.starts_with('<') {
                continue; // wrapper synthétique (<environment_context>, <turn_aborted>, …)
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

// ── Antigravity : lecture de l'historique local `agy` (Story 6.20) ────────────────
//
// ⚠️ Le CLI `agy` n'alimente PAS `conversation_summaries.db` (vide en usage CLI — artefact de
// l'IDE Antigravity). La source RÉELLE combine deux fichiers de `~/.gemini/antigravity-cli/`,
// tous deux JSONL (aucune dépendance SQLite) :
//   - `brain/<conversationId>/.system_generated/logs/transcript.jsonl` : liste COMPLÈTE des
//     conversations (un dossier par conversation) + 1er message + dates ;
//   - `history.jsonl` : log `{conversationId, display, timestamp(ms), workspace}` qui fournit le
//     `workspace` (cwd, ABSENT des transcripts) pour le groupement par projet.
// `brain/` est la source complète ; `history.jsonl` ne couvre qu'une partie récente → il ENRICHIT
// le cwd, il ne pilote pas la liste. Dégradation propre partout : dossier/fichier absent → `vec![]`
// / map vide / cwd `None` (groupe « unknown ») ; ligne illisible sautée ; jamais de panique.

/// Résumés Antigravity mappés en `SessionSummary`. Résout les chemins fixes puis délègue à
/// `read_summaries_from` (factorisé pour être testable sur une arborescence d'exemple en tempdir).
fn read_antigravity_summaries() -> Vec<SessionSummary> {
    let Some(brain) = antigravity_brain_dir() else { return vec![] };
    let history = antigravity_history_file();
    read_summaries_from(&brain, history.as_deref())
}

/// Énumère les conversations sous `brain/` et mappe chacune en `SessionSummary`, en enrichissant le
/// `cwd` depuis `history.jsonl` (si fourni). `brain` absent/illisible → `vec![]` (AC5).
fn read_summaries_from(brain: &Path, history: Option<&Path>) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(brain) else { return vec![] };
    let index = history.map(read_history_index_from).unwrap_or_default();

    let mut summaries = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(conversation_id) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };

        // transcript.jsonl (compact) en priorité, repli transcript_full.jsonl.
        let logs = dir.join(".system_generated").join("logs");
        let compact = logs.join("transcript.jsonl");
        let transcript = if compact.is_file() { compact } else { logs.join("transcript_full.jsonl") };
        if !transcript.is_file() {
            continue; // conversation sans transcript → rien à afficher
        }

        let (msg_from_transcript, started_at) = parse_antigravity_transcript(&transcript);
        let last_active_at = mtime_ms(&transcript).unwrap_or(started_at);
        // Aucun `created_at` parseable dans l'en-tête → repli sur le mtime (évite une date 1970).
        let started_at = if started_at == 0 { last_active_at } else { started_at };

        let hist_entry = index.get(&conversation_id);
        // cwd (workspace) vient de history.jsonl uniquement (absent des transcripts).
        let cwd = hist_entry.and_then(|(ws, _)| ws.clone());
        // 1er message : transcript en priorité, repli sur le `display` de history.jsonl.
        let first_user_message = msg_from_transcript
            .or_else(|| hist_entry.and_then(|(_, d)| d.clone()).map(|d| truncate_60(d.trim())));

        // `transcript_path` = chemin EXACT que l'agent vivant Antigravity rapporte
        // (`…/brain/<id>/.system_generated/logs/transcript_full.jsonl`, slashes AVANT — format
        // Antigravity). Indispensable pour que le badge « Active » de History matche l'agent en
        // cours (comme Claude/Codex), et sert aussi de clé metadata stable. Le resume utilise
        // `session_id` (= conversationId), pas ce champ.
        let transcript_path = format!(
            "{}/{}/.system_generated/logs/transcript_full.jsonl",
            brain.to_string_lossy().replace('\\', "/"),
            conversation_id
        );

        summaries.push(SessionSummary {
            session_id: Some(conversation_id.clone()),
            transcript_path,
            cwd,
            started_at,
            last_active_at,
            model: None,
            first_user_message,
            custom_name: None,
            description: None,
            tags: Vec::new(),
            pinned: false,
        });
    }
    summaries
}

/// Index `history.jsonl` : `conversationId` → (`workspace`, `display`), en retenant l'entrée la
/// plus ANCIENNE par conversation (1er prompt). Fournit le cwd et un repli de 1er message.
/// Fichier absent/illisible → map vide ; ligne sans `conversationId` ou illisible → sautée.
fn read_history_index_from(path: &Path) -> std::collections::HashMap<String, (Option<String>, Option<String>)> {
    use std::collections::HashMap;
    let mut earliest_ts: HashMap<String, u64> = HashMap::new();
    let mut index: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();

    let Ok(file) = File::open(path) else { return index };
    for line in BufReader::new(file).lines().filter_map(|l| l.ok()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(cid) = v.get("conversationId").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        // Timestamp absent/non-u64 → `u64::MAX` (= « le plus récent ») pour ne JAMAIS gagner le
        // « plus ancien » : une ligne sans horodatage ne doit pas figer un `workspace` vide.
        let ts = v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(u64::MAX);
        // On garde l'entrée la plus ancienne (1er prompt) par conversation.
        let keep = earliest_ts.get(cid).is_none_or(|&prev| ts < prev);
        if keep {
            earliest_ts.insert(cid.to_string(), ts);
            let workspace = v.get("workspace").and_then(|w| w.as_str())
                .filter(|s| !s.is_empty()).map(String::from);
            let display = v.get("display").and_then(|d| d.as_str())
                .filter(|s| !s.is_empty()).map(String::from);
            index.insert(cid.to_string(), (workspace, display));
        }
    }
    index
}

/// Parse l'en-tête d'un transcript Antigravity (format step-based snake_case, spike 6.16) → (1er
/// message utilisateur tronqué, `started_at` ms). 1er message = ligne `type:"USER_INPUT"`, `content`
/// enveloppé `<USER_REQUEST>…</USER_REQUEST>`. `started_at` = 1er `created_at` ISO8601.
/// Fichier illisible/vide → (`None`, `0`). Ne panique jamais.
fn parse_antigravity_transcript(path: &Path) -> (Option<String>, u64) {
    let lines = read_head(path, 40);
    let mut started_at = 0u64;
    let mut first_user_message: Option<String> = None;

    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if started_at == 0 {
            if let Some(ts) = v.get("created_at").and_then(|t| t.as_str()) {
                let ms = iso8601_to_unix_ms(ts);
                if ms > 0 {
                    started_at = ms;
                }
            }
        }
        if first_user_message.is_none()
            && v.get("type").and_then(|t| t.as_str()) == Some("USER_INPUT")
        {
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                // content = "<USER_REQUEST>\n…\n</USER_REQUEST>…" → texte de la requête utilisateur.
                // Repli si tag non fermé (transcript tronqué/streaming) : retirer le tag ouvrant
                // pour ne pas faire fuir `<USER_REQUEST>` littéral dans le 1er message affiché.
                let text = extract_tag(content, "USER_REQUEST").unwrap_or_else(|| {
                    content.trim().trim_start_matches("<USER_REQUEST>").trim().to_string()
                });
                let text = text.trim();
                if !text.is_empty() {
                    first_user_message = Some(truncate_60(text));
                }
            }
        }
        if started_at != 0 && first_user_message.is_some() {
            break;
        }
    }
    (first_user_message, started_at)
}

// ── Grok Build : lecture de l'historique local (Story 6.28) ───────────────────────
//
// Source = arbre à DEUX niveaux `~/.grok/sessions/<urlencode(cwd)>/<sessionId>/` (spike T0
// 2026-07-12, disque réel). Divergence FAVORABLE vs Antigravity : le `cwd` est ENCODÉ dans le nom
// du dossier de 1er niveau → groupement par projet complet (pas de croisement d'index partiel).
// Par session :
//   - `updates.jsonl`   : événements de run (PAS le message user) ; sa présence marque une session,
//                         son `mtime` donne `last_active_at`, et son chemin backslash natif est la
//                         cible du badge « Active » — DOIT byte-matcher la synthèse `vigil-grok` 6.27.
//   - `summary.json`    : métadonnées (`info.cwd`, `current_model_id`, `created_at`) + `session_summary`
//                         = **le 1er message user littéral** (retour Steven 6.28 : Grok y stocke le
//                         1er prompt ; vérifié identique au 1er `<user_query>` même sur grosse session).
//   - `chat_history.jsonl` : la vraie conversation, MAIS mêle des blocs synthétiques `<user_info>`
//                         (env, sans `synthetic_reason`) et `<user_query>` (vrai prompt) → parsing
//                         fragile réservé à la Vue Détail (6.29), PAS utilisé pour la liste History.
// Dégradation propre partout : `sessions/` illisible → `vec![]` ; dossier sans `updates.jsonl`
// sauté ; `summary.json` absent/corrompu → `model`/`cwd` en repli ; jamais de panique, jamais
// d'écriture (lecture stricte).

/// Racine des sessions Grok (`~/.grok/sessions`). Même modèle USERPROFILE/HOME que
/// `claude_projects_dir`/`codex_sessions_dir`/`antigravity_brain_dir`, re-déclaré localement pour ne
/// PAS coupler les modules (convention du projet).
fn grok_sessions_dir() -> Option<PathBuf> {
    grok_home_str().map(|home| PathBuf::from(home).join(".grok").join("sessions"))
}

/// Home utilisateur en `String` : sert à construire le `transcript_path` byte-exact. La résolution
/// DOIT être IDENTIQUE à celle de l'agent vivant `vigil-grok::grok_home` (main.rs) — `USERPROFILE`
/// puis repli `HOMEDRIVE`+`HOMEPATH` — car le badge « Active » compare le `transcript_path` du reader
/// à celui rapporté par l'agent : deux résolutions différentes (ex. `HOME` ici mais `HOMEDRIVE+HOMEPATH`
/// là quand `USERPROFILE` est absent) casseraient le match par construction (code review 6.28 D1,
/// leçon 6.20 Correctif #1). `None` si aucune variable d'environnement exploitable.
fn grok_home_str() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(format!("{drive}{path}"))
        })
        .filter(|s| !s.is_empty())
}

/// Métadonnées lues depuis `summary.json` (helper PUR, factorisé pour réutilisation par 6.29 qui
/// peuplera `AgentSnapshot.model` on-demand — un seul point de vérité du schéma `summary.json`).
/// Tout champ absent/illisible → `None` (jamais de panique).
/// `pub(crate)` : réutilisé par la commande `get_agent_model` (Story 6.29) pour peupler le badge
/// modèle de l'agent Grok **vivant** on-demand (`current_model_id`). Un seul point de vérité du
/// schéma `summary.json`. Champs `pub(crate)` pour lecture inter-modules (`.model`).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct GrokSummary {
    pub(crate) cwd: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) session_summary: Option<String>,
    /// Story 6.31 — `session_kind` top-level de `summary.json`. `Some("subagent")` = session de
    /// SOUS-AGENT (exclue de la liste History, cf. `read_grok_summaries_from` ; sert aussi au repli
    /// cold-start de la suppression anti-fantôme, `state_engine::is_grok_subagent_on_disk`). Absent
    /// / ≠ "subagent" → `None` (session normale). ✅ Champ CONFIRMÉ sur disque (e2e 2026-07-15 :
    /// 7 sous-agents = `"subagent"`, parents = absent ; mémoire `project_grok_hook_schema`).
    pub(crate) session_kind: Option<String>,
}

/// Lit `<session_dir>/summary.json` (JSON, pas JSONL). `cwd` est NICHÉ sous `info.cwd`, `model` =
/// `current_model_id` top-level (spike T0). Fichier/champ absent → `None`. `pub(crate)` : réutilisé
/// par `get_agent_model` (6.29) — **ne PAS** ré-implémenter un second lecteur de `summary.json`.
pub(crate) fn read_grok_summary(session_dir: &Path) -> GrokSummary {
    let Ok(content) = std::fs::read_to_string(session_dir.join("summary.json")) else {
        return GrokSummary::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return GrokSummary::default();
    };
    let str_field = |val: Option<&serde_json::Value>| {
        val.and_then(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
    };
    GrokSummary {
        cwd: str_field(v.get("info").and_then(|i| i.get("cwd"))),
        model: str_field(v.get("current_model_id")),
        created_at: str_field(v.get("created_at")),
        session_summary: str_field(v.get("session_summary")),
        session_kind: str_field(v.get("session_kind")),
    }
}

/// Décode un nom de dossier `<urlencode(cwd)>` en cwd natif (inverse de `urlencode_grok` de 6.27) :
/// `%XX` → octet, puis UTF-8 lossy. REPLI du cwd quand `summary.json` manque. Séquence `%XX`
/// malformée → le `%` est conservé littéral (dégradation propre). Résultat vide → `None`.
///
/// On lit les deux chiffres hexa via `str::get` (renvoie `Option`) plutôt que par indexation
/// `&encoded[i+1..i+3]` : un `%` suivi d'un caractère UTF-8 multi-octets ferait tomber `i+3` hors
/// frontière de caractère et l'indexation paniquerait (« byte index N is not a char boundary »).
/// `get` renvoie alors `None` → on conserve le `%` littéral et on repart octet par octet (jamais de
/// panique — invariant AC6, code review 6.28 P2).
fn urldecode_grok(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(hex) = encoded.get(i + 1..i + 3) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&out).into_owned();
    if decoded.is_empty() { None } else { Some(decoded) }
}

/// Synthétise le `transcript_path` d'une session Grok, byte-identique à `vigil-grok::grok_transcript_path_in`
/// (6.27) : `<home>\.grok\sessions\<encoded_cwd>\<sessionId>\updates.jsonl`, backslash natif absolu.
/// On réutilise le nom de dossier encodé LU SUR DISQUE (pas de ré-encodage) → correspondance garantie
/// par construction avec ce que l'agent vivant rapporte (badge « Active », leçon 6.20 Correctif #1).
fn grok_transcript_path(home: &str, encoded_cwd: &str, session_id: &str) -> String {
    let home = home.trim_end_matches(['\\', '/']);
    format!(r"{home}\.grok\sessions\{encoded_cwd}\{session_id}\updates.jsonl")
}

/// Résumés Grok mappés en `SessionSummary`. Résout home + `sessions/` puis délègue à
/// `read_grok_summaries_from` (factorisé pour test tempdir).
fn read_grok_summaries() -> Vec<SessionSummary> {
    let Some(home) = grok_home_str() else { return vec![] };
    let sessions_dir = grok_sessions_dir().unwrap_or_else(|| PathBuf::from(&home).join(".grok").join("sessions"));
    read_grok_summaries_from(&sessions_dir, &home)
}

/// Énumère l'arbre à 2 niveaux `<sessions_dir>/<encoded_cwd>/<sessionId>/` et mappe chaque session
/// (dossier contenant `updates.jsonl`) en `SessionSummary`. `sessions_dir` illisible → `vec![]`.
/// `home` sert à construire le `transcript_path` byte-exact (voir `grok_transcript_path`).
fn read_grok_summaries_from(sessions_dir: &Path, home: &str) -> Vec<SessionSummary> {
    let Ok(cwd_dirs) = std::fs::read_dir(sessions_dir) else { return vec![] };

    let mut summaries = Vec::new();
    for cwd_entry in cwd_dirs.flatten() {
        let cwd_dir = cwd_entry.path();
        // Sauter les non-dossiers (ex. `session_search.sqlite` au niveau racine — piège T0).
        if !cwd_dir.is_dir() {
            continue;
        }
        let Some(encoded_cwd) = cwd_dir.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let Ok(session_dirs) = std::fs::read_dir(&cwd_dir) else { continue };

        for session_entry in session_dirs.flatten() {
            let session_dir = session_entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            let updates = session_dir.join("updates.jsonl");
            if !updates.is_file() {
                continue; // session sans updates.jsonl → sautée (rien à ancrer)
            }
            let Some(session_id) = session_dir.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };

            let meta = read_grok_summary(&session_dir);

            // Story 6.31 — exclusion PURE des sous-agents Grok : une session `session_kind ==
            // "subagent"` n'apparaît JAMAIS dans la liste History (ni à la racine, ni nichée sous le
            // parent — décision Steven : sous-agent éphémère, une longue session peut en spawner des
            // dizaines). Les sessions normales (champ absent / ≠ "subagent") restent listées.
            if meta.session_kind.as_deref() == Some("subagent") {
                continue;
            }

            // cwd : summary.json prioritaire (chemin natif exact), repli = décodage du nom de dossier.
            let cwd = meta.cwd.clone().or_else(|| urldecode_grok(&encoded_cwd));

            // last_active_at = mtime d'updates.jsonl (AC2).
            let last_active_at = mtime_ms(&updates).unwrap_or(0);
            // started_at = created_at de summary.json (ISO8601), repli = mtime (évite une date 1970).
            let started_at = meta
                .created_at
                .as_deref()
                .map(iso8601_to_unix_ms)
                .filter(|&ms| ms > 0)
                .unwrap_or(last_active_at);

            // 1er message = `session_summary` de summary.json (= 1er prompt user littéral, retour
            // Steven 6.28). `chat_history.jsonl` (blocs `<user_info>`/`<user_query>`) est réservé à
            // la Vue Détail (6.29). Vide/absent → None (session sans prompt user réel).
            let first_user_message = meta.session_summary.as_deref().map(|s| truncate_60(s.trim()));

            summaries.push(SessionSummary {
                session_id: Some(session_id.clone()),
                transcript_path: grok_transcript_path(home, &encoded_cwd, &session_id),
                cwd,
                started_at,
                last_active_at,
                model: meta.model,
                first_user_message,
                custom_name: None,
                description: None,
                tags: Vec::new(),
                pinned: false,
            });
        }
    }
    summaries
}

fn normalize_cwd(cwd: &str) -> String {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .to_string()
}

fn project_name_from_cwd(cwd: &str) -> String {
    let normalized = normalize_cwd(cwd);
    if normalized.is_empty() {
        return "unknown".to_string();
    }
    normalized
        .rsplit(['/', '\\'])
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn group_summaries_by_project(summaries: Vec<SessionSummary>) -> Vec<ProjectGroup> {
    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<SessionSummary>> = HashMap::new();
    for s in summaries {
        let key = s
            .cwd
            .as_deref()
            .map(normalize_cwd)
            .unwrap_or_default();
        groups.entry(key).or_default().push(s);
    }

    let mut result: Vec<ProjectGroup> = groups
        .into_iter()
        .map(|(key, mut sessions)| {
            let total = sessions.len();
            // Tri par date desc uniquement (pinned est false partout avant enrichissement).
            // Pas de truncate ici — la troncature à 20 doit se faire APRÈS enrichissement
            // pour ne pas perdre une session épinglée ancienne (rang > 20 par date).
            sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
            let last_activity = sessions.first().map(|s| s.last_active_at).unwrap_or(0);
            let project_name = if key.is_empty() {
                "unknown".to_string()
            } else {
                project_name_from_cwd(&key)
            };
            ProjectGroup {
                project_name,
                project_path: key,
                last_activity,
                session_count: total,
                sessions,
            }
        })
        .collect();

    result.sort_by(|a, b| {
        match (a.project_path.is_empty(), b.project_path.is_empty()) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => b.last_activity.cmp(&a.last_activity),
        }
    });

    result
}

/// Tri final (`pinned` desc, puis `last_active_at` desc) et troncature des sessions
/// d'un groupe. À appeler **après** l'enrichissement metadata, car `pinned` provient
/// du store et n'est pas connu à la sortie de `group_summaries_by_project`.
///
/// Garantie : **toutes** les sessions épinglées sont conservées, indépendamment de
/// `max_non_pinned`. La limite ne s'applique qu'aux sessions non épinglées. Un projet
/// avec 30 sessions épinglées affichera donc au moins ces 30 + jusqu'à `max_non_pinned`
/// non épinglées les plus récentes.
pub fn finalize_group_session_ordering(group: &mut ProjectGroup, max_non_pinned: usize) {
    group.sessions.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.last_active_at.cmp(&a.last_active_at))
    });
    let pinned_count = group.sessions.iter().take_while(|s| s.pinned).count();
    group.sessions.truncate(pinned_count + max_non_pinned);
}

impl HistoryReader {
    /// Sessions groupées par projet pour un outil donné (`"claude_code"` | `"codex"` | `"grok"`).
    /// Convention `tool: &str` partagée avec `config::tool_label` / `stats::reader::get_local_stats`.
    /// Seuls le répertoire, le scan et le parser de résumé diffèrent par format ; le groupement,
    /// le tri et l'enrichissement metadata (via l'appelant) sont communs.
    /// Contrairement à Statistics, **aucun filtre mtime** : History montre TOUTES les sessions.
    pub fn get_all_grouped(tool: &str) -> Vec<ProjectGroup> {
        let summaries: Vec<SessionSummary> = match tool {
            // Chemin Claude — STRICTEMENT INCHANGÉ (répertoire, scan 1 niveau, parser type:"user"/"assistant").
            "claude_code" => {
                let Some(dir) = claude_projects_dir() else { return vec![] };
                let mut files = Vec::new();
                scan_jsonl_files(&dir, &mut files);
                files
                    .into_iter()
                    .filter_map(|path| {
                        let mtime = mtime_ms(&path)?;
                        let mut s = parse_session(&path)?;
                        s.last_active_at = mtime;
                        Some(s)
                    })
                    .collect()
            }
            // Codex CLI — arborescence YYYY/MM/DD → scan récursif + parser de résumé propre au format.
            "codex" => {
                let Some(dir) = codex_sessions_dir() else { return vec![] };
                let mut files = Vec::new();
                scan_jsonl_files_recursive(&dir, &mut files, 0, 4);
                files
                    .into_iter()
                    .filter_map(|path| {
                        let mtime = mtime_ms(&path)?;
                        let mut s = parse_codex_summary(&path)?;
                        s.last_active_at = mtime;
                        Some(s)
                    })
                    .collect()
            }
            // Antigravity CLI — historique JSONL local (`brain/` + `history.jsonl`). Le lecteur
            // remplit lui-même `last_active_at` (mtime du transcript) et enrichit le cwd.
            "antigravity" => read_antigravity_summaries(),
            // Grok Build — arbre `~/.grok/sessions/<encoded_cwd>/<sessionId>/` (Story 6.28). Le
            // lecteur remplit lui-même `last_active_at` (mtime updates.jsonl) et le cwd.
            "grok" => read_grok_summaries(),
            // Kimi CLI — index `~/.kimi-code/session_index.jsonl` + `state.json` par session (Story
            // 9.4). Le lecteur remplit lui-même `last_active_at` (updatedAt), le cwd et le
            // `transcript_path` byte-identique à vigil-kimi (`<sessionDir>/agents/main/wire.jsonl`).
            "kimi" => read_kimi_summaries(),
            // Inconnu — pas de parser : liste vide (placeholder géré côté frontend).
            _ => return vec![],
        };

        group_summaries_by_project(summaries)
    }
}

fn extract_user_text(v: &serde_json::Value) -> Option<String> {
    // Format: {"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}
    if let Some(content) = v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    // Fallback: contenu string direct
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

fn clean_user_text(s: &str) -> String {
    // Contenu de type commande : "<command-name>/foo</command-name><command-args>bar</command-args>"
    // → "/foo bar"
    if s.contains("<command-name>") {
        let name = extract_tag(s, "command-name").unwrap_or_default();
        let args = extract_tag(s, "command-args").unwrap_or_default();
        return if args.is_empty() { name } else { format!("{name} {args}") };
    }
    s.to_string()
}

fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)?;
    let content = s[start..start + end].trim();
    if content.is_empty() { None } else { Some(content.to_string()) }
}

fn truncate_60(s: &str) -> String {
    match s.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn iso8601_to_unix_ms(s: &str) -> u64 {
    if !s.is_ascii() || s.len() < 19 {
        return 0;
    }
    let b = s.as_bytes();
    let year: i64 = s[0..4].parse().unwrap_or(1970);
    let month: u32 = s[5..7].parse().unwrap_or(1);
    let day: u32 = s[8..10].parse().unwrap_or(1);
    let hour: u64 = s[11..13].parse().unwrap_or(0);
    let min: u64 = s[14..16].parse().unwrap_or(0);
    let sec: u64 = s[17..19].parse().unwrap_or(0);

    let ms: u64 = if b.len() > 20 && b[19] == b'.' {
        let frac_len = b[20..].iter().position(|c| !c.is_ascii_digit()).unwrap_or(b.len() - 20);
        let frac_str = &s[20..20 + frac_len];
        let frac: u64 = frac_str.parse().unwrap_or(0);
        match frac_str.len() {
            1 => frac * 100,
            2 => frac * 10,
            3 => frac,
            n if n > 3 => frac / 10u64.pow((n - 3) as u32),
            _ => 0,
        }
    } else {
        0
    };

    let days = days_from_epoch(year, month, day);
    (days as u64) * 86_400_000 + hour * 3_600_000 + min * 60_000 + sec * 1_000 + ms
}

fn days_from_epoch(year: i64, month: u32, day: u32) -> i64 {
    fn is_leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
    fn month_days(m: u32, y: i64) -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => if is_leap(y) { 29 } else { 28 },
            _ => 30,
        }
    }
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += month_days(m, year);
    }
    days += day as i64 - 1;
    days
}

// ── Kimi CLI : lecture de l'historique local (Story 9.4) ──────────────────────────
//
// Source = index `~/.kimi-code/session_index.jsonl` (une ligne JSON par session : `sessionId`,
// `sessionDir`, `workDir`) + un `state.json` par dossier de session (spike T0 2026-07-24, disque
// réel). Contrairement à Grok (arbre `<encoded_cwd>/<sessionId>/`), le `cwd` de Kimi n'est PAS
// encodé dans le nom de dossier de 1er niveau (`wd_<hash>` opaque) → l'index est la seule source du
// `sessionDir` exact, et son chemin en **slashs avant** doit être conservé TEL QUEL pour que le
// `transcript_path` synthétisé soit byte-identique à celui que `vigil-kimi` rapporte à chaud (badge
// « Active », leçon 6.20 Correctif #1 / 6.28). On ré-implémente donc localement la lecture d'index
// (l'app ne peut PAS importer la crate `vigil-kimi`), avec la même tolérance que
// `vigil-kimi::find_in_index` : BOM retiré, ligne non-JSON / `sessionDir` absent-nul-vide → `continue`.
//
// Par session (`state.json`) : `title` = 1er message user (→ `custom_name` si `isCustomTitle`),
// `workDir` = cwd, `createdAt`/`updatedAt` = **chaînes ISO8601** (T0 : PAS des ms Unix comme le
// supposait la story), `agents{}` = sous-agents (`type:"sub"`, dossier `agents/<id>/wire.jsonl`).
//
// Anti-fantôme : un `sessionDir` présent dans l'index mais sans dossier réel sur disque (ni
// `state.json` ni `agents/main/wire.jsonl`) est IGNORÉ. Dégradation propre partout : index illisible
// → `vec![]` ; `state.json` absent/corrompu → repli mtime + cwd de l'index ; jamais de panique,
// jamais d'écriture (lecture stricte de `~/.kimi-code/`).

/// Chemin de l'index Kimi (`~/.kimi-code/session_index.jsonl`). Résolution home IDENTIQUE à
/// `vigil-kimi::kimi_home` (USERPROFILE puis repli HOMEDRIVE+HOMEPATH) — car le `transcript_path`
/// est comparé à celui rapporté par l'agent vivant : deux résolutions différentes casseraient le
/// badge « Active » par construction. `None` si aucune variable d'environnement exploitable.
fn kimi_index_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(format!("{drive}{path}"))
        })
        .filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".kimi-code").join("session_index.jsonl"))
}

/// Métadonnées lues depuis `state.json` d'une session Kimi (schéma T0 spike 9.4). Tout champ
/// absent/illisible → `None` (jamais de panique). `title` = 1er message user ; `is_custom_title`
/// distingue un titre personnalisé d'un titre auto ; `work_dir` = cwd ; `created_at`/`updated_at`
/// = chaînes ISO8601.
#[derive(Debug, Default, Clone, PartialEq)]
struct KimiState {
    title: Option<String>,
    is_custom_title: bool,
    work_dir: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

/// Lit `<session_dir>/state.json` (JSON, pas JSONL). `session_dir` est le chemin **tel quel** de
/// l'index (slashs avant) ; `Path::join` accepte des slashs avant sur Windows pour les I/O disque.
/// Fichier/champ absent → défaut (`None`, jamais de panique).
fn read_kimi_state(session_dir: &str) -> KimiState {
    let Ok(content) = std::fs::read_to_string(Path::new(session_dir).join("state.json")) else {
        return KimiState::default();
    };
    // BOM UTF-8 retiré comme pour l'index (`read_kimi_summaries`) : `serde_json` rejette un BOM en
    // tête, ce qui viderait toutes les métadonnées (title/cwd/dates) en silence.
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        return KimiState::default();
    };
    let str_field = |val: Option<&serde_json::Value>| {
        val.and_then(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
    };
    KimiState {
        title: str_field(v.get("title")),
        is_custom_title: v.get("isCustomTitle").and_then(|b| b.as_bool()).unwrap_or(false),
        work_dir: str_field(v.get("workDir")),
        created_at: str_field(v.get("createdAt")),
        updated_at: str_field(v.get("updatedAt")),
    }
}

/// Synthétise le `transcript_path` d'une session Kimi, byte-identique à `vigil-kimi::find_in_index` :
/// `<sessionDir>/agents/main/wire.jsonl`. Le `sessionDir` est utilisé TEL QUEL (slashs avant lus dans
/// l'index) — on retire seulement d'éventuels séparateurs de fin, jamais de re-normalisation en
/// backslash (sinon divergence avec l'agent vivant → badge « Active » mort, leçon 6.20 Correctif #1).
fn kimi_transcript_path(session_dir: &str) -> String {
    let dir = session_dir.trim_end_matches(['\\', '/']);
    format!("{dir}/agents/main/wire.jsonl")
}

/// Résumés Kimi mappés en `SessionSummary`. Résout le chemin de l'index, le lit (BOM retiré) puis
/// délègue à `read_kimi_summaries_from` (factorisé pour test tempdir). Index absent/illisible →
/// `vec![]`.
fn read_kimi_summaries() -> Vec<SessionSummary> {
    let Some(index_path) = kimi_index_path() else { return vec![] };
    let Ok(content) = std::fs::read_to_string(&index_path) else { return vec![] };
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    read_kimi_summaries_from(content)
}

/// Parse le contenu de `session_index.jsonl` et mappe chaque session (dédupliquée par `sessionId`)
/// en `SessionSummary`, en lisant son `state.json` sur disque. `sessionDir` absolu (slashs avant)
/// dans l'index → les I/O visent directement le disque, ce qui rend la fonction testable en tempdir
/// (l'index de test pointe vers des dossiers créés dans le tempdir). Une session dont le dossier est
/// introuvable (ni `state.json` ni `agents/main/wire.jsonl`) est ignorée (anti-fantôme).
fn read_kimi_summaries_from(index_content: &str) -> Vec<SessionSummary> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut summaries = Vec::new();

    for line in index_content.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Ligne non-JSON / tronquée (écriture concurrente) → ignorée (tolérance find_in_index).
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session_id) = entry.get("sessionId").and_then(|s| s.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(session_dir) = entry.get("sessionDir").and_then(|s| s.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        // Dédup par sessionId : l'index est append-only, une reprise réécrit la même session.
        // Parcours INVERSE (`lines().rev()`, plus bas) pour retenir la DERNIÈRE occurrence — byte-
        // identique à `vigil-kimi::find_in_index` qui fait `content.lines().rev()`. Garder la première
        // ferait diverger le `sessionDir` (donc le `transcript_path`) si une reprise l'a corrigé →
        // badge « Active » mort (contrat §3).
        if !seen.insert(session_id.to_string()) {
            continue;
        }

        let transcript_path = kimi_transcript_path(session_dir);

        // Anti-fantôme : un sessionDir d'index sans dossier réel ne doit PAS produire de session.
        // On garde la session si l'un des deux ancrages existe (state.json OU le wire.jsonl).
        let state_exists = Path::new(session_dir).join("state.json").is_file();
        let wire_exists = Path::new(&transcript_path).is_file();
        if !state_exists && !wire_exists {
            continue;
        }

        let state = read_kimi_state(session_dir);

        // last_active_at = updatedAt (ISO8601) ; repli = mtime du wire.jsonl (évite une date 1970).
        let wire_mtime = mtime_ms(Path::new(&transcript_path)).unwrap_or(0);
        let last_active_at = state
            .updated_at
            .as_deref()
            .map(iso8601_to_unix_ms)
            .filter(|&ms| ms > 0)
            .unwrap_or(wire_mtime);
        // started_at = createdAt (ISO8601) ; repli = last_active_at. Clampé à `last_active_at` : quand
        // ce dernier retombe sur `wire_mtime` (updatedAt absent) et qu'un `createdAt` parse plus tard
        // que le mtime (clone/copie ayant réinitialisé le mtime), on éviterait une durée négative.
        let started_at = state
            .created_at
            .as_deref()
            .map(iso8601_to_unix_ms)
            .filter(|&ms| ms > 0)
            .unwrap_or(last_active_at)
            .min(last_active_at);

        // cwd : workDir de state.json prioritaire, repli = workDir de l'index.
        let cwd = state
            .work_dir
            .clone()
            .or_else(|| entry.get("workDir").and_then(|s| s.as_str()).map(String::from))
            .filter(|s| !s.is_empty());

        // 1er message = `title` (= 1er prompt user, T0). `isCustomTitle` → `custom_name` (champ
        // existant, PAS un nouveau champ) quand l'utilisateur a renommé la session.
        let first_user_message = state.title.as_deref().map(|s| truncate_60(s.trim()));
        let custom_name = if state.is_custom_title { state.title.clone() } else { None };

        summaries.push(SessionSummary {
            session_id: Some(session_id.to_string()),
            transcript_path,
            cwd,
            started_at,
            last_active_at,
            model: None,
            first_user_message,
            custom_name,
            description: None,
            tags: Vec::new(),
            pinned: false,
        });
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── extract_tag ───────────────────────────────────────────────────────────

    #[test]
    fn extract_tag_trouve_le_contenu() {
        assert_eq!(extract_tag("<foo>bar</foo>", "foo"), Some("bar".into()));
    }

    #[test]
    fn extract_tag_trim_les_espaces() {
        assert_eq!(extract_tag("<foo>  bar  </foo>", "foo"), Some("bar".into()));
    }

    #[test]
    fn extract_tag_absent_retourne_none() {
        assert_eq!(extract_tag("<other>x</other>", "foo"), None);
    }

    #[test]
    fn extract_tag_vide_retourne_none() {
        assert_eq!(extract_tag("<foo></foo>", "foo"), None);
    }

    #[test]
    fn extract_tag_espaces_seuls_retourne_none() {
        assert_eq!(extract_tag("<foo>   </foo>", "foo"), None);
    }

    #[test]
    fn extract_tag_multiple_tags_cible_le_bon() {
        let s = "<command-name>/foo</command-name><command-args>bar</command-args>";
        assert_eq!(extract_tag(s, "command-name"), Some("/foo".into()));
        assert_eq!(extract_tag(s, "command-args"), Some("bar".into()));
    }

    // ── clean_user_text ───────────────────────────────────────────────────────

    #[test]
    fn clean_commande_avec_args() {
        let s = "<command-message>bmad-code-review</command-message>\n<command-name>/bmad-code-review</command-name>\n<command-args>4.9</command-args>";
        assert_eq!(clean_user_text(s), "/bmad-code-review 4.9");
    }

    #[test]
    fn clean_commande_sans_args() {
        let s = "<command-message>help</command-message>\n<command-name>/help</command-name>\n<command-args></command-args>";
        assert_eq!(clean_user_text(s), "/help");
    }

    #[test]
    fn clean_commande_sans_balise_args() {
        let s = "<command-name>/foo</command-name>";
        assert_eq!(clean_user_text(s), "/foo");
    }

    #[test]
    fn clean_texte_normal_inchange() {
        let s = "Bonjour, peux-tu lire ce fichier ?";
        assert_eq!(clean_user_text(s), s);
    }

    #[test]
    fn clean_texte_vide_inchange() {
        assert_eq!(clean_user_text(""), "");
    }

    fn write_temp_jsonl(name: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        path
    }

    #[test]
    fn test_parse_session_metadata() {
        let path = write_temp_jsonl("vigil_test_meta.jsonl", &[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello world"}]},"timestamp":"2026-05-12T10:00:00.000Z","cwd":"/home/user/project","sessionId":"abc123"}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-7","role":"assistant","content":[]},"timestamp":"2026-05-12T10:00:01.000Z","cwd":"/home/user/project","sessionId":"abc123"}"#,
        ]);

        let summary = parse_session(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.session_id.as_deref(), Some("abc123"));
        assert_eq!(summary.cwd.as_deref(), Some("/home/user/project"));
        assert_eq!(summary.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(summary.first_user_message.as_deref(), Some("Hello world"));
        assert!(summary.started_at > 0);
    }

    #[test]
    fn test_resolve_terminal_fallback() {
        let result = resolve_terminal_for_test("");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_missing_jsonl_skipped() {
        let result = parse_session(Path::new("C:/nonexistent/path/file.jsonl"));
        assert!(result.is_none());
    }

    #[test]
    fn test_corrupted_line_skipped() {
        let path = write_temp_jsonl("vigil_test_corrupt.jsonl", &[
            r#"NOT VALID JSON {{{"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"test"}]},"timestamp":"2026-05-12T10:00:00.000Z","cwd":"/proj","sessionId":"ok"}"#,
        ]);

        let summary = parse_session(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.session_id.as_deref(), Some("ok"));
        assert_eq!(summary.first_user_message.as_deref(), Some("test"));
    }

    fn resolve_terminal_for_test(preferred: &str) -> String {
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

    fn find_in_path(exe: &str) -> bool {
        std::process::Command::new("where")
            .arg(exe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // ── Groupement par projet (Story 4.15) ────────────────────────────────────

    fn make_summary_with(path: &str, cwd: Option<&str>, last_active: u64) -> SessionSummary {
        SessionSummary {
            session_id: None,
            transcript_path: path.into(),
            cwd: cwd.map(String::from),
            started_at: last_active,
            last_active_at: last_active,
            model: None,
            first_user_message: None,
            custom_name: None,
            description: None,
            tags: Vec::new(),
            pinned: false,
        }
    }

    #[test]
    fn project_name_from_cwd_dernier_segment_unix() {
        assert_eq!(project_name_from_cwd("/home/user/vigil"), "vigil");
    }

    #[test]
    fn project_name_from_cwd_dernier_segment_windows() {
        assert_eq!(project_name_from_cwd("C:\\Users\\Dev\\dev\\vigil"), "vigil");
    }

    #[test]
    fn project_name_from_cwd_trailing_slash() {
        assert_eq!(project_name_from_cwd("/home/user/vigil/"), "vigil");
        assert_eq!(project_name_from_cwd("C:\\Users\\Dev\\vigil\\"), "vigil");
    }

    #[test]
    fn project_name_from_cwd_vide_retourne_unknown() {
        assert_eq!(project_name_from_cwd(""), "unknown");
        assert_eq!(project_name_from_cwd("   "), "unknown");
    }

    #[test]
    fn groupes_tries_par_activite_decroissante() {
        let summaries = vec![
            make_summary_with("/a.jsonl", Some("/home/u/projA"), 1000),
            make_summary_with("/b.jsonl", Some("/home/u/projB"), 3000),
            make_summary_with("/c.jsonl", Some("/home/u/projA"), 2000),
        ];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 2);
        // projB est plus récent (3000) → premier
        assert_eq!(groups[0].project_name, "projB");
        assert_eq!(groups[0].last_activity, 3000);
        // projA en second, sa session la plus récente est 2000
        assert_eq!(groups[1].project_name, "projA");
        assert_eq!(groups[1].last_activity, 2000);
    }

    #[test]
    fn sessions_dans_groupe_triees_par_date() {
        let summaries = vec![
            make_summary_with("/a.jsonl", Some("/proj"), 1000),
            make_summary_with("/b.jsonl", Some("/proj"), 3000),
            make_summary_with("/c.jsonl", Some("/proj"), 2000),
        ];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 1);
        let s = &groups[0].sessions;
        assert_eq!(s[0].last_active_at, 3000);
        assert_eq!(s[1].last_active_at, 2000);
        assert_eq!(s[2].last_active_at, 1000);
    }

    #[test]
    fn group_summaries_garde_toutes_les_sessions_sans_truncate() {
        // `group_summaries_by_project` ne tronque PAS — la troncature est faite
        // par `finalize_group_session_ordering` après enrichissement metadata.
        let summaries: Vec<SessionSummary> = (0..25)
            .map(|i| make_summary_with(&format!("/s{}.jsonl", i), Some("/proj"), i as u64))
            .collect();
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].session_count, 25);
        assert_eq!(groups[0].sessions.len(), 25);
        // La plus récente (i=24) est en tête (sort par date desc)
        assert_eq!(groups[0].sessions[0].last_active_at, 24);
        // Le `last_activity` du groupe reste la date max (24)
        assert_eq!(groups[0].last_activity, 24);
    }

    #[test]
    fn finalize_tronque_non_pinned_a_20_quand_aucun_pinned() {
        let sessions: Vec<SessionSummary> = (0..25)
            .map(|i| make_summary_with(&format!("/s{}.jsonl", i), Some("/proj"), i as u64))
            .collect();
        let mut group = ProjectGroup {
            project_name: "proj".into(),
            project_path: "/proj".into(),
            last_activity: 24,
            session_count: 25,
            sessions,
        };
        finalize_group_session_ordering(&mut group, 20);
        assert_eq!(group.sessions.len(), 20);
        // Top = la plus récente (i=24)
        assert_eq!(group.sessions[0].last_active_at, 24);
        // Dernier conservé = i=5 (24, 23, ..., 5 → 20 sessions)
        assert_eq!(group.sessions[19].last_active_at, 5);
    }

    #[test]
    fn finalize_session_epinglee_ancienne_passe_truncate() {
        // Régression : avant le fix, la session pinned ancienne (i=0) était perdue
        // par la troncature à 20 effectuée AVANT l'enrichissement metadata.
        let mut sessions: Vec<SessionSummary> = (0..25)
            .map(|i| make_summary_with(&format!("/s{}.jsonl", i), Some("/proj"), i as u64))
            .collect();
        sessions[0].pinned = true; // la PLUS ANCIENNE est épinglée (par hypothèse, après enrich)
        let mut group = ProjectGroup {
            project_name: "proj".into(),
            project_path: "/proj".into(),
            last_activity: 24,
            session_count: 25,
            sessions,
        };
        finalize_group_session_ordering(&mut group, 20);
        // 1 pinned + 20 non-pinned = 21 sessions gardées
        assert_eq!(group.sessions.len(), 21);
        // La pinned ancienne est en tête, malgré son rang #25 par date
        assert!(group.sessions[0].pinned);
        assert_eq!(group.sessions[0].last_active_at, 0);
        // Les 20 suivantes sont les 20 plus récentes (24, 23, ..., 5)
        assert_eq!(group.sessions[1].last_active_at, 24);
        assert_eq!(group.sessions[20].last_active_at, 5);
    }

    #[test]
    fn finalize_garde_tous_les_pinned_meme_si_plus_de_20() {
        // 30 sessions pinned + 5 non-pinned → toutes gardées (35 au total)
        let mut sessions: Vec<SessionSummary> = (0..30)
            .map(|i| {
                let mut s = make_summary_with(
                    &format!("/p{}.jsonl", i),
                    Some("/proj"),
                    1000 + i as u64,
                );
                s.pinned = true;
                s
            })
            .collect();
        for i in 0..5 {
            sessions.push(make_summary_with(
                &format!("/n{}.jsonl", i),
                Some("/proj"),
                5000 + i as u64,
            ));
        }
        let mut group = ProjectGroup {
            project_name: "proj".into(),
            project_path: "/proj".into(),
            last_activity: 5004,
            session_count: 35,
            sessions,
        };
        finalize_group_session_ordering(&mut group, 20);
        // 30 pinned + 5 non-pinned (sous le max) = 35
        assert_eq!(group.sessions.len(), 35);
        // Les 30 premiers sont pinned
        assert!(group.sessions[..30].iter().all(|s| s.pinned));
        // Les 5 derniers sont non-pinned
        assert!(group.sessions[30..].iter().all(|s| !s.pinned));
    }

    #[test]
    fn finalize_cap_non_pinned_a_20_meme_avec_quelques_pinned() {
        // 3 pinned + 50 non-pinned → 3 + 20 = 23 gardées
        let mut pinned: Vec<SessionSummary> = (0..3)
            .map(|i| {
                let mut s = make_summary_with(
                    &format!("/p{}.jsonl", i),
                    Some("/proj"),
                    100 + i as u64,
                );
                s.pinned = true;
                s
            })
            .collect();
        let non_pinned: Vec<SessionSummary> = (0..50)
            .map(|i| {
                make_summary_with(&format!("/n{}.jsonl", i), Some("/proj"), 1000 + i as u64)
            })
            .collect();
        pinned.extend(non_pinned);
        let mut group = ProjectGroup {
            project_name: "proj".into(),
            project_path: "/proj".into(),
            last_activity: 1049,
            session_count: 53,
            sessions: pinned,
        };
        finalize_group_session_ordering(&mut group, 20);
        assert_eq!(group.sessions.len(), 23);
        let pinned_kept = group.sessions.iter().filter(|s| s.pinned).count();
        let non_pinned_kept = group.sessions.iter().filter(|s| !s.pinned).count();
        assert_eq!(pinned_kept, 3);
        assert_eq!(non_pinned_kept, 20);
    }

    #[test]
    fn sessions_dans_groupe_triees_par_date_apres_grouping() {
        // Sans enrichissement (pinned = false partout), le grouping seul
        // produit déjà un ordre par date desc.
        let mut older = make_summary_with("/old.jsonl", Some("/proj"), 1000);
        older.pinned = false;
        let recent = make_summary_with("/new.jsonl", Some("/proj"), 3000);
        let summaries = vec![older, recent];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sessions[0].transcript_path, "/new.jsonl");
        assert_eq!(groups[0].sessions[1].transcript_path, "/old.jsonl");
    }

    #[test]
    fn groupe_session_unique_retourne_normalement() {
        let summaries = vec![make_summary_with("/a.jsonl", Some("/seul/projet"), 1000)];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project_name, "projet");
        assert_eq!(groups[0].session_count, 1);
        assert_eq!(groups[0].sessions.len(), 1);
    }

    #[test]
    fn groupe_unknown_pour_cwd_null() {
        let summaries = vec![
            make_summary_with("/a.jsonl", None, 5000),
            make_summary_with("/b.jsonl", Some("/home/u/projA"), 1000),
        ];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 2);
        // unknown placé en dernier malgré son activity plus récente
        assert_eq!(groups[0].project_name, "projA");
        assert_eq!(groups[1].project_name, "unknown");
        assert_eq!(groups[1].project_path, "");
    }

    #[test]
    fn groupe_unknown_pour_cwd_whitespace() {
        let summaries = vec![make_summary_with("/a.jsonl", Some("   "), 1000)];
        let groups = group_summaries_by_project(summaries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project_name, "unknown");
        assert_eq!(groups[0].project_path, "");
    }

    // ── parse_codex_summary (Story 6.11, schéma confirmé par spike) ────────────

    #[test]
    fn codex_summary_extrait_cwd_id_modele_et_1er_message() {
        let path = write_temp_jsonl("vigil_codex_summary_ok.jsonl", &[
            r#"{"timestamp":"2026-06-01T10:01:26.113Z","type":"session_meta","payload":{"id":"019e82a1-8cf6-7731-b88a-3c39db10394c","cwd":"C:\\Users\\Dev\\dev\\projects\\vigil","cli_version":"0.135.0"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4-mini"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions> ..."}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context><cwd>C:\\x</cwd></environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Peux tu analyser le code et me donner ton avis ?"}]}}"#,
        ]);
        let s = parse_codex_summary(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(s.session_id.as_deref(), Some("019e82a1-8cf6-7731-b88a-3c39db10394c"));
        assert_eq!(s.cwd.as_deref(), Some("C:\\Users\\Dev\\dev\\projects\\vigil"));
        assert_eq!(s.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(s.first_user_message.as_deref(), Some("Peux tu analyser le code et me donner ton avis ?"));
        assert!(s.started_at > 0);
    }

    #[test]
    fn codex_summary_saute_messages_synthetiques() {
        // developer + user<environment_context> doivent être ignorés ; aucun vrai message → None.
        let path = write_temp_jsonl("vigil_codex_summary_synth.jsonl", &[
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/proj"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"instructions"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
        ]);
        let s = parse_codex_summary(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(s.session_id.as_deref(), Some("abc"));
        assert_eq!(s.first_user_message, None);
    }

    #[test]
    fn codex_summary_ligne_corrompue_ignoree() {
        let path = write_temp_jsonl("vigil_codex_summary_corrupt.jsonl", &[
            r#"PAS DU JSON {{{"#,
            r#"{"type":"session_meta","payload":{"id":"ok","cwd":"/p"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Salut"}]}}"#,
        ]);
        let s = parse_codex_summary(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(s.session_id.as_deref(), Some("ok"));
        assert_eq!(s.first_user_message.as_deref(), Some("Salut"));
    }

    #[test]
    fn codex_summary_fichier_vide_retourne_none() {
        let path = write_temp_jsonl("vigil_codex_summary_vide.jsonl", &[]);
        let result = parse_codex_summary(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_none());
    }

    #[test]
    fn codex_summary_message_long_tronque_a_60() {
        let long = "a".repeat(100);
        let line = format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{}"}}]}}}}"#,
            long
        );
        let path = write_temp_jsonl("vigil_codex_summary_long.jsonl", &[
            r#"{"type":"session_meta","payload":{"id":"x","cwd":"/p"}}"#,
            &line,
        ]);
        let s = parse_codex_summary(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let msg = s.first_user_message.unwrap();
        // truncate_60 garde 60 caractères + ellipse
        assert!(msg.ends_with('…'));
        assert_eq!(msg.chars().count(), 61);
    }

    // ── scan récursif Codex (arborescence YYYY/MM/DD) ─────────────────────────

    #[test]
    fn scan_recursif_trouve_fichier_profond() {
        let base = std::env::temp_dir().join(format!("vigil_hist_codex_scan_{}", std::process::id()));
        let deep = base.join("2026").join("06").join("01");
        std::fs::create_dir_all(&deep).unwrap();
        let target = deep.join("rollout-x.jsonl");
        std::fs::File::create(&target).unwrap();
        std::fs::File::create(deep.join("ignore.txt")).unwrap();

        let mut results = Vec::new();
        scan_jsonl_files_recursive(&base, &mut results, 0, 4);

        let _ = std::fs::remove_dir_all(&base);
        assert!(results.iter().any(|p| p == &target), "fichier profond non trouvé : {:?}", results);
        assert_eq!(results.len(), 1, "seul le .jsonl doit être collecté");
    }

    #[test]
    fn get_all_grouped_outil_inconnu_retourne_vide() {
        // Grok a désormais son propre parser (Story 6.28) → ne teste plus "grok" ici (voir
        // `grok_get_all_grouped_ne_panique_pas`). Seul un outil INCONNU doit retourner vide.
        assert!(HistoryReader::get_all_grouped("inconnu").is_empty());
    }

    // ── Antigravity : lecteur historique JSONL (brain/ + history.jsonl, Story 6.20) ──

    /// Crée une arborescence `brain/` d'exemple : pour chaque (conversationId, contenu transcript),
    /// écrit `brain/<id>/.system_generated/logs/transcript.jsonl`. Renvoie le dossier `brain`.
    fn write_temp_brain(name: &str, convs: &[(&str, &[&str])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let brain = root.join("brain");
        for (id, lines) in convs {
            let logs = brain.join(id).join(".system_generated").join("logs");
            std::fs::create_dir_all(&logs).unwrap();
            let mut f = std::fs::File::create(logs.join("transcript.jsonl")).unwrap();
            for line in *lines {
                writeln!(f, "{}", line).unwrap();
            }
        }
        brain
    }

    fn write_temp_history(brain: &Path, lines: &[&str]) -> PathBuf {
        // history.jsonl vit à côté de brain/ (dans antigravity-cli/).
        let path = brain.parent().unwrap().join("history.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        path
    }

    const AGY_USER_LINE: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T13:27:00Z","content":"<USER_REQUEST>\nAnalyse du code\n</USER_REQUEST>"}"#;
    const AGY_MODEL_LINE: &str = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-07-05T13:27:01Z","tool_calls":[]}"#;

    #[test]
    fn antigravity_mappe_brain_et_enrichit_cwd_via_history() {
        let brain = write_temp_brain(
            "vigil_agy_brain",
            &[
                ("conv-1", &[AGY_USER_LINE, AGY_MODEL_LINE]),
                // conv-2 : présent dans brain mais ABSENT de history.jsonl → cwd None (unknown).
                (
                    "conv-2",
                    &[r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-06T09:00:00Z","content":"<USER_REQUEST>\nDeuxieme conversation\n</USER_REQUEST>"}"#],
                ),
            ],
        );
        let history = write_temp_history(
            &brain,
            &[r#"{"conversationId":"conv-1","display":"Analyse du code","timestamp":1751721600000,"workspace":"C:\\Users\\Dev\\dev\\projects\\proj"}"#],
        );

        let summaries = read_summaries_from(&brain, Some(&history));
        let _ = std::fs::remove_dir_all(brain.parent().unwrap());

        assert_eq!(summaries.len(), 2);

        let s1 = summaries.iter().find(|s| s.session_id.as_deref() == Some("conv-1")).unwrap();
        // session_id = conversationId (nom de dossier), pour le resume
        assert_eq!(s1.session_id.as_deref(), Some("conv-1"));
        // transcript_path = chemin EXACT rapporté par l'agent vivant (slashes avant,
        // transcript_full.jsonl) → matche le badge « Active » de History
        assert!(
            s1.transcript_path.ends_with("conv-1/.system_generated/logs/transcript_full.jsonl"),
            "transcript_path inattendu : {}",
            s1.transcript_path
        );
        assert!(!s1.transcript_path.contains('\\'), "doit utiliser des slashes avant");
        // cwd vient de history.jsonl (workspace, chemin natif)
        assert_eq!(s1.cwd.as_deref(), Some("C:\\Users\\Dev\\dev\\projects\\proj"));
        // 1er message extrait de <USER_REQUEST> du transcript
        assert_eq!(s1.first_user_message.as_deref(), Some("Analyse du code"));
        // started_at = created_at ISO8601 ; last_active_at = mtime (>0)
        assert_eq!(s1.started_at, iso8601_to_unix_ms("2026-07-05T13:27:00Z"));
        assert!(s1.last_active_at > 0);
        assert_eq!(s1.model, None);

        // conv-2 : absent de history → cwd None, message quand même lu depuis le transcript
        let s2 = summaries.iter().find(|s| s.session_id.as_deref() == Some("conv-2")).unwrap();
        assert_eq!(s2.cwd, None);
        assert_eq!(s2.first_user_message.as_deref(), Some("Deuxieme conversation"));
    }

    #[test]
    fn antigravity_groupe_par_projet_via_workspace() {
        let brain = write_temp_brain("vigil_agy_group", &[("conv-x", &[AGY_USER_LINE])]);
        let history = write_temp_history(
            &brain,
            &[r#"{"conversationId":"conv-x","display":"x","timestamp":1751721600000,"workspace":"C:\\Users\\Dev\\dev\\projects\\proj"}"#],
        );
        let groups = group_summaries_by_project(read_summaries_from(&brain, Some(&history)));
        let _ = std::fs::remove_dir_all(brain.parent().unwrap());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project_name, "proj");
        assert_eq!(groups[0].session_count, 1);
    }

    #[test]
    fn antigravity_sans_history_liste_quand_meme_cwd_none() {
        // history.jsonl absent (None) → la liste vient toujours de brain/, cwd = None (unknown).
        let brain = write_temp_brain("vigil_agy_nohist", &[("conv-1", &[AGY_USER_LINE])]);
        let summaries = read_summaries_from(&brain, None);
        let _ = std::fs::remove_dir_all(brain.parent().unwrap());
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].cwd, None);
        assert_eq!(summaries[0].first_user_message.as_deref(), Some("Analyse du code"));
    }

    #[test]
    fn antigravity_dossier_sans_transcript_ignore() {
        // Un dossier brain sans transcript.jsonl → sauté (rien à afficher), pas de panique.
        let root = std::env::temp_dir().join(format!("vigil_agy_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let brain = root.join("brain");
        std::fs::create_dir_all(brain.join("conv-vide")).unwrap();
        let summaries = read_summaries_from(&brain, None);
        let _ = std::fs::remove_dir_all(&root);
        assert!(summaries.is_empty());
    }

    #[test]
    fn antigravity_brain_absent_retourne_vide() {
        let result = read_summaries_from(Path::new("C:/nonexistent/path/brain"), None);
        assert!(result.is_empty());
    }

    #[test]
    fn antigravity_get_all_grouped_ne_panique_pas() {
        // Sur le vrai poste (brain présent/absent selon l'état) : jamais de panique.
        let _ = HistoryReader::get_all_grouped("antigravity");
    }

    #[test]
    fn history_index_retient_entree_la_plus_ancienne() {
        let root = std::env::temp_dir().join(format!("vigil_agy_hidx_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("history.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            // Deux lignes pour conv-1 : la plus ANCIENNE (ts plus petit) doit gagner.
            writeln!(f, r#"{{"conversationId":"conv-1","display":"deuxieme prompt","timestamp":2000,"workspace":"C:\\ws2"}}"#).unwrap();
            writeln!(f, r#"{{"conversationId":"conv-1","display":"premier prompt","timestamp":1000,"workspace":"C:\\ws1"}}"#).unwrap();
            // Ligne sans conversationId → ignorée.
            writeln!(f, r#"{{"display":"orphan","timestamp":500,"workspace":"C:\\x"}}"#).unwrap();
        }
        let index = read_history_index_from(&path);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(index.len(), 1);
        let (ws, display) = index.get("conv-1").unwrap();
        assert_eq!(ws.as_deref(), Some("C:\\ws1")); // celui du 1er prompt
        assert_eq!(display.as_deref(), Some("premier prompt"));
    }

    #[test]
    fn history_index_fichier_absent_map_vide() {
        let index = read_history_index_from(Path::new("C:/nonexistent/history.jsonl"));
        assert!(index.is_empty());
    }

    #[test]
    fn parse_transcript_extrait_message_et_date() {
        let path = write_temp_jsonl("vigil_agy_transcript.jsonl", &[AGY_USER_LINE, AGY_MODEL_LINE]);
        let (msg, started) = parse_antigravity_transcript(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(msg.as_deref(), Some("Analyse du code"));
        assert_eq!(started, iso8601_to_unix_ms("2026-07-05T13:27:00Z"));
    }

    #[test]
    fn parse_transcript_message_long_tronque_a_60() {
        let long = "a".repeat(100);
        let line = format!(
            r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T13:27:00Z","content":"<USER_REQUEST>\n{}\n</USER_REQUEST>"}}"#,
            long
        );
        let path = write_temp_jsonl("vigil_agy_transcript_long.jsonl", &[&line]);
        let (msg, _) = parse_antigravity_transcript(&path);
        let _ = std::fs::remove_file(&path);
        let msg = msg.unwrap();
        assert!(msg.ends_with('…'));
        assert_eq!(msg.chars().count(), 61);
    }

    // ── Grok Build : lecteur historique (arbre ~/.grok/sessions/, Story 6.28) ──────────

    /// Écrit un arbre `sessions/<encoded_cwd>/<sessionId>/` d'exemple. Chaque tuple =
    /// `(encoded_cwd, session_id, summary_json_optionnel, lignes_chat_history)`. `updates.jsonl` est
    /// toujours écrit (sa présence marque une session valide). Renvoie le dossier `sessions/`.
    fn write_temp_grok_sessions(
        name: &str,
        sessions: &[(&str, &str, Option<&str>, &[&str])],
    ) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        for (encoded_cwd, sid, summary, chat) in sessions {
            let dir = sessions_dir.join(encoded_cwd).join(sid);
            std::fs::create_dir_all(&dir).unwrap();
            // updates.jsonl : présence = session valide + cible du transcript_path + mtime.
            std::fs::write(dir.join("updates.jsonl"), b"{\"timestamp\":1783776114}\n").unwrap();
            if let Some(s) = summary {
                std::fs::write(dir.join("summary.json"), s).unwrap();
            }
            let mut c = std::fs::File::create(dir.join("chat_history.jsonl")).unwrap();
            for line in *chat {
                writeln!(c, "{}", line).unwrap();
            }
        }
        sessions_dir
    }

    /// urlencode minimal (miroir de `vigil-grok::urlencode_grok` 6.27) — test-local, sert au
    /// round-trip avec `urldecode_grok`.
    fn urlencode_grok_for_test(s: &str) -> String {
        let mut out = String::new();
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    const GROK_SUMMARY: &str = r#"{"info":{"id":"019f-aaa","cwd":"C:\\Users\\Dev\\dev\\proj"},"created_at":"2026-07-11T13:21:54.354Z","current_model_id":"grok-4.5","session_summary":"Salut"}"#;

    #[test]
    fn grok_mappe_arbre_sessions_vers_summary() {
        // chat_history contient un <user_info> synthétique en 1er (retour Steven 6.28 : ce bloc
        // n'a PAS de synthetic_reason et polluait la liste). Le 1er message DOIT venir de
        // session_summary, JAMAIS de <user_info> ni du <user_query> de chat_history (réservé 6.29).
        let chat: &[&str] = &[
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: windows\n</user_info>"}]}"#,
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nSalut\n</user_query>"}]}"#,
        ];
        let sessions_dir = write_temp_grok_sessions(
            "vigil_grok_map",
            &[("C%3A%5CUsers%5CDev", "019f-aaa", Some(GROK_SUMMARY), chat)],
        );
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(sessions_dir.parent().unwrap());

        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.session_id.as_deref(), Some("019f-aaa"));
        // transcript_path byte-exact (backslash natif) — cible du badge « Active » (leçon 6.20).
        assert_eq!(
            s.transcript_path,
            r"C:\Users\Dev\.grok\sessions\C%3A%5CUsers%5CDev\019f-aaa\updates.jsonl"
        );
        assert_eq!(s.cwd.as_deref(), Some(r"C:\Users\Dev\dev\proj")); // info.cwd
        assert_eq!(s.model.as_deref(), Some("grok-4.5")); // current_model_id
        assert_eq!(s.first_user_message.as_deref(), Some("Salut")); // session_summary, PAS <user_info>
        assert_eq!(s.started_at, iso8601_to_unix_ms("2026-07-11T13:21:54.354Z")); // created_at
        assert!(s.last_active_at > 0); // mtime updates.jsonl
    }

    #[test]
    fn grok_read_summary_helper_model() {
        // T3 (6.29) : le helper pur `read_grok_summary` (réutilisé par `get_agent_model`) lit
        // `current_model_id` → Some ; `summary.json` absent → None (dégradation propre, jamais panique).
        let dir = std::env::temp_dir().join(format!("vigil_grok_summ_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.json"), GROK_SUMMARY).unwrap();
        assert_eq!(read_grok_summary(&dir).model.as_deref(), Some("grok-4.5"));
        std::fs::remove_file(dir.join("summary.json")).unwrap();
        assert_eq!(read_grok_summary(&dir).model, None); // fichier absent → None
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_summary_absent_model_none_cwd_decode() {
        // Sans summary.json : model None, cwd décodé du nom de dossier, et PAS de 1er message
        // (chat_history réservé à la Vue Détail 6.29 → la liste n'y va pas). Session listée quand même.
        let sessions_dir = write_temp_grok_sessions(
            "vigil_grok_nosummary",
            &[("C%3A%5CUsers%5CDev", "sid-1", None, &[])],
        );
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(sessions_dir.parent().unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model, None);
        assert_eq!(out[0].cwd.as_deref(), Some(r"C:\Users\Dev")); // repli urldecode
        assert_eq!(out[0].first_user_message, None); // pas de session_summary → pas de 1er message
    }

    #[test]
    fn grok_exclut_les_sous_agents_de_la_liste() {
        // Story 6.31 (AC8) : une session `session_kind:"subagent"` est EXCLUE de la liste (exclusion
        // pure, aucun nichage) ; la session normale voisine reste listée.
        const SUB_SUMMARY: &str = r#"{"info":{"cwd":"C:\\Users\\Dev\\dev\\proj"},"created_at":"2026-07-11T13:22:00.000Z","current_model_id":"grok-4.5","session_kind":"subagent","session_summary":"délégué"}"#;
        let sessions_dir = write_temp_grok_sessions(
            "vigil_grok_subagent_excl",
            &[
                ("C%3A%5CUsers%5CDev", "parent-1", Some(GROK_SUMMARY), &[]),
                ("C%3A%5CUsers%5CDev", "child-1", Some(SUB_SUMMARY), &[]),
            ],
        );
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(sessions_dir.parent().unwrap());

        assert_eq!(out.len(), 1, "seule la session normale est listée");
        assert_eq!(out[0].session_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn grok_read_summary_lit_session_kind() {
        // Story 6.31 : le helper pur lit `session_kind` (repli cold-start anti-fantôme). Absent → None.
        let dir = std::env::temp_dir().join(format!("vigil_grok_kind_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.json"), r#"{"session_kind":"subagent"}"#).unwrap();
        assert_eq!(read_grok_summary(&dir).session_kind.as_deref(), Some("subagent"));
        std::fs::write(dir.join("summary.json"), GROK_SUMMARY).unwrap(); // pas de session_kind
        assert_eq!(read_grok_summary(&dir).session_kind, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_session_sans_updates_jsonl_sautee() {
        let root = std::env::temp_dir().join(format!("vigil_grok_noupd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(sessions_dir.join("C%3A%5Cx").join("sid-vide")).unwrap();
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(&root);
        assert!(out.is_empty());
    }

    #[test]
    fn grok_sessions_dir_absent_retourne_vide() {
        let out = read_grok_summaries_from(Path::new("C:/nonexistent/grok/sessions"), r"C:\Users\Dev");
        assert!(out.is_empty());
    }

    #[test]
    fn grok_fichier_non_dossier_au_niveau_racine_ignore() {
        // Piège T0 : `session_search.sqlite` est un FICHIER au niveau sessions/ → sauté (is_dir()),
        // la vraie session à côté reste lue.
        let root = std::env::temp_dir().join(format!("vigil_grok_sqlite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(sessions_dir.join("session_search.sqlite"), b"binaire").unwrap();
        let dir = sessions_dir.join("C%3A%5Cp").join("sid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("updates.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("chat_history.jsonl"), "").unwrap();
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id.as_deref(), Some("sid"));
    }

    #[test]
    fn grok_summary_json_corrompu_degrade_proprement() {
        // summary.json illisible → GrokSummary::default (tous None), session listée quand même,
        // cwd en repli via urldecode, aucune panique.
        let sessions_dir = write_temp_grok_sessions(
            "vigil_grok_badsummary",
            &[("C%3A%5Cp", "sid", Some("PAS DU JSON {{{"), &[])],
        );
        let out = read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev");
        let _ = std::fs::remove_dir_all(sessions_dir.parent().unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model, None);
        assert_eq!(out[0].first_user_message, None);
        assert_eq!(out[0].cwd.as_deref(), Some(r"C:\p")); // repli urldecode
    }

    #[test]
    fn grok_groupe_par_projet_via_cwd_du_nom_de_dossier() {
        // Divergence favorable : le cwd vient du nom de dossier encodé → 2 projets = 2 groupes,
        // gratuitement (même sans summary.json).
        let chat: &[&str] = &[r#"{"type":"user","content":[{"type":"text","text":"hi"}]}"#];
        let sessions_dir = write_temp_grok_sessions(
            "vigil_grok_group",
            &[
                ("C%3A%5CUsers%5CDev%5Cproj-a", "sid-a", None, chat),
                ("C%3A%5CUsers%5CDev%5Cproj-b", "sid-b", None, chat),
            ],
        );
        let groups = group_summaries_by_project(read_grok_summaries_from(&sessions_dir, r"C:\Users\Dev"));
        let _ = std::fs::remove_dir_all(sessions_dir.parent().unwrap());
        assert_eq!(groups.len(), 2);
        let names: Vec<&str> = groups.iter().map(|g| g.project_name.as_str()).collect();
        assert!(names.contains(&"proj-a"), "groupes: {names:?}");
        assert!(names.contains(&"proj-b"), "groupes: {names:?}");
    }

    #[test]
    fn grok_urldecode_round_trip_avec_urlencode_627() {
        let cwd = r"C:\Users\Dev";
        let encoded = urlencode_grok_for_test(cwd);
        assert_eq!(encoded, "C%3A%5CUsers%5CDev"); // = forme disque réelle (spike T0)
        assert_eq!(urldecode_grok(&encoded).as_deref(), Some(cwd));
        // %XX malformé → % conservé littéral (dégradation propre, pas de panique).
        assert_eq!(urldecode_grok("a%ZZb").as_deref(), Some("a%ZZb"));
    }

    #[test]
    fn grok_urldecode_pourcent_suivi_utf8_multioctets_ne_panique_pas() {
        // Régression P2 (code review 6.28) : un `%` littéral suivi d'un caractère UTF-8 multi-octets
        // (`€` = 3 octets E2 82 AC) faisait tomber la borne de slice `&encoded[i+1..i+3]` hors
        // frontière de caractère → panique. Doit désormais conserver `%` littéral, byte-préserver le
        // caractère, et ne JAMAIS paniquer (invariant AC6).
        assert_eq!(urldecode_grok("%€x").as_deref(), Some("%€x"));
        assert_eq!(urldecode_grok("a%😀b").as_deref(), Some("a%😀b"));
        // `%` en toute fin (aucun chiffre hexa derrière) → littéral, pas de panique.
        assert_eq!(urldecode_grok("abc%").as_deref(), Some("abc%"));
        assert_eq!(urldecode_grok("abc%4").as_deref(), Some("abc%4"));
        // Mélange valide + piège : `%5C` décodé, `%€` conservé.
        assert_eq!(urldecode_grok("%5C%€").as_deref(), Some(r"\%€"));
    }

    #[test]
    fn grok_transcript_path_forme_byte_exacte() {
        // DOIT byte-matcher vigil-grok::grok_transcript_path_in (badge « Active », leçon 6.20).
        assert_eq!(
            grok_transcript_path(r"C:\Users\Dev", "C%3A%5CUsers%5CDev", "019f-abc"),
            r"C:\Users\Dev\.grok\sessions\C%3A%5CUsers%5CDev\019f-abc\updates.jsonl"
        );
        // home avec séparateur final normalisé (pas de double backslash).
        assert_eq!(
            grok_transcript_path(r"C:\Users\Dev\", "enc", "sid"),
            r"C:\Users\Dev\.grok\sessions\enc\sid\updates.jsonl"
        );
    }

    #[test]
    fn grok_get_all_grouped_ne_panique_pas() {
        // Poste réel (sessions présent/absent selon l'état) : jamais de panique.
        let _ = HistoryReader::get_all_grouped("grok");
    }

    // ── Kimi (9.4) : read_kimi_summaries_from ────────────────────────────────────
    //
    // Fixtures = extraits RÉELS du poste (spike T0 2026-07-24). `state.json` : createdAt/updatedAt
    // sont des chaînes ISO8601, title = 1er message user, isCustomTitle, workDir = cwd.

    const KIMI_STATE: &str = r#"{"createdAt":"2026-07-22T16:40:55.048Z","updatedAt":"2026-07-22T17:03:36.454Z","title":"Salut","isCustomTitle":false,"agents":{"main":{"type":"main","parentAgentId":null}},"workDir":"C:/Users/Dev","lastPrompt":"Salut"}"#;

    /// Crée un arbre Kimi de test dans un tempdir et rend `(index_content, root)`. Chaque session :
    /// `(session_id, state_json_opt, has_wire)`. Le `sessionDir` inscrit dans l'index est le chemin
    /// RÉEL du tempdir converti en SLASHS AVANT (comme le vrai index) → on valide la byte-identité du
    /// `transcript_path`. `extra_index_lines` permet d'injecter des lignes brutes (non-JSON, doublons).
    fn write_temp_kimi(
        tag: &str,
        sessions: &[(&str, Option<&str>, bool)],
        extra_index_lines: &[&str],
    ) -> (String, PathBuf) {
        let root = std::env::temp_dir().join(format!("vigil_kimi_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut index = String::new();
        for (sid, state, has_wire) in sessions {
            let session_dir = root.join("sessions").join("wd_x").join(sid);
            std::fs::create_dir_all(&session_dir).unwrap();
            if let Some(s) = state {
                std::fs::write(session_dir.join("state.json"), s).unwrap();
            }
            if *has_wire {
                let main = session_dir.join("agents").join("main");
                std::fs::create_dir_all(&main).unwrap();
                std::fs::write(main.join("wire.jsonl"), "{}\n").unwrap();
            }
            // sessionDir en slashs avant (forme réelle de l'index).
            let dir_fwd = session_dir.to_string_lossy().replace('\\', "/");
            index.push_str(&format!(
                r#"{{"sessionId":"{sid}","sessionDir":"{dir_fwd}","workDir":"C:/Users/Dev"}}"#
            ));
            index.push('\n');
        }
        for l in extra_index_lines {
            index.push_str(l);
            index.push('\n');
        }
        (index, root)
    }

    #[test]
    fn kimi_mappe_state_json_en_session_summary() {
        let (index, root) = write_temp_kimi(
            "map",
            &[("session_c4974b71", Some(KIMI_STATE), true)],
            &[],
        );
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.session_id.as_deref(), Some("session_c4974b71"));
        assert_eq!(s.first_user_message.as_deref(), Some("Salut"));
        assert_eq!(s.custom_name, None); // isCustomTitle=false
        assert_eq!(s.cwd.as_deref(), Some("C:/Users/Dev"));
        assert_eq!(s.model, None); // badge on-demand
        // ISO8601 → ms Unix (2026-07-22T17:03:36.454Z).
        assert!(s.last_active_at > 0, "updatedAt doit se parser en ms");
        assert!(s.started_at > 0 && s.started_at <= s.last_active_at);
    }

    #[test]
    fn kimi_transcript_path_byte_identique_a_vigil_kimi() {
        // INVARIANT DUR (leçon 6.20 Correctif #1) : <sessionDir>/agents/main/wire.jsonl, sessionDir
        // TEL QUEL (slashs avant), jamais re-normalisé en backslash.
        assert_eq!(
            kimi_transcript_path("C:/Users/Dev/.kimi-code/sessions/wd_x/session_1"),
            "C:/Users/Dev/.kimi-code/sessions/wd_x/session_1/agents/main/wire.jsonl"
        );
        // séparateur de fin retiré (pas de double slash).
        assert_eq!(
            kimi_transcript_path("C:/Users/Dev/sessions/wd_x/session_1/"),
            "C:/Users/Dev/sessions/wd_x/session_1/agents/main/wire.jsonl"
        );
    }

    #[test]
    fn kimi_transcript_path_dans_summary_est_byte_exact() {
        let (index, root) = write_temp_kimi("tp", &[("session_1", Some(KIMI_STATE), true)], &[]);
        let out = read_kimi_summaries_from(&index);
        let expected = format!(
            "{}/agents/main/wire.jsonl",
            root.join("sessions").join("wd_x").join("session_1").to_string_lossy().replace('\\', "/")
        );
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out[0].transcript_path, expected);
    }

    #[test]
    fn kimi_custom_title_donne_custom_name() {
        const CUSTOM: &str = r#"{"createdAt":"2026-07-22T16:40:55.048Z","updatedAt":"2026-07-22T17:03:36.454Z","title":"Mon titre","isCustomTitle":true,"workDir":"C:/Users/Dev"}"#;
        let (index, root) = write_temp_kimi("custom", &[("session_1", Some(CUSTOM), true)], &[]);
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out[0].custom_name.as_deref(), Some("Mon titre"));
        assert_eq!(out[0].first_user_message.as_deref(), Some("Mon titre"));
    }

    #[test]
    fn kimi_session_sans_state_ni_wire_est_ignoree() {
        // Anti-fantôme : un sessionDir d'index sans dossier réel (ni state.json ni wire) → ignoré.
        // Ici la 2e session a un dossier créé mais VIDE (ni state ni wire) → doit être sautée.
        let (index, root) = write_temp_kimi(
            "phantom",
            &[
                ("session_ok", Some(KIMI_STATE), true),
                ("session_vide", None, false),
            ],
            &[],
        );
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1, "seule la session ancrée est listée");
        assert_eq!(out[0].session_id.as_deref(), Some("session_ok"));
    }

    #[test]
    fn kimi_wire_sans_state_est_liste_avec_replis() {
        // state.json absent mais agents/main/wire.jsonl présent → session listée (repli mtime + cwd
        // de l'index), jamais de panique.
        let (index, root) = write_temp_kimi("nostate", &[("session_1", None, true)], &[]);
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].first_user_message, None); // pas de title
        assert_eq!(out[0].cwd.as_deref(), Some("C:/Users/Dev")); // repli workDir de l'index
    }

    #[test]
    fn kimi_ligne_index_illisible_ou_dir_vide_saute_sans_perdre_les_autres() {
        // Tolérance find_in_index : ligne non-JSON + entrée sessionDir vide → continue, pas return.
        let (index, root) = write_temp_kimi(
            "tolerant",
            &[("session_ok", Some(KIMI_STATE), true)],
            &[
                "ceci n'est pas du JSON {{{",
                r#"{"sessionId":"session_novide","sessionDir":"","workDir":"x"}"#,
            ],
        );
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id.as_deref(), Some("session_ok"));
    }

    #[test]
    fn kimi_dedup_par_session_id() {
        // L'index est append-only : une reprise réécrit la même session → une seule entrée listée.
        let (index, root) = write_temp_kimi("dedup", &[("session_dup", Some(KIMI_STATE), true)], &[]);
        // Duplique la ligne d'index (le dossier existe déjà).
        let doubled = format!("{index}{index}");
        let out = read_kimi_summaries_from(&doubled);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1, "sessionId dédupliqué");
    }

    #[test]
    fn kimi_dedup_garde_la_derniere_occurrence() {
        // Contrat §3 : parcours INVERSE (comme `vigil-kimi::find_in_index`) → sur une reprise qui
        // réécrit la même session avec un sessionDir différent, la DERNIÈRE ligne gagne (sinon le
        // transcript_path diverge de l'agent vivant → badge « Active » mort).
        let (_auto, root) = write_temp_kimi(
            "deduplast",
            &[("session_a", Some(KIMI_STATE), true), ("session_b", Some(KIMI_STATE), true)],
            &[],
        );
        let dir_a = root.join("sessions").join("wd_x").join("session_a").to_string_lossy().replace('\\', "/");
        let dir_b = root.join("sessions").join("wd_x").join("session_b").to_string_lossy().replace('\\', "/");
        let index = format!(
            "{{\"sessionId\":\"dup\",\"sessionDir\":\"{dir_a}\",\"workDir\":\"C:/Users/Dev\"}}\n{{\"sessionId\":\"dup\",\"sessionDir\":\"{dir_b}\",\"workDir\":\"C:/Users/Dev\"}}\n"
        );
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1, "sessionId dédupliqué");
        assert_eq!(
            out[0].transcript_path,
            format!("{dir_b}/agents/main/wire.jsonl"),
            "la dernière occurrence (parcours inverse) gagne"
        );
    }

    #[test]
    fn kimi_state_avec_bom_utf8_parse_quand_meme() {
        // Un state.json portant un BOM UTF-8 ne doit PAS vider les métadonnées (serde rejette le BOM).
        let bommed = format!("\u{FEFF}{KIMI_STATE}");
        let (index, root) = write_temp_kimi("bom", &[("session_1", Some(&bommed), true)], &[]);
        let out = read_kimi_summaries_from(&index);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].first_user_message.as_deref(), Some("Salut"), "métadonnées lues malgré le BOM");
    }

    #[test]
    fn kimi_index_absent_retourne_vide() {
        assert!(read_kimi_summaries_from("").is_empty());
    }

    #[test]
    fn kimi_get_all_grouped_ne_panique_pas() {
        // Poste réel (index présent/absent selon l'état) : jamais de panique.
        let _ = HistoryReader::get_all_grouped("kimi");
    }
}
