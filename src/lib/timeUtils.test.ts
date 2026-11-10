import { describe, it, expect } from "vitest";
import { formatElapsed } from "./timeUtils";

describe("formatElapsed", () => {
  const now = 1_000_000_000;

  it("retourne < 1m si sinceMs === nowMs (0 secondes)", () => {
    expect(formatElapsed(now, now)).toBe("< 1m");
  });

  it("retourne < 1m pour 30 secondes", () => {
    expect(formatElapsed(now - 30_000, now)).toBe("< 1m");
  });

  it("retourne 4m pour 4 minutes", () => {
    expect(formatElapsed(now - 240_000, now)).toBe("4m");
  });

  it("retourne 1h pour exactement 1 heure (0 minutes)", () => {
    expect(formatElapsed(now - 3_600_000, now)).toBe("1h");
  });

  it("retourne 1h 20m pour 1 heure 20 minutes", () => {
    expect(formatElapsed(now - 4_800_000, now)).toBe("1h 20m");
  });

  it("retourne 2h pour 2 heures exactes", () => {
    expect(formatElapsed(now - 7_200_000, now)).toBe("2h");
  });

  it("retourne < 1m si sinceMs > nowMs (elapsed négatif)", () => {
    expect(formatElapsed(now + 5_000, now)).toBe("< 1m");
  });
});
