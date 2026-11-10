import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SessionSummary, SessionMetadata } from "../../types/agent";

interface Props {
  session: SessionSummary;
  isActive: boolean;
  initialMeta: SessionMetadata | null;
  defaultName: string;
  onMetadataSaved: (meta: SessionMetadata) => void;
  /** Outil de l'onglet : le backend choisit la CLI de reprise (path Claude vs session id Codex). */
  tool: string;
}

const MAX_NAME = 40;
const MAX_DESC = 200;
const MAX_TAGS = 2;
const MAX_TAG_LEN = 20;

const truncateCodepoints = (s: string, max: number): string =>
  [...s].slice(0, max).join("");

const NAME_TEXT: React.CSSProperties = {
  fontSize: 15,
  fontWeight: 600,
  lineHeight: 1,
  color: "var(--text-primary)",
  cursor: "text",
  padding: "0 3px",
  borderRadius: 3,
  maxWidth: 240,
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
};

const NAME_INPUT: React.CSSProperties = {
  fontSize: 15,
  fontWeight: 600,
  color: "var(--text-primary)",
  background: "var(--bg-input-subtle)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 4,
  padding: "2px 6px",
  width: 240,
  outline: "none",
  fontFamily: "inherit",
};

const TAG_CHIP: React.CSSProperties = {
  background: "rgba(255,255,255,0.08)",
  color: "var(--text-primary)",
  fontFamily: "monospace",
  fontSize: 11,
  lineHeight: 1,
  height: 20,
  borderRadius: 3,
  padding: "0 2px 0 6px",
  display: "inline-flex",
  alignItems: "center",
  gap: 2,
  flexShrink: 0,
};

const TAG_REMOVE: React.CSSProperties = {
  background: "none",
  border: "none",
  color: "var(--text-muted)",
  cursor: "pointer",
  padding: 0,
  fontSize: 12,
  lineHeight: 1,
  width: 14,
  height: 14,
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
};

const ADD_TAG_BTN: React.CSSProperties = {
  background: "none",
  border: "1px dashed var(--border-subtle)",
  borderRadius: 3,
  color: "var(--text-muted)",
  fontSize: 12,
  fontFamily: "monospace",
  padding: 0,
  height: 20,
  width: 22,
  lineHeight: 1,
  cursor: "pointer",
  flexShrink: 0,
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
};

const TAG_INPUT: React.CSSProperties = {
  background: "var(--bg-input-subtle)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 3,
  color: "var(--text-primary)",
  fontSize: 11,
  fontFamily: "monospace",
  padding: "1px 6px",
  outline: "none",
  width: 80,
  height: 20,
  boxSizing: "border-box",
};

const DESC_TEXT: React.CSSProperties = {
  fontSize: 13,
  color: "var(--text-muted)",
  cursor: "text",
  padding: "1px 4px",
  borderRadius: 3,
  display: "block",
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
  fontStyle: "italic",
};

const DESC_INPUT: React.CSSProperties = {
  fontSize: 13,
  color: "var(--text-primary)",
  background: "var(--bg-input-subtle)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 4,
  padding: "2px 6px",
  width: "100%",
  outline: "none",
  fontFamily: "inherit",
  boxSizing: "border-box",
};

function formatDate(ms: number): string {
  return new Date(ms).toLocaleDateString("en-GB", {
    day: "2-digit",
    month: "2-digit",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function HistorySessionCard({
  session,
  isActive,
  initialMeta,
  defaultName,
  onMetadataSaved,
  tool,
}: Props) {
  const [customName, setCustomName] = useState<string | null>(initialMeta?.customName ?? null);
  const [description, setDescription] = useState<string | null>(initialMeta?.description ?? null);
  const [tags, setTags] = useState<string[]>(initialMeta?.tags ?? []);
  const [pinned, setPinned] = useState<boolean>(initialMeta?.pinned ?? false);

  const stateRef = useRef({ customName, description, tags, pinned });
  useEffect(() => {
    stateRef.current = { customName, description, tags, pinned };
  }, [customName, description, tags, pinned]);

  const initializedFor = useRef<string | null>(null);
  useEffect(() => {
    if (initializedFor.current !== session.transcriptPath && initialMeta) {
      setCustomName(initialMeta.customName ?? null);
      setDescription(initialMeta.description ?? null);
      setTags(initialMeta.tags ?? []);
      setPinned(initialMeta.pinned ?? false);
      initializedFor.current = session.transcriptPath;
    }
  }, [session.transcriptPath, initialMeta]);

  const [editingName, setEditingName] = useState(false);
  const [nameDraft, setNameDraft] = useState("");
  const [editingDesc, setEditingDesc] = useState(false);
  const [descDraft, setDescDraft] = useState("");
  const [tagInput, setTagInput] = useState("");
  const [showTagInput, setShowTagInput] = useState(false);

  const save = useCallback(
    async (patch: Partial<SessionMetadata>) => {
      const cur = stateRef.current;
      const next: SessionMetadata = {
        customName: patch.customName !== undefined ? patch.customName : cur.customName,
        description: patch.description !== undefined ? patch.description : cur.description,
        tags: patch.tags !== undefined ? patch.tags : cur.tags,
        pinned: patch.pinned !== undefined ? patch.pinned : cur.pinned,
      };
      const prev = { ...cur };
      setCustomName(next.customName ?? null);
      setDescription(next.description ?? null);
      setTags(next.tags);
      setPinned(next.pinned);
      try {
        await invoke("set_session_metadata", {
          transcriptPath: session.transcriptPath,
          metadata: next,
        });
        onMetadataSaved(next);
      } catch (err) {
        console.error("[vigil] set_session_metadata:", err);
        setCustomName(prev.customName);
        setDescription(prev.description);
        setTags(prev.tags);
        setPinned(prev.pinned);
      }
    },
    [session.transcriptPath, onMetadataSaved],
  );

  const commitName = () => {
    setEditingName(false);
    const next = truncateCodepoints(nameDraft.trim(), MAX_NAME);
    save({ customName: next || null });
  };
  const commitDesc = () => {
    setEditingDesc(false);
    const next = truncateCodepoints(descDraft.trim(), MAX_DESC);
    save({ description: next || null });
  };
  const addTag = () => {
    const t = truncateCodepoints(tagInput.trim(), MAX_TAG_LEN).toLowerCase();
    setTagInput("");
    setShowTagInput(false);
    const cur = stateRef.current;
    if (!t || cur.tags.includes(t) || cur.tags.length >= MAX_TAGS) return;
    save({ tags: [...cur.tags, t] });
  };
  const removeTag = (t: string) => {
    const cur = stateRef.current;
    save({ tags: cur.tags.filter((x) => x !== t) });
  };
  const togglePin = () => {
    save({ pinned: !stateRef.current.pinned });
  };

  const displayName = customName && customName.length > 0 ? customName : defaultName;

  return (
    <div
      style={{
        position: "relative",
        background: "var(--bg-surface)",
        border: "1px solid var(--border-subtle)",
        borderRadius: 8,
        padding: "10px 14px 8px",
        display: "flex",
        flexDirection: "column",
        gap: 4,
      }}
    >
      {pinned && (
        <div
          style={{
            position: "absolute",
            left: 0,
            top: 0,
            bottom: 0,
            width: 3,
            background: "var(--color-amber)",
            borderRadius: "8px 0 0 8px",
          }}
        />
      )}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 6,
          minWidth: 0,
        }}
      >
        {editingName ? (
          <input
            autoFocus
            maxLength={MAX_NAME}
            value={nameDraft}
            placeholder={defaultName}
            onChange={(e) => setNameDraft(e.target.value)}
            onBlur={commitName}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitName();
              else if (e.key === "Escape") setEditingName(false);
            }}
            onMouseDown={(e) => e.stopPropagation()}
            style={NAME_INPUT}
          />
        ) : (
          <span
            onClick={() => {
              setNameDraft(customName ?? "");
              setEditingName(true);
            }}
            title="Cliquer pour renommer la session"
            style={NAME_TEXT}
          >
            {displayName}
          </span>
        )}

        {tags.map((tag) => (
          <span key={tag} style={TAG_CHIP}>
            {tag}
            <button
              onClick={() => removeTag(tag)}
              onMouseDown={(e) => {
                e.preventDefault();
                e.stopPropagation();
              }}
              title={`Retirer ${tag}`}
              style={TAG_REMOVE}
            >
              ×
            </button>
          </span>
        ))}

        {tags.length < MAX_TAGS &&
          (showTagInput ? (
            <input
              autoFocus
              maxLength={MAX_TAG_LEN}
              value={tagInput}
              placeholder="tag"
              onChange={(e) => setTagInput(e.target.value)}
              onBlur={addTag}
              onKeyDown={(e) => {
                if (e.key === "Enter") addTag();
                else if (e.key === "Escape") {
                  setShowTagInput(false);
                  setTagInput("");
                }
              }}
              onMouseDown={(e) => e.stopPropagation()}
              style={TAG_INPUT}
            />
          ) : (
            <button
              onClick={() => setShowTagInput(true)}
              onMouseDown={(e) => e.stopPropagation()}
              title="Ajouter un tag"
              style={ADD_TAG_BTN}
            >
              +
            </button>
          ))}

        <div style={{ flex: 1 }} />

        {isActive ? (
          <span
            style={{
              fontSize: 11,
              fontWeight: 700,
              lineHeight: 1,
              display: "inline-flex",
              alignItems: "center",
              height: 20,
              color: "var(--color-success)",
              background: "var(--color-success-bg)",
              border: "1px solid var(--color-success)",
              borderRadius: 4,
              padding: "0 7px",
              textTransform: "uppercase",
              letterSpacing: "0.05em",
              flexShrink: 0,
              boxSizing: "border-box",
            }}
          >
            Active
          </span>
        ) : (
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() =>
              invoke("resume_session", {
                transcriptPath: session.transcriptPath,
                cwd: session.cwd ?? "",
                tool,
                sessionId: session.sessionId,
              }).catch(console.error)
            }
            style={{
              background: "var(--bg-surface-hover, var(--bg-overlay))",
              border: "1px solid var(--border-subtle)",
              borderRadius: 5,
              color: "var(--text-primary)",
              fontSize: 12,
              fontWeight: 600,
              lineHeight: 1,
              display: "inline-flex",
              alignItems: "center",
              height: 22,
              padding: "0 10px",
              cursor: "pointer",
              flexShrink: 0,
              boxSizing: "border-box",
            }}
          >
            ▶ Resume
          </button>
        )}

        <button
          onClick={togglePin}
          onMouseDown={(e) => e.stopPropagation()}
          title={pinned ? "Désépingler" : "Épingler"}
          style={{
            background: "none",
            border: "none",
            cursor: "pointer",
            padding: 4,
            display: "inline-flex",
            alignItems: "center",
            justifyContent: "center",
            color: pinned ? "var(--color-amber)" : "var(--text-muted)",
            opacity: pinned ? 1 : 0.45,
            flexShrink: 0,
            transition: "opacity 120ms, color 120ms",
            borderRadius: 4,
          }}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.opacity = "1";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.opacity = pinned ? "1" : "0.45";
          }}
        >
          <PinIcon filled={pinned} size={16} />
        </button>
      </div>

      <div>
        {editingDesc ? (
          <input
            autoFocus
            maxLength={MAX_DESC}
            value={descDraft}
            placeholder="Description…"
            onChange={(e) => setDescDraft(e.target.value)}
            onBlur={commitDesc}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitDesc();
              else if (e.key === "Escape") setEditingDesc(false);
            }}
            onMouseDown={(e) => e.stopPropagation()}
            style={DESC_INPUT}
          />
        ) : (
          <span
            onClick={() => {
              setDescDraft(description ?? "");
              setEditingDesc(true);
            }}
            title="Cliquer pour éditer la description"
            style={{
              ...DESC_TEXT,
              opacity: description ? 1 : 0.5,
            }}
          >
            {description || "+ description"}
          </span>
        )}
      </div>

      <div
        style={{
          fontSize: 12,
          color: "var(--text-muted)",
          display: "flex",
          gap: 10,
          marginTop: 2,
        }}
      >
        <span>{formatDate(session.lastActiveAt)}</span>
        {session.model && <span>{session.model}</span>}
      </div>

      {session.firstUserMessage && (
        <div
          style={{
            fontSize: 13,
            color: "var(--text-secondary)",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {session.firstUserMessage}
        </div>
      )}
    </div>
  );
}

function PinIcon({ filled, size = 14 }: { filled: boolean; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill={filled ? "currentColor" : "none"}
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <line x1="12" y1="17" x2="12" y2="22" />
      <path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 2-2V3H6v1a2 2 0 0 0 2 2h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z" />
    </svg>
  );
}
