import { describe, it, expect } from "vitest";
import { stateToVar } from "../components/shared/MascotPixel";
import type { AgentState } from "../types/agent";

const ALL_STATES: AgentState[] = [
  "working",
  "waitingApprove",
  "waitingSpecific",
  "ended",
  "idle",
  "error",
  "disconnected",
  "unknown",
];

describe("stateToVar", () => {
  it("working → var(--state-working)", () => {
    expect(stateToVar("working")).toBe("var(--state-working)");
  });

  it("waitingApprove → var(--state-waiting-approve)", () => {
    expect(stateToVar("waitingApprove")).toBe("var(--state-waiting-approve)");
  });

  it("waitingSpecific → var(--state-waiting-specific)", () => {
    expect(stateToVar("waitingSpecific")).toBe("var(--state-waiting-specific)");
  });

  it("ended → var(--state-ended)", () => {
    expect(stateToVar("ended")).toBe("var(--state-ended)");
  });

  it("idle → var(--state-idle)", () => {
    expect(stateToVar("idle")).toBe("var(--state-idle)");
  });

  it("error → var(--state-error)", () => {
    expect(stateToVar("error")).toBe("var(--state-error)");
  });

  it("disconnected → var(--state-disconnected)", () => {
    expect(stateToVar("disconnected")).toBe("var(--state-disconnected)");
  });

  it("unknown → var(--state-unknown)", () => {
    expect(stateToVar("unknown")).toBe("var(--state-unknown)");
  });

  it("aucune valeur hex en dur dans les résultats", () => {
    const hexPattern = /#[0-9a-fA-F]{3,8}/;
    for (const state of ALL_STATES) {
      expect(stateToVar(state)).not.toMatch(hexPattern);
    }
  });

  it("toutes les valeurs retournées sont des références var()", () => {
    for (const state of ALL_STATES) {
      const result = stateToVar(state);
      expect(result).toMatch(/^var\(--state-[a-z-]+\)$/);
    }
  });
});
