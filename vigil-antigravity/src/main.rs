// Bridge entre Antigravity CLI (`agy`) et Vigil (Epic 6 / story 6.17 — socle A1 du spike 6.16).
//
// Différence MAJEURE vs vigil-claude/codex/grok : Antigravity envoie ses payloads en
// **camelCase, SANS champ `hook_event_name`**, avec une structure propre (`toolCall{name,args}`,
// `conversationId`, `transcriptPath`, `workspacePaths[]`, `modelName`). Ce binaire porte donc
// une **couche de traduction** qui normalise chaque payload vers le contrat interne de Vigil
// (`pipe/protocol.rs`, snake_case, tag `hook_event_name`) AVANT de l'écrire sur le pipe. Le
// pipe et le state engine restent inchangés (AC1 : aucun handler outil-spécifique ajouté).
//
// Particularités traitées ici :
//   - L'event n'est PAS dans le payload → il est passé en **argument de ligne de commande** (argv[1]).
//   - Antigravity **n'émet pas de SessionStart** → on le **synthétise** une fois par conversation
//     (gate marqueur-fichier %TEMP%), seul porteur du `modelName`.
//   - `toolCall:null` (frontière d'invocation) → NON forwardé.
//   - Réponse `PreToolUse` = `{"decision":"allow"}` — JAMAIS `{}` (= deny côté Antigravity, prouvé au spike).
//
// Le binaire est ÉCRIT et TESTÉ mais PAS ENCORE INSTALLÉ (détection/install=6.18, UI=6.19,
// permissions réelles=6.22). Il n'est branché sur aucun `hooks.json` à ce stade.

use std::io::Write;
use std::process;

use vigil_hook_core::{
    check_tool_enabled, debug_log, inject_parent_pid, inject_source_tool, invoked_by_grok,
    open_vigil_pipe, read_stdin, transcript_hash, write_payload,
};

const SOURCE_TOOL: &str = "antigravity";
// Process cible dont on remonte l'arbre pour trouver le terminal parent (best-effort).
const PROCESS_NAME: &str = "agy.exe";

// Réponse stdout attendue par Antigravity sur un hook `PreToolUse`. 🚨 JAMAIS `{}` : le spike 6.16
// a prouvé empiriquement que `{}` = deny → bloquerait tout appel d'outil. v1 = observe-only, allow.
const ANTIGRAVITY_ALLOW: &str = r#"{"decision":"allow"}"#;

fn main() {
    // Garde anti-double-fire (Story 6.26, étendue défense-en-profondeur). Grok scanne les hooks
    // d'autres outils et pourrait un jour exécuter ce binaire en injectant `GROK_SESSION_ID` ; on se
    // retire AVANT toute autre logique. Antigravity réel ne définit jamais cette variable.
    if invoked_by_grok() {
        debug_log("garde anti-double-fire: GROK_SESSION_ID présent → vigil-antigravity se retire (exit 0)");
        process::exit(0);
    }

    // L'event est passé en argv[1] (Antigravity ne met pas de hook_event_name dans le payload).
    // Fail-safe : argv absent → exit(0) sans rien faire (jamais panic, ne bloque pas l'agent).
    let event_name = std::env::args().nth(1).unwrap_or_default();
    if event_name.is_empty() {
        process::exit(0);
    }

    let Some(input) = read_stdin() else {
        process::exit(0);
    };

    if !check_tool_enabled(SOURCE_TOOL) {
        process::exit(0);
    }

    // Frontière d'invocation `toolCall:null` (AC3) : ne rien écrire du tout (ni SessionStart
    // synthétique, ni event). On sort proprement.
    if is_tool_frontier(&event_name, &input) {
        process::exit(0);
    }

    // Synthèse SessionStart : une seule fois par conversation (gate marqueur-fichier). Doit être
    // écrite AVANT l'event réel pour enregistrer l'agent avec son modèle (AC5).
    if should_synthesize_session_start(&input) {
        if let Some(session_start) = build_session_start(&input) {
            emit(&session_start);
        }
    }

    // Event réel traduit (None pour PreInvocation/PostInvocation : porteurs de synthèse seulement).
    if let Some(translated) = translate(&event_name, &input) {
        emit(&translated);
    }

    // Réponse stdout : UNIQUEMENT sur PreToolUse (seul event dont Antigravity lit la sortie).
    if event_name == "PreToolUse" {
        print!("{}", ANTIGRAVITY_ALLOW);
        let _ = std::io::stdout().flush();
    }

    process::exit(0);
}

/// Injecte parent_pid + source_tool puis écrit le payload sur une connexion pipe dédiée.
///
/// ⚠️ Une connexion = un payload : le serveur pipe (`pipe/manager.rs`) parse UN seul `HookEvent`
/// par connexion puis la ferme. On ouvre donc un pipe distinct pour le SessionStart synthétique
/// et pour l'event réel (même pattern que `send_synthetic_stop` du core). Best-effort : toute
/// erreur (pipe indisponible, écriture) est silencieuse — ne jamais bloquer la boucle agent.
fn emit(payload: &str) {
    let payload = inject_parent_pid(payload, PROCESS_NAME).unwrap_or_else(|| payload.to_string());
    let payload = inject_source_tool(&payload, SOURCE_TOOL);
    if let Some(mut pipe) = open_vigil_pipe() {
        let _ = write_payload(&mut pipe, &payload);
    }
}

/// Extrait les champs communs à tout payload Antigravity : (session_id, transcript_path, cwd).
/// `conversationId → session_id`, `transcriptPath → transcript_path`, `workspacePaths[0] → cwd`.
/// Champs absents = strings vides (robustesse).
fn extract_common(v: &serde_json::Value) -> (String, String, String) {
    let session_id = v
        .get("conversationId")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let transcript_path = v
        .get("transcriptPath")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let cwd = v
        .get("workspacePaths")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    (session_id, transcript_path, cwd)
}

/// `true` si l'event est un event d'outil (`PreToolUse`/`PostToolUse`) dont le `toolCall` est
/// null ou absent = frontière d'invocation (AC3). Ce cas ne doit RIEN produire (ni synthèse,
/// ni event). JSON invalide → `false` (on laisse `translate` gérer/ignorer).
fn is_tool_frontier(event_name: &str, raw: &str) -> bool {
    if event_name != "PreToolUse" && event_name != "PostToolUse" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    matches!(v.get("toolCall"), None | Some(serde_json::Value::Null))
}

/// Couche de traduction pure (testable sans I/O) : payload brut Antigravity (camelCase) → JSON
/// normalisé au format `HookEvent` (snake_case + `hook_event_name` injecté), prêt à écrire sur
/// le pipe. Retourne `None` si l'event n'a pas de cible descendante directe (`toolCall:null`,
/// `PreInvocation`/`PostInvocation`, event inconnu).
fn translate(event_name: &str, raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let (session_id, transcript_path, cwd) = extract_common(&v);

    match event_name {
        "PreToolUse" => {
            let tool_call = v.get("toolCall")?;
            if tool_call.is_null() {
                return None;
            }
            let (tool_name, tool_input) = extract_tool_call(tool_call);
            Some(
                serde_json::json!({
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
        "PostToolUse" => {
            let tool_call = v.get("toolCall")?;
            if tool_call.is_null() {
                return None;
            }
            let (tool_name, tool_input) = extract_tool_call(tool_call);
            let error = v.get("error").and_then(|x| x.as_str()).unwrap_or_default();
            if error.is_empty() {
                // Succès → PostToolUse. `tool_response` est REQUIS par PostToolUsePayload → {}.
                Some(
                    serde_json::json!({
                        "hook_event_name": "PostToolUse",
                        "session_id": session_id,
                        "transcript_path": transcript_path,
                        "cwd": cwd,
                        "tool_name": tool_name,
                        "tool_input": tool_input,
                        "tool_response": {},
                    })
                    .to_string(),
                )
            } else {
                // Échec → PostToolUseFailure. `tool_response` non requis sur cette variante.
                Some(
                    serde_json::json!({
                        "hook_event_name": "PostToolUseFailure",
                        "session_id": session_id,
                        "transcript_path": transcript_path,
                        "cwd": cwd,
                        "tool_name": tool_name,
                        "tool_input": tool_input,
                        "error": error,
                    })
                    .to_string(),
                )
            }
        }
        "Stop" => {
            // Extras Antigravity (fullyIdle, terminationReason, executionNum, error) ignorés.
            Some(
                serde_json::json!({
                    "hook_event_name": "Stop",
                    "session_id": session_id,
                    "transcript_path": transcript_path,
                    "cwd": cwd,
                })
                .to_string(),
            )
        }
        // PreInvocation / PostInvocation / event inconnu : pas de cible descendante directe.
        _ => None,
    }
}

/// Extrait `(tool_name, tool_input)` d'un `toolCall{name,args}`. `args` copié tel quel dans
/// `tool_input` (objet JSON). Champs absents → nom vide / `{}`.
fn extract_tool_call(tool_call: &serde_json::Value) -> (String, serde_json::Value) {
    let tool_name = tool_call
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_input = tool_call
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    (tool_name, tool_input)
}

/// Construit le `SessionStart` synthétique — seul porteur du `modelName` (AC5). `None` si le
/// payload n'est pas un objet JSON valide.
fn build_session_start(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let (session_id, transcript_path, cwd) = extract_common(&v);
    let model = v.get("modelName").and_then(|x| x.as_str()).unwrap_or_default();
    Some(
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": session_id,
            "transcript_path": transcript_path,
            "cwd": cwd,
            "model": model,
            "source_tool": SOURCE_TOOL,
        })
        .to_string(),
    )
}

/// Gate stateless « une fois par conversation » (AC5). Le binaire étant un process neuf à chaque
/// hook, on persiste l'état vu dans un **marqueur fichier** %TEMP% nommé via
/// `transcript_hash(transcriptPath)` (clé stable/unique par conversation, cf. spike).
///
/// - Marqueur absent → on le crée et retourne `true` (synthétiser).
/// - Marqueur présent → `false` (déjà fait, ne PAS re-synthétiser : le handler SessionStart fait
///   un `insert()` = remplacement complet qui écraserait l'état accumulé de l'agent).
/// - `transcriptPath` vide (pas de clé fiable) → `false` (impossible de gater proprement).
///
/// Robustesse : si l'écriture du marqueur échoue, on retourne quand même `true` (au pire une
/// re-synthèse bénigne). Le vrai risque à éviter est l'inverse : synthétiser à chaque event.
fn should_synthesize_session_start(raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let transcript_path = v
        .get("transcriptPath")
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    if transcript_path.is_empty() {
        return false;
    }
    let marker = session_start_marker_path(transcript_path);
    // Création ATOMIQUE (`create_new`) plutôt qu'un check-then-write : évite la course TOCTOU où
    // deux hooks concurrents d'une même conversation passeraient tous deux un `exists()` == false
    // → double SessionStart → l'`insert()` du state engine (remplacement complet) écraserait
    // l'état accumulé de l'agent.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        // Marqueur créé par NOUS = première fois pour cette conversation → synthétiser.
        Ok(_) => true,
        // Marqueur déjà présent → déjà synthétisé, ne PAS re-synthétiser.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        // Autre erreur (temp indisponible…) : on tolère une re-synthèse bénigne plutôt que de
        // rater le SessionStart (intention spec : « au pire on re-synthétise, acceptable »).
        Err(_) => true,
    }
}

/// Chemin du marqueur de synthèse SessionStart pour une conversation donnée.
fn session_start_marker_path(transcript_path: &str) -> std::path::PathBuf {
    let hash = transcript_hash(transcript_path);
    std::env::temp_dir().join(format!("vigil-antigravity-session-{}.marker", hash))
}

// ─────────────────────────────────────────────────────────────────────
// Tests — sur payloads RÉELS capturés au spike 6.16 (L2).
//
// ⚠️ La crate binaire ne peut pas importer `HookEvent` (défini dans src-tauri). On valide donc
// la traduction en asserant sur les champs JSON produits (option (a) de la story). La preuve de
// désérialisation dans `HookEvent` vit côté src-tauri (`pipe/protocol.rs` :
// antigravity_*_normalise_deserialise).
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Payloads réels du spike (L2). `transcriptPath` normalisé sur la forme runtime UNIQUE
    // observée (spike 6.16 :220/:226 : `…/brain/<id>/.system_generated/logs/transcript_full.jsonl`,
    // stable sur toute la session). Les valeurs abrégées (`log.jsonl`, chemin sans
    // `.system_generated/logs/`) étaient des remplissages incohérents qui n'existent pas au runtime.
    const PRE_TOOL_USE: &str = r#"{"conversationId":"269a0c26-5309-44b7-b3bf-1e7b295dd45a","modelName":"gemini-3.5-flash-low","stepIdx":3,"toolCall":{"name":"run_command","args":{"CommandLine":"dir","Cwd":"C:\\Users\\Dev","WaitMsBeforeAsync":5000}},"transcriptPath":"C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl","workspacePaths":["C:/Users/Dev"],"artifactDirectoryPath":"..."}"#;

    const POST_TOOL_USE_OK: &str = r#"{"conversationId":"269a0c26","error":"","stepIdx":3,"toolCall":{"name":"run_command","args":{"CommandLine":"dir","Cwd":"C:\\Users\\Dev","toolAction":"Running dir command","toolSummary":"Command execution"}},"modelName":"gemini-3.5-flash-low","transcriptPath":"C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl","workspacePaths":["C:/Users/Dev"]}"#;

    const POST_TOOL_USE_NULL: &str = r#"{"conversationId":"269a0c26","error":"","stepIdx":4,"toolCall":null,"modelName":"gemini-3.5-flash-low","transcriptPath":"C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl","workspacePaths":["C:/Users/Dev"]}"#;

    const STOP: &str = r#"{"conversationId":"49b8d486","error":"","executionNum":0,"fullyIdle":true,"terminationReason":"NO_TOOL_CALL","modelName":"gemini-3.5-flash-low","transcriptPath":"C:/Users/Dev/.gemini/antigravity-cli/brain/49b8d486/.system_generated/logs/transcript_full.jsonl","workspacePaths":["C:/Users/Dev"]}"#;

    const PRE_INVOCATION: &str = r#"{"conversationId":"269a0c26","invocationNum":0,"initialNumSteps":1,"modelName":"gemini-3.5-flash-low","transcriptPath":"C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl","workspacePaths":["C:/Users/Dev"],"artifactDirectoryPath":"..."}"#;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("translate doit produire du JSON valide")
    }

    // ── AC2 : PreToolUse ──────────────────────────────────────────────
    #[test]
    fn pre_tool_use_mappings_complets() {
        let out = translate("PreToolUse", PRE_TOOL_USE).expect("PreToolUse doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["session_id"], "269a0c26-5309-44b7-b3bf-1e7b295dd45a");
        assert_eq!(
            v["transcript_path"],
            "C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl"
        );
        assert_eq!(v["cwd"], "C:/Users/Dev"); // workspacePaths[0]
        assert_eq!(v["tool_name"], "run_command"); // toolCall.name
        assert_eq!(v["tool_input"]["CommandLine"], "dir"); // toolCall.args tel quel
        assert_eq!(v["tool_input"]["WaitMsBeforeAsync"], 5000);
    }

    // ── AC3 : PostToolUse succès ──────────────────────────────────────
    #[test]
    fn post_tool_use_succes_devient_post_tool_use_avec_tool_response_vide() {
        let out = translate("PostToolUse", POST_TOOL_USE_OK).expect("succès doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["tool_name"], "run_command");
        assert!(v["tool_response"].is_object()); // requis par PostToolUsePayload
        assert_eq!(v["tool_response"], serde_json::json!({}));
    }

    // ── AC3 : PostToolUse échec ───────────────────────────────────────
    #[test]
    fn post_tool_use_echec_devient_post_tool_use_failure() {
        let raw = r#"{"conversationId":"c","error":"boom","toolCall":{"name":"run_command","args":{"CommandLine":"dir"}},"transcriptPath":"/t.jsonl","workspacePaths":["/w"]}"#;
        let out = translate("PostToolUse", raw).expect("échec doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "PostToolUseFailure");
        assert_eq!(v["error"], "boom");
        assert_eq!(v["tool_name"], "run_command");
        assert!(v.get("tool_response").is_none(), "pas de tool_response sur la variante failure");
    }

    // ── AC3 : toolCall:null → non forwardé ────────────────────────────
    #[test]
    fn post_tool_use_null_non_forwarde() {
        assert!(
            translate("PostToolUse", POST_TOOL_USE_NULL).is_none(),
            "toolCall:null ne doit PAS être forwardé"
        );
    }

    #[test]
    fn frontiere_tool_call_null_detectee() {
        // is_tool_frontier : le main() sort sans rien écrire dans ce cas.
        assert!(is_tool_frontier("PostToolUse", POST_TOOL_USE_NULL));
        assert!(!is_tool_frontier("PostToolUse", POST_TOOL_USE_OK));
        assert!(!is_tool_frontier("Stop", STOP));
        // toolCall absent (event non-tool) → pas une frontière pour Stop, mais bien pour un tool event.
        assert!(is_tool_frontier("PreToolUse", r#"{"conversationId":"c"}"#));
    }

    // ── AC4 : Stop ────────────────────────────────────────────────────
    #[test]
    fn stop_mappings_communs_extras_ignores() {
        let out = translate("Stop", STOP).expect("Stop doit être forwardé");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "Stop");
        assert_eq!(v["session_id"], "49b8d486");
        assert_eq!(v["cwd"], "C:/Users/Dev");
        // Extras surnuméraires non transmis.
        assert!(v.get("fullyIdle").is_none());
        assert!(v.get("terminationReason").is_none());
        assert!(v.get("executionNum").is_none());
        assert!(v.get("error").is_none());
    }

    // ── PreInvocation/PostInvocation : pas de cible directe ────────────
    #[test]
    fn pre_invocation_pas_de_cible_directe() {
        assert!(translate("PreInvocation", PRE_INVOCATION).is_none());
        assert!(translate("PostInvocation", PRE_INVOCATION).is_none());
    }

    // ── AC5 : SessionStart synthétique porte le model ─────────────────
    #[test]
    fn session_start_synthetique_porte_model() {
        let out = build_session_start(PRE_TOOL_USE).expect("SessionStart synthétisable");
        let v = parse(&out);
        assert_eq!(v["hook_event_name"], "SessionStart");
        assert_eq!(v["session_id"], "269a0c26-5309-44b7-b3bf-1e7b295dd45a");
        assert_eq!(v["model"], "gemini-3.5-flash-low"); // modelName → model
        assert_eq!(v["source_tool"], "antigravity");
        assert_eq!(v["cwd"], "C:/Users/Dev");
    }

    #[test]
    fn session_start_synthetisable_depuis_pre_invocation() {
        // PreInvocation porte modelName : c'est un porteur de synthèse idéal (1er signal d'un tour).
        let out = build_session_start(PRE_INVOCATION).expect("SessionStart depuis PreInvocation");
        let v = parse(&out);
        assert_eq!(v["model"], "gemini-3.5-flash-low");
    }

    // ── AC5 : gate une-fois-par-conversation ──────────────────────────
    #[test]
    fn gate_synthese_une_seule_fois_par_conversation() {
        // transcriptPath unique à ce test pour éviter toute collision de marqueur.
        let unique = format!(
            "C:/tmp/vigil-test-{}-{}/transcript_full.jsonl",
            std::process::id(),
            "gate"
        );
        let raw = format!(
            r#"{{"conversationId":"c","modelName":"m","transcriptPath":"{}","workspacePaths":["/w"]}}"#,
            unique
        );
        // Nettoyage préalable (au cas où un run précédent aurait laissé le marqueur).
        let _ = std::fs::remove_file(session_start_marker_path(&unique));

        assert!(
            should_synthesize_session_start(&raw),
            "1er passage : doit synthétiser"
        );
        assert!(
            !should_synthesize_session_start(&raw),
            "2e passage : marqueur présent, ne doit PAS re-synthétiser"
        );

        // Nettoyage.
        let _ = std::fs::remove_file(session_start_marker_path(&unique));
    }

    #[test]
    fn gate_faux_si_transcript_path_absent() {
        // Pas de clé fiable → on ne gate pas (et donc on ne synthétise pas en boucle).
        assert!(!should_synthesize_session_start(r#"{"conversationId":"c"}"#));
    }
}
