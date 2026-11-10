import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AgentSnapshot, AgentMessage } from "../../types/agent";
import type { AgentStats } from "../../types/stats";
import { useSettings } from "../../hooks/useSettings";
import { formatElapsed } from "../../lib/timeUtils";
import { errorMessages } from "../../lib/errorMessages";
import { fmtTokens, fmtCost } from "../../lib/formatStats";
import { INTERRUPT_SUPPORTED, LIVE_MODEL_SUPPORTED, type StatsTool } from "../../lib/statsTools";
import { DetailHeader } from "./DetailHeader";
import { MessageBubble } from "./MessageBubble";
import { PermissionBlock } from "./PermissionBlock";
import { AgentPermissionMenu } from "./AgentPermissionMenu";
import { AgentNotifMenu } from "./AgentNotifMenu";
import { UsageBar, useUsageBarHeight, useUsageBarPosition } from "../notch/UsageBar";

interface Props {
  agent: AgentSnapshot;
  fromList: boolean;
  onBack: () => void;
  onClose: () => void;
}

const DETAIL_W = 480;
const DETAIL_H_DEFAULT = 380;
const DETAIL_H_PERMISSION = 540;

export function DetailPanel({ agent, fromList, onBack, onClose }: Props) {
  const settings = useSettings();
  const barH = useUsageBarHeight();
  const barPos = useUsageBarPosition();
  const [messages, setMessages] = useState<AgentMessage[]>([]);
  const [loading, setLoading] = useState(true);
  const [stats, setStats] = useState<AgentStats | null>(null);
  const [statsLoading, setStatsLoading] = useState(false);
  const [liveModel, setLiveModel] = useState<string | null>(null);
  const [interruptReason, setInterruptReason] = useState("");
  const [isInterrupting, setIsInterrupting] = useState(false);
  // Revue 8.2 — arbitre des popovers du footer : ils partagent le même ancrage, donc un seul
  // ouvert à la fois. `null` = aucun.
  const [openMenu, setOpenMenu] = useState<"notifs" | "perms" | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const isInitialLoad = useRef(true);

  const handleInterrupt = async () => {
    setIsInterrupting(true);
    const reason = interruptReason.trim() || undefined;
    setInterruptReason("");
    try {
      await invoke("interrupt_agent", { id: agent.transcriptPath, reason });
    } catch (err: unknown) {
      console.error("[vigil] interrupt_agent:", errorMessages[err as string] ?? err);
    } finally {
      // `finally` et non `catch` seul : `isInterrupting` ne signifie QUE « invoke en vol ». Sur le
      // chemin de succès, le bouton est de toute façon démonté (branche `interruptPending`) — le
      // laisser à `true` le figeait « Requesting… » au retour sur la branche bouton (agent relancé
      // après une interruption réussie, ou Cancel), jusqu'au remount du panneau.
      setIsInterrupting(false);
    }
  };

  const handleCancelInterrupt = async () => {
    invoke("cancel_interrupt", { id: agent.transcriptPath }).catch((err: unknown) => {
      console.error("[vigil] cancel_interrupt:", err);
    });
  };

  const limit = settings.detailHistoryMode === "detailed" ? 20 : 3;

  const fetchMessages = async () => {
    if (!agent.transcriptPath) {
      setLoading(false);
      return;
    }
    try {
      const result = await invoke<AgentMessage[]>("get_conversation", {
        id: agent.transcriptPath,
        limit,
        tool: agent.sourceTool,
      });
      setMessages(result);
    } catch (err) {
      console.error("[vigil] get_conversation:", err);
      setMessages([]);
    } finally {
      setLoading(false);
    }
  };

  const fetchStats = async () => {
    if (!agent.transcriptPath) {
      return;
    }
    let cancelled = false;
    const timerId = setTimeout(() => { if (!cancelled) setStatsLoading(true); }, 200);
    try {
      const result = await invoke<AgentStats>("get_agent_stats", {
        transcriptPath: agent.transcriptPath,
        tool: agent.sourceTool,
      });
      setStats(result);
    } catch {
      setStats(null);
    } finally {
      cancelled = true;
      clearTimeout(timerId);
      setStatsLoading(false);
    }
  };

  // Badge modèle de l'agent Grok/Kimi VIVANT : `AgentSnapshot.model` est `undefined` (aucune
  // injection vigil-grok/vigil-kimi au session_start) → lecture backend on-demand à l'ouverture
  // (Story 6.29 Grok / 9.4 Kimi, patron `fetchStats`). Grok lit summary.json.current_model_id, Kimi
  // le dernier llm.request.model du wire.jsonl. Gaté sur `LIVE_MODEL_SUPPORTED && !model` → zéro
  // invoke sur les autres outils (qui portent déjà `model`). Échec silencieux → badge vide.
  const fetchLiveModel = async () => {
    if (!agent.transcriptPath || !LIVE_MODEL_SUPPORTED.includes(agent.sourceTool as StatsTool) || agent.model) {
      return;
    }
    try {
      const model = await invoke<string | null>("get_agent_model", {
        transcriptPath: agent.transcriptPath,
        tool: agent.sourceTool,
      });
      setLiveModel(model ?? null);
    } catch {
      setLiveModel(null);
    }
  };

  // Garde fetchRef à jour pour que le listener conversation:updated utilise toujours le limit courant
  const fetchRef = useRef(fetchMessages);
  useEffect(() => { fetchRef.current = fetchMessages; });

  const fetchStatsRef = useRef(fetchStats);
  useEffect(() => { fetchStatsRef.current = fetchStats; });

  // Redimensionne la fenêtre OS à chaque changement d'état (waitingApprove/waitingSpecific
  // peuvent arriver pendant que le panel est déjà ouvert).
  useEffect(() => {
    const base = (agent.state === "waitingApprove" || agent.state === "waitingSpecific")
      ? DETAIL_H_PERMISSION
      : DETAIL_H_DEFAULT;
    invoke("resize_notch", { width: DETAIL_W, height: base + barH }).catch(() => {});
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent.state, barH]);

  useEffect(() => {
    isInitialLoad.current = true;

    setLoading(true);
    setStats(null);
    setStatsLoading(false);
    setLiveModel(null);
    fetchMessages();
    fetchStats();
    fetchLiveModel();

    if (!agent.transcriptPath) return;

    invoke("start_conversation_watch", { path: agent.transcriptPath }).catch(() => {});

    const unlistenPromise = listen("conversation:updated", () => {
      fetchRef.current();
      fetchStatsRef.current();
    });

    const handleBlur = () => onClose();
    window.addEventListener("blur", handleBlur);

    return () => {
      invoke("stop_conversation_watch").catch(() => {});
      unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
      window.removeEventListener("blur", handleBlur);
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent.transcriptPath]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el || messages.length === 0) return;
    if (isInitialLoad.current) {
      el.scrollTop = el.scrollHeight;
      isInitialLoad.current = false;
    } else {
      messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [messages]);

  const panelHeight = ((agent.state === "waitingApprove" || agent.state === "waitingSpecific")
    ? DETAIL_H_PERMISSION
    : DETAIL_H_DEFAULT) + barH;

  return (
    <div
      onMouseDown={(e) => e.stopPropagation()}
      style={{
        position: "relative",
        display: "flex",
        flexDirection: "column",
        width: DETAIL_W,
        height: panelHeight,
        paddingTop: barPos === "top" ? barH : 0,
        paddingBottom: barPos === "bottom" ? barH : 0,
        background: "var(--bg-notch)",
        borderRadius: "0 0 16px 16px",
        boxShadow: "0 8px 32px var(--shadow-overlay)",
        overflow: "hidden",
        boxSizing: "border-box",
        animation: "detail-appear 200ms ease-out both",
      }}
    >
      <DetailHeader
        agent={{ ...agent, model: agent.model ?? liveModel ?? undefined }}
        fromList={fromList}
        onBack={onBack}
        onClose={onClose}
      />

      <div
        ref={scrollRef}
        style={{
          flex: 1,
          overflowY: "auto",
          padding: "8px 10px",
          margin: "0 4px 6px 0",
          display: "flex",
          flexDirection: "column",
        }}
      >
        {agent.state === "disconnected" && agent.lastActivity > 0 && (
          <div
            style={{
              fontSize: 11,
              color: "var(--text-muted)",
              marginBottom: 8,
              padding: "4px 8px",
              background: "var(--bg-subtle)",
              borderRadius: 4,
            }}
          >
            Last activity: {formatElapsed(agent.lastActivity)}
            {agent.lastActivityLabel ? ` — ${agent.lastActivityLabel}` : ""}
          </div>
        )}

        {loading && (
          <div
            style={{
              fontSize: 12,
              color: "var(--text-muted)",
              textAlign: "center",
              marginTop: 24,
            }}
          >
            Loading…
          </div>
        )}

        {!loading && messages.length === 0 && (
          <div
            style={{
              fontSize: 12,
              color: "var(--text-muted)",
              textAlign: "center",
              marginTop: 24,
            }}
          >
            No history available
          </div>
        )}

        {!loading &&
          messages.map((m, i) => (
            <MessageBubble key={`${m.role}-${m.timestamp}-${i}`} message={m} />
          ))}

        <div ref={messagesEndRef} />
      </div>

      {(agent.state === "waitingApprove" || agent.state === "waitingSpecific") && (
        <PermissionBlock agent={agent} />
      )}

      {agent.state === "disconnected" && (
        <div style={{ flexShrink: 0, borderTop: "1px solid var(--border-subtle)", padding: "8px 12px", display: "flex", gap: 8, alignItems: "center" }}>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() =>
              invoke("resume_session", {
                transcriptPath: agent.transcriptPath,
                cwd: agent.cwd ?? "",
              }).catch(console.error)
            }
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover, var(--bg-overlay))";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            style={{
              background: "var(--bg-surface-hover, var(--bg-overlay))",
              border: "1px solid var(--border-subtle)",
              borderRadius: 5,
              color: "var(--text-primary)",
              fontSize: 11,
              fontWeight: 600,
              padding: "4px 10px",
              cursor: "pointer",
              transition: "background 100ms",
            }}
          >
            ▶ Resume
          </button>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() =>
              invoke("dismiss_agent", { id: agent.transcriptPath }).catch(console.error)
            }
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "none";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
            }}
            style={{
              background: "none",
              border: "1px solid var(--border-subtle)",
              borderRadius: 5,
              color: "var(--text-muted)",
              fontSize: 11,
              fontWeight: 600,
              padding: "4px 10px",
              cursor: "pointer",
              marginLeft: "auto",
              transition: "background 100ms, color 100ms",
            }}
          >
            Dismiss
          </button>
        </div>
      )}

      {/* Interruption : gaté sur l'outil (6.33). `sourceTool === "unknown"` → `includes` false → pas
          d'affordance (correct : on ne sait pas qui consomme le flag). Voir INTERRUPT_SUPPORTED. */}
      {agent.state === "working" && INTERRUPT_SUPPORTED.includes(agent.sourceTool as StatsTool) && (
        <div
          style={{
            flexShrink: 0,
            borderTop: "1px solid var(--border-subtle)",
            padding: "8px 12px",
            display: "flex",
            flexDirection: "column",
            gap: 6,
          }}
        >
          {agent.interruptPending ? (
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                padding: "6px 10px",
                background: "var(--color-amber-bg)",
                borderRadius: 6,
                fontSize: 12,
                color: "var(--color-amber)",
              }}
            >
              <span>⚡ Interrupt requested — waiting for next tool call…</span>
              <button
                onClick={handleCancelInterrupt}
                onMouseDown={(e) => e.stopPropagation()}
                style={{
                  background: "none",
                  border: "none",
                  cursor: "pointer",
                  color: "var(--color-amber)",
                  fontSize: 13,
                  padding: "0 4px",
                  lineHeight: 1,
                }}
                title="Cancel interrupt"
              >
                ×
              </button>
            </div>
          ) : (
            <>
              <textarea
                value={interruptReason}
                onChange={(e) => setInterruptReason(e.target.value)}
                onMouseDown={(e) => e.stopPropagation()}
                placeholder="Reason (optional)…"
                rows={1}
                style={{
                  background: "var(--bg-input-subtle)",
                  border: "1px solid var(--border-subtle)",
                  borderRadius: 5,
                  color: "var(--text-primary)",
                  fontSize: 12,
                  padding: "5px 8px",
                  resize: "none",
                  outline: "none",
                  fontFamily: "inherit",
                  width: "100%",
                  boxSizing: "border-box",
                }}
              />
              <button
                onClick={handleInterrupt}
                onMouseDown={(e) => e.stopPropagation()}
                disabled={isInterrupting}
                style={{
                  alignSelf: "flex-start",
                  background: "var(--color-amber-bg)",
                  border: "1px solid transparent",
                  borderRadius: 5,
                  color: "var(--color-amber)",
                  fontSize: 11,
                  fontWeight: 600,
                  padding: "4px 10px",
                  cursor: isInterrupting ? "default" : "pointer",
                  opacity: isInterrupting ? 0.5 : 1,
                  transition: "background 100ms",
                }}
                onMouseEnter={(e) => {
                  if (!isInterrupting)
                    (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-warning)";
                }}
                onMouseLeave={(e) => {
                  (e.currentTarget as HTMLButtonElement).style.background = "var(--color-amber-bg)";
                }}
              >
                {isInterrupting ? "⚡ Requesting…" : "⚡ Request interrupt"}
              </button>
            </>
          )}
        </div>
      )}

      <div
        style={{
          flexShrink: 0,
          borderTop: "1px solid var(--border-subtle)",
          padding: "5px 12px",
          fontSize: 11,
          color: "var(--text-secondary)",
          display: "flex",
          alignItems: "center",
          gap: 4,
        }}
      >
        <div style={{ flex: 1, display: "flex", alignItems: "center", gap: 4 }}>
          {statsLoading ? (
            <span>…</span>
          ) : stats ? (
            <>
              <span>in: {fmtTokens(stats.inputTokens)}</span>
              <span>·</span>
              <span>out: {fmtTokens(stats.outputTokens)}</span>
              <span>·</span>
              <span>cache: {fmtTokens(stats.cacheTokens)}</span>
              <span>·</span>
              <span>{fmtCost(stats.costUsd)}</span>
              {stats.durationDays != null && (
                <>
                  <span>·</span>
                  <span>{stats.durationDays}j</span>
                </>
              )}
            </>
          ) : (
            <span>— · — · — · —</span>
          )}
        </div>
        {agent.state !== "disconnected" && agent.state !== "unknown" && (
          <>
            {/* Story 8.2 — même gate d'état que « Permissions », même popover ancré au footer.
                `AgentNotifMenu` se rend lui-même `null` (conteneur d'ancrage COMPRIS) si les
                notifications sont globalement coupées : le bouton est alors absent du DOM, pas
                grisé, et il ne reste aucune gouttière fantôme dans ce footer flex.

                Revue 8.2 — les deux popovers sont ancrés au même coin (`bottom:44 / right:8`,
                280 px de large dans un panneau de 480) : ouverts ensemble ils se recouvrent et le
                dernier rendu masque l'autre. `openMenu` est donc l'arbitre unique — ouvrir l'un
                ferme l'autre. */}
            <AgentNotifMenu
              agent={agent}
              open={openMenu === "notifs"}
              onOpenChange={(o) => setOpenMenu(o ? "notifs" : null)}
            />
            <div style={{ position: "relative" }}>
              <AgentPermissionMenu
                agent={agent}
                open={openMenu === "perms"}
                onOpenChange={(o) => setOpenMenu(o ? "perms" : null)}
              />
            </div>
          </>
        )}
      </div>

      <UsageBar />
    </div>
  );
}
