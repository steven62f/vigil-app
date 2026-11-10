import React, { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  loadSettings,
  saveSettings,
  DEFAULT_SETTINGS,
  type VigilSettings,
  type AgentStateKey,
  type StateSound,
  type ToolPermissionAction,
  type ToolPermissionKey,
  type UsageBarDisplay,
  type NotchUsageTool,
} from "../../lib/settingsStore";
import { useActivatedTools } from "../../hooks/useActivatedTools";
import { resolveNotchUsageTool, toolLabel, USAGE_SUPPORTED } from "../../lib/statsTools";
import {
  isGroupRecipient,
  pairingStep,
  notifErrorLabel,
  DEFAULT_NOTIF_SETTINGS,
  type NotifEvents,
  type NotifSettingsDto,
  type NotifyStateEvent,
  type PairedRecipient,
} from "../../types/notifications";
import { listen } from "@tauri-apps/api/event";

const STATE_LABELS: Record<AgentStateKey, string> = {
  working: "Working",
  waitingApprove: "Waiting Approve",
  waitingSpecific: "Waiting Specific",
  ended: "Ended",
  idle: "Idle",
  error: "Error",
  disconnected: "Disconnected",
};

const SOUND_OPTIONS: { value: StateSound; label: string }[] = [
  { value: null, label: "None" },
  { value: "chime", label: "Chime" },
  { value: "pop", label: "Pop" },
  { value: "custom", label: "File…" },
];

// Story 6.35 — libellés AGNOSTIQUES de l'outil (dé-généralisation Claude) + 2 catégories neuves.
const TOOL_PERMISSION_ROWS: { key: ToolPermissionKey; label: string }[] = [
  { key: "Bash",      label: "Shell / Terminal" },
  { key: "EditWrite", label: "Edit / Write" },
  { key: "Read",      label: "Read" },
  { key: "WebFetch",  label: "Web fetch / search" },
  { key: "ReadOnly",  label: "Search / List (read-only)" },
  { key: "Mcp",       label: "MCP tools" },
  { key: "Subagent",  label: "Sub-agents" },
  { key: "Other",     label: "Other" },
];

const TOOL_ACTION_OPTIONS: { value: ToolPermissionAction; label: string }[] = [
  { value: "manual",  label: "Manual" },
  { value: "approve", label: "Approve" },
  { value: "deny",    label: "Deny" },
];

const sectionStyle: React.CSSProperties = {
  background: "var(--bg-surface)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 8,
  padding: "16px 20px",
  display: "flex",
  flexDirection: "column",
  gap: 12,
};

const sectionTitleStyle: React.CSSProperties = {
  margin: 0,
  fontSize: 13,
  fontWeight: 600,
  color: "var(--color-amber)",
  textTransform: "uppercase",
  letterSpacing: "0.05em",
};

const labelStyle: React.CSSProperties = {
  fontSize: 13,
  color: "var(--text-primary)",
  display: "flex",
  alignItems: "center",
  gap: 8,
  userSelect: "none",
};

const selectStyle: React.CSSProperties = {
  background: "var(--bg-overlay)",
  color: "var(--text-primary)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 4,
  padding: "3px 6px",
  fontSize: 13,
};

function RadioGroup<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
}) {
  return (
    <div style={{ display: "flex", gap: 16 }}>
      {options.map((opt) => (
        <label key={opt.value} style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="radio"
            checked={value === opt.value}
            onChange={() => onChange(opt.value)}
          />
          {opt.label}
        </label>
      ))}
    </div>
  );
}

const buttonStyle: React.CSSProperties = {
  alignSelf: "flex-start",
  padding: "4px 12px",
  background: "var(--bg-subtle)",
  color: "var(--text-secondary)",
  border: "1px solid var(--border-subtle)",
  borderRadius: 6,
  cursor: "pointer",
  fontSize: 12,
};

const stepNumStyle: React.CSSProperties = {
  fontSize: 12,
  color: "var(--text-muted)",
  marginRight: 6,
};

/**
 * Section #9 — Mobile notifications (Story 8.1).
 *
 * ⚠️ Cet état ne passe PAS par `loadSettings`/`saveSettings` : la clé `notifSettings` est
 * possédée exclusivement par le Rust (l'identifiant de destinataire est produit par l'appairage
 * côté backend, et `saveSettings` écrit une clé EN BLOC — il l'écraserait). Tout transite donc
 * par les commandes `notify_*`.
 *
 * Le composant est volontairement AGNOSTIQUE du canal : aucun nom de service, aucune URL. Les
 * liens de l'assistant sont construits côté Rust et ouverts via `notify_open_link`.
 */
function MobileNotificationsSection() {
  const [notif, setNotif] = useState<NotifSettingsDto>(DEFAULT_NOTIF_SETTINGS);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // `warn` : succès technique, mais qui ne veut PAS dire que tout est opérationnel (test envoyé
  // alors que l'interrupteur maître est coupé). L'afficher en vert serait mensonger.
  const [info, setInfo] = useState<{ text: string; warn?: boolean } | null>(null);
  // Destinataire capté mais PAS encore mémorisé : il attend la confirmation de l'utilisateur.
  const [pending, setPending] = useState<PairedRecipient | null>(null);

  const reload = () =>
    invoke<NotifSettingsDto>("notify_get_settings")
      .then(setNotif)
      // Sans ce message, un échec de lecture laisserait un utilisateur APPAIRÉ face à
      // l'assistant vierge, sans rien pour comprendre ce qui se passe.
      .catch(() => setError("Could not read the notification settings."));

  useEffect(() => {
    reload();
    // Un envoi ASYNCHRONE qui échoue (réseau coupé, jeton révoqué) n'a personne à qui se
    // plaindre : le manager pousse l'erreur ici pour qu'elle devienne visible.
    const un = listen<NotifyStateEvent>("notify:state", (e) =>
      setNotif((n) => ({ ...n, lastError: e.payload.lastError })),
    );
    return () => { un.then((f) => f()).catch(() => {}); };
  }, []);

  // Enveloppe commune : un seul endroit qui gère « occupé », remise à zéro des messages et
  // traduction des codes d'erreur du Rust.
  async function run(
    name: string,
    action: () => Promise<unknown>,
    done?: { text: string; warn?: boolean },
  ) {
    setBusy(name);
    setError(null);
    setInfo(null);
    try {
      await action();
      await reload();
      if (done) setInfo(done);
    } catch (e) {
      // `invoke` rejette normalement avec la `String` d'erreur du Rust, mais un échec au niveau
      // IPC rejette un objet : `String(e)` rendrait alors « [object Object] ».
      setError(notifErrorLabel(typeof e === "string" ? e : ((e as Error)?.message ?? "unknown_error")));
    } finally {
      setBusy(null);
    }
  }

  /** Étape 4 : capter un destinataire, puis le SOUMETTRE à confirmation (jamais mémoriser seul). */
  const startPairing = () =>
    run("pair", async () => {
      setPending(await invoke<PairedRecipient>("notify_start_pairing"));
    });

  const save = (enabled: boolean, events: NotifEvents) =>
    run("save", () => invoke("notify_set_settings", { enabled, events }));

  const step = pairingStep(notif);
  const ready = step === "ready";
  const shown = error ?? (notif.lastError ? notifErrorLabel(notif.lastError) : null);

  return (
    <div style={sectionStyle}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <p style={sectionTitleStyle}>Mobile notifications</p>
        <span
          title={
            "Bring your own bot: you create a bot on your messaging app, and Vigil talks to it " +
            "directly from this machine — no Vigil server, no account, nothing to pay. Your bot " +
            "token is stored in the Windows credential manager. Note: the message summary " +
            "transits through a third-party service — Vigil never sends secrets, code, file " +
            "contents or tool output, only the same short label the Notch already shows."
          }
          style={{ cursor: "help", color: "var(--text-muted)", fontSize: 12 }}
        >
          ⓘ
        </span>
      </div>

      <label
        style={{ ...labelStyle, cursor: ready ? "pointer" : "not-allowed", opacity: ready ? 1 : 0.5 }}
        title={ready ? undefined : "Finish the setup below first."}
      >
        <input
          type="checkbox"
          checked={notif.enabled}
          disabled={!ready || busy !== null}
          onChange={(e) => save(e.target.checked, notif.events)}
        />
        {" "}Enable mobile notifications
      </label>

      {/* ── Assistant d'appairage (tant que tout n'est pas prêt) ── */}
      {!ready && (
        <div style={{ display: "flex", flexDirection: "column", gap: 10, marginLeft: 20 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={stepNumStyle}>1.</span>
            {/* `disabled` comme partout ailleurs : sans ça, un clic pendant l'attente d'appairage
                remettrait `busy` à null en quelques ms (le `finally` de `run`), réactivant le
                bouton « Link my phone » alors que la première écoute est TOUJOURS en vol — deux
                écoutes concurrentes = conflit côté service. */}
            <button
              style={buttonStyle}
              disabled={busy !== null}
              onClick={() => run("link", () => invoke("notify_open_link", { kind: "botfather" }))}
            >
              Open @BotFather
            </button>
            <span style={{ fontSize: 12, color: "var(--text-muted)" }}>
              Send /newbot, then copy the token it gives you.
            </span>
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={stepNumStyle}>2.</span>
            <input
              type="password"
              value={token}
              placeholder="Paste your bot token"
              onChange={(e) => setToken(e.target.value)}
              style={{ ...selectStyle, flex: 1, minWidth: 180 }}
            />
            <button
              style={buttonStyle}
              disabled={busy !== null}
              onClick={() =>
                run("token", () => invoke("notify_set_token", { token }).then(() => setToken("")))
              }
            >
              {busy === "token" ? "Checking…" : "Validate"}
            </button>
          </div>

          {step === "chat" && (
            <>
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span style={stepNumStyle}>3.</span>
                <span style={{ fontSize: 13, color: "var(--color-success)" }}>
                  Token OK{notif.botUsername ? ` — @${notif.botUsername}` : ""}
                </span>
                <button
                  style={buttonStyle}
                  disabled={busy !== null}
                  onClick={() => run("link", () => invoke("notify_open_link", { kind: "bot" }))}
                >
                  Open my bot
                </button>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span style={stepNumStyle}>4.</span>
                {/* Bouton désactivé pendant l'attente : deux écoutes concurrentes sur le même
                    bot se répondent par un conflit côté service. */}
                <button
                  style={buttonStyle}
                  disabled={busy !== null || pending !== null}
                  onClick={startPairing}
                >
                  {busy === "pair" ? "Waiting for your message…" : "Link my phone"}
                </button>
                <span style={{ fontSize: 12, color: "var(--text-muted)" }}>
                  Send /start to your bot, then click here (waits ~1 min).
                </span>
              </div>

              {/* ── Confirmation d'identité ──────────────────────────────────────────────
                  Capter un message ne prouve PAS qui l'a envoyé : n'importe qui connaissant
                  le nom du bot peut lui écrire, et la file d'updates couvre 24 h. On montre
                  donc le destinataire avant de le mémoriser — un intrus devient visible
                  AVANT le premier envoi, et « Not me » permet de reprendre. */}
              {pending && (
                <div
                  style={{
                    display: "flex", flexDirection: "column", gap: 8, marginLeft: 24,
                    padding: 8, borderRadius: 6, background: "var(--bg-subtle)",
                    border: "1px solid var(--border-subtle)",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                    <span style={{ fontSize: 13 }}>
                      Message received from <strong>{pending.displayName}</strong> — is that you?
                    </span>
                    <button
                      style={buttonStyle}
                      disabled={busy !== null}
                      onClick={() =>
                        run(
                          "confirm",
                          () =>
                            invoke("notify_confirm_pairing", { chatId: pending.chatId })
                              .then(() => setPending(null)),
                          { text: "Paired — notifications are now on." },
                        )
                      }
                    >
                      Yes, that's me
                    </button>
                    <button
                      style={buttonStyle}
                      disabled={busy !== null}
                      onClick={() => { setPending(null); setError(null); }}
                    >
                      No — retry
                    </button>
                  </div>
                  {/* Story 8.3 review D2 — un chat_id négatif = groupe/supergroupe Telegram.
                      Tout membre peut alors taper Approve sur un Bash. Geste délibéré assumé,
                      mais plus silencieux : on avertit avant confirmation. */}
                  {isGroupRecipient(pending.chatId) && (
                    <span style={{ fontSize: 12, color: "var(--color-warning, #c90)", lineHeight: 1.4 }}>
                      This recipient is a group — every member will be able to approve your
                      agents&apos; requests.
                    </span>
                  )}
                </div>
              )}
            </>
          )}
        </div>
      )}

      {/* ── Statut appairé ── */}
      {ready && (
        <div style={{ display: "flex", alignItems: "center", gap: 8, marginLeft: 20 }}>
          <span style={{ fontSize: 13, color: "var(--color-success)" }}>
            Connected ✓{notif.botUsername ? ` @${notif.botUsername}` : ""}
          </span>
          <button
            style={buttonStyle}
            disabled={busy !== null}
            // ⚠️ Le test contourne volontairement le filtrage (il doit marcher avant même que
            // l'interrupteur soit armé). Le dire, sinon un test réussi pendant que tout est
            // coupé fabrique une fausse confiance — vécu le 2026-07-19 : test OK, zéro
            // notification réelle, et l'interrupteur était simplement resté sur off.
            onClick={() =>
              run(
                "test",
                () => invoke("notify_send_test"),
                notif.enabled
                  ? { text: "Test notification sent." }
                  : {
                      text: "Test sent — but notifications are OFF, so real events won't be delivered.",
                      warn: true,
                    },
              )
            }
          >
            {busy === "test" ? "Sending…" : "Send test notification"}
          </button>
          <button
            style={buttonStyle}
            disabled={busy !== null}
            onClick={() => run("disconnect", () => invoke("notify_disconnect"))}
          >
            Disconnect
          </button>
        </div>
      )}

      {/* ── Événements notifiés (grisés tant que l'appairage n'est pas terminé) ── */}
      <div style={{ display: "flex", flexDirection: "column", gap: 6, marginLeft: 20, opacity: ready ? 1 : 0.5 }}>
        <span style={{ ...labelStyle, color: "var(--text-secondary)" }}>Notify me when an agent:</span>
        {([
          ["waiting", "is waiting for me"],
          ["ended", "has finished"],
          ["error", "hits an error"],
        ] as const).map(([key, label]) => (
          <label key={key} style={{ ...labelStyle, cursor: ready ? "pointer" : "not-allowed" }}>
            <input
              type="checkbox"
              checked={notif.events[key]}
              disabled={!ready || busy !== null}
              onChange={(e) => save(notif.enabled, { ...notif.events, [key]: e.target.checked })}
            />
            {" "}{label}
          </label>
        ))}
      </div>

      {shown && <p style={{ margin: 0, fontSize: 12, color: "var(--color-danger)" }}>{shown}</p>}
      {info && (
        <p
          style={{
            margin: 0,
            fontSize: 12,
            color: info.warn ? "var(--color-warn)" : "var(--color-success)",
          }}
        >
          {info.text}
        </p>
      )}
    </div>
  );
}

const HIGHLIGHT_VISIBLE_STATES: AgentStateKey[] = [
  "waitingApprove", "waitingSpecific", "error", "ended", "disconnected",
];

function SettingsPanel() {
  const [settings, setSettings] = useState<VigilSettings>(DEFAULT_SETTINGS);
  const [loaded, setLoaded] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [centerDone, setCenterDone] = useState(false);
  const activatedTools = useActivatedTools() ?? [];

  useEffect(() => {
    loadSettings()
      .then((s) => setSettings(s))
      .catch(() => {/* store absent ou Tauri non dispo — on garde les defaults */})
      .finally(() => setLoaded(true));
  }, []);

  function update(partial: Partial<VigilSettings>) {
    const next = { ...settings, ...partial };
    setSettings(next);
    saveSettings(partial).catch(() => {
      setSaveError("Error saving settings.");
    });
  }

  if (!loaded) {
    return (
      <div style={{ padding: 24, color: "var(--text-secondary)" }}>Loading…</div>
    );
  }

  return (
    <div
      style={{
        padding: 24,
        overflowY: "auto",
        height: "100%",
        boxSizing: "border-box",
        display: "flex",
        flexDirection: "column",
        gap: 16,
        color: "var(--text-primary)",
      }}
    >
      <h2 style={{ margin: 0, fontSize: 18 }}>Vigil Settings</h2>

      {saveError && (
        <p style={{ margin: 0, fontSize: 13, color: "var(--color-danger)" }}>{saveError}</p>
      )}

      {/* Section #1 — Notch Display */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Notch Display</p>
        <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
            <button
              onClick={() => {
                invoke("center_notch").catch(console.error);
                setCenterDone(true);
                setTimeout(() => setCenterDone(false), 2000);
              }}
              style={{
                alignSelf: "flex-start",
                padding: "4px 12px",
                background: "var(--bg-subtle)",
                color: centerDone ? "var(--color-success)" : "var(--text-secondary)",
                border: "1px solid var(--border-subtle)",
                borderRadius: 6,
                cursor: "pointer",
                fontSize: 12,
                transition: "color 200ms",
              }}
            >
              {centerDone ? "✓ Centered" : "Reset notch position"}
            </button>
          </div>
          <div>
            <span style={{ ...labelStyle, marginBottom: 6, display: "block" }}>Mode:</span>
            <RadioGroup
              value={settings.notchDisplayMode}
              options={[
                { value: "multi-agent", label: "Multi-agent" },
                { value: "priority-agent", label: "Priority Agent" },
              ]}
              onChange={(v) => update({ notchDisplayMode: v })}
            />
          </div>
          {settings.notchDisplayMode === "multi-agent" && (
            <label style={labelStyle}>
              Max visible agents:
              <select
                style={selectStyle}
                value={settings.maxVisibleAgents}
                onChange={(e) => update({ maxVisibleAgents: Number(e.target.value) as 4 | 5 | 6 | 7 })}
              >
                {[4, 5, 6, 7].map((n) => (
                  <option key={n} value={n}>{n}</option>
                ))}
              </select>
            </label>
          )}
          <label style={{ ...labelStyle, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={settings.detailedSingleAgent}
              onChange={(e) => update({ detailedSingleAgent: e.target.checked })}
            />
            Detailed view for single agent
          </label>
          <label style={{ ...labelStyle, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={settings.showSubagents}
              onChange={(e) => update({ showSubagents: e.target.checked })}
            />
            Show subagents
          </label>
          <label style={{ ...labelStyle, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={settings.hideNotchIfEmpty}
              onChange={(e) => {
                update({ hideNotchIfEmpty: e.target.checked });
                invoke("set_hide_notch_if_empty", { enabled: e.target.checked }).catch(console.error);
              }}
            />
            Hide Notch when no agents are active
          </label>
        </div>
      </div>

      {/* Section #2 — Display */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Display</p>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.showToolBadge}
            onChange={(e) => update({ showToolBadge: e.target.checked })}
          />
          Show tool badge
        </label>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.showModel}
            onChange={(e) => update({ showModel: e.target.checked })}
          />
          Show model in detail view
        </label>
        <div>
          <span style={{ ...labelStyle, marginBottom: 6, display: "block" }}>Detail view history:</span>
          <RadioGroup
            value={settings.detailHistoryMode}
            options={[
              { value: "detailed", label: "Detailed" },
              { value: "summary", label: "Summary" },
            ]}
            onChange={(v) => update({ detailHistoryMode: v })}
          />
        </div>
      </div>

      {/* Section #3 — State Management */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>State Management</p>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.autoCleanupDisconnected}
            onChange={(e) => update({ autoCleanupDisconnected: e.target.checked })}
          />
          Automatically remove disconnected agents
        </label>
        <div style={{ display: "grid", gridTemplateColumns: "1fr auto auto auto", alignItems: "center", gap: "8px 12px" }}>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>State</span>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>Color</span>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>Sound</span>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>Highlight</span>
          {(Object.keys(settings.stateSettings) as AgentStateKey[]).map((stateKey) => {
            const cfg = settings.stateSettings[stateKey];
            return (
              <React.Fragment key={stateKey}>
                <span style={{ fontSize: 13, color: "var(--text-primary)" }}>
                  {STATE_LABELS[stateKey]}
                </span>
                <input
                  type="color"
                  value={cfg.color ?? "#ffffff"}
                  style={{ width: 32, height: 24, padding: 0, border: "1px solid var(--border-subtle)", borderRadius: 4, cursor: "pointer", background: "none" }}
                  onChange={(e) =>
                    update({
                      stateSettings: {
                        ...settings.stateSettings,
                        [stateKey]: { ...cfg, color: e.target.value },
                      },
                    })
                  }
                />
                <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                  <select
                    style={selectStyle}
                    value={cfg.sound ?? ""}
                    onChange={(e) => {
                      const v = e.target.value;
                      const sound: StateSound = v === "" ? null : (v as StateSound);
                      update({
                        stateSettings: {
                          ...settings.stateSettings,
                          [stateKey]: { ...cfg, sound, soundPath: sound !== "custom" ? null : cfg.soundPath },
                        },
                      });
                    }}
                  >
                    {SOUND_OPTIONS.map((opt) => (
                      <option key={String(opt.value)} value={opt.value ?? ""}>{opt.label}</option>
                    ))}
                  </select>
                  {cfg.sound === "custom" && (
                    <div style={{ display: "flex", gap: 4, alignItems: "center" }}>
                      <span style={{ fontSize: 11, color: "var(--text-muted)", maxWidth: 90, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                        {cfg.soundPath ?? "No file"}
                      </span>
                      <button
                        onClick={async () => {
                          const selected = await openDialog({
                            filters: [{ name: "Audio", extensions: ["mp3", "wav", "ogg"] }],
                          });
                          if (typeof selected === "string") {
                            update({
                              stateSettings: {
                                ...settings.stateSettings,
                                [stateKey]: { ...cfg, soundPath: selected },
                              },
                            });
                          }
                        }}
                        style={{ fontSize: 11, padding: "1px 6px", background: "var(--bg-subtle)", color: "var(--text-secondary)", border: "1px solid var(--border-subtle)", borderRadius: 4, cursor: "pointer", flexShrink: 0 }}
                      >
                        Browse…
                      </button>
                    </div>
                  )}
                </div>
                {HIGHLIGHT_VISIBLE_STATES.includes(stateKey) ? (
                  <input
                    type="checkbox"
                    checked={cfg.autoHighlight}
                    onChange={(e) =>
                      update({
                        stateSettings: {
                          ...settings.stateSettings,
                          [stateKey]: { ...cfg, autoHighlight: e.target.checked },
                        },
                      })
                    }
                  />
                ) : (
                  <span />
                )}
              </React.Fragment>
            );
          })}
        </div>
      </div>

      {/* Section #4 — Navigation */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Navigation</p>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.quickTerminalAccess}
            onChange={(e) => update({ quickTerminalAccess: e.target.checked })}
          />
          Quick terminal access (click on notch cell)
        </label>
      </div>

      {/* Section #5 — System */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>System</p>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.startWithWindows}
            onChange={(e) => {
              update({ startWithWindows: e.target.checked });
              invoke("set_autostart", { enabled: e.target.checked }).catch(console.error);
            }}
          />
          Start with Windows
        </label>
        <label style={{ ...labelStyle, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={settings.autoRepairHooks}
            onChange={(e) => update({ autoRepairHooks: e.target.checked })}
          />
          Auto-repair hooks on launch
        </label>
      </div>

      {/* Section #6 — Permissions */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Permissions</p>
        <p style={{ margin: 0, fontSize: 12, color: "var(--text-secondary)" }}>
          Auto-response for tool permission requests. 1.5s delay before firing.
        </p>
        <div style={{ display: "grid", gridTemplateColumns: "1fr auto", gap: "10px 16px", alignItems: "center" }}>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>Tool</span>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600 }}>Response</span>
          {TOOL_PERMISSION_ROWS.map(({ key, label }) => (
            <React.Fragment key={key}>
              <span style={{ fontSize: 13, color: "var(--text-primary)" }}>{label}</span>
              <RadioGroup<ToolPermissionAction>
                value={settings.toolPermissions[key]}
                options={TOOL_ACTION_OPTIONS}
                onChange={(v) => update({ toolPermissions: { ...settings.toolPermissions, [key]: v } })}
              />
            </React.Fragment>
          ))}
        </div>
      </div>

      {/* Section #7 — Terminal */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Terminal</p>
        <label style={labelStyle}>
          Preferred terminal:
          <select
            style={selectStyle}
            value={settings.preferredTerminal}
            onChange={(e) => update({ preferredTerminal: e.target.value })}
          >
            <option value="wt.exe">Windows Terminal (wt.exe)</option>
            <option value="powershell.exe">PowerShell (powershell.exe)</option>
            <option value="cmd.exe">Command Prompt (cmd.exe)</option>
          </select>
        </label>
      </div>

      {/* Section #8 — Usage */}
      <div style={sectionStyle}>
        <p style={sectionTitleStyle}>Usage</p>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <label style={{ ...labelStyle, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={settings.usageTrackingEnabled}
              onChange={(e) => {
                const enabled = e.target.checked;
                const partial: Partial<VigilSettings> = enabled
                  ? { usageTrackingEnabled: true }
                  : { usageTrackingEnabled: false, showUsageInNotch: "none" };
                update(partial);
                invoke("set_usage_tracking_enabled", { enabled }).catch(console.error);
              }}
            />
            {" "}Enable usage tracking
          </label>
          <span
            title="Calls api.anthropic.com every 5 min using the OAuth token in ~/.claude/.credentials.json to retrieve your quota usage. Pro/Max only."
            style={{ cursor: "help", color: "var(--text-muted)", fontSize: 12 }}
          >
            ⓘ
          </span>
        </div>
        <label
          style={{
            ...labelStyle,
            marginLeft: 20,
            opacity: settings.usageTrackingEnabled ? 1 : 0.5,
          }}
        >
          Usage bar position:
          <select
            style={selectStyle}
            value={settings.showUsageInNotch}
            disabled={!settings.usageTrackingEnabled}
            onChange={(e) => update({ showUsageInNotch: e.target.value as UsageBarDisplay })}
          >
            <option value="none">None</option>
            <option value="bottom">Bottom</option>
            <option value="top">Top</option>
          </select>
        </label>
        {/* Sélecteur d'outil de la barre — visible seulement si la barre est affichée
            (position ≠ None) ET s'il y a plus d'un outil à départager. Pas d'option « None » :
            l'affichage est déjà géré par « Usage bar position » (Story 6.13, AC3). */}
        {settings.usageTrackingEnabled &&
          settings.showUsageInNotch !== "none" &&
          activatedTools.length > 1 && (
          <label style={{ ...labelStyle, marginLeft: 20 }}>
            Notch usage bar tool:
            <select
              style={selectStyle}
              value={resolveNotchUsageTool(
                settings.notchUsageTool,
                activatedTools.filter((t) => USAGE_SUPPORTED.includes(t)),
              ) ?? ""}
              onChange={(e) => update({ notchUsageTool: e.target.value as NotchUsageTool })}
            >
              {activatedTools.map((t) => {
                const supported = USAGE_SUPPORTED.includes(t);
                return (
                  <option key={t} value={t} disabled={!supported}>
                    {toolLabel(t)}{supported ? "" : " (coming soon)"}
                  </option>
                );
              })}
            </select>
          </label>
        )}
        {/* Amorçage de fenêtre (Story 6.25) — opt-in, OFF par défaut. Vigil envoie un message
            minimal à Claude pour démarrer la fenêtre 5h quand elle est fermée (consomme quelques
            tokens). Visible seulement si le tracking est ON ET Claude est activé : sans poll usage
            Claude (`enabled.claude_code`), l'amorçage ne pourrait pas tourner → toggle mort. */}
        {settings.usageTrackingEnabled &&
          activatedTools.includes("claude_code") && (
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginLeft: 20 }}>
            <label style={{ ...labelStyle, cursor: "pointer" }}>
              <input
                type="checkbox"
                checked={settings.usageWindowPrimingEnabled}
                onChange={(e) =>
                  update({ usageWindowPrimingEnabled: e.target.checked })
                }
              />
              {" "}Start my usage window earlier (for Claude Code)
            </label>
            <span
              title="When your 5-hour usage window is closed, Vigil sends a minimal message to Claude to start it, so your resets come earlier. Uses a few tokens each time."
              style={{ cursor: "help", color: "var(--text-muted)", fontSize: 12 }}
            >
              ⓘ
            </span>
          </div>
        )}
      </div>

      {/* Section #9 — Mobile notifications */}
      <MobileNotificationsSection />
    </div>
  );
}

export function SettingsApp() {
  return <SettingsPanel />;
}
