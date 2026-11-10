import type { SourceTool } from "../../types/agent";

const TOOL_LABELS: Partial<Record<SourceTool, string>> = {
  claude_code: "CLAUDE",
  codex: "CODEX",
  grok: "GROK",
  antigravity: "ANTIGRAVITY",
  kimi: "KIMI",
};

const TOOL_BG: Partial<Record<SourceTool, string>> = {
  claude_code: "var(--bg-subtle)",
  codex: "var(--source-codex-bg)",
  grok: "var(--source-grok-bg)",
  antigravity: "var(--source-antigravity-bg)",
  kimi: "var(--source-kimi-bg)",
};

const TOOL_COLOR: Partial<Record<SourceTool, string>> = {
  claude_code: "var(--text-muted)",
  codex: "var(--source-codex)",
  grok: "var(--source-grok)",
  antigravity: "var(--source-antigravity)",
  kimi: "var(--source-kimi)",
};

interface Props {
  sourceTool: SourceTool;
}

export function ToolBadge({ sourceTool }: Props) {
  const label = TOOL_LABELS[sourceTool];
  if (!label) return null;

  return (
    <span
      style={{
        fontSize: 9,
        fontWeight: 600,
        padding: "1px 5px",
        borderRadius: 3,
        textTransform: "uppercase" as const,
        letterSpacing: "0.04em",
        cursor: "default",
        userSelect: "none",
        flexShrink: 0,
        background: TOOL_BG[sourceTool],
        color: TOOL_COLOR[sourceTool],
      }}
    >
      {label}
    </span>
  );
}
