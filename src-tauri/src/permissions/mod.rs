use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::agent::state::SourceTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolPermissionKey {
    Bash,
    EditWrite,
    Read,
    WebFetch,
    ReadOnly,
    // Story 6.35 — catégories neuves (défaut Manual comme les autres). `Mcp` = invocation d'un
    // outil MCP (Claude `mcp__*`, Grok `use_tool`) ; `Subagent` = délégation à un sous-agent
    // (Claude `Task`, Grok `spawn_subagent`/`kill_command_or_subagent`).
    Mcp,
    Subagent,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ToolPermissionAction {
    #[default]
    Manual,
    Approve,
    Deny,
}

pub type AgentPermissionOverrides = HashMap<ToolPermissionKey, ToolPermissionAction>;

/// Story 6.35 — catégorisation **agnostique de l'outil**. Reçoit le nom NATIF de l'outil
/// (`name`, PascalCase Claude/Codex ou snake_case Grok) + son `source_tool`, et dérive la
/// catégorie au point d'usage (Option A : zéro changement de protocole/bridge). Ordre :
///   1. **table explicite** (comparaison insensible à la casse) couvrant Claude/Codex + Grok ;
///   2. préfixe `mcp__` → `Mcp` (outils MCP Claude) ;
///   3. **repli heuristique fail-closed** (`heuristic_key`) ; défaut final `Other`.
///
/// `source_tool` fait partie du contrat (peut orienter une désambiguïsation future) mais la
/// table est aujourd'hui **majoritairement agnostique** : un nom connu mappe pareil quel que
/// soit l'outil (ex. `grep` → `ReadOnly` pour Claude comme pour Grok).
pub fn tool_name_to_key(source_tool: SourceTool, name: &str) -> ToolPermissionKey {
    let _ = source_tool; // réservé pour une désambiguïsation par outil (contrat AC1).
    let n = name.to_ascii_lowercase();
    if let Some(key) = explicit_key(&n) {
        return key;
    }
    // Préfixe MCP déterministe (Claude `mcp__<serveur>__<outil>`), AVANT le repli heuristique.
    if n.starts_with("mcp__") {
        return ToolPermissionKey::Mcp;
    }
    heuristic_key(&n)
}

/// Table explicite normative (cf. story 6.35). `n` est déjà en minuscules. Couvre les noms
/// connus des 4 outils. Certaines entrées existent UNIQUEMENT pour court-circuiter l'heuristique
/// (ex. `image_edit` contiendrait `edit` → EditWrite ; `scheduler_create` contiendrait `create`)
/// et forcer `Other`.
fn explicit_key(n: &str) -> Option<ToolPermissionKey> {
    use ToolPermissionKey::*;
    let key = match n {
        // ── Claude / Codex (PascalCase-en-minuscules) — comportement préservé ──
        "bash" => Bash,
        "edit" | "write" | "multiedit" | "notebookedit" => EditWrite,
        "read" => Read,
        "webfetch" | "websearch" => WebFetch,
        "glob" | "grep" | "ls" => ReadOnly,
        "task" => Subagent,
        // Story 6.35 (review) — outils Claude hors table initiale : entrées explicites pour éviter
        // que le repli heuristique ne les classe à tort (régression pour un store configuré).
        //   • `TodoWrite`/`ExitPlanMode`/`BashOutput` → ReadOnly : aucun effet FS (todo-list interne,
        //     sortie de plan mode, lecture de sortie d'un shell d'arrière-plan). Cohérence avec Grok
        //     `todo_write`/`exit_plan_mode` déjà en ReadOnly. Sans ça : TodoWrite→EditWrite (`write`),
        //     BashOutput→Other, ExitPlanMode→Other.
        //   • `KillShell` → Bash : contrôle du cycle de vie d'un shell d'arrière-plan.
        //   • `SlashCommand` → Other : méta-outil au blast-radius imprévisible, reste Manual (sans ça
        //     l'heuristique le classait Bash via `command`).
        "todowrite" | "exitplanmode" | "bashoutput" => ReadOnly,
        "killshell" => Bash,
        "slashcommand" => Other,

        // ── Grok (noms natifs snake_case) ──
        "run_terminal_command" => Bash,
        "read_file" => Read,
        "search_replace" => EditWrite,
        "list_dir" | "todo_write" | "get_command_or_subagent_output" | "monitor"
        | "enter_plan_mode" | "exit_plan_mode" | "update_goal" | "scheduler_list"
        | "search_tool" => ReadOnly,
        "web_search" | "web_fetch" | "open_page" | "open_page_with_find"
        | "x_user_search" | "x_semantic_search" | "x_keyword_search" | "x_thread_fetch" => WebFetch,
        "spawn_subagent" | "kill_command_or_subagent" => Subagent,
        "use_tool" => Mcp,
        // `Other` explicites : forcent la catégorie contre l'heuristique (create/edit/delete).
        "scheduler_create" | "scheduler_delete" | "image_gen" | "image_edit"
        | "image_to_video" | "reference_to_video" => Other,

        _ => return None,
    };
    Some(key)
}

/// Repli heuristique **fail-closed** pour un nom inconnu de la table (`n` en minuscules).
/// Tests de sous-chaîne, du plus RESTRICTIF au plus permissif : en cas de doute, on gate
/// (Subagent/Mcp/Bash/EditWrite avant les lecture-seule). Filet final `Other`. Sans effet de
/// bord tant que les défauts sont Manual : une mauvaise catégorisation ne « laisse passer »
/// que si l'utilisateur a explicitement mis la catégorie en Approve.
fn heuristic_key(n: &str) -> ToolPermissionKey {
    use ToolPermissionKey::*;
    let has = |kw: &str| n.contains(kw);
    if has("spawn") || has("subagent") {
        Subagent
    } else if has("mcp") {
        Mcp
    // Story 6.35 (review) — durcissement : opérations DESTRUCTRICES de données gatées AVANT le
    // bucket Bash. Sans ça, `prune`/`truncate` tombent dans Bash (sous-chaîne accidentelle `run`)
    // et un nom sans mot-clé write (ex. `read_and_purge`) tomberait dans Read. Fail-closed →
    // EditWrite (gated) plutôt qu'une catégorie moins pertinente.
    } else if has("purge") || has("wipe") || has("prune") || has("truncate") || has("drop")
        || has("reset")
    {
        EditWrite
    } else if has("exec") || has("shell") || has("terminal") || has("command") || has("run") {
        Bash
    } else if has("write") || has("edit") || has("replace") || has("create") || has("delete")
        || has("modify")
    {
        EditWrite
    } else if has("fetch") || has("http") || has("url") || has("web") {
        WebFetch
    } else if has("read") || has("open") || has("view") || has("cat") {
        Read
    } else if has("search") || has("grep") || has("list") || has("glob") || has("find")
        || has("ls")
    {
        ReadOnly
    } else {
        Other
    }
}

pub fn resolve_permission(
    source_tool: SourceTool,
    tool_name: &str,
    overrides: &AgentPermissionOverrides,
    global: &HashMap<ToolPermissionKey, ToolPermissionAction>,
) -> ToolPermissionAction {
    let key = tool_name_to_key(source_tool, tool_name);
    overrides
        .get(&key)
        .copied()
        .unwrap_or_else(|| global.get(&key).copied().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::SourceTool;

    // Raccourcis pour la lisibilité des tests.
    fn claude(name: &str) -> ToolPermissionKey {
        tool_name_to_key(SourceTool::ClaudeCode, name)
    }
    fn grok(name: &str) -> ToolPermissionKey {
        tool_name_to_key(SourceTool::Grok, name)
    }

    // ── AC2 : non-régression Claude/Codex — chaque nom mappe comme AVANT la story ──
    #[test]
    fn claude_codex_categories_inchangees() {
        use ToolPermissionKey::*;
        assert_eq!(claude("Bash"), Bash);
        assert_eq!(claude("Edit"), EditWrite);
        assert_eq!(claude("Write"), EditWrite);
        assert_eq!(claude("MultiEdit"), EditWrite);
        assert_eq!(claude("Read"), Read);
        assert_eq!(claude("WebFetch"), WebFetch);
        assert_eq!(claude("WebSearch"), WebFetch);
        assert_eq!(claude("Glob"), ReadOnly);
        assert_eq!(claude("Grep"), ReadOnly);
        assert_eq!(claude("LS"), ReadOnly);
        // Codex partage le vocabulaire PascalCase → même résolution.
        assert_eq!(tool_name_to_key(SourceTool::Codex, "Bash"), Bash);
        assert_eq!(tool_name_to_key(SourceTool::Codex, "Read"), Read);
    }

    // ── AC1/AC4 : déplacements Task→Subagent, mcp__*→Mcp, NotebookEdit→EditWrite ──
    #[test]
    fn task_devient_subagent() {
        assert_eq!(claude("Task"), ToolPermissionKey::Subagent);
    }

    #[test]
    fn mcp_prefixe_devient_mcp() {
        assert_eq!(claude("mcp__github__create_issue"), ToolPermissionKey::Mcp);
        assert_eq!(claude("mcp__srv__x"), ToolPermissionKey::Mcp);
    }

    #[test]
    fn notebookedit_est_editwrite() {
        assert_eq!(claude("NotebookEdit"), ToolPermissionKey::EditWrite);
    }

    // ── AC3 : cartographie Grok exhaustive ──
    #[test]
    fn grok_cartographie_exhaustive() {
        use ToolPermissionKey::*;
        assert_eq!(grok("run_terminal_command"), Bash);
        assert_eq!(grok("read_file"), Read);
        assert_eq!(grok("write"), EditWrite);
        assert_eq!(grok("search_replace"), EditWrite);
        assert_eq!(grok("list_dir"), ReadOnly);
        assert_eq!(grok("grep"), ReadOnly);
        assert_eq!(grok("todo_write"), ReadOnly);
        assert_eq!(grok("get_command_or_subagent_output"), ReadOnly);
        assert_eq!(grok("monitor"), ReadOnly);
        assert_eq!(grok("enter_plan_mode"), ReadOnly);
        assert_eq!(grok("exit_plan_mode"), ReadOnly);
        assert_eq!(grok("update_goal"), ReadOnly);
        assert_eq!(grok("scheduler_list"), ReadOnly);
        assert_eq!(grok("search_tool"), ReadOnly);
        assert_eq!(grok("web_search"), WebFetch);
        assert_eq!(grok("web_fetch"), WebFetch);
        assert_eq!(grok("open_page"), WebFetch);
        assert_eq!(grok("open_page_with_find"), WebFetch);
        assert_eq!(grok("x_user_search"), WebFetch);
        assert_eq!(grok("x_semantic_search"), WebFetch);
        assert_eq!(grok("x_keyword_search"), WebFetch);
        assert_eq!(grok("x_thread_fetch"), WebFetch);
        assert_eq!(grok("spawn_subagent"), Subagent);
        assert_eq!(grok("kill_command_or_subagent"), Subagent);
        assert_eq!(grok("use_tool"), Mcp);
        assert_eq!(grok("scheduler_create"), Other);
        assert_eq!(grok("scheduler_delete"), Other);
        assert_eq!(grok("image_gen"), Other);
        assert_eq!(grok("image_edit"), Other);
        assert_eq!(grok("image_to_video"), Other);
        assert_eq!(grok("reference_to_video"), Other);
    }

    // ── AC1/AC3 : repli heuristique fail-closed pour un nom inconnu ──
    #[test]
    fn heuristique_fail_closed() {
        use ToolPermissionKey::*;
        // spawn/subagent d'abord (le plus restrictif).
        assert_eq!(grok("spawn_worker_thing"), Subagent);
        // mcp.
        assert_eq!(grok("invoke_mcp_endpoint"), Mcp);
        // shell/command/run.
        assert_eq!(grok("execute_shell_thing"), Bash);
        assert_eq!(grok("some_future_run_x"), Bash);
        // write/edit/create/delete → EditWrite (gated).
        assert_eq!(grok("create_widget"), EditWrite);
        assert_eq!(grok("delete_widget"), EditWrite);
        // fetch/web.
        assert_eq!(grok("fetch_remote_thing"), WebFetch);
        // read/open/view.
        assert_eq!(grok("view_something"), Read);
        // search/list.
        assert_eq!(grok("list_all_things"), ReadOnly);
        // Ambigu read+write → penche vers la catégorie la PLUS restrictive (EditWrite).
        assert_eq!(grok("read_and_write_x"), EditWrite);
    }

    #[test]
    fn nom_totalement_inconnu_retombe_sur_other() {
        assert_eq!(grok("zzz_qqq_unknowable"), ToolPermissionKey::Other);
        assert_eq!(claude("SomeUnknownTool"), ToolPermissionKey::Other);
    }

    // ── Story 6.35 (review, D1) : outils Claude hors table initiale, mappés explicitement pour
    //    éviter la régression heuristique (TodoWrite→EditWrite, KillShell/SlashCommand→Bash) ──
    #[test]
    fn claude_outils_hors_table_categorises_explicitement() {
        use ToolPermissionKey::*;
        // Aucun effet FS → ReadOnly (cohérence avec Grok todo_write/exit_plan_mode).
        assert_eq!(claude("TodoWrite"), ReadOnly);
        assert_eq!(claude("ExitPlanMode"), ReadOnly);
        assert_eq!(claude("BashOutput"), ReadOnly);
        // Contrôle du cycle de vie shell.
        assert_eq!(claude("KillShell"), Bash);
        // Méta-outil au blast-radius imprévisible → reste Other (Manual).
        assert_eq!(claude("SlashCommand"), Other);
    }

    // ── Story 6.35 (review, D2) : durcissement heuristique — opérations destructrices de données
    //    gatées EditWrite AVANT le bucket Bash (la sous-chaîne `run` de prune/truncate les y
    //    faisait tomber), et avant Read (read_and_purge) ──
    #[test]
    fn heuristique_destructive_gate_editwrite() {
        use ToolPermissionKey::*;
        assert_eq!(grok("prune_database"), EditWrite); // contient `run` mais destructeur
        assert_eq!(grok("truncate_table"), EditWrite); // contient `run` mais destructeur
        assert_eq!(grok("wipe_cache"), EditWrite);
        assert_eq!(grok("drop_collection"), EditWrite);
        assert_eq!(grok("reset_workspace"), EditWrite);
        // Nom sans mot-clé write mais destructeur → EditWrite (pas Read).
        assert_eq!(grok("read_and_purge"), EditWrite);
    }

    #[test]
    fn casse_insensible() {
        assert_eq!(grok("READ_FILE"), ToolPermissionKey::Read);
        assert_eq!(claude("bash"), ToolPermissionKey::Bash);
    }

    // ── resolve_permission : override-agent > global > défaut (Manual), source_tool threadé ──
    #[test]
    fn resolve_utilise_regle_agent_si_presente() {
        let mut overrides = AgentPermissionOverrides::new();
        overrides.insert(ToolPermissionKey::Read, ToolPermissionAction::Approve);
        let mut global = HashMap::new();
        global.insert(ToolPermissionKey::Read, ToolPermissionAction::Deny);
        assert_eq!(
            resolve_permission(SourceTool::Grok, "read_file", &overrides, &global),
            ToolPermissionAction::Approve
        );
    }

    #[test]
    fn resolve_fallback_global_si_absent() {
        let overrides = AgentPermissionOverrides::new();
        let mut global = HashMap::new();
        global.insert(ToolPermissionKey::Read, ToolPermissionAction::Deny);
        assert_eq!(
            resolve_permission(SourceTool::Grok, "read_file", &overrides, &global),
            ToolPermissionAction::Deny
        );
    }

    #[test]
    fn resolve_manual_si_aucune_regle() {
        let overrides = AgentPermissionOverrides::new();
        let global = HashMap::new();
        // Défaut Manual, y compris pour un nom Claude (non-régression).
        assert_eq!(
            resolve_permission(SourceTool::ClaudeCode, "Bash", &overrides, &global),
            ToolPermissionAction::Manual
        );
        assert_eq!(
            resolve_permission(SourceTool::Grok, "read_file", &overrides, &global),
            ToolPermissionAction::Manual
        );
    }
}
