import { describe, it, expect } from "vitest";
import { makeAskResponse } from "./PermissionBlock";
import { buildAnswersMap, type PerQuestionState } from "./askAnswers";
import type { AskUserQuestion } from "../../types/agent";

/**
 * Story 8.4 — miroir Vitest du golden Rust `format_reponse_identique_au_poste`.
 * Même vecteur d'entrée → même JSON sémantique que `notify::make_ask_response`.
 */
describe("makeAskResponse (contrat poste ↔ mobile)", () => {
  it("format_reponse_identique_au_poste", () => {
    const questions: AskUserQuestion[] = [
      {
        question: "Deploy?",
        header: "Release",
        options: [
          { label: "prod", description: "live" },
          { label: "staging" },
        ],
        multiSelect: true,
      },
    ];
    // Miroir exact de handleSendSelection : selected.join(", ")
    const answers = { "Deploy?": "prod, staging" };
    const json = makeAskResponse(questions, answers);
    const parsed = JSON.parse(json);
    expect(parsed).toEqual({
      hookSpecificOutput: {
        hookEventName: "PermissionRequest",
        decision: {
          behavior: "allow",
          updatedInput: { questions, answers },
        },
      },
    });
  });

  it("mono-choix : un seul libellé", () => {
    const questions: AskUserQuestion[] = [
      {
        question: "Pick one?",
        options: [{ label: "Alpha" }, { label: "Beta" }],
        multiSelect: false,
      },
    ];
    const parsed = JSON.parse(makeAskResponse(questions, { "Pick one?": "Beta" }));
    expect(parsed.hookSpecificOutput.decision.updatedInput.answers).toEqual({
      "Pick one?": "Beta",
    });
  });
});

/**
 * Story 8.9, AC n°1 — **vecteur croisé** avec le golden Rust
 * `notify::tests::format_reponse_identique_au_poste_multi_questions` (`src-tauri/src/notify/mod.rs`).
 * Le littéral d'entrée et le JSON attendu sont écrits À L'IDENTIQUE des deux côtés.
 *
 * ⚠️ La map `answers` est construite en appelant le **vrai** `buildAnswersMap` — recopier la map
 * à la main ne testerait que le littéral, et c'est exactement comme ça qu'une double vérité repart
 * (cicatrice `transcript_hash`).
 */
describe("contrat multi-questions poste ↔ mobile (Story 8.9)", () => {
  const questions: AskUserQuestion[] = [
    {
      question: "Deploy?",
      header: "Release",
      options: [{ label: "prod", description: "live" }, { label: "staging" }],
      multiSelect: false,
    },
    {
      question: "Which checks?",
      options: [{ label: "lint" }, { label: "tests" }, { label: "types" }],
      multiSelect: true,
    },
    {
      question: "Notes?",
      options: [{ label: "none" }],
      multiSelect: false,
    },
  ];

  it("format_reponse_identique_au_poste_multi_questions", () => {
    // Q0 → staging ; Q1 → tapée dans l'ordre `types` PUIS `lint` ; Q2 → none.
    const state: PerQuestionState = {
      "Deploy?": { selectedLabels: ["staging"], freeText: "" },
      "Which checks?": { selectedLabels: ["types", "lint"], freeText: "" },
      "Notes?": { selectedLabels: ["none"], freeText: "" },
    };
    const answers = buildAnswersMap(questions, state);
    // Propriété 1 : ordre des OPTIONS, jamais l'ordre de tap. Propriété 2 : clé = texte.
    expect(answers).toEqual({
      "Deploy?": "staging",
      "Which checks?": "lint, types",
      "Notes?": "none",
    });

    const parsed = JSON.parse(makeAskResponse(questions, answers));
    expect(parsed).toEqual({
      hookSpecificOutput: {
        hookEventName: "PermissionRequest",
        decision: {
          behavior: "allow",
          updatedInput: {
            questions,
            answers: {
              "Deploy?": "staging",
              "Which checks?": "lint, types",
              "Notes?": "none",
            },
          },
        },
      },
    });
  });

  it("une question sans réponse est ABSENTE de la map", () => {
    const state: PerQuestionState = {
      "Deploy?": { selectedLabels: ["staging"], freeText: "" },
      "Which checks?": { selectedLabels: ["types", "lint"], freeText: "" },
      // "Notes?" laissée vide.
    };
    const answers = buildAnswersMap(questions, state);
    expect(answers).toEqual({ "Deploy?": "staging", "Which checks?": "lint, types" });
    expect("Notes?" in answers).toBe(false);

    // Le golden Rust asserte ce cas sur le JSON PRODUIT ; s'arrêter à `buildAnswersMap` ici
    // laissait les deux moitiés du vecteur croisé à des étages différents (revue 8.9).
    const parsed = JSON.parse(makeAskResponse(questions, answers));
    expect(parsed.hookSpecificOutput.decision.updatedInput.answers).toEqual({
      "Deploy?": "staging",
      "Which checks?": "lint, types",
    });
  });
});
