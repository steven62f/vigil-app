import { Fragment, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useSettings } from "../../hooks/useSettings";
import { useActivatedTools } from "../../hooks/useActivatedTools";
import { resolveNotchUsageTool, USAGE_SUPPORTED, type StatsTool } from "../../lib/statsTools";
import { formatResetLabel } from "../../lib/timeUtils";
import type { UsageState, UsageEvent, WindowUsage } from "../../types/usage";

export const USAGE_BAR_HEIGHT = 4;

function colorForUtilization(u: number): string {
  if (u >= 80) return "var(--usage-critical)";
  if (u >= 50) return "var(--usage-warn)";
  return "var(--usage-ok)";
}

function fmtPct(v: number): string {
  return `${Math.round(v)}%`;
}

function pctOf(u: number | undefined): number {
  return Math.min(u ?? 0, 100);
}

// Les 2 PREMIÈRES fenêtres de la liste ordonnée (plafond dur : contrainte d'espace du Notch).
// Source positionnelle générique — plus aucun nom de champ : 1 fenêtre → une seule, 0 → aucune.
// Le `.filter` reste un garde défensif (objet non-null, `utilization` numérique).
function presentWindows(data: UsageState["data"]): WindowUsage[] {
  return (data?.windows ?? [])
    .filter((w): w is WindowUsage => w != null && typeof w.utilization === "number")
    .slice(0, 2);
}

// --- État d'usage PARTAGÉ par outil (Story 6.13, patch review — option 1) ---------------
// La réservation de hauteur (`useUsageBarPosition`) ET le rendu de la barre lisent la MÊME
// source : on ne réserve la bande que lorsqu'il y a réellement des données à afficher, de
// façon strictement GÉNÉRIQUE (aucun cas spécial « claude »). Un poste Codex-seul se comporte
// exactement comme un poste Claude-seul : pas de données ⇒ pas de bande, aucun outil ne
// bénéficie d'un track transitoire réservé. Cache module-level keyé par outil + abonnement
// unique à l'événement générique `usage:state` → keyer par outil supprime aussi tout affichage
// transitoire des données du mauvais outil lors d'un changement de sélecteur.
const usageCache = new Map<StatsTool, UsageState>();
const usageSubscribers = new Set<() => void>();
const usageInFlight = new Set<StatsTool>();
let usageListenStarted = false;

function notifyUsage() {
  for (const fn of usageSubscribers) fn();
}

function ensureUsageListener() {
  if (usageListenStarted) return;
  usageListenStarted = true;
  // Un seul abonnement global : chaque event ÉTIQUETÉ alimente le cache de son outil.
  listen<UsageEvent>("usage:state", (e) => {
    usageCache.set(e.payload.tool as StatsTool, e.payload.state);
    notifyUsage();
  });
}

// Pull one-shot du cache backend (valeur immédiate au montage / changement d'outil). Dédupliqué
// par outil : plusieurs consommateurs (barre + hook de hauteur) ne déclenchent qu'un seul invoke.
function pullUsage(tool: StatsTool) {
  if (usageInFlight.has(tool)) return;
  usageInFlight.add(tool);
  invoke<UsageState>("get_usage_state", { tool })
    .then((s) => { usageCache.set(tool, s); notifyUsage(); })
    .catch(() => {})
    .finally(() => { usageInFlight.delete(tool); });
}

// État d'usage courant de l'outil résolu (ou `null` si aucun outil / pas encore de données).
function useUsageStateFor(effectiveTool: StatsTool | null): UsageState | null {
  const [, force] = useState(0);
  useEffect(() => {
    ensureUsageListener();
    const rerender = () => force((n) => n + 1);
    usageSubscribers.add(rerender);
    if (effectiveTool !== null) pullUsage(effectiveTool);
    return () => { usageSubscribers.delete(rerender); };
  }, [effectiveTool]);
  return effectiveTool !== null ? usageCache.get(effectiveTool) ?? null : null;
}

// Résout l'outil supporté à afficher dans la barre, de façon générique (Story 6.13).
// `null` tant que les outils ne sont pas chargés ou si aucun outil supporté n'est activé.
function useEffectiveTool(): StatsTool | null {
  const settings = useSettings();
  const activatedTools = useActivatedTools();
  const supported = (activatedTools ?? []).filter((t) => USAGE_SUPPORTED.includes(t));
  return resolveNotchUsageTool(settings.notchUsageTool, supported);
}

// Returns the position the bar should occupy, or null when nothing should be shown.
// Handles stale boolean values that may still be in the store.
export function useUsageBarPosition(): "top" | "bottom" | null {
  const settings = useSettings();
  const effectiveTool = useEffectiveTool();
  const usage = useUsageStateFor(effectiveTool);
  if (!settings.usageTrackingEnabled) return null;
  const v = settings.showUsageInNotch;
  if (v !== "top" && v !== "bottom") return null;
  // Réservation de hauteur pilotée par les DONNÉES, strictement générique (aucun cas Claude) :
  // on ne réserve la bande que si l'outil résolu a au moins une fenêtre exploitable. Aucun outil
  // supporté activé (effectiveTool null), ou pas (encore) de données → pas de réservation. Le
  // léger settle 4px au tout premier fetch à froid est accepté (données servies depuis le cache
  // backend, donc quasi immédiat en pratique).
  if (presentWindows(usage?.data ?? null).length === 0) return null;
  return v;
}

// Pixels each container should reserve (always additive to height regardless
// of position). Returns 0 when disabled.
export function useUsageBarHeight(): number {
  return useUsageBarPosition() !== null ? USAGE_BAR_HEIGHT : 0;
}

// Barre de consommation générique, pilotée par les DONNÉES (Story 6.13) :
// - 1 fenêtre présente → un seul segment pleine largeur (rempli gauche→droite) ;
// - 2 fenêtres présentes → deux demi-barres + séparateur central, remplissage convergent
//   (segment 0 gauche→droite, segment 1 droite→gauche).
// L'outil affiché est résolu génériquement (aucun défaut « claude » codé en dur) et l'état est
// lu depuis la source partagée `useUsageStateFor` (keyée par outil) : un seul chemin uniforme
// pour tous les outils, pull one-shot du cache + push `usage:state`. Pas de données ⇒ barre
// masquée pour TOUS les outils (identique à la hauteur ci-dessus, plus aucune asymétrie).
export function UsageBar() {
  const position = useUsageBarPosition();
  const effectiveTool = useEffectiveTool();
  const usage = useUsageStateFor(effectiveTool);

  // `position` est déjà data-driven (null si aucune donnée) → un seul garde suffit.
  if (!position) return null;

  const windows = presentWindows(usage?.data ?? null);
  if (windows.length === 0) return null; // filet défensif (cohérent avec la hauteur)

  // Échéance : décompte si < 24h, date absolue sinon (logique partagée `formatResetLabel`,
  // identique à la fenêtre Usage) → plus de « resets in 574h5m » illisible.
  // Le NOM de la fenêtre (`w.label`, attribué par le fetcher — « Gemini Models », « Current
  // window »…) préfixe chaque ligne : la position gauche/droite de la barre n'est pas parlante,
  // surtout avec les fenêtres nommées d'Antigravity. Une fenêtre par ligne (le `title` HTML
  // rend les `\n`).
  const nowSec = Math.floor(Date.now() / 1000);
  const tooltip = windows
    .map((w) => `${w.label}: ${fmtPct(w.utilization)}` + (w.resetsAt ? ` (${formatResetLabel(w.resetsAt, nowSec)})` : ""))
    .join(" | ");

  const edge = position === "top" ? { top: 0 } : { bottom: 0 };

  return (
    <div
      title={tooltip}
      style={{
        position: "absolute", top: 0, left: 0, right: 0, bottom: 0,
        borderRadius: "inherit",
        overflow: "hidden",
        pointerEvents: "none",
      }}
    >
      <div
        style={{
          position: "absolute", ...edge, left: 0, right: 0,
          height: USAGE_BAR_HEIGHT,
          display: "flex",
          pointerEvents: "auto",
        }}
      >
        {windows.map((w, i) => {
          // Remplissage convergent : la 2ᵉ moitié se remplit droite→gauche (cas 2 fenêtres).
          const fillFromRight = windows.length === 2 && i === 1;
          return (
            <Fragment key={i}>
              {i > 0 && <div style={{ width: 2, flexShrink: 0, background: "var(--bg-notch)" }} />}
              <div style={{ flex: 1, position: "relative", background: "var(--border-subtle)" }}>
                <div style={{
                  position: "absolute", top: 0, bottom: 0,
                  [fillFromRight ? "right" : "left"]: 0,
                  width: `${pctOf(w.utilization)}%`,
                  background: colorForUtilization(w.utilization),
                  transition: "width 0.3s ease",
                }} />
              </div>
            </Fragment>
          );
        })}
      </div>
    </div>
  );
}
