import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot } from "../../types/agent";
import {
  type AgentNotifOverrides,
  type NotifEvents,
  type NotifKind,
  type NotifSettingsDto,
  DEFAULT_NOTIF_EVENTS,
  NOTIF_KIND_ROWS,
  effectiveNotif,
  isNotifOverridden,
} from "../../types/notifications";

interface Props {
  agent: AgentSnapshot;
  /** Ouverture PILOTÉE par `DetailPanel` : un seul popover du footer ouvert à la fois. */
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Story 8.2 — surcharges de notification pour UN agent. Clone structurel d'`AgentPermissionMenu` :
 * même géométrie de popover, mêmes badges `agent`/`global`, mêmes actions groupées + Reset.
 *
 * Différences assumées avec le menu Permissions :
 *  1. **Interrupteur** et non `select` : il n'y a que 2 états par événement.
 *  2. Le réglage global est lu via `notify_get_settings` et **non** `loadSettings()` : la clé
 *     `notifSettings` est possédée par le Rust (décision 8.1), elle n'est pas dans le store front.
 *  3. Un bouton « ↺ » par ligne (revue 8.2) : le `select` des permissions a un 3ᵉ état qui sert de
 *     porte de sortie, une case à cocher n'en a que 2. Sans lui, recocher une ligne la FIGE en
 *     surcharge `agent` — et couper l'événement globalement plus tard épargnerait cet agent-là,
 *     sans que rien ne l'explique.
 *
 * Ce composant ne fait qu'**éditer** les règles ; la décision d'envoyer se prend côté Rust, donc
 * les notifications continuent de partir fenêtre fermée.
 */
export function AgentNotifMenu({ agent, open, onOpenChange }: Props) {
  const [overrides, setOverrides] = useState<AgentNotifOverrides>({});
  const [globalEvents, setGlobalEvents] = useState<NotifEvents>(DEFAULT_NOTIF_EVENTS);
  const [enabled, setEnabled] = useState(false);
  const [loading, setLoading] = useState(false);
  // Une écriture en vol. Sert à DÉSACTIVER les contrôles : sans ça, décocher une ligne puis
  // cliquer « Reset » aussitôt laisse les deux commandes courir en parallèle, et rien ne garantit
  // que le `delete` arrive après le `set` — le store finit en désaccord avec l'écran (revue 8.2).
  const [busy, setBusy] = useState(false);

  // État de l'interrupteur maître au MONTAGE : il décide de la présence même du bouton.
  // ⚠️ Limite acceptée (AC5) : basculer l'interrupteur dans les Settings pendant qu'une Vue
  // Détail est ouverte ne se voit qu'à la prochaine ouverture de celle-ci — pas de diffusion
  // d'événement pour ça, hors périmètre.
  useEffect(() => {
    let vivant = true;
    invoke<NotifSettingsDto>("notify_get_settings")
      .then((s) => {
        if (!vivant) return;
        setEnabled(s.enabled);
        setGlobalEvents(s.events);
      })
      .catch((err: unknown) => {
        // Le bouton reste absent (`enabled` = false), mais l'échec est TRACÉ : sans ce log, une
        // lecture de store en échec est rigoureusement indiscernable de « notifications coupées »,
        // pour l'utilisateur comme au diagnostic (revue 8.2).
        console.error("[vigil] notify_get_settings (montage AgentNotifMenu):", err);
      });
    return () => {
      vivant = false;
    };
  }, []);

  const openMenu = async () => {
    setLoading(true);
    try {
      // Règles de l'agent + réglages globaux EN PARALLÈLE (patron `AgentPermissionMenu`), pour
      // que les badges `agent`/`global` reflètent l'état réel au moment de l'ouverture.
      const [rules, settings] = await Promise.all([
        invoke<Record<string, boolean>>("get_agent_notif_rules", {
          transcriptPath: agent.transcriptPath,
        }),
        invoke<NotifSettingsDto>("notify_get_settings"),
      ]);
      setOverrides(rules as AgentNotifOverrides);
      setGlobalEvents(settings.events);
      setEnabled(settings.enabled);
      onOpenChange(true);
    } catch (err: unknown) {
      // Sans ce `catch`, un échec laissait le bouton muet : clic → rien, indéfiniment, sans trace.
      console.error("[vigil] get_agent_notif_rules:", err);
    } finally {
      setLoading(false);
    }
  };

  /**
   * Enveloppe commune des écritures : une seule en vol, échec tracé, état local mis à jour
   * UNIQUEMENT si la commande a réussi (l'écran ne prétend jamais avoir enregistré ce qui ne l'a
   * pas été). Écriture IMMÉDIATE, sans bouton « Enregistrer ».
   */
  const ecrire = async (
    commande: string,
    args: Record<string, unknown>,
    apres: (prev: AgentNotifOverrides) => AgentNotifOverrides,
  ) => {
    if (busy) return;
    setBusy(true);
    try {
      await invoke(commande, { transcriptPath: agent.transcriptPath, ...args });
      setOverrides(apres);
    } catch (err: unknown) {
      console.error(`[vigil] ${commande}:`, err);
    } finally {
      setBusy(false);
    }
  };

  const updateRule = (kind: NotifKind, value: boolean) =>
    ecrire("set_agent_notif_rule", { kind, enabled: value }, (prev) => ({ ...prev, [kind]: value }));

  // Rend UNE ligne à l'héritage du global (badge `global`), sans toucher aux deux autres.
  const clearRule = (kind: NotifKind) =>
    ecrire("clear_agent_notif_rule", { kind }, (prev) => {
      const suite = { ...prev };
      delete suite[kind];
      return suite;
    });

  const setAll = (value: boolean) =>
    ecrire("set_all_agent_notif_rules", { enabled: value }, () =>
      Object.fromEntries(NOTIF_KIND_ROWS.map((r) => [r.kind, value])),
    );

  // SUPPRIME les règles de l'agent : les 3 badges repassent visiblement à `global`.
  const reset = () => ecrire("reset_agent_notif_rules", {}, () => ({}));

  // Notifications globalement coupées → bouton ABSENT du DOM (pas grisé) : il n'aurait aucun
  // effet, et l'interrupteur général reste le seul point de contrôle.
  //
  // Le conteneur `position:relative` (ancre du popover) est rendu ICI et non par `DetailPanel` :
  // sinon il subsistait vide dans le footer flex, consommant sa gouttière de 4 px alors que la
  // fonctionnalité est invisible (revue 8.2).
  if (!enabled) return null;

  if (!open) {
    return (
      <div style={{ position: "relative" }}>
        <button
          onClick={openMenu}
          onMouseDown={(e) => e.stopPropagation()}
          onMouseEnter={(e) => {
            if (!loading) {
              (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface)";
            (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
          }}
          disabled={loading}
          style={{
            background: "var(--bg-surface)",
            border: "1px solid var(--border-subtle)",
            borderRadius: 5,
            color: "var(--text-secondary)",
            fontSize: 11,
            fontWeight: 500,
            padding: "4px 10px",
            cursor: loading ? "default" : "pointer",
            opacity: loading ? 0.6 : 1,
            transition: "background 100ms, color 100ms",
          }}
        >
          Notifs
        </button>
      </div>
    );
  }

  return (
    <div style={{ position: "relative" }}>
      <div
        onMouseDown={(e) => e.stopPropagation()}
        style={{
          position: "absolute",
          bottom: 44,
          right: 8,
          width: 280,
          background: "var(--bg-surface)",
          border: "1px solid var(--border-subtle)",
          borderRadius: 8,
          padding: 12,
          zIndex: 100,
          boxShadow: "0 4px 16px rgba(0,0,0,0.4)",
          opacity: busy ? 0.7 : 1,
          transition: "opacity 100ms",
        }}
      >
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 }}>
          <span style={{ fontSize: 11, fontWeight: 600, color: "var(--text-primary)" }}>
            Notifications
          </span>
          <button
            onClick={() => onOpenChange(false)}
            onMouseEnter={(e) => {
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
            }}
            style={{ background: "none", border: "none", cursor: "pointer", color: "var(--text-muted)", fontSize: 14, padding: "0 2px", transition: "color 100ms" }}
          >
            ×
          </button>
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 10 }}>
          {NOTIF_KIND_ROWS.map(({ kind, label }) => {
            const effective = effectiveNotif(kind, overrides, globalEvents);
            const overridden = isNotifOverridden(kind, overrides);
            return (
              <div key={kind} style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
                <div style={{ display: "flex", alignItems: "center", gap: 5, flex: 1, minWidth: 0 }}>
                  <span style={{ fontSize: 11, color: "var(--text-secondary)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                    {label}
                  </span>
                  <span
                    style={{
                      fontSize: 9,
                      padding: "1px 4px",
                      borderRadius: 3,
                      background: overridden ? "var(--color-amber-bg)" : "var(--bg-overlay)",
                      color: overridden ? "var(--color-amber)" : "var(--text-muted)",
                      flexShrink: 0,
                    }}
                  >
                    {overridden ? "agent" : "global"}
                  </span>
                  {/* Retour à l'héritage pour CETTE ligne — n'apparaît que si elle est surchargée,
                      donc jamais de bouton sans effet. */}
                  {overridden && (
                    <button
                      onClick={() => clearRule(kind)}
                      disabled={busy}
                      title={`Revenir au réglage global pour « ${label} »`}
                      aria-label={`Revenir au réglage global pour ${label}`}
                      onMouseEnter={(e) => {
                        if (!busy) (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
                      }}
                      onMouseLeave={(e) => {
                        (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
                      }}
                      style={{
                        background: "none",
                        border: "none",
                        color: "var(--text-muted)",
                        fontSize: 10,
                        lineHeight: 1,
                        padding: "1px 2px",
                        cursor: busy ? "default" : "pointer",
                        flexShrink: 0,
                        transition: "color 100ms",
                      }}
                    >
                      ↺
                    </button>
                  )}
                </div>
                <input
                  type="checkbox"
                  checked={effective}
                  disabled={busy}
                  onChange={(e) => updateRule(kind, e.target.checked)}
                  aria-label={label}
                  style={{ cursor: busy ? "default" : "pointer", flexShrink: 0, accentColor: "var(--color-amber)" }}
                />
              </div>
            );
          })}
        </div>

        <div style={{ display: "flex", flexWrap: "wrap", gap: 4, borderTop: "1px solid var(--border-subtle)", paddingTop: 8 }}>
          {[true, false].map((value) => (
            <button
              key={String(value)}
              onClick={() => setAll(value)}
              disabled={busy}
              onMouseEnter={(e) => {
                if (!busy) {
                  (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-surface)";
                  (e.currentTarget as HTMLButtonElement).style.color = "var(--text-primary)";
                }
              }}
              onMouseLeave={(e) => {
                (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
                (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
              }}
              style={{
                background: "var(--bg-overlay)",
                border: "1px solid var(--border-subtle)",
                borderRadius: 4,
                color: "var(--text-secondary)",
                fontSize: 10,
                padding: "3px 7px",
                cursor: busy ? "default" : "pointer",
                transition: "background 100ms, color 100ms",
              }}
            >
              {value ? "All on" : "All off"}
            </button>
          ))}
          <button
            onClick={reset}
            disabled={busy}
            onMouseEnter={(e) => {
              if (!busy) {
                (e.currentTarget as HTMLButtonElement).style.background = "var(--bg-overlay)";
                (e.currentTarget as HTMLButtonElement).style.color = "var(--text-secondary)";
              }
            }}
            onMouseLeave={(e) => {
              (e.currentTarget as HTMLButtonElement).style.background = "none";
              (e.currentTarget as HTMLButtonElement).style.color = "var(--text-muted)";
            }}
            style={{
              background: "none",
              border: "1px solid var(--border-subtle)",
              borderRadius: 4,
              color: "var(--text-muted)",
              fontSize: 10,
              padding: "3px 7px",
              cursor: busy ? "default" : "pointer",
              transition: "background 100ms, color 100ms",
            }}
          >
            Reset
          </button>
        </div>
      </div>
    </div>
  );
}
