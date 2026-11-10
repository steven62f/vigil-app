use std::path::{Path, PathBuf};
use anyhow::Context;
use serde_json::{json, Value};
use tauri::AppHandle;
use crate::agent::state::SourceTool;
use crate::pipe::manager::hook_timeout_ceiling_secs;

mod hook_mirror;

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart", "SessionEnd",
    "PreToolUse", "PostToolUse", "PostToolUseFailure",
    "PermissionRequest", "Notification",
    "Stop", "StopFailure",
    "UserPromptSubmit",
    "SubagentStart", "SubagentStop",
];

const CODEX_EVENTS: &[&str] = &[
    "SessionStart", "Stop",
    "PreToolUse", "PostToolUse",
    "PermissionRequest",
    // Story 6.14 — sous-agents Codex + prompt. Chemin descendant DÉJÀ en place et
    // agnostique de l'outil (protocol.rs variantes + state_engine handlers + get_transcript_path).
    // Events fire-and-forget : simplement forwardés au pipe (aucun stdout attendu, cf. main.rs).
    "UserPromptSubmit", "SubagentStart", "SubagentStop",
];

// Story 6.26 — validés au spike (2026-07-11, poste Steven, Grok Build installé). Clés d'events en
// PascalCase, globbées par Grok depuis `~/.grok/hooks/*.json`. `UserPromptSubmit` ajouté (pilotage
// d'état, cf. Claude/Codex) ; `SessionEnd` enregistré pour parité/futur (n'a PAS tiré au `/quit` au
// spike → le nettoyage d'agent repose sur le pid_watcher, pas sur SessionEnd).
// Story 6.31 — sous-agents Grok. ⚠️ PIÈGE NOMMAGE (spike 2026-07-12, mémoire
// `project_grok_hook_schema`) : la clé de hook Grok est `SubagentEnd`, PAS `SubagentStop`
// (= réflexe Claude/Codex → clé MORTE chez Grok, 0 tir). Incohérence interne Grok : la clé
// `SubagentEnd` fait tirer un payload dont `hookEventName == "subagent_end"` (c'est ce tag stdin
// que `vigil-grok::translate` matche, pas la clé). Ces 2 events sont émis CÔTÉ PARENT et
// fire-and-forget (forwardés au pipe, aucun stdout attendu) — cf. state_engine handlers
// `SubagentStart`/`SubagentStop` (agnostiques, story 6.14).
// Story 6.34 — État & erreurs Grok. `Notification` (fin de tour, question, agent_error) +
// `StopFailure` (échec de tour, ex. rate-limit 429) TIRENT au spike (clés PascalCase, tags stdin
// `notification`/`stop_failure` confirmés capture `events-run2.jsonl`). Traduits par `vigil-grok`
// vers les handlers socle (agnostiques) `state_engine.rs` — badge `Error` + libellé d'activité,
// réutilisés INCHANGÉS. Suppression sous-agent GRATUITE (garde 6.31 couvre déjà ces events).
// Story 6.37 — Échec d'appel d'outil Grok. `PostToolUseFailure` (event natif chez Grok, clé
// PascalCase, tag stdin `post_tool_use_failure` — à confirmer e2e). Parité socle avec Claude :
// handler `state_engine.rs:127` réutilisé INCHANGÉ → label "Failed: <tool>", agent reste Working.
// `is_interrupt` toujours false pour Grok (hors INTERRUPT_SUPPORTED 6.33) → jamais le chemin
// interrupt. Suppression sous-agent GRATUITE (garde 6.31 couvre aussi cet event). Event passif
// (aucun stdout attendu), fire-and-forget comme tous les autres états/erreurs.
const GROK_EVENTS: &[&str] = &[
    "SessionStart", "UserPromptSubmit",
    "PreToolUse", "PostToolUse", "PostToolUseFailure",
    "Stop", "StopFailure", "SessionEnd",
    "Notification",
    "SubagentStart", "SubagentEnd",
];

// Story 6.18 — Antigravity CLI (`agy`). Les 4 events exploités par le binaire 6.17.
// `PostInvocation` volontairement IGNORÉ : fire-and-forget après tool calls, aucune synthèse à
// porter → on évite du bruit sur le pipe. `PreInvocation` EST enregistré car il porte le 1er
// event d'un tour → garantit la synthèse `SessionStart` (seul porteur du modelName) même sans
// appel d'outil. Deux structures selon l'event (cf. doc hooks.md) :
//   - Grouped (PreToolUse/PostToolUse) : wrapper `matcher` + `hooks[]`.
//   - Flat (PreInvocation/Stop) : liste de handlers directs.
// ⚠️ Casse VERBATIM : le binaire compare `event_name == "PreToolUse"` (sensible à la casse) pour
// répondre `{"decision":"allow"}` — ne jamais transformer la casse de ces libellés.
const ANTIGRAVITY_EVENTS: &[&str] = &["PreToolUse", "PostToolUse", "PreInvocation", "Stop"];
const ANTIGRAVITY_GROUPED_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

/// Nom du hook nommé (clé top-level du `hooks.json` Antigravity) réservé à Vigil. ≠ Codex dont
/// le top-level est directement `"hooks"` : ici c'est un NOM de hook mappant vers des events.
const ANTIGRAVITY_HOOK_NAME: &str = "vigil";

/// `true` si l'event Antigravity utilise la structure Grouped (`matcher` + `hooks[]`),
/// `false` s'il est Flat (liste de handlers directs).
fn antigravity_is_grouped(event: &str) -> bool {
    ANTIGRAVITY_GROUPED_EVENTS.contains(&event)
}

// ── Structures ───────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ToolsStatus {
    pub claude_code_detected: bool,
    pub codex_detected: bool,
    pub grok_detected: bool,
    pub antigravity_detected: bool,
    pub kimi_detected: bool,
    pub claude_code_enabled: bool,
    pub codex_enabled: bool,
    pub grok_enabled: bool,
    pub antigravity_enabled: bool,
    pub kimi_enabled: bool,
    pub claude_code_hooks_ok: bool,
    pub codex_hooks_ok: bool,
    pub grok_hooks_ok: bool,
    pub antigravity_hooks_ok: bool,
    pub kimi_hooks_ok: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct EnabledTools {
    #[serde(default)]
    pub claude_code: bool,
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub grok: bool,
    #[serde(default)]
    pub antigravity: bool,
    /// ⚠️ Nom de champ VERROUILLÉ : `EnabledTools` sérialise par nom de champ, et `vigil-kimi`
    /// interroge la clé littérale `"kimi"` (`vigil-kimi/src/main.rs` → `check_tool_enabled`). Un nom
    /// divergent ne casserait RIEN de visible — `check_tool_enabled` est **fail-open** sur clé
    /// absente (`vigil-hook-core/src/lib.rs:163`) — mais la case « Disabled » de la page
    /// Installation n'aurait alors plus AUCUN effet sur les hooks Kimi. Verrouillé par le test
    /// `enabled_tools_serialise_la_cle_kimi`.
    #[serde(default)]
    pub kimi: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolHookDiff {
    pub tool: String,
    pub config_file_path: String,
    pub before: String,
    pub after: String,
    pub hook_exe_path: String,
}

// ── Path helpers ─────────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let drive = std::env::var("HOMEDRIVE").unwrap_or_else(|_| "C:".to_string());
            let path = std::env::var("HOMEPATH").unwrap_or_else(|_| "\\".to_string());
            PathBuf::from(format!("{}{}", drive, path))
        })
}

pub fn get_claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

pub fn get_codex_hooks_path() -> PathBuf {
    home_dir().join(".codex").join("hooks.json")
}

/// Fichier de hook Grok DÉDIÉ que Vigil possède intégralement. Grok globbe `~/.grok/hooks/*.json`
/// (validé spike 6.26) → install = écrire ce fichier, uninstall = le supprimer (pas de merge tiers,
/// ≠ Claude/Codex/Antigravity). ⚠️ L'ancien `~/.grok/user-settings.json` était un FICHIER FANTÔME
/// jamais lu par Grok — abandonné.
pub fn get_grok_hooks_file_path() -> PathBuf {
    home_dir().join(".grok").join("hooks").join("vigil.json")
}

/// `hooks.json` global d'Antigravity — customization root global `~/.gemini/config/hooks.json`
/// (la variante workspace `<repo>/.agents/hooks.json` est hors scope : on gère le global comme
/// Codex/Grok).
pub fn get_antigravity_hooks_path() -> PathBuf {
    home_dir().join(".gemini").join("config").join("hooks.json")
}

fn get_enabled_tools_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    if appdata.is_empty() {
        return home_dir().join("AppData").join("Roaming").join("Vigil").join("enabled-tools.json");
    }
    PathBuf::from(appdata).join("Vigil").join("enabled-tools.json")
}

/// Basename du binaire de hook de chaque outil — **table unique**, source des cinq résolutions de
/// chemin ET du contrôle de fraîcheur du miroir (Story 10.3, AC4).
///
/// Un sixième outil qu'on oublierait d'inscrire ici perdrait silencieusement le miroir *et* sa
/// détection de dérive : `hook_binary_basename_couvre_les_5_outils` verrouille la table.
fn hook_binary_basename(tool: &str) -> Option<&'static str> {
    match tool {
        "claude_code" => Some(VIGIL_CLAUDE_BASENAME),
        "codex" => Some(VIGIL_CODEX_BASENAME),
        "grok" => Some(VIGIL_GROK_BASENAME),
        "antigravity" => Some(VIGIL_ANTIGRAVITY_BASENAME),
        "kimi" => Some(VIGIL_KIMI_BASENAME),
        _ => None,
    }
}

/// Convention d'écriture du chemin d'exe, **par outil** — table unique, comme `hook_binary_basename`.
///
/// `true` = forward-slashes (Claude/Codex/Grok : invocation via bash, où le backslash échappe).
/// `false` = backslashes natifs (Antigravity/Kimi : invocation via `cmd`, où un forward-slash en
/// tête serait lu comme un délimiteur d'option).
///
/// ⚠️ **Extraite en revue 10.3.** La valeur était écrite en dur aux cinq appels de `resolve_hook_exe`
/// et re-déclarée une SIXIÈME fois dans les tests : un `true` posé par erreur sur Kimi aurait passé
/// tous les tests en envoyant des forward-slashes à `cmd` — la classe de défaut même que cette story
/// dit avoir unifiée. Les tests lisent désormais CETTE table, et exercent le chemin de production.
fn hook_uses_bash_slashes(tool: &str) -> Option<bool> {
    match tool {
        "claude_code" | "codex" | "grok" => Some(true),
        "antigravity" | "kimi" => Some(false),
        _ => None,
    }
}

/// Emplacement SOURCE d'un binaire de hook : à côté de l'exe principal (`%LOCALAPPDATA%\vigil\`),
/// là où le bundle NSIS le dépose. C'est ce chemin — porteur de l'espace du nom de compte — que la
/// Story 10.3 recopie vers le miroir `%ProgramData%` avant de l'inscrire dans une config d'outil.
fn vigil_hook_binary_path(basename: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(basename)))
        .unwrap_or_else(|| PathBuf::from(basename))
}

// Les cinq `get_vigil_<tool>_path` ont disparu en revue 10.3 : c'étaient des enveloppes d'une ligne
// autour de `vigil_hook_binary_path(<BASENAME>)`, et `hook_exe_for_tool` déduit désormais le
// basename de la table `hook_binary_basename`. Une table, pas cinq portes d'entrée.

/// Story 10.3 — Cascade de résolution du chemin d'exe de hook : **miroir sans espace** → forme
/// courte 8.3 → chemin long (AC1, AC5).
///
/// Le miroir (`%ProgramData%\Vigil\hooks\<SID>\`, cf. `hook_mirror`) SUPPRIME l'espace au lieu
/// d'apprendre à chaque outil comment y survivre : cinq comportements de quoting différents, dont
/// **trois réfutés à la sonde** (Antigravity appende la commande brute, Grok découpe sur les espaces
/// sans shell, Codex annonce « completed » sans exécuter — spike F4bis/F6/F7). Un invariant unique
/// remplace cinq cas particuliers.
///
/// La 8.3 n'est plus qu'un **repli** : `GetShortPathNameW` réussit mais rend le chemin LONG quand la
/// génération 8.3 est désactivée sur le volume, donc l'espace y survit — c'est précisément le
/// fail-open silencieux que le miroir referme. Aucun étage de la cascade ne produit de guillemets
/// (AC8) : ils cassent Grok à eux seuls, indépendamment de l'espace.
///
/// `bash_slashes` = mise en forme propre à l'outil, INCHANGÉE par cette story : forward-slashes pour
/// Claude/Codex/Grok (invocation via bash, où le backslash échappe), backslashes natifs pour
/// Antigravity/Kimi (invocation via `cmd`, où un forward-slash en tête serait lu comme une option).
fn resolve_hook_exe(src: &Path, bash_slashes: bool) -> String {
    let mirror = hook_mirror::ensure_mirrored(src);
    #[cfg(target_os = "windows")]
    let short = match mirror {
        Some(_) => None,
        None => to_short_path(src),
    };
    #[cfg(not(target_os = "windows"))]
    let short: Option<String> = None;
    let cmd = hook_mirror::hook_command_core(src, mirror.as_deref(), short.as_deref(), bash_slashes);
    // Dernier filet : l'invariant de la story évalué sur la valeur RÉELLEMENT produite (revue 10.3).
    hook_mirror::warn_if_space_survives(&cmd);
    cmd
}

/// Résolution du chemin de hook **SANS aucun effet de bord** : le miroir est CONSULTÉ, jamais créé,
/// jamais durci, jamais rempli.
///
/// Existe pour le bornage décidé en revue 10.3 : `are_hooks_conformant_for_tool` est appelé à chaque
/// requête d'état et pour les cinq outils, et n'a aucune raison de poser des dossiers, réécrire des
/// DACL ou copier des binaires pour un outil que l'utilisateur n'a **jamais activé**. La comparaison
/// avec la config, elle, est identique — c'est bien le même chemin qui est attendu.
fn resolve_hook_exe_readonly(src: &Path, bash_slashes: bool) -> String {
    let mirror = hook_mirror::mirror_target(src).filter(|t| t.is_file());
    #[cfg(target_os = "windows")]
    let short = match mirror {
        Some(_) => None,
        None => to_short_path(src),
    };
    #[cfg(not(target_os = "windows"))]
    let short: Option<String> = None;
    hook_mirror::hook_command_core(src, mirror.as_deref(), short.as_deref(), bash_slashes)
}

/// Aiguillage entre la résolution qui PROVISIONNE le miroir et celle qui se contente de le lire.
fn resolve_hook_exe_for(src: &Path, bash_slashes: bool, provision: bool) -> String {
    if provision {
        resolve_hook_exe(src, bash_slashes)
    } else {
        resolve_hook_exe_readonly(src, bash_slashes)
    }
}

/// Commande d'exe de hook prête à être inscrite dans la config de `tool` — **point d'entrée unique**.
///
/// Tout est déduit du nom de l'outil : le basename (`hook_binary_basename`) et la convention
/// d'écriture (`hook_uses_bash_slashes`). C'est ce que les tests exercent, plutôt qu'une
/// réimplémentation de la cascade.
fn hook_exe_for_tool(tool: &str, provision: bool) -> Option<String> {
    let basename = hook_binary_basename(tool)?;
    let bash_slashes = hook_uses_bash_slashes(tool)?;
    Some(resolve_hook_exe_for(
        &vigil_hook_binary_path(basename),
        bash_slashes,
        provision,
    ))
}

/// Retourne le chemin de vigil-claude.exe avec des forward slashes.
/// Nécessaire car Claude Code invoque les hooks via bash, où les backslashes
/// Windows sont interprétés comme des caractères d'échappement.
pub fn get_vigil_claude_exe(_app: &AppHandle) -> String {
    hook_exe_for_tool("claude_code", true).unwrap_or_default()
}

pub fn get_vigil_codex_exe(_app: &AppHandle) -> String {
    hook_exe_for_tool("codex", true).unwrap_or_default()
}

pub fn get_vigil_grok_exe(_app: &AppHandle) -> String {
    hook_exe_for_tool("grok", true).unwrap_or_default()
}

/// Convertit un chemin en sa forme courte 8.3 (`GetShortPathNameW`) — sans espace ni guillemets.
/// Renvoie `None` si l'API échoue (chemin inexistant, ou génération 8.3 désactivée sur le volume),
/// auquel cas l'appelant retombe sur le chemin long. Le fichier doit exister à l'appel (garanti au
/// moment de l'installation des hooks : le binaire est bundlé à côté de l'exe principal).
#[cfg(target_os = "windows")]
fn to_short_path(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        // 1er appel (buffer None) : renvoie la taille requise, null compris.
        let needed = GetShortPathNameW(PCWSTR(wide.as_ptr()), None);
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u16; needed as usize];
        let written = GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf));
        // Succès ⇒ `written` = nb de chars hors null (< taille du buffer).
        if written == 0 || written as usize >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..written as usize]))
    }
}

/// Chemin de vigil-antigravity.exe en **backslashes NATIFS** (⚠️ PAS de `.replace('\\', "/")`
/// contrairement à Claude/Codex/Grok). Antigravity exécute les commandes via `cmd /c` (natif
/// Windows), pas bash : un forward-slash en tête de commande serait interprété comme délimiteur
/// d'option, et le spike 6.16 a vu le quoting échouer SILENCIEUSEMENT.
///
/// Story 10.3 — l'espace est désormais SUPPRIMÉE en amont par le miroir `%ProgramData%` (cf.
/// `resolve_hook_exe`) : la 8.3, qui restait le seul rempart, n'est plus qu'un repli. Elle ne
/// suffisait pas — `GetShortPathNameW` réussit en rendant le chemin LONG lorsque la génération 8.3
/// est désactivée sur le volume, et le spike F4bis a prouvé qu'AUCUN quoting ne répare Antigravity
/// (il appende la commande brute dans `cmd /d /s /c`, sans guillemets externes).
pub fn get_vigil_antigravity_exe(_app: &AppHandle) -> String {
    hook_exe_for_tool("antigravity", true).unwrap_or_default()
}

fn tool_in_path(exe: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new("where")
            .arg(exe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("which")
            .arg(exe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ── Settings I/O ─────────────────────────────────────────────────────────────

pub fn read_settings(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Default::default()));
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("hook_settings_unreadable: {}", path.display()))?;
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    serde_json::from_str(content)
        .with_context(|| format!("hook_settings_unreadable: invalid JSON in {}", path.display()))
}

fn write_settings_atomic(path: &Path, settings: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("hook_settings_locked: cannot create dir {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(settings)
        .context("hook_install_failed: serialization error")?;

    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("hook_settings_locked: {}", tmp_path.display()))?;

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(e)
            .context(format!("hook_settings_locked: {}", path.display())));
    }

    Ok(())
}

// ── EnabledTools ──────────────────────────────────────────────────────────────

pub fn read_enabled_tools() -> EnabledTools {
    let path = get_enabled_tools_path();
    if !path.exists() {
        return EnabledTools::default();
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn write_enabled_tools(tools: &EnabledTools) -> anyhow::Result<()> {
    let path = get_enabled_tools_path();
    let value = serde_json::to_value(tools).context("serialization error")?;
    write_settings_atomic(&path, &value)
}

// ── Patron d'install unifié — identité d'un bloc Vigil (Story 10.1) ───────────
//
// Les 5 outils partagent DÉSORMAIS un patron d'install unique, validé pour Kimi en Story 9.2 :
// **retirer TOUS les blocs Vigil (matcher tolérant) puis réécrire les blocs canoniques**. Il
// remplace le mécanisme `already_present` (ajout-si-absent) de Claude/Codex/Antigravity, qui ne
// RÉPARAIT JAMAIS un handler existant : un `timeout` périmé ou un `command` au chemin d'install
// obsolète restait en place indéfiniment (défaut relevé en revue 8.8, `deferred-work.md`).
//
// Deux notions DISTINCTES, à ne jamais confondre :
//   1. « est-ce un bloc Vigil, sous N'IMPORTE QUELLE forme ? » → `command_designates_vigil`,
//      volontairement TOLÉRANT (casse, guillemets, chemin périmé, forme courte 8.3). Sert au
//      RETRAIT, à l'install comme à l'uninstall.
//   2. « y a-t-il EXACTEMENT UN bloc EXACTEMENT canonique par event ? » → `are_*_hooks_installed`,
//      volontairement STRICT (valeurs comprises, `timeout` inclus). Un bloc périmé matche (1) mais
//      échoue (2) → non conforme → l'orchestrateur relance l'install → (1) le retire et le
//      canonique est réécrit.
// C'est ce COUPLAGE qui rend l'auto-réparation réelle : sans conformité stricte, l'auto-repair
// n'est jamais SOLLICITÉ ; sans matcher tolérant, le bloc périmé est INDÉLOGEABLE et l'install
// produit un doublon à chaque passage — donc un double-fire silencieux sur les barrières de
// permission.
//
// Le matcher est quote-tolerant DÈS MAINTENANT alors qu'aucun générateur ne produit encore de
// guillemets : c'est la préparation de la Story 10.2 (quoting). Le jour où le canonique basculera
// en forme quotée, le bloc legacy non-quoté sera reconnu, retiré, puis réécrit quoté — sans
// doublon et sans double-détection à écrire.

const VIGIL_CLAUDE_BASENAME: &str = "vigil-claude.exe";
const VIGIL_CODEX_BASENAME: &str = "vigil-codex.exe";
/// Grok possède intégralement son fichier de hook (`~/.grok/hooks/vigil.json`, réécrit à chaque
/// install) : ce basename ne sert donc pas au matcher tolérant comme les autres, mais à la
/// résolution du chemin et au contrôle de fraîcheur du miroir (Story 10.3).
const VIGIL_GROK_BASENAME: &str = "vigil-grok.exe";
const VIGIL_ANTIGRAVITY_BASENAME: &str = "vigil-antigravity.exe";
const VIGIL_KIMI_BASENAME: &str = "vigil-kimi.exe";

/// Retire les guillemets ENGLOBANTS (`"…"`), **autant de paires qu'il y en a**.
///
/// Revue 10.1 — le retrait d'UNE seule paire ne reconnaissait pas la forme `""…""`, or c'est
/// précisément celle que la Story 10.2 devra produire (le spike `cmd` conclut au **double-wrap
/// obligatoire**, `/s` retirant la paire externe). Un bloc legacy sous cette forme serait resté
/// indélogeable au moment de la migration — c'est-à-dire l'inverse du service que l'AC9 rend.
fn strip_enclosing_quotes(s: &str) -> &str {
    let mut current = s;
    while let Some(inner) = current.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        current = inner;
    }
    current
}

/// Exécutable d'une commande à DEUX tokens (`<exe> <event>`, cas Antigravity) : jusqu'au guillemet
/// FERMANT si la commande est quotée (l'exe peut alors contenir un espace), sinon jusqu'au 1er
/// espace.
fn command_first_token(s: &str) -> &str {
    match s.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => s.split(' ').next().unwrap_or(s),
    }
}

/// Exécutable d'une commande `<exe> <arg>` dont l'exe est NON quoté et contient des espaces :
/// tout ce qui précède le DERNIER espace.
///
/// Revue 10.1 — sans cette lecture, l'intersection « Antigravity **et** chemin d'install à espace »
/// n'était couverte par aucune des deux autres : la lecture entière butait sur l'event collé
/// derrière (`…vigil-antigravity.exe pretooluse`), la lecture « 1er token » coupait à
/// `c:\users\john`. Vigil ne se reconnaissait alors plus lui-même : conformité fausse en permanence,
/// donc **+1 élément par event à chaque auto-repair** (croissance non bornée), et un uninstall qui
/// ne retirait rien. Cas ATTEIGNABLE : `get_vigil_antigravity_exe` se replie sur le chemin long
/// quand la 8.3 échoue, et `GetShortPathNameW` renvoie le chemin long **avec succès** lorsque la
/// génération 8.3 est désactivée sur le volume.
fn command_before_last_space(s: &str) -> &str {
    s.rsplit_once(' ').map(|(head, _)| head).unwrap_or(s)
}

/// `true` si `token` (un chemin d'exe, minuscules, sans guillemets) désigne le binaire de hook Vigil.
fn token_designates_vigil(token: &str, hook_lower: &str, basename: &str) -> bool {
    if token == hook_lower {
        return true;
    }
    let file = token.rsplit(['\\', '/']).next().unwrap_or(token);
    if file == basename {
        return true;
    }
    // Forme COURTE 8.3 (`VIGIL-~1.EXE`) : elle ne contient pas le nom de l'outil, donc ni la forme 1
    // ni la forme 2 ne l'attrapent — sans elle, un bloc au chemin périmé serait indélogeable. Motif
    // volontairement ÉTROIT (`vigil-~<chiffres>.exe`) pour ne jamais happer un programme tiers
    // (`vigilant.exe` ne matche pas). Il ne distingue pas les 5 binaires Vigil entre eux — sans
    // conséquence : un seul d'entre eux n'est jamais installé dans un fichier de config donné.
    file.strip_prefix("vigil-~")
        .and_then(|rest| rest.strip_suffix(".exe"))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// `true` si `command` désigne le binaire de hook Vigil sous N'IMPORTE laquelle de ses formes
/// historiques : commande courante (casse près), chemin périmé au bon `basename`, forme courte 8.3,
/// avec ou sans guillemets englobants. `hook_lower` = commande COURANTE déjà `trim` + minusculée.
///
/// La commande est lue de TROIS façons, et il suffit qu'une seule désigne Vigil :
///   - la commande ENTIÈRE — outils mono-token (Claude/Codex/Grok/Kimi), dont le chemin d'install
///     peut contenir un espace (`C:\Users\John Doe\…`) ;
///   - son 1er token — Antigravity, dont la commande est `<exe> <event>` ;
///   - ce qui précède son DERNIER espace — l'intersection des deux : Antigravity **et** chemin à
///     espace non quoté (cf. `command_before_last_space`).
///
/// Aucune de ces lectures n'est facultative : le 1er token seul raterait un chemin à espace (coupé à
/// `john`), la commande entière seule raterait tout chemin périmé d'Antigravity, et les deux
/// réunies rataient encore leur intersection — chaque trou se payant du même prix, un bloc
/// INDÉLOGEABLE donc un doublon qui se recrée à chaque install (double-fire silencieux sur une
/// barrière de permission).
///
/// ⚠️ **Conséquence VOULUE, pas un dommage collatéral** (décision Steven, revue 10.1) : une commande
/// TIERCE qui se termine par un chemin vers notre binaire — un wrapper, un lanceur, un script
/// d'audit qui l'enrobe — est reconnue, retirée, et remplacée par notre bloc canonique direct. On ne
/// veut PAS conserver ces lignes : un enrobage fausse la surveillance (payload relayé, timing, code
/// de sortie de l'enrobeur au lieu du nôtre). L'étroitesse à préserver est ailleurs, et elle est
/// testée : ne jamais happer un binaire qui ne DÉSIGNE pas le nôtre (`vigilant.exe`,
/// `vigil-claude.bat`, ou le binaire d'un autre outil Vigil sous ce `basename`).
fn command_designates_vigil(command: &str, hook_lower: &str, basename: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    // Le déquotage s'applique à CHAQUE lecture, pas seulement à la commande entière : sur
    // `""<exe>"" <event>`, seule la lecture « avant le dernier espace » isole l'exe, et elle le
    // rendrait encore quoté.
    let designates =
        |token: &str| token_designates_vigil(strip_enclosing_quotes(token), hook_lower, basename);
    designates(&lower)
        || designates(command_first_token(&lower))
        || designates(command_before_last_space(&lower))
}

/// `true` si un handler `{type, command, …}` porte une commande Vigil (matcher tolérant).
fn handler_is_vigil(handler: &Value, hook_lower: &str, basename: &str) -> bool {
    handler["command"]
        .as_str()
        .is_some_and(|c| command_designates_vigil(c, hook_lower, basename))
}

/// Nombre de handlers Vigil portés par un élément d'event, sous les DEUX formes : Grouped
/// (`{hooks:[…]}`, la forme attendue de Claude/Codex) et Flat (`{command}` directement sur
/// l'élément).
///
/// Revue 10.1 — ne compter que la forme Grouped rendait un handler Vigil Flat **invisible** : il
/// n'était ni compté (donc `total == 1` restait vrai à côté du canonique → conformité VERTE avec un
/// orphelin actif, double-fire jamais signalé) ni retiré (le `retain` conservait tout élément sans
/// clé `hooks`) → fuite permanente, y compris à l'uninstall. Antigravity traite explicitement cette
/// classe depuis la 6.18 (`antigravity_count_vigil`) ; le socle grouped l'ignorait.
fn count_vigil_handlers(group: &Value, hook_lower: &str, basename: &str) -> usize {
    let flat = usize::from(handler_is_vigil(group, hook_lower, basename));
    let grouped = group["hooks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|h| handler_is_vigil(h, hook_lower, basename))
        .count();
    flat + grouped
}

/// Normalise la valeur d'une clé d'event en TABLEAU, en préservant une valeur tierce (patron
/// Antigravity : on l'enveloppe, on ne l'écrase jamais). `null` est normalisé en tableau VIDE.
///
/// Revue 10.1 — sans cette normalisation en amont du RETRAIT, une valeur d'event non-tableau était
/// purement sautée (`and_then(as_array_mut)` → `None`) alors que l'INSTALL, lui, l'enveloppait puis
/// poussait le canonique à côté. Deux conséquences mesurées : (a) un bloc Vigil logé sous une telle
/// valeur survivait à l'uninstall — et l'aperçu de consentement mentait, exactement le défaut que
/// cette story corrige par ailleurs sur `uninstall_claude_hooks` ; (b) l'install produisait
/// `[bloc périmé, canonique]`, que sa PROPRE conformité rejette (`total == 2`) : Vigil écrivait un
/// fichier qu'il déclarait aussitôt non conforme, avec un double-fire réel jusqu'au passage suivant.
fn normalize_event_entries(entry: &mut Value) {
    if entry.is_array() {
        return;
    }
    if entry.is_null() {
        *entry = json!([]);
        return;
    }
    let previous = entry.take();
    *entry = json!([previous]);
}

/// Copie de `value` dont toute valeur de clé `command` est minusculée.
///
/// Permet une comparaison STRUCTURELLE de bloc — donc exhaustive sur les clés : une clé en trop, un
/// `matcher` erroné ou un `timeout` périmé rendent la conformité fausse — tout en restant
/// INSENSIBLE À LA CASSE du chemin, comme l'était l'ancienne comparaison de commandes.
fn with_lowercased_commands(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    let item = match item.as_str() {
                        Some(text) if key == "command" => Value::String(text.to_ascii_lowercase()),
                        _ => with_lowercased_commands(item),
                    };
                    (key.clone(), item)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(with_lowercased_commands).collect()),
        other => other.clone(),
    }
}

/// Égalité SÉMANTIQUE d'un bloc de hook avec son canonique, à la casse du `command` près.
///
/// ⚠️ Jamais de comparaison TEXTUELLE (`to_string()`) : elle serait sensible au style de
/// sérialisation et rendrait la conformité fausse en permanence — donc l'auto-repair perpétuel
/// (piège documenté dans `are_kimi_hooks_installed`).
fn hook_block_matches_canonical(actual: &Value, canonical: &Value) -> bool {
    with_lowercased_commands(actual) == with_lowercased_commands(canonical)
}

// ── Socle commun aux outils « tableau de groupes » (Claude, Codex) ─────────────
//
// Même structure : `hooks[<event>] = [ { …, hooks: [ {type, command, …} ] } ]`. Seuls le bloc
// canonique et la liste d'events diffèrent → les trois algorithmes (conformité, install, retrait)
// sont partagés et paramétrés.

/// Conformité : EXACTEMENT UN handler Vigil par event, dans un groupe EXACTEMENT canonique.
fn are_grouped_hooks_installed(
    config: &Value,
    events: &[&str],
    hook_exe: &str,
    basename: &str,
    canonical_group: impl Fn(&str) -> Value,
) -> bool {
    let hook_lower = hook_exe.trim().to_ascii_lowercase();
    let Some(hooks_map) = config.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    for event in events {
        let Some(groups) = hooks_map.get(*event).and_then(|g| g.as_array()) else {
            return false;
        };
        // `count == 1`, PAS `.any()` : un doublon (même commande, même event) échappait à `.any()`
        // → conformité verte alors que le hook se déclencherait DEUX fois, et l'auto-repair (qui
        // dédoublonne) n'était jamais sollicité. Le comptage traverse tous les groupes de l'event.
        let total: usize = groups
            .iter()
            .map(|g| count_vigil_handlers(g, &hook_lower, basename))
            .sum();
        if total != 1 {
            return false;
        }
        let Some(group) = groups
            .iter()
            .find(|g| count_vigil_handlers(g, &hook_lower, basename) == 1)
        else {
            return false;
        };
        // … et ce groupe unique doit être EXACTEMENT canonique : un chemin périmé, un `timeout`
        // obsolète ou un `matcher` erroné le rendent non conforme → réparé à l'install suivante.
        if !hook_block_matches_canonical(group, &canonical_group(event)) {
            return false;
        }
    }
    true
}

/// Install « retire-puis-réécrit » : retire TOUS les blocs Vigil (matcher tolérant) puis pousse le
/// groupe canonique de chaque event. D'où l'idempotence, l'auto-réparation et l'absence de doublon.
fn compute_grouped_with_hooks(
    config: Value,
    events: &[&str],
    hook_exe: &str,
    basename: &str,
    canonical_group: impl Fn(&str) -> Value,
) -> Value {
    let mut config = compute_grouped_without_hooks(config, events, hook_exe, basename);
    if !config.is_object() {
        config = json!({});
    }
    let obj = config.as_object_mut().expect("config must be object");
    let hooks_entry = obj.entry("hooks").or_insert(json!({}));
    if !hooks_entry.is_object() {
        *hooks_entry = json!({});
    }
    let hooks_map = hooks_entry.as_object_mut().expect("hooks must be object");
    for event in events {
        let groups = hooks_map.entry(*event).or_insert(json!([]));
        // Valeur tierce non-tableau occupant la clé d'event : PRÉSERVÉE en l'enveloppant (patron
        // Antigravity), jamais écrasée. Redondant depuis que le RETRAIT normalise lui aussi (le
        // retrait tourne juste au-dessus) — conservé pour que la fonction reste correcte seule.
        normalize_event_entries(groups);
        groups
            .as_array_mut()
            .expect("groups must be array")
            .push(canonical_group(event));
    }
    config
}

/// Retrait TOLÉRANT, précis au HANDLER : un handler tiers logé dans NOTRE groupe survit (le groupe
/// ne part que s'il ne reste plus aucun handler), et un groupe tiers voisin n'est jamais touché.
/// L'ancien code retirait le groupe ENTIER dès qu'il portait une commande Vigil — il emportait donc
/// le handler tiers avec lui.
fn compute_grouped_without_hooks(
    mut config: Value,
    events: &[&str],
    hook_exe: &str,
    basename: &str,
) -> Value {
    let hook_lower = hook_exe.trim().to_ascii_lowercase();
    if let Some(hooks_map) = config
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    {
        for event in events {
            if let Some(entry) = hooks_map.get_mut(*event) {
                normalize_event_entries(entry);
                let groups = entry.as_array_mut().expect("normalisé en tableau");
                groups.retain_mut(|g| {
                    // Élément Flat portant NOTRE commande → retirer (orphelin de mauvaise forme).
                    if handler_is_vigil(g, &hook_lower, basename) {
                        return false;
                    }
                    let Some(handlers) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                        // Élément SANS clé `hooks` (tiers mal formé) : conservé — on ne touche que
                        // ce qui est à nous. Un `null` enveloppé, lui, ne survit pas : il est
                        // invalide pour l'outil, et c'est nous qui l'aurions écrit.
                        return !g.is_null();
                    };
                    let before = handlers.len();
                    handlers.retain(|h| !handler_is_vigil(h, &hook_lower, basename));
                    // Groupe que NOUS venons de vider → retirer. Un groupe DÉJÀ vide est un groupe
                    // tiers (placeholder, hook « désactivé » par son propriétaire) : le supprimer
                    // emportait son `matcher` et, en cascade, la clé d'event que Vigil n'avait
                    // jamais créée — violation directe d'AC7 (revue 10.1).
                    !(handlers.is_empty() && before > 0)
                });
            }
        }
        for event in events {
            if hooks_map.get(*event).and_then(|g| g.as_array()).map(|a| a.is_empty()).unwrap_or(false) {
                hooks_map.remove(*event);
            }
        }
    }
    config
}

// ── Claude Code hooks ─────────────────────────────────────────────────────────

/// Groupe canonique d'un event Claude : `{ matcher: "", hooks: [ {type, command, timeout?} ] }`.
///
/// `PermissionRequest` BLOQUE en attendant la décision utilisateur (cf. `pipe/manager.rs`
/// `is_permission`). Le défaut hook Claude Code (60s) tuerait le hook AVANT l'auto-deny effectif
/// (`APPROVAL_TIMEOUT_SECS` = 600) → on relève le plafond pour que `APPROVAL_TIMEOUT_SECS` reste le
/// SEUL bouton. Autres events = fire-and-forget, sans timeout.
///
/// Story 9.2 — la valeur vient de `hook_timeout_ceiling_secs`, SOURCE DE VÉRITÉ UNIQUE partagée
/// avec l'écrêtage de la fenêtre d'approbation. Plus de littéral recopié : c'est exactement la
/// divergence qui rendrait l'écrêtage faux sans qu'aucun test ne rougisse.
fn claude_hook_group(hook_exe: &str, event: &str) -> Value {
    let handler = if event == "PermissionRequest" {
        json!({"type": "command", "command": hook_exe, "timeout": hook_timeout_ceiling_secs(&SourceTool::ClaudeCode)})
    } else {
        json!({"type": "command", "command": hook_exe})
    };
    json!({ "matcher": "", "hooks": [handler] })
}

pub fn are_claude_hooks_installed(settings: &Value, hook_exe_path: &str) -> bool {
    are_grouped_hooks_installed(
        settings,
        CLAUDE_EVENTS,
        hook_exe_path,
        VIGIL_CLAUDE_BASENAME,
        |event| claude_hook_group(hook_exe_path, event),
    )
}

pub fn compute_claude_with_hooks(settings: Value, hook_exe: &str) -> Value {
    compute_grouped_with_hooks(settings, CLAUDE_EVENTS, hook_exe, VIGIL_CLAUDE_BASENAME, |event| {
        claude_hook_group(hook_exe, event)
    })
}

pub fn compute_claude_without_hooks(settings: Value, hook_exe: &str) -> Value {
    compute_grouped_without_hooks(settings, CLAUDE_EVENTS, hook_exe, VIGIL_CLAUDE_BASENAME)
}

pub fn install_claude_hooks(app: &AppHandle) -> anyhow::Result<()> {
    let hook_exe = get_vigil_claude_exe(app);
    let settings_path = get_claude_settings_path();

    let current = read_settings(&settings_path)?;
    let merged = compute_claude_with_hooks(current, &hook_exe);

    write_settings_atomic(&settings_path, &merged)
}

pub fn uninstall_claude_hooks(app: &AppHandle) -> anyhow::Result<()> {
    let hook_exe = get_vigil_claude_exe(app);
    let settings_path = get_claude_settings_path();

    // Story 10.1 — le retrait était RECOPIÉ ici (égalité stricte sur la commande courante) : un
    // bloc au chemin périmé survivait à l'uninstall, et l'aperçu de consentement (qui passe par
    // `compute_claude_without_hooks`) pouvait mentir sur ce que l'uninstall ferait vraiment.
    // Un seul cœur pur désormais, aligné sur le matcher tolérant.
    let settings = read_settings(&settings_path)?;
    let cleaned = compute_claude_without_hooks(settings, &hook_exe);

    write_settings_atomic(&settings_path, &cleaned)
}

// ── Codex hooks ───────────────────────────────────────────────────────────────

fn codex_hook_entry(hook_exe: &str, timeout_secs: Option<u64>) -> Value {
    let mut handler = json!({ "type": "command", "command": hook_exe, "statusMessage": "vigil hook" });
    if let Some(t) = timeout_secs {
        handler["timeout"] = json!(t);
    }
    json!({ "hooks": [handler] })
}

/// Groupe canonique d'un event Codex : `{ hooks: [ {type, command, statusMessage, timeout?} ] }`
/// (pas de `matcher`, ≠ Claude).
///
/// `PermissionRequest` BLOQUE en attendant la décision utilisateur (cf. `pipe/manager.rs`
/// `is_permission`) → plafond relevé pour survivre à l'auto-deny effectif
/// (`APPROVAL_TIMEOUT_SECS` = 600), aligné sur Claude. Autres events = fire-and-forget.
/// Story 9.2 — valeur lue depuis `hook_timeout_ceiling_secs` (source de vérité unique).
fn codex_hook_group(hook_exe: &str, event: &str) -> Value {
    if event == "PermissionRequest" {
        codex_hook_entry(hook_exe, Some(hook_timeout_ceiling_secs(&SourceTool::Codex)))
    } else {
        codex_hook_entry(hook_exe, None)
    }
}

pub fn are_codex_hooks_installed(config: &Value, hook_exe: &str) -> bool {
    are_grouped_hooks_installed(config, CODEX_EVENTS, hook_exe, VIGIL_CODEX_BASENAME, |event| {
        codex_hook_group(hook_exe, event)
    })
}

pub fn compute_codex_with_hooks(config: Value, hook_exe: &str) -> Value {
    compute_grouped_with_hooks(config, CODEX_EVENTS, hook_exe, VIGIL_CODEX_BASENAME, |event| {
        codex_hook_group(hook_exe, event)
    })
}

pub fn compute_codex_without_hooks(config: Value, hook_exe: &str) -> Value {
    compute_grouped_without_hooks(config, CODEX_EVENTS, hook_exe, VIGIL_CODEX_BASENAME)
}

fn install_codex_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let config = read_settings(path)?;
    let merged = compute_codex_with_hooks(config, hook_exe);
    write_settings_atomic(path, &merged)
}

fn uninstall_codex_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let config = read_settings(path)?;
    let cleaned = compute_codex_without_hooks(config, hook_exe);
    write_settings_atomic(path, &cleaned)
}

pub fn install_codex_hooks(app: &AppHandle) -> anyhow::Result<()> {
    install_codex_hooks_impl(&get_codex_hooks_path(), &get_vigil_codex_exe(app))
}

pub fn uninstall_codex_hooks(app: &AppHandle) -> anyhow::Result<()> {
    uninstall_codex_hooks_impl(&get_codex_hooks_path(), &get_vigil_codex_exe(app))
}

// ── Grok hooks (fichier dédié Vigil-owned) ─────────────────────────────────────
//
// 4e patron d'install, le plus simple (Story 6.26) : Vigil possède intégralement
// `~/.grok/hooks/vigil.json`. Grok globbe `~/.grok/hooks/*.json` → aucun merge ni préservation
// tierce. Install = écrire la structure canonique complète, uninstall = supprimer le fichier.
// Clés d'events PascalCase, chaque event = `[{ "hooks": [{ "type":"command", "command": exe }] }]`
// (validé au spike ; pas de `matcher`, ≠ Claude).

/// Vérifie que le fichier `vigil.json` porte **exactement un** handler Vigil canonique pour CHAQUE
/// event de `GROK_EVENTS`. Sert à la conformité / auto-repair.
///
/// ⚠️ Aligné sur le patron des quatre autres outils (récupéré de la Story 10.2 à son abandon,
/// 2026-07-29). Deux défauts fermés d'un même geste, tous deux de la famille « conformité verte
/// alors que la config est fausse » :
///
/// 1. **Identification tolérante au lieu de l'égalité exacte.** L'ancien test comparait la commande
///    au chemin courant, si bien qu'un handler Vigil au chemin **périmé cohabitant** avec le
///    canonique n'était pas vu : conformité verte, auto-repair jamais sollicité, et l'orphelin tire
///    à côté. `command_designates_vigil` le reconnaît par basename, quelle que soit sa forme
///    (8.3, chemin long, guillemets).
/// 2. **`count == 1` au lieu de `.any()`.** C'est la leçon Kimi que la 10.1 a répliquée partout
///    sauf ici (son AC8 interdisait de toucher à Grok) : `.any()` laisse passer un doublon, donc un
///    double-fire, sans jamais déclencher la réparation qui le dédoublonnerait.
///
/// La comparaison au canonique est **sémantique et exhaustive** (`grok_hook_handler`), pas un
/// simple test de présence : le `timeout` de `PreToolUse` entre enfin dans la conformité — un
/// plafond modifié (120 → 600 en 8.8) laissait jusqu'ici le fichier périmé sans réparation. Sans
/// risque de churn côté tiers : Vigil possède ce fichier en propre et le réécrit en entier.
pub fn are_grok_hooks_installed(config: &Value, hook_exe: &str) -> bool {
    let Some(hooks_map) = config.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    let hook_lower = hook_exe.to_ascii_lowercase();
    for event in GROK_EVENTS {
        let Some(groups) = hooks_map.get(*event).and_then(|g| g.as_array()) else {
            return false;
        };
        let mut vigil = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter())
            .filter(|h| handler_is_vigil(h, &hook_lower, VIGIL_GROK_BASENAME));
        let Some(handler) = vigil.next() else {
            return false; // aucun handler Vigil pour cet event
        };
        if vigil.next().is_some() {
            return false; // doublon → non conforme → l'auto-repair dédoublonne
        }
        // Comparaison SÉMANTIQUE (casse du chemin indifférente, non-régression 6.18), jamais
        // textuelle : l'ordre des clés d'un objet JSON reparsé n'est pas celui de `json!`.
        if with_lowercased_commands(handler)
            != with_lowercased_commands(&grok_hook_handler(hook_exe, event))
        {
            return false;
        }
    }
    true
}

/// Vigil possède le fichier en propre → on IGNORE l'entrée et on retourne la structure canonique
/// complète bâtie sur `GROK_EVENTS`. (`_config` conservé pour l'homogénéité de la signature du
/// dispatch diff/install.)
pub fn compute_grok_with_hooks(_config: Value, hook_exe: &str) -> Value {
    let mut hooks_map = serde_json::Map::new();
    for event in GROK_EVENTS {
        hooks_map.insert(
            (*event).to_string(),
            json!([{ "hooks": [grok_hook_handler(hook_exe, event)] }]),
        );
    }
    json!({ "hooks": hooks_map })
}

/// Handler canonique de Grok pour un event — **source de vérité unique** partagée par la génération
/// (`compute_grok_with_hooks`) et la conformité (`are_grok_hooks_installed`), comme le fait déjà le
/// socle groupé de Claude/Codex depuis la 10.1. Les décrire à deux endroits laissait la conformité
/// ignorer le `timeout` : un plafond modifié (120 → 600 en 8.8) ne provoquait aucune réparation.
///
/// Story 6.30 : `PreToolUse` BLOQUE en attendant la décision utilisateur (vigil-grok devient
/// l'approbateur). Le défaut Grok est 5 s → fail-open (Grok reprend la main AVANT la réponse
/// utilisateur) → on relève le plafond. Le timeout EFFECTIF reste `APPROVAL_TIMEOUT_SECS = 600`
/// côté `PipeManager` (deny auto), envoyé bien avant. Les autres events restent sans `timeout`
/// (fire-and-forget, jamais bloquants).
/// Story 9.2 — valeur lue depuis `hook_timeout_ceiling_secs` (source de vérité unique).
fn grok_hook_handler(hook_exe: &str, event: &str) -> Value {
    if event == "PreToolUse" {
        json!({
            "type": "command",
            "command": hook_exe,
            "timeout": hook_timeout_ceiling_secs(&SourceTool::Grok),
        })
    } else {
        json!({ "type": "command", "command": hook_exe })
    }
}

/// « Vide » = `{}`. L'uninstall réel SUPPRIME le fichier (cf. `uninstall_grok_hooks_impl`) ; cette
/// valeur n'est qu'une REPRÉSENTATION du « après » pour le diff de consentement.
pub fn compute_grok_without_hooks(_config: Value, _hook_exe: &str) -> Value {
    json!({})
}

fn install_grok_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    // Vigil-owned : on n'ouvre/ne merge PAS l'existant. `write_settings_atomic` crée
    // `~/.grok/hooks/` au besoin (create_dir_all sur le parent).
    let canonical = compute_grok_with_hooks(Value::Null, hook_exe);
    write_settings_atomic(path, &canonical)
}

fn uninstall_grok_hooks_impl(path: &Path, _hook_exe: &str) -> anyhow::Result<()> {
    // Vigil possède le fichier → uninstall = suppression. `NotFound` toléré (déjà désinstallé).
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(e)
            .context(format!("hook_settings_locked: {}", path.display()))),
    }
}

pub fn install_grok_hooks(app: &AppHandle) -> anyhow::Result<()> {
    install_grok_hooks_impl(&get_grok_hooks_file_path(), &get_vigil_grok_exe(app))
}

pub fn uninstall_grok_hooks(app: &AppHandle) -> anyhow::Result<()> {
    uninstall_grok_hooks_impl(&get_grok_hooks_file_path(), &get_vigil_grok_exe(app))
}

// ── Antigravity hooks ─────────────────────────────────────────────────────────
//
// Structure DIFFÉRENTE de Codex : le top-level est un NOM de hook (`"vigil"`) → map d'events,
// et chaque event est Grouped (matcher + hooks[]) ou Flat (handlers directs) selon `ANTIGRAVITY_*`.
// La mécanique (comparaison casse-insensible, merge idempotent, retrait + nettoyage, I/O settings)
// reprend le patron Codex.

/// Commande de hook complète `<exe> <event>` (deux tokens). Le binaire 6.17 lit l'event via
/// `argv[1]`. L'`<event>` passe VERBATIM (casse comprise) : réponse `allow` gate sur `== "PreToolUse"`.
fn antigravity_command(hook_exe: &str, event: &str) -> String {
    format!("{} {}", hook_exe, event)
}

/// Élément canonique d'un event : Grouped (`{matcher:"*", hooks:[{command}]}`, `matcher: "*"` = tous
/// les outils, doc hooks.md) ou Flat (`{type, command}`) selon `antigravity_is_grouped`.
/// La commande reste **non quotée** (le quoting est la Story 10.2 — et Antigravity en est exclu).
fn antigravity_canonical_element(hook_exe: &str, event: &str) -> Value {
    let cmd = antigravity_command(hook_exe, event);
    if antigravity_is_grouped(event) {
        json!({ "matcher": "*", "hooks": [{ "type": "command", "command": cmd }] })
    } else {
        json!({ "type": "command", "command": cmd })
    }
}

/// `true` si `command` (string JSON) est une commande Vigil, via le matcher tolérant partagé.
///
/// Story 10.1 — c'était un `starts_with(<exe courant>)` : il ne reconnaissait QUE le chemin
/// d'install courant, donc un bloc laissé par une install précédente ailleurs n'était ni détecté ni
/// retirable (→ doublon à chaque réinstallation). La commande étant `<exe> <event>`, le matcher
/// isole le 1er token avant de comparer le basename.
fn antigravity_command_is_vigil(command: &Value, hook_lower: &str) -> bool {
    command
        .as_str()
        .is_some_and(|c| command_designates_vigil(c, hook_lower, VIGIL_ANTIGRAVITY_BASENAME))
}

/// Nombre de handlers Vigil portés par un élément d'event, Flat (`{command}`) comme Grouped
/// (`{hooks:[…]}`) — le comptage de conformité doit traverser LES DEUX formes, sinon un orphelin mal
/// formé ferait tirer le hook deux fois sans jamais rendre la config non conforme.
fn antigravity_count_vigil(el: &Value, hook_lower: &str) -> usize {
    let flat = usize::from(antigravity_command_is_vigil(&el["command"], hook_lower));
    let grouped = el
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|inner| {
            inner
                .iter()
                .filter(|h| antigravity_command_is_vigil(&h["command"], hook_lower))
                .count()
        })
        .unwrap_or(0);
    flat + grouped
}

/// Commande Vigil portée par un élément d'event, Flat (`{command}`) ou Grouped (`{hooks:[…]}`).
fn antigravity_vigil_command<'a>(el: &'a Value, hook_lower: &str) -> Option<&'a str> {
    if antigravity_command_is_vigil(&el["command"], hook_lower) {
        return el["command"].as_str();
    }
    el.get("hooks")
        .and_then(|h| h.as_array())
        .and_then(|inner| {
            inner
                .iter()
                .find(|h| antigravity_command_is_vigil(&h["command"], hook_lower))
        })
        .and_then(|h| h["command"].as_str())
}

/// Conformité — EXACTEMENT UN handler Vigil par event (à travers Grouped ET Flat), porté par un
/// élément EXACTEMENT canonique. Patron Kimi (`count == 1`, comparaison sémantique) : un chemin
/// périmé, un `matcher` erroné ou une forme inattendue rendent la config non conforme → l'install
/// suivante retire tout ce qui est à nous et réécrit le canonique.
pub fn are_antigravity_hooks_installed(config: &Value, hook_exe: &str) -> bool {
    let Some(hook_obj) = config.get(ANTIGRAVITY_HOOK_NAME).and_then(|h| h.as_object()) else {
        return false;
    };
    let hook_lower = hook_exe.trim().to_ascii_lowercase();
    for event in ANTIGRAVITY_EVENTS {
        let Some(entries) = hook_obj.get(*event).and_then(|e| e.as_array()) else {
            return false;
        };
        let total: usize = entries.iter().map(|el| antigravity_count_vigil(el, &hook_lower)).sum();
        if total != 1 {
            return false;
        }
        let Some(element) = entries.iter().find(|el| antigravity_count_vigil(el, &hook_lower) == 1)
        else {
            return false;
        };
        if !hook_block_matches_canonical(element, &antigravity_canonical_element(hook_exe, event)) {
            return false;
        }
        // L'ARGUMENT d'event, lui, se compare VERBATIM. `hook_block_matches_canonical` lave la casse
        // de la valeur `command` ENTIÈRE (pour rester insensible à celle du chemin, cf. 6.18) — or
        // la commande Antigravity est `<exe> <event>`, si bien que `…exe stop` sous la clé `Stop`
        // passait pour conforme. Le binaire, lui, dispatche sur `match event_name` SENSIBLE à la
        // casse (`vigil-antigravity/src/main.rs`) : l'event serait resté muet en permanence — dont
        // la réponse `allow` de `PreToolUse` — pendant que la page Installation affichait vert, et
        // l'auto-repair n'aurait jamais été sollicité (revue 10.1).
        let Some(command) = antigravity_vigil_command(element, &hook_lower) else {
            return false;
        };
        if !command.ends_with(&format!(" {event}")) {
            return false;
        }
    }
    true
}

/// Install « retire-puis-réécrit » (patron unifié, Story 10.1) : le `continue`-si-présent d'avant ne
/// réparait jamais un bloc existant — chemin périmé ou forme inattendue restaient en place.
pub fn compute_antigravity_with_hooks(config: Value, hook_exe: &str) -> Value {
    let mut config = compute_antigravity_without_hooks(config, hook_exe);
    if !config.is_object() {
        config = json!({});
    }
    let obj = config.as_object_mut().expect("config must be object");
    let hook_entry = obj.entry(ANTIGRAVITY_HOOK_NAME).or_insert(json!({}));
    if !hook_entry.is_object() {
        *hook_entry = json!({});
    }
    let hook_map = hook_entry.as_object_mut().expect("named hook must be object");

    for event in ANTIGRAVITY_EVENTS {
        let entry = hook_map.entry(*event).or_insert(json!([]));
        // Une valeur tierce non-array occupant la clé (collision de nom de hook "vigil") est
        // PRÉSERVÉE en l'enveloppant dans un tableau, jamais écrasée → pas de perte de données.
        if !entry.is_array() {
            let previous = entry.take();
            *entry = json!([previous]);
        }
        // Push d'un seul élément → PRÉSERVE les handlers/groupes tiers déjà présents sous l'event.
        entry
            .as_array_mut()
            .expect("event entries must be array")
            .push(antigravity_canonical_element(hook_exe, event));
    }
    config
}

pub fn compute_antigravity_without_hooks(mut config: Value, hook_exe: &str) -> Value {
    let hook_lower = hook_exe.trim().to_ascii_lowercase();
    let Some(hook_map) = config
        .as_object_mut()
        .and_then(|o| o.get_mut(ANTIGRAVITY_HOOK_NAME))
        .and_then(|h| h.as_object_mut())
    else {
        return config;
    };

    for event in ANTIGRAVITY_EVENTS {
        let Some(entry) = hook_map.get_mut(*event) else {
            continue;
        };
        // Même normalisation que le socle grouped : une valeur d'event non-tableau était sautée au
        // RETRAIT alors que l'install l'enveloppe — un bloc Vigil logé dessous survivait donc à
        // l'uninstall (revue 10.1).
        normalize_event_entries(entry);
        let entries = entry.as_array_mut().expect("normalisé en tableau");
        // Retrait tolérant aux deux formes, quelle que soit la forme attendue de l'event : un
        // handler Vigil peut être stocké à plat (`{command}`) OU dans un groupe (`{hooks:[…]}`).
        // Traiter les deux évite qu'un orphelin mal formé reste indélogeable (fuite permanente).
        entries.retain_mut(|el| {
            // Handler Flat Vigil direct → retirer.
            if antigravity_command_is_vigil(&el["command"], &hook_lower) {
                return false;
            }
            let Some(inner) = el.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                return !el.is_null();
            };
            let before = inner.len();
            inner.retain(|h| !antigravity_command_is_vigil(&h["command"], &hook_lower));
            // Groupe que NOUS venons de vider → retirer ; un groupe DÉJÀ vide appartient à un tiers
            // et reste intact (AC7, revue 10.1).
            !(inner.is_empty() && before > 0)
        });
    }

    // Retirer les clés d'event vidées.
    for event in ANTIGRAVITY_EVENTS {
        if hook_map.get(*event).and_then(|e| e.as_array()).map(|a| a.is_empty()).unwrap_or(false) {
            hook_map.remove(*event);
        }
    }

    // Retirer la clé de hook nommée "vigil" si elle ne contient plus rien.
    if hook_map.is_empty() {
        config.as_object_mut().expect("config must be object").remove(ANTIGRAVITY_HOOK_NAME);
    }
    config
}

fn install_antigravity_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let config = read_settings(path)?;
    let merged = compute_antigravity_with_hooks(config, hook_exe);
    write_settings_atomic(path, &merged)
}

fn uninstall_antigravity_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let config = read_settings(path)?;
    let cleaned = compute_antigravity_without_hooks(config, hook_exe);
    write_settings_atomic(path, &cleaned)
}

pub fn install_antigravity_hooks(app: &AppHandle) -> anyhow::Result<()> {
    install_antigravity_hooks_impl(&get_antigravity_hooks_path(), &get_vigil_antigravity_exe(app))
}

pub fn uninstall_antigravity_hooks(app: &AppHandle) -> anyhow::Result<()> {
    uninstall_antigravity_hooks_impl(&get_antigravity_hooks_path(), &get_vigil_antigravity_exe(app))
}

// ── Kimi CLI hooks (fusion TOML préservant le format) ─────────────────────────
//
// 5e patron d'install, et le seul en TOML. Trois faits MESURÉS dans le bundle JS embarqué de
// `~/.kimi-code/bin/kimi.exe` le commandent — aucun n'est déduit :
//
//   1. `HookDefSchema = object({event, matcher?, command, timeout?}).strict()`
//      → `.strict()` : une clé INCONNUE dans un bloc ne fait pas ignorer le hook, elle fait échouer
//        `validateConfig` sur la config ENTIÈRE (`throw KimiError(CONFIG_INVALID)`) — Kimi ne
//        démarre plus. C'est le risque n° 1 de cette story, verrouillé par test.
//      → `timeout: number().int().min(1).max(600)` : 1800 (la valeur Claude/Codex/Grok) est HORS
//        PLAGE et produirait le même échec. Le plafond de Kimi est DUR, cf.
//        `pipe::manager::hook_timeout_ceiling_secs`.
//      → `matcher` est évalué comme une REGEX (`matches(hook.matcher ?? "", …)`) : `"*"` n'en est
//        pas une valide. On l'OMET sur tous les blocs.
//
//   2. `writeConfigFile` sérialise via **`smol-toml`** (`stringify`), qui ne préserve AUCUN format.
//      Dès que l'utilisateur touche sa config depuis Kimi (login/logout OAuth, `setKimiConfig`,
//      suppression d'un provider), **tous les commentaires disparaissent — les nôtres compris**,
//      tandis que les blocs `[[hooks]]` SURVIVENT (ce sont des données, recopiées via `config.raw`).
//      → **L'identité d'un bloc Vigil ne peut donc PAS être son marqueur.** Le critère de retrait
//        est le champ `command` (cf. `kimi_command_designates_vigil`). Le marqueur
//        `# vigil: managed hook` n'est écrit que pour la LISIBILITÉ humaine, et part avec son bloc
//        quand il est encore là. Un critère « marqueur » aurait laissé des blocs INDÉLOGEABLES
//        après la première réécriture de Kimi.
//      → Les deux processus peuvent écrire ce fichier → notre écriture est ATOMIQUE (tmp + rename),
//        comme celle de Kimi (`atomicWrite`).
//
//   3. `buildHookSpawnOptions` rend `{ shell: true, … }` → sous Windows, Node exécute
//      `cmd.exe /d /s /c "<command>"`. Un chemin contenant un espace (`C:\Users\John Doe\…`) fait
//      parser `C:\Users\John` comme exécutable → **échec silencieux du hook**. C'est EXACTEMENT le
//      piège du spike 6.16 (Antigravity) → `get_vigil_kimi_exe` réutilise son patron : forme courte
//      8.3, backslashes NATIFS, jamais de `.replace('\\', "/")` (patron bash de Claude/Codex/Grok).

/// `config.toml` global de Kimi. ⚠️ SEUL fichier de `~/.kimi-code/` que Vigil écrit : les sessions,
/// l'index et les `wire.jsonl` restent en lecture seule stricte.
pub fn get_kimi_config_path() -> PathBuf {
    home_dir().join(".kimi-code").join("config.toml")
}

/// Chemin de `vigil-kimi.exe` en **backslashes NATIFS** — patron Antigravity (cf.
/// `get_vigil_antigravity_exe`), imposé par le `shell: true` de Kimi (`cmd /d /s /c`).
/// ⚠️ PAS de `.replace('\\', "/")` : ce patron-là est réservé aux outils qui passent par bash.
///
/// Story 10.3 — l'espace est supprimée en amont par le miroir `%ProgramData%` ; la 8.3 est reléguée
/// au repli. Kimi était l'un des deux seuls outils que le quoting réparait (spike F4bis), mais poser
/// des guillemets pour lui seul aurait maintenu quatre autres cas particuliers : l'invariant les
/// remplace tous.
pub fn get_vigil_kimi_exe(_app: &AppHandle) -> String {
    hook_exe_for_tool("kimi", true).unwrap_or_default()
}

/// ALIAS de `CLAUDE_EVENTS`, pas une copie — la liste n'existe qu'une fois.
///
/// Les 16 events du `_enum` de Kimi (`HOOK_EVENT_TYPES`) contiennent les 12 de `CLAUDE_EVENTS`, donc
/// l'alias est valide tel quel (un event hors liste rendrait la config invalide). Les **4 non
/// installés** le sont volontairement : `PermissionResult` (post-décision, sans état à porter),
/// `Interrupt` (l'interruption est MORTE côté Kimi — contrat deny-only, cf. `INTERRUPT_SUPPORTED`),
/// `PreCompact` / `PostCompact` (compaction de contexte, aucun état d'agent).
const KIMI_EVENTS: &[&str] = CLAUDE_EVENTS;

/// Seul event porteur d'un `timeout` : c'est lui qui BLOQUERA en attendant la décision utilisateur
/// (branchement = Story 9.3, cf. `vigil-kimi::convert_response_to_kimi`). Les 11 autres sont
/// fire-and-forget — leur donner un `timeout` n'aurait aucun effet et ajouterait une clé à valider.
const KIMI_BLOCKING_EVENT: &str = "PreToolUse";

/// Marqueur de LISIBILITÉ, jamais d'identité (cf. fait n° 2 ci-dessus : Kimi l'efface).
const KIMI_MARKER: &str = "# vigil: managed hook";

/// `true` si la commande d'un bloc `[[hooks]]` désigne le binaire de hook Kimi de Vigil.
///
/// Trois formes acceptées :
///   1. la commande COURANTE, à la casse près — cas nominal ;
///   2. un chemin périmé se terminant par `vigil-kimi.exe` — Vigil réinstallé ailleurs ;
///   3. un chemin périmé en forme COURTE 8.3 (`VIGIL-~1.EXE`), qui ne contient **pas** la chaîne
///      « kimi » : sans cette 3e forme, un bloc au chemin périmé serait INDÉLOGEABLE et l'install
///      (retrait-puis-ajout) produirait un DOUBLON à chaque réinstallation.
///
/// Story 10.1 — ce matcher, né ici (9.2), est devenu le helper PARTAGÉ des 5 outils
/// (`command_designates_vigil`, paramétré par le basename). Aucun changement d'observable sur les
/// trois formes ; il gagne au passage la tolérance aux guillemets englobants (préparation 10.2).
fn kimi_command_designates_vigil(command: &str, hook_lower: &str) -> bool {
    command_designates_vigil(command, hook_lower, VIGIL_KIMI_BASENAME)
}

fn kimi_table_is_vigil(table: &toml_edit::Table, hook_lower: &str) -> bool {
    table
        .get("command")
        .and_then(|i| i.as_str())
        .is_some_and(|c| kimi_command_designates_vigil(c, hook_lower))
}

/// Bloc canonique. ⚠️ **EXACTEMENT** `event` / `command` (+ `timeout` sur le seul event bloquant),
/// et RIEN d'autre — `.strict()` transforme une clé en trop en `CONFIG_INVALID` sur la config
/// entière. Verrouillé par test (« clés exactes »).
fn kimi_hook_table(event: &str, hook_exe: &str) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert("event", toml_edit::value(event));
    // ⚠️ Valeur posée via l'API `toml_edit` : c'est ELLE qui échappe les backslashes du chemin
    // Windows dans une chaîne TOML basique. Un `format!("command = \"{}\"", path)` produirait des
    // séquences d'échappement invalides (`\U`, `\t`…).
    table.insert("command", toml_edit::value(hook_exe));
    if event == KIMI_BLOCKING_EVENT {
        // Source de vérité UNIQUE, partagée avec l'écrêtage de la fenêtre d'approbation.
        table.insert(
            "timeout",
            toml_edit::value(hook_timeout_ceiling_secs(&SourceTool::Kimi) as i64),
        );
    }
    // `matcher` volontairement ABSENT (regex chez Kimi, cf. en-tête de section).
    table.decor_mut().set_prefix(format!("\n{KIMI_MARKER}\n"));
    table
}

/// Normalise une clé `hooks` écrite sous forme de tableau EN LIGNE (`hooks = [{ … }]`) en tableau de
/// tables (`[[hooks]]`), en préservant les valeurs de chaque entrée.
///
/// Kimi lui-même n'écrit jamais cette forme (`smol-toml` sérialise un tableau d'objets racine en
/// `[[hooks]]`), mais elle est du TOML valide et satisfait son schéma : un utilisateur peut l'avoir
/// écrite à la main. Sans normalisation, `as_array_of_tables_mut` rendrait `None` et l'install
/// écraserait ses hooks. On change donc la FORME (documenté) plutôt que de perdre la DONNÉE.
/// No-op dans tous les autres cas.
fn kimi_normalize_hooks(doc: &mut toml_edit::DocumentMut) {
    let Some(array) = doc.get("hooks").and_then(|i| i.as_array()) else {
        return;
    };
    let mut aot = toml_edit::ArrayOfTables::new();
    for entry in array.iter() {
        let mut table = toml_edit::Table::new();
        if let Some(inline) = entry.as_inline_table() {
            for (key, value) in inline.iter() {
                table.insert(key, toml_edit::value(value.clone()));
            }
        }
        aot.push(table);
    }
    doc.insert("hooks", toml_edit::Item::ArrayOfTables(aot));
}

/// Install : **(1)** retirer TOUS les blocs Vigil existants, **(2)** ajouter les 12 blocs canoniques
/// à la suite.
///
/// Cette forme rend l'install **idempotente**, **auto-réparatrice** (un `timeout` périmé ou un
/// `command` obsolète est corrigé, pas juste ignoré) et **sans doublon**. Elle évite délibérément le
/// défaut connu de `compute_claude_with_hooks` / `compute_codex_with_hooks`, dont le
/// `already_present` ne répare JAMAIS un handler existant (`deferred-work.md`, revue 8.8).
///
/// Tout ce qui n'est pas un bloc Vigil — `providers`, `models`, `default_model`,
/// `default_permission_mode`, `[thinking]`, commentaires, ordre, ET les hooks de l'utilisateur —
/// traverse la fonction intact : `toml_edit` est un parser PRÉSERVANT le format, on ne resérialise
/// jamais depuis un modèle.
pub fn compute_kimi_with_hooks(
    mut doc: toml_edit::DocumentMut,
    hook_exe: &str,
) -> toml_edit::DocumentMut {
    kimi_normalize_hooks(&mut doc);
    let hook_lower = hook_exe.trim().to_ascii_lowercase();

    if !doc.get("hooks").is_some_and(|i| i.is_array_of_tables()) {
        doc.insert(
            "hooks",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let aot = doc
        .get_mut("hooks")
        .and_then(|i| i.as_array_of_tables_mut())
        .expect("hooks vient d'être posé en ArrayOfTables");

    aot.retain(|t| !kimi_table_is_vigil(t, &hook_lower));
    for event in KIMI_EVENTS {
        aot.push(kimi_hook_table(event, hook_exe));
    }
    doc
}

/// Uninstall : retrait par `command` (insensible à la casse), avec le commentaire marqueur **s'il
/// est encore présent** — il vit dans le `Decor` du bloc et part donc avec lui. Un bloc dont Kimi a
/// effacé le marqueur est retiré **quand même** (c'est tout l'objet du critère `command`).
///
/// Si plus aucun bloc ne subsiste, la clé `hooks` est retirée : pas de `hooks = []` résiduel.
pub fn compute_kimi_without_hooks(
    mut doc: toml_edit::DocumentMut,
    hook_exe: &str,
) -> toml_edit::DocumentMut {
    kimi_normalize_hooks(&mut doc);
    let hook_lower = hook_exe.trim().to_ascii_lowercase();

    let mut now_empty = false;
    if let Some(aot) = doc.get_mut("hooks").and_then(|i| i.as_array_of_tables_mut()) {
        aot.retain(|t| !kimi_table_is_vigil(t, &hook_lower));
        now_empty = aot.is_empty();
    }
    if now_empty {
        doc.remove("hooks");
    }
    doc
}

/// Conformité — `true` **seulement si** les 12 events sont présents avec la commande COURANTE
/// **et** que chaque bloc porte exactement les clés canoniques, `timeout` compris.
///
/// La valeur fait partie de la conformité, pas seulement la présence : un bloc laissé par une
/// version antérieure (autre chemin, `timeout` périmé, clé en trop) doit déclencher l'auto-repair.
/// Sans cela, l'auto-réparation de `compute_kimi_with_hooks` ne serait jamais SOLLICITÉE — c'est
/// exactement l'angle mort de Claude/Codex relevé en 8.8.
pub fn are_kimi_hooks_installed(doc: &toml_edit::DocumentMut, hook_exe: &str) -> bool {
    let hook_lower = hook_exe.trim().to_ascii_lowercase();
    let Some(aot) = doc.get("hooks").and_then(|i| i.as_array_of_tables()) else {
        return false;
    };
    for event in KIMI_EVENTS {
        let expected = kimi_hook_table(event, hook_exe);
        let expected_keys: Vec<&str> = expected.iter().map(|(key, _)| key).collect();
        let expected_timeout = expected.get("timeout").and_then(|i| i.as_integer());
        // Bloc(s) Vigil portant cet event — identification TOLÉRANTE (par basename), pas par
        // égalité avec le chemin courant.
        //
        // ⚠️ Récupéré de la Story 10.2 à son abandon (2026-07-29). L'égalité exacte rendait
        // INVISIBLE un bloc Vigil au chemin **périmé cohabitant** avec le canonique : il n'entrait
        // pas dans le décompte, `count == 1` était satisfait, la conformité verte, l'auto-repair
        // jamais sollicité — et l'orphelin tirait à côté du canonique. Le RETRAIT était déjà
        // tolérant (`kimi_table_is_vigil`) ; seul le COMPTAGE ne l'était pas, et les deux notions
        // ne coïncidaient que tant que le canonique EST le chemin nu.
        let mut vigil_for_event = aot.iter().filter(|t| {
            t.get("event").and_then(|i| i.as_str()) == Some(*event)
                && kimi_table_is_vigil(t, &hook_lower)
        });
        // Exactement UN bloc Vigil par event. Un doublon (même `command`, même event) échappait à
        // l'ancien `.any()` : conformité verte alors que le hook se déclencherait DEUX fois, et
        // l'auto-repair n'était jamais sollicité. `count == 1` force l'auto-repair (qui retire tous
        // nos blocs puis réécrit les 12 canoniques) à dédoublonner.
        let Some(block) = vigil_for_event.next() else {
            return false; // aucun bloc Vigil pour cet event
        };
        if vigil_for_event.next().is_some() {
            return false; // doublon → non conforme
        }
        // Le bloc unique doit être EXACTEMENT canonique.
        // ⚠️ Comparaison SÉMANTIQUE, jamais textuelle : un `Value` issu du parsing porte son propre
        // `Decor` (l'espace autour du `=`), un `Value` construit non — comparer des `to_string()`
        // rendrait la conformité FAUSSE en permanence, donc l'auto-repair perpétuel.
        //
        // ⚠️ La `command` est comparée ICI, explicitement, depuis que le filtre ci-dessus est
        // tolérant (2026-07-29). Tant que l'identification se faisait par égalité avec le chemin
        // courant, elle portait implicitement cette vérification ; la rendre tolérante sans ce
        // rattrapage aurait jugé CONFORME un bloc au chemin périmé se trouvant SEUL — l'exact
        // contraire du service rendu. La valeur attendue est lue dans `expected`, jamais
        // reconstruite : si `kimi_hook_table` change un jour la forme écrite, la conformité suit.
        let command_of = |t: &toml_edit::Table| {
            t.get("command")
                .and_then(|i| i.as_str())
                .map(|c| c.trim().to_ascii_lowercase())
        };
        let canonical = block.len() == expected_keys.len()
            && expected_keys.iter().all(|key| block.contains_key(key))
            && command_of(block) == command_of(&expected)
            && block.get("timeout").and_then(|i| i.as_integer()) == expected_timeout;
        if !canonical {
            return false;
        }
    }
    true
}

/// Lecture d'un document TOML **et** du style de fin de ligne du fichier (`true` = CRLF).
///
/// Absent → document vide, style LF (celui qu'écrit Kimi lui-même via `smol-toml`). Un **BOM** en
/// tête est retiré avant parsing (il ferait échouer le parser sur la 1re clé) ; il n'est jamais
/// réécrit.
fn read_toml_document_and_eol(path: &Path) -> anyhow::Result<(toml_edit::DocumentMut, bool)> {
    if !path.exists() {
        return Ok((toml_edit::DocumentMut::new(), false));
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("hook_settings_unreadable: {}", path.display()))?;
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    let crlf = content.contains("\r\n");
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("hook_settings_unreadable: invalid TOML in {}", path.display()))?;
    Ok((doc, crlf))
}

fn read_toml_document(path: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    read_toml_document_and_eol(path).map(|(doc, _)| doc)
}

/// Rendu d'un document en **restituant le style de fin de ligne du fichier d'origine**.
///
/// ⚠️ MESURÉ : `toml_edit` NORMALISE les CRLF en LF au rendu (`"a = 1\r\n"` → `"a = 1\n"`), y
/// compris sur le contenu qu'il n'a pas touché. Sans cette restitution, la première install
/// réécrirait INTÉGRALEMENT un `config.toml` en CRLF — 36 octets de diff sur le fichier réel de
/// référence — et l'aller-retour install → uninstall ne rendrait jamais le document d'origine
/// (AC2 « bit-à-bit intact », AC8 « identique à l'original »). Le défaut est silencieux : le TOML
/// reste parfaitement valide, seul l'octet à octet le révèle.
///
/// Un fichier aux fins de ligne MIXTES est uniformisé en CRLF — cas pathologique, assumé.
fn render_toml(doc: &toml_edit::DocumentMut, crlf: bool) -> String {
    let text = doc.to_string();
    if !crlf {
        return text;
    }
    // Normaliser d'abord : le rendu peut porter des CRLF résiduels venus d'une chaîne littérale.
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Écriture ATOMIQUE (tmp + rename), UTF-8 **sans BOM** — Kimi écrit le même fichier
/// (`atomicWrite`), les deux processus peuvent donc se croiser.
fn write_toml_atomic(path: &Path, doc: &toml_edit::DocumentMut, crlf: bool) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("hook_settings_locked: cannot create dir {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, render_toml(doc, crlf).as_bytes())
        .with_context(|| format!("hook_settings_locked: {}", tmp_path.display()))?;
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(e).context(format!("hook_settings_locked: {}", path.display())));
    }
    Ok(())
}

fn install_kimi_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let (doc, crlf) = read_toml_document_and_eol(path)?;
    let merged = compute_kimi_with_hooks(doc, hook_exe);
    write_toml_atomic(path, &merged, crlf)
}

fn uninstall_kimi_hooks_impl(path: &Path, hook_exe: &str) -> anyhow::Result<()> {
    let (doc, crlf) = read_toml_document_and_eol(path)?;
    let before = render_toml(&doc, crlf);
    let cleaned = compute_kimi_without_hooks(doc, hook_exe);
    // Aucun bloc Vigil à retirer (rendu identique) : ne PAS réécrire. Évite une mutation de
    // mtime/format sur un no-op et une course inutile avec le writer de Kimi. Le script NSIS
    // `cleanup-hooks.ps1` garde déjà le même invariant (`if ($drop.Count -gt 0)`).
    if render_toml(&cleaned, crlf) == before {
        return Ok(());
    }
    write_toml_atomic(path, &cleaned, crlf)
}

pub fn install_kimi_hooks(app: &AppHandle) -> anyhow::Result<()> {
    install_kimi_hooks_impl(&get_kimi_config_path(), &get_vigil_kimi_exe(app))
}

pub fn uninstall_kimi_hooks(app: &AppHandle) -> anyhow::Result<()> {
    uninstall_kimi_hooks_impl(&get_kimi_config_path(), &get_vigil_kimi_exe(app))
}

// ── Multi-tool: conformité et détection ──────────────────────────────────────

/// Story 10.3 (AC4) — le miroir du binaire de hook fait PARTIE de la conformité.
///
/// Un miroir **absent** ou **différent du binaire source** rend les hooks non conformes, donc
/// l'auto-repair recopie puis réécrit les configs : une dérive de version ne peut pas survivre à un
/// démarrage.
///
/// > Motif : deux copies d'un même binaire qui divergent en silence, c'est exactement le contrat
/// > `transcript_hash` — deux mois de bug muet. La parade doit être structurelle, pas une intention.
///
/// ⚠️ **Lecture SEULE, et évaluée AVANT toute résolution de chemin** : `get_vigil_*_exe` répare le
/// miroir au passage (`ensure_mirrored`), donc l'interroger d'abord masquerait la dérive même qu'on
/// cherche à révéler.
fn mirror_is_stale_for_tool(tool: &str) -> bool {
    match hook_binary_basename(tool) {
        Some(basename) => hook_mirror::is_stale(&vigil_hook_binary_path(basename)),
        None => false,
    }
}

/// Amorce le miroir des binaires des outils ACTIVÉS — à appeler UNE FOIS au démarrage, avant la
/// première évaluation de conformité (revue 10.3, décision Steven).
///
/// Motif détaillé dans `hook_mirror::prime`. En deux mots : sans amorçage, toute mise à jour de
/// Vigil produit une salve de notifications « Hooks repaired » sur le chemin NOMINAL, et un poste où
/// le miroir est impossible en produit une à chaque lancement.
pub fn prime_hook_mirrors(app: &AppHandle) {
    for tool in ["claude_code", "codex", "grok", "antigravity", "kimi"] {
        if !tool_is_enabled(tool) {
            continue;
        }
        if let Some(basename) = hook_binary_basename(tool) {
            let _ = app; // le chemin source ne dépend pas du handle, gardé pour l'homogénéité
            hook_mirror::prime(&vigil_hook_binary_path(basename));
        }
    }
}

/// `true` si l'utilisateur a ACTIVÉ cet outil — le seul cas où Vigil a une raison de provisionner
/// le miroir (créer des dossiers, poser des DACL, copier un exécutable).
fn tool_is_enabled(tool: &str) -> bool {
    let enabled = read_enabled_tools();
    match tool {
        "claude_code" => enabled.claude_code,
        "codex" => enabled.codex,
        "grok" => enabled.grok,
        "antigravity" => enabled.antigravity,
        "kimi" => enabled.kimi,
        _ => false,
    }
}

/// Conformité des hooks d'un outil.
///
/// ⚠️ **Le provisionnement du miroir est borné aux outils ACTIVÉS** (revue 10.3, décision Steven).
/// Ce prédicat est appelé à chaque requête d'état (`detect_installed_tools`, monté par
/// Panel/Stats/History/Usage et au retour de visibilité) et **pour les cinq outils** : il posait
/// donc, par appel et par outil, deux `create_dir_all`, trois `SetNamedSecurityInfoW` et
/// potentiellement une copie — y compris pour des outils que l'utilisateur n'a jamais touchés. Pour
/// un outil non activé, la résolution passe désormais en LECTURE SEULE ; la comparaison avec la
/// config, elle, est identique.
///
/// Le durcissement lui-même est inchangé : sa ré-application à chaque passage reste délibérée (un
/// dossier préexistant mal permissionné doit être corrigé, pas hérité — test
/// `dossier_preexistant_mal_permissionne_est_corrige`).
pub fn are_hooks_conformant_for_tool(app: &AppHandle, tool: &str) -> bool {
    let provision = tool_is_enabled(tool);
    // La fraîcheur ne concerne que les outils dont on entretient réellement le miroir : sur un outil
    // non activé, le miroir peut légitimement être absent.
    if provision && mirror_is_stale_for_tool(tool) {
        return false;
    }
    let Some(hook_exe) = hook_exe_for_tool(tool, provision) else {
        return false;
    };
    let _ = app;
    match tool {
        "claude_code" => {
            let config = read_settings(&get_claude_settings_path()).unwrap_or_default();
            are_claude_hooks_installed(&config, &hook_exe)
        }
        "codex" => {
            let config = read_settings(&get_codex_hooks_path()).unwrap_or_default();
            are_codex_hooks_installed(&config, &hook_exe)
        }
        "grok" => {
            let config = read_settings(&get_grok_hooks_file_path()).unwrap_or_default();
            are_grok_hooks_installed(&config, &hook_exe)
        }
        "antigravity" => {
            let config = read_settings(&get_antigravity_hooks_path()).unwrap_or_default();
            are_antigravity_hooks_installed(&config, &hook_exe)
        }
        "kimi" => {
            let doc = read_toml_document(&get_kimi_config_path()).unwrap_or_default();
            are_kimi_hooks_installed(&doc, &hook_exe)
        }
        _ => false,
    }
}

pub fn detect_installed_tools(app: &AppHandle) -> ToolsStatus {
    let claude_detected = get_claude_settings_path().exists() || tool_in_path("claude.exe");
    let codex_detected = get_codex_hooks_path().exists() || tool_in_path("codex.exe");
    // AC4 — détection DÉCOUPLÉE du fichier de hook Vigil : via le dossier `~/.grok/` (créé par
    // Grok lui-même) OU le binaire dans le PATH. Sinon Grok ne serait « détecté » qu'APRÈS notre
    // propre install (faux).
    let grok_detected = home_dir().join(".grok").exists()
        || tool_in_path("grok.exe")
        || tool_in_path("grok-build.exe");
    // `agy.exe` est HORS PATH par défaut (ajouté par `agy install`) → le OU avec le dossier
    // `~/.gemini/antigravity-cli/` (créé dès la 1re utilisation) couvre les deux cas. On ne teste
    // PAS l'existence de `hooks.json` (il peut ne pas exister avant qu'on l'écrive).
    let antigravity_detected = tool_in_path("agy.exe")
        || home_dir().join(".gemini").join("antigravity-cli").exists();
    // Story 9.2 — détection DÉCOUPLÉE de nos hooks (patron Grok/AC4 de la 6.26) : on ne teste NI
    // `config.toml`, NI la présence de nos blocs, sinon Kimi ne serait « détecté » qu'APRÈS notre
    // propre install. ⚠️ Le binaire réel vit dans `~/.kimi-code/bin/kimi.exe`, qui n'est PAS
    // nécessairement dans le PATH — le dossier est le chemin NOMINAL, pas un repli théorique.
    let kimi_detected = home_dir().join(".kimi-code").exists() || tool_in_path("kimi.exe");

    let enabled = read_enabled_tools();

    ToolsStatus {
        claude_code_detected: claude_detected,
        codex_detected,
        grok_detected,
        antigravity_detected,
        kimi_detected,
        claude_code_enabled: enabled.claude_code,
        codex_enabled: enabled.codex,
        grok_enabled: enabled.grok,
        antigravity_enabled: enabled.antigravity,
        kimi_enabled: enabled.kimi,
        claude_code_hooks_ok: are_hooks_conformant_for_tool(app, "claude_code"),
        codex_hooks_ok: are_hooks_conformant_for_tool(app, "codex"),
        grok_hooks_ok: are_hooks_conformant_for_tool(app, "grok"),
        antigravity_hooks_ok: are_hooks_conformant_for_tool(app, "antigravity"),
        kimi_hooks_ok: are_hooks_conformant_for_tool(app, "kimi"),
    }
}

// ── Install / Uninstall par outil ─────────────────────────────────────────────

pub fn install_hooks_for_tool(app: &AppHandle, tool: &str) -> anyhow::Result<()> {
    match tool {
        "claude_code" => install_claude_hooks(app)?,
        "codex" => install_codex_hooks(app)?,
        "grok" => install_grok_hooks(app)?,
        "antigravity" => install_antigravity_hooks(app)?,
        "kimi" => install_kimi_hooks(app)?,
        _ => return Err(anyhow::anyhow!("outil inconnu: {}", tool)),
    }
    let mut enabled = read_enabled_tools();
    match tool {
        "claude_code" => enabled.claude_code = true,
        "codex" => enabled.codex = true,
        "grok" => enabled.grok = true,
        "antigravity" => enabled.antigravity = true,
        "kimi" => enabled.kimi = true,
        _ => {}
    }
    write_enabled_tools(&enabled)
}

pub fn uninstall_hooks_for_tool(app: &AppHandle, tool: &str) -> anyhow::Result<()> {
    match tool {
        "claude_code" => uninstall_claude_hooks(app)?,
        "codex" => uninstall_codex_hooks(app)?,
        "grok" => uninstall_grok_hooks(app)?,
        "antigravity" => uninstall_antigravity_hooks(app)?,
        "kimi" => uninstall_kimi_hooks(app)?,
        _ => return Err(anyhow::anyhow!("outil inconnu: {}", tool)),
    }
    let mut enabled = read_enabled_tools();
    match tool {
        "claude_code" => enabled.claude_code = false,
        "codex" => enabled.codex = false,
        "grok" => enabled.grok = false,
        "antigravity" => enabled.antigravity = false,
        "kimi" => enabled.kimi = false,
        _ => {}
    }
    write_enabled_tools(&enabled)
}

// ── Diff ──────────────────────────────────────────────────────────────────────

pub fn get_tool_hook_diff(
    app: &AppHandle,
    tool: &str,
    action: &str,
) -> anyhow::Result<ToolHookDiff> {
    match tool {
        "claude_code" => {
            let path = get_claude_settings_path();
            let before = read_settings(&path).unwrap_or_default();
            let hook_exe = get_vigil_claude_exe(app);
            let after = if action == "install" {
                compute_claude_with_hooks(before.clone(), &hook_exe)
            } else if action == "uninstall" {
                compute_claude_without_hooks(before.clone(), &hook_exe)
            } else {
                return Err(anyhow::anyhow!("action inconnue: {}", action));
            };
            Ok(ToolHookDiff {
                tool: tool.to_string(),
                config_file_path: path.to_string_lossy().to_string(),
                before: serde_json::to_string_pretty(&before)?,
                after: serde_json::to_string_pretty(&after)?,
                hook_exe_path: hook_exe,
            })
        }
        "codex" => {
            let path = get_codex_hooks_path();
            let before = read_settings(&path).unwrap_or_default();
            let hook_exe = get_vigil_codex_exe(app);
            let after = if action == "install" {
                compute_codex_with_hooks(before.clone(), &hook_exe)
            } else if action == "uninstall" {
                compute_codex_without_hooks(before.clone(), &hook_exe)
            } else {
                return Err(anyhow::anyhow!("action inconnue: {}", action));
            };
            Ok(ToolHookDiff {
                tool: tool.to_string(),
                config_file_path: path.to_string_lossy().to_string(),
                before: serde_json::to_string_pretty(&before)?,
                after: serde_json::to_string_pretty(&after)?,
                hook_exe_path: hook_exe,
            })
        }
        "grok" => {
            let path = get_grok_hooks_file_path();
            let before = read_settings(&path).unwrap_or_default();
            let hook_exe = get_vigil_grok_exe(app);
            let after = if action == "install" {
                compute_grok_with_hooks(before.clone(), &hook_exe)
            } else if action == "uninstall" {
                compute_grok_without_hooks(before.clone(), &hook_exe)
            } else {
                return Err(anyhow::anyhow!("action inconnue: {}", action));
            };
            Ok(ToolHookDiff {
                tool: tool.to_string(),
                config_file_path: path.to_string_lossy().to_string(),
                before: serde_json::to_string_pretty(&before)?,
                after: serde_json::to_string_pretty(&after)?,
                hook_exe_path: hook_exe,
            })
        }
        "antigravity" => {
            let path = get_antigravity_hooks_path();
            let before = read_settings(&path).unwrap_or_default();
            let hook_exe = get_vigil_antigravity_exe(app);
            let after = if action == "install" {
                compute_antigravity_with_hooks(before.clone(), &hook_exe)
            } else if action == "uninstall" {
                compute_antigravity_without_hooks(before.clone(), &hook_exe)
            } else {
                return Err(anyhow::anyhow!("action inconnue: {}", action));
            };
            Ok(ToolHookDiff {
                tool: tool.to_string(),
                config_file_path: path.to_string_lossy().to_string(),
                before: serde_json::to_string_pretty(&before)?,
                after: serde_json::to_string_pretty(&after)?,
                hook_exe_path: hook_exe,
            })
        }
        // ⚠️ Seul outil dont le `before`/`after` est du **texte TOML**, pas du JSON pretty-printed.
        // `DiffModal` est agnostique du format : il affiche un diff unifié de deux chaînes.
        "kimi" => {
            let path = get_kimi_config_path();
            // ⚠️ `?` et non `.unwrap_or_default()` : un `config.toml` MALFORMÉ ne doit PAS être
            // avalé en document vide. Sinon l'aperçu de consentement montrerait une insertion
            // propre (`before=""` → 12 blocs) que l'install re-lisant le fichier échouerait ensuite
            // à appliquer — l'utilisateur consentirait à un diff qui ne se produit pas. Fichier
            // ABSENT reste OK (géré dans `read_toml_document_and_eol`) : seul le TOML invalide erre.
            let (before, crlf) = read_toml_document_and_eol(&path)?;
            let hook_exe = get_vigil_kimi_exe(app);
            let after = if action == "install" {
                compute_kimi_with_hooks(before.clone(), &hook_exe)
            } else if action == "uninstall" {
                compute_kimi_without_hooks(before.clone(), &hook_exe)
            } else {
                return Err(anyhow::anyhow!("action inconnue: {}", action));
            };
            // Même rendu que celui qui sera ÉCRIT (fins de ligne comprises) : le diff de
            // consentement doit montrer exactement ce que l'utilisateur va obtenir.
            Ok(ToolHookDiff {
                tool: tool.to_string(),
                config_file_path: path.to_string_lossy().to_string(),
                before: render_toml(&before, crlf),
                after: render_toml(&after, crlf),
                hook_exe_path: hook_exe,
            })
        }
        _ => Err(anyhow::anyhow!("outil inconnu: {}", tool)),
    }
}

// ── Installation required ─────────────────────────────────────────────────────

fn check_installation_required_with(
    enabled: &EnabledTools,
    claude_ok: bool,
    codex_ok: bool,
    grok_ok: bool,
    antigravity_ok: bool,
    kimi_ok: bool,
) -> bool {
    let any_enabled =
        enabled.claude_code || enabled.codex || enabled.grok || enabled.antigravity || enabled.kimi;
    if !any_enabled {
        return true;
    }
    if enabled.claude_code && !claude_ok { return true; }
    if enabled.codex && !codex_ok { return true; }
    if enabled.grok && !grok_ok { return true; }
    if enabled.antigravity && !antigravity_ok { return true; }
    if enabled.kimi && !kimi_ok { return true; }
    false
}

pub fn check_installation_required_sync(app: &AppHandle) -> bool {
    let enabled = read_enabled_tools();
    let claude_ok = are_hooks_conformant_for_tool(app, "claude_code");
    let codex_ok = are_hooks_conformant_for_tool(app, "codex");
    let grok_ok = are_hooks_conformant_for_tool(app, "grok");
    let antigravity_ok = are_hooks_conformant_for_tool(app, "antigravity");
    let kimi_ok = are_hooks_conformant_for_tool(app, "kimi");
    check_installation_required_with(&enabled, claude_ok, codex_ok, grok_ok, antigravity_ok, kimi_ok)
}

// ── Auto-repair des hooks (Story 6.8) ─────────────────────────────────────────

/// Étiquette lisible d'un outil pour les notifications utilisateur.
fn tool_label(tool: &str) -> &'static str {
    match tool {
        "claude_code" => "Claude Code",
        "codex" => "Codex CLI",
        "grok" => "Grok Build",
        "antigravity" => "Antigravity CLI",
        "kimi" => "Kimi CLI",
        _ => "Unknown tool",
    }
}

/// Logique pure : liste des outils activés dont les hooks ne sont pas conformes.
/// Un outil désactivé n'est jamais réparé, quel que soit l'état de ses hooks.
fn tools_needing_repair(
    enabled: &EnabledTools,
    claude_ok: bool,
    codex_ok: bool,
    grok_ok: bool,
    antigravity_ok: bool,
    kimi_ok: bool,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if enabled.claude_code && !claude_ok { out.push("claude_code"); }
    if enabled.codex && !codex_ok { out.push("codex"); }
    if enabled.grok && !grok_ok { out.push("grok"); }
    if enabled.antigravity && !antigravity_ok { out.push("antigravity"); }
    if enabled.kimi && !kimi_ok { out.push("kimi"); }
    out
}

/// Réinstalle les hooks des outils activés dont les hooks sont compromis.
/// Retourne les étiquettes des outils réparés avec succès (pour notification).
/// Un échec d'installation est avalé : l'outil reste non conforme et la page
/// Installation s'ouvrira via `check_installation_required_sync` (dégradation propre).
pub fn auto_repair_hooks(app: &AppHandle) -> Vec<String> {
    let enabled = read_enabled_tools();
    let claude_ok = are_hooks_conformant_for_tool(app, "claude_code");
    let codex_ok = are_hooks_conformant_for_tool(app, "codex");
    let grok_ok = are_hooks_conformant_for_tool(app, "grok");
    let antigravity_ok = are_hooks_conformant_for_tool(app, "antigravity");
    let kimi_ok = are_hooks_conformant_for_tool(app, "kimi");

    let mut repaired = Vec::new();
    for tool in tools_needing_repair(&enabled, claude_ok, codex_ok, grok_ok, antigravity_ok, kimi_ok) {
        if install_hooks_for_tool(app, tool).is_ok() {
            repaired.push(tool_label(tool).to_string());
        }
    }
    repaired
}

// ── Autostart — Windows Registry ─────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub(crate) mod autostart_impl {
    use std::path::Path;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Registry::{
        RegCreateKeyExW, RegOpenKeyExW, RegSetValueExW, RegDeleteValueW,
        RegQueryValueExW, RegCloseKey,
        HKEY_CURRENT_USER, KEY_WRITE, KEY_READ, REG_SZ, REG_OPTION_NON_VOLATILE,
        HKEY,
    };
    #[cfg(test)]
    use windows::Win32::System::Registry::RegDeleteKeyW;
    use windows::core::PCWSTR;

    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0u16)).collect()
    }

    pub fn set_autostart_at(
        key_path: &str,
        value_name: &str,
        enabled: bool,
        exe_path: &Path,
    ) -> anyhow::Result<()> {
        let key_w = to_wide_null(key_path);
        let val_w = to_wide_null(value_name);

        unsafe {
            let mut hkey = HKEY::default();
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(key_w.as_ptr()),
                0,
                PCWSTR(std::ptr::null()),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey,
                None,
            )
            .ok()?;

            if enabled {
                let exe_w: Vec<u16> = exe_path.as_os_str().encode_wide().chain(std::iter::once(0u16)).collect();
                let data: &[u8] = std::slice::from_raw_parts(
                    exe_w.as_ptr() as *const u8,
                    exe_w.len() * 2,
                );
                let r = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(val_w.as_ptr()),
                    0,
                    REG_SZ,
                    Some(data),
                )
                .ok();
                let _ = RegCloseKey(hkey);
                r?;
            } else {
                let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(val_w.as_ptr()));
                let _ = RegCloseKey(hkey);
            }
        }
        Ok(())
    }

    pub fn is_autostart_enabled_at(key_path: &str, value_name: &str) -> bool {
        let key_w = to_wide_null(key_path);
        let val_w = to_wide_null(value_name);

        unsafe {
            let mut hkey = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(key_w.as_ptr()),
                0,
                KEY_READ,
                &mut hkey,
            )
            .is_err()
            {
                return false;
            }

            let mut data_size: u32 = 0;
            let result = RegQueryValueExW(
                hkey,
                PCWSTR::from_raw(val_w.as_ptr()),
                None,
                None,
                None,
                Some(&mut data_size),
            );
            let _ = RegCloseKey(hkey);
            result.ok().is_ok()
        }
    }

    #[cfg(test)]
    pub fn delete_test_key(key_path: &str) {
        let key_w = to_wide_null(key_path);
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR::from_raw(key_w.as_ptr()));
        }
    }
}

#[cfg(target_os = "windows")]
pub fn set_autostart(_app: &AppHandle, enabled: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot resolve exe path: {e}"))?;
    autostart_impl::set_autostart_at(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        "Vigil",
        enabled,
        &exe,
    )
}

#[cfg(not(target_os = "windows"))]
pub fn set_autostart(_app: &AppHandle, _enabled: bool) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn is_autostart_enabled() -> bool {
    autostart_impl::is_autostart_enabled_at(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        "Vigil",
    )
}

#[cfg(not(target_os = "windows"))]
pub fn is_autostart_enabled() -> bool {
    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOOK_EXE: &str = "C:\\Users\\Dev\\AppData\\Local\\vigil\\vigil-claude.exe";
    const CODEX_HOOK_EXE: &str = "C:/Users/Dev/AppData/Local/vigil/vigil-codex.exe";
    const GROK_HOOK_EXE: &str = "C:/Users/Dev/AppData/Local/vigil/vigil-grok.exe";
    // Backslashes NATIFS (≠ Codex/Grok) : Antigravity passe par `cmd /c`, pas bash. Cf. quoting Windows.
    const ANTIGRAVITY_HOOK_EXE: &str = "C:\\Users\\Dev\\AppData\\Local\\Vigil\\vigil-antigravity.exe";

    fn vigil_hook_entry() -> Value {
        json!({
            "matcher": "",
            "hooks": [{"type": "command", "command": HOOK_EXE}]
        })
    }

    // ── are_claude_hooks_installed ──────────────────────────────────────────────────

    #[test]
    fn hooks_installed_retourne_false_si_settings_vide() {
        let settings = json!({});
        assert!(!are_claude_hooks_installed(&settings, HOOK_EXE));
    }

    #[test]
    fn hooks_installed_retourne_false_si_un_evenement_manque() {
        let settings = json!({
            "hooks": {
                "SessionStart": [vigil_hook_entry()]
            }
        });
        assert!(!are_claude_hooks_installed(&settings, HOOK_EXE));
    }

    #[test]
    fn hooks_installed_retourne_true_si_tous_les_evenements_sont_presents() {
        // Story 10.1 — la conformité exige le bloc EXACTEMENT canonique (`timeout` de
        // `PermissionRequest` compris) : la fixture est donc bâtie sur le canonique lui-même, un
        // groupe « générique » pour les 12 events ne suffit plus (et c'est l'objet de l'AC4 : un
        // bloc au timeout périmé DOIT être vu comme non conforme pour être réparé).
        let hooks: serde_json::Map<String, Value> = CLAUDE_EVENTS
            .iter()
            .map(|event| ((*event).to_string(), json!([claude_hook_group(HOOK_EXE, event)])))
            .collect();
        let settings = json!({ "hooks": hooks });
        assert!(are_claude_hooks_installed(&settings, HOOK_EXE));
        // Le même jeu d'events SANS le timeout du bloc bloquant n'est PAS conforme.
        let generique: serde_json::Map<String, Value> = CLAUDE_EVENTS
            .iter()
            .map(|event| ((*event).to_string(), json!([vigil_hook_entry()])))
            .collect();
        assert!(!are_claude_hooks_installed(&json!({ "hooks": generique }), HOOK_EXE));
    }

    #[test]
    fn hooks_installed_retourne_false_si_mauvais_chemin_exe() {
        let entry = json!({
            "matcher": "",
            "hooks": [{"type": "command", "command": "C:\\autre\\outil.exe"}]
        });
        let settings = json!({
            "hooks": {
                "SessionStart":      [entry.clone()],
                "SessionEnd":        [entry.clone()],
                "PreToolUse":        [entry.clone()],
                "PostToolUse":       [entry.clone()],
                "PermissionRequest": [entry.clone()],
                "Notification":      [entry.clone()],
                "Stop":              [entry.clone()],
                "StopFailure":       [entry.clone()],
            }
        });
        assert!(!are_claude_hooks_installed(&settings, HOOK_EXE));
    }

    // ── compute_claude_with_hooks ──────────────────────────────────────────

    #[test]
    fn compute_ajoute_les_8_evenements_sur_settings_vide() {
        let result = compute_claude_with_hooks(json!({}), HOOK_EXE);
        assert!(are_claude_hooks_installed(&result, HOOK_EXE));
    }

    #[test]
    fn compute_preserve_hooks_tiers_existants() {
        let tiers = json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "C:\\MonOutil\\logger.exe"}]
        });
        let before = json!({
            "hooks": {
                "PostToolUse": [tiers.clone()]
            }
        });

        let result = compute_claude_with_hooks(before, HOOK_EXE);

        assert!(are_claude_hooks_installed(&result, HOOK_EXE));

        let post_tool = result["hooks"]["PostToolUse"].as_array().unwrap();
        let tiers_present = post_tool.iter().any(|g| {
            g["hooks"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter()
                .any(|h| h["command"].as_str().unwrap_or("") == "C:\\MonOutil\\logger.exe")
        });
        assert!(tiers_present, "Le hook tiers doit être préservé dans PostToolUse");
    }

    #[test]
    fn compute_ne_cree_pas_de_doublon_si_vigil_deja_present() {
        let after_first = compute_claude_with_hooks(json!({}), HOOK_EXE);
        let after_second = compute_claude_with_hooks(after_first, HOOK_EXE);

        for event in CLAUDE_EVENTS {
            let groups = after_second["hooks"][event].as_array().unwrap();
            let vigil_count = groups.iter().filter(|g| {
                g["hooks"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter()
                    .any(|h| h["command"].as_str().unwrap_or("") == HOOK_EXE)
            }).count();
            assert_eq!(vigil_count, 1, "Doublon détecté pour l'événement {event}");
        }
    }

    #[test]
    fn compute_preserve_les_cles_non_hooks_du_settings() {
        let before = json!({
            "env": {"DEBUG": "true"},
            "permissions": ["allow-all"]
        });
        let result = compute_claude_with_hooks(before, HOOK_EXE);

        assert_eq!(result["env"]["DEBUG"], "true");
        assert_eq!(result["permissions"][0], "allow-all");
    }

    #[test]
    fn compute_claude_timeout_uniquement_sur_permission_request() {
        // `PermissionRequest` est l'event bloquant (l'utilisateur décide) : plafond 1800s pour
        // survivre à l'auto-deny effectif de 600s. Tous les autres events sont fire-and-forget
        // et ne doivent PAS porter de timeout (défaut Claude Code, non bloquant).
        let result = compute_claude_with_hooks(json!({}), HOOK_EXE);
        for event in CLAUDE_EVENTS {
            let handler = &result["hooks"][event][0]["hooks"][0];
            assert_eq!(handler["command"], HOOK_EXE);
            if *event == "PermissionRequest" {
                // Asservi à la source de vérité unique (AC5) : un `1800` recodé en dur dans
                // `compute_claude_with_hooks` doit être attrapé par ce test, pas passer inaperçu.
                assert_eq!(
                    handler["timeout"],
                    hook_timeout_ceiling_secs(&SourceTool::ClaudeCode),
                    "PermissionRequest doit porter le plafond de l'outil"
                );
            } else {
                assert!(
                    handler.get("timeout").is_none(),
                    "{event} ne doit PAS porter de timeout (fire-and-forget)"
                );
            }
        }
    }

    // ── read_settings ────────────────────────────────────────────────────────

    #[test]
    fn read_settings_retourne_objet_vide_si_fichier_inexistant() {
        let path = std::path::Path::new("chemin/inexistant/settings.json");
        let result = read_settings(path).unwrap();
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_settings_parse_json_valide() {
        let tmp = std::env::temp_dir().join("vigil_test_settings.json");
        std::fs::write(&tmp, r#"{"hooks":{}}"#).unwrap();
        let result = read_settings(&tmp).unwrap();
        assert!(result["hooks"].is_object());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn read_settings_echoue_sur_json_invalide() {
        let tmp = std::env::temp_dir().join("vigil_test_invalid.json");
        std::fs::write(&tmp, "pas du json {{{").unwrap();
        assert!(read_settings(&tmp).is_err());
        std::fs::remove_file(&tmp).ok();
    }

    // ── compute_claude_without_hooks ───────────────────────────────────────

    #[test]
    fn compute_without_hooks_retire_les_6_evenements_vigil() {
        let with = compute_claude_with_hooks(json!({}), HOOK_EXE);
        let without = compute_claude_without_hooks(with, HOOK_EXE);
        assert!(!are_claude_hooks_installed(&without, HOOK_EXE));
    }

    #[test]
    fn compute_without_hooks_preserve_hooks_tiers() {
        let tiers = json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "C:\\MonOutil\\logger.exe"}]
        });
        let with_vigil_and_tiers = compute_claude_with_hooks(
            json!({ "hooks": { "PostToolUse": [tiers.clone()] } }),
            HOOK_EXE,
        );
        let without_vigil = compute_claude_without_hooks(with_vigil_and_tiers, HOOK_EXE);

        assert!(!are_claude_hooks_installed(&without_vigil, HOOK_EXE));
        let tiers_present = without_vigil["hooks"]["PostToolUse"]
            .as_array().unwrap().iter().any(|g| {
                g["hooks"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter()
                    .any(|h| h["command"].as_str().unwrap_or("") == "C:\\MonOutil\\logger.exe")
            });
        assert!(tiers_present);
    }

    #[test]
    fn compute_without_hooks_ne_modifie_pas_settings_sans_hooks_vigil() {
        let before = json!({ "env": { "DEBUG": "true" } });
        let after = compute_claude_without_hooks(before.clone(), HOOK_EXE);
        assert_eq!(before, after);
    }

    // ── T7: Tests story 6.3 ──────────────────────────────────────────────────

    #[test]
    #[ignore = "nécessite Claude Code installé sur ce poste (~/.claude/settings.json)"]
    fn detect_claude_code_depuis_settings_json() {
        assert!(get_claude_settings_path().exists());
    }

    #[test]
    #[ignore = "nécessite Codex CLI installé sur ce poste (~/.codex/hooks.json)"]
    fn detect_codex_depuis_hooks_json() {
        assert!(get_codex_hooks_path().exists());
    }

    #[test]
    fn detect_grok_depuis_dossier() {
        // AC4 — détection via le dossier `~/.grok/` (pas le fichier de hook Vigil). Teste
        // seulement que la détection ne panique pas, quel que soit l'environnement.
        let _detected = home_dir().join(".grok").exists()
            || tool_in_path("grok.exe")
            || tool_in_path("grok-build.exe");
    }

    #[test]
    fn get_tool_hook_diff_claude_install() {
        let hook_exe = "C:/test/vigil-claude.exe";
        let before = json!({});
        let after = compute_claude_with_hooks(before.clone(), hook_exe);
        let before_str = serde_json::to_string_pretty(&before).unwrap();
        let after_str = serde_json::to_string_pretty(&after).unwrap();
        assert!(!before_str.contains(hook_exe));
        assert!(after_str.contains(hook_exe));
        assert!(are_claude_hooks_installed(&after, hook_exe));
    }

    #[test]
    fn get_tool_hook_diff_claude_uninstall() {
        let hook_exe = "C:/test/vigil-claude.exe";
        let with_hooks = compute_claude_with_hooks(json!({}), hook_exe);
        let without_hooks = compute_claude_without_hooks(with_hooks.clone(), hook_exe);
        assert!(are_claude_hooks_installed(&with_hooks, hook_exe));
        assert!(!are_claude_hooks_installed(&without_hooks, hook_exe));
    }

    #[test]
    fn install_codex_hooks_cree_fichier_si_absent() {
        let tmp = std::env::temp_dir().join("vigil_test_codex_create.json");
        let _ = std::fs::remove_file(&tmp);

        install_codex_hooks_impl(&tmp, CODEX_HOOK_EXE).unwrap();

        let config = read_settings(&tmp).unwrap();
        assert!(are_codex_hooks_installed(&config, CODEX_HOOK_EXE));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn uninstall_hooks_restaure_etat_initial() {
        let tmp = std::env::temp_dir().join("vigil_test_codex_roundtrip.json");
        let initial = json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "other.exe" }] }]
            }
        });
        std::fs::write(&tmp, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        install_codex_hooks_impl(&tmp, CODEX_HOOK_EXE).unwrap();
        let after_install = read_settings(&tmp).unwrap();
        assert!(are_codex_hooks_installed(&after_install, CODEX_HOOK_EXE));

        uninstall_codex_hooks_impl(&tmp, CODEX_HOOK_EXE).unwrap();
        let after_uninstall = read_settings(&tmp).unwrap();
        assert!(!are_codex_hooks_installed(&after_uninstall, CODEX_HOOK_EXE));

        let other_present = after_uninstall["hooks"]["SessionStart"]
            .as_array().unwrap_or(&vec![]).iter().any(|g| {
                g["hooks"].as_array().unwrap_or(&vec![]).iter()
                    .any(|h| h["command"].as_str().unwrap_or("") == "other.exe")
            });
        assert!(other_present, "Les hooks tiers doivent être préservés après uninstall");

        std::fs::remove_file(&tmp).ok();
    }

    // ── Grok hooks (Story 6.26 — fichier dédié Vigil-owned) ─────────────────────

    #[test]
    fn grok_with_hooks_structure_canonique_tous_events() {
        // AC1/AC3 — structure canonique complète : `hooks` → chaque event PascalCase de GROK_EVENTS
        // porte `[{ "hooks": [{ "type":"command", "command": exe }] }]`.
        let config = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        let hooks = config["hooks"].as_object().expect("hooks doit être un objet");
        assert_eq!(hooks.len(), GROK_EVENTS.len(), "un seul bloc par event, aucun en trop");
        for event in GROK_EVENTS {
            let group = &config["hooks"][event][0];
            let handler = &group["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], GROK_HOOK_EXE);
            assert!(group.get("matcher").is_none(), "pas de matcher pour Grok (≠ Claude)");
            // Story 6.30 : `timeout: 1800` UNIQUEMENT sur `PreToolUse` (event bloquant approbateur),
            // absent sur tous les autres.
            if *event == "PreToolUse" {
                // Asservi à la source de vérité unique (AC5) — cf. test Claude.
                assert_eq!(
                    handler["timeout"],
                    hook_timeout_ceiling_secs(&SourceTool::Grok),
                    "PreToolUse doit porter le plafond de l'outil"
                );
            } else {
                assert!(
                    handler.get("timeout").is_none(),
                    "{event} ne doit PAS porter de timeout (fire-and-forget)"
                );
            }
        }
        // UserPromptSubmit + SessionEnd présents (AC3).
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("SessionEnd"));
        // AC5 : le timeout supplémentaire ne casse pas la conformité (are_grok_hooks_installed
        // vérifie type/command et tolère la clé en plus).
        assert!(are_grok_hooks_installed(&config, GROK_HOOK_EXE));
    }

    #[test]
    fn grok_ignore_entree_existante() {
        // Vigil possède le fichier : une entrée tierce dans l'input est ignorée (pas de merge).
        let with_tiers = json!({ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "command", "command": "tiers.exe" }
        ] }] }, "autre": true });
        let out = compute_grok_with_hooks(with_tiers, GROK_HOOK_EXE);
        assert!(out.get("autre").is_none(), "clé tierce ignorée");
        let pre = out["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "un seul groupe : le nôtre");
        assert_eq!(pre[0]["hooks"][0]["command"], GROK_HOOK_EXE);
    }

    #[test]
    fn grok_without_hooks_est_vide() {
        // Représentation « après » du diff = objet vide.
        let out = compute_grok_without_hooks(json!({ "hooks": { "Stop": [] } }), GROK_HOOK_EXE);
        assert_eq!(out, json!({}));
    }

    #[test]
    fn are_grok_hooks_installed_present_et_absent() {
        let installed = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        assert!(are_grok_hooks_installed(&installed, GROK_HOOK_EXE));
        // Absent : structure vide.
        assert!(!are_grok_hooks_installed(&json!({}), GROK_HOOK_EXE));
        // Un event manquant → non conforme.
        let mut partial = installed.clone();
        partial["hooks"].as_object_mut().unwrap().remove("SessionEnd");
        assert!(!are_grok_hooks_installed(&partial, GROK_HOOK_EXE));
    }

    /// Forme périmée plausible : ancien emplacement d'install. **Doit différer de `GROK_HOOK_EXE`
    /// tout en gardant le basename `vigil-grok.exe`** — c'est par lui que le matcher tolérant le
    /// reconnaît comme nôtre ; un basename étranger ferait passer les tests pour la mauvaise raison
    /// (bloc non identifié, donc non compté).
    const GROK_PERIME: &str = "D:/ancien/vigil-grok.exe";

    /// ⚠️ Récupéré de la Story 10.2 (T2b) à son abandon, 2026-07-29.
    ///
    /// L'identification par ÉGALITÉ EXACTE avec le chemin courant ne voyait pas un handler Vigil au
    /// chemin périmé posé **à côté** du canonique : conformité verte, auto-repair jamais sollicité,
    /// et l'orphelin tire à chaque event, en plus du bon. Éprouvé dans les deux dispositions, car
    /// le décompte traverse les groupes : orphelin DANS notre groupe, et dans un SECOND groupe.
    #[test]
    fn grok_orphelin_perime_cohabitant_rend_la_conformite_fausse() {
        let mut dans_le_groupe = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        dans_le_groupe["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "command", "command": GROK_PERIME }));
        assert!(
            !are_grok_hooks_installed(&dans_le_groupe, GROK_HOOK_EXE),
            "un orphelin périmé DANS notre groupe doit rendre la conformité fausse"
        );

        let mut second_groupe = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        second_groupe["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "hooks": [{ "type": "command", "command": GROK_PERIME }] }));
        assert!(
            !are_grok_hooks_installed(&second_groupe, GROK_HOOK_EXE),
            "un orphelin périmé dans un SECOND groupe doit rendre la conformité fausse"
        );
    }

    /// La leçon Kimi que la 10.1 a répliquée partout **sauf ici** (son AC8 interdisait de toucher à
    /// Grok) : `.any()` laisse passer un doublon — donc un double-fire — sans jamais solliciter la
    /// réparation qui l'aurait dédoublonné.
    #[test]
    fn grok_doublon_exact_rend_la_conformite_fausse() {
        let mut config = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        config["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "hooks": [grok_hook_handler(GROK_HOOK_EXE, "Stop")] }));
        assert!(!are_grok_hooks_installed(&config, GROK_HOOK_EXE));
    }

    /// Le `timeout` n'entrait pas dans la conformité, faute de canonique partagé entre génération et
    /// vérification : un plafond modifié (120 → 600 en 8.8) laissait le fichier périmé **sans
    /// réparation**, donc la barrière bloquante de `PreToolUse` avec la mauvaise fenêtre.
    #[test]
    fn grok_timeout_perime_du_pretooluse_rend_la_conformite_fausse() {
        let mut config = compute_grok_with_hooks(Value::Null, GROK_HOOK_EXE);
        config["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = json!(5);
        assert!(!are_grok_hooks_installed(&config, GROK_HOOK_EXE));
    }

    /// Garde ANTI-RÉGRESSION du passage au matcher tolérant : reconnaître notre binaire par son
    /// basename ne doit jamais faire passer un chemin périmé **seul** pour canonique — ce serait
    /// l'exact contraire du service rendu, et un hook mort déclaré conforme.
    #[test]
    fn grok_chemin_perime_seul_rend_la_conformite_fausse() {
        let config = compute_grok_with_hooks(Value::Null, GROK_PERIME);
        assert!(are_grok_hooks_installed(&config, GROK_PERIME), "témoin du banc");
        assert!(!are_grok_hooks_installed(&config, GROK_HOOK_EXE));
    }

    #[test]
    fn grok_install_ecrit_fichier_dedie_puis_uninstall_le_supprime() {
        // AC1/AC2 — install crée le fichier avec la structure canonique ; uninstall le SUPPRIME.
        let dir = std::env::temp_dir().join(format!("vigil_grok_test_{}", std::process::id()));
        let path = dir.join("hooks").join("vigil.json");
        let _ = std::fs::remove_dir_all(&dir);

        install_grok_hooks_impl(&path, GROK_HOOK_EXE).unwrap();
        assert!(path.exists(), "le fichier vigil.json doit être créé (dossier parent inclus)");
        let config = read_settings(&path).unwrap();
        assert!(are_grok_hooks_installed(&config, GROK_HOOK_EXE));

        uninstall_grok_hooks_impl(&path, GROK_HOOK_EXE).unwrap();
        assert!(!path.exists(), "le fichier doit être supprimé à l'uninstall");

        // Idempotent : ré-uninstall sur fichier absent = Ok (NotFound toléré).
        assert!(uninstall_grok_hooks_impl(&path, GROK_HOOK_EXE).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_hooks_file_path_pointe_dossier_hooks() {
        let path = get_grok_hooks_file_path();
        assert!(path.ends_with(std::path::Path::new(".grok").join("hooks").join("vigil.json")));
    }

    #[test]
    fn enabled_tools_persiste_correctement() {
        let path = get_enabled_tools_path();
        let backup = path.exists().then(|| std::fs::read_to_string(&path).ok()).flatten();

        let tools = EnabledTools { claude_code: true, codex: false, grok: false, antigravity: false, kimi: false };
        write_enabled_tools(&tools).unwrap();
        let read_back = read_enabled_tools();

        match backup {
            Some(content) => { let _ = std::fs::write(&path, content); }
            None => { let _ = std::fs::remove_file(&path); }
        }

        assert!(read_back.claude_code);
        assert!(!read_back.codex);
        assert!(!read_back.grok);
        assert!(!read_back.antigravity);
    }

    #[test]
    fn are_hooks_conformant_true_si_hooks_presents() {
        let config = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        assert!(are_codex_hooks_installed(&config, CODEX_HOOK_EXE));
    }

    #[test]
    fn are_hooks_conformant_false_si_hooks_absents() {
        assert!(!are_codex_hooks_installed(&json!({}), CODEX_HOOK_EXE));
        let with_hooks = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        let without_hooks = compute_codex_without_hooks(with_hooks, CODEX_HOOK_EXE);
        assert!(!are_codex_hooks_installed(&without_hooks, CODEX_HOOK_EXE));
    }

    #[test]
    fn codex_timeout_uniquement_sur_permission_request() {
        // Aligné sur Claude : `PermissionRequest` (event bloquant) porte timeout:1800 pour survivre
        // à l'auto-deny effectif de 600s ; les autres events restent fire-and-forget sans timeout.
        let config = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        for event in CODEX_EVENTS {
            let handler = &config["hooks"][event][0]["hooks"][0];
            assert_eq!(handler["command"], CODEX_HOOK_EXE);
            if *event == "PermissionRequest" {
                // Asservi à la source de vérité unique (AC5) — cf. test Claude.
                assert_eq!(
                    handler["timeout"],
                    hook_timeout_ceiling_secs(&SourceTool::Codex),
                    "PermissionRequest doit porter le plafond de l'outil"
                );
            } else {
                assert!(
                    handler.get("timeout").is_none(),
                    "{event} ne doit PAS porter de timeout (fire-and-forget)"
                );
            }
        }
        // Le champ timeout supplémentaire ne casse pas la détection de conformité.
        assert!(are_codex_hooks_installed(&config, CODEX_HOOK_EXE));
    }

    #[test]
    fn get_installation_required_aucun_outil() {
        let enabled = EnabledTools::default();
        assert!(check_installation_required_with(&enabled, false, false, false, false, false));
        assert!(check_installation_required_with(&enabled, true, true, true, true, true));
    }

    #[test]
    fn get_installation_required_hooks_compromis() {
        let enabled = EnabledTools { claude_code: true, codex: false, grok: false, antigravity: false, kimi: false };
        assert!(check_installation_required_with(&enabled, false, false, false, false, false));
        assert!(check_installation_required_with(&enabled, false, true, true, true, true));
    }

    #[test]
    fn get_installation_required_antigravity_compromis() {
        // Antigravity activé mais hooks non conformes → installation requise (parité multi-outils).
        let enabled = EnabledTools { claude_code: false, codex: false, grok: false, antigravity: true, kimi: false };
        assert!(check_installation_required_with(&enabled, true, true, true, false, false));
        assert!(!check_installation_required_with(&enabled, true, true, true, true, true));
    }

    #[test]
    fn get_installation_required_false_si_tous_ok() {
        let enabled = EnabledTools { claude_code: true, codex: true, grok: false, antigravity: false, kimi: false };
        assert!(!check_installation_required_with(&enabled, true, true, false, false, false));
        // grok/antigravity désactivés → leur état de hooks n'importe pas
        assert!(!check_installation_required_with(&enabled, true, true, true, true, true));
    }

    // ── Auto-repair (Story 6.8) ──────────────────────────────────────────────

    #[test]
    fn tools_needing_repair_aucun_si_aucun_outil_active() {
        let enabled = EnabledTools::default();
        // Premier lancement : aucun outil activé → rien à réparer, même hooks absents.
        assert!(tools_needing_repair(&enabled, false, false, false, false, false).is_empty());
    }

    #[test]
    fn tools_needing_repair_inclut_outil_active_compromis() {
        let enabled = EnabledTools { claude_code: true, codex: false, grok: false, antigravity: false, kimi: false };
        assert_eq!(tools_needing_repair(&enabled, false, false, false, false, false), vec!["claude_code"]);
    }

    #[test]
    fn tools_needing_repair_exclut_outil_active_conforme() {
        let enabled = EnabledTools { claude_code: true, codex: true, grok: false, antigravity: false, kimi: false };
        assert!(tools_needing_repair(&enabled, true, true, false, false, false).is_empty());
    }

    #[test]
    fn tools_needing_repair_exclut_outil_desactive_meme_compromis() {
        // codex/grok désactivés et non conformes → ne doivent PAS être réparés.
        let enabled = EnabledTools { claude_code: true, codex: false, grok: false, antigravity: false, kimi: false };
        assert_eq!(tools_needing_repair(&enabled, false, false, false, false, false), vec!["claude_code"]);
    }

    #[test]
    fn tools_needing_repair_par_outil_independant() {
        let enabled = EnabledTools { claude_code: true, codex: true, grok: true, antigravity: true, kimi: false };
        // seul codex compromis → seul codex réparé
        assert_eq!(tools_needing_repair(&enabled, true, false, true, true, true), vec!["codex"]);
    }

    #[test]
    fn tools_needing_repair_inclut_antigravity_active_compromis() {
        let enabled = EnabledTools { claude_code: false, codex: false, grok: false, antigravity: true, kimi: false };
        assert_eq!(tools_needing_repair(&enabled, true, true, true, false, false), vec!["antigravity"]);
    }

    #[test]
    fn tool_label_mappe_les_quatre_outils() {
        assert_eq!(tool_label("claude_code"), "Claude Code");
        assert_eq!(tool_label("codex"), "Codex CLI");
        assert_eq!(tool_label("grok"), "Grok Build");
        assert_eq!(tool_label("antigravity"), "Antigravity CLI");
    }

    // ── Antigravity hooks (Story 6.18) ───────────────────────────────────────

    #[test]
    fn antigravity_compute_structure_exacte_grouped_et_flat() {
        let config = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let vigil = &config["vigil"];

        // Grouped : PreToolUse / PostToolUse = [{ matcher: "*", hooks: [{type, command}] }].
        for ev in ["PreToolUse", "PostToolUse"] {
            let arr = vigil[ev].as_array().unwrap_or_else(|| panic!("{ev} doit être un array"));
            assert_eq!(arr.len(), 1, "{ev} : un seul groupe Vigil");
            assert_eq!(arr[0]["matcher"], "*");
            let handler = &arr[0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], format!("{} {}", ANTIGRAVITY_HOOK_EXE, ev));
        }

        // Flat : PreInvocation / Stop = [{type, command}] (pas de wrapper matcher/hooks).
        for ev in ["PreInvocation", "Stop"] {
            let arr = vigil[ev].as_array().unwrap_or_else(|| panic!("{ev} doit être un array"));
            assert_eq!(arr.len(), 1, "{ev} : un seul handler Vigil");
            assert_eq!(arr[0]["type"], "command");
            assert_eq!(arr[0]["command"], format!("{} {}", ANTIGRAVITY_HOOK_EXE, ev));
            assert!(arr[0].get("matcher").is_none(), "{ev} est Flat : pas de matcher");
            assert!(arr[0].get("hooks").is_none(), "{ev} est Flat : pas de wrapper hooks");
        }

        assert!(are_antigravity_hooks_installed(&config, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_commande_verbatim_casse() {
        // Le binaire compare `event_name == "PreToolUse"` (sensible casse) → la commande doit
        // porter l'event VERBATIM. On vérifie la présence exacte de "<exe> PreToolUse".
        let config = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let cmd = config["vigil"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.ends_with(" PreToolUse"), "commande = {cmd}");
        assert!(cmd.starts_with(ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_idempotence_pas_de_doublon() {
        let first = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let second = compute_antigravity_with_hooks(first, ANTIGRAVITY_HOOK_EXE);
        for ev in ANTIGRAVITY_EVENTS {
            let arr = second["vigil"][ev].as_array().unwrap();
            assert_eq!(arr.len(), 1, "Doublon détecté pour l'event {ev}");
        }
        assert!(are_antigravity_hooks_installed(&second, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_preserve_hook_tiers_top_level() {
        let before = json!({
            "other-hook": {
                "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "C:\\autre\\outil.exe" }] }]
            }
        });
        let after = compute_antigravity_with_hooks(before, ANTIGRAVITY_HOOK_EXE);
        // La clé de hook tierce est intacte.
        assert_eq!(
            after["other-hook"]["PreToolUse"][0]["hooks"][0]["command"],
            "C:\\autre\\outil.exe"
        );
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_preserve_handler_tiers_sous_vigil() {
        // Un handler tiers vit déjà sous la clé "vigil" (grouped PreToolUse + flat Stop).
        let before = json!({
            "vigil": {
                "PreToolUse": [{ "matcher": "run_command", "hooks": [{ "type": "command", "command": "C:\\tiers\\lint.exe" }] }],
                "Stop": [{ "type": "command", "command": "C:\\tiers\\cleanup.exe" }]
            }
        });
        let after = compute_antigravity_with_hooks(before, ANTIGRAVITY_HOOK_EXE);
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));

        // Grouped : le groupe tiers subsiste À CÔTÉ du groupe Vigil.
        let pre = after["vigil"]["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().any(|g| g["hooks"][0]["command"] == "C:\\tiers\\lint.exe"));
        assert!(pre.iter().any(|g| g["hooks"][0]["command"] == format!("{} PreToolUse", ANTIGRAVITY_HOOK_EXE)));

        // Flat : le handler tiers subsiste à côté du handler Vigil.
        let stop = after["vigil"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|h| h["command"] == "C:\\tiers\\cleanup.exe"));
        assert!(stop.iter().any(|h| h["command"] == format!("{} Stop", ANTIGRAVITY_HOOK_EXE)));
    }

    #[test]
    fn antigravity_are_installed_false_si_un_event_manque() {
        let mut config = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        // Retire un seul event → doit basculer à false.
        config["vigil"].as_object_mut().unwrap().remove("Stop");
        assert!(!are_antigravity_hooks_installed(&config, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_are_installed_false_sur_config_vide() {
        assert!(!are_antigravity_hooks_installed(&json!({}), ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_without_hooks_nettoie_et_retire_cle_vigil() {
        let with = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let without = compute_antigravity_without_hooks(with, ANTIGRAVITY_HOOK_EXE);
        assert!(!are_antigravity_hooks_installed(&without, ANTIGRAVITY_HOOK_EXE));
        // La clé "vigil" ne contenait que nos handlers → elle doit avoir disparu entièrement.
        assert!(without.get("vigil").is_none(), "clé vigil vidée doit être retirée : {without}");
    }

    #[test]
    fn antigravity_without_hooks_preserve_tiers() {
        let before = json!({
            "other-hook": {
                "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "C:\\autre\\outil.exe" }] }]
            },
            "vigil": {
                "Stop": [{ "type": "command", "command": "C:\\tiers\\cleanup.exe" }]
            }
        });
        let with = compute_antigravity_with_hooks(before, ANTIGRAVITY_HOOK_EXE);
        let without = compute_antigravity_without_hooks(with, ANTIGRAVITY_HOOK_EXE);

        assert!(!are_antigravity_hooks_installed(&without, ANTIGRAVITY_HOOK_EXE));
        // Hook tiers top-level intact.
        assert_eq!(
            without["other-hook"]["PreToolUse"][0]["hooks"][0]["command"],
            "C:\\autre\\outil.exe"
        );
        // La clé "vigil" subsiste car elle porte encore le handler tiers Stop.
        let stop = without["vigil"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|h| h["command"] == "C:\\tiers\\cleanup.exe"));
        assert!(!stop.iter().any(|h| h["command"].as_str().unwrap_or("").contains("vigil-antigravity")));
    }

    #[test]
    fn antigravity_orphelin_mauvaise_forme_detecte_et_retire() {
        // Robustesse (review 6.18) : une commande Vigil déposée sous la MAUVAISE forme —
        // à plat sous un event Grouped (PreToolUse), et en groupe sous un event Flat (Stop) —
        // doit être détectée (→ pas de double-fire après auto-repair) ET retirable (→ pas de fuite).
        let mal_forme = json!({
            "vigil": {
                "PreToolUse": [{ "type": "command", "command": format!("{} PreToolUse", ANTIGRAVITY_HOOK_EXE) }],
                "Stop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("{} Stop", ANTIGRAVITY_HOOK_EXE) }] }]
            }
        });
        // Détection tolérante aux deux formes (Story 10.1 : le comptage de conformité, qui a
        // remplacé le booléen `antigravity_event_has_vigil`, traverse Grouped ET Flat).
        let bas = ANTIGRAVITY_HOOK_EXE.to_ascii_lowercase();
        for event in ["PreToolUse", "Stop"] {
            let total: usize = mal_forme["vigil"][event]
                .as_array()
                .unwrap()
                .iter()
                .map(|el| antigravity_count_vigil(el, &bas))
                .sum();
            assert_eq!(total, 1, "orphelin de {event} non détecté");
        }

        // Retrait : l'orphelin de chaque forme disparaît, la clé "vigil" est nettoyée.
        let without = compute_antigravity_without_hooks(mal_forme, ANTIGRAVITY_HOOK_EXE);
        assert!(without.get("vigil").is_none(), "clé vigil devrait être retirée: {without:?}");
    }

    #[test]
    fn antigravity_with_hooks_preserve_valeur_non_array_sous_vigil() {
        // Robustesse (review 6.18) : collision de nom de hook "vigil" où un tiers a stocké une
        // valeur NON-array sous un event → elle doit être préservée (enveloppée), jamais écrasée.
        let before = json!({
            "vigil": { "PreToolUse": { "legacy": "valeur-tierce" } }
        });
        let with = compute_antigravity_with_hooks(before, ANTIGRAVITY_HOOK_EXE);
        let arr = with["vigil"]["PreToolUse"].as_array().expect("event devient un tableau");
        // La valeur tierce est préservée comme premier élément.
        assert!(arr.iter().any(|el| el["legacy"] == "valeur-tierce"), "valeur tierce perdue: {arr:?}");
        // Et la commande Vigil est bien installée à côté.
        assert!(are_antigravity_hooks_installed(&with, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_round_trip_with_are_without_are() {
        let config = json!({});
        let with = compute_antigravity_with_hooks(config, ANTIGRAVITY_HOOK_EXE);
        assert!(are_antigravity_hooks_installed(&with, ANTIGRAVITY_HOOK_EXE));
        let without = compute_antigravity_without_hooks(with, ANTIGRAVITY_HOOK_EXE);
        assert!(!are_antigravity_hooks_installed(&without, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_install_uninstall_impl_sur_fichier_temporaire() {
        let tmp = std::env::temp_dir().join("vigil_test_antigravity_roundtrip.json");
        let _ = std::fs::remove_file(&tmp);

        install_antigravity_hooks_impl(&tmp, ANTIGRAVITY_HOOK_EXE).unwrap();
        let after_install = read_settings(&tmp).unwrap();
        assert!(are_antigravity_hooks_installed(&after_install, ANTIGRAVITY_HOOK_EXE));

        uninstall_antigravity_hooks_impl(&tmp, ANTIGRAVITY_HOOK_EXE).unwrap();
        let after_uninstall = read_settings(&tmp).unwrap();
        assert!(!are_antigravity_hooks_installed(&after_uninstall, ANTIGRAVITY_HOOK_EXE));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn antigravity_are_installed_insensible_casse() {
        // Comparaison insensible à la casse sur le préfixe exe (parité Codex `to_ascii_lowercase`).
        let config = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let upper = ANTIGRAVITY_HOOK_EXE.to_ascii_uppercase();
        assert!(are_antigravity_hooks_installed(&config, &upper));
    }

    // ── Story 10.1 — Patron d'install unifié (matcher tolérant + retire-puis-réécrit) ─────
    //
    // Ces verrous portent sur les DEUX notions couplées : le matcher TOLÉRANT (retrait) et la
    // conformité STRICTE (« exactement 1 bloc exactement canonique par event »). C'est le couplage
    // qui rend l'auto-réparation réelle — sans conformité stricte, l'auto-repair n'est jamais
    // sollicité (angle mort de `already_present`, revue 8.8) ; sans matcher tolérant, un bloc
    // périmé est indélogeable et l'install produit un DOUBLON (= double-fire silencieux).

    /// Chemins PÉRIMÉS (Vigil réinstallé ailleurs) : nom long, puis forme courte 8.3.
    const CLAUDE_PERIME_LONG: &str = "D:/ancien/vigil-claude.exe";
    const CLAUDE_PERIME_83: &str = "D:\\ANCIEN\\VIGIL-~1.EXE";
    const CODEX_PERIME_LONG: &str = "D:/ancien/vigil-codex.exe";
    const CODEX_PERIME_83: &str = "D:\\ANCIEN\\VIGIL-~2.EXE";

    /// Nombre de handlers portant EXACTEMENT `command` (casse indifférente) sous un event, tous
    /// groupes confondus. Sert à compter des doublons / à vérifier une survie tierce — jamais à
    /// juger la conformité (c'est le rôle de `are_*_hooks_installed`).
    fn compte_commandes(config: &Value, event: &str, command: &str) -> usize {
        config["hooks"][event]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .flat_map(|g| g["hooks"].as_array().map(Vec::as_slice).unwrap_or(&[]))
            .filter(|h| h["command"].as_str().unwrap_or("").eq_ignore_ascii_case(command))
            .count()
    }

    /// Idem pour Antigravity : compte à travers Flat (`{command}`) ET Grouped (`{hooks:[…]}`).
    fn compte_commandes_antigravity(config: &Value, event: &str, command: &str) -> usize {
        config[ANTIGRAVITY_HOOK_NAME][event]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|el| {
                let flat = usize::from(el["command"].as_str().unwrap_or("").eq_ignore_ascii_case(command));
                let grouped = el["hooks"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|h| h["command"].as_str().unwrap_or("").eq_ignore_ascii_case(command))
                    .count();
                flat + grouped
            })
            .sum()
    }

    // ── T1 : matcher tolérant partagé ────────────────────────────────────────

    #[test]
    fn matcher_reconnait_les_trois_formes_et_les_guillemets() {
        let bas = HOOK_EXE.to_ascii_lowercase();
        // 1. commande COURANTE, casse indifférente.
        assert!(command_designates_vigil(HOOK_EXE, &bas, VIGIL_CLAUDE_BASENAME));
        assert!(command_designates_vigil(&HOOK_EXE.to_ascii_uppercase(), &bas, VIGIL_CLAUDE_BASENAME));
        // 2. chemin PÉRIMÉ au bon basename (slashs des deux sens).
        assert!(command_designates_vigil(CLAUDE_PERIME_LONG, &bas, VIGIL_CLAUDE_BASENAME));
        assert!(command_designates_vigil(r"D:\ancien\VIGIL-CLAUDE.EXE", &bas, VIGIL_CLAUDE_BASENAME));
        // 3. forme courte 8.3, qui ne contient PAS le nom de l'outil.
        assert!(command_designates_vigil(CLAUDE_PERIME_83, &bas, VIGIL_CLAUDE_BASENAME));
        // 4. guillemets ENGLOBANTS (AC9) — aucun générateur n'en produit encore (Story 10.2), mais
        //    sans cette tolérance un bloc quoté deviendrait indélogeable au moment de la migration.
        assert!(command_designates_vigil(&format!("\"{HOOK_EXE}\""), &bas, VIGIL_CLAUDE_BASENAME));
        assert!(command_designates_vigil(&format!("\"{CLAUDE_PERIME_LONG}\""), &bas, VIGIL_CLAUDE_BASENAME));
        // 5. chemin périmé CONTENANT UN ESPACE : c'est le cas qu'une lecture « 1er token seul »
        //    raterait (elle couperait à `John`), et il doit rester délogeable.
        assert!(command_designates_vigil(
            r"C:\Users\John Doe\vigil\vigil-claude.exe",
            &bas,
            VIGIL_CLAUDE_BASENAME
        ));
    }

    #[test]
    fn matcher_reste_etroit_aucun_programme_tiers_happe() {
        // L'étroitesse porte sur l'IDENTITÉ du binaire désigné, pas sur la forme de la commande :
        // rien de ce qui ne désigne PAS notre binaire ne doit être happé.
        //
        // ⚠️ Revue 10.1 — le cas `"node vigil-claude.exe"` a été RETIRÉ d'ici : il ne passait que
        // parce qu'il ne contient aucun séparateur de chemin, ce qui donnait à ce test l'apparence
        // de verrouiller une étroitesse « aux commandes composées » que le matcher n'a jamais eue.
        // La forme avec chemin est désormais verrouillée dans le sens INVERSE, cf.
        // `matcher_retire_une_commande_tierce_qui_enrobe_notre_binaire`.
        let bas = HOOK_EXE.to_ascii_lowercase();
        for tiers in [
            r"C:\outils\vigilant.exe",
            r"C:\outils\mon-hook.exe",
            r"C:\outils\vigil-claude.bat",
            r"C:\outils\vigil-~.exe",
            r"C:\outils\vigil-~a.exe",
            "node vigil-claude.bat",
            // Le binaire d'un AUTRE outil Vigil n'est pas le nôtre (basename paramétré).
            r"C:\outils\vigil-codex.exe",
        ] {
            assert!(
                !command_designates_vigil(tiers, &bas, VIGIL_CLAUDE_BASENAME),
                "{tiers} ne doit PAS être happé"
            );
        }
    }

    #[test]
    fn matcher_retire_une_commande_tierce_qui_enrobe_notre_binaire() {
        // DÉCISION Steven (revue 10.1) : comportement VOULU, pas un faux positif toléré. Une
        // commande tierce qui enrobe notre binaire (wrapper, lanceur, script d'audit) fausse la
        // surveillance — payload relayé, timing, code de sortie de l'enrobeur au lieu du nôtre. On
        // la retire et on réécrit notre bloc canonique DIRECT.
        let bas = HOOK_EXE.to_ascii_lowercase();
        for enrobage in [
            r"node C:\x\vigil-claude.exe",
            r"powershell -File C:\scripts\audit.ps1 C:\Vigil\vigil-claude.exe",
            r"C:\tiers\wrapper.exe --watch C:\Vigil\vigil-claude.exe",
        ] {
            assert!(
                command_designates_vigil(enrobage, &bas, VIGIL_CLAUDE_BASENAME),
                "{enrobage} DOIT être reconnu : on ne conserve pas un enrobage de notre binaire"
            );
        }
        // Et l'install le remplace effectivement par le canonique, sans doublon.
        let avant = json!({ "hooks": { "Stop": [
            { "matcher": "", "hooks": [{ "type": "command", "command": r"node C:\x\vigil-claude.exe" }] }
        ] }});
        let apres = compute_claude_with_hooks(avant, HOOK_EXE);
        assert_eq!(apres["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(are_claude_hooks_installed(&apres, HOOK_EXE));
    }

    #[test]
    fn matcher_reconnait_le_double_wrap_de_guillemets() {
        // Préparation 10.2 : le spike `cmd` conclut au DOUBLE-WRAP (`/s` retire la paire externe).
        // Sans retrait répété des paires englobantes, le bloc legacy sous cette forme serait resté
        // indélogeable au moment de la migration — l'inverse du service rendu par l'AC9.
        let bas = HOOK_EXE.to_ascii_lowercase();
        assert!(command_designates_vigil(&format!("\"\"{HOOK_EXE}\"\""), &bas, VIGIL_CLAUDE_BASENAME));
        assert!(command_designates_vigil(
            &format!("\"\"\"{HOOK_EXE}\"\"\""),
            &bas,
            VIGIL_CLAUDE_BASENAME
        ));
        // Antigravity : double-wrap sur l'exe, event collé derrière.
        let bas_ag = ANTIGRAVITY_HOOK_EXE.to_ascii_lowercase();
        assert!(command_designates_vigil(
            &format!("\"\"{ANTIGRAVITY_HOOK_EXE}\"\" Stop"),
            &bas_ag,
            VIGIL_ANTIGRAVITY_BASENAME
        ));
    }

    /// Chemin d'install contenant un ESPACE, non quoté — ce que produit le repli de
    /// `get_vigil_antigravity_exe` quand la 8.3 est indisponible sur le volume.
    const ANTIGRAVITY_HOOK_EXE_ESPACE: &str =
        r"C:\Users\John Doe\AppData\Local\Vigil\vigil-antigravity.exe";

    #[test]
    fn antigravity_chemin_a_espace_non_quote_reste_delogeable() {
        // Revue 10.1 — RÉGRESSION corrigée : le matcher ne reconnaissait plus sa PROPRE commande sur
        // l'intersection « Antigravity + espace » (lecture entière butant sur l'event collé
        // derrière, lecture 1er token coupant à `john`). Mesuré alors : conformité fausse juste
        // après l'install, +1 élément par event à CHAQUE auto-repair (donc à chaque démarrage de
        // Vigil), et un uninstall qui ne retirait rien. L'ancien `starts_with` gérait ce cas.
        let exe = ANTIGRAVITY_HOOK_EXE_ESPACE;
        let bas = exe.to_ascii_lowercase();
        assert!(command_designates_vigil(&format!("{exe} PreToolUse"), &bas, VIGIL_ANTIGRAVITY_BASENAME));

        let une = compute_antigravity_with_hooks(json!({}), exe);
        assert!(are_antigravity_hooks_installed(&une, exe), "sortie d'install jugée non conforme");
        let trois = compute_antigravity_with_hooks(compute_antigravity_with_hooks(une.clone(), exe), exe);
        assert_eq!(trois, une, "3 installs ⇒ document identique (aucune croissance)");
        for event in ANTIGRAVITY_EVENTS {
            assert_eq!(trois[ANTIGRAVITY_HOOK_NAME][event].as_array().unwrap().len(), 1);
        }
        // Et un bloc au chemin à espace PÉRIMÉ est délogeable.
        let perime = json!({ ANTIGRAVITY_HOOK_NAME: {
            "Stop": [{ "type": "command", "command": r"D:\vieux dossier\vigil-antigravity.exe Stop" }],
        }});
        assert!(!are_antigravity_hooks_installed(&perime, exe));
        let after = compute_antigravity_with_hooks(perime, exe);
        assert_eq!(after[ANTIGRAVITY_HOOK_NAME]["Stop"].as_array().unwrap().len(), 1);
        let cleaned = compute_antigravity_without_hooks(after, exe);
        assert!(cleaned.get(ANTIGRAVITY_HOOK_NAME).is_none(), "uninstall incomplet : {cleaned}");
    }

    #[test]
    fn antigravity_argument_event_mal_casse_est_non_conforme_puis_repare() {
        // Revue 10.1 — `hook_block_matches_canonical` lave la casse du `command` ENTIER, or la
        // commande est `<exe> <event>` : `…exe stop` sous la clé `Stop` passait pour conforme. Le
        // binaire dispatche sur `match event_name`, SENSIBLE à la casse → event muet en permanence,
        // pastille verte, auto-repair jamais sollicité.
        let mut errone = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        errone[ANTIGRAVITY_HOOK_NAME]["Stop"][0]["command"] =
            json!(format!("{ANTIGRAVITY_HOOK_EXE} stop"));
        assert!(!are_antigravity_hooks_installed(&errone, ANTIGRAVITY_HOOK_EXE));
        let repare = compute_antigravity_with_hooks(errone, ANTIGRAVITY_HOOK_EXE);
        assert_eq!(repare[ANTIGRAVITY_HOOK_NAME]["Stop"].as_array().unwrap().len(), 1);
        assert!(are_antigravity_hooks_installed(&repare, ANTIGRAVITY_HOOK_EXE));
        // La casse du CHEMIN, elle, reste indifférente (non-régression 6.18).
        let upper = ANTIGRAVITY_HOOK_EXE.to_ascii_uppercase();
        let config = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        assert!(are_antigravity_hooks_installed(&config, &upper));
    }

    #[test]
    fn valeur_d_event_non_tableau_portant_un_bloc_vigil_est_normalisee() {
        // Revue 10.1 — le RETRAIT sautait une valeur d'event non-tableau (`and_then(as_array_mut)`)
        // alors que l'INSTALL l'enveloppait : (a) l'install produisait `[bloc périmé, canonique]`,
        // que sa propre conformité rejette — double-fire réel jusqu'au passage suivant ; (b) le bloc
        // survivait à l'uninstall, et l'aperçu de consentement mentait.
        let perime = json!({ "hooks": {
            "Stop": { "matcher": "", "hooks": [{ "type": "command", "command": CLAUDE_PERIME_LONG }] }
        }});
        let after = compute_claude_with_hooks(perime, HOOK_EXE);
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 1, "bloc périmé non retiré");
        assert!(are_claude_hooks_installed(&after, HOOK_EXE), "install produit une config non conforme");

        let a_desinstaller = json!({ "hooks": {
            "Stop": { "matcher": "", "hooks": [{ "type": "command", "command": HOOK_EXE }] }
        }});
        let cleaned = compute_claude_without_hooks(a_desinstaller, HOOK_EXE);
        assert!(cleaned["hooks"].as_object().unwrap().is_empty(), "uninstall incomplet : {cleaned}");

        // Un `null` sous une clé d'event n'est pas enveloppé puis conservé indéfiniment.
        let nul = compute_claude_with_hooks(json!({ "hooks": { "Stop": null } }), HOOK_EXE);
        assert_eq!(nul["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(are_claude_hooks_installed(&nul, HOOK_EXE));
    }

    #[test]
    fn handler_vigil_a_plat_sous_un_event_claude_est_detecte_et_retire() {
        // Revue 10.1 — un handler Vigil de forme Flat n'était ni compté (conformité VERTE à côté du
        // canonique, orphelin actif = double-fire jamais signalé) ni retiré (le `retain` conservait
        // tout élément sans clé `hooks`) → fuite permanente, uninstall compris. Antigravity traitait
        // déjà cette classe depuis la 6.18 ; le socle grouped l'ignorait.
        let orphelin = json!({ "hooks": { "Stop": [
            { "type": "command", "command": HOOK_EXE }
        ] }});
        assert!(!are_claude_hooks_installed(&orphelin, HOOK_EXE));
        let after = compute_claude_with_hooks(orphelin, HOOK_EXE);
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 1, "orphelin flat non retiré");
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));
        // Idem à côté d'un canonique déjà en place : la conformité doit voir les DEUX.
        let mut mixte = compute_claude_with_hooks(json!({}), HOOK_EXE);
        mixte["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "command", "command": HOOK_EXE }));
        assert!(!are_claude_hooks_installed(&mixte, HOOK_EXE));
        let cleaned = compute_claude_without_hooks(mixte, HOOK_EXE);
        assert!(cleaned["hooks"].as_object().unwrap().is_empty(), "orphelin flat survit : {cleaned}");
    }

    #[test]
    fn groupe_tiers_deja_vide_est_preserve() {
        // Revue 10.1 (AC7) — le `retain` jugeait TOUS les groupes de l'event, pas seulement ceux
        // qu'on venait de vider : un placeholder tiers `{matcher, hooks: []}` disparaissait, et avec
        // lui la clé d'event que Vigil n'avait jamais créée.
        let before = json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [] }] } });
        let cleaned = compute_claude_without_hooks(before.clone(), HOOK_EXE);
        assert_eq!(cleaned, before, "un groupe tiers déjà vide doit rester intact");
        let after = compute_claude_with_hooks(before, HOOK_EXE);
        assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));

        // Antigravity : même garantie sur son propre socle.
        let ag_before = json!({ ANTIGRAVITY_HOOK_NAME: {
            "PreToolUse": [{ "matcher": "run_command", "hooks": [] }],
        }});
        let ag_cleaned = compute_antigravity_without_hooks(ag_before.clone(), ANTIGRAVITY_HOOK_EXE);
        assert_eq!(ag_cleaned, ag_before);
    }

    // ── Verrou : `cleanup-hooks.ps1` ne doit pas RE-diverger du cœur pur ──────
    //
    // Ce script (NSIS_HOOK_PREUNINSTALL) porte la DERNIÈRE copie de la logique de retrait que le
    // patron unifié n'absorbe pas — PowerShell ne peut pas appeler le cœur Rust. Sa divergence est
    // par nature SILENCIEUSE : elle ne se voit qu'à la désinstallation, chez l'utilisateur. Elle
    // avait vécu des mois (3 events Codex de la 6.14 jamais ajoutés). `include_str!` fait de ce
    // fichier une dépendance de COMPILATION : le déplacer casse le build ici, jamais en silence.
    const CLEANUP_SCRIPT: &str = include_str!("../../windows/cleanup-hooks.ps1");

    /// Tranche du script comprise entre deux en-têtes de section.
    ///
    /// La vérification DOIT être sectionnelle : `UserPromptSubmit`, `SubagentStart` et
    /// `SubagentStop` — précisément les 3 events qui manquaient à Codex — figurent aussi dans la
    /// liste de Claude. Un `contains` sur le script ENTIER aurait donc validé le bug d'origine.
    fn cleanup_section(from: &str, to: &str) -> &'static str {
        let start = CLEANUP_SCRIPT
            .find(from)
            .unwrap_or_else(|| panic!("en-tête `{from}` absente de cleanup-hooks.ps1"));
        let rest = &CLEANUP_SCRIPT[start..];
        let end = rest.find(to).unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn cleanup_script_couvre_tous_les_events_de_claude_et_codex() {
        let claude = cleanup_section("# ── Claude Code :", "# ── Codex CLI :");
        for event in CLAUDE_EVENTS {
            assert!(
                claude.contains(&format!("'{event}'")),
                "section Claude de `cleanup-hooks.ps1` : event `{event}` absent → le désinstalleur \
                 laissera un bloc orphelin pointant un binaire supprimé"
            );
        }
        let codex = cleanup_section("# ── Codex CLI :", "# ── Grok Build :");
        for event in CODEX_EVENTS {
            assert!(
                codex.contains(&format!("'{event}'")),
                "section Codex de `cleanup-hooks.ps1` : event `{event}` absent → le désinstalleur \
                 laissera un bloc orphelin pointant un binaire supprimé"
            );
        }
    }

    #[test]
    fn cleanup_script_tolere_la_forme_quotee_de_la_story_10_2() {
        // Le JOKER FINAL du motif : sans lui, le `-like` exige que la commande TERMINE par le
        // basename, et le guillemet fermant de la forme quotée (Story 10.2) le fait échouer — le
        // script cesserait SILENCIEUSEMENT de nettoyer Claude et Codex.
        for motif in ["'*vigil-claude.exe*'", "'*vigil-codex.exe*'"] {
            assert!(
                CLEANUP_SCRIPT.contains(motif),
                "`cleanup-hooks.ps1` doit porter le motif {motif} (joker final compris)"
            );
        }
    }

    /// Story 10.3 — le désinstalleur NSIS, verrouillé au même titre que `cleanup-hooks.ps1`.
    const INSTALLER_HOOKS: &str = include_str!("../../windows/installer-hooks.nsh");

    #[test]
    fn desinstalleur_supprime_le_miroir_mais_jamais_pendant_une_mise_a_jour() {
        // AC7 — le PIÈGE de cette story. `cleanup-hooks.ps1` tourne AUSSI au pre-uninstall d'une
        // simple mise à jour : y supprimer les binaires provoquerait une recopie complète à chaque
        // update. La suppression appartient au désinstalleur, et le garde `$UpdateMode` est ce qui
        // l'empêche de mordre sur une mise à jour. Sans lui, du churn à chaque version.
        assert!(
            INSTALLER_HOOKS.contains("NSIS_HOOK_POSTUNINSTALL"),
            "le miroir doit être supprimé au POST-uninstall (l'app est fermée, plus aucun binaire \
             n'est verrouillé par un processus en vol)"
        );
        // ⚠️ Borné à `!macroend` (revue 10.3) : sans cette borne, la tranche courait jusqu'à la FIN
        // du fichier, et trois `contains` indépendants ne disaient rien de l'imbrication. Sortir le
        // `Remove-Item` du bloc `${If}` laissait le test vert — un verrou décoratif posé sur
        // exactement le piège que la story déclare le plus dangereux.
        let post = INSTALLER_HOOKS
            .split("!macro NSIS_HOOK_POSTUNINSTALL")
            .nth(1)
            .expect("macro POSTUNINSTALL absente")
            .split("!macroend")
            .next()
            .expect("`!macroend` absent de la macro POSTUNINSTALL");

        // La garde existe...
        let apres_garde = post
            .split("${If} $UpdateMode <> 1")
            .nth(1)
            .expect("la suppression du miroir DOIT être gardée par `${If} $UpdateMode <> 1`");
        let bloc_garde = apres_garde
            .split("${EndIf}")
            .next()
            .expect("`${EndIf}` absent : le bloc gardé n'est pas refermé");

        // ...et c'est bien DEDANS que la suppression a lieu.
        assert!(
            bloc_garde.contains("Remove-Item") && bloc_garde.contains("ProgramData"),
            "la suppression de %ProgramData%\\Vigil\\hooks\\<SID> doit se trouver À L'INTÉRIEUR du \
             bloc `${{If}} $UpdateMode <> 1`"
        );

        // ...et nulle part ailleurs : une seconde suppression hors garde mordrait sur les updates.
        let avant_garde = post.split("${If} $UpdateMode <> 1").next().unwrap_or("");
        let apres_fin = apres_garde.split("${EndIf}").nth(1).unwrap_or("");
        for hors_bloc in [avant_garde, apres_fin] {
            assert!(
                !hors_bloc.contains("Remove-Item"),
                "aucune suppression ne doit vivre HORS du bloc gardé : `cleanup-hooks.ps1` tourne \
                 au pre-uninstall d'une MISE À JOUR, et une suppression non gardée y provoquerait \
                 une recopie complète des cinq binaires à chaque version"
            );
        }
    }

    #[test]
    fn cleanup_hooks_ne_touche_jamais_aux_binaires_du_miroir() {
        // Le pendant du test précédent, côté script : c'est LUI qui tourne pendant les mises à jour.
        // Il ne nettoie que les fichiers de configuration des outils, jamais `%ProgramData%`.
        // Même motif que la clé d'activation, qu'il ne doit pas davantage effacer.
        assert!(
            !CLEANUP_SCRIPT.to_ascii_lowercase().contains("programdata"),
            "`cleanup-hooks.ps1` tourne au pre-uninstall d'une MISE À JOUR : s'il supprimait le \
             miroir, chaque update paierait une recopie complète des cinq binaires"
        );
    }

    #[test]
    fn matcher_antigravity_opere_sur_le_premier_token() {
        // La commande Antigravity est `<exe> <event>` : c'est le 1er token qui porte l'exe.
        let bas = ANTIGRAVITY_HOOK_EXE.to_ascii_lowercase();
        let courante = format!("{ANTIGRAVITY_HOOK_EXE} PreToolUse");
        assert!(command_designates_vigil(&courante, &bas, VIGIL_ANTIGRAVITY_BASENAME));
        // Chemin périmé (long puis 8.3), event collé derrière.
        assert!(command_designates_vigil(
            r"D:\ancien\vigil-antigravity.exe Stop",
            &bas,
            VIGIL_ANTIGRAVITY_BASENAME
        ));
        assert!(command_designates_vigil(
            r"D:\ANCIEN\VIGIL-~3.EXE PostToolUse",
            &bas,
            VIGIL_ANTIGRAVITY_BASENAME
        ));
        // Quoté (AC9) : le 1er token va jusqu'au guillemet FERMANT, espace compris.
        assert!(command_designates_vigil(
            r#""C:\Users\John Doe\vigil-antigravity.exe" PreInvocation"#,
            &bas,
            VIGIL_ANTIGRAVITY_BASENAME
        ));
        // Un tiers reste un tiers, même suivi d'un nom d'event.
        assert!(!command_designates_vigil(
            r"C:\tiers\lint.exe PreToolUse",
            &bas,
            VIGIL_ANTIGRAVITY_BASENAME
        ));
    }

    // ── Claude : retire-puis-réécrit ─────────────────────────────────────────

    #[test]
    fn claude_install_x2_une_seule_entree_par_event() {
        // AC6 — idempotence.
        let une = compute_claude_with_hooks(json!({}), HOOK_EXE);
        let deux = compute_claude_with_hooks(une.clone(), HOOK_EXE);
        assert_eq!(deux, une, "2 installs ⇒ document identique");
        for event in CLAUDE_EVENTS {
            assert_eq!(deux["hooks"][event].as_array().unwrap().len(), 1, "doublon sur {event}");
        }
        assert!(are_claude_hooks_installed(&deux, HOOK_EXE));
    }

    #[test]
    fn claude_bloc_au_timeout_perime_est_detecte_puis_repare() {
        // AC2 — `already_present` laissait ce bloc INTACT : un `timeout` périmé tue le hook de
        // permission AVANT l'auto-deny effectif, et l'auto-repair n'était jamais sollicité.
        let mut perime = compute_claude_with_hooks(json!({}), HOOK_EXE);
        perime["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"] = json!(60);
        assert!(
            !are_claude_hooks_installed(&perime, HOOK_EXE),
            "un timeout périmé doit rendre la config NON conforme (sinon jamais réparée)"
        );
        let repare = compute_claude_with_hooks(perime, HOOK_EXE);
        assert_eq!(repare["hooks"]["PermissionRequest"].as_array().unwrap().len(), 1, "réparé, pas doublé");
        assert_eq!(
            repare["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            json!(hook_timeout_ceiling_secs(&SourceTool::ClaudeCode))
        );
        assert!(are_claude_hooks_installed(&repare, HOOK_EXE));
    }

    #[test]
    fn claude_bloc_au_matcher_errone_est_detecte_puis_repare() {
        // AC4 — un `matcher` non vide restreint le hook à certains outils : le bloc porte bien
        // notre commande mais ne tirerait pas partout. La conformité compare donc le GROUPE entier,
        // pas seulement le handler.
        let mut errone = compute_claude_with_hooks(json!({}), HOOK_EXE);
        errone["hooks"]["PreToolUse"][0]["matcher"] = json!("Bash");
        assert!(!are_claude_hooks_installed(&errone, HOOK_EXE));
        let repare = compute_claude_with_hooks(errone, HOOK_EXE);
        assert_eq!(repare["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(repare["hooks"]["PreToolUse"][0]["matcher"], "");
        assert!(are_claude_hooks_installed(&repare, HOOK_EXE));
    }

    #[test]
    fn claude_bloc_au_chemin_perime_migre_sans_doublon_ni_orphelin() {
        // AC3/AC5 — chemin d'install obsolète (nom long ET forme 8.3) : détecté, retiré, remplacé.
        let perime = json!({ "hooks": {
            "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": CLAUDE_PERIME_LONG }] }],
            "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": CLAUDE_PERIME_83 }] }],
        }});
        assert!(!are_claude_hooks_installed(&perime, HOOK_EXE));
        let after = compute_claude_with_hooks(perime, HOOK_EXE);
        for event in ["SessionStart", "Stop"] {
            let groups = after["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event} : le bloc périmé est retiré, pas doublé");
            assert_eq!(groups[0]["hooks"][0]["command"], HOOK_EXE);
        }
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));
        // Après uninstall : ZÉRO entrée Vigil, sous aucune forme.
        let cleaned = compute_claude_without_hooks(after, HOOK_EXE);
        assert!(
            cleaned["hooks"].as_object().unwrap().is_empty(),
            "aucun event résiduel : {cleaned}"
        );
    }

    #[test]
    fn claude_doublon_rend_la_conformite_fausse_puis_est_dedoublonne() {
        // AC4 — un doublon (même command + même event) échappait à `.any()` : conformité VERTE
        // alors que le hook se déclencherait DEUX fois.
        let mut double = compute_claude_with_hooks(json!({}), HOOK_EXE);
        let clone = double["hooks"]["PreToolUse"][0].clone();
        double["hooks"]["PreToolUse"].as_array_mut().unwrap().push(clone);
        assert!(!are_claude_hooks_installed(&double, HOOK_EXE));
        // Variante plus vicieuse : les deux handlers dans le MÊME groupe.
        let mut interne = compute_claude_with_hooks(json!({}), HOOK_EXE);
        let handler = interne["hooks"]["Stop"][0]["hooks"][0].clone();
        interne["hooks"]["Stop"][0]["hooks"].as_array_mut().unwrap().push(handler);
        assert!(!are_claude_hooks_installed(&interne, HOOK_EXE));

        let after = compute_claude_with_hooks(double, HOOK_EXE);
        assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));
    }

    #[test]
    fn claude_hook_tiers_survit_a_l_install_et_a_l_uninstall() {
        // AC7 — deux formes de voisinage : un GROUPE tiers sous le même event, et un handler tiers
        // logé DANS notre propre groupe (le retrait est précis au HANDLER, pas au groupe : l'ancien
        // code emportait le handler tiers avec le groupe).
        let before = json!({ "hooks": {
            "PostToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "C:\\MonOutil\\logger.exe" }] },
                { "matcher": "", "hooks": [
                    { "type": "command", "command": HOOK_EXE },
                    { "type": "command", "command": "C:\\MonOutil\\audit.exe" },
                ] },
            ],
        }});
        let after = compute_claude_with_hooks(before, HOOK_EXE);
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));
        let cleaned = compute_claude_without_hooks(after.clone(), HOOK_EXE);
        for tiers in ["C:\\MonOutil\\logger.exe", "C:\\MonOutil\\audit.exe"] {
            assert_eq!(compte_commandes(&after, "PostToolUse", tiers), 1, "{tiers} doit survivre à l'install");
            assert_eq!(compte_commandes(&cleaned, "PostToolUse", tiers), 1, "{tiers} doit survivre à l'uninstall");
        }
        assert_eq!(compte_commandes(&cleaned, "PostToolUse", HOOK_EXE), 0);
    }

    #[test]
    fn claude_bloc_quote_est_reconnu_et_retire() {
        // AC9 — fixture SYNTHÉTIQUE : aucun générateur ne produit encore de guillemets (Story 10.2).
        // Ce verrou garantit que la migration du quoting se fera sans doublon.
        let quote = json!({ "hooks": { "SessionStart": [
            { "matcher": "", "hooks": [{ "type": "command", "command": format!("\"{HOOK_EXE}\"") }] }
        ] }});
        assert!(!are_claude_hooks_installed(&quote, HOOK_EXE), "un bloc quoté n'est pas le canonique de cette story");
        let after = compute_claude_with_hooks(quote, HOOK_EXE);
        assert_eq!(
            after["hooks"]["SessionStart"].as_array().unwrap().len(),
            1,
            "le bloc quoté est RETIRÉ, pas doublé"
        );
        assert!(are_claude_hooks_installed(&after, HOOK_EXE));
    }

    // ── Codex : retire-puis-réécrit ──────────────────────────────────────────

    #[test]
    fn codex_install_x2_une_seule_entree_par_event() {
        let une = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        let deux = compute_codex_with_hooks(une.clone(), CODEX_HOOK_EXE);
        assert_eq!(deux, une, "2 installs ⇒ document identique");
        for event in CODEX_EVENTS {
            assert_eq!(deux["hooks"][event].as_array().unwrap().len(), 1, "doublon sur {event}");
        }
        assert!(are_codex_hooks_installed(&deux, CODEX_HOOK_EXE));
    }

    #[test]
    fn codex_bloc_au_timeout_perime_est_detecte_puis_repare() {
        let mut perime = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        perime["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"] = json!(60);
        assert!(!are_codex_hooks_installed(&perime, CODEX_HOOK_EXE));
        let repare = compute_codex_with_hooks(perime, CODEX_HOOK_EXE);
        assert_eq!(repare["hooks"]["PermissionRequest"].as_array().unwrap().len(), 1);
        assert_eq!(
            repare["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            json!(hook_timeout_ceiling_secs(&SourceTool::Codex))
        );
        assert!(are_codex_hooks_installed(&repare, CODEX_HOOK_EXE));
    }

    #[test]
    fn codex_bloc_canonique_conserve_status_message() {
        // `statusMessage: "vigil hook"` fait partie du canonique — il ne doit pas disparaître dans
        // la refonte, et son absence rend la config non conforme (→ réparée).
        let config = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        for event in CODEX_EVENTS {
            assert_eq!(config["hooks"][event][0]["hooks"][0]["statusMessage"], "vigil hook", "event {event}");
        }
        let mut ampute = config.clone();
        ampute["hooks"]["Stop"][0]["hooks"][0]
            .as_object_mut()
            .unwrap()
            .remove("statusMessage");
        assert!(!are_codex_hooks_installed(&ampute, CODEX_HOOK_EXE));
    }

    #[test]
    fn codex_bloc_au_chemin_perime_migre_sans_doublon_ni_orphelin() {
        let perime = json!({ "hooks": {
            "SessionStart": [{ "hooks": [{ "type": "command", "command": CODEX_PERIME_LONG, "statusMessage": "vigil hook" }] }],
            "Stop": [{ "hooks": [{ "type": "command", "command": CODEX_PERIME_83 }] }],
        }});
        assert!(!are_codex_hooks_installed(&perime, CODEX_HOOK_EXE));
        let after = compute_codex_with_hooks(perime, CODEX_HOOK_EXE);
        for event in ["SessionStart", "Stop"] {
            let groups = after["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event} : le bloc périmé est retiré, pas doublé");
            assert_eq!(groups[0]["hooks"][0]["command"], CODEX_HOOK_EXE);
        }
        assert!(are_codex_hooks_installed(&after, CODEX_HOOK_EXE));
        let cleaned = compute_codex_without_hooks(after, CODEX_HOOK_EXE);
        assert!(cleaned["hooks"].as_object().unwrap().is_empty(), "aucun event résiduel : {cleaned}");
    }

    #[test]
    fn codex_doublon_rend_la_conformite_fausse_puis_est_dedoublonne() {
        let mut double = compute_codex_with_hooks(json!({}), CODEX_HOOK_EXE);
        let clone = double["hooks"]["PreToolUse"][0].clone();
        double["hooks"]["PreToolUse"].as_array_mut().unwrap().push(clone);
        assert!(!are_codex_hooks_installed(&double, CODEX_HOOK_EXE));
        let after = compute_codex_with_hooks(double, CODEX_HOOK_EXE);
        assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(are_codex_hooks_installed(&after, CODEX_HOOK_EXE));
    }

    #[test]
    fn codex_hook_tiers_survit_a_l_install_et_a_l_uninstall() {
        let before = json!({ "hooks": {
            "PostToolUse": [
                { "hooks": [{ "type": "command", "command": "C:\\MonOutil\\logger.exe" }] },
                { "hooks": [
                    { "type": "command", "command": CODEX_HOOK_EXE, "statusMessage": "vigil hook" },
                    { "type": "command", "command": "C:\\MonOutil\\audit.exe" },
                ] },
            ],
        }});
        let after = compute_codex_with_hooks(before, CODEX_HOOK_EXE);
        assert!(are_codex_hooks_installed(&after, CODEX_HOOK_EXE));
        let cleaned = compute_codex_without_hooks(after.clone(), CODEX_HOOK_EXE);
        for tiers in ["C:\\MonOutil\\logger.exe", "C:\\MonOutil\\audit.exe"] {
            assert_eq!(compte_commandes(&after, "PostToolUse", tiers), 1, "{tiers} survit à l'install");
            assert_eq!(compte_commandes(&cleaned, "PostToolUse", tiers), 1, "{tiers} survit à l'uninstall");
        }
        assert_eq!(compte_commandes(&cleaned, "PostToolUse", CODEX_HOOK_EXE), 0);
    }

    #[test]
    fn codex_bloc_quote_est_reconnu_et_retire() {
        let quote = json!({ "hooks": { "SessionStart": [
            { "hooks": [{ "type": "command", "command": format!("\"{CODEX_HOOK_EXE}\""), "statusMessage": "vigil hook" }] }
        ] }});
        assert!(!are_codex_hooks_installed(&quote, CODEX_HOOK_EXE));
        let after = compute_codex_with_hooks(quote, CODEX_HOOK_EXE);
        assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(are_codex_hooks_installed(&after, CODEX_HOOK_EXE));
    }

    // ── Antigravity : retire-puis-réécrit (Grouped ET Flat) ──────────────────

    #[test]
    fn antigravity_bloc_au_chemin_perime_migre_sans_doublon_ni_orphelin() {
        // AC3 — `antigravity_command_is_vigil` faisait un `starts_with(<exe courant>)` : un chemin
        // périmé n'était NI détecté NI retiré → doublon à chaque réinstallation. Le matcher
        // basename le rattrape, sous la forme Grouped comme sous la forme Flat.
        let perime = json!({ ANTIGRAVITY_HOOK_NAME: {
            "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "D:\\ancien\\vigil-antigravity.exe PreToolUse" }] }],
            "Stop": [{ "type": "command", "command": "D:\\ANCIEN\\VIGIL-~3.EXE Stop" }],
        }});
        assert!(!are_antigravity_hooks_installed(&perime, ANTIGRAVITY_HOOK_EXE));
        let after = compute_antigravity_with_hooks(perime, ANTIGRAVITY_HOOK_EXE);
        for event in ANTIGRAVITY_EVENTS {
            assert_eq!(after[ANTIGRAVITY_HOOK_NAME][event].as_array().unwrap().len(), 1, "doublon sur {event}");
        }
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
        // AC5 — après uninstall, plus aucune trace (la clé nommée "vigil" part avec).
        let cleaned = compute_antigravity_without_hooks(after, ANTIGRAVITY_HOOK_EXE);
        assert!(cleaned.get(ANTIGRAVITY_HOOK_NAME).is_none(), "clé vigil résiduelle : {cleaned}");
    }

    #[test]
    fn antigravity_bloc_au_matcher_errone_est_detecte_puis_repare() {
        // AC4 — `matcher: "*"` = tous les outils. Un matcher restreint laisserait le hook muet sur
        // la plupart des appels : non conforme, donc réparé.
        let mut errone = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        errone[ANTIGRAVITY_HOOK_NAME]["PreToolUse"][0]["matcher"] = json!("run_command");
        assert!(!are_antigravity_hooks_installed(&errone, ANTIGRAVITY_HOOK_EXE));
        let repare = compute_antigravity_with_hooks(errone, ANTIGRAVITY_HOOK_EXE);
        assert_eq!(repare[ANTIGRAVITY_HOOK_NAME]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(repare[ANTIGRAVITY_HOOK_NAME]["PreToolUse"][0]["matcher"], "*");
        assert!(are_antigravity_hooks_installed(&repare, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_doublon_rend_la_conformite_fausse_puis_est_dedoublonne() {
        let mut double = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let clone = double[ANTIGRAVITY_HOOK_NAME]["Stop"][0].clone();
        double[ANTIGRAVITY_HOOK_NAME]["Stop"].as_array_mut().unwrap().push(clone);
        assert!(!are_antigravity_hooks_installed(&double, ANTIGRAVITY_HOOK_EXE));
        let after = compute_antigravity_with_hooks(double, ANTIGRAVITY_HOOK_EXE);
        assert_eq!(after[ANTIGRAVITY_HOOK_NAME]["Stop"].as_array().unwrap().len(), 1);
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_orphelin_de_mauvaise_forme_rend_non_conforme_puis_disparait() {
        // Un handler Vigil sous la MAUVAISE forme (Flat sous un event Grouped) tirerait en double
        // à côté du canonique : la conformité doit le voir (count == 1 à travers les deux formes).
        let mut mixte = compute_antigravity_with_hooks(json!({}), ANTIGRAVITY_HOOK_EXE);
        let orphelin = json!({ "type": "command", "command": format!("{ANTIGRAVITY_HOOK_EXE} PreToolUse") });
        mixte[ANTIGRAVITY_HOOK_NAME]["PreToolUse"].as_array_mut().unwrap().push(orphelin);
        assert!(!are_antigravity_hooks_installed(&mixte, ANTIGRAVITY_HOOK_EXE));
        let after = compute_antigravity_with_hooks(mixte, ANTIGRAVITY_HOOK_EXE);
        assert_eq!(after[ANTIGRAVITY_HOOK_NAME]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_hook_tiers_survit_a_l_install_et_a_l_uninstall() {
        // AC7 — un groupe tiers (Grouped) et un handler tiers (Flat) sous la clé "vigil".
        let before = json!({ ANTIGRAVITY_HOOK_NAME: {
            "PreToolUse": [{ "matcher": "run_command", "hooks": [{ "type": "command", "command": "C:\\tiers\\lint.exe" }] }],
            "Stop": [{ "type": "command", "command": "C:\\tiers\\cleanup.exe" }],
        }});
        let after = compute_antigravity_with_hooks(before, ANTIGRAVITY_HOOK_EXE);
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
        let cleaned = compute_antigravity_without_hooks(after.clone(), ANTIGRAVITY_HOOK_EXE);
        assert_eq!(compte_commandes_antigravity(&after, "PreToolUse", "C:\\tiers\\lint.exe"), 1);
        assert_eq!(compte_commandes_antigravity(&cleaned, "PreToolUse", "C:\\tiers\\lint.exe"), 1);
        assert_eq!(compte_commandes_antigravity(&after, "Stop", "C:\\tiers\\cleanup.exe"), 1);
        assert_eq!(compte_commandes_antigravity(&cleaned, "Stop", "C:\\tiers\\cleanup.exe"), 1);
        assert!(!are_antigravity_hooks_installed(&cleaned, ANTIGRAVITY_HOOK_EXE));
    }

    #[test]
    fn antigravity_bloc_quote_est_reconnu_et_retire() {
        // AC9 — fixture synthétique quotée : le 1er token va jusqu'au guillemet fermant.
        let quote = json!({ ANTIGRAVITY_HOOK_NAME: {
            "Stop": [{ "type": "command", "command": format!("\"{ANTIGRAVITY_HOOK_EXE}\" Stop") }],
        }});
        assert!(!are_antigravity_hooks_installed(&quote, ANTIGRAVITY_HOOK_EXE));
        let after = compute_antigravity_with_hooks(quote, ANTIGRAVITY_HOOK_EXE);
        assert_eq!(after[ANTIGRAVITY_HOOK_NAME]["Stop"].as_array().unwrap().len(), 1, "le bloc quoté est RETIRÉ, pas doublé");
        assert!(are_antigravity_hooks_installed(&after, ANTIGRAVITY_HOOK_EXE));
    }
}

// ── Story 10.3 — L'invariant « chemin d'exe sans espace », verrouillé par outil ───────────────
//
// Ces tests couvrent les cinq outils sur le MÊME jeu de propriétés, parce que la leçon du spike est
// que la forme d'une commande ne prouve rien : le quoting qui réparait Kimi et Claude CASSE Grok,
// n'a aucun effet sur Antigravity, et fait annoncer « completed » à Codex sans qu'il exécute quoi
// que ce soit. Une seule propriété se vérifie de la même façon partout — *le chemin ne contient pas
// d'espace* — et c'est celle-là qu'on verrouille.
#[cfg(test)]
mod hook_path_tests {
    use super::*;

    /// Racine d'install PORTEUSE d'une espace — le cas que la story referme (`John Doe`, install
    /// hors défaut, volume à génération 8.3 désactivée).
    const RACINE_A_ESPACE: &str = r"C:\Users\John Doe\AppData\Local\vigil";
    /// Racine d'install sans espace — le cas majoritaire, qui ne doit RIEN changer (AC9).
    const RACINE_SANS_ESPACE: &str = r"C:\vigil";
    /// Dossier miroir tel que le produit `hook_mirror::mirror_dir()`.
    const MIROIR: &str = r"C:\ProgramData\Vigil\hooks\S-1-5-21-3039887197-950812763-1739380582-1001";

    /// Les cinq outils avec leur basename et leur convention d'écriture de chemin
    /// (`true` = forward-slashes, invocation via bash ; `false` = backslashes natifs, via `cmd`).
    const OUTILS: &[(&str, &str, bool)] = &[
        ("claude_code", VIGIL_CLAUDE_BASENAME, true),
        ("codex", VIGIL_CODEX_BASENAME, true),
        ("grok", VIGIL_GROK_BASENAME, true),
        ("antigravity", VIGIL_ANTIGRAVITY_BASENAME, false),
        ("kimi", VIGIL_KIMI_BASENAME, false),
    ];

    /// Commande telle que `resolve_hook_exe` la produirait, miroir disponible.
    fn commande_miroir(basename: &str, racine: &str, bash: bool) -> String {
        hook_mirror::hook_command_core(
            &Path::new(racine).join(basename),
            Some(&Path::new(MIROIR).join(basename)),
            None,
            bash,
        )
    }

    /// Nombre d'occurrences du basename dans une config sérialisée — mesure la présence de Vigil
    /// indépendamment de la structure propre à chaque outil.
    fn occurrences(rendu: &str, basename: &str) -> usize {
        rendu.to_ascii_lowercase().matches(basename).count()
    }

    // ── AC1 / AC8 — l'invariant, pour les 5 outils ────────────────────────────

    #[test]
    fn commande_sans_espace_ni_guillemet_pour_les_5_outils() {
        for (tool, basename, bash) in OUTILS {
            // Sur racine à espace ET sur racine sans espace : le miroir est SYSTÉMATIQUE, jamais
            // conditionné à la présence d'une espace. Un repli qui ne s'activerait que chez les
            // utilisateurs concernés serait dormant partout ailleurs — donc jamais éprouvé en
            // conditions réelles. C'est la définition même du fail-open dormant que l'Epic 10 combat.
            for racine in [RACINE_A_ESPACE, RACINE_SANS_ESPACE] {
                let cmd = commande_miroir(basename, racine, *bash);
                assert!(!cmd.contains(' '), "{tool} : espace résiduelle dans `{cmd}`");
                assert!(!cmd.contains('"'), "{tool} : guillemet dans `{cmd}`");
                assert!(
                    cmd.to_ascii_lowercase().ends_with(basename),
                    "{tool} : `{cmd}` ne désigne pas {basename}"
                );
            }
        }
    }

    #[test]
    fn chaque_outil_conserve_sa_convention_decriture() {
        // Le miroir change le DOSSIER, jamais la convention : un forward-slash en tête de commande
        // serait lu comme un délimiteur d'option par `cmd`, et un backslash serait un échappement
        // pour bash.
        for (tool, basename, bash) in OUTILS {
            let cmd = commande_miroir(basename, RACINE_A_ESPACE, *bash);
            if *bash {
                assert!(!cmd.contains('\\'), "{tool} passe par bash : forward-slashes attendus");
            } else {
                assert!(!cmd.contains('/'), "{tool} passe par cmd : backslashes natifs attendus");
            }
        }
    }

    // ── Revue 10.3 — le CÂBLAGE, pas une réimplémentation ─────────────────────

    #[test]
    fn la_convention_decriture_du_test_est_celle_de_la_production() {
        // `OUTILS` re-déclare la convention par outil : sans ce verrou, la table du test et celle de
        // la production pouvaient diverger en silence, et c'est la production qui part chez l'outil.
        for (tool, basename, bash) in OUTILS {
            assert_eq!(hook_binary_basename(tool), Some(*basename), "outil {tool}");
            assert_eq!(hook_uses_bash_slashes(tool), Some(*bash), "outil {tool}");
        }
        assert_eq!(hook_uses_bash_slashes("inconnu"), None);
    }

    #[test]
    fn le_chemin_de_production_respecte_la_convention_pour_les_5_outils() {
        // Exerce `hook_exe_for_tool` — le point d'entrée RÉEL des cinq `get_vigil_*_exe` — au lieu
        // de rejouer la cascade à côté. C'est le câblage outil→convention qui est vérifié ici :
        // aucun test ne l'atteignait, si bien qu'un `bash_slashes` inversé sur Kimi serait passé
        // inaperçu tout en envoyant des forward-slashes à `cmd`.
        //
        // Mode LECTURE SEULE (`provision = false`) : ce test ne doit RIEN créer sous %ProgramData%.
        for (tool, basename, bash) in OUTILS {
            let cmd = hook_exe_for_tool(tool, false).expect("outil connu");
            assert!(
                cmd.to_ascii_lowercase().ends_with(basename),
                "{tool} : `{cmd}` ne désigne pas {basename}"
            );
            assert!(!cmd.contains('"'), "{tool} : guillemet dans `{cmd}`");
            if *bash {
                assert!(!cmd.contains('\\'), "{tool} passe par bash : forward-slashes attendus");
            } else {
                assert!(!cmd.contains('/'), "{tool} passe par cmd : backslashes natifs attendus");
            }
        }
        assert_eq!(hook_exe_for_tool("inconnu", false), None);
    }

    #[test]
    fn hook_binary_basename_couvre_les_5_outils() {
        // Verrou de la table : un sixième outil qu'on oublierait d'y inscrire perdrait
        // silencieusement le miroir ET son contrôle de fraîcheur (AC4).
        for (tool, basename, _) in OUTILS {
            assert_eq!(hook_binary_basename(tool), Some(*basename), "outil {tool}");
        }
        assert_eq!(hook_binary_basename("inconnu"), None);
        assert!(!mirror_is_stale_for_tool("inconnu"));
    }

    // ── AC8 — aucun générateur ne produit de guillemets ───────────────────────

    #[test]
    fn aucun_generateur_ne_produit_de_guillemets_pour_les_5_outils() {
        // Le quoting est SORTI du chemin nominal : il réparait Kimi et Claude, mais casse Grok à lui
        // seul (spike F6, indépendamment de l'espace) et fait annoncer un faux succès à Codex (F7).
        // `matches('"')` compte les guillemets STRUCTURELS de JSON/TOML : on vérifie donc les
        // valeurs `command`, pas le rendu brut.
        for (_, basename, bash) in OUTILS {
            let exe = commande_miroir(basename, RACINE_A_ESPACE, *bash);
            for command in toutes_les_commandes(&exe, basename) {
                assert!(!command.contains('"'), "guillemet dans la commande générée `{command}`");
            }
        }
    }

    /// Toutes les valeurs `command` que Vigil écrit pour l'outil portant ce basename.
    fn toutes_les_commandes(exe: &str, basename: &str) -> Vec<String> {
        fn commandes_json(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(map) => {
                    for (k, val) in map {
                        if k == "command" {
                            if let Some(s) = val.as_str() {
                                out.push(s.to_string());
                            }
                        }
                        commandes_json(val, out);
                    }
                }
                Value::Array(items) => items.iter().for_each(|i| commandes_json(i, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        match basename {
            b if b == VIGIL_CLAUDE_BASENAME => {
                commandes_json(&compute_claude_with_hooks(json!({}), exe), &mut out)
            }
            b if b == VIGIL_CODEX_BASENAME => {
                commandes_json(&compute_codex_with_hooks(json!({}), exe), &mut out)
            }
            b if b == VIGIL_GROK_BASENAME => {
                commandes_json(&compute_grok_with_hooks(json!({}), exe), &mut out)
            }
            b if b == VIGIL_ANTIGRAVITY_BASENAME => {
                commandes_json(&compute_antigravity_with_hooks(json!({}), exe), &mut out)
            }
            _ => {
                let doc = compute_kimi_with_hooks(toml_edit::DocumentMut::new(), exe);
                for table in doc["hooks"].as_array_of_tables().into_iter().flatten() {
                    if let Some(c) = table.get("command").and_then(|c| c.as_str()) {
                        out.push(c.to_string());
                    }
                }
            }
        }
        assert!(!out.is_empty(), "aucune commande générée pour {basename}");
        out
    }

    // ── AC6 — migration de l'ancien chemin, sans doublon ──────────────────────

    /// Rend `(config après migration, config après désinstallation)` pour l'outil visé, en partant
    /// d'une config portant l'ANCIEN chemin `%LOCALAPPDATA%` **avec une espace**.
    fn migration(basename: &str, ancien: &str, nouveau: &str) -> (String, String, String) {
        match basename {
            b if b == VIGIL_CLAUDE_BASENAME => {
                let avant = compute_claude_with_hooks(json!({}), ancien);
                let apres = compute_claude_with_hooks(avant, nouveau);
                let propre = compute_claude_without_hooks(apres.clone(), nouveau);
                let zero = compute_claude_with_hooks(json!({}), nouveau);
                (apres.to_string(), propre.to_string(), zero.to_string())
            }
            b if b == VIGIL_CODEX_BASENAME => {
                let avant = compute_codex_with_hooks(json!({}), ancien);
                let apres = compute_codex_with_hooks(avant, nouveau);
                let propre = compute_codex_without_hooks(apres.clone(), nouveau);
                let zero = compute_codex_with_hooks(json!({}), nouveau);
                (apres.to_string(), propre.to_string(), zero.to_string())
            }
            b if b == VIGIL_GROK_BASENAME => {
                let avant = compute_grok_with_hooks(json!({}), ancien);
                let apres = compute_grok_with_hooks(avant, nouveau);
                let propre = compute_grok_without_hooks(apres.clone(), nouveau);
                let zero = compute_grok_with_hooks(json!({}), nouveau);
                (apres.to_string(), propre.to_string(), zero.to_string())
            }
            b if b == VIGIL_ANTIGRAVITY_BASENAME => {
                let avant = compute_antigravity_with_hooks(json!({}), ancien);
                let apres = compute_antigravity_with_hooks(avant, nouveau);
                let propre = compute_antigravity_without_hooks(apres.clone(), nouveau);
                let zero = compute_antigravity_with_hooks(json!({}), nouveau);
                (apres.to_string(), propre.to_string(), zero.to_string())
            }
            _ => {
                let avant = compute_kimi_with_hooks(toml_edit::DocumentMut::new(), ancien);
                let apres = compute_kimi_with_hooks(avant, nouveau);
                let propre = compute_kimi_without_hooks(apres.clone(), nouveau);
                let zero = compute_kimi_with_hooks(toml_edit::DocumentMut::new(), nouveau);
                (apres.to_string(), propre.to_string(), zero.to_string())
            }
        }
    }

    #[test]
    fn migration_ancien_chemin_vers_miroir_sans_doublon_pour_les_5_outils() {
        // AC6 — aucun code neuf n'est attendu ici : le matcher tolérant de la 10.1 reconnaît un bloc
        // par son BASENAME, donc l'ancien chemin `%LOCALAPPDATA%` (espace comprise) est reconnu,
        // retiré, puis réécrit vers le miroir. C'est à PROUVER, pas à supposer.
        for (tool, basename, bash) in OUTILS {
            let ancien = commande_ancienne(basename, *bash);
            let nouveau = commande_miroir(basename, RACINE_A_ESPACE, *bash);
            let (apres, propre, depuis_zero) = migration(basename, &ancien, &nouveau);

            // ⚠️ Revue 10.3 — la ligne GROK de cette boucle est VACUE, et il faut le dire plutôt que
            // de la laisser passer pour une preuve : `compute_grok_with_hooks` ignore son entrée et
            // reconstruit la structure canonique (Vigil possède `~/.grok/hooks/vigil.json` en
            // propre), `compute_grok_without_hooks` rend `{}`. Les trois assertions y sont donc
            // vraies par construction. La migration Grok EST gratuite — il n'y a rien à prouver —
            // mais l'AC6 n'est réellement ÉPROUVÉE que pour les quatre autres outils.
            assert_eq!(
                occurrences(&apres, basename),
                occurrences(&depuis_zero, basename),
                "{tool} : la migration doit produire EXACTEMENT autant d'entrées qu'une install neuve"
            );
            // Le nom de compte à espace est la signature de l'ancien chemin : il ne peut pas
            // apparaître dans un chemin miroir. On le cherche plutôt que le chemin entier, dont les
            // backslashes sont échappés différemment par JSON et par TOML.
            assert!(
                !apres.to_ascii_lowercase().contains("john doe"),
                "{tool} : l'ancien chemin `%LOCALAPPDATA%` doit avoir disparu de `{apres}`"
            );
            assert_eq!(
                occurrences(&propre, basename),
                0,
                "{tool} : zéro entrée résiduelle après désinstallation"
            );
        }
    }

    /// Chemin tel que Vigil l'écrivait AVANT cette story : à côté de l'exe principal, sous
    /// `%LOCALAPPDATA%` — donc porteur de l'espace du nom de compte.
    fn commande_ancienne(basename: &str, bash: bool) -> String {
        let brut = Path::new(RACINE_A_ESPACE).join(basename);
        let s = brut.to_string_lossy().into_owned();
        if bash {
            s.replace('\\', "/")
        } else {
            s
        }
    }

    #[test]
    fn migration_reconnait_aussi_la_forme_courte_83_laissee_par_lancienne_version() {
        // Antigravity et Kimi écrivaient une forme 8.3 (`VIGIL-~1.EXE`) qui ne contient PAS le nom
        // de l'outil : sans la reconnaître, le bloc legacy serait indélogeable et l'install
        // produirait un doublon à chaque passage — donc un double-fire sur une barrière de
        // permission.
        let ancien_83 = r"C:\Users\JOHNDO~1\AppData\Local\vigil\VIGIL-~1.EXE";
        let nouveau = commande_miroir(VIGIL_ANTIGRAVITY_BASENAME, RACINE_A_ESPACE, false);
        let avant = compute_antigravity_with_hooks(json!({}), ancien_83);
        let apres = compute_antigravity_with_hooks(avant, &nouveau);
        let depuis_zero = compute_antigravity_with_hooks(json!({}), &nouveau);
        assert_eq!(
            occurrences(&apres.to_string(), VIGIL_ANTIGRAVITY_BASENAME),
            occurrences(&depuis_zero.to_string(), VIGIL_ANTIGRAVITY_BASENAME)
        );
        assert!(are_antigravity_hooks_installed(&apres, &nouveau));
    }

    // ── AC5 — repli en cascade, jamais de panne dure ──────────────────────────

    #[test]
    fn repli_sur_83_puis_chemin_long_quand_le_miroir_est_indisponible() {
        for (tool, basename, bash) in OUTILS {
            let src = Path::new(RACINE_A_ESPACE).join(basename);
            let court = format!(r"C:\Users\JOHNDO~1\AppData\Local\vigil\{}", basename.to_uppercase());

            let via_83 = hook_mirror::hook_command_core(&src, None, Some(&court), *bash);
            assert!(!via_83.contains(' '), "{tool} : le repli 8.3 doit rester sans espace");

            // Dernier étage : l'espace revient, mais l'outil n'est JAMAIS laissé sans hook — le
            // miroir dégrade la résolution, il n'interrompt rien.
            let via_long = hook_mirror::hook_command_core(&src, None, None, *bash);
            assert!(via_long.to_ascii_lowercase().ends_with(basename), "{tool}");
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod autostart_tests {
    use super::autostart_impl::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    const TEST_KEY: &str = "Software\\Vigil\\TestAutostart";
    const TEST_VALUE: &str = "VigilTest";

    fn cleanup() {
        delete_test_key(TEST_KEY);
    }

    #[test]
    fn autostart_active_cree_la_cle() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cleanup();

        let exe = PathBuf::from("C:\\test\\vigil.exe");
        let result = set_autostart_at(TEST_KEY, TEST_VALUE, true, &exe);
        assert!(result.is_ok(), "set_autostart_at doit réussir: {:?}", result);
        assert!(
            is_autostart_enabled_at(TEST_KEY, TEST_VALUE),
            "La valeur doit exister après activation"
        );

        cleanup();
    }

    #[test]
    fn autostart_desactive_supprime_la_cle() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cleanup();

        let exe = PathBuf::from("C:\\test\\vigil.exe");
        set_autostart_at(TEST_KEY, TEST_VALUE, true, &exe).unwrap();
        assert!(is_autostart_enabled_at(TEST_KEY, TEST_VALUE));

        let result = set_autostart_at(TEST_KEY, TEST_VALUE, false, &exe);
        assert!(result.is_ok(), "disable doit réussir: {:?}", result);
        assert!(
            !is_autostart_enabled_at(TEST_KEY, TEST_VALUE),
            "La valeur ne doit plus exister après désactivation"
        );

        cleanup();
    }

    #[test]
    fn autostart_double_enable_idempotent() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cleanup();

        let exe = PathBuf::from("C:\\test\\vigil.exe");
        set_autostart_at(TEST_KEY, TEST_VALUE, true, &exe).unwrap();
        let result = set_autostart_at(TEST_KEY, TEST_VALUE, true, &exe);
        assert!(result.is_ok(), "double enable doit réussir: {:?}", result);
        assert!(
            is_autostart_enabled_at(TEST_KEY, TEST_VALUE),
            "La valeur doit toujours exister après double activation"
        );

        cleanup();
    }

    #[test]
    fn autostart_disable_absent_ne_plante_pas() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cleanup();

        let exe = PathBuf::from("C:\\test\\vigil.exe");
        let result = set_autostart_at(TEST_KEY, TEST_VALUE, false, &exe);
        assert!(
            result.is_ok(),
            "disable sans valeur existante ne doit pas échouer: {:?}",
            result
        );

        cleanup();
    }
}

// ── Tests — Kimi CLI (Story 9.2) ──────────────────────────────────────────────
//
// Cœurs PURS paramétrés par le TEXTE du document : aucun test ne dépend de `~/.kimi-code/`.
#[cfg(test)]
mod kimi_tests {
    use super::*;

    /// Forme réellement produite par `get_vigil_kimi_exe` : chemin 8.3, backslashes NATIFS,
    /// MAJUSCULES. Ne pas « nettoyer » en un joli chemin long : le nom court est le cas nominal,
    /// et c'est lui qui ne contient pas la chaîne « kimi ».
    const KIMI_EXE: &str = r"C:\Users\DEVUSR~1\AppData\Local\vigil\VIGIL-~5.EXE";

    /// Fixture d'un `~/.kimi-code/config.toml` utilisateur (providers locaux, commentaires) :
    /// c'est ce fichier-là qu'il ne faut pas abîmer.
    const CONFIG_REEL: &str = r#"# Ma config Kimi — ne pas toucher
default_permission_mode = "manual"
default_model = "lmstudio/qwen/qwen3-30b-a3b-2507"

[thinking]
enabled = false

[providers.lmstudio]
type = "openai"
api_key = "none"
base_url = "http://127.0.0.1:1234/v1"

[models."lmstudio/qwen/qwen3-30b-a3b-2507"]
provider = "lmstudio"
model = "qwen/qwen3-30b-a3b-2507"
max_context_size = 262144
max_output_size = 16384
capabilities = [ "tool_use" ]  # commentaire en fin de ligne
display_name = "Qwen3 30B A3B 2507"

[models."lmstudio/openai/gpt-oss-20b"]
provider = "lmstudio"
model = "openai/gpt-oss-20b"
max_context_size = 131072
max_output_size = 32768
capabilities = [ "thinking", "tool_use" ]
display_name = "GPT OSS 20B"
"#;

    fn parse(text: &str) -> toml_edit::DocumentMut {
        text.parse().expect("fixture TOML valide")
    }

    fn blocs(doc: &toml_edit::DocumentMut) -> Vec<&toml_edit::Table> {
        doc.get("hooks")
            .and_then(|i| i.as_array_of_tables())
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    // ── Fusion ────────────────────────────────────────────────────────────────

    #[test]
    fn kimi_fusion_pose_les_douze_blocs() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        let tables = blocs(&after);
        assert_eq!(tables.len(), KIMI_EVENTS.len(), "un bloc par event, pas un de plus");
        let events: Vec<&str> = tables.iter().filter_map(|t| t["event"].as_str()).collect();
        assert_eq!(events, KIMI_EVENTS.to_vec());
        assert!(are_kimi_hooks_installed(&after, KIMI_EXE));
    }

    /// ⚠️ LE test de cette story. `HookDefSchema` est `.strict()` : une clé en trop ne fait pas
    /// ignorer le hook, elle fait échouer `validateConfig` sur la config ENTIÈRE — Kimi ne démarre
    /// plus. On verrouille donc l'ENSEMBLE EXACT des clés, pas seulement leur présence.
    #[test]
    fn kimi_cles_exactes_de_chaque_bloc() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        for table in blocs(&after) {
            let mut keys: Vec<&str> = table.iter().map(|(k, _)| k).collect();
            keys.sort_unstable();
            let event = table["event"].as_str().unwrap();
            let attendu: Vec<&str> = if event == KIMI_BLOCKING_EVENT {
                vec!["command", "event", "timeout"]
            } else {
                vec!["command", "event"]
            };
            assert_eq!(keys, attendu, "bloc {event} : clés hors contrat → CONFIG_INVALID");
        }
    }

    /// ⚠️ `matcher` est évalué comme une REGEX par Kimi (`matches(hook.matcher ?? "", …)`) : `"*"`
    /// n'en est pas une valide. Le réflexe copier-coller depuis le chemin Claude/Antigravity
    /// (qui posent un `matcher`) est explicitement interdit ici.
    #[test]
    fn kimi_aucun_bloc_ne_porte_de_matcher() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        for table in blocs(&after) {
            assert!(!table.contains_key("matcher"), "matcher interdit (regex chez Kimi)");
        }
    }

    /// ⚠️ `timeout: number().int().min(1).max(600)`. La valeur 1800 recopiée du chemin
    /// Claude/Codex/Grok est HORS PLAGE et briquerait la config entière.
    #[test]
    fn kimi_timeout_600_sur_pretooluse_seulement() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        for table in blocs(&after) {
            let event = table["event"].as_str().unwrap();
            if event == KIMI_BLOCKING_EVENT {
                assert_eq!(
                    table["timeout"].as_integer(),
                    Some(600),
                    "PreToolUse doit porter timeout = 600 (plafond DUR d'outil)",
                );
            } else {
                assert!(
                    !table.contains_key("timeout"),
                    "{event} est fire-and-forget : aucun timeout",
                );
            }
        }
    }

    /// Le `timeout` écrit ne doit JAMAIS être un littéral recopié : il vient de la même fonction que
    /// l'écrêtage de la fenêtre d'approbation. Sans ce lien, l'un des deux pourrait dériver et le
    /// dépassement chez Kimi est FAIL-OPEN (l'appel passe sans approbation, aucune alarme).
    #[test]
    fn kimi_timeout_installe_egale_le_plafond_de_l_outil() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        let bloc = blocs(&after)
            .into_iter()
            .find(|t| t["event"].as_str() == Some(KIMI_BLOCKING_EVENT))
            .expect("PreToolUse présent");
        assert_eq!(
            bloc["timeout"].as_integer(),
            Some(hook_timeout_ceiling_secs(&SourceTool::Kimi) as i64),
        );
    }

    // ── Préservation ──────────────────────────────────────────────────────────

    /// Providers, alias de modèles, réglages racine, commentaires (dont un commentaire de FIN DE
    /// LIGNE) et ordre : tout traverse l'install intact. Vérifié en retirant du document produit
    /// exactement ce que nous y avons ajouté — le reste doit être bit-à-bit l'original.
    #[test]
    fn kimi_install_preserve_tout_le_reste_bit_a_bit() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        let sans_nous = compute_kimi_without_hooks(after, KIMI_EXE);
        assert_eq!(
            sans_nous.to_string(),
            CONFIG_REEL,
            "aller-retour install → uninstall doit rendre le document D'ORIGINE",
        );
    }

    #[test]
    fn kimi_hook_utilisateur_preserve_a_l_install_et_a_l_uninstall() {
        let avec_tiers = format!(
            "{CONFIG_REEL}\n# hook maison\n[[hooks]]\nevent = \"Stop\"\ncommand = 'C:\\outils\\mon-hook.exe'\ntimeout = 30\n"
        );
        let after = compute_kimi_with_hooks(parse(&avec_tiers), KIMI_EXE);
        assert_eq!(blocs(&after).len(), KIMI_EVENTS.len() + 1, "le bloc tiers survit à l'install");

        let cleaned = compute_kimi_without_hooks(after, KIMI_EXE);
        assert_eq!(
            cleaned.to_string(),
            avec_tiers,
            "le bloc tiers ET son commentaire survivent à l'uninstall, à l'octet près",
        );
    }

    // ── Idempotence & auto-réparation ─────────────────────────────────────────

    #[test]
    fn kimi_install_idempotente_aucun_doublon() {
        let une = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        let deux = compute_kimi_with_hooks(une.clone(), KIMI_EXE);
        assert_eq!(deux.to_string(), une.to_string(), "2 installs ⇒ document identique");
        assert_eq!(blocs(&deux).len(), KIMI_EVENTS.len(), "aucun doublon");
    }

    /// Auto-réparation RÉELLE (≠ Claude/Codex, dont `already_present` ne répare jamais — défaut
    /// relevé en revue 8.8) : un bloc laissé par une version antérieure avec un `timeout` périmé
    /// est CORRIGÉ, pas conservé tel quel. Et la non-conformité doit être DÉTECTÉE avant, sinon
    /// l'auto-repair ne serait jamais sollicité.
    #[test]
    fn kimi_bloc_au_timeout_perime_est_detecte_puis_repare() {
        let perime = format!(
            "{CONFIG_REEL}\n[[hooks]]\nevent = \"PreToolUse\"\ncommand = '{KIMI_EXE}'\ntimeout = 1800\n"
        );
        let doc = parse(&perime);
        assert!(
            !are_kimi_hooks_installed(&doc, KIMI_EXE),
            "un timeout hors contrat doit rendre la config NON conforme (sinon jamais réparée)",
        );
        let after = compute_kimi_with_hooks(doc, KIMI_EXE);
        assert_eq!(blocs(&after).len(), KIMI_EVENTS.len(), "le bloc périmé est retiré, pas doublé");
        assert!(are_kimi_hooks_installed(&after, KIMI_EXE));
    }

    #[test]
    fn kimi_bloc_au_chemin_obsolete_est_repare_sans_doublon() {
        // Vigil réinstallé ailleurs : chemin PÉRIMÉ, en nom long comme en nom court 8.3.
        let obsolete = format!(
            "{CONFIG_REEL}\n[[hooks]]\nevent = \"Stop\"\ncommand = 'D:\\ancien\\vigil-kimi.exe'\n\
             \n[[hooks]]\nevent = \"Notification\"\ncommand = 'D:\\ancien\\VIGIL-~2.EXE'\n"
        );
        let doc = parse(&obsolete);
        assert!(!are_kimi_hooks_installed(&doc, KIMI_EXE));
        let after = compute_kimi_with_hooks(doc, KIMI_EXE);
        assert_eq!(
            blocs(&after).len(),
            KIMI_EVENTS.len(),
            "les 2 blocs obsolètes sont retirés — sinon doublon à CHAQUE réinstallation",
        );
        assert!(are_kimi_hooks_installed(&after, KIMI_EXE));
    }

    // ── Retrait ───────────────────────────────────────────────────────────────

    #[test]
    fn kimi_retrait_insensible_a_la_casse() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        // La casse du chemin passé à l'uninstall diffère de celle écrite dans le fichier.
        let cleaned = compute_kimi_without_hooks(after, &KIMI_EXE.to_ascii_lowercase());
        assert_eq!(cleaned.to_string(), CONFIG_REEL);
    }

    /// ⚠️ LE cas qui justifie que l'identité soit `command` et non le marqueur. Kimi réécrit
    /// `config.toml` via `smol-toml`, qui EFFACE tous les commentaires — les nôtres compris — tout
    /// en conservant les blocs `[[hooks]]` (données). Un critère « marqueur » laisserait ici des
    /// blocs INDÉLOGEABLES à vie.
    #[test]
    fn kimi_bloc_au_marqueur_efface_est_retire_quand_meme() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        // Simule la réécriture par Kimi : tous les commentaires disparaissent, blocs intacts.
        let sans_commentaires: String = after
            .to_string()
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!sans_commentaires.contains(KIMI_MARKER), "le marqueur a bien disparu");
        let doc = parse(&sans_commentaires);
        assert_eq!(blocs(&doc).len(), KIMI_EVENTS.len(), "les blocs, eux, ont survécu");

        let cleaned = compute_kimi_without_hooks(doc, KIMI_EXE);
        assert!(blocs(&cleaned).is_empty(), "les blocs doivent être retirés malgré le marqueur effacé");
        assert!(cleaned.get("hooks").is_none());
    }

    #[test]
    fn kimi_cle_hooks_retiree_si_plus_aucun_bloc() {
        let after = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        let cleaned = compute_kimi_without_hooks(after, KIMI_EXE);
        assert!(cleaned.get("hooks").is_none(), "pas de `hooks = []` résiduel");
        assert!(!cleaned.to_string().contains("hooks"));
    }

    #[test]
    fn kimi_uninstall_sur_document_sans_hooks_ne_change_rien() {
        let cleaned = compute_kimi_without_hooks(parse(CONFIG_REEL), KIMI_EXE);
        assert_eq!(cleaned.to_string(), CONFIG_REEL);
    }

    // ── Conformité ────────────────────────────────────────────────────────────

    #[test]
    fn kimi_are_installed_false_si_un_seul_event_manque() {
        let mut doc = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        doc.get_mut("hooks")
            .and_then(|i| i.as_array_of_tables_mut())
            .unwrap()
            .remove(0);
        assert!(!are_kimi_hooks_installed(&doc, KIMI_EXE));
    }

    /// Forme périmée plausible : chemin LONG de l'ancien emplacement d'install, avant le miroir de
    /// la 10.3 — exactement ce qu'une config existante porte au moment de la migration.
    ///
    /// ⚠️ Volontairement en forme longue, et pas en 8.3. Le matcher tolérant reconnaît un bloc
    /// périmé par son **basename** ; or `KIMI_EXE` est nominalement en 8.3 (`VIGIL-~5.EXE`), un nom
    /// qui n'a aucun rapport lexical avec `vigil-kimi.exe`. Un périmé lui aussi en 8.3 ne serait
    /// donc **pas identifié comme nôtre**, et ces tests passeraient pour la mauvaise raison (bloc
    /// invisible, jamais compté) au lieu de la bonne (bloc compté, jugé non canonique). Limite
    /// résiduelle assumée du matcher, sans portée depuis la 10.3 : le chemin nominal est désormais
    /// le miroir, sans espace, donc sans 8.3 — les basenames y sont de nouveau lisibles.
    const KIMI_PERIME: &str = r"C:\Users\Dev\AppData\Local\vigil\vigil-kimi.exe";

    /// ⚠️ Récupéré de la Story 10.2 (T2b) à son abandon, 2026-07-29.
    ///
    /// Le RETRAIT était déjà tolérant (`kimi_table_is_vigil`), le COMPTAGE ne l'était pas : les deux
    /// notions ne coïncidaient que tant que le canonique EST le chemin nu. Un bloc Vigil au chemin
    /// périmé **cohabitant** avec le canonique n'entrait donc pas dans le décompte → `count == 1`
    /// satisfait → conformité verte → auto-repair jamais sollicité, pendant que l'orphelin tire.
    #[test]
    fn kimi_orphelin_perime_cohabitant_rend_la_conformite_fausse() {
        let doc = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE);
        assert!(are_kimi_hooks_installed(&doc, KIMI_EXE), "témoin du banc");

        let pollue =
            format!("{doc}\n[[hooks]]\nevent = \"SessionStart\"\ncommand = '{KIMI_PERIME}'\n");
        assert!(!are_kimi_hooks_installed(&parse(&pollue), KIMI_EXE));
    }

    /// Garde ANTI-RÉGRESSION du passage au matcher tolérant : le filtre reconnaît désormais notre
    /// binaire par basename, si bien qu'un bloc au chemin périmé **seul** est bien identifié comme
    /// nôtre. Il ne doit surtout pas être jugé CANONIQUE pour autant — d'où la comparaison
    /// explicite de la `command`, qui n'était jusque-là portée qu'implicitement par le filtre.
    #[test]
    fn kimi_chemin_perime_seul_rend_la_conformite_fausse() {
        let doc = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_PERIME);
        assert!(are_kimi_hooks_installed(&doc, KIMI_PERIME), "témoin du banc");
        assert!(!are_kimi_hooks_installed(&doc, KIMI_EXE));
    }

    #[test]
    fn kimi_are_installed_false_sur_document_vide() {
        assert!(!are_kimi_hooks_installed(&toml_edit::DocumentMut::new(), KIMI_EXE));
        assert!(!are_kimi_hooks_installed(&parse(CONFIG_REEL), KIMI_EXE));
    }

    /// Une clé en trop dans NOTRE bloc doit rendre la config non conforme : c'est précisément
    /// l'état qui empêche Kimi de démarrer, il ne doit jamais être considéré comme « installé ».
    #[test]
    fn kimi_are_installed_false_si_cle_en_trop() {
        let pollue = format!(
            "{CONFIG_REEL}\n[[hooks]]\nevent = \"SessionStart\"\ncommand = '{KIMI_EXE}'\nmatcher = \"*\"\n"
        );
        assert!(!are_kimi_hooks_installed(&parse(&pollue), KIMI_EXE));
    }

    // ── Identité de commande ──────────────────────────────────────────────────

    #[test]
    fn kimi_identite_de_commande_les_trois_formes_et_leurs_limites() {
        let bas = KIMI_EXE.to_ascii_lowercase();
        // 1. commande courante (casse indifférente)
        assert!(kimi_command_designates_vigil(KIMI_EXE, &bas));
        assert!(kimi_command_designates_vigil(&KIMI_EXE.to_ascii_uppercase(), &bas));
        // 2. chemin périmé en nom long
        assert!(kimi_command_designates_vigil(r"D:\vieux\vigil-kimi.exe", &bas));
        assert!(kimi_command_designates_vigil("D:/vieux/VIGIL-KIMI.EXE", &bas));
        // 3. chemin périmé en nom court 8.3 (ne contient PAS « kimi »)
        assert!(kimi_command_designates_vigil(r"D:\vieux\VIGIL-~1.EXE", &bas));
        assert!(kimi_command_designates_vigil(r"D:\vieux\vigil-~12.exe", &bas));
        // ⚠️ Le motif reste ÉTROIT : aucun programme tiers de l'utilisateur ne doit être happé.
        for tiers in [
            r"C:\outils\vigilant.exe",
            r"C:\outils\mon-hook.exe",
            r"C:\outils\vigil-kimi.bat",
            r"C:\outils\vigil-~.exe",
            r"C:\outils\vigil-~a.exe",
            "node vigil-kimi.exe",
        ] {
            assert!(!kimi_command_designates_vigil(tiers, &bas), "{tiers} ne doit PAS être happé");
        }
    }

    /// Story 10.1 (AC8/AC9) — le matcher de Kimi est désormais le helper PARTAGÉ par les 5 outils,
    /// paramétré par le basename. Aucun changement d'observable sur les 3 formes historiques (test
    /// ci-dessus), et il gagne GRATUITEMENT la tolérance aux guillemets englobants dont la Story
    /// 10.2 aura besoin pour migrer sans doublon.
    #[test]
    fn kimi_matcher_generalise_accepte_desormais_les_guillemets() {
        let bas = KIMI_EXE.to_ascii_lowercase();
        assert!(kimi_command_designates_vigil(&format!("\"{KIMI_EXE}\""), &bas));
        assert!(kimi_command_designates_vigil(r#""D:\vieux\vigil-kimi.exe""#, &bas));
        assert!(kimi_command_designates_vigil(r#""D:\vieux\VIGIL-~1.EXE""#, &bas));
        // Le chemin à ESPACE reste délogeable (lecture de la commande ENTIÈRE, pas du 1er token).
        assert!(kimi_command_designates_vigil(r"C:\Users\John Doe\vigil-kimi.exe", &bas));
        // L'étroitesse est conservée.
        assert!(!kimi_command_designates_vigil(r#""C:\outils\vigilant.exe""#, &bas));
    }

    // ── BOM & validité TOML ───────────────────────────────────────────────────

    #[test]
    fn kimi_bom_tolere_en_lecture_jamais_reecrit() {
        let dir = std::env::temp_dir().join(format!("vigil-kimi-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, format!("\u{FEFF}{CONFIG_REEL}")).unwrap();

        let doc = read_toml_document(&path).expect("un BOM ne doit pas casser le parsing");
        assert_eq!(doc.to_string(), CONFIG_REEL);

        install_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        let octets = std::fs::read(&path).unwrap();
        assert!(!octets.starts_with(&[0xEF, 0xBB, 0xBF]), "le BOM ne doit jamais être réécrit");

        uninstall_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG_REEL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Le document produit doit se REPARSER, et chaque table `hooks` ne porter que des clés
    /// autorisées par `HookDefSchema` — la garantie que Kimi chargera sa config.
    #[test]
    fn kimi_document_produit_est_du_toml_valide_et_conforme_au_schema() {
        let rendu = compute_kimi_with_hooks(parse(CONFIG_REEL), KIMI_EXE).to_string();
        let reparse: toml_edit::DocumentMut = rendu.parse().expect("le TOML produit doit se reparser");
        const AUTORISEES: &[&str] = &["event", "matcher", "command", "timeout"];
        for table in blocs(&reparse) {
            for (key, _) in table.iter() {
                assert!(AUTORISEES.contains(&key), "clé `{key}` hors HookDefSchema");
            }
            // Le chemin Windows doit ressortir INTACT après aller-retour d'écriture/lecture :
            // c'est ce que garantit le passage par l'API `toml_edit` (jamais un `format!`).
            assert_eq!(table["command"].as_str(), Some(KIMI_EXE));
        }
    }

    /// ⚠️ Le `config.toml` réel de référence est en **CRLF**, et `toml_edit` NORMALISE les CRLF en
    /// LF au rendu — y compris sur le contenu qu'il n'a pas touché. Sans `render_toml`, la première
    /// install réécrirait TOUT le fichier (36 octets de diff sur le fichier réel) et l'aller-retour
    /// install → uninstall ne rendrait jamais l'original. Défaut SILENCIEUX : le TOML produit reste
    /// parfaitement valide, seule la comparaison octet à octet le révèle.
    #[test]
    fn kimi_fins_de_ligne_crlf_preservees_a_l_octet_pres() {
        let dir = std::env::temp_dir().join(format!("vigil-kimi-crlf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let crlf_source = CONFIG_REEL.replace('\n', "\r\n");
        std::fs::write(&path, &crlf_source).unwrap();

        install_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        let apres_install = std::fs::read_to_string(&path).unwrap();
        assert!(apres_install.contains("\r\n"), "le fichier reste en CRLF");
        assert!(
            !apres_install.replace("\r\n", "").contains('\n'),
            "aucun LF isolé : ni dans l'existant, ni dans les blocs que nous ajoutons",
        );
        assert!(are_kimi_hooks_installed(&read_toml_document(&path).unwrap(), KIMI_EXE));

        uninstall_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            crlf_source.as_bytes(),
            "aller-retour sur un fichier CRLF ⇒ document d'origine, OCTET À OCTET",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Miroir du précédent : un fichier en LF ne doit JAMAIS gagner de CRLF.
    #[test]
    fn kimi_fins_de_ligne_lf_jamais_converties() {
        let dir = std::env::temp_dir().join(format!("vigil-kimi-lf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, CONFIG_REEL).unwrap();

        install_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains('\r'));

        uninstall_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), CONFIG_REEL.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kimi_install_sur_fichier_absent_cree_une_config_valide() {
        let dir = std::env::temp_dir().join(format!("vigil-kimi-neuf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        install_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        let doc = read_toml_document(&path).unwrap();
        assert!(are_kimi_hooks_installed(&doc, KIMI_EXE));

        uninstall_kimi_hooks_impl(&path, KIMI_EXE).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "document vidé de nos blocs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Forme EN LIGNE écrite à la main (`hooks = [{ … }]`) : du TOML valide qui satisfait le schéma
    /// de Kimi. Sans normalisation, `as_array_of_tables_mut` rendrait `None` et l'install ÉCRASERAIT
    /// les hooks de l'utilisateur. On change la FORME, jamais la DONNÉE.
    #[test]
    fn kimi_hooks_en_ligne_normalises_sans_perte() {
        let en_ligne = "hooks = [{ event = \"Stop\", command = 'C:\\outils\\a.exe' }]\n";
        let after = compute_kimi_with_hooks(parse(en_ligne), KIMI_EXE);
        let tables = blocs(&after);
        assert_eq!(tables.len(), KIMI_EVENTS.len() + 1);
        assert_eq!(tables[0]["event"].as_str(), Some("Stop"));
        assert_eq!(tables[0]["command"].as_str(), Some(r"C:\outils\a.exe"));
        assert!(are_kimi_hooks_installed(&after, KIMI_EXE));
    }

    // ── Socle d'install ───────────────────────────────────────────────────────

    /// ⚠️ `vigil-kimi` interroge la clé LITTÉRALE `"kimi"` (`check_tool_enabled`). Un nom de champ
    /// divergent ne casserait rien de visible — `check_tool_enabled` est fail-open sur clé absente —
    /// mais la case « Disabled » de la page Installation n'aurait plus AUCUN effet sur les hooks
    /// Kimi. C'est ce test qui l'empêche.
    #[test]
    fn enabled_tools_serialise_la_cle_kimi() {
        let json = serde_json::to_value(EnabledTools { kimi: true, ..Default::default() }).unwrap();
        assert_eq!(json["kimi"], serde_json::json!(true));
        // Et la relecture d'un fichier hérité (sans la clé) ne doit pas échouer.
        let herite: EnabledTools =
            serde_json::from_str(r#"{"claude_code":true}"#).expect("clé kimi absente tolérée");
        assert!(!herite.kimi);
    }

    #[test]
    fn kimi_est_un_outil_de_premier_rang_du_socle() {
        assert_eq!(tool_label("kimi"), "Kimi CLI");

        let enabled = EnabledTools { kimi: true, ..Default::default() };
        assert!(check_installation_required_with(&enabled, true, true, true, true, false));
        assert!(!check_installation_required_with(&enabled, true, true, true, true, true));
        assert_eq!(
            tools_needing_repair(&enabled, true, true, true, true, false),
            vec!["kimi"],
        );
        assert!(tools_needing_repair(&enabled, true, true, true, true, true).is_empty());
    }
}

