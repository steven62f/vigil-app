import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { useAgentStore } from "../stores/agentStore";
import type { AgentSnapshot } from "../types/agent";

export function useTauriEvents() {
  const setAgents = useAgentStore((s) => s.setAgents);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    listen<AgentSnapshot[]>("agents:snapshot", (event) => {
      setAgents(event.payload);
    }).then((fn) => {
      if (cancelled) {
        fn();
      } else {
        unlisten = fn;
      }
    }).catch((err) => {
      console.error("[useTauriEvents] Failed to subscribe to agents:snapshot:", err);
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [setAgents]);
}
