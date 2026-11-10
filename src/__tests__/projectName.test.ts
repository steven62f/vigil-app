import { describe, it, expect } from "vitest";
import { projectNameFromCwd } from "../lib/projectName";

describe("projectNameFromCwd", () => {
  it("retourne le dernier segment d'un chemin POSIX", () => {
    expect(projectNameFromCwd("/home/user/mon-projet", "fallback")).toBe("mon-projet");
  });

  it("retourne le dernier segment d'un chemin Windows", () => {
    expect(projectNameFromCwd("C:\\Users\\Dev\\dev\\vigil", "fallback")).toBe("vigil");
  });

  it("ignore les slashes en fin de chemin", () => {
    expect(projectNameFromCwd("/home/user/projet/", "fallback")).toBe("projet");
  });

  it("retourne le fallback si cwd est null", () => {
    expect(projectNameFromCwd(null, "fallback")).toBe("fallback");
  });

  it("retourne le fallback si cwd est undefined", () => {
    expect(projectNameFromCwd(undefined, "fallback")).toBe("fallback");
  });

  it("retourne le fallback si cwd est vide", () => {
    expect(projectNameFromCwd("", "fallback")).toBe("fallback");
  });

  it("retourne le fallback si cwd ne contient que des slashes", () => {
    expect(projectNameFromCwd("///", "fallback")).toBe("fallback");
  });

  it("gère les chemins mixtes / et \\", () => {
    expect(projectNameFromCwd("C:/Users/Dev\\dev/vigil", "fallback")).toBe("vigil");
  });

  it("retourne le fallback si le dernier segment est whitespace seul", () => {
    expect(projectNameFromCwd("/a/  /", "fallback")).toBe("fallback");
  });
});
