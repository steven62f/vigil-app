export interface WindowUsage {
  id: string;               // identifiant stable de la fenêtre (attribué par le fetcher)
  label: string;            // libellé affiché (attribué par le fetcher, plus par l'UI)
  utilization: number;      // 0–100 (pourcentage, ex: 35.0)
  resetsAt: string | null;  // ISO8601, peut être null
}

// Liste ORDONNÉE de fenêtres, agnostique aux noms de champs (Story 6.15) : `windows[0]` = fenêtre
// primaire (courante), les suivantes = secondaires. L'UI boucle et rend `window.label` → une
// fenêtre d'un type inédit s'affiche automatiquement, sans mapping codé en dur.
export interface UsageStats {
  windows: WindowUsage[];
}

export interface UsageState {
  trackingEnabled: boolean;
  data: UsageStats | null;
  lastFetchUnix: number | null;
  lastError: string | null;
}

// Payload de l'événement générique `usage:state` (Story 6.13) : l'état d'un outil, ÉTIQUETÉ
// par `tool`. Chaque consommateur (Notch/UsageBar, fenêtre Usage) filtre sur son outil →
// un seul chemin push pour tous les outils (remplace l'ancien `usage:updated` Claude-only).
export interface UsageEvent {
  tool: string;
  state: UsageState;
}
