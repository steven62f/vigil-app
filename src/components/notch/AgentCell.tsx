import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot, AgentState } from "../../types/agent";
import { MascotPixel, stateToVar } from "../shared/MascotPixel";
import { SubagentDots } from "../shared/SubagentDots";
import { useSettings } from "../../hooks/useSettings";
import { mascotForTool } from "../../lib/settingsStore";
import { useNavigation } from "../../hooks/NavigationProvider";
import { toStateLabel } from "../../lib/agentDisplay";

interface Props {
  agent: AgentSnapshot;
  showStateLabel?: boolean;
}

export function AgentCell({ agent, showStateLabel = true }: Props) {
  const [hovered, setHovered] = useState(false);
  const settings = useSettings();
  const { openDetail } = useNavigation();

  const handleClick = () => {
    if (settings.quickTerminalAccess && agent.terminalPid != null) {
      invoke("focus_terminal", { pid: agent.terminalPid }).catch(() => {});
    } else {
      openDetail(agent.transcriptPath, false);
    }
  };

  return (
    <div
      onClick={handleClick}
      onMouseDown={(e) => e.stopPropagation()}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 6,
        padding: "0 8px",
        height: 48,
        minWidth: 32,
        borderRadius: 8,
        overflow: "visible",
        position: "relative",
        cursor: "default",
        background: hovered ? "var(--bg-surface-hover)" : "transparent",
        transition: "background 150ms ease",
        boxSizing: "border-box",
      }}
    >
      <MascotPixel state={agent.state} size={32} interruptPending={agent.interruptPending ?? false} variant={mascotForTool(agent.sourceTool, settings)} />
      <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
        <span
          style={{
            fontSize: 13,
            fontWeight: 500,
            color: "var(--text-primary)",
            maxWidth: 80,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {agent.name}
        </span>
        {showStateLabel && (
          <span
            style={{
              fontSize: 11,
              fontWeight: 600,
              color: stateToVar(agent.state),
              lineHeight: 1,
              transition: "color 300ms ease-out",
            }}
          >
            {toStateLabel(agent.state)}
          </span>
        )}
        {(agent.subagents?.length ?? 0) > 0 && (
          <div style={{ marginTop: 2 }}>
            <SubagentDots subagents={agent.subagents!} />
          </div>
        )}
      </div>
    </div>
  );
}
