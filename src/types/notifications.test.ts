import { describe, it, expect } from "vitest";
import {
  isGroupRecipient,
  pairingStep,
  notifErrorLabel,
  effectiveNotif,
  isNotifOverridden,
  DEFAULT_NOTIF_SETTINGS,
  DEFAULT_NOTIF_EVENTS,
  NOTIF_KIND_ROWS,
  type AgentNotifOverrides,
  type NotifEvents,
  type NotifSettingsDto,
} from "./notifications";

const dto = (over: Partial<NotifSettingsDto> = {}): NotifSettingsDto => ({
  ...DEFAULT_NOTIF_SETTINGS,
  ...over,
});

describe("pairingStep", () => {
  it("part de l'étape jeton quand rien n'est posé", () => {
    expect(pairingStep(dto())).toBe("token");
  });

  it("passe à l'appairage une fois le jeton validé", () => {
    expect(pairingStep(dto({ hasToken: true, botUsername: "monbot" }))).toBe("chat");
  });

  it("est prêt quand jeton ET destinataire sont là", () => {
    expect(pairingStep(dto({ hasToken: true, chatId: "123", botUsername: "monbot" }))).toBe(
      "ready",
    );
  });

  it("retombe sur l'étape jeton si le secret a disparu du coffre de l'OS", () => {
    // Destinataire mémorisé mais secret retiré à la main du gestionnaire d'identifiants :
    // aucun envoi n'est possible, l'assistant doit repartir du début.
    expect(pairingStep(dto({ hasToken: false, chatId: "123" }))).toBe("token");
  });

  it("ne dépend pas de l'interrupteur maître", () => {
    // `enabled` pilote l'ENVOI, pas l'état de l'appairage : les deux sont indépendants.
    const paired = { hasToken: true, chatId: "123" };
    expect(pairingStep(dto({ ...paired, enabled: false }))).toBe("ready");
    expect(pairingStep(dto({ ...paired, enabled: true }))).toBe("ready");
  });

  it("traite un chatId vide comme non appairé", () => {
    expect(pairingStep(dto({ hasToken: true, chatId: "" }))).toBe("chat");
  });
});

describe("isGroupRecipient", () => {
  // Story 8.3 review D2 — un destinataire de type groupe laisse tout membre approuver.
  it("détecte un identifiant de groupe (préfixe -)", () => {
    expect(isGroupRecipient("-1001234567890")).toBe(true);
    expect(isGroupRecipient("-42")).toBe(true);
  });

  it("laisse passer une conversation privée", () => {
    expect(isGroupRecipient("123456789")).toBe(false);
    expect(isGroupRecipient("")).toBe(false);
  });
});

describe("défauts livrés", () => {
  it("miroir de impl Default for NotifEvents : Waiting ON, Ended ON, Error OFF", () => {
    expect(DEFAULT_NOTIF_EVENTS).toEqual({ waiting: true, ended: true, error: false });
  });

  it("rien ne part tant que l'utilisateur n'a pas activé", () => {
    expect(DEFAULT_NOTIF_SETTINGS.enabled).toBe(false);
  });
});

// ── Story 8.2 : surcharges par agent (logique d'affichage du menu, testée SANS rendu) ──

describe("effectiveNotif", () => {
  const global: NotifEvents = { waiting: true, ended: false, error: true };

  it("utilise la surcharge de l'agent quand elle existe", () => {
    const o: AgentNotifOverrides = { Waiting: false, Ended: true };
    expect(effectiveNotif("Waiting", o, global)).toBe(false);
    expect(effectiveNotif("Ended", o, global)).toBe(true);
  });

  it("hérite du global quand la clé est absente", () => {
    const o: AgentNotifOverrides = {};
    expect(effectiveNotif("Waiting", o, global)).toBe(true);
    expect(effectiveNotif("Ended", o, global)).toBe(false);
    expect(effectiveNotif("Error", o, global)).toBe(true);
  });

  it("ne confond pas une surcharge à false avec une clé absente", () => {
    // Le piège du `??` : `false` est une valeur réglée, pas un trou.
    expect(effectiveNotif("Waiting", { Waiting: false }, { ...global, waiting: true })).toBe(false);
  });

  it("suit immédiatement un changement de global pour un agent non surchargé", () => {
    const o: AgentNotifOverrides = {};
    expect(effectiveNotif("Waiting", o, { ...global, waiting: true })).toBe(true);
    expect(effectiveNotif("Waiting", o, { ...global, waiting: false })).toBe(false);
    // …alors qu'un agent surchargé reste sur sa valeur.
    const s: AgentNotifOverrides = { Waiting: true };
    expect(effectiveNotif("Waiting", s, { ...global, waiting: false })).toBe(true);
  });
});

describe("isNotifOverridden", () => {
  it("distingue le badge agent du badge global", () => {
    const o: AgentNotifOverrides = { Waiting: false };
    expect(isNotifOverridden("Waiting", o)).toBe(true);
    expect(isNotifOverridden("Ended", o)).toBe(false);
  });

  it("après Reset, les 3 lignes repassent en héritage", () => {
    for (const { kind } of NOTIF_KIND_ROWS) {
      expect(isNotifOverridden(kind, {})).toBe(false);
    }
  });
});

describe("NOTIF_KIND_ROWS", () => {
  // L'exhaustivité elle-même est désormais verrouillée à la COMPILATION par les
  // `Record<NotifKind, …>` de notifications.ts (revue 8.2) : réaffirmer la liste ici serait une
  // tautologie. Ce qui reste vérifiable au runtime, c'est l'intégrité des lignes produites.
  it("ne produit ni doublon ni libellé vide", () => {
    const kinds = NOTIF_KIND_ROWS.map((r) => r.kind);
    expect(new Set(kinds).size).toBe(kinds.length);
    expect(NOTIF_KIND_ROWS.every((r) => r.label.trim().length > 0)).toBe(true);
  });

  it("chaque ligne lit une case globale DISTINCTE (attrape un copier-coller dans la table)", () => {
    // Un seul champ global à `true` à la fois : une seule ligne doit s'allumer. Si deux lignes
    // pointaient vers le même champ de NotifEvents, deux s'allumeraient.
    for (const { kind } of NOTIF_KIND_ROWS) {
      const eteint: NotifEvents = { waiting: false, ended: false, error: false };
      const allumees = NOTIF_KIND_ROWS.filter((r) => effectiveNotif(r.kind, {}, eteint));
      expect(allumees).toHaveLength(0);
      expect(effectiveNotif(kind, {}, eteint)).toBe(false);
    }
    const un: NotifEvents = { waiting: true, ended: false, error: false };
    expect(NOTIF_KIND_ROWS.filter((r) => effectiveNotif(r.kind, {}, un))).toHaveLength(1);
    const deux: NotifEvents = { waiting: true, ended: true, error: false };
    expect(NOTIF_KIND_ROWS.filter((r) => effectiveNotif(r.kind, {}, deux))).toHaveLength(2);
  });
});

describe("notifErrorLabel", () => {
  it("traduit les codes connus", () => {
    expect(notifErrorLabel("invalid_token")).toMatch(/Invalid token/);
    expect(notifErrorLabel("pairing_timeout")).toMatch(/\/start/);
    expect(notifErrorLabel("rate_limited:7")).toMatch(/Rate limited/);
    // Story 8.3 — la garde d'exclusion appairage / boucle retour doit se voir à l'écran, pas
    // seulement en console : une garde muette se lit comme une application cassée.
    // Revue 8.3 — le libellé doit rester NEUTRE : la garde refuse aussi pendant le drainage du
    // long-poll, table vide, sans qu'aucun agent n'attende.
    expect(notifErrorLabel("busy_receiving")).toMatch(/busy receiving/);
    expect(notifErrorLabel("busy_receiving")).not.toMatch(/agent/i);
    expect(notifErrorLabel("network_error: dns")).toMatch(/Network error/);
    expect(notifErrorLabel("api_error_500")).toContain("HTTP 500");
  });

  it("affiche un code inconnu tel quel plutôt que de le masquer", () => {
    expect(notifErrorLabel("truc_inattendu")).toBe("truc_inattendu");
  });
});
