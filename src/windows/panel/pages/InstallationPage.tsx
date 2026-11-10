import React, { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { errorMessages } from "../../../lib/errorMessages";
import type { ToolsStatus, ToolHookDiff } from "../../../types/tools";
import { ToolCard } from "./ToolCard";
import { DiffModal } from "./DiffModal";
import { useSettings } from "../../../hooks/useSettings";
import { saveSettings } from "../../../lib/settingsStore";
import { DEFAULT_MASCOT_VARIANT, type MascotVariantId } from "../../../components/shared/mascotVariants";

type ToolId = "claude_code" | "codex" | "grok" | "antigravity" | "kimi";

interface PendingToggle {
  tool: ToolId;
  newValue: boolean;
  diff: ToolHookDiff;
}

function getToolInfo(status: ToolsStatus, toolId: ToolId) {
  const map = {
    claude_code: {
      detected: status.claudeCodeDetected,
      enabled: status.claudeCodeEnabled,
      hooksOk: status.claudeCodeHooksOk,
    },
    codex: {
      detected: status.codexDetected,
      enabled: status.codexEnabled,
      hooksOk: status.codexHooksOk,
    },
    grok: {
      detected: status.grokDetected,
      enabled: status.grokEnabled,
      hooksOk: status.grokHooksOk,
    },
    antigravity: {
      detected: status.antigravityDetected,
      enabled: status.antigravityEnabled,
      hooksOk: status.antigravityHooksOk,
    },
    kimi: {
      detected: status.kimiDetected,
      enabled: status.kimiEnabled,
      hooksOk: status.kimiHooksOk,
    },
  };
  return map[toolId];
}

const ALL_TOOLS: ToolId[] = ["claude_code", "codex", "grok", "antigravity", "kimi"];

export function InstallationPage() {
  const settings = useSettings();
  const [status, setStatus] = useState<ToolsStatus | null>(null);
  const [pendingToggle, setPendingToggle] = useState<PendingToggle | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<ToolsStatus>("get_tools_status")
      .then(setStatus)
      .catch(() => setError("Failed to load tools status."));
  }, []);

  async function handleToggle(tool: ToolId, newValue: boolean) {
    if (!status) return;
    setError(null);
    setLoading(true);
    try {
      const action = newValue ? "install" : "uninstall";
      const diff = await invoke<ToolHookDiff>("get_tool_hook_diff", { tool, action });
      setPendingToggle({ tool, newValue, diff });
    } catch (e: unknown) {
      const key = typeof e === "string" ? e : "hook_install_failed";
      setError(errorMessages[key] ?? errorMessages.hook_install_failed);
    } finally {
      setLoading(false);
    }
  }

  async function handleConfirm() {
    if (!pendingToggle || !status) return;
    setLoading(true);
    setError(null);
    try {
      await invoke("set_tool_enabled", { tool: pendingToggle.tool, enabled: pendingToggle.newValue });
      // Propage la (dé)activation aux autres fenêtres — notamment le Notch (overlay always-on-top
      // qui ne reçoit pas de façon fiable focus/visibilitychange) : `useActivatedTools` re-fetch
      // `get_tools_status` sur cet événement → bascule/repli d'outil de la barre sans rechargement
      // (Story 6.13, AC6). Événement dédié : `vigil:settings-changed` porte un payload VigilSettings
      // complet appliqué tel quel par SettingsProvider, il ne doit pas être émis à vide ici.
      emit("vigil:tools-changed").catch(() => {});
    } catch (e: unknown) {
      const key = typeof e === "string" ? e : "hook_install_failed";
      setError(errorMessages[key] ?? errorMessages.hook_install_failed);
    } finally {
      setPendingToggle(null);
      invoke<ToolsStatus>("get_tools_status").then(setStatus).catch(() => {});
      setLoading(false);
    }
  }

  function handleCancel() {
    setPendingToggle(null);
  }

  // Persiste la mascotte choisie pour un outil. saveSettings émet vigil:settings-changed
  // → SettingsProvider re-render automatiquement la page ET le notch (pas de state local).
  function handleMascotChange(tool: ToolId, variant: MascotVariantId) {
    saveSettings({ mascotByTool: { ...settings.mascotByTool, [tool]: variant } });
  }

  const detected = status ? ALL_TOOLS.filter((t) => getToolInfo(status, t).detected) : [];
  const undetected = status ? ALL_TOOLS.filter((t) => !getToolInfo(status, t).detected) : [];

  return (
    <div
      style={{
        padding: 24,
        // Le modal de consentement est en `position: fixed` : il reste à l'écran, mais la molette
        // continuerait de faire défiler CETTE liste sous lui (l'événement remonte jusqu'ici).
        // On gèle donc le défilement tant qu'une décision est en attente — le modal « prend le
        // dessus » réellement. `overflow: hidden` préserve la position de défilement courante :
        // à la fermeture, on retrouve l'outil sur lequel on venait de cliquer.
        overflowY: pendingToggle ? "hidden" : "auto",
        height: "100%",
        boxSizing: "border-box",
        display: "flex",
        flexDirection: "column",
        gap: 16,
        color: "var(--text-primary)",
        position: "relative",
      }}
    >
      <h2 style={{ margin: 0, fontSize: 18 }}>Installation</h2>

      {error && (
        <p style={{ margin: 0, fontSize: 13, color: "var(--color-danger)" }}>{error}</p>
      )}

      {!status && !error && (
        <p style={{ margin: 0, color: "var(--text-secondary)", fontSize: 13 }}>Loading…</p>
      )}

      {status && (
        <>
          {detected.length > 0 && (
            <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
              {detected.map((toolId) => {
                const info = getToolInfo(status, toolId);
                return (
                  <ToolCard
                    key={toolId}
                    toolId={toolId}
                    detected={info.detected}
                    enabled={info.enabled}
                    hooksOk={info.hooksOk}
                    disabled={loading}
                    onToggle={(newVal) => handleToggle(toolId, newVal)}
                    onRepair={() => handleToggle(toolId, true)}
                    mascotVariant={settings.mascotByTool[toolId] ?? DEFAULT_MASCOT_VARIANT}
                    onMascotChange={(v) => handleMascotChange(toolId, v)}
                  />
                );
              })}
            </div>
          )}

          {undetected.length > 0 && (
            <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
              <p style={{ margin: 0, fontSize: 11, color: "var(--text-muted)", fontWeight: 600, textTransform: "uppercase", letterSpacing: "0.05em" }}>
                Not installed
              </p>
              {undetected.map((toolId) => {
                const info = getToolInfo(status, toolId);
                return (
                  <ToolCard
                    key={toolId}
                    toolId={toolId}
                    detected={info.detected}
                    enabled={info.enabled}
                    hooksOk={info.hooksOk}
                    disabled={loading}
                    onToggle={(newVal) => handleToggle(toolId, newVal)}
                    onRepair={() => handleToggle(toolId, true)}
                    mascotVariant={settings.mascotByTool[toolId] ?? DEFAULT_MASCOT_VARIANT}
                    onMascotChange={(v) => handleMascotChange(toolId, v)}
                  />
                );
              })}
            </div>
          )}
        </>
      )}

      {pendingToggle && (
        <DiffModal
          diff={pendingToggle.diff}
          onConfirm={handleConfirm}
          onCancel={handleCancel}
          loading={loading}
        />
      )}
    </div>
  );
}
