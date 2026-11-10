// Bridge entre Claude Code et Vigil — un binaire dédié par outil CLI (Epic 6 / story 6.1).
// Comportement identique au précédent `vigil-hook.exe` : régression zéro.

use std::io::Write;
use std::process;

use vigil_hook_core::{
    check_tool_enabled, debug_log, handle_interrupt_flag_claude_code, inject_parent_pid,
    inject_source_tool, invoked_by_grok, open_vigil_pipe, parse_event_common, read_pipe_response,
    read_stdin, write_payload,
};

const SOURCE_TOOL: &str = "claude_code";
const PROCESS_NAME: &str = "claude.exe";

fn main() {
    // AC5 (Story 6.26) : garde anti-double-fire. Grok scanne aussi `~/.claude/settings.json` et
    // exécute ce binaire ; il injecte `GROK_SESSION_ID` → on se retire AVANT de lire stdin / ouvrir
    // le pipe (fail-open côté Grok, n'interrompt aucun outil). Claude réel ne définit jamais cette
    // variable → régression zéro.
    if invoked_by_grok() {
        debug_log("garde anti-double-fire: GROK_SESSION_ID présent → vigil-claude se retire (exit 0)");
        process::exit(0);
    }

    let Some(input) = read_stdin() else {
        process::exit(0);
    };

    // AC5 : filtrage avant ouverture du pipe.
    if !check_tool_enabled(SOURCE_TOOL) {
        process::exit(0);
    }

    let common = parse_event_common(&input);

    // Vérifie le flag d'interrupt avant d'écrire sur le pipe.
    if handle_interrupt_flag_claude_code(
        &common.hook_event_name,
        &common.transcript_path,
        &common.session_id,
        &common.cwd,
        SOURCE_TOOL,
    ) {
        process::exit(0);
    }

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
            // Réponse Vigil au format Claude Code — relayée telle quelle.
            print!("{}", response);
            let _ = std::io::stdout().flush();
        }
    }

    process::exit(0);
}
