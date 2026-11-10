import { useState, useEffect, useRef } from "react";
import type { AgentState } from "../../types/agent";
import {
  mascotVariants,
  DEFAULT_MASCOT_VARIANT,
  type MascotVariantId,
} from "./mascotVariants";
import "./MascotPixel.css";

interface Props {
  state: AgentState;
  size?: number;
  interruptPending?: boolean;
  variant?: MascotVariantId;
}

export function stateToVar(state: AgentState): string {
  switch (state) {
    case "working":
      return "var(--state-working)";
    case "waitingApprove":
      return "var(--state-waiting-approve)";
    case "waitingSpecific":
      return "var(--state-waiting-specific)";
    case "ended":
      return "var(--state-ended)";
    case "idle":
      return "var(--state-idle)";
    case "error":
      return "var(--state-error)";
    case "disconnected":
      return "var(--state-disconnected)";
    case "unknown":
      return "var(--state-unknown)";
    default:
      return "var(--state-unknown)";
  }
}

// États sans animation en boucle — transition appliquée immédiatement
const IMMEDIATE_STATES = new Set<AgentState>(["idle", "disconnected"]);

export function MascotPixel({
  state,
  size = 28,
  interruptPending = false,
  variant = DEFAULT_MASCOT_VARIANT,
}: Props) {
  // Aiguillage data-driven : lecture indexée du registre, aucune branche par variante.
  const geo = mascotVariants[variant] ?? mascotVariants[DEFAULT_MASCOT_VARIANT];
  const [displayState, setDisplayState] = useState<AgentState>(state);
  const isAnimating = useRef(false);
  const pendingState = useRef<AgentState | null>(null);

  // P5 — calculé une seule fois par rendu, partagé entre useEffect et JSX
  const prefersReducedMotion =
    typeof window !== "undefined" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const prefersReducedMotionRef = useRef(prefersReducedMotion);
  prefersReducedMotionRef.current = prefersReducedMotion;

  useEffect(() => {
    // P3 — vérifier l'état ENTRANT (state), pas l'état affiché
    if (prefersReducedMotionRef.current || IMMEDIATE_STATES.has(state) || !isAnimating.current) {
      setDisplayState(state);
    } else {
      pendingState.current = state;
    }
  }, [state]);

  // P2 — guard contre les events bubblés depuis les éléments enfants animés
  const handleAnimationStart = (e: React.AnimationEvent<SVGSVGElement>) => {
    if (e.target !== e.currentTarget) return;
    isAnimating.current = true;
  };

  const handleAnimationIteration = (e: React.AnimationEvent<SVGSVGElement>) => {
    if (e.target !== e.currentTarget) return;
    if (pendingState.current !== null) {
      // P1 — capturer + vider avant setDisplayState pour éviter la race
      const next = pendingState.current;
      pendingState.current = null;
      setDisplayState(next);
      isAnimating.current = false;
    }
  };

  const handleAnimationEnd = (e: React.AnimationEvent<SVGSVGElement>) => {
    if (e.target !== e.currentTarget) return;
    isAnimating.current = false;
    if (pendingState.current !== null) {
      setDisplayState(pendingState.current);
      pendingState.current = null;
    }
  };

  const isWaiting = displayState === "waitingApprove" || displayState === "waitingSpecific";
  const isIdle = displayState === "idle";
  const isDisconnected = displayState === "disconnected";

  // Classe CSS appliquée sur l'élément <svg> lui-même (animation du personnage entier)
  const svgAnimClass = (() => {
    switch (displayState) {
      case "waitingApprove":
      case "waitingSpecific": return "mascot-waiting";
      case "ended":           return "mascot-ended";
      case "error":           return "mascot-error";
      case "unknown":         return "mascot-unknown";
      default:                return "";
    }
  })();

  // Classe CSS sur le groupe corps (bob vertical en état working)
  const bodyClass = displayState === "working" ? "mascot-walk-body" : "";

  // Classes CSS sur les groupes pattes (alternance L/R en steps(1))
  const legLeftClass  = displayState === "working" ? "mascot-walk-l" : "";
  const legRightClass = displayState === "working" ? "mascot-walk-r" : "";

  // Classe CSS sur le bras droit (levé en état waiting)
  const armRightClass = isWaiting ? "mascot-arm-raise" : "";

  // Filtre drop-shadow et opacité appliqués sur le <svg>
  // rgb(from ...) : CSS relative colors — préserve la teinte, réduit l'alpha
  const svgStyle: React.CSSProperties = {
    imageRendering: "pixelated",
    color: stateToVar(displayState),
    transition: "color 300ms ease-out",
    overflow: "visible",
    transformOrigin: "bottom center", // pivot pour sway, bounce, shake
    display: "block",
    ...(displayState === "working" && {
      filter: "drop-shadow(0 0 8px rgb(from var(--state-working) r g b / 0.27))",
    }),
    ...(isWaiting && {
      filter: `drop-shadow(0 0 10px rgb(from ${stateToVar(displayState)} r g b / 0.33))`,
    }),
    ...(displayState === "ended" && {
      filter: "drop-shadow(0 0 6px rgb(from var(--state-ended) r g b / 0.20))",
    }),
    ...(displayState === "error" && {
      filter: "drop-shadow(0 0 8px rgb(from var(--state-error) r g b / 0.33))",
    }),
    ...(isIdle && { opacity: 0.55 }),
    ...(isDisconnected && { opacity: 0.85 }),
    ...(prefersReducedMotion && displayState === "unknown" && { opacity: 0.55 }),
  };

  // height = ratio viewBox 9/14 (ratio exact de "0 -1 14 9")
  const svgHeight = size * (9 / 14);

  return (
    <div style={{ position: "relative", display: "inline-block" }}>
      {interruptPending && (
        <div
          aria-hidden="true"
          style={{
            position: "absolute",
            top: -3,
            right: -4,
            fontSize: 9,
            color: "var(--color-amber)",
            lineHeight: 1,
            pointerEvents: "none",
          }}
        >
          ⚡
        </div>
      )}
      <svg
        viewBox="0 -1 14 9"
        width={size}
        height={svgHeight}
        shapeRendering="crispEdges"
        className={svgAnimClass}
        style={svgStyle}
        onAnimationStart={handleAnimationStart}
        onAnimationIteration={handleAnimationIteration}
        onAnimationEnd={handleAnimationEnd}
      >
        {/* Pattes — dessinées EN PREMIER (le corps les recouvre à la jonction) */}
        <g className={legLeftClass}>
          {geo.legsLeft.map((r, i) => (
            <rect key={`ll${i}`} x={r.x} y={r.y} width={r.w} height={r.h} fill="currentColor" />
          ))}
        </g>
        <g className={legRightClass}>
          {geo.legsRight.map((r, i) => (
            <rect key={`lr${i}`} x={r.x} y={r.y} width={r.w} height={r.h} fill="currentColor" />
          ))}
        </g>

        {/* Corps + bras + yeux. Pour grok : cornes + patte centrale sont dans `body`. */}
        <g className={bodyClass}>
          {geo.body.map((r, i) => (
            <rect key={`b${i}`} x={r.x} y={r.y} width={r.w} height={r.h} fill="currentColor" />
          ))}
          {/* Bras gauche — statique */}
          {geo.armLeft.map((r, i) => (
            <rect key={`al${i}`} x={r.x} y={r.y} width={r.w} height={r.h} fill="currentColor" />
          ))}
          {/* Bras droit — levé si waiting via classe mascot-arm-raise */}
          {geo.armRight.map((r, i) => (
            <rect
              key={`ar${i}`}
              className={armRightClass}
              x={r.x} y={r.y} width={r.w} height={r.h}
              fill="currentColor"
            />
          ))}
          {/* Yeux — rects PLEINS couleur du fond du Notch, dessinés PAR-DESSUS le corps.
              Étant dans le groupe corps, ils suivent le bob de la marche ; étant pleins et
              au-dessus, ils masquent toute patte qui passe derrière pendant la marche. */}
          {geo.eyes.map((r, i) => (
            <rect key={`eye${i}`} x={r.x} y={r.y} width={r.w} height={r.h} fill="var(--bg-notch)" />
          ))}
        </g>
      </svg>

      {isIdle && (
        <div className="mascot-zzz" aria-hidden="true">
          <span className="mascot-z mascot-z-1">z</span>
          <span className="mascot-z mascot-z-2">Z</span>
          <span className="mascot-z mascot-z-3">Z</span>
        </div>
      )}
    </div>
  );
}
