// Bridge entre Codex CLI et Vigil (Epic 6 / story 6.1).
// Différences vs vigil-claude :
//   - SOURCE_TOOL = "codex"
//   - Pas de gestion de flag d'interrupt en V1 (Codex n'a pas le flow Vigil interrupt).
//   - PermissionRequest : la réponse Vigil est renormalisée au format de décision Codex.

use std::io::Write;
use std::process;

use vigil_hook_core::{
    check_tool_enabled, debug_log, inject_parent_pid, inject_source_tool, invoked_by_grok,
    open_vigil_pipe, parse_event_common, read_pipe_response, read_stdin, write_payload,
};

const SOURCE_TOOL: &str = "codex";
// Validé en condition réelle (story 6.7) : parent_pid résolu correctement via codex.exe.
const PROCESS_NAME: &str = "codex.exe";

// Format de décision attendu par Codex sur stdout pour un hook PermissionRequest.
// Doc officielle hooks Codex : enveloppe `hookSpecificOutput.decision.behavior` — IDENTIQUE
// au format Claude Code (`pipe/protocol.rs` ALLOW_RESPONSE/DENY_RESPONSE). `message` optionnel pour deny.
const CODEX_ALLOW: &str =
    r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
// Libellé neutre : le deny peut venir de l'utilisateur, d'une règle d'auto-permission ou du
// timeout côté PipeManager (`APPROVAL_TIMEOUT_SECS`, 600 s depuis la Story 8.8) — éviter « par
// l'utilisateur » qui serait inexact dans ces cas.
const CODEX_DENY: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Refusé via Vigil"}}}"#;

fn main() {
    // Garde anti-double-fire (Story 6.26, étendue défense-en-profondeur). Grok scanne les hooks
    // d'autres outils et pourrait un jour exécuter ce binaire en injectant `GROK_SESSION_ID` ; on se
    // retire AVANT de lire stdin / ouvrir le pipe. Codex réel ne définit jamais cette variable.
    if invoked_by_grok() {
        debug_log("garde anti-double-fire: GROK_SESSION_ID présent → vigil-codex se retire (exit 0)");
        process::exit(0);
    }

    let Some(input) = read_stdin() else {
        process::exit(0);
    };

    if !check_tool_enabled(SOURCE_TOOL) {
        process::exit(0);
    }

    let common = parse_event_common(&input);

    let payload = inject_parent_pid(&input, PROCESS_NAME).unwrap_or_else(|| input.clone());
    let payload = inject_source_tool(&payload, SOURCE_TOOL);

    let Some(mut pipe) = open_vigil_pipe() else {
        process::exit(0);
    };

    if !write_payload(&mut pipe, &payload) {
        process::exit(0);
    }

    if common.hook_event_name == "PermissionRequest" {
        if let Some(response) = read_pipe_response(&mut pipe) {
            let codex_response = convert_response_to_codex(&response);
            print!("{}", codex_response);
            let _ = std::io::stdout().flush();
        }
    }

    process::exit(0);
}

/// Renormalise la réponse Vigil (format interne `hookSpecificOutput`, cf. `ALLOW_RESPONSE`/
/// `DENY_RESPONSE` de `pipe/protocol.rs`) vers la décision attendue par Codex sur stdout.
///
/// Codex partage EXACTEMENT l'enveloppe `hookSpecificOutput.decision.behavior` (doc officielle
/// des hooks Codex). On émet donc le format canonique `CODEX_ALLOW`/`CODEX_DENY` au lieu de
/// l'ancien `{"decision":"allow"}` qui était silencieusement ignoré par Codex (bug story 6.7).
/// Fallback `deny` si le parsing échoue (safe par défaut).
fn convert_response_to_codex(vigil_response: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(vigil_response.trim()).unwrap_or(serde_json::Value::Null);
    let behavior = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("decision"))
        .and_then(|d| d.get("behavior"))
        .and_then(|b| b.as_str())
        .unwrap_or("deny");
    match behavior {
        "allow" => CODEX_ALLOW.to_string(),
        _ => CODEX_DENY.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_allow() {
        // Format réellement envoyé par Vigil sur le pipe (ALLOW_RESPONSE).
        let vigil = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        assert_eq!(convert_response_to_codex(vigil), CODEX_ALLOW);
    }

    #[test]
    fn convert_deny() {
        let vigil = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#;
        assert_eq!(convert_response_to_codex(vigil), CODEX_DENY);
    }

    #[test]
    fn convert_allow_avec_newline_final() {
        // read_pipe_response renvoie la ligne avec le \n terminal : le trim doit le neutraliser.
        let vigil = "{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"allow\"}}}\n";
        assert_eq!(convert_response_to_codex(vigil), CODEX_ALLOW);
    }

    #[test]
    fn convert_fallback_deny_si_invalide() {
        assert_eq!(convert_response_to_codex("pas du json"), CODEX_DENY);
    }

    #[test]
    fn sortie_codex_est_du_json_valide_au_bon_format() {
        // Garde-fou : la décision émise doit être désérialisable et porter behavior=allow.
        let out = convert_response_to_codex(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
    }

    #[test]
    fn sortie_deny_est_du_json_valide_avec_message() {
        // Garde-fou chemin deny : CODEX_DENY contient une apostrophe/accent dans une string JSON —
        // vérifier qu'il reste désérialisable et porte behavior=deny + un message non vide.
        let out = convert_response_to_codex(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert!(v["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()));
    }
}
