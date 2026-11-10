import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ProjectGroup, AgentSnapshot, SourceTool } from "../../types/agent";
import type { ToolsStatus } from "../../types/tools";
import { activatedToolsFromStatus, HISTORY_SUPPORTED, toolLabel } from "../../lib/statsTools";
import { HistoryPanel } from "../../components/list/HistoryPanel";

// Les helpers d'onglets multi-outils (`statsTools`) sont génériques et réutilisés tels quels (6.10).
type HistoryTool = Exclude<SourceTool, "unknown">;

function toActiveSet(agents: AgentSnapshot[]): Set<string> {
  return new Set(
    agents
      .filter((a) => a.state !== "disconnected")
      .map((a) => a.transcriptPath)
  );
}

const messageStyle: React.CSSProperties = {
  padding: 24,
  color: "var(--text-muted)",
  textAlign: "center",
  fontSize: 13,
};

/**
 * Contenu d'un onglet outil : lit le répertoire/format propres à l'outil via get_history_grouped.
 * Outil non supporté (Grok en V1) → placeholder, AUCUN invoke (AC4). Supporté → liste via HistoryPanel
 * (gère lui-même « No history available » quand 0 session, AC5).
 */
function ToolHistoryView({
  tool,
  activeTranscriptPaths,
}: {
  tool: HistoryTool;
  activeTranscriptPaths: Set<string>;
}) {
  const [groups, setGroups] = useState<ProjectGroup[]>([]);

  // History a son propre set de support (diverge de Stats/Usage depuis Antigravity, cf. statsTools).
  const supported = HISTORY_SUPPORTED.includes(tool);

  useEffect(() => {
    if (!supported) return;
    let active = true;
    invoke<ProjectGroup[]>("get_history_grouped", { tool })
      .then((g) => { if (active) setGroups(g); })
      .catch(console.error);
    return () => { active = false; };
  }, [tool, supported]);

  if (!supported) {
    return <div style={messageStyle}>Data not available for this tool — coming soon</div>;
  }

  return (
    <HistoryPanel
      groups={groups}
      activeTranscriptPaths={activeTranscriptPaths}
      tool={tool}
    />
  );
}

function TabBar({
  tabs,
  active,
  onSelect,
}: {
  tabs: HistoryTool[];
  active: HistoryTool;
  onSelect: (t: HistoryTool) => void;
}) {
  return (
    <div
      style={{
        display: "flex",
        gap: 4,
        borderBottom: "1px solid var(--border-subtle)",
        padding: "20px 20px 0",
        flexShrink: 0,
      }}
    >
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

export function HistoryApp() {
  const [activatedTools, setActivatedTools] = useState<HistoryTool[] | null>(null);
  const [activeTab, setActiveTab] = useState<HistoryTool>("claude_code");
  const [activeTranscriptPaths, setActiveTranscriptPaths] = useState<Set<string>>(new Set());

  // Reconstruit la liste d'onglets depuis get_tools_status (mount + retour de visibilité, AC6).
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

  // Si l'onglet actif n'existe plus après reconstruction (outil désactivé) → premier onglet (AC6).
  useEffect(() => {
    if (!activatedTools) return;
    if (!activatedTools.includes(activeTab)) {
      setActiveTab(activatedTools[0]);
    }
  }, [activatedTools, activeTab]);

  // Poll global des sessions actives (2s) — inchangé, indépendant de l'onglet courant.
  useEffect(() => {
    const refresh = () => {
      invoke<AgentSnapshot[]>("get_agents_snapshot")
        .then((agents) => setActiveTranscriptPaths(toActiveSet(agents)))
        .catch(console.error);
    };
    refresh();
    const interval = setInterval(refresh, 2000);
    return () => clearInterval(interval);
  }, []);

  if (!activatedTools) return null;

  // Barre d'onglets TOUJOURS rendue (AC1) ; un onglet par outil, PAS d'onglet « All » (cf. Dev Notes).
  const tabs = activatedTools;
  // Onglet effectif dérivé au render : si `activeTab` n'est plus valide (outil désactivé), on rend
  // immédiatement le premier onglet — sans attendre le useEffect (pas d'onglet fantôme, AC6).
  const effectiveTab: HistoryTool = tabs.includes(activeTab) ? activeTab : activatedTools[0];

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        background: "var(--bg-overlay)",
        color: "var(--text-primary)",
      }}
    >
      <TabBar tabs={tabs} active={effectiveTab} onSelect={setActiveTab} />
      <div style={{ flex: 1, minHeight: 0 }}>
        {/* `key` force le remount au changement d'outil → réinitialise la liste (pattern 6.10). */}
        <ToolHistoryView
          key={effectiveTab}
          tool={effectiveTab}
          activeTranscriptPaths={activeTranscriptPaths}
        />
      </div>
    </div>
  );
}
