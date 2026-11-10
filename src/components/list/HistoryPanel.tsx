import { useCallback, useState } from "react";
import type { ProjectGroup, SessionMetadata } from "../../types/agent";
import { ProjectAccordion } from "./ProjectAccordion";

interface Props {
  groups: ProjectGroup[];
  activeTranscriptPaths: Set<string>;
  /** Outil de l'onglet courant, threadé jusqu'au Resume (CLI propre à l'outil, AC7). */
  tool: string;
}

export function HistoryPanel({ groups, activeTranscriptPaths, tool }: Props) {
  const [metaMap, setMetaMap] = useState<Map<string, SessionMetadata>>(new Map());

  const handleMetadataSaved = useCallback((path: string, meta: SessionMetadata) => {
    setMetaMap((prev) => {
      const next = new Map(prev);
      next.set(path, meta);
      return next;
    });
  }, []);

  const isEmpty = groups.length === 0 || groups.every((g) => g.sessions.length === 0);

  return (
    <div
      style={{
        padding: "16px 20px",
        overflowY: "auto",
        height: "100%",
        boxSizing: "border-box",
        display: "flex",
        flexDirection: "column",
        gap: 10,
        color: "var(--text-primary)",
      }}
    >
      <h2 style={{ margin: "0 0 8px", fontSize: 16, fontWeight: 600 }}>Session History</h2>

      {isEmpty && (
        <div style={{ fontSize: 13, color: "var(--text-muted)", textAlign: "center", marginTop: 32 }}>
          No history available
        </div>
      )}

      {groups.map((group) => (
        <ProjectAccordion
          key={group.projectPath || "__unknown__"}
          group={group}
          activeTranscriptPaths={activeTranscriptPaths}
          metaMap={metaMap}
          onMetadataSaved={handleMetadataSaved}
          tool={tool}
        />
      ))}
    </div>
  );
}
