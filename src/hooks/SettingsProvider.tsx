import { useState, useEffect, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { loadSettings, DEFAULT_SETTINGS, type VigilSettings, type AgentStateKey } from "../lib/settingsStore";
import { SettingsContext } from "./useSettings";

interface Props { children: ReactNode; }

const STATE_CSS_VARS: Record<AgentStateKey, string> = {
  working:         "--state-working",
  waitingApprove:  "--state-waiting-approve",
  waitingSpecific: "--state-waiting-specific",
  ended:           "--state-ended",
  idle:            "--state-idle",
  error:           "--state-error",
  disconnected:    "--state-disconnected",
};

function applyStateColors(settings: VigilSettings) {
  const root = document.documentElement;
  for (const [state, cssVar] of Object.entries(STATE_CSS_VARS) as [AgentStateKey, string][]) {
    const color = settings.stateSettings[state]?.color;
    if (color) root.style.setProperty(cssVar, color);
    else root.style.removeProperty(cssVar);
  }
}

export function SettingsProvider({ children }: Props) {
  const [settings, setSettings] = useState<VigilSettings>(DEFAULT_SETTINGS);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let eventReceived = false;

    // Subscribe before loading — captures events that fire during the async load
    listen<VigilSettings>("vigil:settings-changed", (event) => {
      eventReceived = true;
      setSettings(event.payload);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    }).catch((err) => {
      console.error("[SettingsProvider] Failed to subscribe:", err);
    });

    // Only apply loaded settings if no fresher event has arrived yet
    loadSettings()
      .then((s) => { if (!eventReceived) setSettings(s); })
      .catch((err) => console.error("[SettingsProvider] loadSettings failed:", err));

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    applyStateColors(settings);
  }, [settings]);

  return <SettingsContext.Provider value={settings}>{children}</SettingsContext.Provider>;
}
