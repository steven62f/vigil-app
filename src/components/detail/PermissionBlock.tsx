import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentSnapshot, AskUserQuestion } from "../../types/agent";
import { errorMessages } from "../../lib/errorMessages";
import {
  applyFreeText,
  applyToggle,
  buildAnswersMap,
  inputFor,
  isComplete,
  type PerQuestionState,
} from "./askAnswers";

interface Props {
  agent: AgentSnapshot;
}

/** Contrat partagé avec Rust `make_ask_response` (Story 8.4) — golden tests croisés. */
export function makeAskResponse(
  questions: AskUserQuestion[],
  answers: Record<string, string>,
): string {
  return JSON.stringify({
    hookSpecificOutput: {
      hookEventName: "PermissionRequest",
      decision: { behavior: "allow", updatedInput: { questions, answers } },
    },
  });
}

function formatToolInput(toolName: string, toolInput: Record<string, unknown>): string {
  // Noms Claude/Codex (PascalCase) ET natifs Grok (snake_case) — cf. Story 6.30. Comparaison
  // insensible à la casse : Grok a déjà renommé ses outils une fois (snake_case → PascalCase,
  // cf. is_ask_question_tool côté Rust) — se prémunir d'une prochaine dérive similaire.
  const name = toolName.toLowerCase();
  if (name === "bash" || name === "run_terminal_command") {
    const cmd = typeof toolInput.command === "string" ? toolInput.command : "";
    return cmd.length > 100 ? cmd.slice(0, 100) + "…" : cmd;
  }
  if (["read", "edit", "write", "search_replace", "read_file"].includes(name)) {
    const path =
      typeof toolInput.file_path === "string"
        ? toolInput.file_path
        : typeof toolInput.path === "string"
          ? toolInput.path
          : "";
    return path;
  }
  const json = JSON.stringify(toolInput);
  return json.length > 120 ? json.slice(0, 120) + "…" : json;
}

function ReplyInTerminalHint({ text }: { text: string }) {
  return (
    <div style={{ fontSize: 12, fontWeight: 700, color: "var(--state-waiting-approve)", marginTop: 8 }}>
      ↳ {text}
    </div>
  );
}

interface HoverBtnProps {
  onClick: () => void;
  label: string;
  baseStyle: React.CSSProperties;
  hoverStyle: React.CSSProperties;
  disabled?: boolean;
}

function HoverBtn({ onClick, label, baseStyle, hoverStyle, disabled }: HoverBtnProps) {
  const [hovered, setHovered] = useState(false);
  const [active, setActive] = useState(false);

  return (
    <button
      disabled={disabled}
      onClick={onClick}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => { setHovered(false); setActive(false); }}
      onMouseDown={() => setActive(true)}
      onMouseUp={() => setActive(false)}
      style={{
        padding: "5px 12px",
        borderRadius: 4,
        border: "none",
        cursor: disabled ? "default" : "pointer",
        fontSize: 12,
        fontWeight: 600,
        transition: "opacity 80ms, transform 80ms",
        opacity: disabled ? 0.4 : active ? 0.65 : hovered ? 0.85 : 1,
        transform: active ? "scale(0.96)" : "none",
        ...(hovered && !disabled ? hoverStyle : baseStyle),
      }}
    >
      {label}
    </button>
  );
}

const blockStyle: React.CSSProperties = {
  margin: "0 10px 10px",
  padding: "10px 12px",
  background: "var(--bg-warning)",
  borderLeft: "3px solid var(--state-waiting-approve)",
  borderRadius: 4,
};

/**
 * Story 4.19 — carrousel de questions `AskUserQuestion` (1 à 4). Présente les questions UNE à la
 * fois avec navigation `‹ ›` + jauge de complétude, accumule les réponses par question, et
 * n'envoie qu'une seule fois — via `makeAskResponse(questions, answers)` — quand TOUTES sont
 * répondues. FRONT-ONLY : la forme du JSON est immuable, on n'ajoute que des clés à `answers`.
 * N=1 : aucun chrome de navigation, comportement perçu identique à avant (décision #8).
 */
function QuestionCarousel({ id, questions }: { id: string; questions: AskUserQuestion[] }) {
  const [index, setIndex] = useState(0);
  const [perQuestion, setPerQuestion] = useState<PerQuestionState>({});
  const [error, setError] = useState<string | null>(null);

  const multiQuestion = questions.length > 1;
  const q = questions[index];

  // Garde de robustesse : `questions` vide/malformé, ou `index` hors bornes (ex. une nouvelle
  // demande plus courte réutilisant le même composant) → `q` est `undefined`. On ne déréférence
  // jamais `q` sans l'avoir vérifié (l'ancien rendu protégeait déjà tout via `q?.`).
  if (!q) return null;

  const input = inputFor(perQuestion, q.question);
  const answers = buildAnswersMap(questions, perQuestion);
  const complete = isComplete(questions, answers);

  const setInput = (next: PerQuestionState[string]) =>
    setPerQuestion((prev) => ({ ...prev, [q.question]: next }));

  const handleToggle = (label: string, checked: boolean) => {
    setInput(applyToggle(q, input, label, checked));
    // Décision #6 / AC5 — répondre à une question single-select fait AVANCER à la suivante (fluidité),
    // sans rien envoyer. Pas d'auto-avance sur la dernière (sinon on ne peut plus revenir corriger).
    if (!q.multiSelect && checked && index < questions.length - 1) {
      setIndex(index + 1);
    }
  };

  const handleSendAll = () => {
    setError(null);
    // Décision #7 — un seul appel, même chemin que la 4.4. La map `answers` est COMPLÈTE.
    const responseJson = makeAskResponse(questions, buildAnswersMap(questions, perQuestion));
    invoke("send_message", { id, responseJson })
      .then(() => {
        setPerQuestion({});
        setIndex(0);
      })
      .catch((err) => setError(errorMessages[err as string] ?? errorMessages.send_message_failed));
  };

  return (
    <div style={blockStyle}>
      {multiQuestion && (
        <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 8 }}>
          <HoverBtn
            onClick={() => setIndex(index - 1)}
            label="‹"
            disabled={index === 0}
            baseStyle={{ background: "var(--bg-subtle)", color: "var(--text-primary)", padding: "2px 10px" }}
            hoverStyle={{ background: "var(--bg-badge-hover)", color: "var(--text-primary)", padding: "2px 10px" }}
          />
          <span style={{ fontSize: 11, fontWeight: 700, color: "var(--text-secondary)" }}>
            Question {index + 1}/{questions.length}
          </span>
          <HoverBtn
            onClick={() => setIndex(index + 1)}
            label="›"
            disabled={index === questions.length - 1}
            baseStyle={{ background: "var(--bg-subtle)", color: "var(--text-primary)", padding: "2px 10px" }}
            hoverStyle={{ background: "var(--bg-badge-hover)", color: "var(--text-primary)", padding: "2px 10px" }}
          />
          <div style={{ display: "flex", gap: 4, marginLeft: "auto" }}>
            {questions.map((qq, i) => {
              const answered = !!answers[qq.question];
              return (
                <span
                  key={i}
                  title={`Question ${i + 1}${answered ? " — answered" : ""}`}
                  style={{
                    width: 8,
                    height: 8,
                    borderRadius: "50%",
                    border: "1px solid var(--state-waiting-approve)",
                    background: answered ? "var(--state-waiting-approve)" : "transparent",
                    opacity: i === index ? 1 : 0.7,
                  }}
                />
              );
            })}
          </div>
        </div>
      )}

      {q?.header && (
        <div style={{ fontSize: 11, fontWeight: 700, color: "var(--text-secondary)", marginBottom: 4 }}>
          {q.header}
        </div>
      )}
      {q?.question && (
        <div style={{ fontSize: 12, color: "var(--text-primary)", marginBottom: 8 }}>
          {q.question}
        </div>
      )}

      {q?.options?.map((opt) => (
        <label
          key={opt.label}
          style={{ display: "block", marginBottom: 4, cursor: "pointer", fontSize: 12 }}
        >
          <input
            type={q.multiSelect ? "checkbox" : "radio"}
            name={`perm-${id}-${index}`}
            value={opt.label}
            checked={input.selectedLabels.includes(opt.label)}
            onChange={(e) => handleToggle(opt.label, e.target.checked)}
            style={{ marginRight: 6 }}
          />
          <span style={{ color: "var(--text-primary)" }}>{opt.label}</span>
          {opt.description && (
            <span style={{ color: "var(--text-muted)", fontSize: 10, marginLeft: 6 }}>
              {opt.description}
            </span>
          )}
        </label>
      ))}

      {/* Échappatoire texte libre PAR question (AC3). Décision #4 : taper efface la sélection. */}
      <div style={{ marginTop: 8, paddingTop: 8, borderTop: "1px solid var(--border-separator)" }}>
        <textarea
          value={input.freeText}
          onChange={(e) => setInput(applyFreeText(input, e.target.value))}
          placeholder="Or reply freely…"
          rows={2}
          style={{
            width: "100%",
            boxSizing: "border-box",
            background: "var(--bg-input-subtle)",
            border: "1px solid var(--border-subtle)",
            borderRadius: 4,
            color: "var(--text-primary)",
            fontSize: 12,
            padding: "4px 8px",
            resize: "none",
            fontFamily: "inherit",
          }}
        />
      </div>

      {/* Décision #6 — SEUL déclencheur d'envoi, désactivé tant que les N questions ne sont pas
          toutes répondues (aucun envoi partiel). N=1 : bouton « Send » comme avant. */}
      <HoverBtn
        onClick={handleSendAll}
        label={multiQuestion ? "Send all" : "Send"}
        disabled={!complete}
        baseStyle={{ background: "var(--color-success-bg)", color: "var(--color-success)", marginTop: 8 }}
        hoverStyle={{ background: "var(--color-success-hover)", color: "var(--color-success)", marginTop: 8 }}
      />
      {error && (
        <div style={{ color: "var(--color-danger)", fontSize: 11, marginTop: 4 }}>{error}</div>
      )}
    </div>
  );
}

export function PermissionBlock({ agent }: Props) {
  const [error, setError] = useState<string | null>(null);

  const id = agent.transcriptPath;

  if (!agent.permissionRequest) {
    return (
      <div style={{ padding: "8px 12px", fontSize: 11, color: "var(--text-muted)" }}>
        Permission request pending…
      </div>
    );
  }

  const { toolName, toolInput } = agent.permissionRequest;

  const handleApprove = () => {
    setError(null);
    invoke("approve_agent", { id })
      .catch((err) => setError(errorMessages[err as string] ?? errorMessages.approve_failed));
  };

  const handleDeny = () => {
    setError(null);
    invoke("deny_agent", { id })
      .catch((err) => setError(errorMessages[err as string] ?? errorMessages.deny_failed));
  };

  // Story 6.35 — « Always approve » (PORTÉE AGENT STRICTE, jamais global) : approuve la demande
  // courante ET passe la catégorie de l'outil en Approve POUR CET AGENT. La catégorie est dérivée
  // côté Rust (même tool_name_to_key, source unique) depuis sourceTool+toolName ; on ne touche
  // JAMAIS toolPermissions/saveSettings ici (le réglage global passe par la fenêtre Settings).
  const handleAlwaysApprove = (toolName: string) => {
    setError(null);
    invoke("always_approve_agent", {
      transcriptPath: id,
      sourceTool: agent.sourceTool,
      toolName,
    }).catch((err) => setError(errorMessages[err as string] ?? errorMessages.approve_failed));
  };

  if (agent.state === "waitingSpecific") {
    const questions = (toolInput.questions as AskUserQuestion[] | undefined) ?? [];

    // Story 6.30 / 8.4 — lecture seule pilotée par la DEMANDE (`readOnly`), pas par `sourceTool`.
    // Grok pose `read_only: true` dans `emit_question` ; le front n'a plus de seconde source de vérité.
    // Décision #9/#13 — la lecture seule n'est PAS actionnable : invite terminal, jamais de carrousel.
    if (agent.permissionRequest.readOnly === true) {
      const q = questions[0];
      return (
        <div style={blockStyle}>
          {q?.header && (
            <div style={{ fontSize: 11, fontWeight: 700, color: "var(--text-secondary)", marginBottom: 4 }}>
              {q.header}
            </div>
          )}
          {q?.question && (
            <div style={{ fontSize: 12, color: "var(--text-primary)", marginBottom: 8 }}>
              {q.question}
            </div>
          )}
          {q?.options?.map((opt) => (
            <label
              key={opt.label}
              style={{ display: "block", marginBottom: 4, cursor: "default", fontSize: 12, opacity: 0.6 }}
            >
              <input
                type={q.multiSelect ? "checkbox" : "radio"}
                name={`perm-${id}`}
                value={opt.label}
                disabled
                checked={false}
                readOnly
                style={{ marginRight: 6 }}
              />
              <span style={{ color: "var(--text-primary)" }}>{opt.label}</span>
              {opt.description && (
                <span style={{ color: "var(--text-muted)", fontSize: 10, marginLeft: 6 }}>
                  {opt.description}
                </span>
              )}
            </label>
          ))}
          <ReplyInTerminalHint text="Reply in the terminal" />
        </div>
      );
    }

    // Décision #8 — le carrousel est le cas GÉNÉRAL ; N=1 en est le cas dégénéré (aucun chrome de
    // navigation, comportement perçu identique à avant). La clé rend un carrousel neuf par demande.
    return <QuestionCarousel key={id} id={id} questions={questions} />;
  }

  // waitingApprove
  // Story 6.30 (Grok) — permission en mode `default`/ask (`readOnly`) : le hook ne peut PAS approuver
  // depuis Vigil (l'`allow` ne bypass pas le TUI) → affichage lecture-seule + invite terminal, pas de
  // boutons. `readOnly` absent/false (Claude/Codex, ou Grok `bypassPermissions`) → boutons actionnables.
  const permReadOnly = agent.permissionRequest.readOnly === true;
  return (
    <div style={blockStyle}>
      <div style={{ fontSize: 12, fontWeight: 700, color: "var(--text-primary)", marginBottom: 4 }}>
        {toolName}
      </div>
      <div style={{ fontSize: 11, color: "var(--text-secondary)", fontFamily: "monospace", wordBreak: "break-all" }}>
        {formatToolInput(toolName, toolInput as Record<string, unknown>)}
      </div>
      {permReadOnly ? (
        <ReplyInTerminalHint text="Approve or deny in the terminal" />
      ) : (
        <>
          <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
            <HoverBtn
              onClick={handleApprove}
              label="✓ Approve"
              baseStyle={{ background: "var(--color-success-bg)", color: "var(--color-success)" }}
              hoverStyle={{ background: "var(--color-success-hover)", color: "var(--color-success)" }}
            />
            <HoverBtn
              onClick={() => handleAlwaysApprove(toolName)}
              label="✓✓ Always approve"
              baseStyle={{ background: "var(--bg-subtle)", color: "var(--color-success)" }}
              hoverStyle={{ background: "var(--color-success-bg)", color: "var(--color-success)" }}
            />
            <HoverBtn
              onClick={handleDeny}
              label="✗ Deny"
              baseStyle={{ background: "var(--color-danger-bg)", color: "var(--color-danger)", marginLeft: "auto" }}
              hoverStyle={{ background: "var(--color-danger-hover)", color: "var(--color-danger)", marginLeft: "auto" }}
            />
          </div>
          {error && (
            <div style={{ color: "var(--color-danger)", fontSize: 11, marginTop: 4 }}>{error}</div>
          )}
        </>
      )}
    </div>
  );
}
