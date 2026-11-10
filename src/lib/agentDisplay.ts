import type { AgentSnapshot, AgentState } from "../types/agent";

export function toStateLabel(state: AgentState): string {
  switch (state) {
    case "working":          return "WORKING";
    case "waitingApprove":   return "WAITING";
    case "waitingSpecific":  return "WAITING";
    case "ended":            return "ENDED";
    case "idle":             return "IDLE";
    case "error":            return "ERROR";
    case "disconnected":     return "DISCONNECTED";
    case "unknown":          return "CONNECTING";
    default:                 return "CONNECTING";
  }
}

export const PRIORITY_ORDER: Record<AgentState, number> = {
  waitingApprove:  0,
  waitingSpecific: 1,
  error:           2,
  working:         3,
  ended:           4,
  idle:            5,
  disconnected:    6,
  unknown:         7,
};

export function selectPriorityAgent(agents: AgentSnapshot[]): AgentSnapshot | null {
  if (agents.length === 0) return null;
  return agents.reduce((best, agent) => {
    const stateDiff = PRIORITY_ORDER[agent.state] - PRIORITY_ORDER[best.state];
    if (stateDiff < 0) return agent;
    if (stateDiff > 0) return best;
    return agent.lastActivity > best.lastActivity ? agent : best;
  });
}

export function applyQuota(
  agents: AgentSnapshot[],
  max: number
): { visible: AgentSnapshot[]; overflow: number } {
  if (agents.length <= max) return { visible: agents, overflow: 0 };
  return { visible: agents.slice(0, max), overflow: agents.length - max };
}
