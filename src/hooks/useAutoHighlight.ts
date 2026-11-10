import { useEffect, useRef } from "react";
import { useAgentStore } from "../stores/agentStore";
import { useNavigation } from "./NavigationProvider";
import { useSettings } from "./useSettings";
import type { AgentState, AgentSnapshot } from "../types/agent";
import type { AgentStateKey } from "../lib/settingsStore";

const HIGHLIGHT_PRIORITY: AgentState[] = [
  "waitingApprove",
  "waitingSpecific",
  "error",
  "ended",
  "disconnected",
];

export function priorityOf(state: string): number {
  const idx = (HIGHLIGHT_PRIORITY as string[]).indexOf(state);
  return idx === -1 ? Infinity : idx;
}

/** États « en attente d'une décision de l'utilisateur » — les seuls résolvables hors de Vigil. */
export function isWaitingState(state: string): boolean {
  return state === "waitingApprove" || state === "waitingSpecific";
}

export function useAutoHighlight() {
  const agents = useAgentStore((s) => s.agents);
  const selectedTranscriptPath = useAgentStore((s) => s.selectedTranscriptPath);
  const { view, openDetail } = useNavigation();
  const settings = useSettings();

  const prevAgentsRef = useRef<AgentSnapshot[]>([]);
  // stateRef keeps latest nav/settings values fresh without them being effect deps
  const stateRef = useRef({ view, selectedTranscriptPath, openDetail, settings });
  stateRef.current = { view, selectedTranscriptPath, openDetail, settings };

  useEffect(() => {
    const { view, selectedTranscriptPath, openDetail, settings } = stateRef.current;
    const prevMap = new Map(prevAgentsRef.current.map((a) => [a.transcriptPath, a]));

    const candidates: AgentSnapshot[] = [];
    for (const agent of agents) {
      const prevState = prevMap.get(agent.transcriptPath)?.state;
      if (prevState === agent.state) continue;
      const cfg = settings.stateSettings[agent.state as AgentStateKey];
      if (cfg?.autoHighlight) {
        candidates.push(agent);
      }
    }

    if (candidates.length > 0) {
      candidates.sort((a, b) => priorityOf(a.state) - priorityOf(b.state));
      const candidate = candidates[0];

      const forWaiting = isWaitingState(candidate.state);

      if (view === "detail" && selectedTranscriptPath === candidate.transcriptPath) {
        // Same agent already open → no action (AC #1)
      } else if (view === "detail" && selectedTranscriptPath) {
        // Different agent open → switch only if candidate has strictly higher priority (AC #1)
        const currentAgent = agents.find((a) => a.transcriptPath === selectedTranscriptPath);
        const currentPriority = currentAgent ? priorityOf(currentAgent.state) : Infinity;
        if (priorityOf(candidate.state) < currentPriority) {
          openDetail(candidate.transcriptPath, false, forWaiting);
        }
      } else {
        // Not in detail view → open (AC #1)
        openDetail(candidate.transcriptPath, false, forWaiting);
      }
    }

    prevAgentsRef.current = agents;
  }, [agents]); // eslint-disable-line react-hooks/exhaustive-deps
}
