import { create } from "zustand";
import type { AgentSnapshot } from "../types/agent";

interface AgentStore {
  agents: AgentSnapshot[];
  selectedTranscriptPath: string | null;
  setAgents: (agents: AgentSnapshot[]) => void;
  selectAgent: (transcriptPath: string | null) => void;
}

export const useAgentStore = create<AgentStore>((set) => ({
  agents: [],
  selectedTranscriptPath: null,
  setAgents: (agents) => set({ agents }),
  selectAgent: (transcriptPath) => set({ selectedTranscriptPath: transcriptPath }),
}));
