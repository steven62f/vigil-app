import { load } from "@tauri-apps/plugin-store";
import { emit } from "@tauri-apps/api/event";
// Imports type-only → effacés à la compilation, pas de dépendance runtime lib→component/types.
import type { MascotVariantId } from "../components/shared/mascotVariants";
import type { SourceTool } from "../types/agent";

export type NotchDisplayMode = "multi-agent" | "priority-agent";
export type UsageBarDisplay = "none" | "top" | "bottom";
// Outil affiché dans la barre de consommation du Notch (Story 6.13).
// `null` = « jamais choisi » → la résolution runtime (resolveNotchUsageTool) détermine
// l'outil réel de façon GÉNÉRIQUE (surtout PAS de "claude_code" codé en dur par défaut).
export type NotchUsageTool = "claude_code" | "codex" | "grok" | null;
export type MaxVisibleAgents = 4 | 5 | 6 | 7;
export type DetailHistoryMode = "detailed" | "summary";
export type StateSound = "chime" | "pop" | "custom" | null;
export type ToolPermissionAction = "manual" | "approve" | "deny";
// Story 6.35 — `Mcp` (outils MCP) et `Subagent` (délégation) ajoutés. Doit rester symétrique
// de l'enum Rust `ToolPermissionKey` (les clés sérialisent en "Mcp"/"Subagent").
export type ToolPermissionKey =
  | "Bash" | "EditWrite" | "Read" | "WebFetch" | "ReadOnly" | "Mcp" | "Subagent" | "Other";

export interface ToolPermissions {
  Bash: ToolPermissionAction;
  EditWrite: ToolPermissionAction;
  Read: ToolPermissionAction;
  WebFetch: ToolPermissionAction;
  ReadOnly: ToolPermissionAction;
  Mcp: ToolPermissionAction;
  Subagent: ToolPermissionAction;
  Other: ToolPermissionAction;
}

const DEFAULT_TOOL_PERMISSIONS: ToolPermissions = {
  Bash: "manual",
  EditWrite: "manual",
  Read: "manual",
  WebFetch: "manual",
  ReadOnly: "manual",
  Mcp: "manual",
  Subagent: "manual",
  Other: "manual",
};

export type AgentStateKey =
  | "working" | "waitingApprove" | "waitingSpecific"
  | "ended" | "idle" | "error" | "disconnected";

export interface StateConfig {
  color: string | null;
  sound: StateSound;
  soundPath: string | null;
  autoHighlight: boolean;
}

export interface VigilSettings {
  notchDisplayMode: NotchDisplayMode;
  maxVisibleAgents: MaxVisibleAgents;
  detailedSingleAgent: boolean;
  showSubagents: boolean;
  autoCleanupDisconnected: boolean;
  stateSettings: Record<AgentStateKey, StateConfig>;
  quickTerminalAccess: boolean;
  detailHistoryMode: DetailHistoryMode;
  startWithWindows: boolean;
  toolPermissions: ToolPermissions;
  preferredTerminal: string;
  usageTrackingEnabled: boolean;
  // Story 6.25 — amorçage opt-in de la fenêtre 5h Claude (Vigil envoie un message minimal
  // quand la fenêtre est fermée). OFF par défaut : c'est la 1re action de Vigil consommant du
  // quota sur le compte. Lu côté backend depuis ce même store partagé (aucune commande Tauri).
  usageWindowPrimingEnabled: boolean;
  showUsageInNotch: UsageBarDisplay;
  notchUsageTool: NotchUsageTool;
  hideNotchIfEmpty: boolean;
  showToolBadge: boolean;
  showModel: boolean;
  autoRepairHooks: boolean;
  // Une variante de mascotte par valeur de `SourceTool` (claude_code/codex/grok/antigravity/unknown).
  // Depuis 6.19, `antigravity` fait partie de `SourceTool` (badge + mascotte dédiée `orbit`) → la clé
  // est désormais requise par `Record<SourceTool, MascotVariantId>`, plus besoin d'intersection Partial.
  mascotByTool: Record<SourceTool, MascotVariantId>;
}

const DEFAULT_STATE_CONFIG: StateConfig = {
  color: null,
  sound: null,
  soundPath: null,
  autoHighlight: false,
};

export const DEFAULT_SETTINGS: VigilSettings = {
  notchDisplayMode: "multi-agent",
  maxVisibleAgents: 4,
  detailedSingleAgent: false,
  showSubagents: true,
  autoCleanupDisconnected: false,
  stateSettings: {
    working:         { ...DEFAULT_STATE_CONFIG, color: "#4a9eff" },
    waitingApprove:  { ...DEFAULT_STATE_CONFIG, color: "#f5a623", autoHighlight: true },
    waitingSpecific: { ...DEFAULT_STATE_CONFIG, color: "#f5a623", autoHighlight: true },
    ended:           { ...DEFAULT_STATE_CONFIG, color: "#4ade80" },
    idle:            { ...DEFAULT_STATE_CONFIG, color: "#4a4f68" },
    error:           { ...DEFAULT_STATE_CONFIG, color: "#f87171" },
    disconnected:    { ...DEFAULT_STATE_CONFIG, color: "#4a4f68" },
  },
  quickTerminalAccess: false,
  detailHistoryMode: "detailed",
  startWithWindows: true,
  toolPermissions: DEFAULT_TOOL_PERMISSIONS,
  preferredTerminal: "wt.exe",
  usageTrackingEnabled: false,
  usageWindowPrimingEnabled: false,
  showUsageInNotch: "none",
  // null = jamais choisi → résolution générique au render (Story 6.13). `loadSettings`
  // ignore déjà les valeurs null stockées → aucune modif de load/saveSettings.
  notchUsageTool: null,
  hideNotchIfEmpty: false,
  // Défaut basculé true → false (Story 6.9) : la mascotte distincte porte désormais
  // l'identité de l'outil, le badge texte devient redondant par défaut (réactivable).
  showToolBadge: false,
  showModel: true,
  autoRepairHooks: true,
  mascotByTool: {
    claude_code: "classic",
    codex: "robot",
    grok: "horned",
    antigravity: "orbit",
    kimi: "lunar",
    unknown: "classic",
  },
};

// Résout la variante de mascotte assignée à un outil (avec fallback robuste pour un
// store partiel issu d'une version antérieure).
export function mascotForTool(
  sourceTool: SourceTool,
  settings: VigilSettings,
): MascotVariantId {
  return settings.mascotByTool[sourceTool] ?? "classic";
}

const STORE_FILE = "vigil-settings.json";

let _cachedSettings: VigilSettings | null = null;

export async function loadSettings(): Promise<VigilSettings> {
  const store = await load(STORE_FILE, { defaults: {}, autoSave: false });
  const result = { ...DEFAULT_SETTINGS };
  for (const key of Object.keys(DEFAULT_SETTINGS) as (keyof VigilSettings)[]) {
    const stored = await store.get(key);
    if (stored !== undefined && stored !== null) {
      // @ts-expect-error — dynamic key assignment
      result[key] = stored;
    }
  }
  // Deep-merge des Records totaux indexés par SourceTool : un store hérité d'une version
  // antérieure à l'ajout d'une clé d'outil (ex. `antigravity` ajouté en 6.19) ne possède
  // pas toutes les clés → le merge superficiel ci-dessus l'écrase en bloc et perd la
  // mascotte dédiée du nouvel outil (retour silencieux sur `classic`, cf. mascotForTool).
  // On backfille les défauts manquants. Vaut pour tout futur outil (1 outil = 1 clé mascotByTool).
  result.mascotByTool = { ...DEFAULT_SETTINGS.mascotByTool, ...result.mascotByTool };
  // Story 6.35 — même leçon (6.19) pour `toolPermissions` : un store hérité écrit avant l'ajout
  // des clés `Mcp`/`Subagent` ne les possède pas → le merge superficiel ci-dessus l'écrase en
  // bloc et ces clés retomberaient `undefined` (rupture UI/résolution). On backfille les défauts
  // manquants (`manual`) SANS écraser les valeurs déjà configurées par l'utilisateur.
  result.toolPermissions = { ...DEFAULT_TOOL_PERMISSIONS, ...result.toolPermissions };
  // Troisième instance de la même classe (défer 2.4, levé le 2026-07-17) — mais `stateSettings`
  // exige DEUX niveaux, contrairement aux deux Records ci-dessus : ses valeurs sont des OBJETS
  // (`StateConfig`), pas des scalaires. Un simple `{ ...DEFAULT, ...result }` backfillerait les
  // clés d'état manquantes, PAS les champs manquants dans chaque `StateConfig` — le jour où
  // `DEFAULT_STATE_CONFIG` gagne un champ, l'objet stocké écrase le défaut en bloc et le champ
  // retombe `undefined` (déjà vécu avec `autoHighlight`/`sound` : masqué car auto-guérissant à la
  // première sauvegarde). On backfille donc état par état, puis champ par champ, sans jamais
  // écraser une valeur configurée par l'utilisateur.
  result.stateSettings = Object.fromEntries(
    (Object.keys(DEFAULT_SETTINGS.stateSettings) as AgentStateKey[]).map((key) => [
      key,
      { ...DEFAULT_SETTINGS.stateSettings[key], ...result.stateSettings?.[key] },
    ]),
  ) as Record<AgentStateKey, StateConfig>;
  _cachedSettings = result;
  return result;
}

export async function saveSettings(partial: Partial<VigilSettings>): Promise<void> {
  const store = await load(STORE_FILE, { defaults: {}, autoSave: false });
  for (const [key, value] of Object.entries(partial)) {
    await store.set(key, value);
  }
  await store.save();
  _cachedSettings = { ...(_cachedSettings ?? DEFAULT_SETTINGS), ...partial };
  await emit("vigil:settings-changed", _cachedSettings);
}
