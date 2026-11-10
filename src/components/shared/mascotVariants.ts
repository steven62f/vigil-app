// Registre de variantes de mascotte (Story 6.9)
// Module PUR : aucune dépendance React / settings.
// L'aiguillage variante → géométrie est une simple lecture indexée (pas de switch/if
// par variante dans le JSX). Ajouter une variante = ajouter UNE entrée ici + une option
// dans MASCOT_VARIANT_OPTIONS. Rien d'autre.

// Identifiants génériques liés à la FORME, pas à l'outil (l'utilisateur assigne librement).
export type MascotVariantId = "classic" | "robot" | "horned" | "orbit" | "lunar";

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

// Slots alimentant les GROUPES animés du SVG (cf. MascotPixel).
// L'animation est appliquée aux groupes, jamais aux variantes → orthogonalité.
export interface MascotGeometry {
  legsLeft: Rect[]; // groupe <g className={legLeftClass}> — marche (working)
  legsRight: Rect[]; // groupe <g className={legRightClass}> — marche (working)
  body: Rect[]; // groupe corps masqué — bob (working). Cornes + patte centrale (grok) vont ici.
  armLeft: Rect[]; // statique, dans le groupe corps
  armRight: Rect[]; // se lève en waiting (className={armRightClass}), dans le groupe corps
  eyes: Rect[]; // creux transparents du masque (yeux / visière)
}

export const DEFAULT_MASCOT_VARIANT: MascotVariantId = "classic";

// Géométries exactes issues de la maquette ux-mascot-tool-variants-v1.html.
// Grille 14×10, viewBox "0 -1 14 9". NE PAS modifier `classic` (non-régression pixel, AC2).
export const mascotVariants: Record<MascotVariantId, MascotGeometry> = {
  // A — Claude (référence, identique au rendu historique)
  classic: {
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
  },
  // B — "robot" (bipède mécanique : pattes larges, visière unique, bras hauts).
  // Variante par défaut de l'outil Codex.
  robot: {
    legsLeft: [{ x: 2, y: 6, w: 4, h: 2 }],
    legsRight: [{ x: 8, y: 6, w: 4, h: 2 }],
    body: [{ x: 2, y: -1, w: 10, h: 7 }],
    armLeft: [{ x: 0, y: 2, w: 2, h: 3 }],
    armRight: [{ x: 12, y: 2, w: 2, h: 3 }],
    eyes: [{ x: 3, y: 1, w: 8, h: 2 }],
  },
  // C — "horned" (tripède à cornes : corps 1 px plus court, 2 cornes + patte centrale, yeux larges).
  // Variante par défaut de l'outil Grok.
  horned: {
    legsLeft: [{ x: 2, y: 6, w: 3, h: 2 }],
    legsRight: [{ x: 9, y: 6, w: 3, h: 2 }],
    body: [
      { x: 2, y: 0, w: 10, h: 6 }, // corps (1 px plus court)
      { x: 4, y: -1, w: 2, h: 1 }, // corne gauche
      { x: 8, y: -1, w: 2, h: 1 }, // corne droite
      { x: 6, y: 6, w: 2, h: 2 }, // patte centrale (statique, suit le bob du corps)
    ],
    armLeft: [{ x: 0, y: 1, w: 2, h: 3 }],
    armRight: [{ x: 12, y: 1, w: 2, h: 3 }],
    eyes: [
      { x: 3, y: 1, w: 3, h: 1 },
      { x: 8, y: 1, w: 3, h: 1 },
    ],
  },
  // D — "orbit" (droïde flottant : coussins fins détachés = lévitation, antenne satellite, yeux carrés).
  // Variante par défaut de l'outil Antigravity (Story 6.19).
  orbit: {
    legsLeft: [{ x: 2, y: 7, w: 4, h: 1 }], // demi-coussin flottant gauche (fin, détaché du corps)
    legsRight: [{ x: 8, y: 7, w: 4, h: 1 }], // demi-coussin flottant droit
    body: [
      { x: 2, y: 0, w: 10, h: 6 }, // corps (se termine en y=6 → gap avant coussins y=7)
      { x: 6, y: -1, w: 2, h: 1 }, // antenne / nub satellite (dans le groupe corps → suit le bob)
    ],
    armLeft: [{ x: 0, y: 2, w: 2, h: 2 }],
    armRight: [{ x: 12, y: 2, w: 2, h: 2 }], // se lève en waiting (className armRightClass)
    eyes: [
      { x: 4, y: 1, w: 2, h: 2 }, // œil gauche carré
      { x: 8, y: 1, w: 2, h: 2 }, // œil droit carré
    ],
  },
  // E — "lunar" (calotte en dôme, stance large, yeux en croissant).
  // Variante par défaut de l'outil Kimi (Story 9.2 — Kimi ← Moonshot AI).
  lunar: {
    legsLeft: [{ x: 2, y: 6, w: 2, h: 2 }], // stance LARGE et écartée (≠ classic/robot/horned)
    legsRight: [{ x: 10, y: 6, w: 2, h: 2 }],
    body: [
      { x: 3, y: -1, w: 8, h: 1 }, // calotte plus étroite que le corps → dôme
      { x: 2, y: 0, w: 10, h: 6 }, // corps
    ],
    armLeft: [{ x: 0, y: 1, w: 2, h: 2 }], // bras hauts et courts (position inédite)
    armRight: [{ x: 12, y: 1, w: 2, h: 2 }], // se lève en waiting (className armRightClass)
    eyes: [
      // 2 croissants diagonaux de 2 px
      { x: 4, y: 1, w: 1, h: 1 },
      { x: 5, y: 2, w: 1, h: 1 },
      { x: 9, y: 1, w: 1, h: 1 },
      { x: 8, y: 2, w: 1, h: 1 },
    ],
  },
};

// Options pour alimenter les sélecteurs visuels (page Installation). Labels UI en anglais.
export const MASCOT_VARIANT_OPTIONS: { value: MascotVariantId; label: string }[] = [
  { value: "classic", label: "Classic" },
  { value: "robot", label: "Robot" },
  { value: "horned", label: "Horned" },
  { value: "orbit", label: "Orbit" },
  { value: "lunar", label: "Lunar" },
];
