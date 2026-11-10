import type { SubagentSnapshot } from "../../types/agent";

const MAX_DOTS = 5;

interface Props {
  subagents: SubagentSnapshot[];
}

export function SubagentDots({ subagents }: Props) {
  if (subagents.length === 0) return null;

  const visible = subagents.slice(0, MAX_DOTS);
  const overflow = subagents.length - MAX_DOTS;

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 3,
        flexShrink: 0,
      }}
    >
      {visible.map((sub) => (
        <span
          key={sub.agentId}
          title={sub.agentType}
          style={{
            width: 8,
            height: 8,
            borderRadius: "50%",
            flexShrink: 0,
            background:
              sub.state === "working"
                ? "var(--state-working)"
                : "var(--state-disconnected)",
          }}
        />
      ))}
      {overflow > 0 && (
        <span
          style={{
            fontSize: 9,
            color: "var(--text-muted)",
            lineHeight: 1,
            flexShrink: 0,
          }}
        >
          +{overflow}
        </span>
      )}
    </div>
  );
}
