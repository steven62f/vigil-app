// Bridge entre Grok Build et Vigil (Epic 6 / stories 6.1, 6.26, 6.27).
//
// Différence MAJEURE vs vigil-claude/codex : Grok envoie ses payloads avec des clés **camelCase**
// (`hookEventName`, `sessionId`, `transcriptPath`, `toolName`, `toolInput`) et des **valeurs
// d'events snake_case** (`session_start`, `pre_tool_use`…), SANS le champ `hook_event_name`
// PascalCase sur lequel Vigil dispatche (`#[serde(tag = "hook_event_name")]`). Un simple forward
// serait donc rejeté EN SILENCE par le parseur → aucun agent. Ce binaire porte donc une **couche
// de traduction** (comme vigil-antigravity 6.17) qui normalise chaque payload vers le contrat
// interne de Vigil (`pipe/protocol.rs`, snake_case + tag `hook_event_name`) AVANT de l'écrire sur
// le pipe. Le pipe et le state engine restent inchangés (aucun handler outil-spécifique).
//
// Particularités Grok (spike 2026-07-11, mémoire `project_grok_hook_schema`) :
//   - L'event est DANS le payload (`hookEventName`), pas en argv (≠ Antigravity).
//   - Grok émet `session_start` NATIVEMENT mais SANS `transcriptPath` → on le **synthétise**
//     (`~/.grok/sessions/<urlencode(cwd)>/<sessionId>/updates.jsonl`, chemin absolu backslash
//     natif Windows). ⚠️ RISQUE #1 (bloquant) : cette synthèse doit byte-matcher le
//     `transcriptPath` réel des events suivants — le moteur dédup sur `transcript_path`
//     (`state_engine.rs:22`, `SessionStart` = `insert()` L76) — sinon DEUX agents fantômes. Le
//     schéma d'encodage est calé sur le disque réel (`cwd` `C:\Users\Dev` → groupe
//     `C%3A%5CUsers%5CDev`, `grok_home` = `C:\Users\Dev\.grok`, séparateurs backslash). La
//     preuve byte-exacte FINALE est l'e2e AC7 : si un double agent apparaît → ajuster le schéma de
//     séparateurs de `grok_transcript_path` (décision Steven 2026-07-11 : option « synthèse d'après
//     le disque, preuve à l'e2e »).
//   - Permissions (Story 6.30) : Grok n'a AUCUN event `PermissionRequest` ; c'est `pre_tool_use`
//     qui porte `permissionMode`. ⚠️ Le hook est un VETO deny-only : `{"decision":"deny"}` bloque
//     (autoritaire, tous modes) mais `{"decision":"allow"}` NE bypass QU'en `bypassPermissions`
//     (en `default`/ask le TUI prompte quand même — validé e2e run 6). vigil-grok classe donc
//     (`classify_pre_tool_use`) en 4 cas : **Approve** (`bypassPermissions` : le hook est le seul
//     portillon → `PermissionRequest` actionnable, BLOQUE `read_pipe_response` sur la MÊME connexion
//     comme vigil-codex, mappe la décision Vigil → stdout — approve/deny à distance), **Inform**
//     (`default`/ask : `allow` ne bypass pas → `PermissionRequest` **read-only** fire-and-forget, PAS
//     de blocage : Vigil affiche pour l'awareness, l'utilisateur répond au terminal), **Question**
//     (`ask_user_question` : `AskUserQuestion` read-only, la TUI Grok pose la question) et **Monitor**
//     (sous-agent/autres modes : comportement 6.27 inchangé, `emit` + `exit 0`).
//   - Cleanup via `pid_watcher` (le `session_end` ne fire pas au `/quit` ; on le traduit s'il
//     arrive) — d'où l'injection de `parent_pid` (shell hôte) par `emit`.
//
// La garde `invoked_by_grok()` n'est **PAS** câblée ici : vigil-grok DOIT tourner quand
// `GROK_SESSION_ID` est présent (c'est Grok invoquant son propre binaire, cf. story 6.26). Elle
// n'est câblée que sur les binaires NON-Grok (vigil-claude/codex/antigravity).

use std::io::Write;
use std::process;

use serde_json::{json, Value};
use vigil_hook_core::{
    check_tool_enabled, inject_parent_pid, inject_source_tool, open_vigil_pipe, read_pipe_response,
    read_stdin, write_payload,
};

const SOURCE_TOOL: &str = "grok";
// Process cible dont on remonte l'arbre pour trouver le terminal parent (best-effort, pour le
// nettoyage par `pid_watcher`). Le nom exact peut varier (`grok.exe`/`grok-build.exe`) mais la
// remontée au shell hôte le plus externe (`get_terminal_pid_for`) n'en dépend pas.
const PROCESS_NAME: &str = "grok.exe";

// Format de décision attendu par Grok sur stdout pour un hook `pre_tool_use` bloquant (doc
// `~/.grok/docs/user-guide/10-hooks.md`, validé e2e runs 4-7, 2026-07-13). ⚠️ `{"decision":"allow"}`
// (+ exit 0) NE bypass le TUI QU'en `bypassPermissions` (en `default`/ask le TUI prompte quand même
// — run 6) ; `{"decision":"deny","reason":…}` (+ exit 2) bloque l'outil dans TOUS les modes (veto).
const GROK_ALLOW: &str = r#"{"decision":"allow"}"#;
// Libellé neutre : le deny peut venir de l'utilisateur, d'une règle d'auto-permission ou du
// timeout côté PipeManager (`APPROVAL_TIMEOUT_SECS`, 600 s depuis la Story 8.8) — éviter « par
// l'utilisateur » qui serait inexact dans ces cas.
const GROK_DENY: &str = r#"{"decision":"deny","reason":"Refusé via Vigil"}"#;

/// Classification d'un `pre_tool_use` Grok (Story 6.30, AC1). Fonction PURE (aucun I/O) → testable.
#[derive(Debug, PartialEq, Eq)]
enum PermMode {
    /// `bypassPermissions` : le hook est le SEUL portillon (allow passe sans prompt, deny bloque —
    /// validé e2e run 7) → Vigil décide (synthèse `PermissionRequest` actionnable, BLOQUE la réponse).
    Approve,
    /// `default`/ask : l'`allow` du hook NE bypass PAS le TUI (validé run 6) → on ne peut ni approuver
    /// ni refuser depuis Vigil → `PermissionRequest` **read-only** (awareness), PAS de blocage.
    Inform,
    /// `ask_user_question` → affichage lecture-seule (synthèse `AskUserQuestion`, PAS de blocage).
    Question,
    /// Sous-agent, ou event non déniable → pass-through 6.27 (`emit` + `exit 0`).
    Monitor,
}

fn main() {
    let Some(input) = read_stdin() else {
        process::exit(0);
    };

    if !check_tool_enabled(SOURCE_TOOL) {
        process::exit(0);
    }

    // L'event est dans le payload (clé `hookEventName`, valeur snake_case).
    let event = grok_event_name(&input);

    // Story 6.30 : `pre_tool_use` est le SEUL event déniable → il est routé par classification.
    // Approve → bloque et répond sur stdout (n'atteint jamais le `translate`/`emit` ci-dessous) ;
    // Question → synthétise `AskUserQuestion` non bloquant ; Monitor → retombe sur le flow 6.27.
    if event == "pre_tool_use" {
        let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
        match classify_pre_tool_use(&v) {
            PermMode::Approve => handle_approve(&v), // ne retourne jamais (process::exit interne)
            PermMode::Inform => {
                emit_permission_readonly(&v);
                process::exit(0);
            }
            PermMode::Question => {
                emit_question(&v);
                process::exit(0);
            }
            PermMode::Monitor => {} // pass-through → translate/emit ci-dessous (6.27 inchangé)
        }
    }

    // Event inconnu / non mappé / JSON invalide → `translate` retourne `None` : on sort proprement
    // sans écrire, sans panic (fail-open, ne bloque jamais Grok).
    if let Some(translated) = translate(&event, &input) {
        emit(&translated);
    }

    process::exit(0);
}

/// Reconnaît l'outil « question » quel que soit le style de nom émis par Grok : `ask_user_question`
/// (snake, spike 12/07) OU `AskQuestion` (PascalCase, e2e 13/07 — Grok a basculé sur les noms style
/// Claude). Insensible à la casse pour robustesse aux futures variantes.
fn is_ask_question_tool(tool_name: &str) -> bool {
    let n = tool_name.to_ascii_lowercase();
    n == "ask_user_question" || n == "askquestion" || n == "askuserquestion"
}

/// Classe un `pre_tool_use` Grok selon AC1. PURE (lit `permissionMode`, `toolName`, présence de
/// `subagentType`).
///
/// Précédence (garde sous-agent AC7 d'abord = jamais de prompt/PermissionRequest pour un
/// sous-agent, même s'il pose une question) :
///   1. `subagentType` présent → `Monitor` (auto-allow, on ne noie pas l'utilisateur — AC7).
///   2. `toolName == "ask_user_question"` → `Question` (affichage lecture-seule).
///   3. `permissionMode == "bypassPermissions"` → `Approve` : le hook est le seul portillon,
///      `allow`/`deny` à distance (validé e2e run 7, 2026-07-13).
///   4. `permissionMode == "default"` (ask) → `Inform` : l'`allow` ne bypass PAS le TUI (validé run
///      6) → ni approve ni deny depuis Vigil, juste awareness read-only + invite terminal.
///   5. tout le reste (acceptEdits/dontAsk/plan…) → `Monitor` (pass-through 6.27).
fn classify_pre_tool_use(v: &Value) -> PermMode {
    if v.get("subagentType").is_some() {
        return PermMode::Monitor;
    }
    let tool_name = v.get("toolName").and_then(|x| x.as_str()).unwrap_or_default();
    if is_ask_question_tool(tool_name) {
        return PermMode::Question;
    }
    let mode = v
        .get("permissionMode")
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    match mode {
        "bypassPermissions" => PermMode::Approve,
        "default" => PermMode::Inform,
        _ => PermMode::Monitor,
    }
}

/// Cas **Approve** (AC2) : synthétise un `PermissionRequest` interne (Grok n'a pas cet event),
/// l'écrit sur le pipe ET **BLOQUE** en lisant la réponse sur la MÊME connexion (schéma vigil-codex,
/// PAS `emit()` qui ouvre/ferme une connexion dédiée), puis mappe la décision Vigil vers le format
/// Grok sur stdout + code de sortie. Ne retourne jamais (termine le process).
///
/// Fail-open (pipe indisponible / écriture / lecture échouées) → `exit 0` sans stdout : Grok reprend
/// la main et prompte l'utilisateur dans sa TUI. On ne bloque JAMAIS Grok si Vigil est absent.
fn handle_approve(v: &Value) -> ! {
    let (session_id, transcript_path, cwd) = extract_common(v);
    let (tool_name, tool_input) = extract_tool(v);
    // `tool_name` reste le nom NATIF Grok (ex. `run_terminal_command`) — l'auto-permission et
    // l'affichage overlay sont agnostiques de l'outil (cf. Dev Notes).
    let payload = json!({
        "hook_event_name": "PermissionRequest",
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "tool_name": tool_name,
        "tool_input": tool_input,
    })
    .to_string();

    let payload = inject_parent_pid(&payload, PROCESS_NAME).unwrap_or(payload);
    let payload = inject_source_tool(&payload, SOURCE_TOOL);

    let Some(mut pipe) = open_vigil_pipe() else {
        process::exit(0);
    };
    if !write_payload(&mut pipe, &payload) {
        process::exit(0);
    }
    if let Some(response) = read_pipe_response(&mut pipe) {
        let (out, code) = convert_response_to_grok(&response);
        print!("{out}");
        let _ = std::io::stdout().flush();
        process::exit(code);
    }
    process::exit(0);
}

/// Cas **Inform** (mode `default`/ask) : le hook NE PEUT PAS approuver (l'`allow` ne bypass pas en
/// ask — validé e2e run 6) → on N'ATTEND PAS. Synthétise un `PermissionRequest` **read-only**
/// (`read_only: true` → le frontend masque les boutons + invite « réponds dans le terminal »),
/// `emit` fire-and-forget, puis l'appelant `exit(0)` : la TUI Grok gère approve/deny. L'agent
/// retombe en `Working` au `post_tool_use` (approbation) ou au `stop` (refus). Miroir non bloquant
/// de `handle_approve`, pour une permission d'outil réelle.
fn emit_permission_readonly(v: &Value) {
    let (session_id, transcript_path, cwd) = extract_common(v);
    let (tool_name, tool_input) = extract_tool(v);
    let payload = json!({
        "hook_event_name": "PermissionRequest",
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "tool_name": tool_name,
        "tool_input": tool_input,
        "read_only": true,
    })
    .to_string();
    emit(&payload);
}

/// Cas **Question** (AC8) : synthétise un `PermissionRequest`/`AskUserQuestion` NON bloquant
/// (`tool_name = "AskUserQuestion"` → déclenche `WaitingSpecific`) avec les questions **normalisées**
/// (`multi_select` → `multiSelect`), l'`emit` fire-and-forget, puis l'appelant `exit(0)` SANS lire le
/// pipe : la TUI Grok pose la question (le hook ne peut pas injecter la réponse — `updatedInput`
/// infirmé e2e run 4). Le `post_tool_use` de `ask_user_question` fera retomber l'agent en `Working`.
fn emit_question(v: &Value) {
    emit(&question_payload(v));
}

/// Payload `PermissionRequest`/`AskUserQuestion` — pur pour test (Story 8.4).
/// `read_only: true` : le hook a déjà `exit(0)` ; Vigil ne peut pas injecter la réponse.
fn question_payload(v: &Value) -> String {
    let (session_id, transcript_path, cwd) = extract_common(v);
    let questions = normalize_questions(v);
    json!({
        "hook_event_name": "PermissionRequest",
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "tool_name": "AskUserQuestion",
        "tool_input": { "questions": questions },
        "read_only": true,
    })
    .to_string()
}

/// Normalise `toolInput.questions[]` (schéma Grok) vers le schéma frontend `AskUserQuestion`
/// (`src/types/agent.ts`) : `multi_select` → `multiSelect` (défaut `false`), `question`/`options[]`
/// conservés tels quels, `header` omis s'il est absent (Grok ne le porte pas — le rendu le gère en
/// optionnel). Retourne un tableau JSON (vide si `questions` absent/mal formé).
fn normalize_questions(v: &Value) -> Value {
    let tool_input = v.get("toolInput");
    // `title` de niveau toolInput (schéma PascalCase 13/07) → header par défaut si la question n'en a pas.
    let title = tool_input.and_then(|ti| ti.get("title")).cloned();
    let questions = tool_input
        .and_then(|ti| ti.get("questions"))
        .and_then(|q| q.as_array())
        .cloned()
        .unwrap_or_default();

    let normalized: Vec<Value> = questions
        .iter()
        .map(|q| {
            let mut obj = serde_json::Map::new();
            // Texte de la question : `prompt` (schéma PascalCase 13/07) OU `question` (snake 12/07).
            // Défaut `""` (plutôt qu'omettre la clé) si aucun des deux n'est présent, pour rester
            // cohérent avec le contrat frontend (`AskUserQuestion.question: string`, requis).
            let question = q
                .get("prompt")
                .or_else(|| q.get("question"))
                .cloned()
                .unwrap_or_else(|| json!(""));
            obj.insert("question".to_string(), question);
            // `header` : per-question s'il existe, sinon le `title` du toolInput (sinon omis → optionnel).
            if let Some(header) = q.get("header").cloned().or_else(|| title.clone()) {
                obj.insert("header".to_string(), header);
            }
            // `options[]` : `label` présent dans les 2 schémas ; `id`/`description` tolérés (le frontend
            // lit `label`, ignore `id`, rend `description` optionnelle). Conservés tels quels, mais
            // uniquement si c'est bien un tableau JSON (défaut `[]` sinon, pour éviter un `.map()` frontend
            // sur une valeur non-itérable si Grok envoie un `options` malformé).
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            obj.insert("options".to_string(), json!(options));
            // `multi_select` (snake 12/07) / `multiSelect` (camel) → `multiSelect` ; absent en 13/07 = false.
            let multi = q
                .get("multi_select")
                .or_else(|| q.get("multiSelect"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            obj.insert("multiSelect".to_string(), json!(multi));
            Value::Object(obj)
        })
        .collect();

    json!(normalized)
}

/// Renormalise la réponse Vigil (format interne `hookSpecificOutput.decision.behavior`, cf.
/// `ALLOW_RESPONSE`/`DENY_RESPONSE` de `pipe/protocol.rs`) vers la décision Grok sur stdout + code
/// de sortie. Miroir de `convert_response_to_codex` (`vigil-codex/src/main.rs:76`). Fallback `deny`
/// (safe par défaut) si le parsing échoue.
fn convert_response_to_grok(vigil_response: &str) -> (String, i32) {
    let parsed: Value = serde_json::from_str(vigil_response.trim()).unwrap_or(Value::Null);
    let behavior = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("decision"))
        .and_then(|d| d.get("behavior"))
        .and_then(|b| b.as_str())
        .unwrap_or("deny");
    match behavior {
        "allow" => (GROK_ALLOW.to_string(), 0),
        _ => (GROK_DENY.to_string(), 2),
    }
}

/// Injecte `parent_pid` + `source_tool` puis écrit le payload sur une connexion pipe dédiée.
/// Best-effort : toute erreur (pipe indisponible, écriture) est silencieuse — ne jamais bloquer
/// la boucle de l'agent.
fn emit(payload: &str) {
    let payload = inject_parent_pid(payload, PROCESS_NAME).unwrap_or_else(|| payload.to_string());
    let payload = inject_source_tool(&payload, SOURCE_TOOL);
    if let Some(mut pipe) = open_vigil_pipe() {
        let _ = write_payload(&mut pipe, &payload);
    }
}

/// Lit `hookEventName` (valeur snake_case) du payload brut Grok. Vide si absent / JSON invalide.
fn grok_event_name(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("hookEventName")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Extrait les champs communs à tout payload Grok : `(session_id, transcript_path, cwd)`.
/// `sessionId → session_id`, `transcriptPath → transcript_path` (absent sur `session_start` →
/// vide), `cwd → cwd`. Champs absents = strings vides (robustesse).
fn extract_common(v: &serde_json::Value) -> (String, String, String) {
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (get("sessionId"), get("transcriptPath"), get("cwd"))
}

/// Extrait `(subagent_id, subagent_type)` d'un payload sous-agent Grok émis CÔTÉ PARENT
/// (`SubagentStart`/`SubagentEnd`, story 6.31). `subagentId` = **session enfant** (deviendra
/// `agent_id` interne = clé de l'ensemble anti-fantôme backend, DOIT égaler le `sessionId` que
/// l'enfant portera sur ses propres events). `subagentType` = type de sous-agent (présent sur
/// `subagent_start`, souvent absent sur `subagent_end`). Miroir de `extract_tool` ; défauts `""`.
fn extract_subagent(v: &serde_json::Value) -> (String, String) {
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (get("subagentId"), get("subagentType"))
}

/// Extrait `(tool_name, tool_input)` d'un payload d'outil Grok. `toolName → tool_name`,
/// `toolInput → tool_input` (objet copié tel quel ; absent → `{}`).
fn extract_tool(v: &serde_json::Value) -> (String, serde_json::Value) {
    let tool_name = v
        .get("toolName")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_input = v.get("toolInput").cloned().unwrap_or_else(|| json!({}));
    (tool_name, tool_input)
}

/// Couche de traduction **pure** (testable sans I/O réseau) : payload brut Grok (camelCase) → JSON
/// normalisé au format `HookEvent` (snake_case + `hook_event_name` PascalCase injecté), prêt à
/// écrire sur le pipe. `None` si l'event est inconnu, le JSON invalide, ou un `session_start`
/// impossible à ancrer (cwd/sessionId manquant → pas de clé de dédup fiable).
///
/// NB : le `source_tool` n'est PAS injecté ici — il l'est par `emit` (via `inject_source_tool`),
/// uniformément sur tous les events.
///
/// Un seul `match` fait à la fois la garde d'event connu/inconnu ET le choix du tag PascalCase —
/// contrairement à une garde séparée (`grok_event_to_pascal`) + un second `match` sur les mêmes 6
/// events, qui obligerait à maintenir deux énumérations synchronisées (piège pour les stories Grok
/// futures 6.28-6.33 qui ajouteront des events).
fn translate(event: &str, raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let (session_id, transcript_path, cwd) = extract_common(&v);

    // Story 6.34 (review) — garde d'ancrage UNIFORME. Tout event PORTÉ (≠ `session_start`, qui
    // SYNTHÉTISE son chemin plus bas) doit avoir un `transcript_path` non vide : c'est la clé de
    // dédup côté backend. Un chemin vide ferait créer au handler un agent fantôme clé "" (en
    // `Error` pour un `stop_failure`), qu'aucune garde ne rattrape (`is_grok_subagent_on_disk("")`
    // = false). Grok porte toujours le champ sur ces events → garde défensive, une seule fois pour
    // les 8 bras plutôt que dupliquée.
    if event != "session_start" && transcript_path.is_empty() {
        return None;
    }

    match event {
        "pre_tool_use" => {
            let (tool_name, tool_input) = extract_tool(&v);
            Some(
                json!({
                    "hook_event_name": "PreToolUse",
                    "session_id": session_id,
                    "transcript_path": transcript_path,
                    "cwd": cwd,
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                })
                .to_string(),
            )
        }
        "post_tool_use" => {
            let (tool_name, tool_input) = extract_tool(&v);
            // `tool_response` est REQUIS par `PostToolUsePayload` → `toolResult` sinon `{}`.
            let tool_response = v.get("toolResult").cloned().unwrap_or_else(|| json!({}));
            Some(
                json!({
                    "hook_event_name": "PostToolUse",
                    "session_id": session_id,
                    "transcript_path": transcript_path,
                    "cwd": cwd,
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                    "tool_response": tool_response,
                })
                .to_string(),
            )
        }
        // Story 6.37 — Échec d'appel d'outil. Calqué sur `post_tool_use` (AC2) : même `(tool_name,
        // tool_input)` extraction. La source d'info d'échec diffère selon l'outil (Claude/Codex : champ
        // `error` ; Antigravity : dérivé de `PostToolUse.error≠""` ; Grok : **`toolResult`**). Pour
        // Vigil, le champ `error: Option<String>` est non lu (`#[allow(dead_code)]`) — le handler socle
        // (state_engine.rs:127) n'en dépend pas, il utilise `tool_name`/`tool_input` pour le label
        // « Failed: … ». On mappe `toolResult` dans `error` par cohérence/futur ; absence d'`error` ne
        // bloque pas (`is_interrupt` omis → default false → branche label). **Pas de `is_interrupt`** :
        // Grok hors `INTERRUPT_SUPPORTED` (6.33), ne renseigne jamais le flag → chemin interrupt jamais
        // emprunté.
        "post_tool_use_failure" => {
            let (tool_name, tool_input) = extract_tool(&v);
            // Dériver `error` de `toolResult` (présent souvent, mais pas garanti). `error` est
            // `Option` côté Vigil (`#[serde(default)]` → une omission reste inerte) : mappé s'il
            // existe, omis sinon. Comme le bras jumeau `post_tool_use`, on préserve TOUTE forme :
            // `toolResult` peut être une string OU un JSON structuré (objet/nombre) selon l'outil ;
            // dans ce dernier cas on sérialise en texte plutôt que de laisser tomber l'info.
            let mut obj = json!({
                "hook_event_name": "PostToolUseFailure",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
                "tool_name": tool_name,
                "tool_input": tool_input,
            });
            if let Some(tr) = v.get("toolResult") {
                let err = tr.as_str().map(str::to_owned).unwrap_or_else(|| tr.to_string());
                obj["error"] = json!(err);
            }
            Some(obj.to_string())
        }
        "user_prompt_submit" => {
            let mut obj = json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
            });
            // `prompt` est `Option` côté payload (et non lu aujourd'hui) → mappé s'il existe, omis
            // sinon (pas de valeur bidon).
            if let Some(prompt) = v.get("prompt").and_then(|x| x.as_str()) {
                obj["prompt"] = json!(prompt);
            }
            Some(obj.to_string())
        }
        "stop" => Some(
            json!({
                "hook_event_name": "Stop",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
            })
            .to_string(),
        ),
        "session_end" => {
            let mut obj = json!({
                "hook_event_name": "SessionEnd",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
            });
            if let Some(reason) = v.get("reason").and_then(|x| x.as_str()) {
                obj["reason"] = json!(reason);
            }
            Some(obj.to_string())
        }
        // Story 6.34 — État & erreurs. Réutilisent les handlers socle (agnostiques) sans code neuf.
        // `notification` = annotatif (met à jour `last_activity_label` sans changer l'état) ;
        // `stop_failure` = force `AgentState::Error`. `transcript_path` porté tel quel (présent, ≠
        // session_start → PAS de synthèse). Un `notification`/`stop_failure` de SOUS-AGENT (session_id
        // enfant) est supprimé en amont par la garde 6.31 (`is_grok_subagent_phantom`).
        "notification" => {
            let mut obj = json!({
                "hook_event_name": "Notification",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
                // `message` est REQUIS par `NotificationPayload` → toujours présent, défaut "".
                "message": v.get("message").and_then(|x| x.as_str()).unwrap_or_default(),
            });
            // `notificationType` → `notification_type` (Option côté payload) : mappé s'il existe,
            // omis sinon (aucun filtrage sur la valeur — le handler est purement annotatif).
            if let Some(nt) = v.get("notificationType").and_then(|x| x.as_str()) {
                obj["notification_type"] = json!(nt);
            }
            Some(obj.to_string())
        }
        "stop_failure" => {
            let mut obj = json!({
                "hook_event_name": "StopFailure",
                "session_id": session_id,
                "transcript_path": transcript_path,
                "cwd": cwd,
            });
            // `error` (texte complet Grok, ex. « API error status 429 rate-limit ») → `error_type`
            // (Option) : mappé s'il existe, omis sinon. `promptId` NON mappé (non lu par le payload).
            if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
                obj["error_type"] = json!(err);
            }
            Some(obj.to_string())
        }
        // Story 6.31 — sous-agents, émis CÔTÉ PARENT. `subagentId` (= session enfant) → `agent_id`
        // interne ; `transcriptPath` porté = celui du PARENT (≠ session_start) → `find_parent_key`
        // matche par clé directe (comme Claude). `session_id` reste = parent (extract_common) → il
        // n'est JAMAIS dans l'ensemble anti-fantôme backend, donc ces events ne sont pas supprimés.
        "subagent_start" => {
            let (sub_id, sub_type) = extract_subagent(&v);
            // Défensif (défer 6.31) : sans `subagentId`, la session enfant n'est PAS enregistrée dans
            // l'ensemble anti-fantôme (`agent_id` vide → le handler saute l'insertion) → les events de
            // l'enfant créeraient un agent de 1er niveau non supprimé. Grok porte toujours le champ
            // (spike) → garde défensive : pas de clé enfant fiable → ne pas émettre.
            if sub_id.is_empty() {
                return None;
            }
            Some(
                json!({
                    "hook_event_name": "SubagentStart",
                    "session_id": session_id,        // = parent
                    "transcript_path": transcript_path, // = parent (présent, ≠ session_start)
                    "cwd": cwd,
                    "agent_id": sub_id,              // = subagentId = session enfant
                    "agent_type": sub_type,
                })
                .to_string(),
            )
        }
        // ⚠️ Clé de hook Grok = `SubagentEnd`, mais le payload stdin porte `hookEventName ==
        // "subagent_end"` (incohérence interne Grok, spike 2026-07-12) → on matche `subagent_end`,
        // JAMAIS `subagent_stop`. Traduit vers la variante INTERNE `SubagentStop` (le handler
        // `SubagentStop` retrouve le sous-agent par `agent_id`, `agent_type` non lu → défaut `""`).
        "subagent_end" => {
            let (sub_id, sub_type) = extract_subagent(&v);
            // Symétrique de `subagent_start` : sans `subagentId`, le handler `SubagentStop` ne peut
            // retrouver aucun sous-agent (`.find(|s| s.agent_id == …)` échoue) → event no-op. On ne
            // l'émet pas (hygiène ; Grok porte toujours le champ).
            if sub_id.is_empty() {
                return None;
            }
            Some(
                json!({
                    "hook_event_name": "SubagentStop",
                    "session_id": session_id,        // = parent
                    "transcript_path": transcript_path, // = parent
                    "cwd": cwd,
                    "agent_id": sub_id,              // = subagentId = session enfant
                    "agent_type": sub_type,          // défaut "" si absent (non utilisé par le handler)
                })
                .to_string(),
            )
        }
        "session_start" => {
            // Grok n'envoie PAS de `transcriptPath` sur `session_start` → on le SYNTHÉTISE. Si
            // cwd/sessionId manquent, pas de clé fiable → ne PAS émettre (retour `None`) : le 1er
            // event réel créera l'agent avec le vrai `transcriptPath` (voir repli Risque #1).
            let synth = grok_transcript_path(&cwd, &session_id)?;
            Some(
                json!({
                    "hook_event_name": "SessionStart",
                    "session_id": session_id,
                    "transcript_path": synth,
                    "cwd": cwd,
                    // Pas de `model` : badge modèle Grok déféré 6.28 (absent du payload de hook, AC5).
                })
                .to_string(),
            )
        }
        // Event Grok inconnu / non supporté (ex. un futur event ajouté par Grok avant que Vigil
        // ne le supporte) → sortie propre, fail-open.
        _ => None,
    }
}

/// Synthétise le `transcript_path` d'une session Grok à partir de `(cwd, session_id)`, en résolvant
/// le home réel de l'utilisateur. Voir `grok_transcript_path_in` pour le schéma exact.
/// `None` si le home est introuvable ou si `cwd`/`session_id` est vide.
fn grok_transcript_path(cwd: &str, session_id: &str) -> Option<String> {
    let home = grok_home()?;
    grok_transcript_path_in(&home, cwd, session_id)
}

/// Cœur PUR de la synthèse (home injecté → testable byte-exact sans dépendre de l'environnement).
///
/// Grok stocke chaque session sous `<home>\.grok\sessions\<urlencode(cwd)>\<sessionId>\updates.jsonl`
/// (chemin absolu, séparateurs backslash natifs Windows — confirmé sur disque : `cwd`
/// `C:\Users\Dev` → groupe `C%3A%5CUsers%5CDev`, `grok_home` = `C:\Users\Dev\.grok`).
///
/// ⚠️ RISQUE #1 : cette valeur DOIT byte-matcher le `transcriptPath` que Grok place dans les events
/// suivants (le moteur dédup sur `transcript_path`). Preuve byte-exacte finale = e2e AC7.
///
/// `None` si `cwd` ou `session_id` est vide (pas de clé fiable → on n'émet pas de `SessionStart`).
///
/// `cwd` est normalisé (slash final retiré, sauf racine de lecteur `C:\`) avant encodage, en
/// symétrie avec `home` : un `cwd` porté avec ou sans séparateur final par un event ultérieur ne
/// doit pas produire un groupe encodé différent (cf. defer Risque #1 — dimension non capturée en
/// e2e, normalisée par précaution plutôt que laissée dépendre de la forme exacte reçue).
fn grok_transcript_path_in(home: &str, cwd: &str, session_id: &str) -> Option<String> {
    if cwd.is_empty() || session_id.is_empty() {
        return None;
    }
    let home = home.trim_end_matches(['\\', '/']);
    let cwd = normalize_cwd(cwd);
    let encoded = urlencode_grok(&cwd);
    Some(format!(
        r"{home}\.grok\sessions\{encoded}\{session_id}\updates.jsonl"
    ))
}

/// Retire un séparateur final de `cwd`, sauf pour une racine de lecteur (`C:\` doit rester
/// `C:\`, sinon `C:` seul est un chemin relatif Windows différent). Sans effet sur un `cwd` déjà
/// sans slash final (cas nominal observé et validé e2e AC7).
fn normalize_cwd(cwd: &str) -> String {
    if cwd.len() > 3 {
        cwd.trim_end_matches(['\\', '/']).to_string()
    } else {
        cwd.to_string()
    }
}

/// Résout le répertoire home de l'utilisateur (aligné sur `config/mod.rs::home_dir`). `None` si
/// aucune variable d'environnement exploitable (cas dégénéré : on n'émet alors pas de SessionStart).
fn grok_home() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(format!("{drive}{path}"))
        })
        .filter(|s| !s.is_empty())
}

/// Encode `s` façon URL comme Grok nomme ses dossiers de session : tout octet hors du jeu
/// « unreserved » RFC 3986 (`A-Z a-z 0-9 - _ . ~`) devient `%XX` en hexadécimal MAJUSCULE.
/// Reproduit le schéma observé sur disque : `C:\Users\Dev` → `C%3A%5CUsers%5CDev`
/// (`:` → `%3A`, `\` → `%5C`). Implémentation locale minimale (pas de dépendance ajoutée).
fn urlencode_grok(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// Tests — sur payloads Grok réalistes calqués sur le spike 2026-07-11 (clés camelCase, valeurs
// snake_case). Comme la crate binaire ne peut pas importer `HookEvent` (défini dans src-tauri), on
// valide la traduction en asserant sur les champs JSON produits. La preuve de désérialisation dans
// `HookEvent` vit côté src-tauri (`pipe/protocol.rs` : `grok_*_normalise_deserialise`).
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Cas de référence (poste Steven, confirmé sur disque). Sert au test de correspondance T3.
    const HOME: &str = r"C:\Users\Dev";
    const SESSION: &str = "019f5157-76b1-74c3-a8a4-7df953935f21";
    // `transcriptPath` réel PRÉSUMÉ (forme disque : backslash absolu). La preuve byte-exacte finale
    // que Grok émet bien cette forme dans le payload de hook = e2e AC7 (Risque #1).
    const REAL_TRANSCRIPT: &str = r"C:\Users\Dev\.grok\sessions\C%3A%5CUsers%5CDev\019f5157-76b1-74c3-a8a4-7df953935f21\updates.jsonl";

    // ── 6 payloads Grok (camelCase / valeurs snake) ──────────────────────
    // `cwd` en backslash (forme réelle du champ, cf. summary.json). `transcriptPath` présent sur
    // tous SAUF `session_start`.
    const SESSION_START: &str = r#"{"hookEventName":"session_start","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","permissionMode":"always-approve","workspaceRoot":"C:\\Users\\Dev","timestamp":"2026-07-11T13:21:54+00:00"}"#;
    const USER_PROMPT_SUBMIT: &str = r#"{"hookEventName":"user_prompt_submit","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","prompt":"liste le dossier"}"#;
    const PRE_TOOL_USE: &str = r#"{"hookEventName":"pre_tool_use","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","toolName":"run_terminal_command","toolInput":{"command":"dir"},"toolUseId":"call-123","permissionMode":"always-approve"}"#;
    const POST_TOOL_USE: &str = r#"{"hookEventName":"post_tool_use","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","toolName":"run_terminal_command","toolInput":{"command":"dir"},"toolResult":{"output":"file1"}}"#;
    const STOP: &str = r#"{"hookEventName":"stop","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","reason":"end_turn"}"#;
    const SESSION_END: &str = r#"{"hookEventName":"session_end","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","reason":"quit"}"#;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("translate doit produire du JSON valide")
    }

    // ── grok_event_name ───────────────────────────────────────────────────
    // Le mapping snake_case → PascalCase est vérifié event par event dans les tests `translate()`
    // ci-dessous (chaque test asserte `hook_event_name`) — plus de fonction séparée à tester
    // (fusionnée dans `translate()`, cf. commentaire au-dessus de `translate`).
    #[test]
    fn event_name_extrait_du_payload() {
        assert_eq!(grok_event_name(PRE_TOOL_USE), "pre_tool_use");
        assert_eq!(grok_event_name(SESSION_START), "session_start");
        assert_eq!(grok_event_name("pas du json"), "");
    }

    // ── urlencode (schéma Grok) ──────────────────────────────────────────
    #[test]
    fn urlencode_reproduit_le_schema_grok() {
        assert_eq!(urlencode_grok(r"C:\Users\Dev"), "C%3A%5CUsers%5CDev");
        // Jeu unreserved RFC 3986 préservé, casse hexa majuscule.
        assert_eq!(urlencode_grok("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencode_grok("a b"), "a%20b");
    }

    // ── AC1 : pre_tool_use ───────────────────────────────────────────────
    #[test]
    fn pre_tool_use_mappings_complets() {
        let out = translate("pre_tool_use", PRE_TOOL_USE).expect("PreToolUse doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["session_id"], SESSION);
        assert_eq!(v["transcript_path"], REAL_TRANSCRIPT);
        assert_eq!(v["cwd"], r"C:\Users\Dev");
        assert_eq!(v["tool_name"], "run_terminal_command");
        assert_eq!(v["tool_input"]["command"], "dir");
    }

    // ── AC1 : post_tool_use (tool_response requis) ───────────────────────
    #[test]
    fn post_tool_use_porte_tool_response() {
        let out = translate("post_tool_use", POST_TOOL_USE).expect("PostToolUse doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["tool_name"], "run_terminal_command");
        assert_eq!(v["tool_response"]["output"], "file1"); // toolResult → tool_response
    }

    #[test]
    fn post_tool_use_sans_tool_result_defaut_objet_vide() {
        let raw = r#"{"hookEventName":"post_tool_use","sessionId":"s","cwd":"C:\\x","transcriptPath":"/t","toolName":"write","toolInput":{}}"#;
        let v = parse(&translate("post_tool_use", raw).unwrap());
        assert_eq!(v["tool_response"], json!({})); // requis par PostToolUsePayload
    }

    // ── AC1 : user_prompt_submit ─────────────────────────────────────────
    #[test]
    fn user_prompt_submit_mappe_le_prompt() {
        let v = parse(&translate("user_prompt_submit", USER_PROMPT_SUBMIT).unwrap());
        assert_eq!(v["hook_event_name"], "UserPromptSubmit");
        assert_eq!(v["session_id"], SESSION);
        assert_eq!(v["prompt"], "liste le dossier");
    }

    #[test]
    fn user_prompt_submit_sans_prompt_omet_le_champ() {
        let raw = r#"{"hookEventName":"user_prompt_submit","sessionId":"s","cwd":"C:\\x","transcriptPath":"/t"}"#;
        let v = parse(&translate("user_prompt_submit", raw).unwrap());
        assert!(v.get("prompt").is_none(), "prompt absent → omis (pas de valeur bidon)");
    }

    // ── AC4 : stop / session_end ─────────────────────────────────────────
    #[test]
    fn stop_mappings_communs() {
        let v = parse(&translate("stop", STOP).unwrap());
        assert_eq!(v["hook_event_name"], "Stop");
        assert_eq!(v["session_id"], SESSION);
        assert_eq!(v["transcript_path"], REAL_TRANSCRIPT);
        assert_eq!(v["cwd"], r"C:\Users\Dev");
    }

    #[test]
    fn session_end_traduit_avec_reason() {
        let v = parse(&translate("session_end", SESSION_END).unwrap());
        assert_eq!(v["hook_event_name"], "SessionEnd");
        assert_eq!(v["reason"], "quit");
    }

    // ── AC1 : event inconnu / JSON invalide → None ───────────────────────
    #[test]
    fn event_inconnu_retourne_none() {
        // `subagent_start`/`subagent_end` sont désormais traduits (story 6.31) → on teste un event
        // réellement inconnu + l'event vide.
        assert!(translate("un_event_du_futur", PRE_TOOL_USE).is_none());
        assert!(translate("", PRE_TOOL_USE).is_none());
    }

    #[test]
    fn json_invalide_retourne_none() {
        assert!(translate("pre_tool_use", "pas du json {{{").is_none());
    }

    // ── Story 6.31 : sous-agents (subagent_start / subagent_end, parent-side) ──────
    const SUBAGENT_START: &str = r#"{"hookEventName":"subagent_start","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","subagentId":"019f9999-child-session","subagentType":"Explore","description":"cherche les usages"}"#;
    const SUBAGENT_END: &str = r#"{"hookEventName":"subagent_end","sessionId":"019f5157-76b1-74c3-a8a4-7df953935f21","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl","subagentId":"019f9999-child-session","exitCode":0,"durationMs":4200}"#;

    #[test]
    fn subagent_start_mappe_vers_parent_side() {
        // AC2 : subagentId → agent_id (session enfant), transcriptPath/session_id = parent,
        // subagentType → agent_type, hook_event_name PascalCase interne "SubagentStart".
        let v = parse(&translate("subagent_start", SUBAGENT_START).expect("SubagentStart forwardé"));
        assert_eq!(v["hook_event_name"], "SubagentStart");
        assert_eq!(v["session_id"], SESSION); // = parent
        assert_eq!(v["transcript_path"], REAL_TRANSCRIPT); // = parent
        assert_eq!(v["cwd"], r"C:\Users\Dev");
        assert_eq!(v["agent_id"], "019f9999-child-session"); // = subagentId
        assert_eq!(v["agent_type"], "Explore");
    }

    #[test]
    fn subagent_end_mappe_vers_subagent_stop_interne() {
        // AC3 : hookEventName "subagent_end" → variante INTERNE "SubagentStop" (JAMAIS subagent_stop).
        let v = parse(&translate("subagent_end", SUBAGENT_END).expect("SubagentEnd forwardé"));
        assert_eq!(v["hook_event_name"], "SubagentStop");
        assert_eq!(v["session_id"], SESSION); // = parent
        assert_eq!(v["transcript_path"], REAL_TRANSCRIPT); // = parent
        assert_eq!(v["agent_id"], "019f9999-child-session"); // = subagentId
    }

    #[test]
    fn subagent_end_sans_subagent_type_defaut_chaine_vide() {
        // AC3 : `subagentType` absent (cas nominal du subagent_end) → agent_type == "".
        let v = parse(&translate("subagent_end", SUBAGENT_END).unwrap());
        assert_eq!(v["agent_type"], "", "subagentType absent → agent_type défaulté à \"\"");
    }

    #[test]
    fn subagent_stop_nest_pas_un_event_grok_valide() {
        // Filet anti-régression du piège nommage : la clé/tag "subagent_stop" (réflexe Claude) n'est
        // PAS reconnue par translate → None. Seul "subagent_end" (tag stdin réel Grok) est mappé.
        assert!(translate("subagent_stop", SUBAGENT_END).is_none());
    }

    // ── Story 6.34 : notification / stop_failure (état & erreurs) ─────────────────
    // Fixtures = payloads RÉELS de la capture `events-run2.jsonl` (2026-07-12) : notification
    // `agent_error` (message 429 complet) + `idle_prompt` ("Turn complete") + stop_failure
    // (`error` = texte complet 429). `transcript_path` porté tel quel (≠ session_start → pas de synthèse).
    const NOTIFICATION_AGENT_ERROR: &str = r#"{"hookEventName":"notification","sessionId":"019f5738-76b1-74c3-a8a4-7df95393e43dd","cwd":"C:\\Users\\Dev","workspaceRoot":"C:\\Users\\Dev","timestamp":"2026-07-12T10:00:00+00:00","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-76b1-74c3-a8a4-7df95393e43dd\\updates.jsonl","notificationType":"agent_error","message":"Rate limited: \"API error (status 429 Too Many Requests)\"","level":"error"}"#;
    const NOTIFICATION_IDLE_PROMPT: &str = r#"{"hookEventName":"notification","sessionId":"019f5728-76b1-74c3-a8a4-7df9539d9c45","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5728-76b1-74c3-a8a4-7df9539d9c45\\updates.jsonl","notificationType":"idle_prompt","message":"Turn complete","level":"info"}"#;
    const STOP_FAILURE: &str = r#"{"hookEventName":"stop_failure","sessionId":"019f5738-76b1-74c3-a8a4-7df95393e43dd","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-76b1-74c3-a8a4-7df95393e43dd\\updates.jsonl","promptId":"prompt-42","error":"Rate limited: \"API error (status 429 Too Many Requests): please retry\""}"#;

    #[test]
    fn notification_mappe_message_et_notification_type() {
        // AC2 : notificationType → notification_type ; message présent ; transcript_path tel quel.
        let v = parse(&translate("notification", NOTIFICATION_AGENT_ERROR).expect("Notification forwardée"));
        assert_eq!(v["hook_event_name"], "Notification");
        assert_eq!(v["session_id"], "019f5738-76b1-74c3-a8a4-7df95393e43dd");
        assert!(v["message"].as_str().unwrap().contains("429"));
        assert_eq!(v["notification_type"], "agent_error");
        // transcript_path porté tel quel (pas de synthèse ≠ session_start).
        assert!(v["transcript_path"].as_str().unwrap().ends_with(r"\updates.jsonl"));
    }

    #[test]
    fn notification_idle_prompt_passe_sans_filtrage() {
        // AC2 : ZÉRO filtrage sur notificationType → idle_prompt ("Turn complete") passe aussi.
        let v = parse(&translate("notification", NOTIFICATION_IDLE_PROMPT).unwrap());
        assert_eq!(v["message"], "Turn complete");
        assert_eq!(v["notification_type"], "idle_prompt");
    }

    #[test]
    fn notification_sans_message_defaut_chaine_vide() {
        // AC2 : `message` REQUIS par NotificationPayload → défaut "" si absent (jamais omis).
        let raw = r#"{"hookEventName":"notification","sessionId":"s","cwd":"C:\\x","transcriptPath":"/t","notificationType":"idle_prompt"}"#;
        let v = parse(&translate("notification", raw).unwrap());
        assert_eq!(v["message"], "", "message absent → défaut \"\" (requis)");
    }

    #[test]
    fn notification_sans_notification_type_omet_le_champ() {
        // AC2 : notificationType absent → notification_type omis (Option côté payload, pas de null bidon).
        let raw = r#"{"hookEventName":"notification","sessionId":"s","cwd":"C:\\x","transcriptPath":"/t","message":"coucou"}"#;
        let v = parse(&translate("notification", raw).unwrap());
        assert_eq!(v["message"], "coucou");
        assert!(v.get("notification_type").is_none(), "notificationType absent → champ omis");
    }

    #[test]
    fn stop_failure_mappe_error_vers_error_type() {
        // AC3 : error (texte complet) → error_type ; promptId NON mappé.
        let v = parse(&translate("stop_failure", STOP_FAILURE).expect("StopFailure forwardée"));
        assert_eq!(v["hook_event_name"], "StopFailure");
        assert_eq!(v["session_id"], "019f5738-76b1-74c3-a8a4-7df95393e43dd");
        assert!(v["error_type"].as_str().unwrap().contains("429"));
        assert!(v.get("prompt_id").is_none(), "promptId non mappé");
        assert!(v.get("promptId").is_none(), "promptId non mappé");
    }

    #[test]
    fn stop_failure_sans_error_omet_error_type() {
        // AC3 : error absent → error_type omis (Option). Le handler force quand même Error (badge sans message).
        let raw = r#"{"hookEventName":"stop_failure","sessionId":"s","cwd":"C:\\x","transcriptPath":"/t","promptId":"p1"}"#;
        let v = parse(&translate("stop_failure", raw).unwrap());
        assert_eq!(v["hook_event_name"], "StopFailure");
        assert!(v.get("error_type").is_none(), "error absent → error_type omis");
    }

    // ── Story 6.37 : post_tool_use_failure (échec d'appel d'outil) ─────────────────
    // Fixture = payload RÉEL d'un outil échoué chez Grok. Différence clé avec post_tool_use : l'info
    // d'échec est portée par `toolResult` (pas de champ `error` natif comme Claude). Le handler socle
    // (state_engine.rs:127) n'utilise que `tool_name`/`tool_input` pour le label « Failed: … » ; l'`error`
    // est `#[allow(dead_code)]` mais on le mappe pour cohérence/futur. `is_interrupt` omis → default false
    // → branche label (jamais interrupt pour Grok, hors INTERRUPT_SUPPORTED 6.33).
    const POST_TOOL_USE_FAILURE_WITH_RESULT: &str = r#"{"hookEventName":"post_tool_use_failure","sessionId":"019f5738-76b1-74c3-a8a4-7df95393e43dd","cwd":"C:\\Users\\Dev","workspaceRoot":"C:\\Users\\Dev","timestamp":"2026-07-17T10:00:00+00:00","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-76b1-74c3-a8a4-7df95393e43dd\\updates.jsonl","toolName":"run_terminal_command","toolUseId":"tool-123","toolInput":{"command":"exit 1"},"toolResult":"Command failed with exit code 1"}"#;
    const POST_TOOL_USE_FAILURE_NO_RESULT: &str = r#"{"hookEventName":"post_tool_use_failure","sessionId":"019f5738-76b1-74c3-a8a4-7df95393e43dd","cwd":"C:\\Users\\Dev","transcriptPath":"C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-76b1-74c3-a8a4-7df95393e43dd\\updates.jsonl","toolName":"write","toolUseId":"tool-456","toolInput":{"path":"/tmp/file"}}"#;

    #[test]
    fn post_tool_use_failure_mappe_tool_name_et_input() {
        // AC2 : toolName → tool_name, toolInput → tool_input ; transcript_path porté tel quel.
        let v = parse(&translate("post_tool_use_failure", POST_TOOL_USE_FAILURE_WITH_RESULT)
            .expect("PostToolUseFailure forwardée"));
        assert_eq!(v["hook_event_name"], "PostToolUseFailure");
        assert_eq!(v["session_id"], "019f5738-76b1-74c3-a8a4-7df95393e43dd");
        assert_eq!(v["tool_name"], "run_terminal_command");
        assert_eq!(v["tool_input"]["command"], "exit 1");
    }

    #[test]
    fn post_tool_use_failure_derive_error_de_tool_result() {
        // AC2 : toolResult → error (dérivation). Si présent, le `toolResult` string devient `error`.
        let v = parse(&translate("post_tool_use_failure", POST_TOOL_USE_FAILURE_WITH_RESULT).unwrap());
        assert_eq!(v["error"], "Command failed with exit code 1", "toolResult → error");
    }

    #[test]
    fn post_tool_use_failure_sans_tool_result_omet_error() {
        // AC2 : toolResult absent → error omis (Option, pas de null bidon). Handler ne s'en sert pas
        // (non lu) mais on l'omet pour propreté.
        let v = parse(&translate("post_tool_use_failure", POST_TOOL_USE_FAILURE_NO_RESULT).unwrap());
        assert_eq!(v["hook_event_name"], "PostToolUseFailure");
        assert_eq!(v["tool_name"], "write");
        assert!(v.get("error").is_none(), "toolResult absent → error omis");
    }

    #[test]
    fn post_tool_use_failure_pas_interrupt_support() {
        // AC3 : is_interrupt toujours false pour Grok (omis → default serde false). Le payload Grok
        // ne porte jamais ce champ → on ne le renseigne pas → la désérialisation apporte le défaut.
        // Vérif : pas de clé "is_interrupt" dans l'output traduit (elle est renseignée à l'émission par
        // `inject_*`, jamais par translate).
        let v = parse(&translate("post_tool_use_failure", POST_TOOL_USE_FAILURE_WITH_RESULT).unwrap());
        assert!(v.get("is_interrupt").is_none(), "is_interrupt non renseigné par translate");
    }

    #[test]
    fn post_tool_use_failure_anti_regression_vs_post_tool_use() {
        // Piège : post_tool_use a "tool_response" (le toolResult brut) ; post_tool_use_failure a "error"
        // (dérivé). Ne pas confondre les deux.
        let post_use = parse(&translate("post_tool_use", POST_TOOL_USE_FAILURE_WITH_RESULT).unwrap());
        assert!(post_use.get("tool_response").is_some(), "post_tool_use porte tool_response");
        assert!(post_use.get("error").is_none(), "post_tool_use n'a pas error");

        let failure = parse(&translate("post_tool_use_failure", POST_TOOL_USE_FAILURE_WITH_RESULT).unwrap());
        assert!(failure.get("error").is_some(), "post_tool_use_failure porte error");
        assert!(failure.get("tool_response").is_none(), "post_tool_use_failure n'a pas tool_response");
    }

    #[test]
    fn subagent_sans_subagent_id_renvoie_none() {
        // Défer 6.31 (quick win) : sans `subagentId`, la session enfant ne peut pas être enregistrée
        // dans l'ensemble anti-fantôme → ne pas émettre l'event (subagent_start ET subagent_end).
        let start = r#"{"hookEventName":"subagent_start","sessionId":"parent","cwd":"C:\\x","transcriptPath":"/parent","subagentType":"Explore"}"#;
        assert!(translate("subagent_start", start).is_none(), "subagent_start sans subagentId → None");
        let end = r#"{"hookEventName":"subagent_end","sessionId":"parent","cwd":"C:\\x","transcriptPath":"/parent"}"#;
        assert!(translate("subagent_end", end).is_none(), "subagent_end sans subagentId → None");
        // Sanity : avec un subagentId, l'event est bien émis.
        let ok = r#"{"hookEventName":"subagent_start","sessionId":"parent","cwd":"C:\\x","transcriptPath":"/parent","subagentId":"child-1","subagentType":"Explore"}"#;
        assert!(translate("subagent_start", ok).is_some(), "subagent_start avec subagentId → émis");
    }

    #[test]
    fn transcript_path_vide_sur_event_porte_renvoie_none() {
        // Défer 2 (review 6.34) : garde d'ancrage uniforme. Un event PORTÉ sans `transcriptPath` ne
        // doit PAS être traduit (sinon fantôme backend clé ""). Vaut pour tous les bras ≠ session_start.
        let notif = r#"{"hookEventName":"notification","sessionId":"s","cwd":"C:\\x","message":"x"}"#;
        assert!(translate("notification", notif).is_none(), "notification sans transcript_path → None");
        let sf = r#"{"hookEventName":"stop_failure","sessionId":"s","cwd":"C:\\x","error":"boom"}"#;
        assert!(translate("stop_failure", sf).is_none(), "stop_failure sans transcript_path → None");
        let stop = r#"{"hookEventName":"stop","sessionId":"s","cwd":"C:\\x"}"#;
        assert!(translate("stop", stop).is_none(), "stop sans transcript_path → None (garde uniforme)");
    }

    // ── AC2 : synthèse session_start + TEST DE CORRESPONDANCE (le plus important) ──
    #[test]
    fn synthese_transcript_path_correspond_au_reel() {
        // La synthèse (cwd + sessionId) doit reproduire EXACTEMENT le transcriptPath réel des
        // events suivants → un seul agent dans le moteur (pas de fantôme). Risque #1.
        let synth = grok_transcript_path_in(HOME, r"C:\Users\Dev", SESSION)
            .expect("synthèse possible");
        assert_eq!(synth, REAL_TRANSCRIPT);

        // Et il doit égaler le transcriptPath porté par un pre_tool_use réel de la même session.
        let real = parse(PRE_TOOL_USE)["transcriptPath"].as_str().unwrap().to_string();
        assert_eq!(synth, real, "Risque #1 : synthèse != transcriptPath réel → double agent");
    }

    #[test]
    fn session_start_synthetise_le_transcript_path() {
        // translate résout le home réel de l'env de test ; on vérifie le suffixe déterministe
        // (le préfixe home varie selon la machine).
        let v = parse(&translate("session_start", SESSION_START).unwrap());
        assert_eq!(v["hook_event_name"], "SessionStart");
        assert_eq!(v["session_id"], SESSION);
        assert!(v.get("model").is_none(), "pas de model en 6.27 (AC5)");
        let tp = v["transcript_path"].as_str().unwrap();
        assert!(
            tp.ends_with(r"\.grok\sessions\C%3A%5CUsers%5CDev\019f5157-76b1-74c3-a8a4-7df953935f21\updates.jsonl"),
            "suffixe de synthèse inattendu: {tp}"
        );
    }

    #[test]
    fn session_start_sans_cwd_ou_session_non_emis() {
        // Pas de clé fiable → None (pas d'agent fantôme). Le 1er event réel créera l'agent.
        assert!(translate("session_start", r#"{"hookEventName":"session_start"}"#).is_none());
        assert!(grok_transcript_path_in(HOME, "", SESSION).is_none());
        assert!(grok_transcript_path_in(HOME, r"C:\x", "").is_none());
    }

    #[test]
    fn home_avec_slash_final_est_normalise() {
        // Robustesse : un home avec séparateur final ne double pas le backslash.
        let a = grok_transcript_path_in(r"C:\Users\Dev", "cwd", "s").unwrap();
        let b = grok_transcript_path_in(r"C:\Users\Dev\", "cwd", "s").unwrap();
        assert_eq!(a, b);
    }

    // ── Durcissement Risque #1 (review 2026-07-11, décision Steven) ─────────
    // Dimensions non couvertes par l'e2e AC7 (poste Steven = cwd ASCII sans slash final,
    // USERPROFILE standard). Verrouille le comportement CONNU de l'implémentation ; ne prouve
    // toujours pas que Grok encode de la même façon sur ces dimensions (aucune capture
    // disponible) — la casse/profil redirigé reste hors de portée de vigil-grok seul (dédup
    // state_engine.rs par chaîne exacte, hors scope 6.27).

    #[test]
    fn cwd_avec_slash_final_produit_le_meme_groupe_encode() {
        // Un cwd porté avec ou sans séparateur final par des events différents d'une même
        // session ne doit pas faire dériver deux groupes encodés distincts.
        let sans_slash = grok_transcript_path_in(HOME, r"C:\Users\Dev", SESSION).unwrap();
        let avec_slash = grok_transcript_path_in(HOME, r"C:\Users\Dev\", SESSION).unwrap();
        assert_eq!(sans_slash, avec_slash);
        assert_eq!(sans_slash, REAL_TRANSCRIPT);
    }

    #[test]
    fn cwd_racine_de_lecteur_conserve_son_slash() {
        // "C:" seul (sans slash) est un chemin RELATIF au lecteur courant sous Windows, distinct
        // de "C:\" (racine absolue) — ne pas trimmer le slash d'une racine de lecteur.
        let racine = grok_transcript_path_in(HOME, r"C:\", SESSION).unwrap();
        let attendu = format!(r"{HOME}\.grok\sessions\C%3A%5C\{SESSION}\updates.jsonl");
        assert_eq!(racine, attendu);
        assert_eq!(urlencode_grok(r"C:\"), "C%3A%5C");
        assert_eq!(urlencode_grok("C:"), "C%3A");
    }

    #[test]
    fn urlencode_non_ascii_verrouille_le_comportement_actuel() {
        // RFC 3986 par octet UTF-8 : chaque octet hors du jeu unreserved devient %XX. Verrouille
        // la valeur produite en régression ; ne prouve PAS que Grok encode pareil sur un cwd/nom
        // d'utilisateur non-ASCII réel (aucune capture disponible sur cette dimension — risque
        // résiduel documenté, cf. Review Findings de la story).
        assert_eq!(urlencode_grok("é"), "%C3%A9"); // 'é' = 2 octets UTF-8 (0xC3 0xA9)
        assert_eq!(urlencode_grok(r"C:\Users\Stève"), "C%3A%5CUsers%5CSt%C3%A8ve");
        assert_eq!(urlencode_grok("Пётр"), "%D0%9F%D1%91%D1%82%D1%80");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Story 6.30 — permissions approve/deny
    // ─────────────────────────────────────────────────────────────────────

    // ── AC1 : classify_pre_tool_use ──────────────────────────────────────
    #[test]
    fn classify_default_ask_est_inform() {
        // Mode `default`/ask : l'`allow` ne bypass pas le TUI (validé run 6) → read-only awareness,
        // PAS de blocage (ni approve ni deny depuis Vigil).
        let v = parse(
            r#"{"toolName":"run_terminal_command","permissionMode":"default","toolInput":{"command":"dir"}}"#,
        );
        assert_eq!(classify_pre_tool_use(&v), PermMode::Inform);
    }

    #[test]
    fn classify_bypass_permissions_est_approve() {
        // `bypassPermissions` : le hook est le SEUL portillon (validé run 7) → Vigil décide,
        // approve/deny à distance.
        let v = parse(r#"{"toolName":"write","permissionMode":"bypassPermissions"}"#);
        assert_eq!(classify_pre_tool_use(&v), PermMode::Approve);
    }

    #[test]
    fn classify_autre_mode_est_monitor() {
        // Modes non gérés (acceptEdits/dontAsk/plan, ou valeur inconnue) → pass-through 6.27.
        let a = parse(r#"{"toolName":"write","permissionMode":"acceptEdits"}"#);
        let b = parse(r#"{"toolName":"write","permissionMode":"always-approve"}"#);
        assert_eq!(classify_pre_tool_use(&a), PermMode::Monitor);
        assert_eq!(classify_pre_tool_use(&b), PermMode::Monitor);
    }

    #[test]
    fn classify_ask_user_question_est_question() {
        // Quel que soit permissionMode (tant qu'il n'y a pas de subagentType).
        let v = parse(
            r#"{"toolName":"ask_user_question","permissionMode":"default","toolInput":{"questions":[]}}"#,
        );
        assert_eq!(classify_pre_tool_use(&v), PermMode::Question);
    }

    #[test]
    fn classify_askquestion_pascalcase_est_question() {
        // Grok a basculé sur les noms style Claude (e2e 13/07) : "AskQuestion" doit AUSSI → Question
        // (sinon la question est classée Inform et rendue en permission read-only — bug constaté).
        let v = parse(
            r#"{"toolName":"AskQuestion","permissionMode":"default","toolInput":{"title":"T","questions":[{"prompt":"Q ?","options":[{"id":"a","label":"A"}]}]}}"#,
        );
        assert_eq!(classify_pre_tool_use(&v), PermMode::Question);
    }

    #[test]
    fn classify_subagent_est_monitor_meme_en_default() {
        // AC7 : présence de subagentType → Monitor/auto-allow, jamais de prompt (précédence 1).
        let v = parse(
            r#"{"toolName":"run_terminal_command","permissionMode":"default","subagentType":"Explore"}"#,
        );
        assert_eq!(classify_pre_tool_use(&v), PermMode::Monitor);
        // Même un sous-agent qui pose une question ne prompte pas (garde AC7 prime sur Question).
        let q = parse(r#"{"toolName":"ask_user_question","subagentType":"Explore"}"#);
        assert_eq!(classify_pre_tool_use(&q), PermMode::Monitor);
    }

    // ── AC2 : convert_response_to_grok ───────────────────────────────────
    #[test]
    fn convert_allow_exit0() {
        let vigil = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        let (out, code) = convert_response_to_grok(vigil);
        assert_eq!(out, GROK_ALLOW);
        assert_eq!(code, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["decision"], "allow");
    }

    #[test]
    fn convert_deny_exit2_json_valide() {
        let vigil = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#;
        let (out, code) = convert_response_to_grok(vigil);
        assert_eq!(out, GROK_DENY);
        assert_eq!(code, 2);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn convert_fallback_deny_si_invalide() {
        // read_pipe_response renvoie la ligne avec un \n terminal → le trim doit le neutraliser.
        let (out_nl, code_nl) = convert_response_to_grok("{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"allow\"}}}\n");
        assert_eq!((out_nl.as_str(), code_nl), (GROK_ALLOW, 0));
        // JSON invalide → deny safe.
        let (out, code) = convert_response_to_grok("pas du json");
        assert_eq!((out.as_str(), code), (GROK_DENY, 2));
    }

    // ── AC8 : normalize_questions (multi_select → multiSelect) ────────────
    #[test]
    fn normalize_questions_renomme_multi_select_et_conserve_options() {
        let v = parse(
            r#"{"toolInput":{"questions":[
                {"question":"Quelle branche ?","options":[{"label":"main","description":"stable"},{"label":"dev"}],"multi_select":true}
            ]}}"#,
        );
        let out = normalize_questions(&v);
        let q0 = &out[0];
        assert_eq!(q0["question"], "Quelle branche ?");
        assert_eq!(q0["multiSelect"], true, "multi_select → multiSelect");
        assert!(q0.get("multi_select").is_none(), "l'ancien nom snake ne fuite pas");
        assert_eq!(q0["options"][0]["label"], "main");
        assert_eq!(q0["options"][0]["description"], "stable");
        assert!(q0.get("header").is_none(), "header absent chez Grok → omis");
    }

    #[test]
    fn normalize_questions_defauts_robustes() {
        // multi_select absent → false ; options absent → [] ; questions absent → [].
        let v = parse(r#"{"toolInput":{"questions":[{"question":"OK ?"}]}}"#);
        let out = normalize_questions(&v);
        assert_eq!(out[0]["multiSelect"], false);
        assert_eq!(out[0]["options"], json!([]));
        assert_eq!(normalize_questions(&parse("{}")), json!([]));
    }

    #[test]
    fn normalize_questions_ni_prompt_ni_question_defaut_chaine_vide() {
        // Review finding P6 : ni "prompt" ni "question" → "question" défaulte à "" (clé toujours
        // présente, contrat frontend `question: string` respecté) au lieu d'être omise.
        let v = parse(r#"{"toolInput":{"questions":[{"options":[]}]}}"#);
        let out = normalize_questions(&v);
        assert_eq!(out[0]["question"], "");
    }

    #[test]
    fn normalize_questions_options_non_tableau_replie_sur_tableau_vide() {
        // Review finding P1 : "options" présent mais pas un tableau (ex. string malformée côté
        // Grok) → replié sur [] au lieu de propager la valeur non-itérable vers le frontend.
        let v = parse(r#"{"toolInput":{"questions":[{"question":"OK ?","options":"oops"}]}}"#);
        let out = normalize_questions(&v);
        assert_eq!(out[0]["options"], json!([]));
    }

    // ── Story 8.4 : emit_question pose read_only ──────────────────────────────

    #[test]
    fn question_payload_est_read_only() {
        let v = parse(
            r#"{"sessionId":"s","transcriptPath":"/t","cwd":"C:\\x","toolInput":{"questions":[
                {"question":"OK ?","options":[{"label":"oui"}]}
            ]}}"#,
        );
        let out = parse(&question_payload(&v));
        assert_eq!(out["hook_event_name"], "PermissionRequest");
        assert_eq!(out["tool_name"], "AskUserQuestion");
        assert_eq!(out["read_only"], true, "source de vérité mobile + front");
        assert_eq!(out["tool_input"]["questions"][0]["question"], "OK ?");
    }
}
