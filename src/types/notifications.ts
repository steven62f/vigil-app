// Notifications mobiles (Epic 8, Story 8.1) — miroir des types Rust de `src-tauri/src/notify/`.
//
// Le front ne connaît AUCUN canal : ni le nom du service, ni son URL, ni la forme de ses
// identifiants. Il ne manipule que « un secret est-il posé ? », « un destinataire est-il
// appairé ? » et « quels événements sont cochés ? ». Brancher Discord ensuite ne touchera pas
// une ligne de ce fichier.

/** Miroir exact de `NotifKind` côté Rust (sérialisé tel quel : "Waiting" | "Ended" | "Error"). */
export type NotifKind = "Waiting" | "Ended" | "Error";

export interface NotifEvents {
  waiting: boolean;
  ended: boolean;
  error: boolean;
}

/**
 * Vue lecture des réglages, renvoyée par `notify_get_settings`.
 *
 * `hasToken` remplace le secret lui-même : celui-ci vit dans le gestionnaire d'identifiants de
 * l'OS et ne traverse JAMAIS la frontière Rust → front.
 */
export interface NotifSettingsDto {
  enabled: boolean;
  events: NotifEvents;
  chatId: string | null;
  botUsername: string | null;
  hasToken: boolean;
  lastError: string | null;
}

/** Payload de l'événement `notify:state` (échec d'un envoi asynchrone). */
export interface NotifyStateEvent {
  lastError: string | null;
}

/**
 * Destinataire capté par `notify_start_pairing`, **avant** mémorisation.
 *
 * La capture ne prouve pas l'identité : n'importe qui connaissant le nom du bot peut lui écrire,
 * et la file d'updates couvre 24 h. `displayName` est donc montré à l'utilisateur pour qu'il
 * confirme — c'est le seul rempart contre un tiers qui aurait pris sa place.
 *
 * Un `chatId` commençant par `-` désigne un **groupe** (convention des canaux qui exposent un
 * identifiant numérique signé). Depuis la Story 8.3, les messages portent des boutons Approve/Deny
 * et tout membre du groupe peut les taper : l'UI d'appairage doit en avertir avant confirmation.
 */
export interface PairedRecipient {
  chatId: string;
  displayName: string;
}

/** `true` si le destinataire capté est un groupe (identifiant négatif). */
export function isGroupRecipient(chatId: string): boolean {
  return chatId.startsWith("-");
}

/** Défauts livrés : Waiting ON, Ended ON, Error OFF — miroir de `impl Default for NotifEvents`. */
export const DEFAULT_NOTIF_EVENTS: NotifEvents = {
  waiting: true,
  ended: true,
  error: false,
};

export const DEFAULT_NOTIF_SETTINGS: NotifSettingsDto = {
  enabled: false,
  events: DEFAULT_NOTIF_EVENTS,
  chatId: null,
  botUsername: null,
  hasToken: false,
  lastError: null,
};

// ── Surcharges par agent (Story 8.2) ────────────────────────────────────────────

/**
 * Miroir de `AgentNotifOverrides` côté Rust (`HashMap<NotifKind, bool>`), persisté sous
 * `agentNotifs:{transcriptPath}` : `{"Waiting": false}`.
 *
 * Une clé **absente** signifie « hérite du réglage global », jamais « désactivé » — c'est ce qui
 * fait qu'un changement du global se répercute aussitôt sur les agents non surchargés.
 *
 * ⚠️ Ce type ne sert qu'à **éditer** les règles. La décision d'envoyer se prend intégralement
 * côté Rust (`should_send`), car les notifications doivent partir fenêtre fermée.
 */
export type AgentNotifOverrides = Partial<Record<NotifKind, boolean>>;

/**
 * Libellé de chaque événement, dans l'ordre d'affichage du menu.
 *
 * ⚠️ Le type `Record<NotifKind, string>` est le **verrou d'exhaustivité** côté TS (revue 8.2) :
 * ajouter une variante à `NotifKind` casse la compilation ici, exactement comme le `match`
 * exhaustif la casse côté Rust. Une simple liste de littéraux, elle, resterait silencieusement
 * incomplète — et le menu perdrait une ligne sans que rien ne le signale.
 */
const NOTIF_KIND_LABELS: Record<NotifKind, string> = {
  Waiting: "Waiting for you",
  Ended: "Finished",
  Error: "Errors",
};

/** Les lignes du menu, dérivées des libellés — miroir TS d'`ALL_NOTIF_KINDS` (Rust). */
export const NOTIF_KIND_ROWS: { kind: NotifKind; label: string }[] = (
  Object.keys(NOTIF_KIND_LABELS) as NotifKind[]
).map((kind) => ({ kind, label: NOTIF_KIND_LABELS[kind] }));

/**
 * Correspondance `NotifKind` → champ de `NotifEvents`. Même verrou d'exhaustivité que ci-dessus,
 * et pendant TS de `NotifEvents::get` (Rust) : le seul endroit du front qui associe une nature
 * d'événement à sa case globale.
 */
const NOTIF_EVENT_FIELD: Record<NotifKind, keyof NotifEvents> = {
  Waiting: "waiting",
  Ended: "ended",
  Error: "error",
};

/**
 * Valeur EFFECTIVE affichée pour une ligne : surcharge de l'agent > réglage global.
 * Fonction PURE — miroir TS de `resolve_notification` (Rust), testée sans rendu.
 */
export function effectiveNotif(
  kind: NotifKind,
  overrides: AgentNotifOverrides,
  global: NotifEvents,
): boolean {
  const own = overrides[kind];
  if (own !== undefined) return own;
  return global[NOTIF_EVENT_FIELD[kind]];
}

/** La ligne porte-t-elle une règle propre à cet agent (badge « agent ») ou hérite-t-elle ? */
export function isNotifOverridden(kind: NotifKind, overrides: AgentNotifOverrides): boolean {
  return overrides[kind] !== undefined;
}

/** Étape courante de l'assistant d'appairage. */
export type PairingStep = "token" | "chat" | "ready";

/**
 * Où en est l'appairage ? Fonction PURE : c'est elle qui pilote l'affichage de la section
 * (assistant vs statut connecté) et le grisage des cases d'événements.
 *
 * - `"token"` : aucun secret posé → il faut créer le bot et coller son jeton.
 * - `"chat"`  : secret validé, mais personne n'a encore parlé au bot → il faut appairer.
 * - `"ready"` : secret + destinataire → tout est prêt.
 *
 * Un `chatId` sans jeton retombe volontairement sur `"token"` : sans secret, aucun envoi n'est
 * possible même avec un destinataire mémorisé (cas d'un secret retiré à la main du gestionnaire
 * d'identifiants de l'OS).
 */
export function pairingStep(settings: NotifSettingsDto): PairingStep {
  if (!settings.hasToken) return "token";
  if (!settings.chatId) return "chat";
  return "ready";
}

/**
 * Traduction des codes d'erreur produits par le Rust en message lisible. Un code inconnu est
 * affiché tel quel plutôt que masqué : mieux vaut un message technique qu'un échec silencieux.
 */
export function notifErrorLabel(code: string): string {
  if (code === "invalid_token") return "Invalid token — check what you pasted.";
  if (code === "empty_token") return "Paste your bot token first.";
  if (code === "missing_token") return "No token stored — add your bot token first.";
  if (code === "missing_recipient") return "Nothing to confirm — start the pairing again.";
  if (code === "not_paired") return "Not paired yet — finish the setup steps first.";
  if (code === "pairing_timeout") return "No message received. Send /start to your bot, then retry.";
  // Story 8.3 — l'appairage et la boucle retour se partagent le même canal entrant : elles ne
  // peuvent pas tourner ensemble. Sans ce libellé, la garde backend se déclencherait muette
  // (bouton actif, code brut en console) — exactement le défaut relevé à la revue 6.33.
  //
  // ⚠️ Formulation NEUTRE : depuis la revue 8.3, la garde refuse aussi quand la table est vide
  // mais que la boucle de réception draine encore son long-poll (~35 s après la dernière
  // résolution). Nommer « un agent attend » y serait faux et enverrait rediagnostiquer côté
  // agents un refus qui n'a rien à voir.
  if (code === "busy_receiving")
    return "The notification channel is busy receiving — try again in a moment.";
  if (code === "api_rejected") return "The service rejected the request — try pairing again.";
  if (code === "invalid_response") return "Unexpected response — are you behind a captive portal?";
  if (code.startsWith("rate_limited")) return "Rate limited — try again in a moment.";
  if (code.startsWith("network_error")) return "Network error — check your connection.";
  if (code.startsWith("keyring_error")) return "Could not access the Windows credential manager.";
  if (code.startsWith("api_error_")) return `Service error (${code.replace("api_error_", "HTTP ")}).`;
  return code;
}
