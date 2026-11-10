import { describe, it, expect } from "vitest";
import { priorityOf, isWaitingState } from "./useAutoHighlight";

describe("isWaitingState (repli auto du détail)", () => {
  it("vrai pour les états d'attente d'une décision utilisateur", () => {
    expect(isWaitingState("waitingApprove")).toBe(true);
    expect(isWaitingState("waitingSpecific")).toBe(true);
  });

  it("faux pour tout état non résolvable hors de Vigil", () => {
    expect(isWaitingState("working")).toBe(false);
    expect(isWaitingState("idle")).toBe(false);
    expect(isWaitingState("error")).toBe(false);
    expect(isWaitingState("ended")).toBe(false);
    expect(isWaitingState("disconnected")).toBe(false);
  });
});

describe("priorityOf", () => {
  it("retourne 0 pour waitingApprove (plus haute priorité)", () => {
    expect(priorityOf("waitingApprove")).toBe(0);
  });

  it("retourne Infinity pour working et idle (pas dans la liste highlight)", () => {
    expect(priorityOf("working")).toBe(Infinity);
    expect(priorityOf("idle")).toBe(Infinity);
  });

  it("waitingApprove a priorité plus haute que error", () => {
    expect(priorityOf("waitingApprove")).toBeLessThan(priorityOf("error"));
  });

  it("error a priorité plus haute que ended", () => {
    expect(priorityOf("error")).toBeLessThan(priorityOf("ended"));
  });
});

describe("logique highlight — sélection candidat", () => {
  it("sélectionne waitingApprove sur error quand les deux transitent simultanément", () => {
    const candidates = [
      { transcriptPath: "/a.jsonl", state: "error" as const },
      { transcriptPath: "/b.jsonl", state: "waitingApprove" as const },
    ];
    candidates.sort((a, b) => priorityOf(a.state) - priorityOf(b.state));
    expect(candidates[0].state).toBe("waitingApprove");
  });

  it("à priorité égale, le premier dans la liste est retenu (stable sort)", () => {
    const candidates = [
      { transcriptPath: "/a.jsonl", state: "error" as const },
      { transcriptPath: "/b.jsonl", state: "error" as const },
    ];
    candidates.sort((a, b) => priorityOf(a.state) - priorityOf(b.state));
    expect(candidates[0].transcriptPath).toBe("/a.jsonl");
  });
});

describe("logique highlight — condition de switch", () => {
  it("bascule vers le candidat si vue détail sur agent moins prioritaire", () => {
    const currentState = "ended";
    const candidateState = "waitingApprove";
    const shouldSwitch = priorityOf(candidateState) < priorityOf(currentState);
    expect(shouldSwitch).toBe(true);
  });

  it("ne bascule pas si vue détail sur agent plus prioritaire", () => {
    const currentState = "waitingApprove";
    const candidateState = "error";
    const shouldSwitch = priorityOf(candidateState) < priorityOf(currentState);
    expect(shouldSwitch).toBe(false);
  });

  it("ne bascule pas si vue détail sur agent de même priorité", () => {
    const currentState = "error";
    const candidateState = "error";
    const shouldSwitch = priorityOf(candidateState) < priorityOf(currentState);
    expect(shouldSwitch).toBe(false);
  });
});
