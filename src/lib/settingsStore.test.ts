import { describe, it, expect, vi } from "vitest";

// Mock @tauri-apps/plugin-store pour les tests (non disponible hors Tauri)
vi.mock("@tauri-apps/plugin-store", () => ({
  load: vi.fn(),
}));

import { load } from "@tauri-apps/plugin-store";
import { DEFAULT_SETTINGS, loadSettings, mascotForTool, type VigilSettings } from "./settingsStore";

// Fabrique un faux store dont `get(key)` retourne les valeurs fournies (undefined sinon).
function mockStore(values: Record<string, unknown>) {
  vi.mocked(load).mockResolvedValue({
    get: vi.fn(async (k: string) => values[k]),
    set: vi.fn(),
    save: vi.fn(),
  } as unknown as Awaited<ReturnType<typeof load>>);
}

describe("DEFAULT_SETTINGS", () => {
  it("notchDisplayMode défaut est multi-agent", () => {
    expect(DEFAULT_SETTINGS.notchDisplayMode).toBe("multi-agent");
  });

  it("maxVisibleAgents défaut est 4", () => {
    expect(DEFAULT_SETTINGS.maxVisibleAgents).toBe(4);
  });

  it("quickTerminalAccess défaut est false", () => {
    expect(DEFAULT_SETTINGS.quickTerminalAccess).toBe(false);
  });

  it("detailHistoryMode défaut est detailed", () => {
    expect(DEFAULT_SETTINGS.detailHistoryMode).toBe("detailed");
  });

  it("startWithWindows défaut est true", () => {
    expect(DEFAULT_SETTINGS.startWithWindows).toBe(true);
  });

  it("showToolBadge défaut est false (basculé en Story 6.9 : la mascotte porte l'identité)", () => {
    expect(DEFAULT_SETTINGS.showToolBadge).toBe(false);
  });

  it("mascotByTool a un défaut par outil (6 clés SourceTool ; antigravity: orbit Story 6.19, kimi: lunar Story 9.2)", () => {
    expect(DEFAULT_SETTINGS.mascotByTool).toEqual({
      claude_code: "classic",
      codex: "robot",
      grok: "horned",
      antigravity: "orbit",
      kimi: "lunar",
      unknown: "classic",
    });
  });

  it("showModel défaut est true", () => {
    expect(DEFAULT_SETTINGS.showModel).toBe(true);
  });

  it("autoRepairHooks défaut est true", () => {
    expect(DEFAULT_SETTINGS.autoRepairHooks).toBe(true);
  });

  it("usageWindowPrimingEnabled défaut est false (opt-in strict, Story 6.25)", () => {
    expect(DEFAULT_SETTINGS.usageWindowPrimingEnabled).toBe(false);
  });

  it("stateSettings contient les 7 états", () => {
    const states = Object.keys(DEFAULT_SETTINGS.stateSettings);
    expect(states).toContain("working");
    expect(states).toContain("waitingApprove");
    expect(states).toContain("waitingSpecific");
    expect(states).toContain("ended");
    expect(states).toContain("idle");
    expect(states).toContain("error");
    expect(states).toContain("disconnected");
  });

  it("chaque état a sound=null par défaut", () => {
    for (const config of Object.values(DEFAULT_SETTINGS.stateSettings)) {
      expect(config.sound).toBeNull();
    }
  });

  it("waitingApprove et waitingSpecific ont autoHighlight=true par défaut", () => {
    expect(DEFAULT_SETTINGS.stateSettings.waitingApprove.autoHighlight).toBe(true);
    expect(DEFAULT_SETTINGS.stateSettings.waitingSpecific.autoHighlight).toBe(true);
  });

  it("les autres états ont autoHighlight=false par défaut", () => {
    const otherStates = ["working", "ended", "idle", "error", "disconnected"] as const;
    for (const key of otherStates) {
      expect(DEFAULT_SETTINGS.stateSettings[key].autoHighlight).toBe(false);
    }
  });

  it("stateSettings contient exactement 7 états", () => {
    expect(Object.keys(DEFAULT_SETTINGS.stateSettings)).toHaveLength(7);
  });

  it("soundPath est null pour tous les états par défaut", () => {
    for (const config of Object.values(DEFAULT_SETTINGS.stateSettings)) {
      expect(config.soundPath).toBeNull();
    }
  });

  // Story 6.35 — les 2 catégories neuves existent avec défaut manual (aucun flip).
  it("toolPermissions défaut = 8 catégories toutes 'manual' (Mcp/Subagent ajoutées)", () => {
    expect(DEFAULT_SETTINGS.toolPermissions).toEqual({
      Bash: "manual",
      EditWrite: "manual",
      Read: "manual",
      WebFetch: "manual",
      ReadOnly: "manual",
      Mcp: "manual",
      Subagent: "manual",
      Other: "manual",
    });
  });
});

// Story 6.35 — migration backfill (AC5) : un store hérité sans les clés Mcp/Subagent les reçoit
// avec leur défaut (manual) SANS écraser une valeur déjà configurée par l'utilisateur.
describe("loadSettings — backfill toolPermissions (Story 6.35)", () => {
  it("backfille Mcp/Subagent=manual sur un store hérité sans écraser une valeur configurée", async () => {
    mockStore({
      // Store hérité : Bash configuré en approve, PAS de Mcp/Subagent.
      toolPermissions: { Bash: "approve", EditWrite: "manual", Read: "manual", WebFetch: "manual", ReadOnly: "manual", Other: "manual" },
    });
    const settings = await loadSettings();
    // Valeur configurée préservée.
    expect(settings.toolPermissions.Bash).toBe("approve");
    // Clés neuves backfillées au défaut.
    expect(settings.toolPermissions.Mcp).toBe("manual");
    expect(settings.toolPermissions.Subagent).toBe("manual");
  });

  it("un store sans toolPermissions du tout retombe sur les 8 défauts", async () => {
    mockStore({});
    const settings = await loadSettings();
    expect(settings.toolPermissions).toEqual(DEFAULT_SETTINGS.toolPermissions);
  });
});

// Défer 2.4 levé le 2026-07-17 — 3e instance de la classe « Record backfillé » (après mascotByTool
// /6.19 et toolPermissions/6.35), mais la SEULE à valeurs OBJET → merge à DEUX niveaux requis.
describe("loadSettings — backfill stateSettings (défer 2.4)", () => {
  // NIVEAU 1 — garde de non-régression : le merge à un seul niveau couvrait DÉJÀ ce cas (une clé
  // absente du store retombe sur le défaut). Conservé pour verrouiller le comportement, mais ce
  // n'est PAS lui qui justifie le merge à deux niveaux : c'est le test NIVEAU 2 ci-dessous.
  it("backfille un état manquant sans écraser un état configuré", async () => {
    // Store hérité RÉALISTE : la clé `disconnected` est ABSENTE (JSON n'a pas d'`undefined`).
    const { disconnected: _absent, ...legacyStates } = DEFAULT_SETTINGS.stateSettings;
    mockStore({
      stateSettings: {
        ...legacyStates,
        working: { color: "#ff0000", sound: "none", soundPath: null, autoHighlight: false },
      },
    });
    const settings = await loadSettings();
    expect(settings.stateSettings.working.color).toBe("#ff0000");
    expect(settings.stateSettings.disconnected).toEqual(DEFAULT_SETTINGS.stateSettings.disconnected);
  });

  it("NIVEAU 2 : backfille un champ manquant DANS un StateConfig stocké, sans écraser les autres", async () => {
    mockStore({
      // Store écrit avant l'ajout d'`autoHighlight` à StateConfig : la clé existe, le champ non.
      // Un merge à un seul niveau laisserait `autoHighlight` à `undefined` (falsy → feature morte).
      stateSettings: {
        ...DEFAULT_SETTINGS.stateSettings,
        waitingApprove: { color: "#123456", sound: "none", soundPath: null },
      },
    });
    const settings = await loadSettings();
    expect(settings.stateSettings.waitingApprove.color).toBe("#123456");
    expect(settings.stateSettings.waitingApprove.autoHighlight).toBe(
      DEFAULT_SETTINGS.stateSettings.waitingApprove.autoHighlight,
    );
  });

  it("un store sans stateSettings du tout retombe sur les défauts", async () => {
    mockStore({});
    const settings = await loadSettings();
    expect(settings.stateSettings).toEqual(DEFAULT_SETTINGS.stateSettings);
  });
});

describe("mascotForTool", () => {
  it("résout la variante par défaut de chaque outil", () => {
    expect(mascotForTool("claude_code", DEFAULT_SETTINGS)).toBe("classic");
    expect(mascotForTool("codex", DEFAULT_SETTINGS)).toBe("robot");
    expect(mascotForTool("grok", DEFAULT_SETTINGS)).toBe("horned");
    expect(mascotForTool("antigravity", DEFAULT_SETTINGS)).toBe("orbit");
  });

  it("unknown retombe sur classic", () => {
    expect(mascotForTool("unknown", DEFAULT_SETTINGS)).toBe("classic");
  });

  it("respecte un override libre (même variante pour plusieurs outils)", () => {
    const settings: VigilSettings = {
      ...DEFAULT_SETTINGS,
      mascotByTool: { ...DEFAULT_SETTINGS.mascotByTool, codex: "classic", grok: "classic" },
    };
    expect(mascotForTool("codex", settings)).toBe("classic");
    expect(mascotForTool("grok", settings)).toBe("classic");
    expect(mascotForTool("claude_code", settings)).toBe("classic");
  });

  it("fallback classic si la clé est absente (store partiel d'une version antérieure)", () => {
    const partial = {
      ...DEFAULT_SETTINGS,
      mascotByTool: { claude_code: "classic" },
    } as unknown as VigilSettings;
    expect(mascotForTool("codex", partial)).toBe("classic");
  });
});
