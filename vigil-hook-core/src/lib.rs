// Socle commun aux binaires hook Vigil (vigil-claude, vigil-codex, vigil-grok).
// Toute la logique réutilisable vit ici ; chaque binaire ne porte que sa constante
// SOURCE_TOOL/PROCESS_NAME et son flow `main()`.

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

// ── stdin / pipe I/O ──────────────────────────────────────────────────

/// Lit stdin jusqu'à EOF, trim, retourne `None` si vide ou en cas d'erreur.
pub fn read_stdin() -> Option<String> {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return None;
    }
    let input = input.trim().to_string();
    if input.is_empty() {
        return None;
    }
    Some(input)
}

/// Ouvre `\\.\pipe\vigil` en lecture/écriture. `None` si le pipe est indisponible.
pub fn open_vigil_pipe() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\vigil")
        .ok()
}

/// Écrit `payload\n` puis flush. Retourne `false` à la première erreur.
pub fn write_payload(pipe: &mut std::fs::File, payload: &str) -> bool {
    pipe.write_all(payload.as_bytes()).is_ok()
        && pipe.write_all(b"\n").is_ok()
        && pipe.flush().is_ok()
}

/// Lit une ligne (jusqu'à `\n`) depuis le pipe. `None` si vide ou erreur.
pub fn read_pipe_response(pipe: &mut std::fs::File) -> Option<String> {
    let mut reader = io::BufReader::new(pipe);
    let mut response = String::new();
    if reader.read_line(&mut response).is_ok() && !response.is_empty() {
        Some(response)
    } else {
        None
    }
}

// ── Manipulation JSON ─────────────────────────────────────────────────

/// Injecte `source_tool` dans le payload JSON s'il n'est pas déjà présent (défensif).
/// Retourne la string originale si parsing JSON échoue (robustesse).
pub fn inject_source_tool(json: &str, source_tool: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return json.to_string();
    };
    if !obj.contains_key("source_tool") {
        obj.insert(
            "source_tool".to_string(),
            serde_json::Value::String(source_tool.to_string()),
        );
    }
    serde_json::to_string(&value).unwrap_or_else(|_| json.to_string())
}

/// Injecte les infos d'hôte dérivées de l'arbre de process (UN seul snapshot via [`host_info`]) :
/// `parent_pid` (= `terminal_pid` à surveiller) et `terminal_source` (libellé d'hôte, ex. « VS Code »).
/// Retourne `None` si le payload n'est pas un objet JSON valide. `process_name` conservé pour l'API.
pub fn inject_parent_pid(json: &str, _process_name: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object_mut()?;
    let info = host_info();
    if let Some(pid) = info.terminal_pid {
        obj.insert("parent_pid".to_string(), serde_json::Value::Number(pid.into()));
    }
    obj.insert(
        "terminal_source".to_string(),
        serde_json::Value::String(info.source),
    );
    serde_json::to_string(obj).ok()
}

/// Champs communs présents dans tout payload de hook event.
pub struct EventCommon {
    pub hook_event_name: String,
    pub transcript_path: String,
    pub session_id: String,
    pub cwd: String,
}

/// Parse les 4 champs partagés. Champs absents = strings vides.
pub fn parse_event_common(json: &str) -> EventCommon {
    let parsed = serde_json::from_str::<serde_json::Value>(json).ok();
    let extract = |k: &str| -> String {
        parsed
            .as_ref()
            .and_then(|v| v.get(k).and_then(|n| n.as_str()).map(str::to_owned))
            .unwrap_or_default()
    };
    EventCommon {
        hook_event_name: extract("hook_event_name"),
        transcript_path: extract("transcript_path"),
        session_id: extract("session_id"),
        cwd: extract("cwd"),
    }
}

// ── Garde anti-double-fire Grok (Story 6.26) ──────────────────────────

/// `true` si le process courant a été lancé PAR Grok. Grok scanne les emplacements de hooks
/// d'autres outils (aujourd'hui `~/.claude/settings.json`, `~/.cursor` ; potentiellement `~/.codex`
/// demain) et exécute les binaires qui s'y trouvent, en injectant `GROK_SESSION_ID` dans
/// l'environnement de tout hook qu'il lance. Tout binaire Vigil **non-Grok** (`vigil-claude`,
/// `vigil-codex`, `vigil-antigravity`) s'en sert comme sentinelle pour se retirer (`exit 0`) et ne
/// pas attribuer une session Grok au mauvais outil. Les vrais outils hôtes (Claude/Codex/Antigravity)
/// ne définissent JAMAIS cette variable → aucune régression. À ne PAS câbler dans `vigil-grok`, qui
/// DOIT au contraire tourner quand `GROK_SESSION_ID` est présent (c'est Grok invoquant son propre
/// binaire). [Story 6.26 : garde étendue aux 3 binaires non-Grok — défense en profondeur.]
pub fn invoked_by_grok() -> bool {
    std::env::var("GROK_SESSION_ID").is_ok()
}

/// Journalise une ligne de diagnostic sur stderr **uniquement** si `VIGIL_DEBUG` est défini dans
/// l'environnement. Silencieux par défaut (aucune sortie) → préserve le fail-open des binaires de
/// hook (l'outil hôte, Grok compris, ne reçoit rien en fonctionnement normal). À utiliser pour
/// rendre traçables les chemins de sortie précoce autrement muets (ex. la garde `invoked_by_grok`),
/// diagnosticables en positionnant `VIGIL_DEBUG=1`.
pub fn debug_log(msg: &str) {
    if std::env::var_os("VIGIL_DEBUG").is_some() {
        eprintln!("[vigil] {msg}");
    }
}

// ── enabled-tools.json (filtrage AC5/AC6) ─────────────────────────────

/// Vérifie si `source_tool` est activé dans `%APPDATA%\Vigil\enabled-tools.json`.
/// Fail-open par défaut (fichier absent, illisible, JSON invalide, clé absente → true).
/// Désactivé uniquement si la clé est explicitement `false`.
pub fn check_tool_enabled(source_tool: &str) -> bool {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return true;
    };
    let path = PathBuf::from(appdata)
        .join("Vigil")
        .join("enabled-tools.json");
    check_tool_enabled_at(&path, source_tool)
}

/// Variante explicitement chemin-paramétrée (testable).
pub fn check_tool_enabled_at(path: &Path, source_tool: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return true;
    };
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return true;
    };
    !matches!(value.get(source_tool), Some(serde_json::Value::Bool(false)))
}

// ── Process snapshot ──────────────────────────────────────────────────

/// Snapshot Windows : pid → (exe_name, parent_pid) pour tous les process.
pub fn process_snapshot() -> Option<HashMap<u32, (String, u32)>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }

        let mut map = HashMap::new();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        while ok {
            let raw: Vec<u16> = entry
                .szExeFile
                .iter()
                .take_while(|&&c| c != 0)
                .copied()
                .collect();
            let name = String::from_utf16_lossy(&raw).to_string();
            map.insert(entry.th32ProcessID, (name, entry.th32ParentProcessID));
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        Some(map)
    }
}

/// Noms de shells hôtes (celui dans lequel l'utilisateur a tapé la commande — il meurt à la
/// fermeture de la fenêtre). Comparaison en minuscules.
const HOST_SHELL_NAMES: &[&str] = &[
    "pwsh.exe",
    "powershell.exe",
    "cmd.exe",
    "bash.exe",
    "sh.exe",
    "zsh.exe",
    "fish.exe",
];

/// Process système / racines de bureau : la remontée s'arrête ici (rien d'intéressant au-dessus).
/// ⚠️ Ne contient PAS les IDE/émulateurs de terminal (`code.exe`, `windowsterminal.exe`…) : ceux-là
/// sont des **conteneurs** = l'hôte à afficher (détectés par [`analyze_ancestry`], voir
/// [`friendly_host_name`]). Comparaison en minuscules.
const SYSTEM_PROCESSES: &[&str] = &[
    "explorer.exe",
    "services.exe",
    "svchost.exe",
    "wininit.exe",
    "winlogon.exe",
    "csrss.exe",
    "smss.exe",
    "userinit.exe",
    "sihost.exe",
    "runtimebroker.exe",
    "dwm.exe",
    "conhost.exe",
    "openconsole.exe",
    "lsass.exe",
    "ctfmon.exe",
];

/// Infos d'hôte dérivées de l'arbre de process au moment où le hook tourne.
pub struct HostInfo {
    /// PID du shell/terminal à surveiller pour la liveness (`None` = pas de terminal fiable, ex. IDE
    /// headless → agent non surveillé, nettoyé par `SessionEnd`).
    pub terminal_pid: Option<u32>,
    /// Libellé convivial de l'hôte d'exécution (« VS Code », « Windows Terminal », « PowerShell »,
    /// « Cursor »…, ou nom de process nettoyé pour un hôte inconnu). Jamais vide.
    pub source: String,
}

/// Retourne le PID du terminal parent à surveiller (`None` = aucun terminal fiable → agent nettoyé
/// par `SessionEnd`). Fin wrapper autour de [`analyze_ancestry`] — voir son POURQUOI. `process_name`
/// est conservé pour la stabilité de l'API (le CLI est désormais repéré génériquement, pas par nom).
pub fn get_terminal_pid_for(_process_name: &str) -> Option<u32> {
    host_info().terminal_pid
}

/// Snapshot + analyse de l'arbre en UNE passe → [`HostInfo`] (`terminal_pid` + `source`).
/// Fail-safe : si le snapshot échoue, renvoie `None` / « Terminal ».
pub fn host_info() -> HostInfo {
    match process_snapshot() {
        Some(map) => analyze_ancestry(&map, std::process::id()),
        None => HostInfo { terminal_pid: None, source: "Terminal".to_string() },
    }
}

/// Analyse l'ascendance du hook et en déduit `terminal_pid` **et** `source` (libellé de l'hôte).
///
/// **Modèle (fix VS Code 2026-07-18 + détection de source générique).** Topologie type :
/// `hook → [shells hook-runner éphémères] → CLI → [shell persistant] → [conteneur IDE/émulateur] →
/// système`. Claude exécute chaque hook via un shell jetable (`bash -c`, ou `cmd /c` pour agy) → ces
/// shells sont SOUS le CLI et meurent aussitôt : on ne doit JAMAIS les retenir comme `terminal_pid`
/// (sinon le `pid_watcher` fauche l'agent en boucle — cas headless VS Code).
///
/// Règles, en remontant depuis le hook :
/// - on saute le binaire de hook, puis le run de shells hook-runner ;
/// - le **1er process non-shell** = le CLI (`claude.exe`/`node.exe`/`codex.exe`/`agy.exe`…, repéré
///   génériquement, pas par nom) ;
/// - `terminal_pid` = le shell hôte le plus externe **au-dessus** du CLI (terminal → `pwsh` ;
///   IDE headless → aucun → `None`) ; repli défensif = shell le plus externe vu si aucun CLI ;
/// - `source` = le **conteneur** au-dessus du CLI (1er non-shell suivant : `Code.exe` → « VS Code »,
///   `WindowsTerminal.exe` → « Windows Terminal », inconnu → nom nettoyé), sinon le **shell
///   persistant** (`pwsh` → « PowerShell », `bash` → « Git Bash »…), sinon « Terminal ».
///
/// Testable (map + start injectés).
fn analyze_ancestry(map: &HashMap<u32, (String, u32)>, start: u32) -> HostInfo {
    let default = || HostInfo { terminal_pid: None, source: "Terminal".to_string() };
    // On saute le process de départ (le binaire de hook lui-même, non-shell).
    let Some((_, first_parent)) = map.get(&start) else { return default() };
    let mut pid = *first_parent;

    let mut passed_cli = false; // a-t-on dépassé le process CLI (1er non-shell) ?
    let mut outermost_above_cli: Option<u32> = None; // candidat terminal_pid valide
    // Repli défensif quand AUCUN CLI n'est trouvé (ancêtre manquant/reparenté). On ne conserve un
    // shell QUE s'il est SEUL et parent direct du hook (cas `…ancetre_manquant_conserve_shell…`).
    // Une CHAÎNE de shells sans CLI = des hook-runner éphémères dont le CLI a disparu du snapshot →
    // ne JAMAIS les retenir (sinon le pid_watcher fauche l'agent → flicker, code-review 6.38).
    let mut below_cli_shell_count: u32 = 0;
    let mut first_below_cli_shell: Option<u32> = None; // shell parent direct du hook (profondeur 0)
    let mut persistent_shell_name: Option<String> = None; // shell au-dessus du CLI (libellé de repli)
    let mut container_name: Option<String> = None; // 1er non-shell au-dessus du CLI (IDE / émulateur)

    for _ in 0..16 {
        let Some((name, parent)) = map.get(&pid) else { break };
        let lname = name.to_ascii_lowercase();

        if HOST_SHELL_NAMES.contains(&lname.as_str()) {
            if passed_cli {
                outermost_above_cli = Some(pid); // shell persistant (au-dessus du CLI)
                if persistent_shell_name.is_none() {
                    persistent_shell_name = Some(lname.clone());
                }
            } else {
                // Shell AVANT tout CLI → hook-runner éphémère présumé (compté pour le repli).
                below_cli_shell_count += 1;
                if first_below_cli_shell.is_none() {
                    first_below_cli_shell = Some(pid);
                }
            }
        } else if SYSTEM_PROCESSES.contains(&lname.as_str()) {
            break; // racine système → fin de la remontée pertinente.
        } else if !passed_cli {
            passed_cli = true; // 1er process non-shell = le CLI.
        } else if container_name.is_none() {
            container_name = Some(lname); // conteneur au-dessus du CLI (IDE / émulateur de terminal).
        }

        if *parent <= 4 {
            break;
        }
        pid = *parent;
    }

    let terminal_pid = if passed_cli {
        outermost_above_cli
    } else if below_cli_shell_count == 1 {
        // Shell direct isolé (aucune chaîne éphémère) : cas défensif ancêtre-manquant légitime.
        first_below_cli_shell
    } else {
        // Chaîne de shells hook-runner sans CLI (ou aucun shell) → None (pas de faux terminal_pid).
        None
    };
    let source = classify_source(container_name.as_deref(), persistent_shell_name.as_deref());
    HostInfo { terminal_pid, source }
}

/// Déduit le libellé de source : conteneur (IDE/émulateur) prioritaire, sinon shell persistant,
/// sinon « Terminal ». Un hôte inconnu retombe sur son nom de process nettoyé (générique auto).
fn classify_source(container: Option<&str>, shell: Option<&str>) -> String {
    if let Some(c) = container {
        return friendly_host_name(c).map(str::to_string).unwrap_or_else(|| clean_process_name(c));
    }
    if let Some(s) = shell {
        return friendly_shell_name(s).map(str::to_string).unwrap_or_else(|| clean_process_name(s));
    }
    "Terminal".to_string()
}

/// Libellé convivial d'un IDE / émulateur de terminal connu (nom de process en minuscules).
fn friendly_host_name(lname: &str) -> Option<&'static str> {
    Some(match lname {
        "code.exe" => "VS Code",
        "cursor.exe" => "Cursor",
        "windsurf.exe" => "Windsurf",
        "devenv.exe" => "Visual Studio",
        "zed.exe" => "Zed",
        "sublime_text.exe" => "Sublime Text",
        "idea64.exe" => "IntelliJ IDEA",
        "pycharm64.exe" => "PyCharm",
        "webstorm64.exe" => "WebStorm",
        "phpstorm64.exe" => "PhpStorm",
        "rider64.exe" => "Rider",
        "clion64.exe" => "CLion",
        "goland64.exe" => "GoLand",
        "rubymine64.exe" => "RubyMine",
        "datagrip64.exe" => "DataGrip",
        "windowsterminal.exe" => "Windows Terminal",
        "alacritty.exe" => "Alacritty",
        "wezterm-gui.exe" => "WezTerm",
        "conemu64.exe" | "conemu.exe" => "ConEmu",
        "hyper.exe" => "Hyper",
        "tabby.exe" => "Tabby",
        "mintty.exe" => "Mintty",
        _ => return None,
    })
}

/// Libellé convivial d'un shell connu (nom de process en minuscules).
fn friendly_shell_name(lname: &str) -> Option<&'static str> {
    Some(match lname {
        "pwsh.exe" | "powershell.exe" => "PowerShell",
        "bash.exe" => "Git Bash",
        "cmd.exe" => "Command Prompt",
        "zsh.exe" => "Zsh",
        "fish.exe" => "Fish",
        "sh.exe" => "Shell",
        _ => return None,
    })
}

/// Nom de process nettoyé pour un hôte inconnu : retire `.exe`, capitalise l'initiale
/// (`newide.exe` → « Newide »). Générique auto — aucun nouvel IDE/terminal à câbler en dur.
fn clean_process_name(lname: &str) -> String {
    let base = lname.strip_suffix(".exe").unwrap_or(lname);
    let mut chars = base.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Terminal".to_string(),
    }
}

// ── Flag d'interrupt (Claude Code) ────────────────────────────────────

/// Consomme atomiquement le flag (rename → read → unlink). `None` si absent.
pub fn consume_flag(path: &Path) -> Option<String> {
    let tmp = path.with_extension("consuming");
    let _ = std::fs::remove_file(&tmp);
    std::fs::rename(path, &tmp).ok()?;
    let content = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    Some(content)
}

/// Hash SHA256-tronqué (**16 octets → 32 hex**) du chemin de transcript — nom des fichiers
/// `%TEMP%` partagés avec l'app Vigil (flag d'interrupt, marqueur SessionStart Antigravity).
///
/// ⚠️ **CONTRAT INTER-CRATES — doit rester byte-identique à `vigil::flags::transcript_hash`**
/// (`src-tauri/src/flags.rs`). L'app ÉCRIT le flag d'interrupt avec sa version, ce binaire le
/// CHERCHE avec celle-ci : toute divergence = nom de fichier différent = flag jamais trouvé =
/// **interruption morte en silence** (aucune erreur, le tour continue normalement).
///
/// C'est arrivé : la story 4.14 a élargi la version de l'app 8 → 16 octets en ne mettant à jour
/// que SON test ; cette copie est restée à 8 → interruption cassée pour TOUS les outils du
/// 2026-05-18 au 2026-07-17 (~2 mois), sans un seul test rouge. Verrouillé depuis par
/// `transcript_hash_contrat_inter_crates` ici ET côté app (mêmes vecteurs).
/// Toute modification ici DOIT être répercutée là-bas, et réciproquement.
pub fn transcript_hash(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    let r = h.finalize();
    let mut out = String::with_capacity(32);
    for byte in &r[..16] {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// Émet un événement synthétique `Stop` sur le pipe Vigil (best-effort).
/// `source_tool` est inclus dans le payload pour que la story 6.2 puisse l'exploiter.
pub fn send_synthetic_stop(session_id: &str, transcript_path: &str, cwd: &str, source_tool: &str) {
    let stop = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "source_tool": source_tool,
        // Story 8.1 (revue) — marque ce `Stop` comme SYNTHÉTIQUE, c'est-à-dire provoqué par le
        // bouton « Interrompre » de Vigil. Sans ce drapeau, l'app ne peut pas distinguer une
        // interruption d'une fin naturelle (le seul autre signal, `PostToolUseFailure
        // { is_interrupt: true }`, n'est émis par AUCUN outil) et notifie « ✅ terminé »
        // l'utilisateur qui vient précisément de couper son agent.
        // ⚠️ Contrat partagé avec `StopPayload` (src-tauri/src/pipe/protocol.rs) : le champ y est
        // `#[serde(default)]`, donc un hook non réinstallé dégrade sans casser.
        "interrupted": true,
    });
    let stop_str = stop.to_string();
    if let Ok(mut pipe) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\vigil")
    {
        let _ = pipe.write_all(stop_str.as_bytes());
        let _ = pipe.write_all(b"\n");
        let _ = pipe.flush();
    }
}

/// Gère le flag d'interrupt pour `PreToolUse`/`PostToolUse` (format Claude Code).
/// Retourne `true` si l'interrupt a été consommé : l'appelant doit alors `exit(0)`.
/// Réutilisable pour Codex/Grok plus tard si le besoin se présente.
pub fn handle_interrupt_flag_claude_code(
    hook_event: &str,
    transcript_path: &str,
    session_id: &str,
    cwd: &str,
    source_tool: &str,
) -> bool {
    if (hook_event != "PreToolUse" && hook_event != "PostToolUse") || transcript_path.is_empty() {
        return false;
    }
    let hash = transcript_hash(transcript_path);
    let interrupt_path = std::env::temp_dir().join(format!("vigil-interrupt-{}.flag", hash));

    let Some(reason_content) = consume_flag(&interrupt_path) else {
        return false;
    };
    let reason = reason_content.trim();

    if reason.is_empty() {
        // Format 1 : arrêt dur, pas de raison transmise à Claude
        print!(r#"{{"continue": false, "stopReason": "Interrupted by user"}}"#);
    } else if hook_event == "PostToolUse" {
        // Format 2 : Claude reçoit la raison en contexte (valide pour PostToolUse)
        let block_output = serde_json::json!({
            "decision": "block",
            "reason": format!("Interrupted by user: {}", reason)
        });
        print!("{}", block_output);
    } else {
        // Format 3 : PreToolUse permission deny — Claude reçoit permissionDecisionReason
        let hook_output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "permissionDecision": "deny",
                "permissionDecisionReason": format!("Interrupted by user: {}", reason)
            }
        });
        print!("{}", hook_output);
    }

    let _ = io::stdout().flush();
    send_synthetic_stop(session_id, transcript_path, cwd, source_tool);
    true
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── get_terminal_pid_for / outermost_host_shell_pid (Story 6.20) ──────────
    // Topologies réelles capturées sur le poste (proc_tree.ps1). Le hook (PID fictif 99999)
    // est un descendant du CLI. On vérifie qu'on remonte au bon shell hôte (celui qui meurt
    // à la fermeture de la fenêtre), identique pour Claude, corrigé pour Codex/agy.

    fn proc(name: &str, parent: u32) -> (String, u32) {
        (name.to_string(), parent)
    }

    /// Raccourci : `terminal_pid` seul (les tests historiques n'assertent que lui).
    fn term_pid(map: &HashMap<u32, (String, u32)>, start: u32) -> Option<u32> {
        analyze_ancestry(map, start).terminal_pid
    }

    #[test]
    fn shell_hote_codex_saute_node_intermediaire() {
        // hook → node(41352) → pwsh(34264) → WindowsTerminal(24216). Attendu : pwsh 34264.
        let map = HashMap::from([
            (99999u32, proc("vigil-codex.exe", 41352)),
            (41352, proc("node.exe", 34264)),
            (34264, proc("pwsh.exe", 24216)),
            (24216, proc("WindowsTerminal.exe", 1884)),
        ]);
        assert_eq!(term_pid(&map, 99999), Some(34264));
    }

    #[test]
    fn shell_hote_agy_ignore_wrapper_cmd_ephemere() {
        // hook → cmd/c(50000) → agy(38496) → pwsh(29452) → explorer. Attendu : pwsh 29452,
        // PAS le cmd /c éphémère interne (écrasé par le shell externe).
        let map = HashMap::from([
            (99999u32, proc("vigil-antigravity.exe", 50000)),
            (50000, proc("cmd.exe", 38496)),
            (38496, proc("agy.exe", 29452)),
            (29452, proc("pwsh.exe", 700)),
            (700, proc("explorer.exe", 1884)),
        ]);
        assert_eq!(term_pid(&map, 99999), Some(29452));
    }

    #[test]
    fn shell_hote_claude_inchange() {
        // hook → claude.exe(19540) → pwsh(3632) → WindowsTerminal. Attendu : pwsh 3632 —
        // identique à l'ancien comportement (parent de claude.exe), non-régression Claude.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 19540)),
            (19540, proc("claude.exe", 3632)),
            (3632, proc("pwsh.exe", 24216)),
            (24216, proc("WindowsTerminal.exe", 1884)),
        ]);
        assert_eq!(term_pid(&map, 99999), Some(3632));
    }

    #[test]
    fn shell_hote_aucun_shell_retourne_none() {
        // Chaîne sans shell hôte → None. Depuis le fix VS Code (2026-07-18), il n'y a PLUS de repli
        // `legacy_parent_of_named` : None est la valeur VOULUE (session headless non surveillée par
        // liveness, nettoyée par SessionEnd) — évite de fabriquer un terminal_pid éphémère.
        let map = HashMap::from([
            (99999u32, proc("vigil-codex.exe", 41352)),
            (41352, proc("node.exe", 24216)),
            (24216, proc("WindowsTerminal.exe", 1884)),
        ]);
        assert_eq!(term_pid(&map, 99999), None);
    }

    #[test]
    fn shell_hote_claude_vscode_headless_retourne_none() {
        // BUG VS Code (fix 2026-07-18) : l'extension lance claude HEADLESS, directement sous Code.exe,
        // SANS shell hôte. Topologie réelle capturée sur le poste (diag proc-tree) :
        // hook → claude.exe(27080) → Code.exe(25800) → Code.exe(28412). Aucun pwsh/cmd/bash.
        // Attendu = None → l'agent n'aura pas de terminal_pid → PAS de faux nettoyage par le
        // pid_watcher. Avant le fix, l'ancien repli renvoyait le parent (éphémère) → flicker.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 27080)),
            (27080, proc("claude.exe", 25800)),
            (25800, proc("Code.exe", 28412)),
            (28412, proc("Code.exe", 10164)),
        ]);
        assert_eq!(term_pid(&map, 99999), None);
    }

    #[test]
    fn hook_runner_bash_vscode_retourne_none() {
        // Topologie RÉELLE capturée (log hook, 2026-07-18) — VS Code + Git Bash. Claude exécute le
        // hook via `bash -c` → paire de bash ÉPHÉMÈRES sous claude.exe, puis Code.exe (pas de shell
        // au-dessus du CLI). Attendu = None : les bash hook-runner ne doivent PAS devenir terminal_pid
        // (sinon pid_watcher les voit morts au tick suivant → agent fauché en boucle).
        let map = HashMap::from([
            (8584u32, proc("vigil-claude.exe", 39988)),
            (39988, proc("bash.exe", 26280)),
            (26280, proc("bash.exe", 41096)),
            (41096, proc("claude.exe", 13624)),
            (13624, proc("Code.exe", 28412)),
            (28412, proc("Code.exe", 10164)),
        ]);
        assert_eq!(term_pid(&map, 8584), None);
    }

    #[test]
    fn hook_runner_bash_terminal_retourne_shell_persistant() {
        // Topologie RÉELLE capturée (log hook, 2026-07-18) — même bash hook-runner éphémère, MAIS
        // vraie session terminal : au-dessus de claude.exe se trouve le `pwsh` PERSISTANT sous
        // WindowsTerminal. Attendu = pwsh 31324 (le shell au-dessus du CLI), PAS les bash sous lui.
        let map = HashMap::from([
            (7984u32, proc("vigil-claude.exe", 28084)),
            (28084, proc("bash.exe", 41076)),
            (41076, proc("bash.exe", 8256)),
            (8256, proc("claude.exe", 31324)),
            (31324, proc("pwsh.exe", 25664)),
            (25664, proc("WindowsTerminal.exe", 1888)),
        ]);
        assert_eq!(term_pid(&map, 7984), Some(31324));
    }

    #[test]
    fn shell_hote_ancetre_manquant_conserve_shell_deja_trouve() {
        // hook → pwsh(3632) → parent 24216 ABSENT du snapshot (process mort/reparenté). L'ancien
        // `?` renvoyait None → terminal_pid=None → agent jamais nettoyé. Le `let-else break`
        // conserve le shell déjà trouvé plus bas.
        let map = HashMap::from([
            (99999u32, proc("vigil-codex.exe", 3632)),
            (3632, proc("pwsh.exe", 24216)),
            // 24216 absent : la remontée s'arrête sans jeter le shell 3632.
        ]);
        assert_eq!(term_pid(&map, 99999), Some(3632));
    }

    #[test]
    fn hook_runner_chain_cli_manquant_retourne_none() {
        // Régression code-review 6.38 (fix affiné) : si le CLI (claude) est ABSENT du snapshot
        // (race/reparenté), la CHAÎNE de bash hook-runner ne doit PAS devenir terminal_pid (sinon
        // le pid_watcher la voit morte → flicker). Chaîne ≥2 shells sans CLI → None. À contraster
        // avec `shell_hote_ancetre_manquant_conserve_shell_deja_trouve` (shell UNIQUE direct → gardé).
        let map = HashMap::from([
            (8584u32, proc("vigil-claude.exe", 39988)),
            (39988, proc("bash.exe", 26280)),
            (26280, proc("bash.exe", 41096)),
            // 41096 (claude.exe) ABSENT du snapshot → la remontée casse sans jamais voir de CLI.
        ]);
        assert_eq!(term_pid(&map, 8584), None);
    }

    #[test]
    fn shell_hote_avec_parent_system_est_enregistre() {
        // hook → pwsh(3632) dont le parent est System (<=4). Le record AVANT le break parent<=4
        // garantit que ce shell hôte le plus externe compte quand même.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 19540)),
            (19540, proc("claude.exe", 3632)),
            (3632, proc("pwsh.exe", 4)),
        ]);
        assert_eq!(term_pid(&map, 99999), Some(3632));
    }

    // ── détection de source (libellé d'hôte) ──────────────────────────

    fn src(map: &HashMap<u32, (String, u32)>, start: u32) -> String {
        analyze_ancestry(map, start).source
    }

    #[test]
    fn source_vscode() {
        // Topologie réelle : hook → bash → bash → claude → Code.exe → Code.exe.
        let map = HashMap::from([
            (8584u32, proc("vigil-claude.exe", 39988)),
            (39988, proc("bash.exe", 26280)),
            (26280, proc("bash.exe", 41096)),
            (41096, proc("claude.exe", 13624)),
            (13624, proc("Code.exe", 28412)),
            (28412, proc("Code.exe", 10164)),
        ]);
        assert_eq!(src(&map, 8584), "VS Code");
    }

    #[test]
    fn source_windows_terminal_conteneur_prioritaire_sur_shell() {
        // Terminal réel : le conteneur (Windows Terminal) l'emporte sur le shell (pwsh).
        let map = HashMap::from([
            (7984u32, proc("vigil-claude.exe", 28084)),
            (28084, proc("bash.exe", 41076)),
            (41076, proc("bash.exe", 8256)),
            (8256, proc("claude.exe", 31324)),
            (31324, proc("pwsh.exe", 25664)),
            (25664, proc("WindowsTerminal.exe", 1888)),
            (1888, proc("svchost.exe", 1652)),
        ]);
        assert_eq!(src(&map, 7984), "Windows Terminal");
    }

    #[test]
    fn source_powershell_console_nue_repli_sur_shell() {
        // pwsh lancé directement (parent = explorer, pas d'émulateur) → repli sur le shell.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 19540)),
            (19540, proc("claude.exe", 3632)),
            (3632, proc("pwsh.exe", 5000)),
            (5000, proc("explorer.exe", 1884)),
        ]);
        assert_eq!(src(&map, 99999), "PowerShell");
    }

    #[test]
    fn source_cursor() {
        // Cursor (fork VS Code) lance claude headless sous Cursor.exe → « Cursor ».
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 40000)),
            (40000, proc("bash.exe", 41000)),
            (41000, proc("claude.exe", 42000)),
            (42000, proc("Cursor.exe", 43000)),
            (43000, proc("Cursor.exe", 10)),
        ]);
        assert_eq!(src(&map, 99999), "Cursor");
    }

    #[test]
    fn source_ide_inconnu_nom_nettoye_generique() {
        // Générique auto : un IDE inconnu (headless sous NewEditor.exe) → nom de process nettoyé.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 40000)),
            (40000, proc("bash.exe", 41000)),
            (41000, proc("claude.exe", 42000)),
            (42000, proc("NewEditor.exe", 10)),
        ]);
        assert_eq!(src(&map, 99999), "Neweditor");
    }

    #[test]
    fn source_git_bash_sans_conteneur() {
        // Session directement dans Git Bash (pas de conteneur émulateur reconnu au-dessus).
        // hook → bash(hook-runner) → claude → bash(persistant) → explorer → repli shell = Git Bash.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 40000)),
            (40000, proc("bash.exe", 41000)),
            (41000, proc("claude.exe", 42000)),
            (42000, proc("bash.exe", 43000)),
            (43000, proc("explorer.exe", 10)),
        ]);
        assert_eq!(src(&map, 99999), "Git Bash");
    }

    #[test]
    fn source_defaut_terminal_si_rien() {
        // Aucun shell/conteneur identifiable → « Terminal » par défaut.
        let map = HashMap::from([
            (99999u32, proc("vigil-claude.exe", 42000)),
            (42000, proc("claude.exe", 10)),
        ]);
        assert_eq!(src(&map, 99999), "Terminal");
    }

    // ── inject_source_tool ────────────────────────────────────────────

    #[test]
    fn source_tool_injecte_correctement() {
        let input = r#"{"hook_event_name":"SessionStart","session_id":"abc"}"#;
        let out = inject_source_tool(input, "codex");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["source_tool"], "codex");
        assert_eq!(parsed["hook_event_name"], "SessionStart");
        assert_eq!(parsed["session_id"], "abc");
    }

    #[test]
    fn source_tool_n_ecrase_pas_existant() {
        let input = r#"{"hook_event_name":"SessionStart","source_tool":"claude_code"}"#;
        let out = inject_source_tool(input, "codex");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["source_tool"], "claude_code",
            "source_tool déjà présent doit être préservé (défensif)"
        );
    }

    #[test]
    fn source_tool_robuste_si_json_invalide() {
        // Robustesse documentée : retourne la string originale
        let bad = "pas du json {{{";
        let out = inject_source_tool(bad, "codex");
        assert_eq!(out, bad);
    }

    // ── check_tool_enabled_at (fixtures fichier) ──────────────────────

    fn unique_tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "vigil_test_{}_{}.json",
            name,
            std::process::id()
        ))
    }

    #[test]
    fn outil_desactive_exit_0() {
        let tmp = unique_tmp("desactive");
        std::fs::write(&tmp, r#"{"grok":false}"#).unwrap();
        let r = check_tool_enabled_at(&tmp, "grok");
        std::fs::remove_file(&tmp).ok();
        assert!(!r, "grok=false doit retourner false");
    }

    #[test]
    fn outil_active_retourne_true() {
        let tmp = unique_tmp("active");
        std::fs::write(&tmp, r#"{"claude_code":true}"#).unwrap();
        let r = check_tool_enabled_at(&tmp, "claude_code");
        std::fs::remove_file(&tmp).ok();
        assert!(r, "claude_code=true doit retourner true");
    }

    #[test]
    fn fail_open_si_config_absente() {
        let tmp = unique_tmp("absent");
        std::fs::remove_file(&tmp).ok(); // garantit l'absence
        assert!(
            check_tool_enabled_at(&tmp, "claude_code"),
            "fichier absent → fail-open (true)"
        );
    }

    #[test]
    fn fail_open_si_json_invalide() {
        let tmp = unique_tmp("invalide");
        std::fs::write(&tmp, "pas du json {{{").unwrap();
        let r = check_tool_enabled_at(&tmp, "claude_code");
        std::fs::remove_file(&tmp).ok();
        assert!(r, "JSON invalide → fail-open (true)");
    }

    #[test]
    fn fail_open_si_outil_absent_du_json() {
        let tmp = unique_tmp("clef_absente");
        std::fs::write(&tmp, r#"{"claude_code":true}"#).unwrap();
        let r = check_tool_enabled_at(&tmp, "codex");
        std::fs::remove_file(&tmp).ok();
        assert!(r, "clé absente du JSON → fail-open (true), pas désactivé");
    }

    #[test]
    fn check_tool_enabled_at_strip_bom() {
        // Le fichier peut être créé par un éditeur Windows avec BOM UTF-8.
        let tmp = unique_tmp("bom");
        let with_bom = format!("\u{FEFF}{}", r#"{"grok":false}"#);
        std::fs::write(&tmp, with_bom).unwrap();
        let r = check_tool_enabled_at(&tmp, "grok");
        std::fs::remove_file(&tmp).ok();
        assert!(!r, "Le BOM doit être strippé avant parsing");
    }

    // ── parse_event_common ────────────────────────────────────────────

    #[test]
    fn parse_event_common_extrait_les_4_champs() {
        let input = r#"{
            "hook_event_name":"PreToolUse",
            "transcript_path":"C:\\transcripts\\foo.jsonl",
            "session_id":"sess-1",
            "cwd":"C:\\code"
        }"#;
        let c = parse_event_common(input);
        assert_eq!(c.hook_event_name, "PreToolUse");
        assert_eq!(c.transcript_path, r"C:\transcripts\foo.jsonl");
        assert_eq!(c.session_id, "sess-1");
        assert_eq!(c.cwd, r"C:\code");
    }

    #[test]
    fn parse_event_common_champs_manquants_retournent_vide() {
        let c = parse_event_common(r#"{"hook_event_name":"Stop"}"#);
        assert_eq!(c.hook_event_name, "Stop");
        assert_eq!(c.transcript_path, "");
        assert_eq!(c.session_id, "");
        assert_eq!(c.cwd, "");
    }

    // ── transcript_hash ───────────────────────────────────────────────

    #[test]
    fn transcript_hash_deterministe() {
        let a = transcript_hash("/foo/bar.jsonl");
        let b = transcript_hash("/foo/bar.jsonl");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32); // 16 octets en hex
    }

    /// Story 6.36 — VERROU du contrat inter-crates. Ces vecteurs sont DUPLIQUÉS à l'identique dans
    /// `vigil::flags::transcript_hash_contrat_inter_crates` (`src-tauri/src/flags.rs`) : les deux
    /// crates ne pouvant pas se dépendre, c'est le seul moyen mécanique de faire ROUGIR un test si
    /// l'une des deux copies dérive. Ne PAS « corriger » une valeur attendue ici sans corriger
    /// l'autre côté — une divergence = interruption morte en silence (cf. régression 4.14, ~2 mois).
    #[test]
    fn transcript_hash_contrat_inter_crates() {
        // sha256("/foo/bar.jsonl")[..16] et sha256("C:\\x\\y.jsonl")[..16], en hex.
        assert_eq!(transcript_hash("/foo/bar.jsonl"), "92cd7836e480832bdb9be281b4752a79");
        assert_eq!(transcript_hash(r"C:\x\y.jsonl"), "fe29a574fcf05fcf7409c4fdf4d0f33a");
    }

    // ── invoked_by_grok (Story 6.26 — garde anti-double-fire) ─────────

    #[test]
    fn invoked_by_grok_detecte_la_sentinelle() {
        // Var d'env process-globale : seul ce test touche GROK_SESSION_ID.
        std::env::remove_var("GROK_SESSION_ID");
        assert!(!invoked_by_grok(), "absente → false (session Claude réelle)");

        std::env::set_var("GROK_SESSION_ID", "sess-grok-123");
        assert!(invoked_by_grok(), "présente → true (lancé par Grok)");

        // Même vide, la présence de la variable suffit (Grok l'injecte toujours).
        std::env::set_var("GROK_SESSION_ID", "");
        assert!(invoked_by_grok(), "présente-mais-vide → true");

        std::env::remove_var("GROK_SESSION_ID");
        assert!(!invoked_by_grok());
    }
}
