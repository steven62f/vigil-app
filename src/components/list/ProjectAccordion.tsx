import { useState } from "react";
import type { ProjectGroup, SessionMetadata } from "../../types/agent";
import { HistorySessionCard } from "./HistorySessionCard";

interface Props {
  group: ProjectGroup;
  activeTranscriptPaths: Set<string>;
  metaMap: Map<string, SessionMetadata>;
  onMetadataSaved: (path: string, meta: SessionMetadata) => void;
  /** Outil de l'onglet courant, threadé jusqu'au Resume (AC7). */
  tool: string;
}

function formatGroupDate(ms: number): string {
  return new Date(ms).toLocaleDateString("en-GB", {
    day: "2-digit",
    month: "2-digit",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function metaFromSession(session: ProjectGroup["sessions"][number]): SessionMetadata | null {
  const hasAny =
    !!session.customName ||
    !!session.description ||
    (session.tags?.length ?? 0) > 0 ||
    !!session.pinned;
  if (!hasAny) return null;
  return {
    customName: session.customName ?? null,
    description: session.description ?? null,
    tags: session.tags ?? [],
    pinned: session.pinned ?? false,
  };
}

export function ProjectAccordion({
  group,
  activeTranscriptPaths,
  metaMap,
  onMetadataSaved,
  tool,
}: Props) {
  const [open, setOpen] = useState(true);
  const sessionCount = group.sessionCount;

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      <button
        onClick={() => setOpen((o) => !o)}
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          padding: "8px 12px",
          background: "var(--bg-surface)",
          border: "1px solid var(--border-subtle)",
          borderRadius: 6,
          cursor: "pointer",
          color: "var(--text-primary)",
          fontSize: 14,
          fontWeight: 600,
          textAlign: "left",
          width: "100%",
        }}
      >
        <span style={{ fontSize: 11, color: "var(--text-muted)", width: 10 }}>
          {open ? "▾" : "▸"}
        </span>
        <span>{group.projectName}</span>
        {!open && (
          <span style={{ fontSize: 12, fontWeight: 400, color: "var(--text-muted)" }}>
            · {sessionCount} session{sessionCount !== 1 ? "s" : ""} ·{" "}
            {formatGroupDate(group.lastActivity)}
          </span>
        )}
      </button>

      {open && (
        <div style={{ display: "flex", flexDirection: "column", gap: 8, paddingLeft: 8 }}>
          {group.sessions.map((session) => {
            const fromMap = metaMap.get(session.transcriptPath);
            const initial = fromMap ?? metaFromSession(session);
            return (
              <HistorySessionCard
                key={session.transcriptPath}
                session={session}
                isActive={activeTranscriptPaths.has(session.transcriptPath)}
                initialMeta={initial}
                defaultName={group.projectName}
                onMetadataSaved={(m) => onMetadataSaved(session.transcriptPath, m)}
                tool={tool}
              />
            );
          })}
        </div>
      )}
    </div>
  );
}
