import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot } from "../../types/agent";
import {
  type ToolPermissionAction,
  type ToolPermissionKey,
  type ToolPermissions,
  loadSettings,
} from "../../lib/settingsStore";

// Story 6.35 — libellés AGNOSTIQUES de l'outil (dé-généralisation Claude). La clé
// ToolPermissionKey est inchangée ; seul le libellé est cosmétique. `setAll`/`Object.fromEntries`
// restent data-driven sur TOOL_ROWS → couvrent automatiquement les 2 lignes neuves.
const TOOL_ROWS: { key: ToolPermissionKey; label: string }[] = [
  { key: "Bash",      label: "Shell / Terminal" },
  { key: "EditWrite", label: "Edit / Write" },
  { key: "Read",      label: "Read" },
  { key: "WebFetch",  label: "Web fetch / search" },
  { key: "ReadOnly",  label: "Search / List (read-only)" },
  { key: "Mcp",       label: "MCP tools" },
  { key: "Subagent",  label: "Sub-agents" },
  { key: "Other",     label: "Other" },
];

const ACTION_OPTIONS: { value: ToolPermissionAction; label: string }[] = [
  { value: "manual",  label: "Manual" },
  { value: "approve", label: "Auto-approve" },
  { value: "deny",    label: "Auto-deny" },
];

interface Props {
  agent: AgentSnapshot;
  /**
   * Ouverture PILOTÉE par `DetailPanel` (revue 8.2). Depuis que le footer porte DEUX popovers
   * (« Notifs » et « Permissions »), tous deux ancrés `bottom:44 / right:8` en 280 px de large
   * dans un panneau de 480, deux menus ouverts se recouvraient et celui rendu en dernier masquait
   * l'autre. L'état vit donc chez le parent, qui n'en autorise qu'un à la fois.
   */
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AgentPermissionMenu({ agent, open, onOpenChange }: Props) {
  const [overrides, setOverrides] = useState<Partial<Record<ToolPermissionKey, ToolPermissionAction>>>({});
  const [globalPerms, setGlobalPerms] = useState<ToolPermissions | null>(null);
  const [loading, setLoading] = useState(false);

  const openMenu = async () => {
    setLoading(true);
    try {
      const [rules, settings] = await Promise.all([
        invoke<Record<string, ToolPermissionAction>>("get_agent_rules", {
          transcriptPath: agent.transcriptPath,
        }),
        loadSettings(),
      ]);
      setOverrides(rules as Partial<Record<ToolPermissionKey, ToolPermissionAction>>);
      setGlobalPerms(settings.toolPermissions);
      onOpenChange(true);
    } finally {
      setLoading(false);
    }
  };

  const updateRule = async (key: ToolPermissionKey, action: ToolPermissionAction) => {
    await invoke("set_agent_rule", { transcriptPath: agent.transcriptPath, key, action });
    setOverrides((prev) => ({ ...prev, [key]: action }));
  };

  const setAll = async (action: ToolPermissionAction) => {
    await invoke("set_all_agent_rules", { transcriptPath: agent.transcriptPath, action });
    const all = Object.fromEntries(TOOL_ROWS.map((r) => [r.key, action]));
    setOverrides(all);
  };

  const reset = async () => {
    await invoke("reset_agent_rules", { transcriptPath: agent.transcriptPath });
    setOverrides({});
  };

  const effectiveAction = (key: ToolPermissionKey): ToolPermissionAction =>
    overrides[key] ?? globalPerms?.[key] ?? "manual";

  const isOverridden = (key: ToolPermissionKey): boolean => key in overrides;

  if (!open) {
    return (
      <button
        onClick={openMenu}
        onMouseDown={(e) => e.stopPropagation()}
        onMouseEnter={(e) => {
          if (!loading) {
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
          }
        }}
        onMouseLeave={(e) => {
          (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface)";
          (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
        }}
        disabled={loading}
        style={{
          background: "var(--bg-surface)",
          border: "1px solid var(--border-subtle)",
          borderRadius: 5,
          color: "var(--text-secondary)",
          fontSize: 11,
          fontWeight: 500,
          padding: "4px 10px",
          cursor: loading ? "default" : "pointer",
          opacity: loading ? 0.6 : 1,
          transition: "background 100ms, color 100ms",
        }}
      >
        Permissions
      </button>
    );
  }

  return (
    <div
      onMouseDown={(e) => e.stopPropagation()}
      style={{
        position: "absolute",
        bottom: 44,
        right: 8,
        width: 280,
        background: "var(--bg-surface)",
        border: "1px solid var(--border-subtle)",
        borderRadius: 8,
        padding: 12,
        zIndex: 100,
        boxShadow: "0 4px 16px rgba(0,0,0,0.4)",
      }}
    >
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 }}>
        <span style={{ fontSize: 11, fontWeight: 600, color: "var(--text-primary)" }}>
          Permissions
        </span>
        <button
          onClick={() => onOpenChange(false)}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
          }}
          style={{ background: "none", border: "none", cursor: "pointer", color: "var(--text-muted)", fontSize: 14, padding: "0 2px", transition: "color 100ms" }}
        >
          ×
        </button>
      </div>

      <div style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 10 }}>
        {TOOL_ROWS.map(({ key, label }) => {
          const effective = effectiveAction(key);
          const overridden = isOverridden(key);
          return (
            <div key={key} style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 5, flex: 1, minWidth: 0 }}>
                <span style={{ fontSize: 11, color: "var(--text-secondary)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {label}
                </span>
                <span
                  style={{
                    fontSize: 9,
                    padding: "1px 4px",
                    borderRadius: 3,
                    background: overridden ? "var(--color-amber-bg)" : "var(--bg-overlay)",
                    color: overridden ? "var(--color-amber)" : "var(--text-muted)",
                    flexShrink: 0,
                  }}
                >
                  {overridden ? "agent" : "global"}
                </span>
              </div>
              <select
                value={effective}
                onChange={(e) => updateRule(key, e.target.value as ToolPermissionAction)}
                style={{
                  background: "var(--bg-overlay)",
                  border: "1px solid var(--border-subtle)",
                  borderRadius: 4,
                  color: "var(--text-primary)",
                  fontSize: 10,
                  padding: "2px 4px",
                  cursor: "pointer",
                  flexShrink: 0,
                }}
              >
                {ACTION_OPTIONS.map((o) => (
                  <option key={o.value} value={o.value}>{o.label}</option>
                ))}
              </select>
            </div>
          );
        })}
      </div>

      <div style={{ display: "flex", flexWrap: "wrap", gap: 4, borderTop: "1px solid var(--border-subtle)", paddingTop: 8 }}>
        {(["approve", "deny", "manual"] as ToolPermissionAction[]).map((action) => (
          <button
            key={action}
            onClick={() => setAll(action)}
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
            }}
            style={{
              background: "var(--bg-overlay)",
              border: "1px solid var(--border-subtle)",
              borderRadius: 4,
              color: "var(--text-secondary)",
              fontSize: 10,
              padding: "3px 7px",
              cursor: "pointer",
              transition: "background 100ms, color 100ms",
            }}
          >
            {action === "approve" ? "Approve all" : action === "deny" ? "Deny all" : "Manual all"}
          </button>
        ))}
        <button
          onClick={reset}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = "none";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
          }}
          style={{
            background: "none",
            border: "1px solid var(--border-subtle)",
            borderRadius: 4,
            color: "var(--text-muted)",
            fontSize: 10,
            padding: "3px 7px",
            cursor: "pointer",
            transition: "background 100ms, color 100ms",
          }}
        >
          Reset
        </button>
      </div>
    </div>
  );
}
