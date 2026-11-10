// Returns the time remaining until an ISO reset timestamp, e.g. "4h50m", "<1m".
// nowSec defaults to the current time so callers with a reactive clock can pass
// their own value to avoid recomputing Date.now() on every render.
export function formatTimeRemaining(iso: string, nowSec: number = Math.floor(Date.now() / 1000)): string {
  try {
    const remaining = Math.max(0, Math.floor(new Date(iso).getTime() / 1000) - nowSec);
    if (remaining < 60) return "<1m";
    const h = Math.floor(remaining / 3600);
    const m = Math.floor((remaining % 3600) / 60);
    if (h === 0) return `${m}m`;
    if (m === 0) return `${h}h`;
    return `${h}h${m}m`;
  } catch { return ""; }
}

// Date d'échéance absolue, locale-aware, ex. "Jul 15, 3:00 PM".
// `toLocaleString` ne lève PAS sur une date invalide (il renvoie "Invalid Date") : on garde
// donc explicitement le cas NaN pour retomber sur l'ISO brut plutôt qu'afficher "Invalid Date".
function formatResetDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  try {
    return d.toLocaleString(undefined, {
      month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    });
  } catch { return iso; }
}

// Libellé d'échéance partagé par la fenêtre Usage ET le tooltip de la barre Notch :
//   temps restant < 24h → décompte      ("resets in 4h50m")
//   temps restant ≥ 24h → date absolue  ("resets Jul 15, 3:00 PM")
// Évite les décomptes illisibles type "resets in 574h5m". `relative = false` force la date
// absolue même sous 24h (ex. la ligne Weekly de la fenêtre Usage, par cohérence d'affichage).
const RELATIVE_RESET_THRESHOLD_SECS = 24 * 3600;
export function formatResetLabel(
  iso: string,
  nowSec: number = Math.floor(Date.now() / 1000),
  relative = true,
): string {
  const remaining = Math.floor(new Date(iso).getTime() / 1000) - nowSec;
  if (relative && remaining > 0 && remaining < RELATIVE_RESET_THRESHOLD_SECS) {
    return `resets in ${formatTimeRemaining(iso, nowSec)}`;
  }
  return `resets ${formatResetDate(iso)}`;
}

export function formatElapsed(sinceMs: number, nowMs: number = Date.now()): string {
  const elapsed = Math.max(0, nowMs - sinceMs);
  const totalSeconds = Math.floor(elapsed / 1000);
  if (totalSeconds < 60) return "< 1m";
  const totalMinutes = Math.floor(totalSeconds / 60);
  if (totalMinutes < 60) return `${totalMinutes}m`;
  const hours = Math.floor(totalMinutes / 60);
  const mins = totalMinutes % 60;
  return mins === 0 ? `${hours}h` : `${hours}h ${mins}m`;
}
