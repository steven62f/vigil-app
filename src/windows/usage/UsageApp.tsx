import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { UsageState, UsageEvent, WindowUsage } from "../../types/usage";
import type { SourceTool } from "../../types/agent";
import type { ToolsStatus } from "../../types/tools";
import { formatResetLabel } from "../../lib/timeUtils";
import { activatedToolsFromStatus, toolLabel, USAGE_SUPPORTED } from "../../lib/statsTools";

type UsageTool = Exclude<SourceTool, "unknown">;

function colorForPct(u: number): string {
  if (u >= 80) return "var(--usage-critical)";
  if (u >= 50) return "var(--usage-warn)";
  return "var(--usage-ok)";
}

function fmtPct(u: number): string {
  return `${Math.round(u)}%`;
}

function fmtLastFetch(unix: number | null, nowSec: number): string {
  if (!unix) return "";
  const s = Math.max(0, nowSec - unix);
  if (s < 60) return "Updated just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `Updated ${m} min ago`;
  const h = Math.floor(m / 60);
  return `Updated ${h}h ago`;
}

// Messages d'erreur clairs, avec un indice de reconnexion adapté à l'outil (AC4).
// Le libellé Claude reste byte-identique à aujourd'hui (tool === "claude_code").
function fmtError(e: string, hasCached: boolean, tool: UsageTool): string {
  const isCodex = tool === "codex";
  // Antigravity : la source est le serveur local du process agy (Story 6.24). Fermé → cache.
  if (e === "agy_not_running")
    return hasCached
      ? "Antigravity is closed — showing cached usage. Refresh to fetch live."
      : "Antigravity is closed — open agy (or Refresh) to fetch your usage.";
  // agy tourne mais ne répond pas encore (démarrage, ou port gRPC/HTTPS-only) : distinct de « fermé ».
  if (e === "agy_unreachable")
    return hasCached
      ? "Antigravity isn't responding — showing cached usage. Refresh to retry."
      : "Antigravity is running but not responding yet — Refresh to retry.";
  if (e === "agy_not_found" || e.startsWith("spawn_error"))
    return "Couldn't launch Antigravity to refresh usage.";
  const loginHint = isCodex ? "run `codex login`" : "open Claude Code";
  if (e === "token_expired") return `Token expired — ${loginHint} to refresh it`;
  if (e.startsWith("api_error_401") || e.startsWith("api_error_403"))
    return isCodex ? "Authentication failed — check your ChatGPT plan" : "Authentication failed (Pro/Max only)";
  if (e.startsWith("api_error_")) return `API error ${e.slice(10)}`;
  if (e.startsWith("network_error")) return "Network error — check your connection";
  if (e.startsWith("credentials_not_found")) return `Credentials not found — ${loginHint} to log in`;
  // Réponse illisible / enveloppe dérivée (endpoint volatile) : distinguer d'une simple absence.
  if (e.startsWith("parse_error") || e.startsWith("read_error"))
    return hasCached ? "Using cached data" : "Usage data unavailable — the service response changed";
  return hasCached ? "Using cached data" : "Data unavailable";
}

function Gauge({ utilization }: { utilization: number }) {
  const pct = Math.min(utilization, 100);
  return (
    <div style={{ flex: 1, height: 6, background: "var(--border-subtle)", borderRadius: 3, overflow: "hidden" }}>
      <div style={{
        width: `${pct}%`, height: "100%",
        background: colorForPct(utilization),
        transition: "width 0.3s ease",
      }} />
    </div>
  );
}

function UsageRow({ label, window, nowSec, relativeReset }: {
  label: string;
  window: WindowUsage;
  nowSec?: number;
  relativeReset?: boolean;
}) {
  // Décompte court seulement pour les échéances proches (< 24h, ex. fenêtre 5h) ;
  // au-delà (fenêtre 7j/mensuelle), date absolue — plus lisible. Logique partagée avec le
  // tooltip de la barre Notch (`formatResetLabel`). `relativeReset` non passé (Weekly) → date.
  const resetLabel = window.resetsAt
    ? formatResetLabel(window.resetsAt, nowSec, relativeReset ?? false)
    : undefined;

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
      <div style={{ display: "flex", justifyContent: "space-between", fontSize: 13 }}>
        <span style={{ color: "var(--text-secondary)" }}>{label}</span>
        <span style={{ color: colorForPct(window.utilization), fontWeight: 600 }}>
          {fmtPct(window.utilization)}
        </span>
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <Gauge utilization={window.utilization} />
        <span style={{
          fontSize: 12, color: "var(--text-muted)", whiteSpace: "nowrap",
          width: 124, flexShrink: 0, textAlign: "right",
        }}>
          {resetLabel ?? ""}
        </span>
      </div>
    </div>
  );
}

const btnStyle = {
  padding: "4px 12px",
  background: "var(--bg-subtle)",
  color: "var(--text-secondary)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 6,
  cursor: "pointer",
  fontSize: 12,
} as const;

const messageStyle: React.CSSProperties = { margin: 0, fontSize: 13, color: "var(--text-muted)" };

/**
 * Contenu d'un onglet outil. Outil supporté → logique existante paramétrée par `tool`
 * (pull `get_usage_state(tool)`, refresh `refresh_usage(tool)`). Outil non supporté
 * (Grok en V1) → placeholder « coming soon », AUCUN invoke (AC5).
 */
function ToolUsageView({ tool }: { tool: UsageTool }) {
  const supported = USAGE_SUPPORTED.includes(tool);
  const [state, setState] = useState<UsageState | null>(null);
  const [nowSec, setNowSec] = useState(() => Math.floor(Date.now() / 1000));
  const [refreshing, setRefreshing] = useState(false);
  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pullRef = useRef<() => void>(() => {});

  useEffect(() => {
    if (!supported) return; // Grok : jamais d'appel backend (AC5).
    let cancelled = false;

    const pull = () => {
      invoke<UsageState>("get_usage_state", { tool })
        .then((s) => { if (!cancelled) setState(s); })
        .catch(console.error);
    };
    pullRef.current = pull;
    pull();

    // Push générique ÉTIQUETÉ (Story 6.13) : le backend émet `usage:state` pour TOUS les
    // outils → chaque onglet ne garde que le sien. Remplace l'ancien `usage:updated`
    // Claude-only ; l'onglet Codex reçoit enfin le push (plus de dépendance au poll).
    const unlistenData = listen<UsageEvent>("usage:state", (e) => {
      if (!cancelled && e.payload.tool === tool) {
        setState(e.payload.state);
        setRefreshing(false);
      }
    });

    // Changements de Settings (toggle global) → re-pull l'état Rust.
    const unlistenSettings = listen("vigil:settings-changed", () => pull());

    // Re-sync au retour de visibilité/focus (filet peu coûteux : lit le cache backend).
    const onVisibility = () => { if (document.visibilityState === "visible") pull(); };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", pull);

    // Tick horloge pour l'étiquette « Updated X min ago ». Les DONNÉES arrivent désormais
    // par push (`usage:state`) → plus de re-pull périodique nécessaire ici.
    const clockId = setInterval(() => setNowSec(Math.floor(Date.now() / 1000)), 30_000);

    return () => {
      cancelled = true;
      unlistenData.then((fn) => fn());
      unlistenSettings.then((fn) => fn());
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", pull);
      clearInterval(clockId);
      if (refreshTimerRef.current !== null) clearTimeout(refreshTimerRef.current);
    };
  }, [tool, supported]);

  const handleRefresh = async () => {
    if (refreshTimerRef.current !== null) clearTimeout(refreshTimerRef.current);
    setRefreshing(true);
    try { await invoke("refresh_usage", { tool }); } catch { setRefreshing(false); return; }
    // Le fetch émet `usage:state` (générique) → l'abonnement remet refreshing=false et met à
    // jour l'état. Filet de sécurité pour le cas « cooldown » (fetch court-circuité, aucun
    // événement émis) : on re-pull et on lève le flag après un court délai.
    refreshTimerRef.current = setTimeout(() => {
      pullRef.current();
      setRefreshing(false);
    }, 1200);
  };

  if (!supported) {
    return <p style={messageStyle}>Data not available for this tool — coming soon</p>;
  }

  if (state === null) {
    return <p style={messageStyle}>Loading…</p>;
  }

  if (!state.trackingEnabled) {
    return (
      <p style={{ margin: 0, fontSize: 13, color: "var(--text-secondary)" }}>
        Usage tracking is disabled. Enable it in Settings to fetch your usage quota.
      </p>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      {state.lastError && (
        <p style={{
          margin: 0, fontSize: 12,
          color: state.data ? "var(--text-muted)" : "var(--color-danger)",
        }}>
          {fmtError(state.lastError, !!state.data, tool)}
        </p>
      )}

      {state.data && (
        <>
          {/* Boucle GÉNÉRIQUE sur la liste ordonnée : le label vient de `w.label` (attribué par
              le fetcher), plus aucun mapping champ→ligne codé en dur → une fenêtre d'un type
              inédit s'affiche automatiquement (AC4). Décompte relatif UNIQUEMENT sur la 1re
              fenêtre (primaire/courante) ; les suivantes → date absolue (comportement préservé). */}
          {(state.data.windows ?? []).map((w, i) => (
            <UsageRow key={w.id} label={w.label} window={w} nowSec={nowSec} relativeReset={i === 0} />
          ))}
        </>
      )}

      {!state.data && !state.lastError && (
        <p style={{ margin: 0, fontSize: 12, color: "var(--text-muted)" }}>
          Waiting for first fetch…
        </p>
      )}

      <div style={{
        display: "flex", justifyContent: "space-between", alignItems: "center",
        paddingTop: 8, borderTop: "1px solid var(--border-subtle)",
      }}>
        <span style={{ fontSize: 11, color: "var(--text-muted)" }}>
          {fmtLastFetch(state.lastFetchUnix, nowSec)}
        </span>
        <button
          style={{ ...btnStyle, opacity: refreshing ? 0.6 : 1 }}
          disabled={refreshing}
          onClick={handleRefresh}
        >
          {refreshing ? "Refreshing…" : "Refresh"}
        </button>
      </div>
    </div>
  );
}

function TabBar({ tabs, active, onSelect }: { tabs: UsageTool[]; active: UsageTool; onSelect: (t: UsageTool) => void }) {
  return (
    <div style={{ display: "flex", gap: 4, borderBottom: "1px solid var(--border-subtle)", marginBottom: 16 }}>
      {tabs.map((tab) => {
        const isActive = tab === active;
        return (
          <button
            key={tab}
            type="button"
            onClick={() => onSelect(tab)}
            style={{
              padding: "8px 14px",
              background: "transparent",
              color: isActive ? "var(--text-primary)" : "var(--text-secondary)",
              border: "none",
              borderBottom: isActive ? "2px solid var(--color-amber)" : "2px solid transparent",
              cursor: "pointer",
              fontSize: 13,
              fontFamily: "inherit",
              marginBottom: -1,
            }}
          >
            {toolLabel(tab)}
          </button>
        );
      })}
    </div>
  );
}

export function UsageApp() {
  const [activatedTools, setActivatedTools] = useState<UsageTool[] | null>(null);
  const [activeTab, setActiveTab] = useState<UsageTool>("claude_code");

  // Reconstruit la liste d'onglets depuis get_tools_status (mount + retour de visibilité, AC6).
  const loadTools = () => {
    invoke<ToolsStatus>("get_tools_status")
      .then((status) => {
        const tools = activatedToolsFromStatus(status);
        setActivatedTools(tools.length > 0 ? tools : ["claude_code"]);
      })
      .catch(() => setActivatedTools(["claude_code"]));
  };

  useEffect(() => {
    loadTools();
    const onVisible = () => { if (document.visibilityState === "visible") loadTools(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { document.removeEventListener("visibilitychange", onVisible); };
  }, []);

  // Si l'onglet actif n'existe plus après reconstruction (outil désactivé) → premier onglet (AC6).
  useEffect(() => {
    if (!activatedTools) return;
    if (!activatedTools.includes(activeTab)) {
      setActiveTab(activatedTools[0]);
    }
  }, [activatedTools, activeTab]);

  if (!activatedTools) return null;

  // Pas d'onglet « All » (quota live par outil, incomparable — cf. Dev Notes).
  const tabs = activatedTools;

  // Onglet effectif dérivé au render : pas d'onglet fantôme ni d'invoke superflu sur
  // l'outil retiré (AC6, pattern 6.10/6.11).
  const effectiveTab: UsageTool = tabs.includes(activeTab) ? activeTab : activatedTools[0];

  return (
    <div style={{
      padding: 16,
      color: "var(--text-primary)",
      background: "var(--bg-overlay)",
      minHeight: "100vh",
      boxSizing: "border-box",
    }}>
      <h2 style={{ margin: "0 0 12px", fontSize: 16, fontWeight: 600 }}>Usage</h2>
      <TabBar tabs={tabs} active={effectiveTab} onSelect={setActiveTab} />
      {/* `key` force le remount au changement d'outil → réinitialise l'état (pas de flash
          des données de l'onglet précédent, pattern 6.10/6.11). */}
      <ToolUsageView key={effectiveTab} tool={effectiveTab} />
    </div>
  );
}
