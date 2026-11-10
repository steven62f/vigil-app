import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot } from "../../types/agent";
import { MascotPixel, stateToVar } from "../shared/MascotPixel";
import { SubagentDots } from "../shared/SubagentDots";
import { SourceBadge } from "./SourceBadge";
import { ToolBadge } from "../shared/ToolBadge";
import { formatElapsed } from "../../lib/timeUtils";
import { toStateLabel } from "../../lib/agentDisplay";
import { useSettings } from "../../hooks/useSettings";
import { mascotForTool } from "../../lib/settingsStore";

interface Props {
  agent: AgentSnapshot;
  onApprove: () => void;
  onDeny: () => void;
  onRowClick: () => void;
  onDismiss: () => void;
}

export function AgentRow({ agent, onApprove, onDeny, onRowClick, onDismiss }: Props) {
  const settings = useSettings();
  const prioColor =
    agent.state === "waitingApprove" || agent.state === "waitingSpecific"
      ? "var(--state-waiting-approve)"
      : agent.state === "error"
      ? "var(--state-error)"
      : null;

  const isWaiting =
    agent.state === "waitingApprove" || agent.state === "waitingSpecific";

  const timeColor = isWaiting ? stateToVar(agent.state) : "var(--text-muted)";

  const handleTerminalClick = () => {
    if (agent.terminalPid == null) return;
    invoke("focus_terminal", { pid: agent.terminalPid }).catch(console.error);
  };

  return (
    <div
      onMouseDown={(e) => e.stopPropagation()}
      onClick={onRowClick}
      style={{
        position: "relative",
        display: "flex",
        alignItems: "center",
        gap: 10,
        padding: "9px 18px",
        cursor: "pointer",
        transition: "background 120ms",
        borderBottom: "1px solid var(--border-row)",
      }}
      onMouseEnter={(e) => {
        (e.currentTarget as HTMLDivElement).style.background = "var(--bg-row-hover)";
      }}
      onMouseLeave={(e) => {
        (e.currentTarget as HTMLDivElement).style.background = "transparent";
      }}
    >
      {prioColor && (
        <div
          style={{
            position: "absolute",
            left: 0,
            top: 8,
            bottom: 8,
            width: 2,
            borderRadius: "0 2px 2px 0",
            background: prioColor,
          }}
        />
      )}

      <MascotPixel state={agent.state} size={32} interruptPending={agent.interruptPending ?? false} variant={mascotForTool(agent.sourceTool, settings)} />

      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 5, flexWrap: "nowrap", overflow: "hidden" }}>
          <span
            style={{
              display: "inline-flex",
              alignItems: "center",
              height: 18,
              fontSize: 13,
              fontWeight: 500,
              lineHeight: 1.3,
              color: "var(--text-primary)",
              whiteSpace: "nowrap",
              flexShrink: 0,
            }}
          >
            {agent.name}
          </span>
          <span
            style={{
              display: "inline-flex",
              alignItems: "center",
              height: 18,
              fontSize: 9,
              lineHeight: 1,
              color: stateToVar(agent.state),
              flexShrink: 0,
            }}
          >
            {toStateLabel(agent.state)}
          </span>
          {agent.tags?.map((tag) => (
            <span
              key={tag}
              style={{
                background: "rgba(255,255,255,0.08)",
                color: "var(--text-primary)",
                fontFamily: "monospace",
                fontSize: 10,
                lineHeight: 1,
                display: "inline-flex",
                alignItems: "center",
                height: 18,
                borderRadius: 3,
                padding: "0 5px",
                flexShrink: 0,
              }}
            >
              {tag}
            </span>
          ))}
          {settings.showToolBadge && <ToolBadge sourceTool={agent.sourceTool} />}
          <SourceBadge
            source={agent.source}
            onTerminalClick={
              agent.terminalPid != null ? handleTerminalClick : undefined
            }
          />
          {(agent.subagents?.length ?? 0) > 0 && (
            <SubagentDots subagents={agent.subagents!} />
          )}
        </div>

        <div
          style={{
            fontSize: 11,
            color: "var(--text-muted)",
            marginTop: 2,
            maxWidth: 320,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {agent.lastActivityLabel ?? ""}
        </div>
      </div>

      <span style={{ fontSize: 10, color: timeColor, flexShrink: 0 }}>
        {formatElapsed(agent.lastActivity)}
      </span>

      {agent.state === "disconnected" && (
        <button
          title="Dismiss"
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => { e.stopPropagation(); onDismiss(); }}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover)";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface)";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
          }}
          style={{
            fontSize: 11,
            fontWeight: 700,
            padding: "3px 9px",
            borderRadius: 5,
            border: "1px solid var(--border-subtle)",
            background: "var(--bg-surface)",
            color: "var(--text-muted)",
            cursor: "pointer",
            transition: "background 100ms, color 100ms",
          }}
        >
          ×
        </button>
      )}

      {agent.state === "waitingApprove" && (
        <div style={{ display: "flex", gap: 5, flexShrink: 0 }}>
          <button
            title="Approve"
            onMouseDown={(e) => {
              e.stopPropagation();
              (e.currentTarget as HTMLButtonElement).style.opacity = "0.6";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(0.94)";
            }}
            onMouseUp={(e) => {
              (e.currentTarget as HTMLButtonElement).style.opacity = "1";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(1)";
            }}
            onClick={(e) => { e.stopPropagation(); onApprove(); }}
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--color-success-hover)";
              (e.currentTarget as HTMLButtonElement).style.borderColor = "var(--color-success-hover)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--color-success-bg)";
              (e.currentTarget as HTMLButtonElement).style.borderColor = "var(--color-success-bg)";
              (e.currentTarget as HTMLButtonElement).style.opacity = "1";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(1)";
            }}
            style={{
              fontSize: 11,
              fontWeight: 700,
              padding: "3px 9px",
              borderRadius: 5,
              border: "1px solid var(--color-success-bg)",
              background: "var(--color-success-bg)",
              color: "var(--color-success)",
              cursor: "pointer",
              transition: "background 100ms, opacity 80ms, transform 80ms",
            }}
          >
            ✓
          </button>
          <button
            title="Deny"
            onMouseDown={(e) => {
              e.stopPropagation();
              (e.currentTarget as HTMLButtonElement).style.opacity = "0.6";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(0.94)";
            }}
            onMouseUp={(e) => {
              (e.currentTarget as HTMLButtonElement).style.opacity = "1";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(1)";
            }}
            onClick={(e) => { e.stopPropagation(); onDeny(); }}
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--color-danger-hover)";
              (e.currentTarget as HTMLButtonElement).style.borderColor = "var(--color-danger-hover)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--color-danger-bg)";
              (e.currentTarget as HTMLButtonElement).style.borderColor = "var(--color-danger-bg)";
              (e.currentTarget as HTMLButtonElement).style.opacity = "1";
              (e.currentTarget as HTMLButtonElement).style.transform = "scale(1)";
            }}
            style={{
              fontSize: 11,
              fontWeight: 700,
              padding: "3px 9px",
              borderRadius: 5,
              border: "1px solid var(--color-danger-bg)",
              background: "var(--color-danger-bg)",
              color: "var(--color-danger)",
              cursor: "pointer",
              transition: "background 100ms, opacity 80ms, transform 80ms",
            }}
          >
            ✗
          </button>
        </div>
      )}
    </div>
  );
}
