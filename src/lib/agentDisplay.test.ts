import { describe, it, expect } from "vitest";
import { selectPriorityAgent, applyQuota } from "./agentDisplay";
import type { AgentSnapshot } from "../types/agent";

const agent = (state: AgentSnapshot["state"], lastActivity = 0): AgentSnapshot => ({
  transcriptPath: state,
  sessionId: null,
  name: state,
  state,
  waitingSince: null,
  lastActivity,
  lastActivityLabel: null,
  terminalPid: null,
  source: "terminal",
  sourceTool: "unknown",
});

describe("selectPriorityAgent", () => {
  it("retourne null si la liste est vide", () => {
    expect(selectPriorityAgent([])).toBeNull();
  });

  it("sélectionne waitingApprove avant working", () => {
    const result = selectPriorityAgent([agent("working"), agent("waitingApprove")]);
    expect(result?.state).toBe("waitingApprove");
  });

  it("sélectionne error avant working", () => {
    const result = selectPriorityAgent([agent("working"), agent("error")]);
    expect(result?.state).toBe("error");
  });

  it("sélectionne working avant ended", () => {
    const result = selectPriorityAgent([agent("ended"), agent("working")]);
    expect(result?.state).toBe("working");
  });

  it("garde le premier en cas d'égalité de priorité", () => {
    const a1 = { ...agent("working"), transcriptPath: "first" };
    const a2 = { ...agent("working"), transcriptPath: "second" };
    const result = selectPriorityAgent([a1, a2]);
    expect(result?.transcriptPath).toBe("first");
  });

  it("retourne l'unique agent si liste de taille 1", () => {
    const a = agent("idle");
    expect(selectPriorityAgent([a])).toBe(a);
  });

  it("retourne l'agent avec lastActivity le plus élevé en cas d'égalité d'état", () => {
    const old = agent("working", 100);
    const recent = agent("working", 200);
    expect(selectPriorityAgent([old, recent])).toBe(recent);
    expect(selectPriorityAgent([recent, old])).toBe(recent);
  });

  it("la priorité d'état prime sur lastActivity", () => {
    const lowPrioRecent = agent("working", 9999);
    const highPrioOld = agent("waitingApprove", 1);
    expect(selectPriorityAgent([lowPrioRecent, highPrioOld])).toBe(highPrioOld);
  });

  it("retourne le premier agent en cas d'égalité exacte de lastActivity", () => {
    const a1 = { ...agent("working", 100), transcriptPath: "first" };
    const a2 = { ...agent("working", 100), transcriptPath: "second" };
    expect(selectPriorityAgent([a1, a2])?.transcriptPath).toBe("first");
    expect(selectPriorityAgent([a2, a1])?.transcriptPath).toBe("second");
  });
});

describe("applyQuota", () => {
  it("retourne tous les agents si count ≤ max (overflow = 0)", () => {
    const agents = [agent("working"), agent("idle")];
    const result = applyQuota(agents, 4);
    expect(result.visible).toHaveLength(2);
    expect(result.overflow).toBe(0);
  });

  it("retourne tous si count === max exactement", () => {
    const agents = [agent("working"), agent("idle"), agent("error")];
    const result = applyQuota(agents, 3);
    expect(result.visible).toHaveLength(3);
    expect(result.overflow).toBe(0);
  });

  it("tronque et calcule l'overflow si count > max", () => {
    const agents = [agent("working"), agent("idle"), agent("error"), agent("ended"), agent("waitingApprove")];
    const result = applyQuota(agents, 3);
    expect(result.visible).toHaveLength(3);
    expect(result.overflow).toBe(2);
  });

  it("cas limite max=1", () => {
    const agents = [agent("working"), agent("idle")];
    const result = applyQuota(agents, 1);
    expect(result.visible).toHaveLength(1);
    expect(result.overflow).toBe(1);
    expect(result.visible[0].state).toBe("working");
  });
});
