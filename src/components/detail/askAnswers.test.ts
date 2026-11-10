import { describe, it, expect } from "vitest";
import {
  applyFreeText,
  applyToggle,
  resolveQuestionAnswer,
  buildAnswersMap,
  isComplete,
  inputFor,
  type PerQuestionState,
} from "./askAnswers";
import { makeAskResponse } from "./PermissionBlock";
import type { AskUserQuestion } from "../../types/agent";

/**
 * Story 4.19 — logique PURE du carrousel multi-questions. Le projet n'a aucun rendu de
 * composant testé (décision #11) : on teste l'accumulation, la résolution par question, la
 * complétude et le tri canonique — jamais le DOM.
 */

const single = (question: string): AskUserQuestion => ({
  question,
  options: [{ label: "Yes" }, { label: "No" }],
  multiSelect: false,
});

const multi = (question: string): AskUserQuestion => ({
  question,
  options: [{ label: "A" }, { label: "B" }, { label: "C" }],
  multiSelect: true,
});

describe("askAnswers — logique pure du carrousel", () => {
  it("reponse_complete_un_seul_send_avec_toutes_les_cles", () => {
    const q1 = single("Deploy?");
    const q2 = multi("Which envs?");
    const q3 = single("Notify?");
    const questions = [q1, q2, q3];

    let state: PerQuestionState = {};
    state = { ...state, [q1.question]: applyToggle(q1, inputFor(state, q1.question), "Yes", true) };
    state = { ...state, [q2.question]: applyToggle(q2, inputFor(state, q2.question), "A", true) };
    state = { ...state, [q2.question]: applyToggle(q2, inputFor(state, q2.question), "C", true) };
    state = { ...state, [q3.question]: applyToggle(q3, inputFor(state, q3.question), "No", true) };

    const answers = buildAnswersMap(questions, state);
    expect(answers).toEqual({
      "Deploy?": "Yes",
      "Which envs?": "A, C",
      "Notify?": "No",
    });

    // La forme du JSON reste STRICTEMENT celle de la 4.4 — on n'a fait qu'ajouter des clés.
    const parsed = JSON.parse(makeAskResponse(questions, answers));
    expect(parsed.hookSpecificOutput.decision.updatedInput.answers).toEqual(answers);
    expect(parsed.hookSpecificOutput.decision.updatedInput.questions).toEqual(questions);
  });

  it("envoyer_tout_desactive_tant_qu_incomplet", () => {
    const q1 = single("Deploy?");
    const q2 = single("Notify?");
    const questions = [q1, q2];

    let state: PerQuestionState = {
      [q1.question]: applyToggle(q1, inputFor({}, q1.question), "Yes", true),
    };
    expect(isComplete(questions, buildAnswersMap(questions, state))).toBe(false);

    state = { ...state, [q2.question]: applyToggle(q2, inputFor(state, q2.question), "No", true) };
    expect(isComplete(questions, buildAnswersMap(questions, state))).toBe(true);
  });

  it("texte_libre_et_selection_mutuellement_exclusifs", () => {
    const q = multi("Pick?");

    // Sélection PUIS texte → le texte gagne (la sélection est effacée).
    let input = applyToggle(q, inputFor({}, q.question), "A", true);
    input = applyToggle(q, input, "B", true);
    input = applyFreeText(input, "custom answer");
    expect(input.selectedLabels).toEqual([]);
    expect(resolveQuestionAnswer(q, input)).toBe("custom answer");

    // Texte PUIS sélection → la sélection gagne (le texte est effacé).
    let input2 = applyFreeText(inputFor({}, q.question), "typed first");
    input2 = applyToggle(q, input2, "B", true);
    expect(input2.freeText).toBe("");
    expect(resolveQuestionAnswer(q, input2)).toBe("B");
  });

  it("texte_libre_espaces_seuls_ne_repond_pas", () => {
    const q = single("Deploy?");
    const input = applyFreeText(inputFor({}, q.question), "   ");
    expect(resolveQuestionAnswer(q, input)).toBe("");
    expect(isComplete([q], buildAnswersMap([q], { [q.question]: input }))).toBe(false);
  });

  it("ordre_canonique_des_libelles_multi", () => {
    const q = multi("Pick?"); // options A, B, C
    // On coche dans l'ordre C, A, B → la réponse suit l'ordre des OPTIONS : "A, B, C".
    let input = applyToggle(q, inputFor({}, q.question), "C", true);
    input = applyToggle(q, input, "A", true);
    input = applyToggle(q, input, "B", true);
    expect(resolveQuestionAnswer(q, input)).toBe("A, B, C");
  });

  it("une_seule_question_forme_inchangee", () => {
    // N=1 doit produire EXACTEMENT le JSON d'aujourd'hui (non-régression, décision #8).
    const q = single("Deploy?");
    const input = applyToggle(q, inputFor({}, q.question), "Yes", true);
    const answers = buildAnswersMap([q], { [q.question]: input });
    expect(answers).toEqual({ "Deploy?": "Yes" });

    const parsed = JSON.parse(makeAskResponse([q], answers));
    expect(parsed).toEqual({
      hookSpecificOutput: {
        hookEventName: "PermissionRequest",
        decision: {
          behavior: "allow",
          updatedInput: { questions: [q], answers: { "Deploy?": "Yes" } },
        },
      },
    });
  });

  it("libelles_identiques_partagent_la_cle", () => {
    // Décision #10 — collision assumée : deux questions au même texte → une seule clé.
    const qa: AskUserQuestion = { question: "Same?", options: [{ label: "X" }], multiSelect: false };
    const qb: AskUserQuestion = { question: "Same?", options: [{ label: "Y" }], multiSelect: false };
    const questions = [qa, qb];
    const state: PerQuestionState = {
      "Same?": applyToggle(qb, inputFor({}, "Same?"), "Y", true),
    };
    const answers = buildAnswersMap(questions, state);
    expect(Object.keys(answers)).toEqual(["Same?"]);
    // Une seule clé remplie suffit à rendre les DEUX questions « répondues » (collision documentée).
    expect(isComplete(questions, answers)).toBe(true);
  });
});
