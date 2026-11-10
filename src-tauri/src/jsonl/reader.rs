use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use crate::agent::state::AgentMessage;

pub struct JsonlReader;

#[derive(Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: String,
    message: Option<JsonlMessage>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct JsonlMessage {
    content: Option<Value>,
}

impl JsonlReader {
    /// Lit les `limit` derniers messages pertinents du transcript et les retourne en ordre
    /// chronologique. **Dispatch par outil** : chaque format de transcript a son propre parser
    /// (cf. [[project-degeneraliser-claude]] — le format Claude n'est plus qu'une branche parmi
    /// d'autres). L'outil est fourni explicitement par l'appelant (`agent.sourceTool`), jamais
    /// deviné par sniffing de contenu. Antigravity, Codex et Grok ont leur branche dédiée ; le `_`
    /// couvre `claude_code` et tout outil inconnu (parser Claude) — comportement runtime
    /// strictement inchangé pour ces outils.
    ///
    /// ⚠️ **Grok** : `transcript_path` = `…\updates.jsonl` (run-events, PAS la conversation) →
    /// `read_tail_grok` **dérive** le dossier de session et lit le sibling `chat_history.jsonl`.
    pub fn read_tail(transcript_path: &str, limit: usize, tool: &str) -> Vec<AgentMessage> {
        if limit == 0 {
            return vec![];
        }
        match tool {
            "antigravity" => read_tail_antigravity(transcript_path, limit),
            "codex" => read_tail_codex(transcript_path, limit),
            "grok" => read_tail_grok(transcript_path, limit),
            "kimi" => read_tail_kimi(transcript_path, limit),
            _ => read_tail_claude(transcript_path, limit),
        }
    }
}

/// Parser Claude Code (format historique : `{type:"user"/"assistant", message.content, timestamp}`).
/// Logique extraite telle quelle de l'ancien `read_tail` — comportement byte-identique.
fn read_tail_claude(transcript_path: &str, limit: usize) -> Vec<AgentMessage> {
    let file = match File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);

    let messages = reader.lines().filter_map(|line_result| {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            _ => return None,
        };

        let entry: JsonlEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => {
                eprintln!("[vigil/jsonl] skipping unparseable line");
                return None;
            }
        };

        if entry.entry_type != "user" && entry.entry_type != "assistant" {
            return None;
        }

        let msg = entry.message.as_ref()?;
        let timestamp = entry.timestamp.as_deref().map(iso8601_to_unix_ms).unwrap_or(0);
        let (role, content) = extract_message(&entry.entry_type, msg)?;
        if content.is_empty() {
            return None;
        }
        Some(AgentMessage { role, content, timestamp })
    });

    tail(messages, limit)
}

/// Garde les `limit` derniers `AgentMessage` d'un flux, en ordre chronologique (ring buffer
/// partagé par tous les parsers pour ne pas dupliquer la logique de troncature `limit`).
fn tail<I: Iterator<Item = AgentMessage>>(messages: I, limit: usize) -> Vec<AgentMessage> {
    let mut ring: VecDeque<AgentMessage> = VecDeque::with_capacity(limit + 1);
    for msg in messages {
        ring.push_back(msg);
        if ring.len() > limit {
            ring.pop_front();
        }
    }
    ring.into_iter().collect()
}

fn extract_message(entry_type: &str, msg: &JsonlMessage) -> Option<(String, String)> {
    match entry_type {
        "user" => match &msg.content {
            Some(Value::String(s)) if !s.is_empty() => Some(("user".to_string(), s.clone())),
            Some(Value::Array(blocks)) => {
                let is_tool_result = blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
                if is_tool_result {
                    let content = blocks
                        .iter()
                        .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                        .and_then(|b| {
                            // content peut être string ou array
                            match b.get("content") {
                                Some(Value::String(s)) => Some(truncate(s)),
                                Some(Value::Array(arr)) => {
                                    let text = arr
                                        .iter()
                                        .filter(|x| x.get("type").and_then(Value::as_str) == Some("text"))
                                        .filter_map(|x| x.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    if text.is_empty() { None } else { Some(truncate(&text)) }
                                }
                                _ => None,
                            }
                        })?;
                    Some(("tool".to_string(), content))
                } else {
                    let text = blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() { None } else { Some(("user".to_string(), text)) }
                }
            }
            _ => None,
        },
        "assistant" => match &msg.content {
            Some(Value::Array(blocks)) => {
                let text = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() { None } else { Some(("assistant".to_string(), truncate(&text))) }
            }
            _ => None,
        },
        _ => None,
    }
}

// ── Antigravity : parser de transcript step-based (format `agy`, spike 6.16) ──────────
//
// Le transcript Antigravity est un JSONL step-based snake_case INCOMPATIBLE avec le format
// Claude : une ligne = une « étape » `{step_index, source, type, status, created_at, content?,
// thinking?, tool_calls?}`. Ce parser mappe chaque étape pertinente sur un `AgentMessage`
// (rôles génériques user/assistant/tool déjà stylés par `MessageBubble`) et filtre les étapes
// rejouées / sans contenu affichable. Lecture STRICTEMENT en lecture, ne panique jamais.

/// Une étape du transcript Antigravity. Tous les champs sont optionnels : une ligne corrompue ou
/// partielle (streaming) se désérialise en `None` plutôt que d'échouer, et `map_step` la filtre.
#[derive(Deserialize)]
struct AntigravityStep {
    source: Option<String>,
    #[serde(rename = "type")]
    step_type: Option<String>,
    created_at: Option<String>,
    content: Option<String>,
    thinking: Option<String>,
    tool_calls: Option<Vec<AntigravityToolCall>>,
}

/// Un appel d'outil dans une étape `MODEL`. `args` = valeurs JSON **stringifiées** (sous-champs =
/// chaînes contenant du JSON échappé) → on prend les chaînes telles quelles pour le résumé, sans
/// re-parser en profondeur (fragile et inutile pour un affichage lisible).
#[derive(Deserialize)]
struct AntigravityToolCall {
    name: Option<String>,
    args: Option<Value>,
}

/// Lit les `limit` derniers messages Antigravity en ordre chronologique. Le transcript est déjà
/// ordonné par `step_index` croissant → l'ordre de lecture = ordre chronologique. Fichier
/// introuvable/illisible → `vec![]` ; ligne inparsable → sautée (comme le parser Claude).
fn read_tail_antigravity(transcript_path: &str, limit: usize) -> Vec<AgentMessage> {
    let file = match open_antigravity_transcript(transcript_path) {
        Some(f) => f,
        None => return vec![],
    };
    let reader = BufReader::new(file);

    let messages = reader.lines().filter_map(|line_result| {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            _ => return None,
        };
        let step: AntigravityStep = match serde_json::from_str(&line) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("[vigil/jsonl] skipping unparseable antigravity line");
                return None;
            }
        };
        map_step(&step)
    });

    tail(messages, limit)
}

/// Ouvre le transcript Antigravity. Repli défensif (AC4) : si le chemin `transcript_full.jsonl`
/// est absent, tenter `transcript.jsonl` dans le même dossier. Le chemin reçu est normalement déjà
/// le bon (rapporté par l'agent vivant) — ce repli ne fait pas de sur-ingénierie.
fn open_antigravity_transcript(path: &str) -> Option<File> {
    if let Ok(f) = File::open(path) {
        return Some(f);
    }
    if let Some(prefix) = path.strip_suffix("transcript_full.jsonl") {
        let alt = format!("{prefix}transcript.jsonl");
        if let Ok(f) = File::open(&alt) {
            return Some(f);
        }
    }
    None
}

/// Mappe une étape Antigravity sur un `AgentMessage`, ou `None` si l'étape n'a rien d'affichable.
/// Priorité (une étape = au plus un message) :
///   1. `USER_INPUT` → `user` (texte de `<USER_REQUEST>`)
///   2. `source:MODEL` + `content` non vide → `assistant` (réponse textuelle, prioritaire)
///   3. `tool_calls[]` non vide → `tool` (résumé lisible `nom — toolSummary`)
///   4. `source:MODEL` + `thinking` seul → `assistant`
///
/// Filtré : `CHECKPOINT`, `CONVERSATION_HISTORY` (historique rejoué), étape sans contenu affichable.
fn map_step(step: &AntigravityStep) -> Option<AgentMessage> {
    let step_type = step.step_type.as_deref().unwrap_or("");
    if step_type == "CHECKPOINT" || step_type == "CONVERSATION_HISTORY" {
        return None;
    }
    let timestamp = step.created_at.as_deref().map(iso8601_to_unix_ms).unwrap_or(0);
    let is_model = step.source.as_deref() == Some("MODEL");

    // 1. Message utilisateur : texte enveloppé `<USER_REQUEST>…</USER_REQUEST>`.
    if step_type == "USER_INPUT" {
        let raw = step.content.as_deref()?;
        // Repli si tag non fermé/vide (transcript tronqué/streaming, ou balise ouvrante absente) :
        // retirer les tags ouvrant ET fermant pour ne pas faire fuir `<USER_REQUEST>`/`</USER_REQUEST>`
        // littéral dans le message affiché (même garde-fou que 6.20).
        let text = extract_tag(raw, "USER_REQUEST").unwrap_or_else(|| {
            raw.trim()
                .trim_start_matches("<USER_REQUEST>")
                .trim_end_matches("</USER_REQUEST>")
                .trim()
                .to_string()
        });
        let text = text.trim();
        return if text.is_empty() {
            None
        } else {
            Some(AgentMessage { role: "user".into(), content: truncate(text), timestamp })
        };
    }

    // 2. Réponse modèle textuelle — `content` prioritaire sur `thinking`.
    if is_model {
        if let Some(content) = step.content.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(AgentMessage { role: "assistant".into(), content: truncate(content), timestamp });
        }
    }

    // 3. Appels d'outils → résumé lisible (nom + toolSummary/toolAction, pas de dump JSON).
    // Gardé par `is_model` (comme les branches 2 et 4) : la spec lie `tool_calls` à `source:"MODEL"`
    // → une étape non-modèle non filtrée n'émet pas de message `tool` parasite.
    if is_model {
        if let Some(calls) = step.tool_calls.as_deref() {
            let summary = calls
                .iter()
                .filter_map(summarize_tool_call)
                .collect::<Vec<_>>()
                .join("\n");
            if !summary.trim().is_empty() {
                return Some(AgentMessage { role: "tool".into(), content: truncate(&summary), timestamp });
            }
        }
    }

    // 4. Repli : pensée modèle seule (aucun `content`, aucun `tool_call` exploitable).
    if is_model {
        if let Some(thinking) = step.thinking.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(AgentMessage { role: "assistant".into(), content: truncate(thinking), timestamp });
        }
    }

    None
}

/// Résume un appel d'outil en une ligne humaine (`view_file — Reading src/main.rs`). Les
/// sous-champs de `args` sont des chaînes JSON stringifiées → on les prend telles quelles.
fn summarize_tool_call(call: &AntigravityToolCall) -> Option<String> {
    let detail = call.args.as_ref().and_then(|args| {
        ["toolSummary", "toolAction"].iter().find_map(|k| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
    });
    let name = call.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match (name, detail) {
        (Some(name), Some(detail)) => Some(format!("{name} — {detail}")),
        (Some(name), None) => Some(name.to_string()),
        (None, Some(detail)) => Some(detail),
        (None, None) => None,
    }
}

// ── Codex : parser de rollout de session (format `codex`, spike 6.23) ─────────────────
//
// Le rollout Codex est un JSONL une-ligne-par-item : `{timestamp, type, payload}`. Le
// `timestamp` est au NIVEAU RACINE de la ligne (≠ Antigravity où `created_at` vit dans l'étape).
// La conversation persistée canonique vit dans les `type:"response_item"` ; les `event_msg`
// (token_count, agent_message, …) sont des événements de stream UI qui DOUBLONNENT le
// `response_item` → filtrés. Formes FS-vérifiées au spike 6.23 sur sessions réelles :
//   - message user      → payload.type:"message", role:"user",      content:[{type:"input_text",  text}]
//   - message assistant → payload.type:"message", role:"assistant", content:[{type:"output_text", text}]
//   - function_call     → payload.type:"function_call", name (str), arguments (CHAÎNE JSON échappée)
//   - reasoning         → payload.type:"reasoning", summary TOUJOURS vide `[]`, texte dans
//                         `encrypted_content` (opaque, non affichable) → FILTRÉ (≠ thinking
//                         Antigravity qui était en clair)
//   - function_call_output (résultat brut d'outil), session_meta, turn_context, tool_search_*,
//     event_msg → tous FILTRÉS.
// Lecture STRICTEMENT en lecture, ne panique jamais ; fichier absent/illisible → `vec![]`.

/// Une ligne racine du rollout Codex. Tous les champs sont optionnels : une ligne partielle
/// (streaming) ou corrompue se désérialise sans échec, et `map_codex_item` la filtre. `payload`
/// reste un `Value` opaque (formes multiples selon `payload.type`) — inspecté à la volée.
#[derive(Deserialize)]
struct CodexLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    line_type: Option<String>,
    payload: Option<Value>,
}

/// Lit les `limit` derniers messages Codex en ordre chronologique. Le rollout est écrit en ordre
/// d'apparition → l'ordre de lecture = ordre chronologique (comme Antigravity). Fichier
/// introuvable/illisible (dont `transcript_path` vide/`null`, AC5) → `vec![]` ; ligne inparsable → sautée.
fn read_tail_codex(transcript_path: &str, limit: usize) -> Vec<AgentMessage> {
    let file = match File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);

    let messages = reader.lines().filter_map(|line_result| {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            _ => return None,
        };
        let parsed: CodexLine = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => {
                eprintln!("[vigil/jsonl] skipping unparseable codex line");
                return None;
            }
        };
        map_codex_item(&parsed)
    });

    tail(messages, limit)
}

/// Mappe une ligne de rollout Codex sur un `AgentMessage`, ou `None` si rien d'affichable.
/// Seuls les `response_item` portent la conversation persistée : tout le reste (session_meta,
/// turn_context, event_msg) est filtré d'entrée. Garantit un `content` non vide dans chaque
/// branche (aucun message vide émis).
fn map_codex_item(line: &CodexLine) -> Option<AgentMessage> {
    // Seul `response_item` est la source canonique. session_meta / turn_context / event_msg → filtrés.
    if line.line_type.as_deref() != Some("response_item") {
        return None;
    }
    let payload = line.payload.as_ref()?;
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = line.timestamp.as_deref().map(iso8601_to_unix_ms).unwrap_or(0);

    match payload_type {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                // Message utilisateur : filtre les blocs synthétiques (developer, `<…>`).
                "user" => {
                    let text = extract_codex_user_text(payload)?;
                    Some(AgentMessage { role: "user".into(), content: truncate(&text), timestamp })
                }
                // Réponse modèle : texte dans les blocs `output_text`.
                "assistant" => {
                    let text = extract_codex_output_text(payload)?;
                    Some(AgentMessage { role: "assistant".into(), content: truncate(&text), timestamp })
                }
                // `developer` (permissions) et tout autre rôle synthétique → filtrés.
                _ => None,
            }
        }
        // Appel d'outil → résumé lisible (nom + extrait d'arguments, chaîne prise telle quelle).
        "function_call" => {
            let name = payload.get("name").and_then(Value::as_str);
            let arguments = payload.get("arguments").and_then(Value::as_str);
            let summary = summarize_codex_tool_call(name, arguments)?;
            Some(AgentMessage { role: "tool".into(), content: truncate(&summary), timestamp })
        }
        // reasoning (chiffré, summary vide → non affichable), function_call_output (résultat brut),
        // tool_search_call/output, et tout item sans texte → filtrés.
        _ => None,
    }
}

/// Extrait le texte d'un message utilisateur Codex depuis un `payload` `response_item`, en filtrant
/// les messages synthétiques (ré-implémentation locale du helper de `history/reader.rs` pour garder
/// `jsonl/` self-contained — pas de dépendance croisée entre modules) :
///   - `role:"developer"` (instructions de permissions) → ignoré
///   - `role:"user"` dont le texte commence par `<` (`<environment_context>`, `<turn_aborted>`, …)
///     → wrapper synthétique injecté, ignoré
fn extract_codex_user_text(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = payload.get("content").and_then(Value::as_array)?;
    let text = content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None; // wrapper synthétique (<environment_context>, <turn_aborted>, …)
    }
    Some(trimmed.to_string())
}

/// Concatène les blocs `output_text` d'un message assistant Codex. `None` si aucun texte non vide.
fn extract_codex_output_text(payload: &Value) -> Option<String> {
    let content = payload.get("content").and_then(Value::as_array)?;
    let text = content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() { None } else { Some(text.to_string()) }
}

/// Résume un appel d'outil Codex en une ligne humaine (`shell — {"command":"cargo test"}`).
/// `arguments` est une CHAÎNE JSON (potentiellement échappée) : on la prend telle quelle, on
/// aplatit les blancs et on la borne — PAS de re-parsing en profondeur (fragile, inutile pour un
/// affichage lisible ; même principe que les args Antigravity en 6.21). Sans nom d'outil → `None`.
fn summarize_codex_tool_call(name: Option<&str>, arguments: Option<&str>) -> Option<String> {
    let name = name.map(str::trim).filter(|s| !s.is_empty())?;
    let args = arguments
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_to(&s.split_whitespace().collect::<Vec<_>>().join(" "), 120));
    Some(match args {
        Some(args) => format!("{name} — {args}"),
        None => name.to_string(),
    })
}

// ── Grok : parser de conversation `chat_history.jsonl` (format `grok`, spike 6.29) ────
//
// **Piège structurel #1** : `transcript_path` d'un agent Grok vivant = `…\updates.jsonl`
// (run-events synthétisés par `vigil-grok` 6.27), qui **ne contient PAS la conversation**. La
// conversation vit dans le **sibling `chat_history.jsonl`** (même dossier de session) → ce parser
// dérive `parent(transcript_path)` puis ouvre `chat_history.jsonl`. C'est LA divergence vs
// Codex/Antigravity (où le `transcript_path` reçu EST le fichier de conversation).
//
// Schéma FS-vérifié au spike 6.29 (7 sessions réelles ~/.grok/sessions/…) :
//   - `system`      : `{content:str}` = gros prompt système (embarque un `<user_query>` duplicata
//                     du 1er prompt) → FILTRÉ (le vrai 1er prompt réapparaît en ligne `user`).
//   - `user`        : `{content:[{type:"text",text}], synthetic_reason?}` — `content` = LISTE de
//                     blocs. Mêle `<user_info>` (synthetic_reason ABSENT = le piège), `<system-reminder>`
//                     (synthetic_reason présent), et le vrai prompt `<user_query>…</user_query>`. →
//                     émet `user` UNIQUEMENT sur le contenu de `<user_query>` ; tout le reste filtré.
//   - `reasoning`   : `{summary:[{type:"summary_text",text}], encrypted_content, …}` — texte de
//                     summary en clair MAIS `encrypted_content` présent (comme Codex), et PRÉCÈDE
//                     chaque tour assistant → FILTRÉ (l'afficher doublerait chaque bulle et
//                     saturerait le tail 3/20 ; le vrai contenu est dans `assistant.content`).
//   - `assistant`   : `{content:str, tool_calls?:[{name,arguments:str}], model_id, …}` — `content`
//                     TOUJOURS une chaîne (vide si la ligne ne porte que des tool_calls). Une ligne
//                     émet DEUX messages au plus : `assistant`(content) + `tool`(résumé des calls).
//   - `tool_result` : `{tool_call_id, content:str}` = sortie brute SANS nom d'outil → FILTRÉ (non
//                     résumable proprement ; l'appel est déjà montré via `assistant.tool_calls`).
//   - **horodatage** : AUCUN champ sur aucune ligne → `timestamp = 0` ; ordre = ordre du fichier.
// Lecture STRICTEMENT en lecture, ne panique jamais.

/// Une ligne de `chat_history.jsonl`. Tous les champs optionnels : ligne partielle/corrompue →
/// désérialisation sans échec, `map_grok_line` la filtre. `content` reste un `Value` opaque car il
/// varie (chaîne pour `assistant`/`system`/`tool_result`, liste de blocs pour `user`).
#[derive(Deserialize)]
struct GrokChatLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    content: Option<Value>,
    synthetic_reason: Option<String>,
    tool_calls: Option<Vec<GrokToolCall>>,
}

/// Un appel d'outil dans une ligne `assistant`. Même forme que Codex (`function_call`) : `name` +
/// `arguments` = CHAÎNE JSON échappée → résumé via `summarize_codex_tool_call` (réutilisé). Champs
/// typés `Option<Value>` (opaques, comme Codex lit `arguments`) et NON `Option<String>` : un
/// `arguments`/`name` non-string (objet/nombre, si le format Grok évoluait) désérialise sans échec
/// et retombe sur `.as_str() → None` au lieu de faire échouer TOUTE la ligne (perdant alors le
/// `content` texte de l'assistant). Robustesse review 6.29 (parité Codex).
#[derive(Deserialize)]
struct GrokToolCall {
    name: Option<Value>,
    arguments: Option<Value>,
}

/// Lit les `limit` derniers messages Grok en ordre chronologique. **Dérive la source** : le
/// `transcript_path` reçu = `…\updates.jsonl` → on lit le sibling `chat_history.jsonl` dans le même
/// dossier de session. Parent absent (chemin vide/dégénéré), sibling introuvable/illisible → `vec![]`
/// (AC6). `chat_history.jsonl` est écrit en ordre d'apparition → ordre de lecture = chronologique.
fn read_tail_grok(transcript_path: &str, limit: usize) -> Vec<AgentMessage> {
    let Some(session_dir) = Path::new(transcript_path).parent() else {
        return vec![];
    };
    let chat_path = session_dir.join("chat_history.jsonl");
    let file = match File::open(&chat_path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);

    // `flat_map` : une ligne `assistant` peut produire 2 messages (texte + outil), les autres 0 ou 1.
    let messages = reader.lines().flat_map(|line_result| {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            _ => return Vec::new(),
        };
        let parsed: GrokChatLine = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => {
                eprintln!("[vigil/jsonl] skipping unparseable grok line");
                return Vec::new();
            }
        };
        map_grok_line(&parsed)
    });

    tail(messages, limit)
}

/// Mappe une ligne `chat_history.jsonl` sur 0..2 `AgentMessage` (schéma T0 spike 6.29). `timestamp`
/// toujours 0 (aucun champ d'horodatage dans le format). Garantit un `content` non vide.
fn map_grok_line(line: &GrokChatLine) -> Vec<AgentMessage> {
    match line.line_type.as_deref().unwrap_or("") {
        // Prompt utilisateur : uniquement le contenu de `<user_query>`. Filtre les blocs
        // synthétiques : `synthetic_reason` présent (`<system-reminder>`) et toute ligne sans
        // `<user_query>` (`<user_info>` — piège : PAS de synthetic_reason mais PAS de user_query).
        "user" => {
            if line.synthetic_reason.is_some() {
                return Vec::new();
            }
            let text = grok_blocks_text(line.content.as_ref());
            match extract_tag(&text, "user_query") {
                Some(query) => vec![AgentMessage { role: "user".into(), content: truncate(&query), timestamp: 0 }],
                None => Vec::new(),
            }
        }
        // Réponse modèle : texte (`content`) ET/OU appels d'outils (`tool_calls`) → 0..2 messages.
        "assistant" => {
            let mut out = Vec::new();
            if let Some(text) = line
                .content
                .as_ref()
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.push(AgentMessage { role: "assistant".into(), content: truncate(text), timestamp: 0 });
            }
            if let Some(calls) = line.tool_calls.as_deref() {
                // Grok `tool_calls` = même forme que Codex `function_call` (name + arguments string).
                // `name`/`arguments` lus via `.as_str()` sur un `Value` opaque (parité Codex) : un
                // type inattendu → `None` sans perdre le message (robustesse review 6.29).
                let summary = calls
                    .iter()
                    .filter_map(|c| {
                        summarize_codex_tool_call(
                            c.name.as_ref().and_then(Value::as_str),
                            c.arguments.as_ref().and_then(Value::as_str),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !summary.trim().is_empty() {
                    out.push(AgentMessage { role: "tool".into(), content: truncate(&summary), timestamp: 0 });
                }
            }
            out
        }
        // system (prompt), reasoning (chiffré + saturant), tool_result (brut, sans nom) → filtrés.
        _ => Vec::new(),
    }
}

/// Concatène le texte d'un `content` Grok. `user` → liste de blocs `{type:"text", text}` ; repli
/// défensif si `content` est déjà une chaîne. Blocs sans `text` ignorés.
fn grok_blocks_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ── Kimi CLI : parser de conversation `wire.jsonl` (format propriétaire, spike T0 9.4) ────
//
// Le `transcript_path` d'un agent Kimi = `<sessionDir>/agents/main/wire.jsonl` — c'est bien le
// fichier de conversation (contrairement à Grok où il faut dériver un sibling). Format PROPRIÉTAIRE,
// une ligne JSON par event (`type`). Schéma FS-vérifié au spike T0 (sessions réelles ~/.kimi-code) :
//   - `context.append_message` : `{message:{role,content:[{type:"text",text}],toolCalls},time}` —
//     N'ENREGISTRE QUE le rôle `user` (les réponses assistant vivent dans les loop events). →
//     1 message `user` (contenu = concaténation des blocs texte).
//   - `context.append_loop_event` : enveloppe des events de la boucle d'inférence. `event.type` :
//       • `content.part` + `part.type=="text"`  → texte visible de l'assistant → 1 message `assistant`.
//       • `content.part` + `part.type=="think"` → raisonnement (saturant) → FILTRÉ.
//       • `tool.call` (`name`,`args`)            → 1 message `tool` (résumé nom + args).
//       • `tool.result`, `step.begin`, `step.end` → FILTRÉS.
//   - `metadata`, `config.update`, `tools.set_active_tools`, `llm.tools_snapshot`, `turn.prompt`,
//     `llm.request`, `usage.record`, `permission.*` → FILTRÉS (ni user ni assistant visible).
//   - **horodatage** : champ `time` (ms Unix) sur les events → `timestamp = time`.
//
// ⚠️ ÉCART ASSUMÉ vs le texte de l'AC3, corrigé par le spike T0 : l'AC listait
// `context.append_loop_event` comme ligne « à filtrer » et supposait que `context.append_message`
// portait aussi l'assistant et les `toolCalls`. Le disque montre l'INVERSE : `append_message` est
// user-only et TOUT le contenu assistant (texte + appels d'outils) vit dans `append_loop_event`. Le
// filtrer produirait une Vue Détail sans AUCUNE réponse d'agent — contraire à l'INTENT de l'AC
// (« les derniers messages s'affichent »). On projette donc les loop events pertinents.

/// Lit les `limit` derniers messages Kimi en ordre chronologique. `wire.jsonl` est écrit en ordre
/// d'apparition → ordre de lecture = chronologique. Fichier introuvable/illisible → `vec![]` (AC3).
/// Chaque ligne est désérialisée en `Value` opaque (tolérant : une ligne tronquée est sautée sans
/// perdre les autres), puis projetée par `map_kimi_line`.
fn read_tail_kimi(transcript_path: &str, limit: usize) -> Vec<AgentMessage> {
    let file = match File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);

    // `flat_map` : une ligne produit 0 ou 1 message (jamais 2 chez Kimi, contrairement à Grok).
    let messages = reader.lines().flat_map(|line_result| {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            _ => return Vec::new(),
        };
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[vigil/jsonl] skipping unparseable kimi line");
                return Vec::new();
            }
        };
        map_kimi_line(&parsed)
    });

    tail(messages, limit)
}

/// Lit **on-demand** le modèle d'un agent Kimi VIVANT depuis son `wire.jsonl` (Story 9.4, AC4).
/// Le badge n'est PAS frontend-zéro (`vigil-kimi` n'injecte pas de modèle au session_start, comme
/// `vigil-grok`) → lecture backend à l'ouverture de la Vue Détail. Source = dernier event
/// `llm.request` portant un `model` non vide (T0 : `{type:"llm.request", model:"…", modelAlias:"…"}` ;
/// on retient `model`, la forme propre, pas l'alias). Fichier/champ absent → `None` (badge vide,
/// dégradation propre). Le dernier gagne = le modèle courant si l'utilisateur en a changé en session.
pub fn read_kimi_model(transcript_path: &str) -> Option<String> {
    let file = File::open(transcript_path).ok()?;
    let reader = BufReader::new(file);
    let mut model: Option<String> = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("llm.request") {
            continue;
        }
        if let Some(m) = v.get("model").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            model = Some(m.to_string());
        }
    }
    model
}

/// Mappe une ligne `wire.jsonl` sur 0..1 `AgentMessage`. Garantit un `content` non vide. `timestamp`
/// = champ `time` (ms Unix) quand présent, sinon 0.
fn map_kimi_line(line: &Value) -> Vec<AgentMessage> {
    let time = line.get("time").and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64))).unwrap_or(0);
    match line.get("type").and_then(Value::as_str).unwrap_or("") {
        // Message user (le seul rôle enregistré ici ; l'assistant vit dans les loop events).
        "context.append_message" => {
            let Some(msg) = line.get("message") else { return Vec::new() };
            if msg.get("role").and_then(Value::as_str) != Some("user") {
                return Vec::new();
            }
            let content = msg.get("content");
            let text = kimi_content_text(content);
            let text = text.trim();
            let display = if !text.is_empty() {
                truncate(text)
            } else if kimi_content_has_blocks(content) {
                // Tour user composé UNIQUEMENT de blocs non-texte (image/pièce jointe) : placeholder
                // plutôt que faire disparaître le tour (préserve la paire requête/réponse).
                "[pièce jointe]".to_string()
            } else {
                return Vec::new();
            };
            vec![AgentMessage { role: "user".into(), content: display, timestamp: time }]
        }
        // Events de la boucle : texte assistant (content.part/text) + appels d'outils (tool.call).
        "context.append_loop_event" => {
            let Some(event) = line.get("event") else { return Vec::new() };
            match event.get("type").and_then(Value::as_str).unwrap_or("") {
                "content.part" => {
                    let Some(part) = event.get("part") else { return Vec::new() };
                    // Seul le texte visible produit un message ; `think` (raisonnement) filtré.
                    if part.get("type").and_then(Value::as_str) != Some("text") {
                        return Vec::new();
                    }
                    let text = part.get("text").and_then(Value::as_str).map(str::trim).unwrap_or("");
                    if text.is_empty() {
                        return Vec::new();
                    }
                    vec![AgentMessage { role: "assistant".into(), content: truncate(text), timestamp: time }]
                }
                "tool.call" => {
                    match summarize_kimi_tool_call(event.get("name").and_then(Value::as_str), event.get("args")) {
                        Some(s) => vec![AgentMessage { role: "tool".into(), content: truncate(&s), timestamp: time }],
                        None => Vec::new(),
                    }
                }
                // tool.result (brut, sans nom), step.begin/step.end → filtrés.
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Concatène le texte d'un `content` Kimi (`[{type:"text",text}]`). Repli défensif si `content` est
/// déjà une chaîne. Blocs sans `text` ignorés (jamais de panique).
fn kimi_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `true` si `content` est un tableau non vide : des blocs existent (même sans champ `text`). Sert à
/// distinguer un tour user réellement vide (à supprimer) d'un tour à blocs non-texte (image/pièce
/// jointe) qui mérite un placeholder plutôt que la disparition.
fn kimi_content_has_blocks(content: Option<&Value>) -> bool {
    matches!(content, Some(Value::Array(b)) if !b.is_empty())
}

/// Résumé d'un `tool.call` Kimi : `name — {args compacts}`. `args` est un OBJET JSON
/// (`{command,timeout}`, `{file_path}`, …) rendu compact et tronqué à 120 car. `name` vide/absent →
/// `None` (aucun message). `args` absent/nul/`{}` → juste le nom.
fn summarize_kimi_tool_call(name: Option<&str>, args: Option<&Value>) -> Option<String> {
    let name = name.map(str::trim).filter(|s| !s.is_empty())?;
    let args_str = args
        .filter(|a| !a.is_null())
        .map(|a| truncate_to(&a.to_string(), 120));
    Some(match args_str {
        Some(a) if a != "{}" => format!("{name} — {a}"),
        _ => name.to_string(),
    })
}

/// Tronque `s` à `max` **caractères** (sûr sur les frontières UTF-8), suffixe `…` si tronqué.
fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// Extrait le contenu de `<tag>…</tag>` (repli local du helper de `history/reader.rs`, ré-implémenté
/// ici pour garder chaque parser self-contained — pas de dépendance croisée entre modules).
fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)?;
    let content = s[start..start + end].trim();
    if content.is_empty() { None } else { Some(content.to_string()) }
}

fn truncate(s: &str) -> String {
    const MAX: usize = 2000;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let boundary = s.char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= MAX)
            .last()
            .unwrap_or(0);
        format!("{}…", &s[..boundary])
    }
}

/// Convertit une chaîne ISO 8601 "YYYY-MM-DDTHH:MM:SS[.fff]Z" en millisecondes Unix.
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
            _ => 0,
        }
    }
    let mut d: i64 = 0;
    for y in 1970..year {
        d += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        d += month_days(m, year);
    }
    d + day as i64 - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("vigil_jsonl_test_{}.jsonl", suffix));
        p
    }

    #[test]
    fn test_missing_file_returns_empty() {
        let result = JsonlReader::read_tail("/this/path/does/not/exist/vigil.jsonl", 5, "claude_code");
        assert!(result.is_empty());
    }

    #[test]
    fn test_tail_limit() {
        let path = tmp_path("tail");
        let mut f = fs::File::create(&path).unwrap();
        for i in 0..5u32 {
            let line = format!(
                r#"{{"type":"user","message":{{"role":"user","content":"message {}"}},"timestamp":"2026-05-0{}T10:00:00.000Z"}}"#,
                i, i + 1
            );
            writeln!(f, "{}", line).unwrap();
        }
        drop(f);

        let result = JsonlReader::read_tail(path.to_str().unwrap(), 3, "claude_code");
        let _ = fs::remove_file(&path);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].content, "message 2");
        assert_eq!(result[2].content, "message 4");
    }

    #[test]
    fn test_parse_user_and_assistant() {
        let path = tmp_path("parse");
        let user_line = r#"{"type":"user","message":{"role":"user","content":"Bonjour"},"timestamp":"2026-05-05T12:00:00.000Z"}"#;
        let asst_line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Bonjour ! Comment ?"}]},"timestamp":"2026-05-05T12:00:01.500Z"}"#;
        let noise = r#"{"type":"permission-mode","permissionMode":"default"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", user_line, asst_line, noise)).unwrap();

        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "claude_code");
        let _ = fs::remove_file(&path);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Bonjour");
        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content, "Bonjour ! Comment ?");
        assert!(result[1].timestamp > 0);
    }

    #[test]
    fn test_iso8601_to_unix_ms() {
        assert_eq!(iso8601_to_unix_ms("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(iso8601_to_unix_ms("1970-01-01T00:00:01.000Z"), 1_000);
        assert_eq!(iso8601_to_unix_ms("1970-01-01T00:01:00.000Z"), 60_000);
        assert_eq!(iso8601_to_unix_ms("1970-01-01T01:00:00.000Z"), 3_600_000);
    }

    #[test]
    fn test_unknown_tool_falls_back_to_claude() {
        // Un outil inconnu tombe sur le parser Claude par défaut (branche `_`), comportement runtime
        // INCHANGÉ. `"codex"` (6.23) et `"grok"` (6.29) ont désormais leur propre parser et ne
        // figurent PLUS dans cette liste.
        let path = tmp_path("fallback");
        let user_line = r#"{"type":"user","message":{"role":"user","content":"Salut"},"timestamp":"2026-05-05T12:00:00.000Z"}"#;
        fs::write(&path, format!("{}\n", user_line)).unwrap();

        for tool in ["unknown", "claude_code"] {
            let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, tool);
            assert_eq!(result.len(), 1, "tool {tool} devrait utiliser le parser Claude");
            assert_eq!(result[0].content, "Salut");
        }
        let _ = fs::remove_file(&path);
    }

    // ── Antigravity : parser step-based (Story 6.21) ──────────────────────────

    // Échantillon step-based réel (spike 6.16) : user, réponse modèle textuelle, appel d'outil
    // avec args stringifiés, thinking-seul, + étapes filtrées CHECKPOINT / CONVERSATION_HISTORY.
    const AGY_USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T11:29:03Z","content":"<USER_REQUEST>\nBonjour Antigravity\n</USER_REQUEST>"}"#;
    const AGY_MODEL_TEXT: &str = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-07-05T11:29:04Z","content":"Voici ma réponse."}"#;
    const AGY_TOOL: &str = r#"{"step_index":2,"source":"MODEL","type":"VIEW_FILE","status":"DONE","created_at":"2026-07-05T11:29:05Z","thinking":"je lis le fichier","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"C:/x/main.rs\"","toolSummary":"Reading main.rs"}}]}"#;
    const AGY_CHECKPOINT: &str = r#"{"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-07-05T11:29:06Z"}"#;
    const AGY_HISTORY: &str = r#"{"step_index":4,"source":"SYSTEM","type":"CONVERSATION_HISTORY","status":"DONE","created_at":"2026-07-05T11:29:07Z","content":"rejoué"}"#;
    const AGY_THINKING_ONLY: &str = r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-07-05T11:29:08Z","thinking":"réflexion sans contenu final"}"#;

    fn write_agy(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = tmp_path(name);
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    #[test]
    fn agy_mappe_user_assistant_tool_et_filtre_checkpoint_history() {
        let path = write_agy(
            "agy_full",
            &[AGY_USER, AGY_MODEL_TEXT, AGY_TOOL, AGY_CHECKPOINT, AGY_HISTORY],
        );
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "antigravity");
        let _ = fs::remove_file(&path);

        // CHECKPOINT + CONVERSATION_HISTORY filtrés → 3 messages (user, assistant, tool).
        assert_eq!(result.len(), 3, "checkpoint/history doivent être filtrés");

        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Bonjour Antigravity");
        assert!(!result[0].content.contains("<USER_REQUEST>"), "le tag ne doit pas fuir");

        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content, "Voici ma réponse.");

        assert_eq!(result[2].role, "tool");
        assert_eq!(result[2].content, "view_file — Reading main.rs");

        // Ordre chronologique + timestamps parsés.
        assert!(result[0].timestamp > 0);
        assert!(result[0].timestamp < result[1].timestamp);
        assert!(result[1].timestamp < result[2].timestamp);
    }

    #[test]
    fn agy_thinking_seul_devient_assistant() {
        let path = write_agy("agy_thinking", &[AGY_THINKING_ONLY]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content, "réflexion sans contenu final");
    }

    #[test]
    fn agy_tail_limit() {
        // 3 messages produits (user, assistant, tool) → limit 2 garde les 2 derniers.
        let path = write_agy("agy_tail", &[AGY_USER, AGY_MODEL_TEXT, AGY_TOOL]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 2, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[1].role, "tool");
    }

    #[test]
    fn agy_fichier_absent_retourne_vide() {
        let result = JsonlReader::read_tail("/no/such/agy/transcript_full.jsonl", 10, "antigravity");
        assert!(result.is_empty());
    }

    #[test]
    fn agy_ligne_corrompue_sautee() {
        let path = write_agy("agy_corrupt", &["PAS DU JSON {{{", AGY_USER]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Bonjour Antigravity");
    }

    #[test]
    fn agy_user_tag_non_ferme_ne_fuit_pas() {
        // Transcript tronqué (streaming) : <USER_REQUEST> non fermé → strip du tag ouvrant.
        let line = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T11:29:03Z","content":"<USER_REQUEST>\nMessage tronqué"}"#;
        let path = write_agy("agy_unclosed", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Message tronqué");
        assert!(!result[0].content.contains("<USER_REQUEST>"));
    }

    #[test]
    fn agy_tool_sans_summary_utilise_le_nom() {
        // args sans toolSummary/toolAction → fallback sur le nom d'outil seul.
        let line = r#"{"step_index":1,"source":"MODEL","type":"SEARCH_WEB","status":"DONE","created_at":"2026-07-05T11:29:05Z","tool_calls":[{"name":"search_web","args":{"Query":"rust serde"}}]}"#;
        let path = write_agy("agy_tool_noname", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].content, "search_web");
    }

    #[test]
    fn agy_etape_vide_filtree() {
        // Étape MODEL sans content/thinking/tool_calls → aucun message émis.
        let line = r#"{"step_index":9,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-07-05T11:29:09Z"}"#;
        let path = write_agy("agy_empty_step", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty());
    }

    #[test]
    fn agy_user_tag_fermant_orphelin_ne_fuit_pas() {
        // Review 6.21 (P1) : `extract_tag` échoue (ouvrante absente ou contenu vide) → le repli doit
        // aussi strip la balise FERMANTE, sinon `</USER_REQUEST>` fuit dans le message affiché.
        // (a) balise fermante orpheline → texte propre.
        let orphan = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T11:29:03Z","content":"Bonjour</USER_REQUEST>"}"#;
        let path = write_agy("agy_orphan_close", &[orphan]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "Bonjour");
        assert!(!result[0].content.contains("</USER_REQUEST>"));

        // (b) requête vide `<USER_REQUEST></USER_REQUEST>` → aucun message (pas de tag littéral).
        let empty = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-07-05T11:29:03Z","content":"<USER_REQUEST></USER_REQUEST>"}"#;
        let path = write_agy("agy_empty_req", &[empty]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty(), "requête vide ne doit rien émettre, surtout pas le tag littéral");
    }

    #[test]
    fn agy_tool_calls_hors_model_ignores() {
        // Review 6.21 (P2) : une étape non-MODEL non filtrée portant `tool_calls` ne doit pas émettre
        // de message `tool` parasite (branche gardée par `is_model`, comme content/thinking).
        let line = r#"{"step_index":0,"source":"SYSTEM","type":"SOME_SYSTEM_STEP","status":"DONE","created_at":"2026-07-05T11:29:05Z","tool_calls":[{"name":"view_file","args":{"toolSummary":"parasite"}}]}"#;
        let path = write_agy("agy_tool_non_model", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "antigravity");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty(), "tool_calls hors MODEL ne doit pas émettre de message parasite");
    }

    // ── Codex : parser de rollout de session (Story 6.23) ──────────────────────
    //
    // Échantillons issus des FORMES RÉELLES FS-vérifiées au spike 6.23 (sessions
    // ~/.codex/sessions/…) : timestamp racine, response_item message user/assistant (output_text),
    // function_call (arguments = chaîne JSON), + tous les types filtrés (session_meta, turn_context,
    // event_msg, reasoning chiffré, function_call_output, blocs synthétiques developer/<…>).

    const CX_META: &str = r#"{"timestamp":"2026-07-05T08:30:00.000Z","type":"session_meta","payload":{"id":"019f3165","cwd":"C:/Users/Dev"}}"#;
    const CX_TURN: &str = r#"{"timestamp":"2026-07-05T08:30:01.000Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#;
    const CX_ENV: &str = r#"{"timestamp":"2026-07-05T08:30:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>C:/Users/Dev</cwd>\n</environment_context>"}]}}"#;
    const CX_DEV: &str = r#"{"timestamp":"2026-07-05T08:30:03.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"You must ask for permission."}]}}"#;
    const CX_USER: &str = r#"{"timestamp":"2026-07-05T08:30:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Peux-tu analyser le code ?"}]}}"#;
    const CX_REASONING: &str = r#"{"timestamp":"2026-07-05T08:30:05.000Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"gAAAAABopaque"}}"#;
    const CX_TOOL: &str = r#"{"timestamp":"2026-07-05T08:30:06.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"cargo test\"}"}}"#;
    const CX_TOOL_OUT: &str = r#"{"timestamp":"2026-07-05T08:30:07.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"{\"ok\":true}"}}"#;
    const CX_ASSISTANT: &str = r#"{"timestamp":"2026-07-05T08:30:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Voici mon analyse."}]}}"#;
    const CX_EVENT: &str = r#"{"timestamp":"2026-07-05T08:30:08.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Voici mon analyse."}}"#;

    fn write_cx(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = tmp_path(name);
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    #[test]
    fn cx_mappe_user_assistant_tool_et_filtre_le_bruit() {
        let path = write_cx(
            "cx_full",
            &[
                CX_META, CX_TURN, CX_ENV, CX_DEV, CX_USER, CX_REASONING, CX_TOOL, CX_TOOL_OUT,
                CX_ASSISTANT, CX_EVENT,
            ],
        );
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "codex");
        let _ = fs::remove_file(&path);

        // Filtrés : session_meta, turn_context, environment_context (<…>), developer, reasoning,
        // function_call_output, event_msg (doublon stream) → restent user, tool, assistant.
        assert_eq!(result.len(), 3, "seuls user + function_call + assistant doivent survivre");

        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Peux-tu analyser le code ?");

        assert_eq!(result[1].role, "tool");
        assert_eq!(result[1].content, r#"shell — {"command":"cargo test"}"#);

        assert_eq!(result[2].role, "assistant");
        assert_eq!(result[2].content, "Voici mon analyse.");

        // Ordre chronologique + timestamps racine parsés.
        assert!(result[0].timestamp > 0);
        assert!(result[0].timestamp < result[1].timestamp);
        assert!(result[1].timestamp < result[2].timestamp);
    }

    #[test]
    fn cx_filtre_blocs_synthetiques_user() {
        // environment_context (texte `<…>`) et developer ne doivent JAMAIS fuir dans la conversation.
        let path = write_cx("cx_synthetic", &[CX_ENV, CX_DEV]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty(), "blocs synthétiques developer/<…> doivent être filtrés");
    }

    #[test]
    fn cx_reasoning_chiffre_filtre() {
        // reasoning Codex = summary vide + encrypted_content opaque → non affichable → filtré
        // (≠ thinking Antigravity en clair). Décision tranchée au spike 6.23.
        let path = write_cx("cx_reasoning", &[CX_REASONING]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty(), "reasoning chiffré ne doit rien émettre");
    }

    #[test]
    fn cx_tool_sans_arguments_utilise_le_nom() {
        let line = r#"{"timestamp":"2026-07-05T08:30:06.000Z","type":"response_item","payload":{"type":"function_call","name":"list_dir"}}"#;
        let path = write_cx("cx_tool_noargs", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "codex");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].content, "list_dir");
    }

    #[test]
    fn cx_tail_limit_et_ordre_chrono() {
        // user, tool, assistant → limit 2 garde les 2 derniers en ordre chronologique.
        let path = write_cx("cx_tail", &[CX_USER, CX_TOOL, CX_ASSISTANT]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 2, "codex");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[1].role, "assistant");
    }

    #[test]
    fn cx_fichier_absent_retourne_vide() {
        let result = JsonlReader::read_tail("/no/such/codex/rollout.jsonl", 10, "codex");
        assert!(result.is_empty());
    }

    #[test]
    fn cx_chemin_vide_retourne_vide() {
        // AC5 : transcript_path vide (sous-agents Codex, cf. 6.14) → File::open échoue → vec![].
        let result = JsonlReader::read_tail("", 10, "codex");
        assert!(result.is_empty());
    }

    #[test]
    fn cx_ligne_corrompue_sautee() {
        let path = write_cx("cx_corrupt", &["PAS DU JSON {{{", CX_USER]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Peux-tu analyser le code ?");
    }

    #[test]
    fn cx_message_vide_non_emis() {
        // Message assistant sans texte affichable → filtré (aucun message vide émis, AC4).
        let line = r#"{"timestamp":"2026-07-05T08:30:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"   "}]}}"#;
        let path = write_cx("cx_empty_asst", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty());
    }

    #[test]
    fn cx_user_multibloc_concatene() {
        // Review 6.23 (patch) : un message user multi-blocs `input_text` doit concaténer TOUS les
        // blocs (`join("\n")`), pas seulement le premier — symétrie avec `extract_codex_output_text`.
        let line = r#"{"timestamp":"2026-07-05T08:30:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Ligne 1"},{"type":"input_text","text":"Ligne 2"}]}}"#;
        let path = write_cx("cx_user_multibloc", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Ligne 1\nLigne 2");
    }

    #[test]
    fn cx_function_call_arguments_non_string_utilise_le_nom() {
        // Robustesse : si `arguments` est un objet JSON (au lieu de la chaîne attendue), `as_str`
        // échoue → dégradation propre sur le nom d'outil seul (pas de dump, pas de panique).
        let line = r#"{"timestamp":"2026-07-05T08:30:06.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":{"command":"cargo test"}}}"#;
        let path = write_cx("cx_tool_argsobj", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].content, "shell");
    }

    #[test]
    fn cx_function_call_sans_nom_filtre() {
        // Un appel d'outil sans `name` n'est pas affichable → filtré (aucun message `tool` vide).
        let line = r#"{"timestamp":"2026-07-05T08:30:06.000Z","type":"response_item","payload":{"type":"function_call","arguments":"{\"command\":\"ls\"}"}}"#;
        let path = write_cx("cx_tool_noname", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty());
    }

    #[test]
    fn cx_user_vide_non_emis() {
        // Chemin user : message dont le seul bloc `input_text` est vide/blanc → filtré.
        let line = r#"{"timestamp":"2026-07-05T08:30:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"   "}]}}"#;
        let path = write_cx("cx_user_vide", &[line]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty());
    }

    #[test]
    fn cx_payload_absent_ou_null_filtre() {
        // `response_item` sans `payload`, ou avec `payload:null` (ligne partielle/streaming) → filtré
        // proprement (garde `line.payload.as_ref()?`), jamais de panique.
        let no_payload = r#"{"timestamp":"2026-07-05T08:30:00.000Z","type":"response_item"}"#;
        let null_payload = r#"{"timestamp":"2026-07-05T08:30:01.000Z","type":"response_item","payload":null}"#;
        let path = write_cx("cx_payload_absent", &[no_payload, null_payload]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 10, "codex");
        let _ = fs::remove_file(&path);
        assert!(result.is_empty());
    }

    // ── Grok : parser de conversation chat_history.jsonl (Story 6.29) ──────────
    //
    // Échantillons calqués sur les FORMES RÉELLES FS-vérifiées au spike 6.29 (texte de conversation
    // fictif) : source = sibling `chat_history.jsonl` dérivé du dossier de `updates.jsonl` ; types
    // system/user(user_info|system-reminder|user_query)/reasoning/assistant(content|tool_calls)/tool_result ;
    // AUCUN horodatage (timestamp=0, ordre=fichier).

    const GK_SYSTEM: &str = r#"{"type":"system","content":"You are Grok. <user_query>doublon ignoré</user_query>"}"#;
    const GK_USER_INFO: &str = r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS=windows\n</user_info>"}]}"#;
    const GK_SYS_REMINDER: &str = r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>note interne</system-reminder>"}],"synthetic_reason":"system_reminder"}"#;
    const GK_USER_QUERY: &str = r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nAnalyse le code\n</user_query>"}]}"#;
    const GK_REASONING: &str = r#"{"type":"reasoning","id":"rc_1","summary":[{"type":"summary_text","text":"je reflechis"}],"encrypted_content":"gAAAopaque","status":"completed"}"#;
    const GK_ASST_TEXT: &str = r#"{"type":"assistant","content":"Voici ma reponse.","model_id":"grok-4.5","reasoning_effort":"low"}"#;
    const GK_ASST_TEXT_TOOLS: &str = r#"{"type":"assistant","content":"Je regarde le fichier.","tool_calls":[{"id":"c1","name":"read_file","arguments":"{\"path\":\"main.rs\"}"}],"model_id":"grok-4.5"}"#;
    const GK_ASST_TOOLS_ONLY: &str = r#"{"type":"assistant","content":"","tool_calls":[{"id":"c2","name":"grep","arguments":"{\"q\":\"fn\"}"}],"model_id":"grok-4.5"}"#;
    const GK_TOOL_RESULT: &str = r#"{"type":"tool_result","tool_call_id":"c1","content":"contenu brut du fichier, long et illisible"}"#;

    /// Crée un dossier de session Grok temporaire et renvoie le chemin d'`updates.jsonl` (le
    /// `transcript_path` reçu). `chat_lines = None` → PAS de sibling `chat_history.jsonl` (cas absent).
    fn write_grok_session(name: &str, chat_lines: Option<&[&str]>) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("vigil_grok_test_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let updates = dir.join("updates.jsonl");
        // updates.jsonl = run-events factices ; le parser NE doit PAS le lire.
        fs::write(&updates, "{\"method\":\"run\",\"params\":{}}\n").unwrap();
        if let Some(lines) = chat_lines {
            fs::write(dir.join("chat_history.jsonl"), format!("{}\n", lines.join("\n"))).unwrap();
        }
        updates
    }

    fn cleanup_grok_session(updates: &std::path::Path) {
        if let Some(dir) = updates.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn grok_mappe_conversation_et_filtre_le_bruit() {
        let updates = write_grok_session(
            "full",
            Some(&[
                GK_SYSTEM, GK_USER_INFO, GK_SYS_REMINDER, GK_USER_QUERY, GK_REASONING,
                GK_ASST_TEXT, GK_ASST_TEXT_TOOLS, GK_ASST_TOOLS_ONLY, GK_TOOL_RESULT,
            ]),
        );
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);

        // Filtrés : system, user_info, system-reminder, reasoning, tool_result.
        // Restent : user, assistant(texte), assistant(texte)+tool, tool → 5 messages.
        assert_eq!(result.len(), 5, "system/user_info/system-reminder/reasoning/tool_result filtrés");

        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Analyse le code");
        assert!(!result[0].content.contains("<user_query>"), "le tag ne doit pas fuir");

        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content, "Voici ma reponse.");

        assert_eq!(result[2].role, "assistant");
        assert_eq!(result[2].content, "Je regarde le fichier.");

        assert_eq!(result[3].role, "tool");
        assert_eq!(result[3].content, r#"read_file — {"path":"main.rs"}"#);

        assert_eq!(result[4].role, "tool");
        assert_eq!(result[4].content, r#"grep — {"q":"fn"}"#);

        // Aucun bloc synthétique ne fuit dans toute la conversation.
        for m in &result {
            assert!(!m.content.contains("<user_info>"));
            assert!(!m.content.contains("<system-reminder>"));
        }
        // chat_history.jsonl ne porte AUCUN horodatage → timestamp=0 partout (spike T0), ordre=fichier.
        assert!(result.iter().all(|m| m.timestamp == 0), "aucun horodatage dans le format Grok");
    }

    #[test]
    fn grok_filtre_blocs_synthetiques_user() {
        // <user_info> (sans synthetic_reason = le piège) et <system-reminder> ne doivent jamais fuir.
        let updates = write_grok_session("synthetic", Some(&[GK_SYSTEM, GK_USER_INFO, GK_SYS_REMINDER]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert!(result.is_empty(), "system + user_info + system-reminder → aucune conversation");
    }

    #[test]
    fn grok_reasoning_et_tool_result_filtres() {
        let updates = write_grok_session("reasoning_toolres", Some(&[GK_REASONING, GK_TOOL_RESULT]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert!(result.is_empty(), "reasoning (chiffré) + tool_result (brut) filtrés");
    }

    #[test]
    fn grok_assistant_tools_seul_emet_un_message_tool() {
        // Ligne assistant sans texte (content vide) mais avec tool_calls → 1 seul message `tool`.
        let updates = write_grok_session("tools_only", Some(&[GK_ASST_TOOLS_ONLY]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].content, r#"grep — {"q":"fn"}"#);
    }

    #[test]
    fn grok_tail_limit_et_ordre_chrono() {
        // user, assistant, assistant, tool (les 2 derniers issus de GK_ASST_TEXT_TOOLS) → limit 2.
        let updates = write_grok_session("tail", Some(&[GK_USER_QUERY, GK_ASST_TEXT, GK_ASST_TEXT_TOOLS]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 2, "grok");
        cleanup_grok_session(&updates);
        // Produits : user, assistant("Voici"), assistant("Je regarde"), tool → limit 2 = les 2 derniers.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content, "Je regarde le fichier.");
        assert_eq!(result[1].role, "tool");
    }

    #[test]
    fn grok_chat_history_absent_retourne_vide() {
        // updates.jsonl présent mais AUCUN sibling chat_history.jsonl (session sans conversation).
        let updates = write_grok_session("no_chat", None);
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert!(result.is_empty());
    }

    #[test]
    fn grok_ligne_corrompue_sautee() {
        let updates = write_grok_session("corrupt", Some(&["PAS DU JSON {{{", GK_USER_QUERY]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Analyse le code");
    }

    #[test]
    fn grok_transcript_path_vide_retourne_vide() {
        // AC6 : transcript_path vide/sans parent exploitable → vec![], jamais de panique.
        assert!(JsonlReader::read_tail("", 20, "grok").is_empty());
    }

    #[test]
    fn grok_user_query_vide_non_emis() {
        // <user_query></user_query> vide → extract_tag renvoie None → aucun message (pas de tag littéral).
        let empty = r#"{"type":"user","content":[{"type":"text","text":"<user_query></user_query>"}]}"#;
        let updates = write_grok_session("empty_query", Some(&[empty]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        assert!(result.is_empty());
    }

    #[test]
    fn grok_tool_call_arguments_non_string_preserve_le_content() {
        // Robustesse review 6.29 (parité Codex) : `GrokToolCall` typé `Option<Value>` → un `arguments`
        // (ou `name`) NON-string (objet, si le format Grok évoluait) désérialise SANS échec au lieu de
        // faire échouer toute la ligne. Le `content` texte de l'assistant est préservé ; l'outil est
        // résumé par son seul nom (args non lisibles → `.as_str() → None`, comme Codex).
        let asst = r#"{"type":"assistant","content":"Je lance l'outil.","tool_calls":[{"id":"c9","name":"do_it","arguments":{"path":"main.rs"}}],"model_id":"grok-4.5"}"#;
        let updates = write_grok_session("tool_args_object", Some(&[asst]));
        let result = JsonlReader::read_tail(updates.to_str().unwrap(), 20, "grok");
        cleanup_grok_session(&updates);
        // La ligne ne doit PAS être jetée : content assistant + résumé outil (nom seul).
        assert_eq!(result.len(), 2, "arguments non-string ne doit pas jeter la ligne assistant");
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content, "Je lance l'outil.");
        assert_eq!(result[1].role, "tool");
        assert_eq!(result[1].content, "do_it", "args non-string → résumé par le nom seul");
    }

    // ── Kimi (9.4) : read_tail_kimi / map_kimi_line / read_kimi_model ─────────────
    //
    // Fixtures = extraits RÉELS du poste (spike T0 2026-07-24). L'assistant vit dans
    // `context.append_loop_event` (content.part/text + tool.call), PAS dans `append_message`.

    const KIMI_USER: &str = r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"Salut"}],"toolCalls":[],"origin":{"kind":"user"}},"time":1784738461022}"#;
    const KIMI_THINK: &str = r#"{"type":"context.append_loop_event","event":{"type":"content.part","turnId":"0","step":1,"part":{"type":"think","think":"raisonnement interne"}},"time":1784738573816}"#;
    const KIMI_TEXT: &str = r#"{"type":"context.append_loop_event","event":{"type":"content.part","turnId":"0","step":1,"part":{"type":"text","text":"Salut ! Comment puis-je t'aider ?"}},"time":1784738573816}"#;
    const KIMI_TOOL: &str = r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"Hp4","name":"Bash","args":{"command":"sleep 5","timeout":10}},"time":1784745010451}"#;
    const KIMI_TOOLRESULT: &str = r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"Hp4","result":"ok"},"time":1784745015451}"#;
    const KIMI_NOISE: &str = r#"{"type":"usage.record","model":"lmstudio/qwen","usage":{"inputOther":21296,"output":58},"usageScope":"turn","time":1784738573817}"#;

    fn write_kimi_wire(suffix: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = tmp_path(&format!("kimi_wire_{suffix}"));
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    #[test]
    fn kimi_projette_user_assistant_et_filtre_le_reste() {
        let path = write_kimi_wire(
            "conv",
            &[KIMI_USER, KIMI_NOISE, KIMI_THINK, KIMI_TEXT, KIMI_TOOL, KIMI_TOOLRESULT],
        );
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "kimi");
        let _ = fs::remove_file(&path);
        // user + assistant(text) + tool(call) ; think/tool.result/usage.record filtrés.
        assert_eq!(result.len(), 3, "user + assistant + tool");
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "Salut");
        assert_eq!(result[0].timestamp, 1784738461022, "time ms Unix repris tel quel");
        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content, "Salut ! Comment puis-je t'aider ?");
        assert_eq!(result[2].role, "tool");
        assert!(result[2].content.starts_with("Bash — "), "résumé nom + args: {}", result[2].content);
        assert!(result[2].content.contains("sleep 5"));
    }

    #[test]
    fn kimi_append_message_assistant_ignore() {
        // Défensif : si un jour un append_message portait role assistant, il ne doit PAS produire de
        // message (l'assistant passe UNIQUEMENT par les loop events → sinon doublon).
        let asst_msg = r#"{"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"x"}]},"time":1}"#;
        let path = write_kimi_wire("asstmsg", &[KIMI_USER, asst_msg]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "kimi");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
    }

    #[test]
    fn kimi_ligne_illisible_sautee_sans_perdre_les_autres() {
        let path = write_kimi_wire("badline", &["ceci n'est pas du JSON {{{", KIMI_USER, KIMI_TEXT]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "kimi");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 2, "ligne corrompue sautée, les 2 autres conservées");
    }

    #[test]
    fn kimi_content_vide_ne_produit_pas_de_message() {
        let empty_user = r#"{"type":"context.append_message","message":{"role":"user","content":[]},"time":1}"#;
        let empty_text = r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"   "}},"time":2}"#;
        let path = write_kimi_wire("empty", &[empty_user, empty_text, KIMI_TEXT]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "kimi");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 1, "content vide/blanc filtré, seul le vrai texte reste");
        assert_eq!(result[0].role, "assistant");
    }

    #[test]
    fn kimi_user_blocs_non_texte_donne_placeholder() {
        // Un tour user composé UNIQUEMENT de blocs non-texte (image/pièce jointe) ne doit PAS
        // disparaître : placeholder plutôt que suppression (paire requête/réponse préservée).
        let img_user = r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"image","url":"x"}]},"time":1}"#;
        let path = write_kimi_wire("imguser", &[img_user, KIMI_TEXT]);
        let result = JsonlReader::read_tail(path.to_str().unwrap(), 20, "kimi");
        let _ = fs::remove_file(&path);
        assert_eq!(result.len(), 2, "le tour user à blocs non-texte est conservé");
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "[pièce jointe]");
    }

    #[test]
    fn kimi_read_tail_fichier_absent_retourne_vide() {
        assert!(JsonlReader::read_tail("/nonexistent/kimi/wire.jsonl", 5, "kimi").is_empty());
    }

    #[test]
    fn kimi_read_model_dernier_llm_request_gagne() {
        let req1 = r#"{"type":"llm.request","model":"qwen/qwen3-30b","modelAlias":"lmstudio/qwen","time":1}"#;
        let req2 = r#"{"type":"llm.request","model":"kimi-k2","time":2}"#;
        let path = write_kimi_wire("model", &[KIMI_USER, req1, KIMI_TEXT, req2]);
        let model = read_kimi_model(path.to_str().unwrap());
        let _ = fs::remove_file(&path);
        assert_eq!(model.as_deref(), Some("kimi-k2"), "le dernier llm.request gagne");
    }

    #[test]
    fn kimi_read_model_absent_retourne_none() {
        let path = write_kimi_wire("nomodel", &[KIMI_USER, KIMI_TEXT]);
        let model = read_kimi_model(path.to_str().unwrap());
        let _ = fs::remove_file(&path);
        assert_eq!(model, None);
        assert_eq!(read_kimi_model("/nonexistent/wire.jsonl"), None);
    }
}
