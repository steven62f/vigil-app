// Helpers purs (sans React) pour les onglets multi-outils de la fenêtre Statistics (Story 6.10).
import type { ToolsStatus } from "../types/tools";
import type { LocalStats } from "../types/stats";
import type { SourceTool } from "../types/agent";
import type { NotchUsageTool } from "./settingsStore";

/** Outils susceptibles d'apparaître en onglet, dans l'ordre d'affichage fixe. */
export type StatsTool = Exclude<SourceTool, "unknown">;

/** Outils dont les stats locales (tokens) sont parsables de façon fiable. Grok rejoint le set en 6.32
 * (tokens sur disque `~/.grok/sessions/…/updates.jsonl`). Antigravity reste exclu → placeholder
 * (aucun token stocké localement).
 *
 * Kimi rejoint le set en **9.4** : tokens sur disque dans les `wire.jsonl` (`usage.record`, scope
 * "turn") — la donnée existe en LOCAL (contrairement à Antigravity), scannée par
 * `get_local_stats("kimi")`. Coût = grille **Moonshot** (`pricing_for_kimi_model`, estimation V1 ~$,
 * ancrée sur Kimi K2.6) pour le cas GÉNÉRAL ; les modèles auto-hébergés (LM Studio/Ollama) sont à 0
 * (aucun coût d'API). Jamais la grille Claude par défaut (bras `"kimi"` explicite côté Rust). */
export const LOCAL_STATS_SUPPORTED: StatsTool[] = ["claude_code", "codex", "grok", "kimi"];

/**
 * Outils dont le quota d'usage live est récupérable en V1 (fenêtre Usage, Story 6.12).
 * Sémantique DISTINCTE de LOCAL_STATS_SUPPORTED (stats disque). DIVERGE de LOCAL_STATS_SUPPORTED :
 * Antigravity a un usage live SANS stats tokens locales (aucun token stocké sur disque).
 *
 * ⚠️ Antigravity (Story 6.24) : la source N'EST PAS le cloud Google mais le SERVEUR LOCAL du
 * process `agy` (`GetUserStatus`, protocole Connect-RPC) — le cloud expose un quota Gemini Code
 * Assist journalier sans rapport, et l'usage Antigravity cloud exige un login OAuth dédié. Donc
 * l'usage n'est LIVE que si agy tourne ; sinon on affiche le dernier cache (« Updated X ago »),
 * avec repli spawn-bref sur Refresh manuel.
 *
 * Grok rejoint le set en 6.32 : quota HEBDO en crédits via `billing?format=credits`
 * (token `~/.grok/auth.json`).
 *
 * Kimi : ABSENT — Story **9.5**, elle-même DÉFÉRÉE et spike-first (la source d'usage de Moonshot
 * est à ce jour inconnue). Ne pas l'ajouter avant d'avoir vu la donnée.
 */
export const USAGE_SUPPORTED: StatsTool[] = ["claude_code", "codex", "antigravity", "grok"];

/**
 * Outils dont l'onglet History rend une LISTE (pas le placeholder), Story 6.20.
 *
 * ⚠️ Set DÉLIBÉRÉMENT DISTINCT de LOCAL_STATS_SUPPORTED. Jusqu'à 6.19, History réutilisait
 * LOCAL_STATS_SUPPORTED car History et Stats couvraient exactement les mêmes outils (Claude+Codex).
 * Antigravity CASSE cette hypothèse : il a un History (liste JSONL lue depuis
 * `~/.gemini/antigravity-cli/` : `brain/` + `history.jsonl`) ET, depuis 6.24, un Usage
 * (source LOCALE `GetUserStatus` du process agy) — mais TOUJOURS pas de Stats tokens locales
 * (aucun token stocké sur disque). Ces notions divergent réellement → set dédié pour History.
 *
 * NE PAS ajouter antigravity à LOCAL_STATS_SUPPORTED : cela ferait basculer son onglet Stats
 * de placeholder à « supporté » → invoke sur un outil sans token local → régression.
 * (Usage : antigravity légitimement supporté via 6.24.)
 *
 * Grok (Story 6.28) rejoint History : arbre `~/.grok/sessions/<urlencode(cwd)>/<sessionId>/` lu par
 * `read_grok_summaries` (dispatch Rust `get_all_grouped("grok")`). Depuis 6.32, Grok a AUSSI ses
 * Stats locales (tokens `updates.jsonl`) ET son Usage (quota hebdo crédits) → présent dans les
 * trois sets. Antigravity reste le seul divergent (History + Usage, sans Stats tokens locales).
 *
 * Kimi rejoint History en **9.4** : index `~/.kimi-code/session_index.jsonl` + `state.json` par
 * session, lus par `read_kimi_summaries` (dispatch Rust `get_all_grouped("kimi")`). Le reader est
 * self-contained (ré-implémentation locale de la lecture d'index, l'app ne peut pas importer la
 * crate `vigil-kimi`), et le `transcript_path` = `<sessionDir>/agents/main/wire.jsonl` est
 * byte-identique à ce que `vigil-kimi` rapporte à chaud (badge « Active »). Depuis 9.4, Kimi a AUSSI
 * ses Stats locales (tokens `wire.jsonl`) → présent dans History + Statistics ; Usage reste 9.5.
 */
export const HISTORY_SUPPORTED: StatsTool[] = ["claude_code", "codex", "antigravity", "grok", "kimi"];

/**
 * Outils dont l'INTERRUPTION fonctionne réellement (Story 6.33) — c.-à-d. dont le bridge CONSOMME
 * le flag `%TEMP%/vigil-interrupt-*.flag` écrit par `interrupt_agent`.
 *
 * ⚠️ Claude only, et ce n'est PAS un arbitrage de priorisation : c'est la seule interruption qui
 * existe. Seul `vigil-claude` consomme le flag (`handle_interrupt_flag_claude_code`) et tue le tour
 * via `{"continue":false}`. Les trois autres NE le consomment PAS → afficher le bouton pour eux
 * laissait `interrupt_pending` (« waiting for next tool call… ») bloqué à vie + un flag orphelin.
 *
 * - **Grok** : infaisable via hook, confirmé e2e (spike 6.33). Son contrat est deny-only
 *   (`{"decision":"deny"}` sur `PreToolUse`) = veto PAR OUTIL : l'appel est bien bloqué, mais
 *   l'agent reroute et termine son tour — aucun équivalent au `{"continue":false}` de Claude.
 *   La vraie parité passe par **ACP** (`grok agent stdio`, Vigil client → cancel natif) = story future.
 * - **Codex / Antigravity** : aucun canal d'interruption câblé (leurs bridges ignorent le flag).
 * - **Kimi** : ABSENT = MORT côté outil, même contrat deny-only que Grok/Codex/Antigravity. Aucune
 *   story ne l'ajoutera. Déjà d'accord avec le miroir Rust `flags::interrupt_supported(Kimi)`
 *   (faux depuis la 9.1) : rien à changer ici, l'absence est le comportement CORRECT.
 *
 * Ajouter un outil ici = ✅ **seulement après** avoir câblé la consommation du flag dans son bridge
 * (une entrée sans consommation = affordance morte, exactement le bug corrigé par 6.33).
 * ⚠️ **Miroir Rust obligatoire** : `flags::interrupt_supported` (`src-tauri/src/flags.rs`) exprime la
 * même vérité pour la garde backend — les deux listes DOIVENT rester synchro.
 */
export const INTERRUPT_SUPPORTED: StatsTool[] = ["claude_code"];

/**
 * Outils dont le badge modèle de l'agent VIVANT se lit ON-DEMAND côté backend (`get_agent_model`),
 * parce que leur `AgentSnapshot.model` est `undefined` (aucun modèle injecté au session_start,
 * contrairement à Claude/Codex/Antigravity qui le portent déjà).
 *
 * - **Grok** (6.29) : `summary.json`.`current_model_id`.
 * - **Kimi** (9.4) : dernier `llm.request`.`model` du `wire.jsonl`. La garde `fetchLiveModel`
 *   EXCLUAIT Kimi (`sourceTool !== "grok"`) → sans cet élargissement le badge restait vide.
 *
 * ⚠️ Miroir du dispatch Rust `get_agent_model` (`commands/mod.rs`) : un outil ici DOIT y avoir un
 * bras qui lit réellement le modèle, sinon la garde laisse partir un invoke qui renvoie toujours
 * `None` (affordance morte). Les autres outils portent `model` dans le snapshot → hors de cette liste.
 */
export const LIVE_MODEL_SUPPORTED: StatsTool[] = ["grok", "kimi"];

/**
 * Liste des outils activés (champs `*Enabled` de `get_tools_status`), dans l'ordre fixe
 * claude_code → codex → grok → antigravity → kimi. On suit l'ACTIVATION (comme la page
 * Installation), pas la détection.
 */
export function activatedToolsFromStatus(status: ToolsStatus): StatsTool[] {
  const tools: StatsTool[] = [];
  if (status.claudeCodeEnabled) tools.push("claude_code");
  if (status.codexEnabled) tools.push("codex");
  if (status.grokEnabled) tools.push("grok");
  if (status.antigravityEnabled) tools.push("antigravity");
  if (status.kimiEnabled) tools.push("kimi");
  return tools;
}

/**
 * Résout l'outil à afficher dans la barre de consommation du Notch (Story 6.13),
 * de façon strictement GÉNÉRIQUE — AUCUN cas spécial "claude_code".
 *
 * - `stored` (choix explicite de l'utilisateur) est retenu UNIQUEMENT s'il fait encore
 *   partie des outils activés ET supportés (`activatedSupportedTools`). Sinon il est ignoré
 *   (outil désactivé/non supporté → repli générique — couvre AC6 sans nettoyage du store).
 * - Repli : le PREMIER de `activatedSupportedTools` (ordre d'affichage claude → codex),
 *   quel qu'il soit. Un poste Codex-seul retourne "codex", un poste Claude-seul "claude_code".
 * - Liste vide → `null` (barre masquée, AC4/AC5).
 *
 * La préférence pour Claude quand plusieurs outils sont activés ne vient QUE de l'ordre de
 * `activatedToolsFromStatus`, jamais d'une mention littérale de l'outil ici.
 */
export function resolveNotchUsageTool(
  stored: NotchUsageTool,
  activatedSupportedTools: StatsTool[],
): StatsTool | null {
  if (stored !== null && activatedSupportedTools.includes(stored)) return stored;
  return activatedSupportedTools[0] ?? null;
}

/** Libellé UI d'un outil (mêmes valeurs que `config::tool_label` côté Rust). */
export function toolLabel(tool: StatsTool): string {
  switch (tool) {
    case "claude_code": return "Claude Code";
    case "codex": return "Codex CLI";
    case "grok": return "Grok Build";
    case "antigravity": return "Antigravity CLI";
    case "kimi": return "Kimi CLI";
  }
}

/**
 * Agrégation « All » : volume total de tokens (input + output + cache) par date, toutes
 * sources confondues. AUCUN coût (les pricings sont incomparables entre outils).
 * Les LocalStats partagent les 7 mêmes dates UTC ; la somme par date est donc triviale.
 */
export function aggregateAllTools(perTool: LocalStats[]): { date: string; tokens: number }[] {
  const byDate = new Map<string, number>();
  for (const stats of perTool) {
    for (const day of stats.days) {
      const tokens = day.inputTokens + day.outputTokens + day.cacheTokens;
      byDate.set(day.date, (byDate.get(day.date) ?? 0) + tokens);
    }
  }
  return [...byDate.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([date, tokens]) => ({ date, tokens }));
}
