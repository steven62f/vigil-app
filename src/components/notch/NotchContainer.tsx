import { useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { motion, AnimatePresence } from "motion/react";
import type { AgentSnapshot } from "../../types/agent";
import { AgentCell } from "./AgentCell";
import { OverflowCounter } from "./OverflowCounter";
import { ListBadge } from "./ListBadge";
import { AgentRow } from "../list/AgentRow";
import { useSettings } from "../../hooks/useSettings";
import { useNavigation } from "../../hooks/NavigationProvider";
import { selectPriorityAgent, applyQuota, PRIORITY_ORDER } from "../../lib/agentDisplay";
import { errorMessages } from "../../lib/errorMessages";
import { UsageBar, useUsageBarHeight, useUsageBarPosition } from "./UsageBar";

const GAP = 8;
export const MIN_W = 160;
const PADDING_H = 32;
const DETAILED_W = 380; // largeur de contenu AgentRow en mode détaillé (choix de design)
const ANIM_DURATION = 200;
const ease = (t: number) => 1 - Math.pow(1 - t, 3);

const PLACEHOLDER_AGENT: AgentSnapshot = {
  transcriptPath: "__placeholder__",
  sessionId: null,
  name: "Waiting...",
  state: "idle",
  waitingSince: null,
  lastActivity: 0,
  lastActivityLabel: null,
  terminalPid: null,
  source: "terminal",
  sourceTool: "unknown",
};

interface Props {
  agents: AgentSnapshot[];
  onOpenList?: () => void;
  onWidthChange?: (width: number) => void;
}

export function NotchContainer({ agents, onOpenList, onWidthChange }: Props) {
  const settings = useSettings();
  const { openDetail } = useNavigation();
  const barH = useUsageBarHeight();
  const barPos = useUsageBarPosition();
  const barHRef = useRef(barH);
  barHRef.current = barH;
  const containerRef = useRef<HTMLDivElement>(null);
  const innerRef = useRef<HTMLDivElement>(null);
  const detailedRef = useRef<HTMLDivElement>(null);
  const currentWRef = useRef<number | null>(null);
  const currentHRef = useRef<number>(48);
  const animRafRef = useRef<number | null>(null);

  // Vue détaillée : priority-agent (toujours 1 visible) ou multi-agent avec 1 seul agent
  const isDetailed =
    settings.detailedSingleAgent &&
    agents.length > 0 &&
    (settings.notchDisplayMode === "priority-agent" || agents.length === 1);

  const isDetailedRef = useRef(isDetailed);
  isDetailedRef.current = isDetailed;

  // En mode détaillé priority-agent avec plusieurs agents : afficher le compteur +N
  const detailedHasOverflow =
    isDetailed &&
    settings.notchDisplayMode === "priority-agent" &&
    agents.length > 1;
  const detailedHasOverflowRef = useRef(detailedHasOverflow);
  detailedHasOverflowRef.current = detailedHasOverflow;

  // Agent à afficher en mode détaillé
  const visibleAgent = isDetailed
    ? settings.notchDisplayMode === "priority-agent"
      ? selectPriorityAgent(agents)
      : (agents[0] ?? null)
    : null;

  // Calculs mode normal (utilisés uniquement si !isDetailed)
  let visible: AgentSnapshot[] = [];
  let overflow = 0;
  let showListBadge = false;
  if (agents.length > 0 && !isDetailed) {
    if (settings.notchDisplayMode === "priority-agent") {
      const top = selectPriorityAgent(agents);
      visible = top ? [top] : [];
      overflow = agents.length > 1 ? agents.length - 1 : 0;
    } else {
      const ordered =
        agents.length > settings.maxVisibleAgents
          ? [...agents].sort(
              (a, b) => (PRIORITY_ORDER[a.state] ?? 999) - (PRIORITY_ORDER[b.state] ?? 999)
            )
          : agents;
      const quota = applyQuota(ordered, settings.maxVisibleAgents);
      visible = quota.visible;
      overflow = quota.overflow;
      if (overflow === 0 && agents.length > 1) showListBadge = true;
    }
  }

  // Extrait en useCallback pour être partagé entre les deux effects ci-dessous
  const applyResize = useCallback((targetW: number, targetH: number) => {
    if (animRafRef.current !== null) cancelAnimationFrame(animRafRef.current);

    // Premier rendu : appliqué immédiatement, sans animation
    if (currentWRef.current === null) {
      currentWRef.current = targetW;
      currentHRef.current = targetH;
      invoke("resize_notch", { width: targetW, height: targetH }).catch(console.error);
      onWidthChange?.(targetW);
      return;
    }

    const startW = currentWRef.current;
    const startH = currentHRef.current;
    if (Math.abs(targetW - startW) < 1 && Math.abs(targetH - startH) < 1) return;

    const startTime = performance.now();

    const animate = (now: number) => {
      const t = Math.min((now - startTime) / ANIM_DURATION, 1);
      const w = Math.round(startW + (targetW - startW) * ease(t));
      const h = Math.round(startH + (targetH - startH) * ease(t));
      currentWRef.current = w;
      currentHRef.current = h;
      invoke("resize_notch", { width: w, height: h }).catch(console.error);
      if (t < 1) {
        animRafRef.current = requestAnimationFrame(animate);
      } else {
        animRafRef.current = null;
        currentWRef.current = targetW;
        currentHRef.current = targetH;
        onWidthChange?.(targetW);
      }
    };
    animRafRef.current = requestAnimationFrame(animate);
  }, [onWidthChange]);

  // ResizeObserver : détecte les changements de taille du contenu
  useEffect(() => {
    const resize = () => {
      if (isDetailedRef.current) {
        const container = containerRef.current;
        const detailed = detailedRef.current;
        if (!container || !detailed) return;
        const h = container.offsetHeight;
        const w = detailed.offsetWidth;
        if (h > 0 && w > 0) applyResize(Math.max(w + PADDING_H, MIN_W), h);
      } else {
        const inner = innerRef.current;
        if (!inner || !inner.offsetWidth) return;
        const w = Math.max(inner.offsetWidth + PADDING_H, MIN_W);
        applyResize(w, 48 + barHRef.current);
      }
    };

    const ro = new ResizeObserver(resize);
    if (innerRef.current) ro.observe(innerRef.current);
    if (containerRef.current) ro.observe(containerRef.current);
    if (detailedRef.current) ro.observe(detailedRef.current);
    resize();

    return () => {
      ro.disconnect();
      if (animRafRef.current !== null) cancelAnimationFrame(animRafRef.current);
    };
  }, [applyResize]);

  // Déclenchement direct quand isDetailed ou le compteur change.
  // Le ResizeObserver ne suffit pas : containerRef.offsetWidth = largeur fenêtre (circulaire),
  // et detailedRef peut être null au moment où l'observer a été créé.
  useEffect(() => {
    if (!isDetailed) return;
    const container = containerRef.current;
    const detailed = detailedRef.current;
    if (!container || !detailed) return;
    const h = container.offsetHeight;
    const w = detailed.offsetWidth;
    if (h <= 0 || w <= 0) return;
    applyResize(Math.max(w + PADDING_H, MIN_W), h);
  }, [isDetailed, detailedHasOverflow, applyResize]);

  return (
    <div
      ref={containerRef}
      data-tauri-drag-region
      style={{
        position: "relative",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        ...(isDetailed ? {} : { height: 48 + barH }),
        paddingTop: barPos === "top" ? barH : 0,
        paddingBottom: barPos === "bottom" ? barH : 0,
        boxSizing: "border-box",
        background: "var(--bg-notch)",
        borderRadius: "0 0 14px 14px",
        overflow: "visible",
        cursor: "grab",
      }}
    >
      {/* Mode normal — toujours dans le DOM, masqué en mode détaillé pour conserver la ref */}
      <div
        ref={innerRef}
        style={{
          display: isDetailed ? "none" : "flex",
          alignItems: "center",
          gap: GAP,
          padding: "0 8px",
        }}
      >
        {agents.length === 0 ? (
          <AgentCell agent={PLACEHOLDER_AGENT} showStateLabel={false} />
        ) : (
          <>
            <AnimatePresence mode="popLayout">
              {visible.map((agent) => (
                <motion.div
                  key={agent.transcriptPath}
                  initial={{ opacity: 0, scale: 0.9 }}
                  animate={{ opacity: 1, scale: 1, transition: { type: "spring", duration: 0.35 } }}
                  exit={{ opacity: 0, scale: 0.9, transition: { duration: 0.25, ease: "easeIn" } }}
                  style={{ display: "flex" }}
                >
                  <AgentCell agent={agent} />
                </motion.div>
              ))}
            </AnimatePresence>
            {overflow > 0 && <OverflowCounter count={overflow} onClick={onOpenList} />}
            {showListBadge && <ListBadge onClick={onOpenList} />}
          </>
        )}
      </div>

      <UsageBar />

      {/* Mode détaillé mono-agent */}
      {isDetailed && visibleAgent && (
        <div ref={detailedRef} style={{ display: "flex", alignItems: "center", gap: GAP }}>
          <div style={{ width: DETAILED_W }}>
            <AgentRow
              agent={visibleAgent}
              onApprove={() =>
                invoke("approve_agent", { id: visibleAgent.transcriptPath })
                  .catch((err) => console.error("[vigil] approve_agent:", errorMessages[err as string] ?? err))
              }
              onDeny={() =>
                invoke("deny_agent", { id: visibleAgent.transcriptPath })
                  .catch((err) => console.error("[vigil] deny_agent:", errorMessages[err as string] ?? err))
              }
              onRowClick={() => openDetail(visibleAgent.transcriptPath, false)}
              onDismiss={() =>
                invoke("dismiss_agent", { id: visibleAgent.transcriptPath }).catch(console.error)
              }
            />
          </div>
          {detailedHasOverflow && (
            <OverflowCounter count={agents.length - 1} onClick={onOpenList} />
          )}
        </div>
      )}
    </div>
  );
}
