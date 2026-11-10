import { describe, it, expect } from "vitest";
import { fmtCost, fmtTokens, fmtDayLabel, barColor } from "./StatsApp";

describe("fmtCost", () => {
  it("retourne — pour 0", () => {
    expect(fmtCost(0)).toBe("—");
  });

  it("retourne — en dessous du seuil (0.004)", () => {
    expect(fmtCost(0.004)).toBe("—");
  });

  it("retourne ~$0.01 exactement au seuil (0.005)", () => {
    expect(fmtCost(0.005)).toBe("~$0.01");
  });

  it("retourne ~$1.23 pour 1.23", () => {
    expect(fmtCost(1.23)).toBe("~$1.23");
  });

  it("retourne ~$10.00 pour 10", () => {
    expect(fmtCost(10)).toBe("~$10.00");
  });
});

describe("fmtTokens", () => {
  it("retourne la valeur brute en dessous de 1 000", () => {
    expect(fmtTokens(0)).toBe("0");
    expect(fmtTokens(999)).toBe("999");
  });

  it("retourne 1k à partir de 1 000", () => {
    expect(fmtTokens(1_000)).toBe("1k");
  });

  it("retourne 999k juste en dessous du million", () => {
    expect(fmtTokens(999_499)).toBe("999k");
  });

  it("retourne 1.0m à partir de 1 000 000", () => {
    expect(fmtTokens(1_000_000)).toBe("1.0m");
  });

  it("retourne 1.5m pour 1 500 000", () => {
    expect(fmtTokens(1_500_000)).toBe("1.5m");
  });
});

describe("fmtDayLabel", () => {
  it("formate correctement un lundi", () => {
    expect(fmtDayLabel("2026-05-18")).toBe("Mon 18");
  });

  it("formate correctement un vendredi en fin de mois", () => {
    expect(fmtDayLabel("2026-05-01")).toBe("Fri 1");
  });
});

describe("barColor", () => {
  it("ratio 0 → vert sombre hsl(142, 40%, 28%)", () => {
    expect(barColor(0)).toBe("hsl(142, 40%, 28%)");
  });

  it("ratio 1 → vert vif hsl(142, 72%, 58%)", () => {
    expect(barColor(1)).toBe("hsl(142, 72%, 58%)");
  });

  it("ratio 0.5 → vert intermédiaire hsl(142, 56%, 43%)", () => {
    expect(barColor(0.5)).toBe("hsl(142, 56%, 43%)");
  });

  it("la luminosité augmente avec le ratio", () => {
    const low = barColor(0.2);
    const high = barColor(0.8);
    const lLow = parseInt(low.match(/(\d+)%\)/)![1]);
    const lHigh = parseInt(high.match(/(\d+)%\)/)![1]);
    expect(lHigh).toBeGreaterThan(lLow);
  });
});
