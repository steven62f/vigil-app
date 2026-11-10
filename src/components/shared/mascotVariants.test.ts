import { describe, it, expect } from "vitest";
import {
  mascotVariants,
  MASCOT_VARIANT_OPTIONS,
  DEFAULT_MASCOT_VARIANT,
  type MascotVariantId,
  type MascotGeometry,
} from "./mascotVariants";

const SLOTS: (keyof MascotGeometry)[] = [
  "legsLeft",
  "legsRight",
  "body",
  "armLeft",
  "armRight",
  "eyes",
];

describe("mascotVariants — registre", () => {
  it("contient exactement les 5 variantes attendues (orbit 6.19, lunar 9.2)", () => {
    expect(Object.keys(mascotVariants).sort()).toEqual([
      "classic",
      "horned",
      "lunar",
      "orbit",
      "robot",
    ]);
  });

  it("orbit est présente dans le registre et dans les options (variante Antigravity)", () => {
    expect(mascotVariants.orbit).toBeDefined();
    expect(MASCOT_VARIANT_OPTIONS.some((o) => o.value === "orbit")).toBe(true);
  });

  it("lunar est présente dans le registre et dans les options (variante Kimi)", () => {
    expect(mascotVariants.lunar).toBeDefined();
    expect(MASCOT_VARIANT_OPTIONS.some((o) => o.value === "lunar")).toBe(true);
  });

  // La 5e variante doit se DISTINGUER au premier coup d'œil : sans cela, un 5e outil recevrait
  // une silhouette déjà portée par un autre et le repère visuel du FleetView se dilue.
  it("chaque variante a une géométrie distincte de toutes les autres", () => {
    const ids = Object.keys(mascotVariants) as MascotVariantId[];
    const empreintes = ids.map((id) => JSON.stringify(mascotVariants[id]));
    expect(new Set(empreintes).size).toBe(ids.length);
  });

  it("DEFAULT_MASCOT_VARIANT est classic et présent dans le registre", () => {
    expect(DEFAULT_MASCOT_VARIANT).toBe("classic");
    expect(mascotVariants[DEFAULT_MASCOT_VARIANT]).toBeDefined();
  });

  it("MASCOT_VARIANT_OPTIONS couvre toutes les variantes du registre", () => {
    const optionValues = MASCOT_VARIANT_OPTIONS.map((o) => o.value).sort();
    expect(optionValues).toEqual(Object.keys(mascotVariants).sort());
    // chaque option a un label non vide
    for (const opt of MASCOT_VARIANT_OPTIONS) {
      expect(opt.label.trim().length).toBeGreaterThan(0);
    }
  });

  it("chaque variante possède les 6 slots non vides", () => {
    for (const id of Object.keys(mascotVariants) as MascotVariantId[]) {
      const geo = mascotVariants[id];
      for (const slot of SLOTS) {
        expect(Array.isArray(geo[slot]), `${id}.${slot} doit être un tableau`).toBe(true);
        expect(geo[slot].length, `${id}.${slot} ne doit pas être vide`).toBeGreaterThan(0);
      }
    }
  });

  it("tous les rectangles restent dans la grille (viewBox 0 -1 14 9)", () => {
    for (const id of Object.keys(mascotVariants) as MascotVariantId[]) {
      const geo = mascotVariants[id];
      for (const slot of SLOTS) {
        for (const r of geo[slot]) {
          expect(r.w, `${id}.${slot} width > 0`).toBeGreaterThan(0);
          expect(r.h, `${id}.${slot} height > 0`).toBeGreaterThan(0);
          expect(r.x, `${id}.${slot} x >= 0`).toBeGreaterThanOrEqual(0);
          expect(r.x + r.w, `${id}.${slot} x+w <= 14`).toBeLessThanOrEqual(14);
          expect(r.y, `${id}.${slot} y >= -1`).toBeGreaterThanOrEqual(-1);
          expect(r.y + r.h, `${id}.${slot} y+h <= 8`).toBeLessThanOrEqual(8);
        }
      }
    }
  });

  // Garde-fou anti-régression pixel : fige la géométrie classic = AC2.
  it("invariant classic — géométrie strictement identique à la mascotte historique", () => {
    expect(mascotVariants.classic).toEqual({
      legsLeft: [
        { x: 3, y: 6, w: 1, h: 2 },
        { x: 5, y: 6, w: 1, h: 2 },
      ],
      legsRight: [
        { x: 8, y: 6, w: 1, h: 2 },
        { x: 10, y: 6, w: 1, h: 2 },
      ],
      body: [{ x: 2, y: -1, w: 10, h: 7 }],
      armLeft: [{ x: 0, y: 2, w: 2, h: 2 }],
      armRight: [{ x: 12, y: 2, w: 2, h: 2 }],
      eyes: [
        { x: 4, y: 1, w: 1, h: 2 },
        { x: 9, y: 1, w: 1, h: 2 },
      ],
    });
  });
});
