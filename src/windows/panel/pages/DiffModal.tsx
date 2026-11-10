import React from "react";
import { computeUnifiedDiff, type DiffLine } from "../../../lib/diffUtils";
import type { ToolHookDiff } from "../../../types/tools";

function UnifiedDiff({ before, after }: { before: string; after: string }) {
  const lines = computeUnifiedDiff(before, after);
  return (
    <div style={{ overflow: "auto", maxHeight: 300, borderRadius: 6,
                  border: "1px solid var(--border-subtle)", background: "var(--bg-surface)" }}>
      <pre style={{ margin: 0, padding: 10, fontSize: 12,
                    fontFamily: "'JetBrains Mono', monospace",
                    whiteSpace: "pre", width: "max-content", minWidth: "100%",
                    color: "var(--text-primary)", boxSizing: "border-box" }}>
        {lines.map((line: DiffLine, idx: number) => {
          const prefix = line.type === "added" ? "+" : line.type === "removed" ? "-" : " ";
          const bg = line.type === "added" ? "var(--diff-added-bg)"
                   : line.type === "removed" ? "var(--diff-removed-bg)"
                   : "transparent";
          const color = line.type === "added" ? "var(--color-success)"
                      : line.type === "removed" ? "var(--color-danger)"
                      : "inherit";
          return (
            <span key={idx} style={{ display: "block", width: "100%", background: bg, color }}>
              {prefix} {line.text}
            </span>
          );
        })}
      </pre>
    </div>
  );
}

interface DiffModalProps {
  diff: ToolHookDiff;
  onConfirm: () => void;
  onCancel: () => void;
  loading: boolean;
}

export function DiffModal({ diff, onConfirm, onCancel, loading }: DiffModalProps) {
  // Échap ferme la demande de consentement — un modal qui BLOQUE le défilement doit être
  // renvoyable au clavier. Ignoré pendant `loading`, comme les deux boutons : l'installation est
  // déjà partie, fermer l'UI ne l'annulerait pas et laisserait croire le contraire.
  React.useEffect(() => {
    if (loading) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [loading, onCancel]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      style={{
        // ⚠️ `fixed`, PAS `absolute`. En `absolute`, la boîte de référence est le conteneur
        // scrollable d'`InstallationPage` (`position: relative` + `overflowY: auto`) pris dans son
        // état NON scrollé : l'overlay se calait donc en haut du CONTENU et défilait avec lui.
        // Résultat vécu (retour Steven, 2026-07-23) : en activant un outil situé en bas de page —
        // Kimi est le 5ᵉ — le modal apparaissait hors écran, plusieurs étages plus haut. Le toggle
        // semblait mort, et même le fond noir ne couvrait pas la zone regardée : aucun indice.
        // `fixed` ancre l'overlay sur la FENÊTRE, donc toujours là où l'utilisateur regarde.
        // ⚠️ Ne jamais introduire de `transform` / `filter` / `will-change` / `contain` sur un
        // ancêtre : chacun recréerait un bloc conteneur et ramènerait le bug (vérifié : il n'y en
        // a aucun dans l'arbre du panneau).
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.45)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: 16,
        boxSizing: "border-box",
        zIndex: 100,
      }}
    >
      <div
        style={{
          background: "var(--bg-overlay)",
          border: "1px solid var(--border-subtle)",
          borderRadius: 10,
          padding: "20px 24px",
          display: "flex",
          flexDirection: "column",
          gap: 14,
          width: "min(480px, 100%)",
          maxHeight: "100%",
          overflow: "hidden",
        }}
      >
        <p style={{ margin: 0, fontSize: 13, color: "var(--text-secondary)" }}>
          Diff — <strong style={{ color: "var(--text-primary)" }}>{diff.configFilePath}</strong>
        </p>
        <UnifiedDiff before={diff.before} after={diff.after} />
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <button
            onClick={onConfirm}
            disabled={loading}
            style={{
              padding: "6px 14px",
              background: "var(--color-success-bg)",
              color: "var(--color-success)",
              border: "1px solid var(--color-success)",
              borderRadius: 6,
              cursor: loading ? "not-allowed" : "pointer",
              fontSize: 13,
              opacity: loading ? 0.6 : 1,
            }}
          >
            {loading ? "In progress…" : "Confirm"}
          </button>
          <button
            onClick={onCancel}
            disabled={loading}
            style={{
              padding: "6px 14px",
              background: "var(--bg-subtle)",
              color: "var(--text-secondary)",
              border: "1px solid var(--border-subtle)",
              borderRadius: 6,
              cursor: loading ? "not-allowed" : "pointer",
              fontSize: 13,
              opacity: loading ? 0.6 : 1,
              marginLeft: "auto",
            }}
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
