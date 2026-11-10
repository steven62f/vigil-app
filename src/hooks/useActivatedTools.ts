import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ToolsStatus } from "../types/tools";
import { activatedToolsFromStatus, type StatsTool } from "../lib/statsTools";

// Source PARTAGÉE des outils activés pour la barre Notch (Story 6.13).
// `UsageBar()`, `useUsageBarPosition()` et `useUsageBarHeight()` en ont tous besoin ;
// un cache module-level + abonnements évite 3 `get_tools_status` désynchronisés et les
// jitters de layout (un seul fetch alimente tous les consommateurs).
//
// Valeur `null` = pas encore chargé. La barre étant désormais réservée uniquement quand
// l'outil résolu a des données (cf. useUsageBarPosition, pilotée par les données), aucun
// comportement optimiste n'est nécessaire : pas d'outil connu / pas de données ⇒ pas de bande.

let cache: StatsTool[] | null = null;
const subscribers = new Set<(tools: StatsTool[]) => void>();
let started = false;

function publish(tools: StatsTool[]) {
  cache = tools;
  for (const fn of subscribers) fn(tools);
}

function fetchTools() {
  invoke<ToolsStatus>("get_tools_status")
    .then((status) => publish(activatedToolsFromStatus(status)))
    .catch(() => publish([]));
}

// Démarré une seule fois (au premier abonnement) : fetch initial + re-fetch sur retour de
// visibilité / focus + sur `vigil:settings-changed` + sur `vigil:tools-changed` — ce dernier
// émis par la page Installation à chaque (dé)activation d'outil, seul canal fiable pour le Notch
// (overlay always-on-top qui ne reçoit pas focus/visibilitychange), cf. AC6. Pas de code Rust.
function ensureStarted() {
  if (started) return;
  started = true;
  fetchTools();
  const onVisibility = () => { if (document.visibilityState === "visible") fetchTools(); };
  document.addEventListener("visibilitychange", onVisibility);
  window.addEventListener("focus", fetchTools);
  listen("vigil:settings-changed", () => fetchTools());
  listen("vigil:tools-changed", () => fetchTools());
}

export function useActivatedTools(): StatsTool[] | null {
  const [tools, setTools] = useState<StatsTool[] | null>(cache);

  useEffect(() => {
    ensureStarted();
    // Synchronise avec le cache courant si un fetch a déjà eu lieu avant ce mount.
    setTools(cache);
    subscribers.add(setTools);
    return () => { subscribers.delete(setTools); };
  }, []);

  return tools;
}
