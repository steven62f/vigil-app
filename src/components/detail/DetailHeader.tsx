import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot, SessionMetadata } from "../../types/agent";
import { MascotPixel, stateToVar } from "../shared/MascotPixel";
import { SubagentDots } from "../shared/SubagentDots";
import { SourceBadge } from "../list/SourceBadge";
import { ToolBadge } from "../shared/ToolBadge";
import { toStateLabel } from "../../lib/agentDisplay";
import { useSettings } from "../../hooks/useSettings";
import { mascotForTool } from "../../lib/settingsStore";

interface Props {
  agent: AgentSnapshot;
  fromList: boolean;
  onBack: () => void;
  onClose: () => void;
}

const MAX_NAME = 40;
const MAX_DESC = 200;
const MAX_TAGS = 2;
const MAX_TAG_LEN = 20;

const truncateCodepoints = (s: string, max: number): string =>
  [...s].slice(0, max).join("");

const BTN: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  width: 32,
  height: 32,
  background: "none",
  border: "none",
  cursor: "pointer",
  borderRadius: 6,
  color: "var(--text-muted)",
  fontSize: 16,
  padding: 0,
  flexShrink: 0,
  transition: "color 120ms, background 120ms",
};

const NAME_TEXT: React.CSSProperties = {
  fontSize: 13,
  fontWeight: 500,
  lineHeight: 1.3,
  color: "var(--text-primary)",
  maxWidth: 130,
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
  cursor: "text",
  padding: "0 2px",
  borderRadius: 3,
};

const NAME_INPUT: React.CSSProperties = {
  fontSize: 13,
  fontWeight: 500,
  color: "var(--text-primary)",
  background: "var(--bg-input-subtle)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 4,
  padding: "1px 5px",
  width: 140,
  outline: "none",
  fontFamily: "inherit",
};

const TAG_CHIP: React.CSSProperties = {
  background: "rgba(255,255,255,0.08)",
  color: "var(--text-primary)",
  fontFamily: "monospace",
  fontSize: 10,
  height: 18,
  lineHeight: 1,
  borderRadius: 3,
  padding: "0 2px 0 5px",
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
  fontSize: 11,
  lineHeight: 1,
  width: 12,
  height: 12,
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
};

const ADD_TAG_BTN: React.CSSProperties = {
  background: "none",
  border: "1px dashed var(--border-subtle)",
  borderRadius: 3,
  color: "var(--text-muted)",
  fontSize: 10,
  fontFamily: "monospace",
  padding: 0,
  height: 18,
  width: 20,
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
  fontSize: 10,
  fontFamily: "monospace",
  padding: "1px 5px",
  outline: "none",
  width: 70,
  height: 18,
  boxSizing: "border-box",
};

const DESC_TEXT: React.CSSProperties = {
  fontSize: 11,
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
  fontSize: 11,
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

export function DetailHeader({ agent, fromList, onBack, onClose }: Props) {
  const settings = useSettings();
  const [customName, setCustomName] = useState<string | null>(agent.customName ?? null);
  const [description, setDescription] = useState<string | null>(agent.description ?? null);
  const [tags, setTags] = useState<string[]>(agent.tags ?? []);
  const [pinned, setPinned] = useState<boolean>(agent.pinned ?? false);

  const stateRef = useRef({ customName, description, tags, pinned });
  useEffect(() => {
    stateRef.current = { customName, description, tags, pinned };
  }, [customName, description, tags, pinned]);

  useEffect(() => {
    setCustomName(agent.customName ?? null);
  }, [agent.customName]);
  useEffect(() => {
    setDescription(agent.description ?? null);
  }, [agent.description]);
  useEffect(() => {
    setTags(agent.tags ?? []);
  }, [agent.tags]);
  useEffect(() => {
    setPinned(agent.pinned ?? false);
  }, [agent.pinned]);

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
          transcriptPath: agent.transcriptPath,
          metadata: next,
        });
      } catch (err) {
        console.error("[vigil] set_session_metadata:", err);
        setCustomName(prev.customName);
        setDescription(prev.description);
        setTags(prev.tags);
        setPinned(prev.pinned);
      }
    },
    [agent.transcriptPath],
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

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 2,
        padding: "6px 12px",
        borderBottom: "1px solid var(--border-subtle)",
        flexShrink: 0,
        boxSizing: "border-box",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", height: 32, gap: 6, minWidth: 0 }}>
        {fromList && (
          <button
            onClick={onBack}
            onMouseDown={(e) => e.stopPropagation()}
            style={BTN}
            title="Back to list"
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
              (e.currentTarget as HTMLButtonElement).style.background = "none";
            }}
          >
            ‹
          </button>
        )}

        <MascotPixel state={agent.state} size={32} interruptPending={agent.interruptPending ?? false} variant={mascotForTool(agent.sourceTool, settings)} />

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 5,
            minWidth: 0,
            flex: "1 1 auto",
            flexWrap: "nowrap",
            overflow: "hidden",
          }}
        >
          {editingName ? (
            <input
              autoFocus
              maxLength={MAX_NAME}
              value={nameDraft}
              placeholder={agent.name}
              onChange={(e) => setNameDraft(e.target.value)}
              onBlur={commitName}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitName();
                else if (e.key === "Escape") setEditingName(false);
              }}
              onMouseDown={(e) => e.stopPropagation()}
              onClick={(e) => e.stopPropagation()}
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
              {agent.name}
            </span>
          )}

          <span
            style={{
              display: "inline-flex",
              alignItems: "center",
              height: 18,
              fontSize: 9,
              fontWeight: 600,
              lineHeight: 1,
              color: stateToVar(agent.state),
              whiteSpace: "nowrap",
              flexShrink: 0,
            }}
          >
            {toStateLabel(agent.state)}
          </span>

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
                onClick={(e) => e.stopPropagation()}
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
        </div>

        {settings.showToolBadge && <ToolBadge sourceTool={agent.sourceTool} />}

        {settings.showModel && agent.model && (
          <span
            style={{
              fontSize: 9,
              fontWeight: 600,
              lineHeight: 1,
              display: "inline-flex",
              alignItems: "center",
              height: 18,
              padding: "0 5px",
              borderRadius: 3,
              background: "var(--bg-subtle)",
              color: "var(--text-muted)",
              whiteSpace: "nowrap",
              flexShrink: 0,
              userSelect: "none",
              maxWidth: 120,
              overflow: "hidden",
              textOverflow: "ellipsis",
            }}
          >
            {agent.model}
          </span>
        )}

        <SourceBadge
          source={agent.source}
          onTerminalClick={
            agent.terminalPid != null
              ? () => invoke("focus_terminal", { pid: agent.terminalPid }).catch(() => {})
              : undefined
          }
        />

        {(agent.subagents?.length ?? 0) > 0 && <SubagentDots subagents={agent.subagents!} />}

        <button
          onClick={onClose}
          onMouseDown={(e) => e.stopPropagation()}
          style={BTN}
          title="Close"
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover)";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
            (e.currentTarget as HTMLButtonElement).style.background = "none";
          }}
        >
          ×
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
            onClick={(e) => e.stopPropagation()}
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
    </div>
  );
}
