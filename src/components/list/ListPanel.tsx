import { useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot } from "../../types/agent";
import { PRIORITY_ORDER } from "../../lib/agentDisplay";
import { useNavigation } from "../../hooks/NavigationProvider";
import { AgentRow } from "./AgentRow";
import { errorMessages } from "../../lib/errorMessages";
import { UsageBar, useUsageBarHeight, useUsageBarPosition } from "../notch/UsageBar";

const PANEL_W = 500;
const DURATION = 220;
const ease = (t: number) => 1 - Math.pow(1 - t, 3);

interface Props {
  agents: AgentSnapshot[];
  onClose: () => void;
  notchWidth: number;
}

export function ListPanel({ agents, onClose, notchWidth }: Props) {
  const panelRef = useRef<HTMLDivElement>(null);
  const { openDetail } = useNavigation();
  const barH = useUsageBarHeight();
  const barPos = useUsageBarPosition();
  const closedH = 48 + barH;

  // `vh` est relatif à la hauteur de la fenêtre Tauri (48px au départ) → toujours 0.
  // On utilise screen.availHeight pour obtenir la hauteur réelle de l'écran.
  const maxListHeight = Math.max(200, Math.floor(window.screen.availHeight * 0.8) - 48);

  // Dimensions courantes pendant les animations (partagé ouverture ↔ fermeture)
  const sizeRef = useRef({ w: notchWidth, h: closedH });
  const closedHRef = useRef(closedH);
  closedHRef.current = closedH;
  // RAF de l'animation d'ouverture — annulable si fermeture démarre avant la fin
  const openRafRef = useRef<number | null>(null);
  // RAF de l'animation de fermeture — annulable au démontage
  const closeRafRef = useRef<number | null>(null);
  const isClosingRef = useRef(false);
  // Capture notchWidth au montage pour les deux animations
  const notchWidthRef = useRef(notchWidth);

  const sortedAgents = [...agents].sort(
    (a, b) => (PRIORITY_ORDER[a.state] ?? 999) - (PRIORITY_ORDER[b.state] ?? 999)
  );

  // Animation d'ouverture (largeur + hauteur) + ResizeObserver pour mises à jour dynamiques
  useEffect(() => {
    const el = panelRef.current;
    if (!el) return;

    const startW = notchWidthRef.current;
    let animating = true;

    const startAnimation = () => {
      const targetH = el.offsetHeight;
      if (targetH <= 0) return;
      const startTime = performance.now();

      const animate = (now: number) => {
        const t = Math.min((now - startTime) / DURATION, 1);
        const w = Math.round(startW + (PANEL_W - startW) * ease(t));
        const startH = closedHRef.current;
        const h = Math.round(startH + (targetH - startH) * ease(t));
        sizeRef.current = { w, h };
        invoke("resize_notch", { width: w, height: h }).catch(console.error);
        if (t < 1) {
          openRafRef.current = requestAnimationFrame(animate);
        } else {
          openRafRef.current = null;
          animating = false;
        }
      };
      openRafRef.current = requestAnimationFrame(animate);
    };

    openRafRef.current = requestAnimationFrame(startAnimation);

    const ro = new ResizeObserver(() => {
      if (animating) return;
      const h = el.offsetHeight;
      if (h > 0) {
        sizeRef.current = { w: PANEL_W, h };
        invoke("resize_notch", { width: PANEL_W, height: h }).catch(console.error);
      }
    });
    ro.observe(el);

    return () => {
      if (openRafRef.current !== null) cancelAnimationFrame(openRafRef.current);
      ro.disconnect();
      animating = false;
      isClosingRef.current = false;
    };
  }, []);

  // Animation de fermeture (largeur + hauteur → notch), puis démontage
  const handleClose = useCallback(() => {
    if (isClosingRef.current) return;
    isClosingRef.current = true;

    // Annuler l'animation d'ouverture si elle est encore en cours
    if (openRafRef.current !== null) {
      cancelAnimationFrame(openRafRef.current);
      openRafRef.current = null;
    }

    const startW = sizeRef.current.w;
    const startH = sizeRef.current.h;
    const targetW = notchWidthRef.current;
    const startTime = performance.now();

    const animate = (now: number) => {
      const t = Math.min((now - startTime) / DURATION, 1);
      const w = Math.round(startW + (targetW - startW) * ease(t));
      const endH = closedHRef.current;
      const h = Math.round(startH + (endH - startH) * ease(t));
      invoke("resize_notch", { width: w, height: h }).catch(console.error);
      if (t < 1) {
        closeRafRef.current = requestAnimationFrame(animate);
      } else {
        closeRafRef.current = null;
        onClose();
      }
    };
    closeRafRef.current = requestAnimationFrame(animate);
  }, [onClose]);

  useEffect(() => {
    window.addEventListener("blur", handleClose);
    return () => {
      window.removeEventListener("blur", handleClose);
      if (closeRafRef.current !== null) cancelAnimationFrame(closeRafRef.current);
    };
  }, [handleClose]);

  useEffect(() => {
    if (agents.length === 0) handleClose();
  }, [agents.length, handleClose]);

  return (
    <div
      ref={panelRef}
      onMouseDown={(e) => e.stopPropagation()}
      style={{
        position: "relative",
        background: "var(--bg-notch)",
        borderRadius: "0 0 16px 16px",
        boxShadow: "0 8px 32px rgba(0,0,0,0.4)",
        width: PANEL_W,
        overflow: "hidden",
        paddingTop: barPos === "top" ? barH : 0,
        paddingBottom: barPos === "bottom" ? 6 + barH : 6,
      }}
    >
      <div
        style={{
          padding: "10px 18px 9px",
          borderBottom: "1px solid var(--border-subtle)",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
        }}
      >
        <span
          style={{
            fontSize: 10,
            fontWeight: 700,
            color: "var(--text-muted)",
            textTransform: "uppercase",
            letterSpacing: "0.1em",
          }}
        >
          Active agents
        </span>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ fontSize: 10, color: "var(--text-muted)" }}>
            {agents.length} agent{agents.length > 1 ? "s" : ""}
          </span>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => invoke("show_history_window").catch(console.error)}
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface-hover)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-subtle)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
            }}
            style={{
              background: "var(--bg-subtle)",
              border: "1px solid var(--border-subtle)",
              borderRadius: 5,
              cursor: "pointer",
              color: "var(--text-muted)",
              fontSize: 11,
              fontWeight: 500,
              padding: "2px 8px",
              transition: "background 120ms, color 120ms",
            }}
          >
            History
          </button>
        </div>
      </div>

      <div style={{ overflowY: "auto", maxHeight: maxListHeight }}>
        {sortedAgents.map((agent) => (
          <AgentRow
            key={agent.transcriptPath}
            agent={agent}
            onApprove={() =>
              invoke("approve_agent", { id: agent.transcriptPath })
                .catch((err) => console.error("[vigil] approve_agent:", errorMessages[err as string] ?? err))
            }
            onDeny={() =>
              invoke("deny_agent", { id: agent.transcriptPath })
                .catch((err) => console.error("[vigil] deny_agent:", errorMessages[err as string] ?? err))
            }
            onRowClick={() => openDetail(agent.transcriptPath, true)}
            onDismiss={() =>
              invoke("dismiss_agent", { id: agent.transcriptPath }).catch(console.error)
            }
          />
        ))}
      </div>

      <UsageBar />
    </div>
  );
}
