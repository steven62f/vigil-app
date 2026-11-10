import { describe, it, expect } from "vitest";
import { computeUnifiedDiff } from "./diffUtils";

describe("computeUnifiedDiff", () => {
  it("strings identiques → toutes les lignes sont 'same'", () => {
    const text = '{\n  "hooks": {}\n}';
    const result = computeUnifiedDiff(text, text);
    expect(result.every((l) => l.type === "same")).toBe(true);
    expect(result.map((l) => l.text).join("\n")).toBe(text);
  });

  it("before vide, after avec contenu → toutes les lignes sont 'added'", () => {
    const result = computeUnifiedDiff("", '{\n  "hooks": {}\n}');
    expect(result.every((l) => l.type === "added")).toBe(true);
  });

  it("after vide, before avec contenu → toutes les lignes sont 'removed'", () => {
    const result = computeUnifiedDiff('{\n  "hooks": {}\n}', "");
    expect(result.every((l) => l.type === "removed")).toBe(true);
  });

  it("ajout de lignes en fin → lignes existantes 'same', nouvelles 'added'", () => {
    const before = "ligne A\nligne B";
    const after  = "ligne A\nligne B\nligne C\nligne D";
    const result = computeUnifiedDiff(before, after);

    const same    = result.filter((l) => l.type === "same");
    const added   = result.filter((l) => l.type === "added");
    const removed = result.filter((l) => l.type === "removed");

    expect(same.map((l) => l.text)).toEqual(["ligne A", "ligne B"]);
    expect(added.map((l) => l.text)).toEqual(["ligne C", "ligne D"]);
    expect(removed).toHaveLength(0);
  });

  it("suppression de lignes → lignes retirées en 'removed'", () => {
    const before = "ligne A\nligne B\nligne C";
    const after  = "ligne A\nligne C";
    const result = computeUnifiedDiff(before, after);

    const removed = result.filter((l) => l.type === "removed");
    expect(removed.map((l) => l.text)).toEqual(["ligne B"]);
  });

  it("merge JSON — hooks tiers préservés, hooks Vigil ajoutés", () => {
    const before = JSON.stringify({
      hooks: {
        PostToolUse: [{ matcher: "Bash", hooks: [{ type: "command", command: "C:\\MonOutil\\logger.exe" }] }],
      },
    }, null, 2);

    const after = JSON.stringify({
      hooks: {
        PostToolUse: [
          { matcher: "Bash", hooks: [{ type: "command", command: "C:\\MonOutil\\logger.exe" }] },
          { matcher: "", hooks: [{ type: "command", command: "C:\\vigil\\vigil-claude.exe" }] },
        ],
        SessionStart: [{ matcher: "", hooks: [{ type: "command", command: "C:\\vigil\\vigil-claude.exe" }] }],
      },
    }, null, 2);

    const result = computeUnifiedDiff(before, after);

    const addedTexts = result.filter((l) => l.type === "added").map((l) => l.text);
    const removedTexts = result.filter((l) => l.type === "removed").map((l) => l.text);

    // Les lignes du hook tiers doivent être 'same', pas supprimées
    expect(removedTexts).toHaveLength(0);
    // Des lignes Vigil apparaissent comme 'added'
    expect(addedTexts.some((t) => t.includes("vigil-claude.exe"))).toBe(true);
    // Les lignes logger.exe sont inchangées
    expect(result.filter((l) => l.text.includes("logger.exe")).every((l) => l.type === "same")).toBe(true);
  });

  it("conserve l'ordre des lignes dans le résultat final", () => {
    const before = "A\nB\nC";
    const after  = "A\nX\nB\nY\nC";
    const result = computeUnifiedDiff(before, after);

    const texts = result.map((l) => l.text);
    expect(texts).toEqual(["A", "X", "B", "Y", "C"]);
  });
});
