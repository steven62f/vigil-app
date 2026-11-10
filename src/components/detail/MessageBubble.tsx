import { useState } from "react";
import type { AgentMessage } from "../../types/agent";

interface Props {
  message: AgentMessage;
}

function formatTimestamp(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

const ROLE_STYLES: Record<string, React.CSSProperties> = {
  user: {
    background: "var(--bg-surface-hover)",
    borderLeft: "2px solid var(--text-muted)",
    paddingLeft: 8,
    color: "var(--text-secondary)",
  },
  assistant: {
    background: "var(--bg-surface)",
    color: "var(--text-primary)",
  },
  tool: {
    background: "var(--bg-subtle)",
    color: "var(--text-secondary)",
    opacity: 0.85,
  },
};

const MAX_LINES = 3;

export function MessageBubble({ message }: Props) {
  const [expanded, setExpanded] = useState(false);

  const lines = message.content.split("\n");
  const needsExpand = lines.length > MAX_LINES;
  const displayLines = expanded || !needsExpand ? lines : lines.slice(0, MAX_LINES);
  const displayText = displayLines.join("\n");

  const roleStyle = ROLE_STYLES[message.role] ?? ROLE_STYLES.assistant;

  return (
    <div
      style={{
        fontFamily: "var(--font-mono, 'JetBrains Mono', 'Geist Mono', monospace)",
        fontSize: 12,
        fontWeight: 400,
        lineHeight: 1.5,
        padding: "6px 10px",
        borderRadius: 4,
        marginBottom: 4,
        wordBreak: "break-word",
        whiteSpace: "pre-wrap",
        ...roleStyle,
      }}
    >
      {message.timestamp > 0 && (
        <div
          style={{
            fontSize: 10,
            color: "var(--text-muted)",
            marginBottom: 2,
            fontFamily: "inherit",
          }}
        >
          {formatTimestamp(message.timestamp)}
          {message.role !== "user" && (
            <span
              style={{
                marginLeft: 6,
                textTransform: "uppercase",
                letterSpacing: "0.05em",
                opacity: 0.7,
              }}
            >
              {message.role}
            </span>
          )}
        </div>
      )}
      <span>{displayText}</span>
      {needsExpand && (
        <button
          onClick={() => setExpanded((v) => !v)}
          style={{
            display: "block",
            marginTop: 2,
            background: "none",
            border: "none",
            padding: 0,
            cursor: "pointer",
            color: "var(--text-muted)",
            fontSize: 10,
            fontFamily: "inherit",
          }}
        >
          {expanded ? "↑ moins" : `↓ +${lines.length - MAX_LINES} ligne${lines.length - MAX_LINES > 1 ? "s" : ""}`}
        </button>
      )}
    </div>
  );
}
