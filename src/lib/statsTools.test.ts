import { describe, it, expect } from "vitest";
import { activatedToolsFromStatus, aggregateAllTools, HISTORY_SUPPORTED, INTERRUPT_SUPPORTED, LIVE_MODEL_SUPPORTED, LOCAL_STATS_SUPPORTED, USAGE_SUPPORTED, toolLabel, resolveNotchUsageTool } from "./statsTools";
import type { ToolsStatus } from "../types/tools";
import type { LocalStats } from "../types/stats";

const status = (over: Partial<ToolsStatus>): ToolsStatus => ({
  claudeCodeDetected: false,
  codexDetected: false,
  grokDetected: false,
  antigravityDetected: false,
  kimiDetected: false,
  claudeCodeEnabled: false,
  codexEnabled: false,
  grokEnabled: false,
  antigravityEnabled: false,
  kimiEnabled: false,
  claudeCodeHooksOk: false,
  codexHooksOk: false,
  grokHooksOk: false,
  antigravityHooksOk: false,
  kimiHooksOk: false,
  ...over,
});

const localStats = (days: { date: string; inputTokens?: number; outputTokens?: number; cacheTokens?: number }[]): LocalStats => ({
  days: days.map((d) => ({
    date: d.date,
    inputTokens: d.inputTokens ?? 0,
    outputTokens: d.outputTokens ?? 0,
    cacheTokens: d.cacheTokens ?? 0,
    costUsd: 0,
    sessionCount: 0,
  })),
  totalCostUsd: 0,
  totalTokens: 0,
});

describe("activatedToolsFromStatus", () => {
  it("renvoie [] quand aucun outil n'est activé", () => {
    expect(activatedToolsFromStatus(status({}))).toEqual([]);
  });

  it("renvoie un seul outil activé", () => {
    expect(activatedToolsFromStatus(status({ claudeCodeEnabled: true }))).toEqual(["claude_code"]);
    expect(activatedToolsFromStatus(status({ codexEnabled: true }))).toEqual(["codex"]);
  });

  it("respecte l'ordre fixe claude_code → codex → grok → antigravity quel que soit l'ordre des champs", () => {
    expect(activatedToolsFromStatus(status({ antigravityEnabled: true, grokEnabled: true, codexEnabled: true, claudeCodeEnabled: true })))
      .toEqual(["claude_code", "codex", "grok", "antigravity"]);
  });

  it("place antigravity EN DERNIER (Story 6.19)", () => {
    expect(activatedToolsFromStatus(status({ antigravityEnabled: true }))).toEqual(["antigravity"]);
    expect(activatedToolsFromStatus(status({ claudeCodeEnabled: true, antigravityEnabled: true })))
      .toEqual(["claude_code", "antigravity"]);
  });

  it("ignore antigravity seulement détecté mais non activé", () => {
    expect(activatedToolsFromStatus(status({ antigravityDetected: true }))).toEqual([]);
  });

  it("ignore les outils seulement détectés mais non activés", () => {
    expect(activatedToolsFromStatus(status({ codexDetected: true, grokDetected: true }))).toEqual([]);
  });
});

describe("LOCAL_STATS_SUPPORTED", () => {
  it("contient claude_code, codex, grok ET kimi (6.32 updates.jsonl / 9.4 usage.record wire.jsonl)", () => {
    expect(LOCAL_STATS_SUPPORTED).toContain("claude_code");
    expect(LOCAL_STATS_SUPPORTED).toContain("codex");
    expect(LOCAL_STATS_SUPPORTED).toContain("grok");
    expect(LOCAL_STATS_SUPPORTED).toContain("kimi");
  });

  it("ne contient PAS antigravity (placeholder : aucun token stocké localement)", () => {
    expect(LOCAL_STATS_SUPPORTED).not.toContain("antigravity");
  });
});

describe("USAGE_SUPPORTED", () => {
  it("contient claude_code, codex, antigravity ET grok (6.24 GetUserStatus + 6.32 crédits hebdo)", () => {
    expect(USAGE_SUPPORTED).toContain("claude_code");
    expect(USAGE_SUPPORTED).toContain("codex");
    expect(USAGE_SUPPORTED).toContain("antigravity");
    expect(USAGE_SUPPORTED).toContain("grok");
  });

  it("antigravity a Usage mais PAS Stats locales (divergence 6.24 : garde-fou LOCAL_STATS)", () => {
    expect(USAGE_SUPPORTED).toContain("antigravity");
    expect(LOCAL_STATS_SUPPORTED).not.toContain("antigravity");
  });

  it("ne contient PAS kimi (Story 9.5 déférée, spike-first — garde-fou : ne rien annoncer de non câblé)", () => {
    expect(USAGE_SUPPORTED).not.toContain("kimi");
  });
});

describe("HISTORY_SUPPORTED", () => {
  it("contient claude_code, codex, antigravity, grok ET kimi (6.28 / 9.4)", () => {
    expect(HISTORY_SUPPORTED).toContain("claude_code");
    expect(HISTORY_SUPPORTED).toContain("codex");
    expect(HISTORY_SUPPORTED).toContain("antigravity");
    expect(HISTORY_SUPPORTED).toContain("grok");
    expect(HISTORY_SUPPORTED).toContain("kimi");
  });

  it("Kimi a History + Stats (9.4) mais PAS Usage (9.5 déférée)", () => {
    expect(HISTORY_SUPPORTED).toContain("kimi");
    expect(LOCAL_STATS_SUPPORTED).toContain("kimi");
    expect(USAGE_SUPPORTED).not.toContain("kimi");
  });

  it("Grok a désormais History (6.28), Stats ET Usage (6.32) — présent dans les trois sets", () => {
    expect(HISTORY_SUPPORTED).toContain("grok");
    expect(LOCAL_STATS_SUPPORTED).toContain("grok");
    expect(USAGE_SUPPORTED).toContain("grok");
  });

  it("DIVERGE de LOCAL_STATS_SUPPORTED : antigravity a History et Usage (6.24) mais pas Stats tokens (garde-fou)", () => {
    expect(HISTORY_SUPPORTED).toContain("antigravity");
    expect(USAGE_SUPPORTED).toContain("antigravity"); // Usage désormais supporté (6.24)
    expect(LOCAL_STATS_SUPPORTED).not.toContain("antigravity"); // Stats tokens toujours absentes
  });
});

describe("INTERRUPT_SUPPORTED", () => {
  it("contient claude_code (seul bridge qui consomme le flag d'interrupt)", () => {
    expect(INTERRUPT_SUPPORTED).toContain("claude_code");
  });

  it("ne contient RIEN d'autre que claude_code (Story 6.33 — affordance sinon morte)", () => {
    // Grok : deny-only, l'agent reroute et finit son tour (spike e2e 6.33) → ACP requis.
    // Codex / Antigravity : aucun canal d'interruption câblé (flag ignoré par leurs bridges).
    // Égalité stricte (et non des `not.toContain` outil par outil) : l'invariant doit tenir aussi
    // pour un outil FUTUR ajouté ici par symétrie avec les autres sets, sans bridge qui consomme
    // le flag — c'est exactement le bug que 6.33 corrige.
    expect(INTERRUPT_SUPPORTED).toEqual(["claude_code"]);
  });

  it("DIVERGE des autres sets : Grok a History, Stats et Usage mais pas l'interruption", () => {
    expect(HISTORY_SUPPORTED).toContain("grok");
    expect(LOCAL_STATS_SUPPORTED).toContain("grok");
    expect(USAGE_SUPPORTED).toContain("grok");
    expect(INTERRUPT_SUPPORTED).not.toContain("grok");
  });
});

describe("LIVE_MODEL_SUPPORTED", () => {
  it("contient grok ET kimi (badge modèle lu on-demand : model absent du snapshot)", () => {
    expect(LIVE_MODEL_SUPPORTED).toContain("grok");
    expect(LIVE_MODEL_SUPPORTED).toContain("kimi");
  });

  it("n'inclut PAS les outils qui portent déjà model dans le snapshot", () => {
    expect(LIVE_MODEL_SUPPORTED).not.toContain("claude_code");
    expect(LIVE_MODEL_SUPPORTED).not.toContain("codex");
    expect(LIVE_MODEL_SUPPORTED).not.toContain("antigravity");
  });
});

describe("toolLabel", () => {
  it("mappe les libellés UI attendus", () => {
    expect(toolLabel("claude_code")).toBe("Claude Code");
    expect(toolLabel("codex")).toBe("Codex CLI");
    expect(toolLabel("grok")).toBe("Grok Build");
    expect(toolLabel("antigravity")).toBe("Antigravity CLI");
    expect(toolLabel("kimi")).toBe("Kimi CLI");
  });
});

describe("resolveNotchUsageTool", () => {
  it("Codex seul activé → codex (aucun défaut « claude » quand Claude n'est pas activé)", () => {
    expect(resolveNotchUsageTool(null, ["codex"])).toBe("codex");
  });

  it("Claude seul activé → claude_code", () => {
    expect(resolveNotchUsageTool(null, ["claude_code"])).toBe("claude_code");
  });

  it("stored valide + activé + supporté → lui-même", () => {
    expect(resolveNotchUsageTool("codex", ["claude_code", "codex"])).toBe("codex");
  });

  it("stored désactivé (plus dans activatedSupportedTools) → premier supporté (repli générique)", () => {
    expect(resolveNotchUsageTool("codex", ["claude_code"])).toBe("claude_code");
  });

  it("plusieurs outils, aucun choix → premier de la liste (ordre d'affichage)", () => {
    expect(resolveNotchUsageTool(null, ["claude_code", "codex"])).toBe("claude_code");
  });

  it("activatedSupportedTools vide → null (barre masquée)", () => {
    expect(resolveNotchUsageTool(null, [])).toBeNull();
    expect(resolveNotchUsageTool("claude_code", [])).toBeNull();
  });

  it("n'applique JAMAIS un défaut « claude » quand Claude n'est pas dans la liste", () => {
    // Même si l'utilisateur avait choisi claude auparavant, un poste Codex-seul
    // ne doit jamais retomber sur claude_code (générique strict).
    expect(resolveNotchUsageTool("claude_code", ["codex"])).toBe("codex");
    // grok stocké mais non supporté → premier supporté, pas de « claude » injecté.
    expect(resolveNotchUsageTool("grok", ["codex"])).toBe("codex");
  });
});

describe("aggregateAllTools", () => {
  it("somme les tokens par date sur plusieurs outils", () => {
    const a = localStats([{ date: "2026-05-18", inputTokens: 100, outputTokens: 50, cacheTokens: 10 }]);
    const b = localStats([{ date: "2026-05-18", inputTokens: 200, outputTokens: 0, cacheTokens: 0 }]);
    expect(aggregateAllTools([a, b])).toEqual([{ date: "2026-05-18", tokens: 360 }]);
  });

  it("gère les dates manquantes dans l'un des outils (union triée)", () => {
    const a = localStats([{ date: "2026-05-17", inputTokens: 10 }, { date: "2026-05-18", inputTokens: 20 }]);
    const b = localStats([{ date: "2026-05-18", inputTokens: 5 }]);
    expect(aggregateAllTools([a, b])).toEqual([
      { date: "2026-05-17", tokens: 10 },
      { date: "2026-05-18", tokens: 25 },
    ]);
  });

  it("renvoie [] pour une liste vide", () => {
    expect(aggregateAllTools([])).toEqual([]);
  });

  it("ne renvoie aucun coût (uniquement date + tokens)", () => {
    const a = localStats([{ date: "2026-05-18", inputTokens: 100 }]);
    const result = aggregateAllTools([a]);
    expect(Object.keys(result[0])).toEqual(["date", "tokens"]);
  });
});
