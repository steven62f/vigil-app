// Story 4.19 — logique PURE du carrousel multi-questions `AskUserQuestion`, extraite de
// `PermissionBlock.tsx` pour être testable en Vitest (le projet n'a aucun rendu de composant
// testé, cf. décision #11). La forme du JSON envoyé reste portée par `makeAskResponse` —
// ces helpers ne font que CALCULER la map `answers` : ils n'ouvrent aucune nouvelle source de
// vérité vis-à-vis du golden Rust ([[project_transcript_hash_contrat]]).

import type { AskUserQuestion } from "../../types/agent";

/** État de saisie d'UNE question. Les deux champs sont mutuellement exclusifs (décision #4). */
export interface QuestionInput {
  selectedLabels: string[];
  freeText: string;
}

/** État de saisie de toutes les questions, indexé par TEXTE de question (décision #10 : clé = texte). */
export type PerQuestionState = Record<string, QuestionInput>;

const EMPTY_INPUT: QuestionInput = { selectedLabels: [], freeText: "" };

/** Lit l'état d'une question (jamais répondue → créneau vide). */
export function inputFor(state: PerQuestionState, questionText: string): QuestionInput {
  return state[questionText] ?? EMPTY_INPUT;
}

/**
 * Décision #4 — taper du texte EFFACE la sélection d'options de cette question.
 * Exclusion mutuelle, dernière action gagne : un créneau réponse = UNE seule string.
 */
export function applyFreeText(_input: QuestionInput, text: string): QuestionInput {
  return { selectedLabels: [], freeText: text };
}

/**
 * Décision #4 — cocher/décocher une option EFFACE le texte libre de cette question.
 * `multiSelect` est propre à la question (AC2) : radio → une seule, cases → cumul.
 */
export function applyToggle(
  question: AskUserQuestion,
  input: QuestionInput,
  label: string,
  checked: boolean,
): QuestionInput {
  if (question.multiSelect) {
    const selectedLabels = checked
      ? [...input.selectedLabels, label]
      : input.selectedLabels.filter((l) => l !== label);
    return { selectedLabels, freeText: "" };
  }
  return { selectedLabels: checked ? [label] : [], freeText: "" };
}

/**
 * Résout la réponse d'une question en UNE string (décision #4).
 * Texte libre si non vide ; sinon libellés sélectionnés triés par ORDRE DES OPTIONS —
 * jamais l'ordre de clic (décision #5, cf. `PermissionBlock.tsx` `handleSendSelection`).
 */
export function resolveQuestionAnswer(question: AskUserQuestion, input: QuestionInput): string {
  const free = input.freeText.trim();
  if (free) return free;
  // `options` est typé requis, mais le rendu le protège déjà via `q?.options?.map` : on aligne
  // le helper (appelé à chaque render via `buildAnswersMap`) pour ne pas crasher sur un payload
  // malformé où `options` serait absent.
  return (question.options ?? [])
    .filter((o) => input.selectedLabels.includes(o.label))
    .map((o) => o.label)
    .join(", ");
}

/**
 * Construit la map `answers` COMPLÈTE (décision #3 : c'est ce qu'on enverra).
 * N'inclut que les questions au créneau non vide. Clé = texte de la question (décision #10 :
 * deux libellés identiques s'écrasent — collision assumée, héritée du contrat de Claude).
 */
export function buildAnswersMap(
  questions: AskUserQuestion[],
  state: PerQuestionState,
): Record<string, string> {
  const answers: Record<string, string> = {};
  for (const q of questions) {
    const value = resolveQuestionAnswer(q, inputFor(state, q.question));
    if (value) answers[q.question] = value;
  }
  return answers;
}

/**
 * Décision #6 — « Envoyer tout » n'est actif que si TOUTES les questions sont répondues
 * (aucun envoi partiel). Une map `answers` ne contient que les créneaux non vides, donc la
 * complétude se vérifie sur la présence d'une clé par question.
 */
export function isComplete(questions: AskUserQuestion[], answers: Record<string, string>): boolean {
  return questions.length > 0 && questions.every((q) => !!answers[q.question]);
}
