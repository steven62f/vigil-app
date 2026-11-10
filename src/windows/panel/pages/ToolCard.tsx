import React, { useState } from "react";
import { MascotPixel } from "../../../components/shared/MascotPixel";
import {
  MASCOT_VARIANT_OPTIONS,
  type MascotVariantId,
} from "../../../components/shared/mascotVariants";

const TOOL_LABELS: Record<string, string> = {
  claude_code: "Claude Code",
  codex: "Codex CLI",
  grok: "Grok Build",
  antigravity: "Antigravity CLI",
  kimi: "Kimi CLI",
};

// Une capacité affichée dans la carte : un label, plus une note optionnelle qui explique
// une limite propre à l'outil (ex. Grok, dont le support diffère de Claude). Voir plus bas.
interface Feature {
  label: string;
  note?: string;
}

// Capacités Vigil réellement livrées par outil (page Installation). Ordre canonique commun :
// chaque liste est un sous-ensemble ORDONNÉ de la référence ci-dessous. Source de vérité de chaque
// capacité — ET la raison de son ABSENCE chez certains outils (pour ne pas re-chercher partout).
// ⚠️ Distinguer MORT côté outil (l'outil ne l'expose pas → impossible, aucune story) vs DÉFÉRÉ
// (faisable, mais pas encore priorisé/implémenté → une story pourra le livrer).
//
//   - Monitoring        : tous (SessionStart/PreToolUse/PostToolUse/Stop enregistrés).
//
//   - Approve / Deny    : event `PermissionRequest` (Claude, Codex, Grok/6.30).
//                         • Kimi : **LIVRÉ en 9.3**, **conditionné au MODE** (jumeau de Grok, pas
//                           clone de Claude — spike e2e 2026-07-22) :
//                           – en `yolo` : Approve/Deny COMPLET depuis Vigil — `vigil-kimi` synthétise
//                             un `PermissionRequest(read_only:false)` depuis le `PreToolUse` et BLOQUE
//                             en lisant la décision de Vigil (patron `vigil-grok::handle_approve`) ;
//                           – en `manual` / `auto` : awareness READ-ONLY seule, et c'est **MORT
//                             côté outil** — les hooks Kimi sont restrictifs PAR DESIGN,
//                             `permissionDecision:"allow"` est **IGNORÉ** (mesuré deux fois au
//                             spike, avec et sans saut de ligne final). Aucune story ne le
//                             débloquera : seul le `deny` porte.
//                           Câblé (9.3) → annoncé (`note` conditionnée au mode ci-dessous) : pas
//                           d'affordance morte, la capacité existe réellement (leçon 6.33 respectée).
//                         • Antigravity : NON RETENU = MORT côté outil pour la FONCTION (spike e2e
//                           2026-07-17, story 6.22 fermée). Le hook PreToolUse peut *véto* (un `deny`
//                           bloque bien l'outil), MAIS trois murs interdisent un approve/deny fidèle
//                           depuis Vigil : (1) l'`allow` ne SUPPRIME PAS le prompt natif d'Antigravity
//                           (validé e2e : allow → la TUI redemande quand même → « approuver depuis
//                           Vigil » ne fait pas gagner de contexte, double décision + course) ; (2) le
//                           payload PreToolUse ne porte AUCUN signal « ce tool déclenche une demande »
//                           (pas d'équivalent au `permissionMode` de Grok ; `list_dir` et `run_command`
//                           ont le même schéma) → impossible de n'afficher QUE les vraies demandes du
//                           TUI ; une heuristique par catégorie ferait des faux positifs/négatifs
//                           (refusé Steven : pas de demande fantôme dans Vigil, ni d'oubli) ; (3) pas de
//                           mode auto-approve global (≠ CTRL+O Grok) → aucune valeur « travailler sans
//                           valider chaque outil ». → binaire reste observe-only SILENCIEUX (répond
//                           `{"decision":"allow"}`, n'émet aucun PermissionRequest). Aucune story.
//
//   - Ask User Question : question interactive (Claude ; Grok/6.30 en lecture seule — hook veto only).
//                         • Codex : ABSENT = MORT côté outil — Codex n'a pas de mécanisme de question
//                           interactive.
//                         • Antigravity : ABSENT = MORT côté outil — aucun hook de question (surface
//                           = 5 hooks outil/invocation uniquement, spike 6.16).
//                         • Kimi : ABSENT en 9.2 = même condition de mode que Approve/Deny ci-dessus
//                           (→ 9.3). Non annoncé tant que rien n'est câblé.
//
//   - Interrupt agent   : coupe un agent en cours depuis la Vue Détail (flag `%TEMP%` consommé au
//                         prochain PreToolUse/PostToolUse) → Claude seul (4.5), gaté par
//                         `INTERRUPT_SUPPORTED` (statsTools.ts) + miroir Rust `flags::interrupt_supported`
//                         (6.33 — avant cette story le bouton s'affichait pour les 4 outils = affordance
//                         MORTE : flag orphelin + `interrupt_pending` bloqué à vie).
//                         • Grok : ABSENT = MORT via hook (6.33, prouvé e2e) — un `{"decision":"deny"}`
//                           est un veto PAR OUTIL : l'appel est bloqué mais l'agent reroute et finit son
//                           tour ; aucun équivalent au `{"continue":false}` de Claude. Seule voie de
//                           parité = ACP (`grok agent stdio`, Vigil client → cancel natif) = story future.
//                         • Codex : ABSENT = MORT via hook (spike e2e 2026-07-17, chantier #2). Les hooks
//                           Codex sont de lignée Claude Code MAIS le kill MID-TOUR n'existe pas : sur
//                           `PreToolUse`, `{"continue":false}` est « unsupported » (erreur explicite +
//                           l'outil part quand même) ; sur `PostToolUse` il n'arrête pas (remplace le
//                           résultat + « continue from there » = soft, l'agent enchaîne). Les seuls
//                           hard-stops (`Stop`/`UserPromptSubmit`) sont des frontières de tour → inutiles
//                           pour couper un tour EN COURS. Doc officielle Codex + e2e concordants.
//                         • Antigravity : ABSENT = MORT via hook (spike permissions 2026-07-17) — stdout
//                           lu seulement sur `PreToolUse`, réponse `{"decision":"deny"}` = veto PAR OUTIL,
//                           l'agent reroute ; aucun `continue:false`, `Stop` ne lit pas le stdout.
//                         • Kimi : ABSENT = MORT via hook — même contrat deny-only que Grok/Codex/
//                           Antigravity. Déjà exclu de `INTERRUPT_SUPPORTED` ET de son miroir Rust
//                           `flags::interrupt_supported(Kimi)` depuis la 9.1 : rien à changer.
//                         ⇒ L'interruption est ARCHITECTURALEMENT Claude-only (seul à honorer un
//                           `{"continue":false}` MID-TOUR). L'allowlist `INTERRUPT_SUPPORTED=["claude_code"]`
//                           (6.33) couvre les 3 correctement. Aucune story ; seule voie de parité = ACP.
//
//   - Sub-agents        : events `SubagentStart`/`SubagentStop` (Claude ; Codex/6.14 ; Grok/6.31).
//                         • Antigravity : ABSENT = MORT côté outil — aucun hook de cycle de vie
//                           sous-agent, donc rien pour rattacher un sous-agent à son parent (spike
//                           6.16 : capacité tranchée « non supportée », aucune story).
//                         • Kimi : PRÉSENT (9.2) — `SubagentStart`/`SubagentStop` installés, mécanisme
//                           générique réutilisé INCHANGÉ. Aucune garde anti-fantôme façon Codex n'est
//                           ajoutée : les events Kimi portent le `session_id` du PARENT, le bug
//                           qu'elle corrige ne peut pas se produire.
//
//   - API errors        : event `StopFailure` (Claude ; Grok/6.34).
//                         • Codex / Antigravity : ABSENT = MORT côté outil — ces CLI n'émettent pas de
//                           hook d'échec de tour distinct (StopFailure ∉ CODEX_EVENTS/ANTIGRAVITY_EVENTS).
//                         • Kimi : ABSENT = **DÉFÉRÉ**, pas mort. `StopFailure` EST dans
//                           `HOOK_EVENT_TYPES`, EST installé par la 9.2 et EST traduit par
//                           `vigil-kimi` — mais il n'a **jamais été observé** au spike. On ne
//                           l'annonce pas avant de l'avoir vu tirer en vrai.
//
//   - History           : `HISTORY_SUPPORTED` (statsTools.ts) → Claude, Codex, Antigravity, Grok (6.28),
//                         Kimi (livré en 9.4 : index `session_index.jsonl` + `state.json`).
//
//   - Statistics        : `LOCAL_STATS_SUPPORTED` → Claude, Codex, Grok (6.32), Kimi (livré en 9.4 :
//                         tokens `usage.record` des `wire.jsonl` ; coût = grille Moonshot ~$, modèles
//                         auto-hébergés à 0).
//                         • Antigravity : ABSENT = MORT côté outil — aucun token/coût stocké en local,
//                           rien à parser sur disque (spike 6.16, aucune story).
//
//   - Usage             : `USAGE_SUPPORTED` → Claude, Codex, Antigravity, Grok (6.32).
//                         • Kimi : ABSENT = DÉFÉRÉ à la Story **9.5**, elle-même spike-first (la
//                           source d'usage de Moonshot est à ce jour INCONNUE).
//
//   - Usage window priming : amorçage précoce de la fenêtre d'usage (`usageWindowPrimingEnabled`,
//                         opt-in, Settings) → Claude seul (6.25).
//                         • Codex : ABSENT = DÉFÉRÉ, VALEUR NULLE au palier actuel (observé 2026-07-17 :
//                           fenêtre d'usage Codex = PLUSIEURS JOURS, pas 5h → amorcer ne rapproche aucun
//                           reset utile). L'amorçage n'a de sens que sur une fenêtre glissante COURTE
//                           (type 5h Claude). À réévaluer SI un abo payant expose une fenêtre horaire.
//                         • Antigravity : ABSENT = exclu VOLONTAIREMENT (fenêtre de crédits HEBDO + on
//                           tient à le garder sans prompt — l'amorçage consommerait du quota).
//                         • Grok : ABSENT = non applicable (mécanisme propre à la fenêtre 5h Claude).
//                         • Kimi : ABSENT = non applicable (aucune fenêtre d'usage connue, cf. 9.5).
//
// Notifications : retiré de la liste — ce n'est pas une fonction Vigil à part entière.
const TOOL_FEATURES: Record<string, Feature[]> = {
  claude_code: [
    { label: "Monitoring" },
    { label: "Approve / Deny" },
    { label: "Ask User Question" },
    { label: "Interrupt agent" },
    { label: "Sub-agents" },
    { label: "API errors" },
    { label: "History" },
    { label: "Statistics" },
    { label: "Usage" },
    {
      label: "Usage window priming",
      note: "Opt-in — when your usage window is closed, Vigil starts it early so your resets come sooner (uses a few tokens each time). Enable it in Settings.",
    },
  ],
  codex: [
    { label: "Monitoring" },
    { label: "Approve / Deny" },
    { label: "Sub-agents" },
    { label: "History" },
    { label: "Statistics" },
    { label: "Usage" },
  ],
  grok: [
    { label: "Monitoring" },
    {
      label: "Approve / Deny",
      note: "Full Approve/Deny from Vigil only in always-approve mode. In default mode the request is shown read-only — confirm in the terminal.",
    },
    {
      label: "Ask User Question",
      note: "Shown read-only in Vigil — answer in the terminal (unlike Claude, Vigil can't submit the answer for you).",
    },
    { label: "Sub-agents" },
    { label: "API errors" },
    { label: "History" },
    { label: "Statistics" },
    { label: "Usage" },
  ],
  antigravity: [
    { label: "Monitoring" },
    { label: "History" },
    { label: "Usage" },
  ],
  // ⚠️ Story 9.3 — Approve/Deny conditionné au mode (jumeau de Grok). Story 9.4 — History +
  // Statistics livrés (après Sub-agents, ordre canonique Claude/Grok). Usage reste 9.5 (déférée).
  // Voir le commentaire ci-dessus pour le détail MORT côté outil (allow ignoré hors yolo) vs DÉFÉRÉ.
  kimi: [
    { label: "Monitoring" },
    {
      label: "Approve / Deny",
      note: "Full Approve/Deny from Vigil only in yolo mode. In manual or auto mode the request is shown read-only — confirm in the terminal.",
    },
    { label: "Sub-agents" },
    { label: "History" },
    { label: "Statistics" },
  ],
};

interface ToolCardProps {
  toolId: "claude_code" | "codex" | "grok" | "antigravity" | "kimi";
  detected: boolean;
  enabled: boolean;
  hooksOk: boolean;
  disabled?: boolean;
  onToggle: (newVal: boolean) => void;
  onRepair: () => void;
  mascotVariant: MascotVariantId;
  onMascotChange: (v: MascotVariantId) => void;
}

export function ToolCard({ toolId, detected, enabled, hooksOk, disabled, onToggle, onRepair, mascotVariant, onMascotChange }: ToolCardProps) {
  const [hovered, setHovered] = useState(false);
  const label = TOOL_LABELS[toolId];
  const features = TOOL_FEATURES[toolId];
  const showAlert = enabled && !hooksOk;

  return (
    <div
      style={{
        background: "var(--bg-surface)",
        border: `1px solid ${showAlert ? "var(--color-amber)" : "var(--border-subtle)"}`,
        borderRadius: 8,
        padding: "14px 16px",
        display: "flex",
        flexDirection: "column",
        gap: 10,
        opacity: detected ? 1 : 0.55,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
        <span style={{ fontSize: 14, fontWeight: 600, color: "var(--text-primary)", flex: 1 }}>
          {label}
        </span>

        {showAlert && (
          <span
            title="Hooks are compromised — repair to restore monitoring"
            style={{
              fontSize: 13,
              color: "var(--color-amber)",
              display: "inline-flex",
              alignItems: "center",
              gap: 4,
            }}
          >
            ⚠ Hooks compromised
          </span>
        )}

        {showAlert && (
          <button
            onClick={() => { if (!disabled) onRepair(); }}
            disabled={disabled}
            style={{
              fontSize: 12,
              fontWeight: 500,
              padding: "3px 12px",
              borderRadius: 6,
              background: "var(--bg-subtle)",
              color: "var(--color-amber)",
              border: "1px solid var(--color-amber)",
              cursor: disabled ? "not-allowed" : "pointer",
              opacity: disabled ? 0.5 : 1,
            }}
          >
            Repair
          </button>
        )}

        {detected && (
          <label
            style={{ display: "inline-flex", alignItems: "center", gap: 6, cursor: disabled ? "not-allowed" : "pointer" }}
            onMouseEnter={() => setHovered(true)}
            onMouseLeave={() => setHovered(false)}
          >
            <div
              style={{
                position: "relative",
                width: 36,
                height: 20,
                borderRadius: 10,
                background: enabled ? "var(--color-success)" : "var(--bg-subtle)",
                border: `1px solid ${enabled ? "var(--color-success)" : "var(--border-subtle)"}`,
                transition: "background 150ms, border-color 150ms",
                cursor: disabled ? "not-allowed" : "pointer",
                outline: hovered ? "2px solid var(--color-amber)" : "none",
                outlineOffset: 2,
                opacity: disabled ? 0.5 : 1,
              }}
              onClick={() => { if (!disabled) onToggle(!enabled); }}
            >
              <div
                style={{
                  position: "absolute",
                  top: 2,
                  left: enabled ? 17 : 2,
                  width: 14,
                  height: 14,
                  borderRadius: "50%",
                  background: "var(--bg-overlay)",
                  transition: "left 150ms",
                }}
              />
            </div>
            <span style={{ fontSize: 12, color: "var(--text-secondary)" }}>
              {enabled ? "Enabled" : "Disabled"}
            </span>
          </label>
        )}
      </div>

      {enabled && detected && (
        <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600, textTransform: "uppercase", letterSpacing: "0.05em" }}>
            Available features
          </span>
          <ul style={{ margin: 0, padding: "0 0 0 16px", display: "flex", flexDirection: "column", gap: 3 }}>
            {features.map((f) => (
              <li key={f.label} style={{ fontSize: 12, color: "var(--text-secondary)" }}>
                {f.label}
                {f.note && (
                  <span style={{ display: "block", fontSize: 11, color: "var(--text-muted)", lineHeight: 1.4, marginTop: 1 }}>
                    {f.note}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}

      {enabled && detected && (
        <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <span style={{ fontSize: 11, color: "var(--text-muted)", fontWeight: 600, textTransform: "uppercase", letterSpacing: "0.05em" }}>
            Mascot
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            {MASCOT_VARIANT_OPTIONS.map((opt) => {
              const active = opt.value === mascotVariant;
              return (
                <button
                  key={opt.value}
                  type="button"
                  title={opt.label}
                  aria-pressed={active}
                  onClick={() => { if (!disabled) onMascotChange(opt.value); }}
                  disabled={disabled}
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    alignItems: "center",
                    gap: 3,
                    padding: "6px 8px",
                    borderRadius: 6,
                    background: active ? "var(--bg-subtle)" : "transparent",
                    border: `1px solid ${active ? "var(--color-success)" : "var(--border-subtle)"}`,
                    outline: active ? "1px solid var(--color-success)" : "none",
                    cursor: disabled ? "not-allowed" : "pointer",
                    opacity: disabled ? 0.5 : 1,
                  }}
                >
                  <MascotPixel state="working" size={28} variant={opt.value} />
                  <span style={{ fontSize: 10, color: active ? "var(--text-primary)" : "var(--text-secondary)" }}>
                    {opt.label}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      )}

      {toolId === "codex" && (
        <div
          style={{
            background: "var(--bg-subtle)",
            border: "1px solid var(--border-subtle)",
            borderRadius: 6,
            padding: "8px 10px",
            fontSize: 12,
            color: "var(--text-secondary)",
            lineHeight: 1.5,
          }}
        >
          ℹ Codex CLI hooks must be reviewed and approved by the user in the Codex interface before they take effect.
        </div>
      )}
    </div>
  );
}
