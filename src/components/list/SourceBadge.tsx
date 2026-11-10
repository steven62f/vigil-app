import { useState } from "react";
import type { TerminalSource } from "../../types/agent";

interface Props {
  /** Libellé d'hôte déjà convivial (« VS Code », « PowerShell »…) — le CSS le met en majuscules. */
  source: TerminalSource;
  onTerminalClick?: () => void;
}

export function SourceBadge({ source, onTerminalClick }: Props) {
  const [hovered, setHovered] = useState(false);
  return (
    <span
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => {
        // Ne capter le clic que si le badge est cliquable (agent terminal). Pour un agent IDE
        // (`onTerminalClick` absent), laisser propager pour que la ligne ouvre le détail — sinon
        // le badge serait une zone morte (code-review 6.38).
        if (onTerminalClick) {
          e.stopPropagation();
          onTerminalClick();
        }
      }}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      style={{
        fontSize: 9,
        fontWeight: 600,
        padding: "1px 5px",
        borderRadius: 3,
        background: hovered ? "var(--bg-badge-hover)" : "var(--bg-subtle)",
        color: hovered ? "var(--text-secondary)" : "var(--text-muted)",
        textTransform: "uppercase" as const,
        letterSpacing: "0.04em",
        cursor: onTerminalClick ? "pointer" : "default",
        transition: "background 120ms, color 120ms",
        userSelect: "none",
        flexShrink: 0,
      }}
    >
      {source}
    </span>
  );
}
