import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { LocalStats } from "../../types/stats";
import type { SourceTool } from "../../types/agent";
import type { ToolsStatus } from "../../types/tools";
import { fmtCost, fmtTokens } from "../../lib/formatStats";
import { activatedToolsFromStatus, aggregateAllTools, LOCAL_STATS_SUPPORTED, toolLabel } from "../../lib/statsTools";

export { fmtCost, fmtTokens };

type StatsTool = Exclude<SourceTool, "unknown">;
/** Onglet : un outil activé, ou l'agrégat « All ». */
type Tab = StatsTool | "all";

export function fmtDayLabel(date: string): string {
  const d = new Date(date + "T12:00:00Z");
  const day = d.toLocaleDateString("en-US", { weekday: "short", timeZone: "UTC" });
  const num = d.getUTCDate();
  return `${day} ${num}`;
}

function StatBlock({ label, cost, tokens }: { label: string; cost?: number; tokens: number }) {
  return (
    <div style={{ flex: 1 }}>
      <div style={{ fontSize: 11, color: "var(--text-muted)", textTransform: "uppercase", letterSpacing: "0.05em", marginBottom: 8 }}>
        {label}
      </div>
      <div style={{ display: "flex", gap: 20 }}>
        {cost !== undefined && (
          <div>
            <div style={{ fontSize: 22, fontWeight: 600 }}>{fmtCost(cost)}</div>
            <div style={{ fontSize: 12, color: "var(--text-muted)" }}>estimated cost</div>
          </div>
        )}
        <div>
          <div style={{ fontSize: 22, fontWeight: 600 }}>{fmtTokens(tokens)}</div>
          <div style={{ fontSize: 12, color: "var(--text-muted)" }}>tokens</div>
        </div>
      </div>
    </div>
  );
}

export function barColor(ratio: number): string {
  // Vert sombre (faible conso) → vert vif #4ade80 (maximum)
  const h = 142;
  const s = Math.round(40 + ratio * 32); // 40% → 72%
  const l = Math.round(28 + ratio * 30); // 28% → 58%
  return `hsl(${h}, ${s}%, ${l}%)`;
}

/** Une barre du graphe : valeur de hauteur (coût OU tokens), label formaté, tooltip optionnel. */
interface BarDatum {
  date: string;
  value: number;
  label: string;
  tooltip?: string;
}

/**
 * Graphe à barres 7 jours, paramétré par la métrique de hauteur.
 * Onglets outil → métrique = coût ; onglet « All » → métrique = tokens. Composant unique réutilisé.
 */
function BarChart({ data }: { data: BarDatum[] }) {
  const maxValue = Math.max(...data.map(d => d.value), 0.001);
  const BAR_MAX_HEIGHT = 100;

  return (
    <div style={{ display: "flex", alignItems: "flex-end", gap: 8, height: BAR_MAX_HEIGHT + 52, padding: "0 4px" }}>
      {data.map((d) => {
        const ratio = d.value / maxValue;
        const barH = d.value > 0 ? Math.max(4, Math.round(ratio * BAR_MAX_HEIGHT)) : 0;
        return (
          <div key={d.date} style={{ flex: 1, display: "flex", flexDirection: "column", alignItems: "center", gap: 6 }}>
            <div style={{ height: BAR_MAX_HEIGHT, width: "100%", display: "flex", alignItems: "flex-end" }}>
              {barH > 0 ? (
                <div
                  title={d.tooltip}
                  style={{
                    width: "100%",
                    height: barH,
                    background: barColor(ratio),
                    borderRadius: "3px 3px 0 0",
                    cursor: "default",
                    transition: "height 0.2s ease",
                  }}
                />
              ) : (
                <div style={{ width: "100%", height: 1, background: "var(--border-subtle)" }} />
              )}
            </div>
            <div style={{ fontSize: 12, color: "var(--text-muted)", textAlign: "center", lineHeight: 1.4 }}>
              <div>{fmtDayLabel(d.date)}</div>
              <div style={{ color: d.value > 0 ? "var(--text-primary)" : "var(--text-muted)" }}>
                {d.label}
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );
}

const messageStyle: React.CSSProperties = { padding: 24, color: "var(--text-muted)", textAlign: "center" };

/** Contenu d'un onglet outil : invoke get_local_stats(tool), totaux + graphe (métrique = coût). */
function ToolStatsView({ tool }: { tool: StatsTool }) {
  const [stats, setStats] = useState<LocalStats | null>(null);
  const [loading, setLoading] = useState(true);

  // Outil non supporté (Grok en V1) → placeholder, AUCUN invoke (AC4).
  const supported = LOCAL_STATS_SUPPORTED.includes(tool);

  useEffect(() => {
    if (!supported) { setLoading(false); return; }
    let active = true;
    setLoading(true);
    invoke<LocalStats>("get_local_stats", { tool })
      .then((s) => { if (active) setStats(s); })
      .catch(() => { if (active) setStats(null); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [tool, supported]);

  if (!supported) {
    return <div style={messageStyle}>Data not available for this tool — coming soon</div>;
  }
  if (loading) return null;

  if (!stats || stats.days.length === 0 || (stats.totalCostUsd === 0 && stats.totalTokens === 0)) {
    return <div style={messageStyle}>No data available</div>;
  }

  const today = stats.days[stats.days.length - 1];
  const todayTokens = today.inputTokens + today.outputTokens + today.cacheTokens;
  const data: BarDatum[] = stats.days.map((day) => ({
    date: day.date,
    value: day.costUsd,
    label: fmtCost(day.costUsd),
    tooltip: day.costUsd > 0
      ? `in: ${fmtTokens(day.inputTokens)} · out: ${fmtTokens(day.outputTokens)} · cache: ${fmtTokens(day.cacheTokens)}`
      : undefined,
  }));

  return (
    <>
      <div style={{ display: "flex", marginBottom: 28 }}>
        <StatBlock label="Last 7 days" cost={stats.totalCostUsd} tokens={stats.totalTokens} />
        <div style={{ width: 1, background: "var(--border-subtle)", margin: "0 20px" }} />
        <StatBlock label="Today" cost={today.costUsd} tokens={todayTokens} />
      </div>
      <BarChart data={data} />
    </>
  );
}

/**
 * Onglet « All » : volume total de tokens toutes sources confondues, SANS coût (AC6).
 * Agrégation côté frontend : un invoke par outil supporté & activé, puis aggregateAllTools.
 */
function AllToolsView({ tools }: { tools: StatsTool[] }) {
  const [agg, setAgg] = useState<{ date: string; tokens: number }[] | null>(null);
  const [loading, setLoading] = useState(true);

  const supportedTools = tools.filter((t) => LOCAL_STATS_SUPPORTED.includes(t));

  useEffect(() => {
    let active = true;
    setLoading(true);
    Promise.all(
      supportedTools.map((tool) =>
        invoke<LocalStats>("get_local_stats", { tool }).catch(() => null)
      )
    )
      .then((results) => {
        if (!active) return;
        const valid = results.filter((r): r is LocalStats => r !== null);
        setAgg(aggregateAllTools(valid));
      })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tools.join(",")]);

  if (loading) return null;

  const total = agg?.reduce((sum, d) => sum + d.tokens, 0) ?? 0;
  if (!agg || total === 0) {
    return <div style={messageStyle}>No data available</div>;
  }

  const data: BarDatum[] = agg.map((d) => ({
    date: d.date,
    value: d.tokens,
    label: fmtTokens(d.tokens),
    tooltip: d.tokens > 0 ? `${fmtTokens(d.tokens)} tokens` : undefined,
  }));

  return (
    <>
      <div style={{ display: "flex", marginBottom: 28 }}>
        <StatBlock label="Last 7 days — all tools" tokens={total} />
      </div>
      <BarChart data={data} />
    </>
  );
}

function TabBar({ tabs, active, onSelect }: { tabs: Tab[]; active: Tab; onSelect: (t: Tab) => void }) {
  return (
    <div style={{ display: "flex", gap: 4, borderBottom: "1px solid var(--border-subtle)", marginBottom: 20 }}>
      {tabs.map((tab) => {
        const isActive = tab === active;
        const label = tab === "all" ? "All" : toolLabel(tab);
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
            {label}
          </button>
        );
      })}
    </div>
  );
}

export function StatsApp() {
  const [activatedTools, setActivatedTools] = useState<StatsTool[] | null>(null);
  const [activeTab, setActiveTab] = useState<Tab>("claude_code");

  // Reconstruit la liste d'onglets depuis get_tools_status (mount + retour de visibilité, AC7).
  const loadTools = () => {
    invoke<ToolsStatus>("get_tools_status")
      .then((status) => {
        const tools = activatedToolsFromStatus(status);
        // Cas dégénéré (aucun outil activé) : retomber sur Claude Code par défaut, sans crash.
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

  // Si l'onglet actif n'existe plus après reconstruction (outil désactivé) → retomber sur le premier (AC7).
  useEffect(() => {
    if (!activatedTools) return;
    const validTabs: Tab[] = activatedTools.length > 1 ? [...activatedTools, "all"] : [...activatedTools];
    if (!validTabs.includes(activeTab)) {
      setActiveTab(activatedTools[0]);
    }
  }, [activatedTools, activeTab]);

  if (!activatedTools) return null;

  // Barre d'onglets TOUJOURS rendue (AC1). Onglet « All » en dernier, uniquement si > 1 outil (AC6).
  const tabs: Tab[] = activatedTools.length > 1 ? [...activatedTools, "all"] : [...activatedTools];

  // Onglet effectif dérivé au render : si `activeTab` n'est plus valide (outil désactivé), on rend
  // immédiatement le premier onglet — sans attendre le useEffect post-render (pas d'onglet fantôme
  // ni d'invoke superflu sur l'outil retiré, AC7). L'effet ci-dessus se contente de synchroniser l'état.
  const effectiveTab: Tab = tabs.includes(activeTab) ? activeTab : activatedTools[0];

  return (
    <div style={{ padding: "20px 16px", color: "var(--text-primary)", background: "var(--bg-overlay)", minHeight: "100vh" }}>
      <TabBar tabs={tabs} active={effectiveTab} onSelect={setActiveTab} />
      {effectiveTab === "all"
        ? <AllToolsView tools={activatedTools} />
        // `key` force le remount au changement d'outil → réinitialise stats/loading (pas de flash
        // des données de l'onglet précédent le temps que le fetch du nouvel outil se résolve).
        : <ToolStatsView key={effectiveTab} tool={effectiveTab} />}
    </div>
  );
}
