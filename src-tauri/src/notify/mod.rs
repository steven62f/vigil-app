//! Notifications mobiles (Epic 8, Story 8.1) — socle **agnostique du canal ET de l'outil**.
//!
//! ## Ce que ce module est
//!
//! Un **transport sortant** : une transition d'état d'agent entre, un message texte sort. La
//! décision (« faut-il notifier ? ») et la mise en forme sont des **fonctions pures** sans
//! `AppHandle` ni I/O — c'est ce qui les rend testables et c'est là que la Story 8.2 (granularité
//! par agent) viendra s'insérer, au même endroit, sans déplacer de code.
//!
//! ## Agnosticité du canal
//!
//! La logique métier ne connaît que `NotificationChannel` / `ActiveChannel`. Le vocabulaire d'un
//! canal donné (URL d'API, identifiant de destinataire, format des erreurs) reste **confiné** dans
//! son implémentation (`telegram.rs`). Brancher Discord = une variante de plus dans `ActiveChannel`
//! et un fichier voisin — **zéro** ligne à toucher dans `manager.rs`, `pipe/manager.rs`,
//! `commands/` ou le front.
//!
//! ## Agnosticité de l'outil
//!
//! Le déclencheur est une **transition d'`AgentState`**, pas un type de hook. `AgentState` est le
//! seul vocabulaire commun aux 4 outils supportés (Claude/Codex/Grok/Antigravity) : écouter
//! `HookEvent::Stop` serait outil-spécifique par accident. `source_tool` n'est utilisé que comme
//! **étiquette d'affichage**, jamais pour brancher un comportement.
//!
//! ## Sécurité (règle permanente de l'epic)
//!
//! Le message ne porte qu'un **résumé de métadonnées** — le même libellé que celui déjà affiché
//! dans le Notch. Jamais de `tool_input`, de réponse d'outil, de contenu de fichier, de chemin de
//! transcript, de `session_id` ni de secret. Cf. `format_notif_text`.

pub mod manager;
pub mod secret;
pub mod telegram;

/// Destinataire capté à l'appairage, ré-exporté ici pour que les appelants (commandes, front)
/// n'aient jamais à nommer l'implémentation de canal. Une seconde implémentation rendrait le même
/// type : c'est le contrat de l'assistant d'appairage, pas un détail Telegram.
pub use telegram::PairedRecipient;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

use crate::agent::state::{AgentSnapshot, AgentState, SourceTool};
use crate::SETTINGS_STORE;

/// Clé de `vigil-settings.json` portant les réglages de notification.
///
/// ⚠️ Cette clé est possédée **exclusivement par le Rust** : le front ne l'écrit JAMAIS via
/// `saveSettings()`. Raison : `saveSettings` fait un `store.set(key, value)` **en bloc**, donc un
/// écrit côté front écraserait l'objet entier et perdrait l'identifiant de destinataire produit
/// par l'appairage côté Rust. Un seul écrivain = zéro course.
pub const NOTIF_SETTINGS_KEY: &str = "notifSettings";

/// Longueur maximale du résumé embarqué dans un message (en **caractères**, pas en octets).
const SUMMARY_MAX_CHARS: usize = 200;

/// Plafond du délai de ré-essai imposé par un canal (garde-fou : un canal qui demanderait
/// d'attendre une heure ne doit pas immobiliser la boucle d'envoi).
const MAX_RETRY_AFTER_SECS: u64 = 30;

/// Préfixe d'erreur normalisé signalant « ralentis, réessaie dans N secondes ». Produit par les
/// implémentations de canal, interprété par `retry_after_secs` — le manager n'a donc pas besoin
/// de connaître le format d'erreur d'un canal particulier.
pub const RATE_LIMITED_PREFIX: &str = "rate_limited:";

// ── Types du domaine ─────────────────────────────────────────────────────────────

/// Nature de l'événement notifié. Sérialise en `"Waiting"` / `"Ended"` / `"Error"` — miroir exact
/// du type TS `NotifKind` (`src/types/notifications.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotifKind {
    Waiting,
    Ended,
    Error,
}

/// Liste **exhaustive** des variantes, source unique pour les traitements groupés
/// (`set_all_agent_notif_rules`, tests). Ajouter un 4ᵉ type d'événement se répercute ici et
/// nulle part ailleurs — aucun recensement manuel dans les commandes.
pub const ALL_NOTIF_KINDS: [NotifKind; 3] =
    [NotifKind::Waiting, NotifKind::Ended, NotifKind::Error];

/// Surcharges de notification propres à UN agent (Story 8.2).
///
/// Clone structurel d'`AgentPermissionOverrides` (`permissions/mod.rs`) : une clé **absente**
/// signifie « hérite du global », jamais « désactivé ». C'est ce qui fait qu'un changement du
/// réglage global se répercute immédiatement sur tous les agents non surchargés, sans réécrire
/// la moindre clé de store.
pub type AgentNotifOverrides = HashMap<NotifKind, bool>;

/// Ce qui part réellement sur le canal. Volontairement **pauvre** : tout champ ajouté ici est un
/// champ qui peut fuiter vers un tiers.
///
/// Story 8.3 — `request_id` / clavier : corrélation opaque, pas de contenu d'outil.
/// Story 8.4 — `keyboard` paramétrable (Approve/Deny **ou** options) ; `body_extra` = exception
/// contenu **bornée** (libellés question/options uniquement).
#[derive(Debug, Clone, PartialEq)]
pub struct MobileNotif {
    pub kind: NotifKind,
    /// Nom convivial de l'agent (`custom_name` si l'utilisateur en a mis un, sinon le nom dérivé
    /// du dossier de travail). **Jamais** le chemin de transcript.
    pub agent_label: String,
    pub tool: SourceTool,
    /// Résumé d'action repris de `last_activity_label` (celui du Notch). `None` si l'agent n'a
    /// encore rien fait de nommable.
    pub summary: Option<String>,
    /// Identifiant de corrélation si un clavier est armé. `None` → notif-only.
    pub request_id: Option<String>,
    /// Clavier neutre. Si `None` et `request_id` présent → repli Approve/Deny (8.3).
    pub keyboard: Option<ActionKeyboard>,
    /// Exception 8.4 : corps question (header / question / puces d'options), déjà assaini.
    pub body_extra: Option<String>,
}

/// Les 3 événements activables indépendamment. Défauts livrés : `Waiting` ON, `Ended` ON,
/// `Error` OFF (une erreur est souvent transitoire et se voit déjà dans le Notch).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifEvents {
    #[serde(default = "yes")]
    pub waiting: bool,
    #[serde(default = "yes")]
    pub ended: bool,
    #[serde(default)]
    pub error: bool,
}

fn yes() -> bool {
    true
}

impl NotifEvents {
    /// Accès par `NotifKind` — **le seul** `match` du module entre une nature d'événement et sa
    /// case globale. Sans lui, le même `match` serait dupliqué dans `should_send` et dans la
    /// résolution par agent, avec la dérive qui va avec.
    pub fn get(&self, kind: NotifKind) -> bool {
        match kind {
            NotifKind::Waiting => self.waiting,
            NotifKind::Ended => self.ended,
            NotifKind::Error => self.error,
        }
    }
}

impl Default for NotifEvents {
    fn default() -> Self {
        Self { waiting: true, ended: true, error: false }
    }
}

/// Réglages persistés. **Aucun champ de secret** : le jeton d'accès du canal vit dans le
/// gestionnaire d'identifiants de l'OS (`secret.rs`), donc il n'y a rien à écrire en clair même
/// par accident — c'est une garantie **structurelle**, pas une discipline (test
/// `token_stocke_hors_config_claire`).
///
/// `#[serde(default)]` **champ par champ** (et non sur la struct) : un store écrit par une
/// version antérieure — clé absente, ou objet `events` partiel — est backfillé avec les défauts
/// **sans écraser** ce qui est déjà configuré. Même leçon que `toolPermissions` / `mascotByTool`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifSettings {
    /// Interrupteur maître. `false` par défaut : rien ne part tant que l'utilisateur n'a pas
    /// appairé ET activé.
    #[serde(default)]
    pub enabled: bool,
    /// Identifiant de destinataire capté à l'appairage (opaque, propre au canal actif).
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Nom affichable du bot appairé (« Connected ✓ @monbot »).
    #[serde(default)]
    pub bot_username: Option<String>,
    #[serde(default)]
    pub events: NotifEvents,
}

/// Identité du bot renvoyée par la validation d'un secret : c'est le seul retour que l'UI
/// affiche pour prouver que le jeton est bon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelIdentity {
    pub username: String,
}

// ── Boucle retour : types neutres (Story 8.3) ────────────────────────────────────
//
// ⚠️ AC8 — les *types* et *signatures* d'ici sont agnostiques du canal. Toute API propre
// à une implémentation (identifiants de message, flux d'updates, accusés natifs) reste
// confinée dans le fichier du canal (`telegram.rs`). Ce qui franchit la frontière est
// uniquement l'un des types opaques ci-dessous — c'est ce qui rend Discord (Gateway
// WebSocket, vocabulaire totalement différent) gratuit côté `manager.rs`.

/// Référence **opaque** vers un message déjà posté sur le canal. Le contenu de la chaîne est un
/// détail d'implémentation du canal : personne d'autre ne l'interprète, ne la journalise, ni ne la
/// persiste. Elle sert uniquement à retirer le clavier après coup (`disarm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef(String);

impl MessageRef {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }
    /// Réservé à l'implémentation de canal qui l'a produite.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Référence **opaque** vers l'action entrante en cours de traitement, nécessaire pour lui rendre
/// un accusé. Même contrat que `MessageRef` : opaque hors du canal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRef(String);

impl ActionRef {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Point de reprise du flux entrant, **en mémoire uniquement**, mais **hissé hors de la tâche**
/// poller (`Shared::action_cursor`) : il survit à un redémarrage de la *tâche* (arrêt table vide →
/// ré-armement, ou re-spawn après panic), si bien qu'un poller suivant reprend là où le précédent
/// s'est arrêté au lieu de rejouer le dernier tap (« Already handled » non sollicité). C'est la
/// « persistance » visée par la Story 8.5 (AC2), et l'avance est **monotone** (`set` garde le max).
///
/// Pas de persistance **disque** (survie au redémarrage du *processus*, décision Steven 2026-07-21) :
/// la table `PendingRequests` est en mémoire seule → aucune demande ne survit au processus → au
/// redémarrage de l'app aucun poller ne démarre, il n'y a **rien à reprendre**. La déduplication
/// d'un doublon est, elle, assurée par la consommation unique (`PendingRequests::take`), pas par le
/// curseur.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionCursor(Option<i64>);

impl ActionCursor {
    pub fn get(&self) -> Option<i64> {
        self.0
    }
    /// Avance **monotone** : l'offset ne recule jamais. Défensif — un canal peut, en théorie, rendre
    /// un update_id inférieur (réordonnancement, doublon rejoué) ; garder le max évite de re-demander
    /// des updates déjà dépassées et de rejouer un tap (AC2, Story 8.5).
    pub fn set(&mut self, value: i64) {
        self.0 = Some(self.0.map_or(value, |cur| cur.max(value)));
    }
}

/// Décision rendue par l'utilisateur depuis son téléphone (permissions 8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

/// Nature d'une action entrante, **neutre** (Story 8.3 + 8.4).
///
/// Les tags courts d'encodage (`a`/`d`/`o`/`s`) restent confinés dans l'implémentation de canal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingKind {
    Approve,
    Deny,
    /// Tap sur une option (indices 0-based de la **question** et de l'option au sein de celle-ci).
    ///
    /// Story 8.9 — le segment `question` est indispensable : le corps du message n'est **jamais**
    /// ré-édité pendant la sélection (décision 3), donc c'est le bouton qui doit porter à quelle
    /// question il appartient. Un lot d'une seule question mono garde le comportement 8.4
    /// (`question == 0`, un tap répond).
    Choose { question: u8, index: u8 },
    /// Bouton « ✅ Send » — présent dès que `needs_confirm` (multi, **ou** plusieurs questions).
    Confirm,
}

/// Clavier d'actions **paramétrable**, sans vocabulaire de canal (AC8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKeyboard {
    ApproveDeny { request_id: String },
    Choice {
        request_id: String,
        questions: Vec<QuestionData>,
        /// Une sélection par question — invariant `selected.len() == questions.len()`.
        selected: Vec<BTreeSet<usize>>,
    },
}

/// UNE question d'un lot `AskUserQuestion` (Story 8.9 — le lot peut en compter 1 à N).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionData {
    pub multi: bool,
    pub options: Vec<String>,
    pub header: Option<String>,
    pub question: String,
}

/// Données nécessaires pour armer un clavier de choix (extraites de la demande, hors I/O).
///
/// Story 8.9 — **conteneur du lot** : de 1 à N questions. Le champ JSON brut s'appelle désormais
/// `questions_json` (et non plus `questions`) exprès : le renommage force le compilateur à signaler
/// TOUS les usages de l'ancien champ au lieu de les laisser compiler avec un sens différent.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceData {
    pub questions: Vec<QuestionData>,
    /// Tableau `questions` tel quel — pour rejouer `make_ask_response` (décision 8).
    pub questions_json: serde_json::Value,
}

/// **Le pivot des deux comportements** (décision 9) : décide « un tap répond » vs « un tap
/// sélectionne, et il faut un ✅ Send ».
///
/// Une **seule** question **mono** reste le chemin 8.4 : zéro friction, aucun bouton Send. Dès
/// qu'il y a plusieurs questions — même toutes mono — un tap ne peut plus répondre, car l'envoi
/// partiel est interdit (décision 5).
///
/// Un lot **vide** rend `false` : il n'est de toute façon jamais armé (`choice_data_from_permission`
/// rend `None`), mais la fonction reste totale.
pub fn needs_confirm(questions: &[QuestionData]) -> bool {
    questions.len() > 1 || questions.first().is_some_and(|q| q.multi)
}

/// **Le second pivot** (piège n°1) : les boutons et le corps du message portent-ils un numéro de
/// question ?
///
/// Le corps n'étant **jamais** ré-édité pendant la sélection (décision 3), cette numérotation
/// partagée est le **seul** rattachement bouton→question — un `☐ Yes` sous trois questions est
/// ambigu dès qu'un libellé se répète. Elle doit donc être décidée **au même endroit** pour
/// `format_choice_body` (`mod.rs`) et `encode_keyboard` (`telegram.rs`) : le prédicat vivait en
/// littéral recopié dans les deux modules, et rien n'aurait vu les deux copies diverger — chaque
/// fichier ne teste que son propre côté (revue 8.9).
pub fn numbers_questions(questions: &[QuestionData]) -> bool {
    questions.len() > 1
}

/// Une action entrante décodée, **sans un mot de vocabulaire de canal**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAction {
    /// Corrèle l'action avec une `PendingRequest`. Inconnu ⇒ rejeté avec accusé.
    pub request_id: String,
    pub kind: IncomingKind,
    /// Identifiant de la conversation d'où vient l'action. Comparé à `settings.chat_id` AVANT
    /// toute résolution (AC5) : le clavier n'existe que dans la conversation appairée, donc c'est
    /// bien l'identité du destinataire qui est vérifiée, pas celle d'un expéditeur arbitraire.
    pub sender_id: String,
    /// De quoi accuser réception de cette action précise.
    pub action_ref: ActionRef,
}

/// Issue d'une demande, telle qu'elle est affichée sur le message déjà reçu au téléphone quand on
/// retire son clavier. La **mention de l'origine** distingue « c'est moi qui viens de taper » de
/// « ça s'est décidé sans moi » — sans elle, un message qui change tout seul est inquiétant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    ApprovedFromPhone,
    DeniedFromPhone,
    ApprovedAtDesk,
    DeniedAtDesk,
    /// Story 8.4 — réponse à une question à choix depuis le téléphone.
    AnsweredFromPhone,
    /// Story 8.4 — réponse à une question depuis la Vue Détail (`send_message`).
    AnsweredAtDesk,
    /// Résolue directement dans le TERMINAL (approve/deny ou réponse au prompt CLI) : Vigil détecte
    /// l'agent quitter l'état d'attente POUR REPRENDRE son travail (`Working`/`Idle`) sans qu'aucune
    /// voie poste/mobile n'ait désarmé. On ne connaît pas le verdict exact (le CLI ne le dit pas) →
    /// étiquette NEUTRE, sans ✅/❌. Les sorties anormales (interrupt/fin/erreur) restent en `Expired`.
    HandledInTerminal,
    /// Auto-deny du pipe au bout de la fenêtre d'approbation.
    Expired,
    /// Une NOUVELLE demande du même agent a évincé celle-ci (`PendingApprovals::register`).
    Superseded,
    /// La demande a déjà été résolue ailleurs (poste, auto-deny, course) — cause inconnue côté
    /// téléphone. Message **neutre** : étiqueter `Superseded` ici mentait (« remplacée ») alors
    /// que le cas nominal est l'expiration ou une décision prise au poste.
    AlreadyHandled,
}

impl RequestOutcome {
    /// Suffixe ajouté au texte d'origine du message. Ne contient **rien** de neuf : ni chemin, ni
    /// nom d'outil, ni identifiant.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::ApprovedFromPhone => "✅ Approved from your phone",
            Self::DeniedFromPhone => "❌ Denied from your phone",
            Self::ApprovedAtDesk => "✅ Approved from Vigil",
            Self::DeniedAtDesk => "❌ Denied from Vigil",
            Self::AnsweredFromPhone => "Answered from your phone",
            Self::AnsweredAtDesk => "Answered from Vigil",
            Self::HandledInTerminal => "Handled in the terminal",
            Self::Expired => "⏱ Expired — nothing was executed",
            Self::Superseded => "↩︎ Replaced by a newer request",
            Self::AlreadyHandled => "Already handled",
        }
    }
}

/// Réponse injectée dans le pipeline d'approbation pour une décision téléphonique.
/// Extrait en fonction pure pour qu'une inversion Approve/Deny casse un test dédié (AC9).
pub fn response_for_decision(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approve => crate::pipe::protocol::ALLOW_RESPONSE,
        ApprovalDecision::Deny => crate::pipe::protocol::DENY_RESPONSE,
    }
}

/// Issue affichée sur le message quand la décision vient du téléphone.
pub fn outcome_for_phone(decision: ApprovalDecision) -> RequestOutcome {
    match decision {
        ApprovalDecision::Approve => RequestOutcome::ApprovedFromPhone,
        ApprovalDecision::Deny => RequestOutcome::DeniedFromPhone,
    }
}

// ── Table des demandes en attente (Story 8.3) ────────────────────────────────────

/// Nature d'une demande armée (Story 8.3 permissions / 8.4 choix).
#[derive(Debug, Clone)]
pub enum PendingKind {
    ApproveDeny,
    Choice {
        questions: Vec<QuestionData>,
        /// Une sélection par question. ⚠️ Invariant `selected.len() == questions.len()`, posé à la
        /// construction (`arm_request`) et jamais recalculé : toute indexation passe par `get(q)`,
        /// jamais `selected[q]`. Un `callback_data` forgé porte un `q` arbitraire (0..=255) et un
        /// panic ici tuerait la boucle de réception de TOUS les agents.
        selected: Vec<BTreeSet<usize>>,
        questions_json: serde_json::Value,
    },
}

/// Une demande notifiée avec boutons, en attente d'un tap.
///
/// ⚠️ **Jamais persistée.** Une demande ne survit pas au processus : le `handle_client` qui
/// l'attendait est mort avec lui, il n'y aurait plus rien à résoudre. Persister créerait des
/// demandes zombies, résolvables sans destinataire.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub request_id: String,
    /// Clé de résolution auprès de `PendingApprovals` — exactement celle d'`approve_agent`.
    pub transcript_path: String,
    /// Aligné sur la fenêtre d'approbation **effective** de cet agent : un tap ne peut pas arriver
    /// dans une fenêtre où le pipe a déjà rendu un auto-deny.
    pub expires_at: Instant,
    /// Connu seulement APRÈS l'envoi (cf. « piège n°1 ») : l'entrée est créée sans lui.
    pub message: Option<MessageRef>,
    /// Texte d'origine du message, pour le ré-éditer en y ajoutant l'issue.
    pub text: String,
    pub kind: PendingKind,
}

/// Table **en mémoire** des demandes armées. Le verrou n'est JAMAIS tenu pendant un `await`
/// réseau : toutes les méthodes sont synchrones et rendent des valeurs possédées.
#[derive(Debug, Default)]
pub struct PendingRequests {
    inner: Mutex<HashMap<String, PendingRequest>>,
}

impl PendingRequests {
    pub fn insert(&self, request: PendingRequest) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(request.request_id.clone(), request);
        }
    }

    /// **Consommation unique** : c'est l'anti-rejeu (AC5). Un second tap ne trouve plus rien, quel
    /// que soit l'état du réseau ou du désarmement.
    pub fn take(&self, request_id: &str) -> Option<PendingRequest> {
        self.inner.lock().ok()?.remove(request_id)
    }

    /// Le clavier de la demande est-il de type Approve/Deny ? — **sans** la consommer.
    ///
    /// `None` si la demande est absente (inconnue / déjà consommée) ou le verrou empoisonné.
    /// Sert à valider une action `a:`/`d:` **avant** `take` : sans ce contrôle, un callback au
    /// mauvais type (forgé, ou id réattribué) retirerait une demande `Choice` **vivante** de la
    /// table sans la résoudre ni désarmer son clavier → boutons zombies jusqu'au TTL/auto-deny.
    pub fn is_approve_deny(&self, request_id: &str) -> Option<bool> {
        let map = self.inner.lock().ok()?;
        map.get(request_id)
            .map(|r| matches!(r.kind, PendingKind::ApproveDeny))
    }

    /// Retrait par agent — voie utilisée par les résolutions venues du poste (AC7).
    ///
    /// Si plusieurs entrées partagent le même `transcript_path` (course d'éviction), on retire
    /// celle dont le TTL est le **plus tardif** — c'est la demande vivante / la plus récente.
    /// Un `find` naïf sur `HashMap` est non déterministe et peut désarmer l'entrée morte en
    /// laissant la vivante armée (boutons encore actifs sur le message de remplacement).
    pub fn take_by_path(&self, transcript_path: &str) -> Option<PendingRequest> {
        let mut map = self.inner.lock().ok()?;
        let key = map
            .values()
            .filter(|r| r.transcript_path == transcript_path)
            .max_by_key(|r| r.expires_at)
            .map(|r| r.request_id.clone())?;
        map.remove(&key)
    }

    /// Complète l'entrée avec la référence de message rendue par l'envoi. Sans effet si la demande
    /// a déjà été consommée entre-temps (tap plus rapide que la réponse HTTP) : il n'y a alors
    /// plus rien à désarmer, et l'anti-rejeu a déjà fait son travail.
    pub fn attach_message(&self, request_id: &str, message: MessageRef) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(entry) = map.get_mut(request_id) {
                entry.message = Some(message);
            }
        }
    }

    /// Retire et rend les entrées périmées, pour que l'appelant les désarme.
    pub fn purge_expired(&self, now: Instant) -> Vec<PendingRequest> {
        let Ok(mut map) = self.inner.lock() else {
            return Vec::new();
        };
        let perimees: Vec<String> = map
            .values()
            .filter(|r| r.expires_at <= now)
            .map(|r| r.request_id.clone())
            .collect();
        perimees.iter().filter_map(|k| map.remove(k)).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().map(|m| m.is_empty()).unwrap_or(true)
    }

    /// Tap sur une option. **Une seule** question **mono** → répond immédiatement (chemin 8.4,
    /// décision 9) ; tout le reste → mutation de la sélection **sans** `take()`.
    ///
    /// Ordre des contrôles (inchangé depuis 8.4) : verrou → présence → expiration → **bornes** →
    /// mutation. Aucune I/O sous le verrou.
    pub fn apply_choose(
        &self,
        request_id: &str,
        question: usize,
        index: usize,
        now: Instant,
    ) -> ActionVerdict {
        let Ok(mut map) = self.inner.lock() else {
            return ActionVerdict::Unknown;
        };
        let Some(req) = map.get(request_id) else {
            return ActionVerdict::Unknown;
        };
        if req.expires_at <= now {
            let request = map.remove(request_id).expect("présent à l'instant");
            return ActionVerdict::Expired(request);
        }
        let PendingKind::Choice {
            questions,
            questions_json,
            ..
        } = &req.kind
        else {
            return ActionVerdict::Unknown;
        };
        // Bornes AVANT toute mutation : un `callback_data` forgé porte des indices arbitraires.
        let Some(q) = questions.get(question) else {
            return ActionVerdict::Unknown;
        };
        if index >= q.options.len() {
            return ActionVerdict::Unknown;
        }
        // Copié AVANT le ré-emprunt mutable : évite un `questions[question].multi` plus bas, dont
        // la sûreté aurait dépendu d'une lecture croisée avec ce contrôle de bornes (piège n°6 —
        // aucune indexation directe, même prouvée sûre à distance).
        let multi = q.multi;
        // Décision 9 — une seule question mono : le tap EST la réponse, comme en 8.4.
        if !needs_confirm(questions) {
            let questions = questions.clone();
            let questions_json = questions_json.clone();
            // Construit par position plutôt que par indexation : `selection[question]` aurait été
            // la seule indexation directe restante sur un `Vec` dimensionné localement.
            let selection: Vec<BTreeSet<usize>> = (0..questions.len())
                .map(|i| {
                    if i == question {
                        BTreeSet::from([index])
                    } else {
                        BTreeSet::new()
                    }
                })
                .collect();
            let request = map.remove(request_id).expect("présent");
            let response =
                make_ask_response_from_selections(&questions_json, &questions, &selection);
            return ActionVerdict::ResolveChoice(request, response);
        }

        let req = map.get_mut(request_id).expect("présent");
        let PendingKind::Choice {
            questions, selected, ..
        } = &mut req.kind
        else {
            return ActionVerdict::Unknown;
        };
        let Some(sel) = selected.get_mut(question) else {
            return ActionVerdict::Unknown;
        };
        if multi {
            // Bascule 8.4, inchangée.
            if sel.contains(&index) {
                sel.remove(&index);
            } else {
                sel.insert(index);
            }
        } else if sel.contains(&index) {
            // Miroir du poste : un `<input type="radio">` déjà coché n'émet aucun `onChange`.
            // Verdict DISTINCT de `Toggle` : ré-éditer un markup identique ferait répondre
            // `400 Bad Request: message is not modified` à Telegram (piège n°3) — donc ack seul.
            return ActionVerdict::AlreadySelected;
        } else {
            // Mono : la sélection se DÉPLACE (jamais deux sélections sur une question mono).
            sel.clear();
            sel.insert(index);
        }
        let keyboard = ActionKeyboard::Choice {
            request_id: request_id.to_string(),
            questions: questions.clone(),
            selected: selected.clone(),
        };
        ActionVerdict::Toggle {
            message: req.message.clone(),
            keyboard,
        }
    }

    /// « ✅ Send ». Take **seulement** si TOUTES les questions ont une réponse : l'envoi partiel est
    /// interdit (décision 5), et seul un Send complet consomme la demande.
    pub fn apply_confirm(&self, request_id: &str, now: Instant) -> ActionVerdict {
        let Ok(mut map) = self.inner.lock() else {
            return ActionVerdict::Unknown;
        };
        let Some(req) = map.get(request_id) else {
            return ActionVerdict::Unknown;
        };
        if req.expires_at <= now {
            let request = map.remove(request_id).expect("présent");
            return ActionVerdict::Expired(request);
        }
        let PendingKind::Choice {
            questions,
            selected,
            questions_json,
        } = &req.kind
        else {
            return ActionVerdict::Unknown;
        };
        // Un lot vide n'a aucune question manquante : sans cette garde, `missing` serait vide, la
        // demande serait CONSOMMÉE et l'agent débloqué avec `answers: {}`. Inatteignable en
        // production (`choice_data_from_permission` rejette le tableau vide) — mais `apply_confirm`
        // est le seul chemin qui fabrique une décision à partir de cet état, et sa sûreté ne doit
        // pas dépendre d'un invariant posé dans un autre module (revue 8.9).
        if questions.is_empty() {
            return ActionVerdict::Unknown;
        }
        // Une question dont AUCUN indice sélectionné n'est adressable compte comme non répondue :
        // la garde porte sur ce qui partirait réellement, pas sur la taille du `BTreeSet`.
        let missing: Vec<usize> = questions
            .iter()
            .enumerate()
            .filter(|(q, qd)| {
                selected
                    .get(*q)
                    .is_none_or(|s| !s.iter().any(|&i| i < qd.options.len()))
            })
            .map(|(q, _)| q)
            .collect();
        if !missing.is_empty() {
            // N == 1 : l'ack 8.4 (« Select at least one option ») reste le bon message — il n'y a
            // pas de « question 1 » à nommer quand il n'y en a qu'une. Les deux verdicts sont
            // délibérément distincts, pas fusionnés.
            if questions.len() <= 1 {
                return ActionVerdict::EmptySelection;
            }
            return ActionVerdict::IncompleteSelection { missing };
        }
        let questions_json = questions_json.clone();
        let questions = questions.clone();
        let selected = selected.clone();
        let request = map.remove(request_id).expect("présent");
        let response = make_ask_response_from_selections(&questions_json, &questions, &selected);
        ActionVerdict::ResolveChoice(request, response)
    }
}

/// Ce qu'il faut faire d'une action entrante. Séparé du traitement pour que **toute** la logique
/// de sécurité (expéditeur, anti-rejeu, expiration) soit exerçable sans réseau ni `AppHandle`.
#[derive(Debug)]
pub enum ActionVerdict {
    /// Expéditeur ≠ destinataire appairé : rien, pas même un accusé.
    Ignored,
    /// `request_id` inconnu ou déjà consommé → accusé explicatif, aucune décision fabriquée.
    Unknown,
    /// Send sans aucune option cochée sur une demande à **une seule** question — **pas** de `take`,
    /// accusé seulement. Conservé tel quel depuis la 8.4 (ack « Select at least one option »).
    EmptySelection,
    /// Story 8.9 — Send sur un lot de **plusieurs** questions dont au moins une reste sans réponse.
    /// **Pas** de `take` : l'envoi partiel est interdit (décision 5), la demande reste vivante.
    /// `missing` porte les indices 0-based des questions sans réponse, pour un ack qui NOMME ce
    /// qui manque.
    IncompleteSelection { missing: Vec<usize> },
    /// Story 8.9 — re-tap sur l'option **déjà** sélectionnée d'une question **mono** : aucune
    /// mutation, et surtout **aucun** `update_actions` (piège n°3 : Telegram refuse une ré-édition
    /// au markup identique). Ack seul — jamais de silence (décision #13 de la 8.4).
    AlreadySelected,
    /// Tap arrivé après la fenêtre d'approbation → **refusé, pas exécuté**.
    Expired(PendingRequest),
    /// Approve/Deny (8.3) à réinjecter dans `PendingApprovals`.
    Resolve(PendingRequest, ApprovalDecision),
    /// Choix validé (mono ou multi Send) — JSON de réponse prêt (contrat `make_ask_response`).
    ResolveChoice(PendingRequest, String),
    /// Bascule multi : demande **reste** dans la table ; clavier à ré-éditer.
    Toggle {
        message: Option<MessageRef>,
        keyboard: ActionKeyboard,
    },
}

/// Contrôles de sécurité d'une action entrante, dans l'ordre exigé par l'AC5 : identité de la
/// conversation → consommation / mutation sous verrou → expiration. Aucune I/O n'a lieu pendant
/// le verrou : c'est ce qui rend un second tap structurellement inerte (ou une bascule sûre).
pub fn classify_action(
    pending: &PendingRequests,
    paired_recipient: Option<&str>,
    action: &ChannelAction,
    now: Instant,
) -> ActionVerdict {
    if paired_recipient != Some(action.sender_id.as_str()) {
        return ActionVerdict::Ignored;
    }
    match action.kind {
        IncomingKind::Approve | IncomingKind::Deny => {
            // Valider le type **avant** de consommer : un `a:`/`d:` visant une demande `Choice`
            // ne doit pas détruire une entrée vivante (clavier zombie sinon). L'anti-rejeu reste
            // intact : c'est toujours `take` (ci-dessous) qui consomme, et une seule fois.
            match pending.is_approve_deny(&action.request_id) {
                Some(true) => {}
                // Absente / mauvais type : on NE consomme PAS l'entrée.
                Some(false) | None => return ActionVerdict::Unknown,
            }
            let Some(request) = pending.take(&action.request_id) else {
                // Course : consommée entre le contrôle de type et le take.
                return ActionVerdict::Unknown;
            };
            if request.expires_at <= now {
                return ActionVerdict::Expired(request);
            }
            let decision = match action.kind {
                IncomingKind::Approve => ApprovalDecision::Approve,
                IncomingKind::Deny => ApprovalDecision::Deny,
                _ => unreachable!(),
            };
            ActionVerdict::Resolve(request, decision)
        }
        IncomingKind::Choose { question, index } => {
            pending.apply_choose(&action.request_id, question as usize, index as usize, now)
        }
        IncomingKind::Confirm => pending.apply_confirm(&action.request_id, now),
    }
}

/// Identifiant de corrélation d'une demande : `{run_id}-{n}`.
///
/// **Aucune crate ajoutée** (pas d'`uuid`) : l'imprévisibilité n'achète rien ici — seul le chat
/// appairé peut poster une action, et l'anti-rejeu vient de la consommation unique dans la table.
/// Le préfixe de run (millis Unix au démarrage du processus) est ce qui empêche un bouton d'un run
/// PRÉCÉDENT de tomber sur un `n` réattribué après redémarrage.
pub fn next_request_id() -> String {
    static RUN_ID: OnceLock<u64> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let run = RUN_ID.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    format!("{run}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// La demande de **permission** porte-t-elle des boutons Approve/Deny ? (Story 8.3)
///
/// ⚠️ **Zéro branche sur `source_tool`**. La capacité se mesure sur la DEMANDE :
/// - Antigravity n'émet **aucun** `PermissionRequest` → jamais `WaitingApprove` sur permission ;
/// - Grok en mode `default`/ask est `read_only` (son hook a déjà `exit 0`) → approuver écrirait
///   dans un tuyau fermé ;
/// - `AskUserQuestion` produit `WaitingSpecific` → voir `is_choice_actionable` (Story 8.4).
pub fn is_actionable(agent: &AgentSnapshot) -> bool {
    agent.state == AgentState::WaitingApprove
        && agent.permission_request.as_ref().is_some_and(|r| !r.read_only)
}

/// La demande de **question à choix** porte-t-elle un clavier d'options ? (Story 8.4)
///
/// Critères (décision #2, élargis par la Story 8.9) — **zéro** `match source_tool` :
/// - `WaitingSpecific` ∧ `!read_only`
/// - **au moins une** question dans `tool_input.questions`
/// - **chaque** question a au moins une option avec un `label` non vide (décision 6 : une seule
///   question en texte libre pur fait tomber TOUT le lot en notif-only, puisqu'une réponse
///   partielle est interdite et que le téléphone n'a pas de capacité texte)
/// - le lot tient dans les trois plafonds de `choice_data_from_permission` — `MAX_QUESTIONS` et
///   `MAX_CHOICE_OPTIONS` (adressabilité `u8` du `callback_data`) et `MAX_TOTAL_OPTIONS` (taille
///   du clavier). Ils vivent là-bas, mais ce sont bien des critères d'armement : cette liste
///   serait mensongère sans eux.
///
/// Sinon → notif-only (texte libre pur → 8.6 différée, ou lecture seule Grok).
pub fn is_choice_actionable(agent: &AgentSnapshot) -> bool {
    if agent.state != AgentState::WaitingSpecific {
        return false;
    }
    let Some(pr) = agent.permission_request.as_ref() else {
        return false;
    };
    if pr.read_only {
        return false;
    }
    choice_data_from_permission(pr).is_some()
}

/// Permission **ou** choix : condition unique pour fenêtre longue + armement mobile.
pub fn is_mobile_actionable(agent: &AgentSnapshot) -> bool {
    is_actionable(agent) || is_choice_actionable(agent)
}

/// Extrait les données d'armement d'un clavier de choix, ou `None` si non actionnable.
pub fn choice_from_agent(agent: &AgentSnapshot) -> Option<ChoiceData> {
    if agent.state != AgentState::WaitingSpecific {
        return None;
    }
    let pr = agent.permission_request.as_ref()?;
    if pr.read_only {
        return None;
    }
    choice_data_from_permission(pr)
}

/// Plafond d'options armées : l'indice se décode en `u8` dans le `callback_data` (0..=255).
const MAX_CHOICE_OPTIONS: usize = 256;

/// Plafond de questions armées, même raison que `MAX_CHOICE_OPTIONS` : l'indice de question se
/// décode lui aussi en `u8`. Le domaine réel est 4 (`AskUserQuestion` plafonne à 4 questions × 4
/// options) ; la borne n'est là que pour interdire un clavier **partiellement mort**.
///
/// ⚠️ Depuis la revue 8.9, `MAX_TOTAL_OPTIONS` (100) mord toujours avant lui — une question porte
/// au moins une option, donc plus de 100 questions dépassent déjà le total. Cette borne est
/// conservée parce qu'elle dit une **autre** chose (l'adressabilité `u8`, indépendante de la
/// taille du clavier) et qu'elle redeviendrait mordante si le total était relevé.
const MAX_QUESTIONS: usize = 256;

/// Plafond du **total** de boutons d'un lot (Story 8.9, revue). `MAX_QUESTIONS` et
/// `MAX_CHOICE_OPTIONS` sont deux bornes **unitaires** issues de l'adressabilité `u8` : leur
/// produit (65 536 boutons, un par ligne) les franchit toutes les deux et ferait rejeter le
/// `sendMessage` par Telegram — la demande serait armée, `is_mobile_actionable` vrai, et
/// **aucune notification ne partirait** pendant que l'agent attend sa fenêtre de 1500 s. Un
/// clavier *totalement* mort, alors que l'intention déclarée des deux autres plafonds est
/// d'interdire un clavier *partiellement* mort. Le domaine réel est 16 (4 × 4) ; cette borne est
/// très au-dessus, et très en dessous de ce que le canal refuse.
const MAX_TOTAL_OPTIONS: usize = 100;

fn choice_data_from_permission(pr: &crate::agent::state::PermissionRequestInfo) -> Option<ChoiceData> {
    let questions_json = pr.tool_input.get("questions")?;
    let raw = questions_json.as_array()?;
    if raw.is_empty() || raw.len() > MAX_QUESTIONS {
        return None;
    }
    let mut total_options = 0usize;
    let mut questions = Vec::with_capacity(raw.len());
    for q in raw {
        let options: Vec<String> = q
            .get("options")?
            .as_array()?
            .iter()
            .filter_map(|o| {
                let label = o.get("label")?.as_str()?.trim();
                if label.is_empty() {
                    None
                } else {
                    Some(label.to_string())
                }
            })
            .collect();
        // Décision 6 — UNE question sans option exploitable fait tomber TOUT le lot en notif-only.
        if options.is_empty() {
            return None;
        }
        // L'indice d'option transite dans le `callback_data` et se décode en `u8` (`telegram.rs`) :
        // seuls les indices 0..=255 sont adressables. Au-delà, les boutons surnuméraires seraient
        // affichés mais non-tappables (parse `u8` échoue → tap ignoré, aucun ack). On dégrade
        // proprement en notif-only (AC3) plutôt que d'armer un clavier partiellement mort.
        if options.len() > MAX_CHOICE_OPTIONS {
            return None;
        }
        // Les deux plafonds ci-dessus sont unitaires : seul leur SOMME dit si le clavier tient
        // dans un message. Même dégradation propre en notif-only.
        total_options += options.len();
        if total_options > MAX_TOTAL_OPTIONS {
            return None;
        }
        let multi = q.get("multiSelect").and_then(|v| v.as_bool()).unwrap_or(false);
        let header = q
            .get("header")
            .and_then(|h| h.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let question = q
            .get("question")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        questions.push(QuestionData {
            multi,
            options,
            header,
            question,
        });
    }
    Some(ChoiceData {
        questions,
        questions_json: questions_json.clone(),
    })
}

/// Garde-fou global du corps assemblé : Telegram plafonne un message à 4096 caractères, et le corps
/// s'ajoute au reste de la notif (kind / agent / outil / résumé). On tronque **en dernier recours**
/// sur le total — jamais par option, pour que le texte intégral de chaque libellé reste présent tant
/// que le message tient (décision #6).
const CHOICE_BODY_MAX_CHARS: usize = 3500;

/// Corps exceptionnel d'une notif de question (header / question / puces). **Pur**, testable.
///
/// Le texte des options n'est **pas** tronqué par option (contrairement aux boutons) : c'est le
/// filet de la troncature bouton (décision #6). Seul un plafond global de sécurité s'applique.
///
/// Story 8.9 — N questions : un bloc par question, séparés par une ligne vide, chacun préfixé du
/// **même** numéro que ses boutons (piège n°1 : le corps n'étant jamais ré-édité pendant la
/// sélection, c'est cette numérotation partagée qui rattache un bouton à sa question). Sur **une
/// seule** question le rendu est **strictement** celui de la 8.4 — pas de numéro.
pub fn format_choice_body(questions: &[QuestionData]) -> String {
    let numeroter = numbers_questions(questions);
    let mut blocs = Vec::with_capacity(questions.len());
    for (n, q) in questions.iter().enumerate() {
        let mut lines = Vec::new();
        if let Some(h) = q.header.as_deref() {
            let h = flatten(h, 80);
            if !h.is_empty() {
                lines.push(h);
            }
        }
        let texte = flatten(&q.question, 200);
        if !texte.is_empty() {
            lines.push(texte);
        }
        // Le numéro préfixe la PREMIÈRE ligne du bloc (header s'il existe, sinon l'énoncé) pour
        // que le bloc reste lisible même sans énoncé.
        if numeroter {
            match lines.first_mut() {
                Some(first) => *first = format!("{}. {first}", n + 1),
                None => lines.push(format!("{}.", n + 1)),
            }
        }
        for opt in &q.options {
            lines.push(format!("• {}", sanitize_inline(opt)));
        }
        blocs.push(lines.join("\n"));
    }
    let body = blocs.join("\n\n");
    if body.chars().count() > CHOICE_BODY_MAX_CHARS {
        let kept: String = body.chars().take(CHOICE_BODY_MAX_CHARS).collect();
        format!("{kept}…")
    } else {
        body
    }
}

/// Contrat unique avec le poste (`makeAskResponse` dans `PermissionBlock.tsx`).
///
/// Golden tests : égalité **sémantique** après parse (l'ordre des clés JSON peut différer
/// entre `serde_json` et `JSON.stringify`).
pub fn make_ask_response(questions: &serde_json::Value, answers: &serde_json::Value) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {
                "behavior": "allow",
                "updatedInput": {
                    "questions": questions,
                    "answers": answers
                }
            }
        }
    })
    .to_string()
}

/// ⚠️ **AC n°1 — miroir sémantique de `buildAnswersMap`** (`src/components/detail/askAnswers.ts`).
///
/// Construit la map `answers` d'un lot de N questions, puis délègue l'enveloppe à
/// `make_ask_response`. **Un seul** constructeur d'`answers` vit côté Rust : deux rouvriraient la
/// double vérité à l'INTÉRIEUR de Rust, exactement la cicatrice de `transcript_hash`.
///
/// Règles recopiées **une par une** du poste :
/// - ordre = **ordre des options** de la question, jamais l'ordre de tap (l'itération d'un
///   `BTreeSet<usize>` est croissante — c'est ce qui le garantit sans tri explicite) ;
/// - jointure `", "` ;
/// - clé = **texte** de la question ; deux textes identiques ⇒ le **dernier écrase** (miroir exact
///   de l'affectation TS `answers[q.question] = value` ; collision assumée, héritée du contrat de
///   l'outil) ;
/// - valeur vide ⇒ clé **absente** de la map (miroir de `if (value) …`), et non chaîne vide.
///
/// **Divergence connue et acceptée** : le poste filtre les options par **libellé**
/// (`selectedLabels.includes(o.label)`), Rust par **indice**. Sur deux options au libellé identique
/// dans une même question, le poste émettrait le libellé deux fois, Rust une seule. Cas dégénéré
/// (l'outil ne produit pas ça) et comportement Rust préférable — mais c'est pourquoi le vecteur
/// croisé ne doit contenir aucun libellé dupliqué.
pub fn make_ask_response_from_selections(
    questions_json: &serde_json::Value,
    questions: &[QuestionData],
    selected: &[BTreeSet<usize>],
) -> String {
    debug_assert_eq!(
        questions.len(),
        selected.len(),
        "invariant posé dans `arm_request` : une sélection par question"
    );
    let mut answers = serde_json::Map::new();
    // `zip` plutôt qu'un `get(q).unwrap_or_default()` : l'appariement est structurel, sans
    // arithmétique d'indice. La complaisance de l'ancienne forme (produire une réponse AMPUTÉE
    // quand `selected` est plus court, au lieu d'échouer) était la seule voie par laquelle un
    // envoi partiel aurait pu sortir malgré la décision 5 (revue 8.9).
    for (question, sel) in questions.iter().zip(selected) {
        let value = question
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| sel.contains(i))
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !value.is_empty() {
            answers.insert(question.question.clone(), serde_json::Value::String(value));
        }
    }
    make_ask_response(questions_json, &serde_json::Value::Object(answers))
}

// ── Abstraction de canal ─────────────────────────────────────────────────────────

/// Un canal de notification sortante.
///
/// `async fn` en trait (AFIT, stable depuis Rust 1.75 ; toolchain projet 1.95) plutôt qu'un
/// `Box<dyn NotificationChannel>` : ce dernier imposerait la crate `async-trait` pour rien.
/// L'aiguillage se fait par `enum ActiveChannel` → dispatch **statique**, et Discord = une
/// variante de plus.
#[allow(async_fn_in_trait)] // dispatch statique par enum : aucun `dyn` n'est jamais construit.
pub trait NotificationChannel {
    /// Envoie le message. Toute erreur est une `String` normalisée (`network_error: …`,
    /// `invalid_token`, `rate_limited:{secs}`, `api_error_{code}`) — jamais un type de canal.
    ///
    /// Rend une **référence de message opaque** quand le canal en fournit une : c'est elle qui
    /// rend possible le désarmement hors tap mobile (AC7). `None` = canal sans notion de message
    /// éditable, ou réponse inexploitable — le reste continue de fonctionner (l'anti-rejeu ne
    /// dépend pas du désarmement).
    async fn send(&self, notif: &MobileNotif) -> Result<Option<MessageRef>, String>;
    /// Vérifie le secret et rend l'identité du bot.
    async fn validate(&self) -> Result<ChannelIdentity, String>;
    /// Attend (de façon **bornée**) les actions entrantes depuis le point de reprise, qu'elle
    /// avance. Rendre un vecteur vide est normal : c'est un tour d'attente sans tap.
    async fn poll_actions(&self, cursor: &mut ActionCursor) -> Result<Vec<ChannelAction>, String>;
    /// Accuse réception d'une action — **systématiquement**, succès comme rejet : sans accusé, le
    /// client laisse tourner une roue une trentaine de secondes, ce qui se lit comme une
    /// application cassée plutôt que comme un refus.
    async fn ack_action(&self, action: &ActionRef, text: &str) -> Result<(), String>;
    /// Retire le clavier d'un message déjà posté et y reflète l'issue.
    async fn disarm(&self, message: &MessageRef, text: &str) -> Result<(), String>;
    /// Ré-édite le clavier d'un message (bascule multi-choix 8.4) sans changer le corps.
    async fn update_actions(
        &self,
        message: &MessageRef,
        keyboard: &ActionKeyboard,
    ) -> Result<(), String>;
}

/// Canal effectivement utilisé. Une seule variante en 8.1 ; Discord s'ajoutera **ici** et nulle
/// part ailleurs.
pub enum ActiveChannel {
    Telegram(telegram::TelegramChannel),
}

impl ActiveChannel {
    /// Canal prêt à ENVOYER : exige un secret dans le coffre **et** un destinataire appairé.
    /// `None` → l'appelant court-circuite sans tenter la moindre I/O réseau (AC6).
    pub fn active(settings: &NotifSettings) -> Option<Self> {
        let token = secret::get_token(Self::secret_account())?;
        let chat_id = settings.chat_id.clone().filter(|c| !c.is_empty())?;
        Some(Self::Telegram(telegram::TelegramChannel::new(token, chat_id)))
    }

    /// Canal construit pour la SEULE validation d'un secret candidat, avant tout stockage : on
    /// n'a pas encore de destinataire à ce stade (il vient de l'appairage, qui suit).
    pub fn for_candidate_token(token: String) -> Self {
        Self::Telegram(telegram::TelegramChannel::new(token, String::new()))
    }

    /// Compte sous lequel le secret du canal actif est rangé dans le coffre de l'OS.
    pub fn secret_account() -> &'static str {
        telegram::SECRET_ACCOUNT
    }

    /// Appairage : attend que l'utilisateur se manifeste auprès du bot et rend le destinataire
    /// capté. Opération **exclusive** et bornée dans le temps.
    ///
    /// Le résultat n'est **pas** mémorisé ici : il est d'abord montré à l'utilisateur pour
    /// confirmation (« c'est bien vous ? »), seul rempart contre un tiers qui aurait écrit au bot
    /// pendant la fenêtre d'appairage.
    pub async fn pair(token: &str) -> Result<telegram::PairedRecipient, String> {
        telegram::pair(token).await
    }

    /// Refuse un secret candidat manifestement mal formé **avant** toute requête réseau.
    pub fn token_is_well_formed(token: &str) -> bool {
        telegram::token_is_well_formed(token)
    }

    /// URL à ouvrir dans le navigateur pour une étape de l'assistant d'appairage. Construite
    /// **côté Rust** : aucun ouvreur d'URL générique n'est exposé au front.
    pub fn link_url(kind: &str, bot_username: Option<&str>) -> Option<String> {
        telegram::link_url(kind, bot_username)
    }

    pub async fn send(&self, notif: &MobileNotif) -> Result<Option<MessageRef>, String> {
        match self {
            Self::Telegram(c) => c.send(notif).await,
        }
    }

    pub async fn validate(&self) -> Result<ChannelIdentity, String> {
        match self {
            Self::Telegram(c) => c.validate().await,
        }
    }

    pub async fn poll_actions(
        &self,
        cursor: &mut ActionCursor,
    ) -> Result<Vec<ChannelAction>, String> {
        match self {
            Self::Telegram(c) => c.poll_actions(cursor).await,
        }
    }

    pub async fn ack_action(&self, action: &ActionRef, text: &str) -> Result<(), String> {
        match self {
            Self::Telegram(c) => c.ack_action(action, text).await,
        }
    }

    pub async fn disarm(&self, message: &MessageRef, text: &str) -> Result<(), String> {
        match self {
            Self::Telegram(c) => c.disarm(message, text).await,
        }
    }

    pub async fn update_actions(
        &self,
        message: &MessageRef,
        keyboard: &ActionKeyboard,
    ) -> Result<(), String> {
        match self {
            Self::Telegram(c) => c.update_actions(message, keyboard).await,
        }
    }
}

// ── Décision : faut-il notifier ? (pur) ──────────────────────────────────────────

/// Table de transition **exhaustive** : quel type de notification produire quand un agent passe
/// de `prev` à `next` ?
///
/// - `prev == next` → rien (deux `Stop` d'affilée = une seule notification).
/// - `prev == None` → **notifie** : un agent créé directement dans l'état cible par un upsert
///   (un `Stop` sans `SessionStart` préalable) est un vrai événement, pas une anomalie.
/// - `Ended` provoqué par un **interrupt utilisateur** → rien : l'utilisateur était au clavier,
///   le prévenir serait du bruit.
/// - `Disconnected` → **jamais** : l'agent n'a rien terminé, il a disparu (fermeture de terminal,
///   `SessionEnd`, filet du `pid_watcher`).
/// - `Working` / `Idle` / `Unknown` → jamais (bruit permanent).
///
/// Fonction **pure** : aucun `AppHandle`, aucune I/O, aucun accès à l'horloge.
pub fn notif_kind_for_transition(
    prev: Option<AgentState>,
    next: AgentState,
    was_interrupt: bool,
) -> Option<NotifKind> {
    if prev.as_ref() == Some(&next) {
        return None;
    }
    match next {
        AgentState::Ended => {
            if was_interrupt {
                None
            } else {
                Some(NotifKind::Ended)
            }
        }
        AgentState::Error => Some(NotifKind::Error),
        AgentState::WaitingApprove | AgentState::WaitingSpecific => Some(NotifKind::Waiting),
        AgentState::Working
        | AgentState::Idle
        | AgentState::Disconnected
        | AgentState::Unknown => None,
    }
}

/// Résolution à **trois couches** : surcharge de l'agent > réglage global > défaut livré.
///
/// Miroir exact de `resolve_permission` (`permissions/mod.rs`), à une simplification près :
/// celui-ci reçoit un `tool_name` et le **catégorise** (`tool_name_to_key`) parce que des dizaines
/// de noms d'outils se rangent dans 8 catégories. Ici il n'y a **rien à dériver** — `NotifKind`
/// *est* la clé, produite en amont par `notif_kind_for_transition`.
///
/// L'**absence** de clé dans `overrides` vaut **héritage** (et non « copie figée ») : changer le
/// global bascule aussitôt tous les agents non surchargés.
///
/// Fonction **pure** : aucun `AppHandle`, aucune I/O.
pub fn resolve_notification(
    kind: NotifKind,
    overrides: &AgentNotifOverrides,
    global: &NotifEvents,
) -> bool {
    overrides.get(&kind).copied().unwrap_or_else(|| global.get(kind))
}

/// Filtrage complet : interrupteur maître, PUIS résolution à trois couches de l'événement.
///
/// L'interrupteur maître reste **au-dessus de tout** : une surcharge d'agent à `true` avec
/// `enabled == false` n'envoie rien. La Story 8.2 raffine le filtrage par événement, elle ne
/// contourne pas l'interrupteur général.
pub fn should_send(
    kind: NotifKind,
    settings: &NotifSettings,
    overrides: &AgentNotifOverrides,
) -> bool {
    settings.enabled && resolve_notification(kind, overrides, &settings.events)
}

// ── Mise en forme du message (pur) ───────────────────────────────────────────────

fn kind_line(kind: NotifKind) -> &'static str {
    match kind {
        NotifKind::Waiting => "⏸ Waiting for you",
        NotifKind::Ended => "✅ Finished",
        NotifKind::Error => "⛔ Error",
    }
}

/// Étiquette lisible de l'outil — usage **purement décoratif** (aucune branche de comportement
/// ne lit `source_tool`, cf. AC1).
fn tool_label(tool: &SourceTool) -> &'static str {
    match tool {
        SourceTool::ClaudeCode => "Claude Code",
        SourceTool::Codex => "Codex CLI",
        SourceTool::Grok => "Grok Build",
        SourceTool::Antigravity => "Antigravity CLI",
        SourceTool::Kimi => "Kimi CLI",
        SourceTool::Unknown => "Agent",
    }
}

/// Assainit un texte destiné à un message tiers : neutralise les caractères de contrôle et
/// normalise les espaces. **Ne tronque pas** — utilisé pour les libellés d'options dont le texte
/// intégral doit rester dans le corps (décision #6 ; la troncature est réservée aux boutons).
///
/// Les retours ligne / tabulations / caractères de contrôle deviennent des espaces : sans ça, un
/// libellé multi-ligne casserait la mise en forme du message.
fn sanitize_inline(text: &str) -> String {
    let flat = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Aplatit (`sanitize_inline`) **et** borne un fragment de texte.
///
/// Troncature en **caractères** (`chars`, pas d'octets) → jamais de coupe au milieu d'un caractère
/// UTF-8, donc jamais de message rejeté par le canal.
fn flatten(text: &str, max_chars: usize) -> String {
    let flat = sanitize_inline(text);
    if flat.chars().count() > max_chars {
        let kept: String = flat.chars().take(max_chars).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

/// Assainit un résumé d'action **destiné à un tiers**.
///
/// ⚠️ Ne pas confondre avec `derive_activity_label` (`state_engine.rs`), qui alimente le Notch :
/// là, le libellé complet est légitime — c'est l'écran de l'utilisateur. Ici, le texte part chez
/// un fournisseur de messagerie et y est stocké durablement. Or `derive_activity_label` produit
/// `"Reading C:\Users\<nom>\…\.env"` ou `"Running: git push --force <url de dépôt>"` : un chemin
/// absolu porte le nom d'utilisateur, l'arborescence des projets et parfois le nom d'un client ;
/// une commande shell porte des URL, des branches, parfois des arguments sensibles.
///
/// Règle : on garde le **verbe**, on réduit un chemin à son **nom de fichier** et une commande à
/// son **premier mot** (le binaire). « Editing config.ts », « Running: git ».
pub fn sanitize_summary(summary: &str) -> String {
    let Some((verb, rest)) = summary.split_once(' ') else {
        return summary.to_string();
    };
    let rest = rest.trim();
    match verb {
        // Commande shell : seul le binaire survit, jamais les arguments.
        "Running:" => match rest.split_whitespace().next() {
            Some(bin) => format!("Running: {}", basename(bin)),
            None => "Running a command".to_string(),
        },
        // Chemins : réduits au nom de fichier.
        "Reading" | "Editing" => format!("{verb} {}", basename(rest)),
        // `PostToolUseFailure` préfixe le libellé d'outil par « Failed: » : il faut assainir ce
        // qui suit, sinon un échec de Bash laisserait passer la commande entière.
        "Failed:" => format!("Failed: {}", sanitize_summary(rest)),
        // Libellés déjà sans contenu d'outil (« Error: … », « Using X », « Thinking… »,
        // messages de `Notification`) : inchangés.
        _ => summary.to_string(),
    }
}

/// Dernier segment d'un chemin, quel que soit le séparateur (les deux coexistent sous Windows).
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(path)
}

/// Corps du message, en **texte brut**.
///
/// Aucun `parse_mode` n'est utilisé par les canaux (cf. `telegram.rs`) : un résumé contient
/// naturellement des caractères de balisage (`_`, `*`, `` ` ``, `[`) qui feraient échouer un
/// parsing Markdown (erreur 400 → notification perdue) et ouvriraient une injection de balisage
/// depuis un simple nom de fichier. Texte brut = zéro échappement, zéro classe de bug.
///
/// **Contenu autorisé, exhaustivement** : libellé de l'agent, outil, type d'événement, résumé
/// d'action. Rien d'autre ne doit jamais être ajouté ici.
pub fn format_notif_text(notif: &MobileNotif) -> String {
    let label = flatten(&notif.agent_label, 60);
    let mut out = format!(
        "{}\n{} · {}",
        kind_line(notif.kind),
        if label.is_empty() { "Agent" } else { label.as_str() },
        tool_label(&notif.tool),
    );
    if let Some(summary) = &notif.summary {
        // Assainissement AVANT troncature : le message part chez un tiers (cf. `sanitize_summary`).
        let summary = flatten(&sanitize_summary(summary), SUMMARY_MAX_CHARS);
        if !summary.is_empty() {
            out.push('\n');
            out.push_str(&summary);
        }
    }
    // Story 8.4 — exception contenu bornée (question / options), déjà assainie à la construction.
    if let Some(extra) = &notif.body_extra {
        if !extra.is_empty() {
            out.push('\n');
            out.push_str(extra);
        }
    }
    out
}

/// Extrait le délai d'attente d'une erreur `rate_limited:{secs}`, borné à `MAX_RETRY_AFTER_SECS`.
/// Toute autre erreur rend `None` → **pas** de ré-essai (aucune file d'attente en 8.1).
/// Un délai absent ou illisible vaut 1 s (ré-essai immédiat mais pas instantané).
pub fn retry_after_secs(error: &str) -> Option<u64> {
    let rest = error.strip_prefix(RATE_LIMITED_PREFIX)?;
    let secs = rest.trim().parse::<u64>().unwrap_or(1);
    Some(secs.clamp(1, MAX_RETRY_AFTER_SECS))
}

// ── Lecture / écriture des réglages ──────────────────────────────────────────────

/// Relecture FRAÎCHE du store à chaque usage (patron `read_hook_settings`) : pas de cache, donc
/// un changement dans les Settings s'applique à la notification suivante sans redémarrage.
pub fn read_notif_settings(app: &AppHandle) -> NotifSettings {
    read_notif_settings_from(
        app.store(SETTINGS_STORE).ok().and_then(|s| s.get(NOTIF_SETTINGS_KEY)),
    )
}

/// Décodage **pur** des réglages, isolé pour être testable sans `AppHandle`.
///
/// `#[serde(default)]` ne couvre que les clés **absentes**. Une valeur de type inattendu
/// (`{"events":null}`, `{"chatId":123}`) fait échouer la désérialisation de l'objet ENTIER : un
/// `unwrap_or_default()` nu perdrait alors l'appairage en silence, et le prochain
/// `notify_set_settings` (lecture-modification-écriture) **persisterait** cette perte. On
/// récupère donc champ par champ, en ne sacrifiant que ce qui est réellement illisible.
fn read_notif_settings_from(raw: Option<serde_json::Value>) -> NotifSettings {
    let Some(raw) = raw else {
        return NotifSettings::default();
    };
    if let Ok(parsed) = serde_json::from_value::<NotifSettings>(raw.clone()) {
        return parsed;
    }
    let field = |k: &str| raw.get(k);
    NotifSettings {
        enabled: field("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        chat_id: field("chatId").and_then(|v| v.as_str()).map(str::to_string),
        bot_username: field("botUsername").and_then(|v| v.as_str()).map(str::to_string),
        events: field("events")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
    }
}

/// Clé de store portant les surcharges d'UN agent. **Une seule** définition, partagée par les
/// commandes (écriture) et le manager (lecture) : deux `format!` séparés, c'est deux vérités.
///
/// Même store et même convention que `agentRules:{transcript_path}` — la clé est le
/// `transcript_path`, jamais le nom de l'agent (qui n'est pas stable).
pub fn agent_notifs_key(transcript_path: &str) -> String {
    format!("agentNotifs:{transcript_path}")
}

/// Relecture FRAÎCHE des surcharges d'un agent (même patron que `read_notif_settings`).
pub fn read_agent_notif_overrides(app: &AppHandle, transcript_path: &str) -> AgentNotifOverrides {
    read_agent_notif_overrides_from(
        app.store(SETTINGS_STORE).ok().and_then(|s| s.get(agent_notifs_key(transcript_path))),
    )
}

/// Décodage **pur** des surcharges, isolé pour être testable sans `AppHandle`.
///
/// Décodage **clé par clé**, et non `from_value` en bloc (revue 8.2). La différence n'est pas
/// cosmétique : `HashMap<NotifKind, bool>` est tout-ou-rien, donc **une seule** clé illisible
/// effacerait TOUTES les surcharges de l'agent. Le repli « surcharges vides = héritage du global »
/// est bénin quand il ne change rien, mais pas quand il **ressuscite du bruit** : un agent qu'on
/// avait délibérément fait taire se remettrait à notifier le téléphone, silencieusement — soit
/// exactement le cas d'usage « je suis parti » que l'Epic 8 vise. Cas réel : rollback d'une MAJ
/// ayant introduit un 4ᵉ `NotifKind`, dont la clé devient inconnue de l'ancienne version.
///
/// On ignore donc ce qu'on ne comprend pas et on garde le reste. Un contenu qui n'est pas un objet
/// JSON (`42`, `"Waiting"`, `[]`, `null`) reste un repli complet sur l'héritage : il n'y a là rien
/// à récupérer.
pub fn read_agent_notif_overrides_from(raw: Option<serde_json::Value>) -> AgentNotifOverrides {
    let Some(serde_json::Value::Object(map)) = raw else {
        return AgentNotifOverrides::new();
    };
    map.into_iter()
        .filter_map(|(cle, valeur)| {
            let kind: NotifKind = serde_json::from_value(serde_json::Value::String(cle)).ok()?;
            Some((kind, valeur.as_bool()?))
        })
        .collect()
}

pub fn write_notif_settings(app: &AppHandle, settings: &NotifSettings) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    store.set(
        NOTIF_SETTINGS_KEY,
        serde_json::to_value(settings).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn notif(kind: NotifKind, summary: Option<&str>) -> MobileNotif {
        MobileNotif {
            kind,
            agent_label: "vigil".into(),
            tool: SourceTool::ClaudeCode,
            summary: summary.map(str::to_string),
            request_id: None,
            keyboard: None,
            body_extra: None,
        }
    }

    // ── Story 8.3 : outillage commun ─────────────────────────────────────────────

    /// Instantané d'agent minimal, paramétré par ce qui décide de l'actionnabilité.
    fn agent(state: &str, permission: Option<(&str, bool)>) -> AgentSnapshot {
        use crate::agent::state::PermissionRequestInfo;
        let mut a: AgentSnapshot = serde_json::from_value(serde_json::json!({
            "transcriptPath": "C:\\t.jsonl", "name": "vigil", "state": state,
            "lastActivity": 0, "source": "Terminal", "sourceTool": "claude_code",
        }))
        .unwrap();
        a.permission_request = permission.map(|(tool_name, read_only)| PermissionRequestInfo {
            tool_name: tool_name.into(),
            tool_input: serde_json::json!({}),
            read_only,
        });
        a
    }

    fn pending_with(request_id: &str, ttl: Duration) -> PendingRequests {
        let table = PendingRequests::default();
        table.insert(PendingRequest {
            request_id: request_id.into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: Instant::now() + ttl,
            message: Some(MessageRef::new("77")),
            text: "⏸ Waiting for you".into(),
            kind: PendingKind::ApproveDeny,
        });
        table
    }

    fn action(request_id: &str, kind: IncomingKind, sender_id: &str) -> ChannelAction {
        ChannelAction {
            request_id: request_id.into(),
            kind,
            sender_id: sender_id.into(),
            action_ref: ActionRef::new("cbq-1"),
        }
    }

    fn choice_questions(question: &str, options: &[&str], multi: bool) -> serde_json::Value {
        serde_json::json!([{
            "question": question,
            "options": options.iter().map(|l| serde_json::json!({ "label": l })).collect::<Vec<_>>(),
            "multiSelect": multi,
        }])
    }

    fn qd(question: &str, options: &[&str], multi: bool) -> QuestionData {
        QuestionData {
            multi,
            options: options.iter().map(|s| (*s).to_string()).collect(),
            header: None,
            question: question.into(),
        }
    }

    fn pending_choice(
        request_id: &str,
        ttl: Duration,
        multi: bool,
        options: &[&str],
    ) -> PendingRequests {
        pending_lot(
            request_id,
            ttl,
            &[qd("Pick one?", options, multi)],
            &choice_questions("Pick one?", options, multi),
        )
    }

    /// Fixture N questions (Story 8.9). `questions_json` est fourni explicitement pour que le JSON
    /// rejoué dans la réponse soit exactement celui de l'outil.
    fn pending_lot(
        request_id: &str,
        ttl: Duration,
        questions: &[QuestionData],
        questions_json: &serde_json::Value,
    ) -> PendingRequests {
        let table = PendingRequests::default();
        table.insert(PendingRequest {
            request_id: request_id.into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: Instant::now() + ttl,
            message: Some(MessageRef::new("77")),
            text: "⏸ Waiting for you\nvigil · Claude Code\nAsked you a question".into(),
            kind: PendingKind::Choice {
                questions: questions.to_vec(),
                selected: vec![BTreeSet::new(); questions.len()],
                questions_json: questions_json.clone(),
            },
        });
        table
    }

    /// Le vecteur canonique de l'AC1, écrit **à l'identique** dans le miroir Vitest
    /// (`src/components/detail/makeAskResponse.test.ts`, test `format_reponse_identique_au_poste_multi_questions`).
    /// ⚠️ Aucun libellé dupliqué : cf. la divergence documentée sur
    /// `make_ask_response_from_selections`.
    fn vecteur_croise_json() -> serde_json::Value {
        serde_json::json!([
          { "question": "Deploy?", "header": "Release",
            "options": [{ "label": "prod", "description": "live" }, { "label": "staging" }],
            "multiSelect": false },
          { "question": "Which checks?",
            "options": [{ "label": "lint" }, { "label": "tests" }, { "label": "types" }],
            "multiSelect": true },
          { "question": "Notes?", "options": [{ "label": "none" }], "multiSelect": false }
        ])
    }

    fn vecteur_croise_questions() -> Vec<QuestionData> {
        vec![
            QuestionData {
                multi: false,
                options: vec!["prod".into(), "staging".into()],
                header: Some("Release".into()),
                question: "Deploy?".into(),
            },
            qd("Which checks?", &["lint", "tests", "types"], true),
            qd("Notes?", &["none"], false),
        ]
    }

    const PAIRED: &str = "123456789";

    // ── AC2 : le secret ne peut structurellement pas atterrir dans la config claire ──

    #[test]
    fn token_stocke_hors_config_claire() {
        // Réglages COMPLETS (appairés) sérialisés tels qu'ils partent dans vigil-settings.json.
        let settings = NotifSettings {
            enabled: true,
            chat_id: Some("123456".into()),
            bot_username: Some("monbot".into()),
            events: NotifEvents::default(),
        };
        let json = serde_json::to_value(&settings).unwrap();
        let obj = json.as_object().unwrap();

        // Le jeu de clés est FIGÉ : aucune clé de secret ne peut apparaître sans casser ce test.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["botUsername", "chatId", "enabled", "events"]);

        // Et par sécurité, aucune trace textuelle d'un secret dans le document sérialisé.
        let dump = serde_json::to_string(&settings).unwrap().to_lowercase();
        assert!(!dump.contains("token"), "aucun champ de secret sérialisé : {dump}");
        assert!(!dump.contains("secret"), "aucun champ de secret sérialisé : {dump}");
    }

    // ── AC3 : table de transition ────────────────────────────────────────────────

    #[test]
    fn mapping_transition_vers_notif() {
        use AgentState::*;
        // Vers Ended / Error / Waiting* → notifié.
        assert_eq!(
            notif_kind_for_transition(Some(Working), Ended, false),
            Some(NotifKind::Ended)
        );
        assert_eq!(
            notif_kind_for_transition(Some(Working), Error, false),
            Some(NotifKind::Error)
        );
        assert_eq!(
            notif_kind_for_transition(Some(Working), WaitingApprove, false),
            Some(NotifKind::Waiting)
        );
        assert_eq!(
            notif_kind_for_transition(Some(Working), WaitingSpecific, false),
            Some(NotifKind::Waiting)
        );
        // Vers Working / Idle / Unknown → jamais.
        assert_eq!(notif_kind_for_transition(Some(Idle), Working, false), None);
        assert_eq!(notif_kind_for_transition(Some(Working), Idle, false), None);
        assert_eq!(notif_kind_for_transition(Some(Working), Unknown, false), None);
        // État de départ INCONNU (upsert direct) → notifie : c'est un vrai événement.
        assert_eq!(notif_kind_for_transition(None, Ended, false), Some(NotifKind::Ended));
        assert_eq!(notif_kind_for_transition(None, WaitingApprove, false), Some(NotifKind::Waiting));
        // WaitingApprove → WaitingSpecific est bien une transition (deux états distincts).
        assert_eq!(
            notif_kind_for_transition(Some(WaitingApprove), WaitingSpecific, false),
            Some(NotifKind::Waiting)
        );
    }

    #[test]
    fn disconnected_ne_notifie_jamais() {
        use AgentState::*;
        // Depuis n'importe quel état, y compris inconnu : disparaître n'est pas terminer.
        for prev in [None, Some(Working), Some(Ended), Some(WaitingApprove), Some(Error)] {
            assert_eq!(
                notif_kind_for_transition(prev.clone(), Disconnected, false),
                None,
                "prev={prev:?}"
            );
        }
    }

    #[test]
    fn meme_etat_ne_renotifie_pas() {
        use AgentState::*;
        // Deux `Stop` consécutifs → une seule notification.
        assert_eq!(notif_kind_for_transition(Some(Ended), Ended, false), None);
        assert_eq!(notif_kind_for_transition(Some(Error), Error, false), None);
        assert_eq!(
            notif_kind_for_transition(Some(WaitingApprove), WaitingApprove, false),
            None
        );
    }

    #[test]
    fn interrupt_ne_notifie_pas() {
        use AgentState::*;
        // Un Ended né d'un interrupt utilisateur : l'utilisateur était au clavier.
        assert_eq!(notif_kind_for_transition(Some(Working), Ended, true), None);
        assert_eq!(notif_kind_for_transition(None, Ended, true), None);
        // Le drapeau ne doit PAS étouffer les autres natures d'événement.
        assert_eq!(
            notif_kind_for_transition(Some(Working), Error, true),
            Some(NotifKind::Error)
        );
        assert_eq!(
            notif_kind_for_transition(Some(Working), WaitingApprove, true),
            Some(NotifKind::Waiting)
        );
    }

    // ── AC4 : filtrage + défauts + store hérité ──────────────────────────────────

    /// ⚠️ Story 8.2 — test 8.1 CONSERVÉ, à la seule signature près (`should_send` prend désormais
    /// les surcharges d'agent). Il est exercé avec des surcharges **vides**, ce qui en fait la
    /// preuve de **non-régression** : un agent sans surcharge se comporte exactement comme avant.
    #[test]
    fn filtrage_events_respecte_defaut() {
        let aucune = AgentNotifOverrides::new();

        // Défauts livrés : Waiting ON, Ended ON, Error OFF.
        let defaults = NotifEvents::default();
        assert!(defaults.waiting && defaults.ended && !defaults.error);

        let mut s = NotifSettings { enabled: true, ..Default::default() };
        assert!(should_send(NotifKind::Waiting, &s, &aucune));
        assert!(should_send(NotifKind::Ended, &s, &aucune));
        assert!(!should_send(NotifKind::Error, &s, &aucune));

        // L'interrupteur maître prime sur les cases.
        s.enabled = false;
        for kind in ALL_NOTIF_KINDS {
            assert!(!should_send(kind, &s, &aucune), "{kind:?} ne doit pas partir, maître OFF");
        }

        // Une case décochée coupe son seul événement.
        let s = NotifSettings {
            enabled: true,
            events: NotifEvents { waiting: false, ended: true, error: true },
            ..Default::default()
        };
        assert!(!should_send(NotifKind::Waiting, &s, &aucune));
        assert!(should_send(NotifKind::Ended, &s, &aucune));
        assert!(should_send(NotifKind::Error, &s, &aucune));
    }

    // ── Story 8.2 / AC1 : résolution à trois couches ─────────────────────────────

    fn overrides(pairs: &[(NotifKind, bool)]) -> AgentNotifOverrides {
        pairs.iter().copied().collect()
    }

    #[test]
    fn resolve_notif_utilise_override_agent_si_present() {
        // Global ON, surcharge d'agent OFF → l'agent gagne (et réciproquement).
        let global = NotifEvents { waiting: true, ended: true, error: false };
        let o = overrides(&[(NotifKind::Waiting, false), (NotifKind::Error, true)]);
        assert!(!resolve_notification(NotifKind::Waiting, &o, &global));
        assert!(resolve_notification(NotifKind::Error, &o, &global));
    }

    #[test]
    fn resolve_notif_fallback_global_si_absent() {
        // Clé ABSENTE = héritage, jamais « désactivé ».
        let o = AgentNotifOverrides::new();
        let global = NotifEvents { waiting: true, ended: false, error: true };
        assert!(resolve_notification(NotifKind::Waiting, &o, &global));
        assert!(!resolve_notification(NotifKind::Ended, &o, &global));
        assert!(resolve_notification(NotifKind::Error, &o, &global));
        // Une surcharge partielle ne « fige » pas les autres événements.
        let o = overrides(&[(NotifKind::Waiting, false)]);
        assert!(!resolve_notification(NotifKind::Waiting, &o, &global));
        assert!(!resolve_notification(NotifKind::Ended, &o, &global));
        assert!(resolve_notification(NotifKind::Error, &o, &global));
    }

    #[test]
    fn reset_retablit_heritage_global() {
        // « Reset » côté commande = SUPPRESSION de la clé de store, donc surcharges vides ici.
        let global = NotifEvents { waiting: true, ended: true, error: true };
        let coupe = overrides(&[
            (NotifKind::Waiting, false),
            (NotifKind::Ended, false),
            (NotifKind::Error, false),
        ]);
        for kind in ALL_NOTIF_KINDS {
            assert!(!resolve_notification(kind, &coupe, &global), "{kind:?} coupé");
        }
        let apres_reset = AgentNotifOverrides::new();
        for kind in ALL_NOTIF_KINDS {
            assert!(resolve_notification(kind, &apres_reset, &global), "{kind:?} de nouveau hérité");
        }
    }

    #[test]
    fn changement_global_impacte_agents_non_overrides() {
        // Deux agents : l'un surchargé, l'autre non. Le global bascule → seul le second suit,
        // et SANS qu'aucune clé `agentNotifs:` n'ait été réécrite.
        let surcharge = overrides(&[(NotifKind::Waiting, true)]);
        let hérité = AgentNotifOverrides::new();

        let avant = NotifEvents { waiting: true, ..NotifEvents::default() };
        assert!(resolve_notification(NotifKind::Waiting, &surcharge, &avant));
        assert!(resolve_notification(NotifKind::Waiting, &hérité, &avant));

        let après = NotifEvents { waiting: false, ..NotifEvents::default() };
        assert!(
            resolve_notification(NotifKind::Waiting, &surcharge, &après),
            "l'agent surchargé ne suit PAS le global"
        );
        assert!(
            !resolve_notification(NotifKind::Waiting, &hérité, &après),
            "l'agent non surchargé suit immédiatement le global"
        );
    }

    #[test]
    fn agent_mute_ne_declenche_aucun_envoi() {
        // Les 3 événements coupés pour CET agent : rien ne part, quel que soit le global.
        let s = NotifSettings {
            enabled: true,
            events: NotifEvents { waiting: true, ended: true, error: true },
            ..Default::default()
        };
        let mute: AgentNotifOverrides = ALL_NOTIF_KINDS.into_iter().map(|k| (k, false)).collect();
        for kind in ALL_NOTIF_KINDS {
            assert!(!should_send(kind, &s, &mute), "{kind:?} : agent muet");
        }
        // Un AUTRE agent (sans surcharge) continue de notifier normalement : la coupure est
        // strictement locale à la clé de store d'un agent.
        let autre = AgentNotifOverrides::new();
        for kind in ALL_NOTIF_KINDS {
            assert!(should_send(kind, &s, &autre), "{kind:?} : les autres agents notifient");
        }
    }

    #[test]
    fn interrupteur_maitre_ecrase_override_on() {
        // Surcharge « ON » explicite + interrupteur maître OFF → rien ne part. L'interrupteur
        // général reste AU-DESSUS de tout (décision verrouillée n°2).
        let s = NotifSettings {
            enabled: false,
            events: NotifEvents { waiting: true, ended: true, error: true },
            ..Default::default()
        };
        let tout_on: AgentNotifOverrides = ALL_NOTIF_KINDS.into_iter().map(|k| (k, true)).collect();
        for kind in ALL_NOTIF_KINDS {
            assert!(!should_send(kind, &s, &tout_on), "{kind:?} : maître OFF prime");
        }
    }

    #[test]
    fn override_off_sur_global_on() {
        // Le cas d'usage central de la story : couper UN événement d'UN agent bavard.
        let s = NotifSettings { enabled: true, ..Default::default() };
        let o = overrides(&[(NotifKind::Waiting, false)]);
        assert!(!should_send(NotifKind::Waiting, &s, &o), "l'agent bavard se tait");
        assert!(should_send(NotifKind::Ended, &s, &o), "ses autres événements passent encore");
    }

    // ── Story 8.2 / AC2 : persistance et repli ───────────────────────────────────

    #[test]
    fn overrides_corrompus_retombent_sur_heritage() {
        // Valeur d'un type inattendu, valeur non booléenne, clé inconnue, tableau, null :
        // TOUT retombe sur des surcharges vides = héritage du global. Jamais de panique, jamais
        // un silence total des notifications.
        for brut in [
            serde_json::json!(42),
            serde_json::json!("Waiting"),
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!({ "Waiting": 42 }),
            serde_json::json!({ "Waiting": "false" }),
            serde_json::json!({ "Inconnu": true }),
        ] {
            let o = read_agent_notif_overrides_from(Some(brut.clone()));
            assert!(o.is_empty(), "repli attendu sur l'héritage pour : {brut}");
            // Conséquence concrète : le comportement par défaut de la 8.1 est préservé.
            let s = NotifSettings { enabled: true, ..Default::default() };
            assert!(should_send(NotifKind::Ended, &s, &o));
        }
        // Absence totale de clé → héritage aussi.
        assert!(read_agent_notif_overrides_from(None).is_empty());
    }

    #[test]
    fn cle_illisible_nefface_pas_les_surcharges_voisines() {
        // ⚠️ Revue 8.2 — le décodage est clé par clé, PAS `from_value` en bloc. Une seule clé
        // inconnue ou mal typée ne doit pas emporter les autres : sinon un agent délibérément
        // rendu muet se remettrait à notifier le téléphone en silence (cas « je suis parti »).
        // Cas réel visé : rollback d'une MAJ ayant introduit un 4ᵉ NotifKind.
        let o = read_agent_notif_overrides_from(Some(serde_json::json!({
            "Waiting": false,
            "Digest": true,      // variante d'une version plus récente → inconnue ici
            "Ended": "oui",      // valeur mal typée
            "Error": true,
        })));
        assert_eq!(o.get(&NotifKind::Waiting), Some(&false), "la coupure survit");
        assert_eq!(o.get(&NotifKind::Error), Some(&true), "les voisines aussi");
        assert_eq!(o.len(), 2, "seules les entrées illisibles sont ignorées");

        // Conséquence concrète : l'agent reste muet sur Waiting malgré la clé parasite.
        let s = NotifSettings { enabled: true, ..Default::default() };
        assert!(!should_send(NotifKind::Waiting, &s, &o));
        // Et la ligne SANS surcharge lisible hérite toujours du global.
        assert!(should_send(NotifKind::Ended, &s, &o));
    }

    #[test]
    fn overrides_serialisent_en_cles_notifkind() {
        // Contrat de store ET miroir TS (`AgentNotifOverrides` dans src/types/notifications.ts) :
        // un objet JSON dont les clés sont les variantes de `NotifKind` telles quelles.
        let o = overrides(&[(NotifKind::Waiting, false), (NotifKind::Ended, true)]);
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json, serde_json::json!({ "Waiting": false, "Ended": true }));
        // Seules les clés EXPLICITEMENT réglées sont écrites (une clé absente = héritage).
        assert_eq!(json.as_object().unwrap().len(), 2);

        // Aller-retour : ce qui est écrit se relit à l'identique (survie au redémarrage).
        assert_eq!(read_agent_notif_overrides_from(Some(json)), o);
    }

    #[test]
    fn cle_de_store_est_indexee_sur_le_transcript_path() {
        // Convention identique à `agentRules:` — jamais le nom (qui n'est pas stable), et une
        // NOUVELLE session (autre transcript_path) a forcément une autre clé : zéro propagation.
        assert_eq!(agent_notifs_key("C:\\x\\a.jsonl"), "agentNotifs:C:\\x\\a.jsonl");
        assert_ne!(agent_notifs_key("a.jsonl"), agent_notifs_key("b.jsonl"));
    }

    #[test]
    fn set_all_couvre_toutes_les_variantes() {
        // `set_all_agent_notif_rules` est data-driven sur ALL_NOTIF_KINDS : cette constante doit
        // rester EXHAUSTIVE. Le `match` ci-dessous casse la compilation si une variante s'ajoute
        // sans être recensée — c'est le verrou.
        for kind in ALL_NOTIF_KINDS {
            match kind {
                NotifKind::Waiting | NotifKind::Ended | NotifKind::Error => {}
            }
        }
        assert_eq!(ALL_NOTIF_KINDS.len(), 3);
        let tout: AgentNotifOverrides = ALL_NOTIF_KINDS.into_iter().map(|k| (k, true)).collect();
        assert_eq!(tout.len(), ALL_NOTIF_KINDS.len(), "aucune variante en double");
        // « All off » : le global ne peut plus rien rattraper.
        let rien: AgentNotifOverrides = ALL_NOTIF_KINDS.into_iter().map(|k| (k, false)).collect();
        let global = NotifEvents { waiting: true, ended: true, error: true };
        for kind in ALL_NOTIF_KINDS {
            assert!(resolve_notification(kind, &tout, &NotifEvents::default()));
            assert!(!resolve_notification(kind, &rien, &global));
        }
    }

    #[test]
    fn events_get_est_le_miroir_des_champs() {
        // `NotifEvents::get` est le SEUL match kind → case globale : il doit rester aligné.
        let e = NotifEvents { waiting: true, ended: false, error: true };
        assert_eq!(e.get(NotifKind::Waiting), e.waiting);
        assert_eq!(e.get(NotifKind::Ended), e.ended);
        assert_eq!(e.get(NotifKind::Error), e.error);
    }

    #[test]
    fn settings_heritees_backfillees() {
        // Clé absente du store → tout par défaut (rien ne part).
        let vide: NotifSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(vide, NotifSettings::default());
        assert!(!vide.enabled);
        assert!(vide.events.waiting && vide.events.ended && !vide.events.error);

        // Objet `events` PARTIEL écrit par une version antérieure : la valeur présente est
        // conservée, les absentes reprennent le défaut (jamais `false` par accident).
        let partiel: NotifSettings =
            serde_json::from_str(r#"{"enabled":true,"events":{"error":true}}"#).unwrap();
        assert!(partiel.enabled);
        assert!(partiel.events.error, "valeur configurée conservée");
        assert!(partiel.events.waiting, "champ absent → défaut ON, pas false");
        assert!(partiel.events.ended, "champ absent → défaut ON, pas false");

        // Réglages appairés d'une version antérieure : l'appairage survit à l'ajout de champs.
        let appaire: NotifSettings =
            serde_json::from_str(r#"{"enabled":true,"chatId":"42","botUsername":"b"}"#).unwrap();
        assert_eq!(appaire.chat_id.as_deref(), Some("42"));
        assert_eq!(appaire.bot_username.as_deref(), Some("b"));
        assert_eq!(appaire.events, NotifEvents::default());
    }

    /// ⚠️ Revue 8.1 — `#[serde(default)]` ne couvre que les clés ABSENTES. Une valeur de type
    /// inattendu fait échouer l'objet entier : un `unwrap_or_default()` nu perdait alors
    /// l'appairage en silence, et le prochain enregistrement de réglages PERSISTAIT la perte.
    #[test]
    fn settings_corrompues_ne_perdent_pas_l_appairage() {
        // `events` de mauvais type : l'appairage doit survivre, seul `events` retombe au défaut.
        let s = read_notif_settings_from(Some(serde_json::json!({
            "enabled": true, "chatId": "42", "botUsername": "monbot", "events": null,
        })));
        assert_eq!(s.chat_id.as_deref(), Some("42"), "appairage préservé");
        assert_eq!(s.bot_username.as_deref(), Some("monbot"));
        assert!(s.enabled);
        assert_eq!(s.events, NotifEvents::default());

        // `enabled` de mauvais type : on ne devine pas, on retombe sur `false` (rien ne part),
        // mais l'appairage reste.
        let s = read_notif_settings_from(Some(serde_json::json!({
            "enabled": "oui", "chatId": "42",
        })));
        assert!(!s.enabled);
        assert_eq!(s.chat_id.as_deref(), Some("42"));

        // Un `chatId` numérique (écrit par une version fautive) n'est pas exploitable comme
        // chaîne : on préfère un appairage vide à un destinataire inventé.
        let s = read_notif_settings_from(Some(serde_json::json!({"chatId": 42})));
        assert_eq!(s.chat_id, None);

        // Absence totale de clé → défauts.
        assert_eq!(read_notif_settings_from(None), NotifSettings::default());
    }

    #[test]
    fn notif_kind_serialise_en_miroir_du_front() {
        // Contrat avec `src/types/notifications.ts` : toute dérive casse le filtrage côté UI.
        assert_eq!(serde_json::to_string(&NotifKind::Waiting).unwrap(), r#""Waiting""#);
        assert_eq!(serde_json::to_string(&NotifKind::Ended).unwrap(), r#""Ended""#);
        assert_eq!(serde_json::to_string(&NotifKind::Error).unwrap(), r#""Error""#);
    }

    // ── AC5 : contenu minimisé ───────────────────────────────────────────────────

    #[test]
    fn resume_tronque_a_200() {
        let long = "a".repeat(5_000);
        let text = format_notif_text(&notif(NotifKind::Ended, Some(&long)));
        let ligne_resume = text.lines().next_back().unwrap();
        assert_eq!(ligne_resume.chars().count(), SUMMARY_MAX_CHARS + 1, "200 caractères + ellipse");
        assert!(ligne_resume.ends_with('…'));

        // Un résumé exactement à la limite n'est PAS tronqué (pas d'ellipse parasite).
        let pile = "b".repeat(SUMMARY_MAX_CHARS);
        let text = format_notif_text(&notif(NotifKind::Ended, Some(&pile)));
        assert!(text.ends_with(&pile));
        assert!(!text.ends_with('…'));
    }

    #[test]
    fn texte_sans_tool_input() {
        // Le message ne porte QUE : type d'événement, libellé d'agent, outil, résumé.
        let n = MobileNotif {
            kind: NotifKind::Waiting,
            agent_label: "vigil".into(),
            tool: SourceTool::Grok,
            summary: Some("Bash: cargo test".into()),
            request_id: None,
            keyboard: None,
            body_extra: None,
        };
        let text = format_notif_text(&n);
        assert_eq!(text, "⏸ Waiting for you\nvigil · Grok Build\nBash: cargo test");
        // Aucun vecteur de fuite : la struct n'a même pas de champ pour ça, et le rendu tient
        // en 3 lignes vérifiées ci-dessus.
        assert_eq!(text.lines().count(), 3);
        for interdit in ["jsonl", "session", "C:\\", "tool_input", "tool_response"] {
            assert!(!text.contains(interdit), "fuite potentielle : {interdit}");
        }
    }

    /// ⚠️ Revue 8.1 — le test ci-dessus s'exerçait sur un libellé FABRIQUÉ, donc il ne prouvait
    /// rien : les vrais libellés viennent de `derive_activity_label` et portent des chemins
    /// absolus et des commandes shell complètes. Ici on part des formes réellement produites.
    #[test]
    fn resume_reel_ne_fuite_ni_chemin_ni_commande() {
        // Formes exactes rendues par `derive_activity_label` (state_engine.rs).
        let cas = [
            (
                "Reading C:\\Users\\Dev\\dev\\projets\\client-acme\\.env",
                "Reading .env",
            ),
            (
                "Editing C:\\Users\\Dev\\dev\\projets\\client-acme\\src\\secrets.ts",
                "Editing secrets.ts",
            ),
            (
                "Running: git push --force https://token@github.com/acme/prive.git main",
                "Running: git",
            ),
            (
                "Failed: Running: curl -H 'Authorization: Bearer sk-live-42' https://api.acme.io",
                "Failed: Running: curl",
            ),
            // Libellés déjà exempts de contenu d'outil : inchangés.
            ("Error: rate_limit", "Error: rate_limit"),
            ("Searching files...", "Searching files..."),
            ("Thinking…", "Thinking…"),
        ];
        for (brut, attendu) in cas {
            assert_eq!(sanitize_summary(brut), attendu, "entrée : {brut}");
        }

        // Bout en bout : rien de tout ça n'atteint le message.
        let text = format_notif_text(&notif(
            NotifKind::Ended,
            Some("Running: git push --force https://token@github.com/acme/prive.git main"),
        ));
        for interdit in ["token@", "github.com", "acme", "--force"] {
            assert!(!text.contains(interdit), "fuite : {interdit}");
        }
    }

    #[test]
    fn resume_gere_les_chemins_unix_et_les_formes_degenerees() {
        assert_eq!(sanitize_summary("Reading /home/steve/.ssh/id_rsa"), "Reading id_rsa");
        // Chemin se terminant par un séparateur → on ne rend pas une chaîne vide.
        assert_eq!(sanitize_summary("Editing C:\\dossier\\"), "Editing C:\\dossier\\");
        // Pas d'espace du tout → laissé tel quel (aucun verbe à isoler).
        assert_eq!(sanitize_summary("Thinking…"), "Thinking…");
        assert_eq!(sanitize_summary(""), "");
        // Commande vide après le verbe.
        assert_eq!(sanitize_summary("Running: "), "Running a command");
    }

    #[test]
    fn texte_resiste_a_un_label_pathologique() {
        // Libellé multi-ligne, bourré de caractères de balisage Markdown, très long : aucun
        // échappement n'est nécessaire (texte brut) et la mise en forme reste sur 3 lignes.
        let sale = "**_`[edit]`_**\n\trm -rf /\r\nligne 3 ";
        let n = MobileNotif {
            kind: NotifKind::Error,
            agent_label: "a\nb\tc".into(),
            tool: SourceTool::Unknown,
            summary: Some(sale.into()),
            request_id: None,
            keyboard: None,
            body_extra: None,
        };
        let text = format_notif_text(&n);
        assert_eq!(text.lines().count(), 3, "les sauts de ligne du contenu sont aplatis");
        assert_eq!(text, "⛔ Error\na b c · Agent\n**_`[edit]`_** rm -rf / ligne 3");

        // Résumé vide / uniquement blanc → pas de ligne fantôme.
        let n = MobileNotif { summary: Some("   \n  ".into()), ..n };
        assert_eq!(format_notif_text(&n).lines().count(), 2);
        let n = MobileNotif { summary: None, ..n };
        assert_eq!(format_notif_text(&n).lines().count(), 2);
    }

    #[test]
    fn label_agent_vide_a_un_repli() {
        let n = MobileNotif { agent_label: "  ".into(), ..notif(NotifKind::Ended, None) };
        assert_eq!(format_notif_text(&n), "✅ Finished\nAgent · Claude Code");
    }

    // ── AC6 : ré-essai borné ─────────────────────────────────────────────────────

    // ══ Story 8.3 : boucle retour ════════════════════════════════════════════════

    // ── AC1 : l'actionnabilité se déduit de la DEMANDE, jamais de l'outil ────────

    /// Test nommé par l'epic. Un outil dont la demande n'est pas approuvable depuis Vigil ne doit
    /// **jamais** produire de boutons — et ça se décide sur la demande, pas sur son nom.
    #[test]
    fn outil_sans_approve_pas_de_boutons() {
        // Antigravity : observe-only, n'émet AUCUN `PermissionRequest` → l'agent ne porte pas de
        // demande, quel que soit son état.
        let mut antigravity = agent("waitingApprove", None);
        antigravity.source_tool = SourceTool::Antigravity;
        assert!(!is_actionable(&antigravity), "aucune demande = aucun bouton");

        // Et un agent qui travaille, même porteur d'une vieille demande, n'est pas en attente.
        assert!(!is_actionable(&agent("working", Some(("Bash", false)))));
        assert!(!is_actionable(&agent("idle", Some(("Bash", false)))));
        assert!(!is_actionable(&agent("ended", None)));

        // Le cas nominal, lui, est bien actionnable.
        assert!(is_actionable(&agent("waitingApprove", Some(("Bash", false)))));
    }

    #[test]
    fn askuserquestion_pas_de_boutons_approve_deny() {
        // `AskUserQuestion` produit `WaitingSpecific` : ce n'est PAS une permission Approve/Deny
        // (8.3). Les boutons de choix relèvent de `is_choice_actionable` (8.4).
        let a = agent("waitingSpecific", Some(("AskUserQuestion", false)));
        assert!(!is_actionable(&a));
        assert!(!is_actionable(&agent("waitingSpecific", None)));
    }

    #[test]
    fn grok_read_only_pas_de_boutons() {
        // Grok en mode `default`/ask : le hook a DÉJÀ fait `exit 0`, approuver écrirait dans un
        // tuyau fermé. La condition porte sur `read_only`, PAS sur `source_tool` — la preuve :
        // le même drapeau donne le même verdict quel que soit l'outil.
        for tool in [SourceTool::Grok, SourceTool::ClaudeCode, SourceTool::Codex] {
            let mut lecture_seule = agent("waitingApprove", Some(("run_terminal_command", true)));
            lecture_seule.source_tool = tool.clone();
            assert!(!is_actionable(&lecture_seule), "{tool:?} read_only → notif-only");

            let mut actionnable = agent("waitingApprove", Some(("run_terminal_command", false)));
            actionnable.source_tool = tool.clone();
            assert!(is_actionable(&actionnable), "{tool:?} actionnable → boutons");
        }
    }

    // ── AC2 : corrélation par `request_id` ───────────────────────────────────────

    #[test]
    fn request_id_unique_entre_deux_runs() {
        // Unicité AU SEIN du run : deux appels ne collisionnent jamais.
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b);

        // Forme `{run}-{n}` : le préfixe de run est commun, le compteur avance.
        let (run_a, n_a) = a.rsplit_once('-').unwrap();
        let (run_b, n_b) = b.rsplit_once('-').unwrap();
        assert_eq!(run_a, run_b, "même run = même préfixe");
        assert!(n_b.parse::<u64>().unwrap() > n_a.parse::<u64>().unwrap());

        // Unicité ENTRE deux runs : c'est le point de ce test. Après un redémarrage, le compteur
        // repart de 0 — sans préfixe de run, le bouton d'un run précédent tomberait sur un `n`
        // réattribué et approuverait une AUTRE demande. Ici, un identifiant fabriqué avec un
        // préfixe de run différent ne peut désigner aucune entrée de la table courante.
        let table = pending_with(&a, Duration::from_secs(60));
        let ancien_run = format!("1-{n_a}");
        assert_ne!(ancien_run, a);
        assert!(table.take(&ancien_run).is_none(), "un bouton d'un run précédent ne résout rien");
        assert!(table.take(&a).is_some(), "la demande du run courant, elle, se résout");
    }

    #[test]
    fn purge_retire_les_demandes_perimees() {
        let table = PendingRequests::default();
        let vivante = PendingRequest {
            request_id: "vivante".into(),
            transcript_path: "C:\\a.jsonl".into(),
            expires_at: Instant::now() + Duration::from_secs(600),
            message: None,
            text: String::new(),
            kind: PendingKind::ApproveDeny,
        };
        let perimee = PendingRequest {
            request_id: "perimee".into(),
            transcript_path: "C:\\b.jsonl".into(),
            expires_at: Instant::now() - Duration::from_secs(1),
            message: Some(MessageRef::new("9")),
            text: String::new(),
            kind: PendingKind::ApproveDeny,
        };
        table.insert(vivante);
        table.insert(perimee);

        let sorties = table.purge_expired(Instant::now());
        assert_eq!(sorties.len(), 1);
        assert_eq!(sorties[0].request_id, "perimee");
        // La purge rend l'entrée pour que l'appelant la DÉSARME : sans sa référence de message,
        // le clavier resterait affiché sur un message mort.
        assert!(sorties[0].message.is_some());
        // La vivante est intacte, et la table n'est pas vide (la boucle continue de tourner).
        assert!(!table.is_empty());
        assert!(table.take("vivante").is_some());
        assert!(table.is_empty());
    }

    // ── AC3 / AC5 : réception, réinjection, anti-rejeu ───────────────────────────

    /// Test nommé par l'epic. Le chemin nominal : un tap sur ✅ (resp. ❌) résout la bonne
    /// demande et débloque l'agent par le MÊME appel qu'`approve_agent` / `deny_agent`.
    ///
    /// ⚠️ Exercice aussi `response_for_decision` / `outcome_for_phone` : une inversion
    /// Approve/Deny dans le `match` du manager casserait ces assertions (auparavant le test
    /// rejouait le chemin à la main sans jamais appeler ces mappings).
    #[test]
    fn callback_resout_request_id_et_approuve() {
        use crate::pipe::manager::PendingApprovals;
        use crate::pipe::protocol::{ALLOW_RESPONSE, DENY_RESPONSE};

        // Mapping décision → réponse / issue : c'est CE que handle_action injecte.
        assert_eq!(response_for_decision(ApprovalDecision::Approve), ALLOW_RESPONSE);
        assert_eq!(response_for_decision(ApprovalDecision::Deny), DENY_RESPONSE);
        assert_eq!(
            outcome_for_phone(ApprovalDecision::Approve),
            RequestOutcome::ApprovedFromPhone
        );
        assert_eq!(
            outcome_for_phone(ApprovalDecision::Deny),
            RequestOutcome::DeniedFromPhone
        );

        for (kind, decision, response) in [
            (IncomingKind::Approve, ApprovalDecision::Approve, ALLOW_RESPONSE),
            (IncomingKind::Deny, ApprovalDecision::Deny, DENY_RESPONSE),
        ] {
            let table = pending_with("run-7", Duration::from_secs(600));
            let a = action("run-7", kind, PAIRED);

            let ActionVerdict::Resolve(request, d) =
                classify_action(&table, Some(PAIRED), &a, Instant::now())
            else {
                panic!("un tap légitime doit être résolu");
            };
            assert_eq!(d, decision);
            assert_eq!(request.transcript_path, "C:\\t.jsonl");
            assert_eq!(response_for_decision(d), response);

            // Réinjection dans le pipeline EXISTANT — jamais une écriture directe dans le tuyau.
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let mut pa = PendingApprovals::new();
            pa.register(request.transcript_path.clone(), tx);
            assert!(pa.resolve(&request.transcript_path, response_for_decision(d).to_string()));
            assert_eq!(rx.blocking_recv().unwrap(), response);
            assert!(table.is_empty());
        }
    }

    /// Test nommé par l'epic. Une action venant d'ailleurs que du destinataire appairé est
    /// IGNORÉE — et surtout, elle ne consomme pas la demande de l'utilisateur légitime.
    #[test]
    fn chat_id_etranger_ignore() {
        let table = pending_with("run-7", Duration::from_secs(600));

        for etranger in ["987654321", "-1001234567890123", "", "12345678"] {
            let a = action("run-7", IncomingKind::Approve, etranger);
            assert!(
                matches!(classify_action(&table, Some(PAIRED), &a, Instant::now()), ActionVerdict::Ignored),
                "expéditeur {etranger} doit être ignoré"
            );
        }
        // Aucune consommation : le vrai destinataire peut toujours répondre.
        assert!(!table.is_empty());

        // Aucun destinataire appairé du tout → tout est étranger.
        let a = action("run-7", IncomingKind::Approve, PAIRED);
        assert!(matches!(classify_action(&table, None, &a, Instant::now()), ActionVerdict::Ignored));
        assert!(!table.is_empty());

        // Et le destinataire légitime, lui, passe.
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &a, Instant::now()),
            ActionVerdict::Resolve(..)
        ));
    }

    /// Test nommé par l'epic. Un tap tardif est **refusé, pas exécuté**.
    ///
    /// ⚠️ La preuve n'est PAS « resolve sur une table vide rend false » (tautologie). On laisse
    /// volontairement une demande vivante côté `PendingApprovals` et on vérifie qu'après le
    /// verdict `Expired` elle n'a **pas** été consommée — le chemin d'expiration n'appelle
    /// jamais `resolve`.
    #[test]
    fn tap_apres_expiration_refuse() {
        use crate::pipe::manager::PendingApprovals;
        use crate::pipe::protocol::ALLOW_RESPONSE;

        let table = pending_with("run-7", Duration::from_secs(600));
        let a = action("run-7", IncomingKind::Approve, PAIRED);

        // Côté pipe, la fenêtre n'est pas encore close : une demande attend toujours.
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let mut pa = PendingApprovals::new();
        pa.register("C:\\t.jsonl".into(), tx);

        // Le TTL de la PendingRequest est dépassé : on simule le futur plutôt que d'attendre.
        let plus_tard = Instant::now() + Duration::from_secs(601);
        let verdict = classify_action(&table, Some(PAIRED), &a, plus_tard);
        assert!(matches!(verdict, ActionVerdict::Expired(_)), "tap tardif = refus explicite");

        // AUCUNE approbation fabriquée : la demande pipe est toujours là et résolvable.
        assert!(
            pa.resolve("C:\\t.jsonl", ALLOW_RESPONSE.to_string()),
            "Expired ne doit PAS appeler resolve — la demande pipe reste intacte"
        );
        assert_eq!(rx.blocking_recv().unwrap(), ALLOW_RESPONSE);

        // Et l'entrée notify est bien consommée : pas de seconde chance.
        assert!(table.is_empty());
    }

    #[test]
    fn request_id_inconnu_rejete_avec_accuse() {
        // Identifiant jamais armé, ou d'un run précédent : rejeté. `Unknown` est précisément le
        // verdict qui fait rendre un accusé EXPLICATIF côté manager — jamais un silence, qui
        // laisserait tourner une roue ~30 s sur le téléphone.
        let table = pending_with("run-7", Duration::from_secs(600));
        for inconnu in ["run-8", "1-0", "", "run-7 "] {
            let a = action(inconnu, IncomingKind::Deny, PAIRED);
            assert!(
                matches!(classify_action(&table, Some(PAIRED), &a, Instant::now()), ActionVerdict::Unknown),
                "{inconnu} doit être rejeté"
            );
        }
        // La vraie demande n'a pas été touchée.
        assert!(!table.is_empty());
    }

    #[test]
    fn second_tap_apres_resolution_ne_refait_rien() {
        use crate::pipe::manager::PendingApprovals;
        use crate::pipe::protocol::ALLOW_RESPONSE;

        let table = pending_with("run-7", Duration::from_secs(600));
        let a = action("run-7", IncomingKind::Approve, PAIRED);

        // 1er tap : résolu.
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &a, Instant::now()),
            ActionVerdict::Resolve(..)
        ));

        // 2e tap : la consommation unique l'a rendu inerte AVANT toute I/O. C'est le garde-fou
        // n°1 — il tient même si le désarmement a échoué et que les boutons sont encore là.
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &a, Instant::now()),
            ActionVerdict::Unknown
        ));

        // Garde-fou n°2, INDÉPENDANT : `PendingApprovals::resolve` rend `false` une fois la
        // demande partie. Deux voies concurrentes ne peuvent pas produire deux décisions.
        let (tx, _rx) = tokio::sync::oneshot::channel::<String>();
        let mut pa = PendingApprovals::new();
        pa.register("C:\\t.jsonl".into(), tx);
        assert!(pa.resolve("C:\\t.jsonl", ALLOW_RESPONSE.to_string()));
        assert!(!pa.resolve("C:\\t.jsonl", ALLOW_RESPONSE.to_string()));
    }

    // ── AC7 : désarmement sur TOUTE résolution ───────────────────────────────────

    /// Test nommé par l'epic. Résoudre depuis le poste retire l'entrée ET rend de quoi éditer le
    /// message déjà reçu au téléphone.
    ///
    /// Les 5 sites de câblage de `request_resolved` (approve / deny / always_approve / auto-deny /
    /// éviction) se résument à un couple `(take_by_path, outcome)` — on exerce ici la table et
    /// **chaque** issue qu'ils peuvent produire, y compris `AlreadyHandled` (course téléphone).
    #[test]
    fn desarmement_apres_resolution() {
        let table = pending_with("run-7", Duration::from_secs(600));

        // Voie « poste » : la clé est le `transcript_path` (le poste ne connaît aucun request_id).
        let request = table.take_by_path("C:\\t.jsonl").expect("demande trouvée par agent");
        assert_eq!(request.message, Some(MessageRef::new("77")));
        assert!(table.is_empty(), "l'entrée est retirée immédiatement (anti-rejeu)");

        // Un second désarmement ne trouve plus rien : idempotent, aucun double envoi.
        assert!(table.take_by_path("C:\\t.jsonl").is_none());

        // Le message ré-édité porte l'issue ET son origine, sans rien ajouter d'autre.
        let corps = format!("{}\n{}", request.text, RequestOutcome::ApprovedAtDesk.suffix());
        assert_eq!(corps, "⏸ Waiting for you\n✅ Approved from Vigil");

        // Contrat des 5 sites de câblage + course téléphone (AlreadyHandled).
        for (outcome, attendu) in [
            (RequestOutcome::ApprovedFromPhone, "✅ Approved from your phone"),
            (RequestOutcome::DeniedFromPhone, "❌ Denied from your phone"),
            (RequestOutcome::ApprovedAtDesk, "✅ Approved from Vigil"), // approve_agent
            (RequestOutcome::DeniedAtDesk, "❌ Denied from Vigil"),     // deny_agent
            (RequestOutcome::AnsweredFromPhone, "Answered from your phone"),
            (RequestOutcome::AnsweredAtDesk, "Answered from Vigil"), // send_message
            (RequestOutcome::HandledInTerminal, "Handled in the terminal"), // résolue au terminal
            (RequestOutcome::Expired, "⏱ Expired — nothing was executed"), // auto-deny
            (RequestOutcome::Superseded, "↩︎ Replaced by a newer request"), // éviction register
            (RequestOutcome::AlreadyHandled, "Already handled"), // resolve == false côté téléphone
        ] {
            assert_eq!(outcome.suffix(), attendu);
        }
    }

    /// `take_by_path` départage sur `expires_at` : la demande **vivante** (TTL le plus tardif)
    /// est retirée, pas une entrée morte laissée par une course d'éviction.
    #[test]
    fn take_by_path_prefere_la_demande_vivante() {
        let table = PendingRequests::default();
        let now = Instant::now();
        table.insert(PendingRequest {
            request_id: "morte".into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: now + Duration::from_secs(60),
            message: Some(MessageRef::new("1")),
            text: "old".into(),
            kind: PendingKind::ApproveDeny,
        });
        table.insert(PendingRequest {
            request_id: "vivante".into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: now + Duration::from_secs(1500),
            message: Some(MessageRef::new("2")),
            text: "new".into(),
            kind: PendingKind::ApproveDeny,
        });
        // Autre agent : ne doit pas être touché.
        table.insert(PendingRequest {
            request_id: "autre".into(),
            transcript_path: "C:\\autre.jsonl".into(),
            expires_at: now + Duration::from_secs(900),
            message: None,
            text: String::new(),
            kind: PendingKind::ApproveDeny,
        });

        let prise = table.take_by_path("C:\\t.jsonl").expect("vivante");
        assert_eq!(prise.request_id, "vivante");
        assert_eq!(prise.message, Some(MessageRef::new("2")));
        // La morte reste — un second appel (éviction) pourra la retirer ensuite.
        assert_eq!(table.take_by_path("C:\\t.jsonl").unwrap().request_id, "morte");
        assert!(table.take("autre").is_some());
    }

    #[test]
    fn message_ref_attachee_apres_envoi_et_retiree_sur_echec() {
        // « Piège n°1 » : l'entrée est créée AVANT l'envoi, donc sans référence de message.
        let table = PendingRequests::default();
        table.insert(PendingRequest {
            request_id: "run-1".into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: Instant::now() + Duration::from_secs(60),
            message: None,
            text: "⏸ Waiting for you".into(),
            kind: PendingKind::ApproveDeny,
        });
        table.attach_message("run-1", MessageRef::new("42"));
        assert_eq!(table.take("run-1").unwrap().message, Some(MessageRef::new("42")));

        // Attacher à une demande déjà consommée (tap plus rapide que la réponse HTTP) ne
        // ressuscite rien : la table reste vide.
        table.attach_message("run-1", MessageRef::new("42"));
        assert!(table.is_empty());
    }

    // ── AC5 : le contenu du message est INCHANGÉ par rapport à la 8.1 ────────────

    #[test]
    fn boutons_najoutent_rien_au_contenu_du_message() {
        // Le `request_id` ne doit apparaître NULLE PART dans le texte : c'est une donnée de
        // corrélation, pas du contenu. Le rendu doit être byte-identique avec et sans boutons.
        let sans = notif(NotifKind::Waiting, Some("Wants to run: Bash"));
        let avec = MobileNotif { request_id: Some("1753000000000-3".into()), ..sans.clone() };
        assert_eq!(format_notif_text(&avec), format_notif_text(&sans));
        assert!(!format_notif_text(&avec).contains("1753000000000"));
        assert_eq!(format_notif_text(&avec).lines().count(), 3);
    }

    #[test]
    fn retry_after_borne() {
        assert_eq!(retry_after_secs("rate_limited:5"), Some(5));
        // Borné à 30 s : un canal qui demande 10 minutes ne doit pas immobiliser la boucle.
        assert_eq!(retry_after_secs("rate_limited:600"), Some(MAX_RETRY_AFTER_SECS));
        // Délai illisible ou absent → 1 s (on réessaie, mais jamais instantanément).
        assert_eq!(retry_after_secs("rate_limited:"), Some(1));
        assert_eq!(retry_after_secs("rate_limited:abc"), Some(1));
        assert_eq!(retry_after_secs("rate_limited:0"), Some(1));
        // Tout autre échec n'est PAS retenté (pas de file d'attente en 8.1).
        assert_eq!(retry_after_secs("invalid_token"), None);
        assert_eq!(retry_after_secs("network_error: timeout"), None);
        assert_eq!(retry_after_secs("api_error_500"), None);
    }

    // ══ Story 8.4 : questions à choix ════════════════════════════════════════════

    fn agent_choice(
        questions: serde_json::Value,
        read_only: bool,
    ) -> AgentSnapshot {
        use crate::agent::state::PermissionRequestInfo;
        let mut a = agent("waitingSpecific", Some(("AskUserQuestion", read_only)));
        a.permission_request = Some(PermissionRequestInfo {
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({ "questions": questions }),
            read_only,
        });
        a
    }

    /// Test nommé par l'epic. Mono-choix : un tap répond, consomme, format poste.
    #[test]
    fn choix_unique_repond_et_desarme() {
        use crate::pipe::manager::PendingApprovals;

        let table = pending_choice("run-c", Duration::from_secs(600), false, &["Alpha", "Beta"]);
        let a = action("run-c", IncomingKind::Choose { question: 0, index: 1 }, PAIRED);
        let ActionVerdict::ResolveChoice(request, response) =
            classify_action(&table, Some(PAIRED), &a, Instant::now())
        else {
            panic!("mono-choix doit résoudre");
        };
        assert!(table.is_empty(), "take() a consommé la demande");
        assert_eq!(request.message, Some(MessageRef::new("77")));

        let expected = make_ask_response_from_selections(
            &choice_questions("Pick one?", &["Alpha", "Beta"], false),
            &[qd("Pick one?", &["Alpha", "Beta"], false)],
            &[BTreeSet::from([1])],
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            serde_json::from_str::<serde_json::Value>(&expected).unwrap()
        );

        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let mut pa = PendingApprovals::new();
        pa.register(request.transcript_path.clone(), tx);
        assert!(pa.resolve(&request.transcript_path, response));
        let got = rx.blocking_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&got).unwrap()["hookSpecificOutput"]
                ["decision"]["updatedInput"]["answers"]["Pick one?"],
            "Beta"
        );
    }

    /// Test nommé par l'epic. Multi : bascule sans take.
    #[test]
    fn multi_choix_bascule_sans_consommer() {
        let table = pending_choice("run-m", Duration::from_secs(600), true, &["A", "B", "C"]);
        let a = action("run-m", IncomingKind::Choose { question: 0, index: 0 }, PAIRED);
        let ActionVerdict::Toggle { keyboard, message } =
            classify_action(&table, Some(PAIRED), &a, Instant::now())
        else {
            panic!("bascule multi attendue");
        };
        assert!(!table.is_empty(), "pas de take sur bascule");
        assert_eq!(message, Some(MessageRef::new("77")));
        match keyboard {
            ActionKeyboard::Choice { selected, .. } => {
                assert!(selected[0].contains(&0));
                assert_eq!(selected[0].len(), 1);
            }
            _ => panic!("clavier Choice attendu"),
        }
        // Seconde bascule sur la même option → décocher.
        let ActionVerdict::Toggle { keyboard, .. } =
            classify_action(&table, Some(PAIRED), &a, Instant::now())
        else {
            panic!("2e bascule");
        };
        match keyboard {
            ActionKeyboard::Choice { selected, .. } => assert!(selected[0].is_empty()),
            _ => panic!(),
        }
        assert!(!table.is_empty());
    }

    /// Test nommé par l'epic.
    #[test]
    fn seul_send_consomme_la_demande() {
        let table = pending_choice("run-m", Duration::from_secs(600), true, &["A", "B"]);
        // Bascule A et B.
        let _ = classify_action(
            &table,
            Some(PAIRED),
            &action("run-m", IncomingKind::Choose { question: 0, index: 0 }, PAIRED),
            Instant::now(),
        );
        let _ = classify_action(
            &table,
            Some(PAIRED),
            &action("run-m", IncomingKind::Choose { question: 0, index: 1 }, PAIRED),
            Instant::now(),
        );
        assert!(!table.is_empty());
        let ActionVerdict::ResolveChoice(request, response) = classify_action(
            &table,
            Some(PAIRED),
            &action("run-m", IncomingKind::Confirm, PAIRED),
            Instant::now(),
        ) else {
            panic!("Send doit résoudre");
        };
        assert!(table.is_empty());
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Pick one?"],
            "A, B"
        );
        assert_eq!(request.request_id, "run-m");
    }

    /// Test nommé par l'epic.
    #[test]
    fn send_sans_selection_refuse() {
        let table = pending_choice("run-m", Duration::from_secs(600), true, &["A", "B"]);
        let v = classify_action(
            &table,
            Some(PAIRED),
            &action("run-m", IncomingKind::Confirm, PAIRED),
            Instant::now(),
        );
        assert!(matches!(v, ActionVerdict::EmptySelection));
        assert!(!table.is_empty(), "pas de take si sélection vide");
    }

    /// Story 8.9 — ex-`plusieurs_questions_pas_de_boutons`, assertion **inversée** : un lot
    /// multi-questions est désormais armé. C'est le verrou de l'AC8 : `is_mobile_actionable`
    /// devient vrai **sans** qu'aucune ligne de `approval_window_secs` n'ait été touchée, donc la
    /// fenêtre passe de 600 s à 1500 s gratuitement.
    #[test]
    fn plusieurs_questions_armees() {
        let questions = serde_json::json!([
            { "question": "Q1?", "options": [{ "label": "a" }], "multiSelect": false },
            { "question": "Q2?", "options": [{ "label": "b" }], "multiSelect": true },
            { "question": "Q3?", "options": [{ "label": "c" }], "multiSelect": false },
        ]);
        let a = agent_choice(questions, false);
        assert!(is_choice_actionable(&a));
        assert!(is_mobile_actionable(&a));
        let data = choice_from_agent(&a).expect("lot armable");
        assert_eq!(data.questions.len(), 3);
        assert!(!data.questions[0].multi);
        assert!(data.questions[1].multi);
        assert_eq!(data.questions[2].options, vec!["c".to_string()]);
        assert!(needs_confirm(&data.questions), "N > 1 exige un Send");
    }

    /// Story 8.9 — décision 6 : UNE question sans option exploitable fait tomber TOUT le lot.
    #[test]
    fn un_lot_avec_une_question_sans_option_nest_pas_arme() {
        // Q2 en texte libre pur → notif-only pour les trois.
        let questions = serde_json::json!([
            { "question": "Q1?", "options": [{ "label": "a" }], "multiSelect": false },
            { "question": "Libre ?", "options": [], "multiSelect": false },
            { "question": "Q3?", "options": [{ "label": "c" }], "multiSelect": false },
        ]);
        let a = agent_choice(questions, false);
        assert!(!is_choice_actionable(&a), "tout le lot tombe, pas seulement Q2");
        assert!(choice_from_agent(&a).is_none());

        // Même chose avec des labels vides uniquement (règle 8.4 inchangée, appliquée par question).
        let questions = serde_json::json!([
            { "question": "Q1?", "options": [{ "label": "a" }], "multiSelect": false },
            { "question": "Q2?", "options": [{ "label": "   " }], "multiSelect": false },
        ]);
        assert!(!is_choice_actionable(&agent_choice(questions, false)));

        // ...et le lot reste armé si TOUTES les questions ont une option exploitable.
        let questions = serde_json::json!([
            { "question": "Q1?", "options": [{ "label": "a" }], "multiSelect": false },
            { "question": "Q2?", "options": [{ "label": "b" }], "multiSelect": false },
        ]);
        assert!(is_choice_actionable(&agent_choice(questions, false)));
    }

    /// Story 8.9 — plafonds d'adressage : l'indice de question ET l'indice d'option se décodent en
    /// `u8`. Au-delà, on dégrade en notif-only plutôt que d'armer un clavier partiellement mort.
    #[test]
    fn au_dela_des_plafonds_le_lot_nest_pas_arme() {
        let une = |i: usize| {
            serde_json::json!({
                "question": format!("Q{i}?"),
                "options": [{ "label": "a" }],
                "multiSelect": false
            })
        };
        // Un lot d'une option par question est borné par le TOTAL de boutons, pas par le nombre de
        // questions : `MAX_TOTAL_OPTIONS` (100) mord avant `MAX_QUESTIONS` (256).
        let pile: Vec<_> = (0..MAX_TOTAL_OPTIONS).map(une).collect();
        assert!(
            is_choice_actionable(&agent_choice(serde_json::Value::Array(pile), false)),
            "{MAX_TOTAL_OPTIONS} boutons tiennent dans un clavier"
        );
        let trop: Vec<_> = (0..=MAX_TOTAL_OPTIONS).map(une).collect();
        assert!(!is_choice_actionable(&agent_choice(
            serde_json::Value::Array(trop),
            false
        )));

        // ⚠️ Revue 8.9 — le plafond du TOTAL n'est impliqué par AUCUN des deux plafonds unitaires :
        // 4 questions × 30 options passent `MAX_QUESTIONS` et `MAX_CHOICE_OPTIONS` sans effort,
        // mais font 120 boutons. Sans cette garde, la demande serait armée, `is_mobile_actionable`
        // vrai, le `sendMessage` rejeté par Telegram — et AUCUNE notification ne partirait pendant
        // que l'agent attend 1500 s. C'est l'assertion qui verrouille le correctif.
        let large = |i: usize| {
            let options: Vec<_> = (0..30)
                .map(|o| serde_json::json!({ "label": format!("o{i}-{o}") }))
                .collect();
            serde_json::json!({ "question": format!("Q{i}?"), "options": options, "multiSelect": false })
        };
        let gros: Vec<_> = (0..4).map(large).collect();
        assert!(
            !is_choice_actionable(&agent_choice(serde_json::Value::Array(gros), false)),
            "4 × 30 = 120 boutons : sous les deux plafonds unitaires, au-dessus du total"
        );

        // Tableau vide : rien à armer (règle 8.4 conservée).
        assert!(!is_choice_actionable(&agent_choice(serde_json::json!([]), false)));

        // Trop d'options sur UNE question du lot → tout le lot tombe.
        let options: Vec<_> = (0..=MAX_CHOICE_OPTIONS)
            .map(|i| serde_json::json!({ "label": format!("o{i}") }))
            .collect();
        let questions = serde_json::json!([
            { "question": "Q1?", "options": [{ "label": "a" }], "multiSelect": false },
            { "question": "Q2?", "options": options, "multiSelect": false },
        ]);
        assert!(!is_choice_actionable(&agent_choice(questions, false)));
    }

    /// Story 8.9 — le pivot des deux comportements (décision 9), exercé seul.
    #[test]
    fn needs_confirm_pivote_sur_une_seule_question_mono() {
        assert!(!needs_confirm(&[qd("Q?", &["a", "b"], false)]), "1 mono → un tap répond");
        assert!(needs_confirm(&[qd("Q?", &["a", "b"], true)]), "1 multi → Send");
        assert!(
            needs_confirm(&[qd("Q1?", &["a"], false), qd("Q2?", &["b"], false)]),
            "N mono → Send quand même (envoi partiel interdit)"
        );
        assert!(!needs_confirm(&[]), "lot vide : fonction totale, jamais armée");
    }

    /// Test nommé par l'epic.
    #[test]
    fn question_sans_option_pas_de_boutons() {
        let questions = serde_json::json!([
            { "question": "Libre ?", "options": [], "multiSelect": false },
        ]);
        assert!(!is_choice_actionable(&agent_choice(questions, false)));
        // Labels vides uniquement.
        let questions = serde_json::json!([
            { "question": "?", "options": [{ "label": "  " }], "multiSelect": false },
        ]);
        assert!(!is_choice_actionable(&agent_choice(questions, false)));
    }

    #[test]
    fn question_read_only_pas_de_boutons() {
        let questions = choice_questions("OK?", &["yes", "no"], false);
        // Grok / toute demande non injectable — zéro match source_tool.
        for tool in [SourceTool::Grok, SourceTool::ClaudeCode, SourceTool::Codex] {
            let mut a = agent_choice(questions.clone(), true);
            a.source_tool = tool.clone();
            assert!(!is_choice_actionable(&a), "{tool:?} read_only");
            assert!(!is_mobile_actionable(&a));
            let mut ok = agent_choice(questions.clone(), false);
            ok.source_tool = tool;
            assert!(is_choice_actionable(&ok));
            assert!(is_mobile_actionable(&ok));
        }
    }

    /// Test nommé par l'epic.
    #[test]
    fn format_reponse_identique_au_poste() {
        let questions = serde_json::json!([{
            "question": "Deploy?",
            "header": "Release",
            "options": [
                { "label": "prod", "description": "live" },
                { "label": "staging" }
            ],
            "multiSelect": true
        }]);
        // Miroir exact de makeAskResponse TS + handleSendSelection (join ", ").
        let answers = serde_json::json!({ "Deploy?": "prod, staging" });
        let rust = make_ask_response(&questions, &answers);
        let expected = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "allow",
                    "updatedInput": { "questions": questions, "answers": answers }
                }
            }
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&rust).unwrap(),
            expected
        );
        // Via sélection d'options (chemin mobile).
        let via_sel = make_ask_response_from_selections(
            &serde_json::json!([{
                "question": "Deploy?",
                "options": [{ "label": "prod" }, { "label": "staging" }],
                "multiSelect": true
            }]),
            &[qd("Deploy?", &["prod", "staging"], true)],
            &[BTreeSet::from([0, 1])],
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&via_sel).unwrap()["hookSpecificOutput"]
                ["decision"]["updatedInput"]["answers"],
            answers
        );
    }

    /// ⚠️ **AC n°1 — vecteur croisé Rust ↔ Vitest.** Le même littéral d'entrée et le même JSON
    /// attendu sont écrits des deux côtés : ici, et dans
    /// `src/components/detail/makeAskResponse.test.ts` (test de même nom), où la map `answers` est
    /// construite en appelant le **vrai** `buildAnswersMap` du poste.
    ///
    /// Deux propriétés que ce vecteur prouve :
    /// 1. `"lint, types"` et non `"types, lint"` — l'ordre est celui des **options** ;
    /// 2. la clé de chaque question est son **texte**.
    ///
    /// ⚠️ Portée exacte de la propriété 1 **ici** : le `BTreeSet` d'entrée a déjà effacé l'ordre
    /// de tap, donc ce golden ne peut pas, à lui seul, prouver que l'ordre des **taps** est
    /// ignoré — il verrouille la sortie de la fonction, pas le chemin qui l'alimente. Cette
    /// propriété-là est prouvée ailleurs, pour de vrai : `send_complet_consomme_une_seule_fois`
    /// tape `(1,2)` puis `(1,0)` via `apply_choose` et attend `"lint, types"`, et le miroir Vitest
    /// passe `selectedLabels: ["types", "lint"]` au **vrai** `buildAnswersMap`, où le filtre sur
    /// `question.options` peut authentiquement échouer (revue 8.9).
    ///
    /// Et la seconde assertion prouve la troisième : une question laissée vide est **absente** de
    /// la map, pas présente avec une chaîne vide — c'est ce qui rend `isComplete` (poste)
    /// équivalent à la garde d'`IncompleteSelection` (mobile).
    #[test]
    fn format_reponse_identique_au_poste_multi_questions() {
        let questions_json = vecteur_croise_json();
        let questions = vecteur_croise_questions();

        // Q0 → {1} ; Q1 → taps `types` puis `lint` ⇒ {0, 2} ; Q2 → {0}.
        let selected = vec![
            BTreeSet::from([1]),
            BTreeSet::from([2, 0]),
            BTreeSet::from([0]),
        ];
        let rust = make_ask_response_from_selections(&questions_json, &questions, &selected);
        let parsed: serde_json::Value = serde_json::from_str(&rust).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"],
            serde_json::json!({
                "Deploy?": "staging",
                "Which checks?": "lint, types",
                "Notes?": "none"
            })
        );
        // Enveloppe complète + `questions` réémis TELS QUELS.
        assert_eq!(
            parsed,
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "allow",
                        "updatedInput": {
                            "questions": questions_json,
                            "answers": {
                                "Deploy?": "staging",
                                "Which checks?": "lint, types",
                                "Notes?": "none"
                            }
                        }
                    }
                }
            })
        );

        // Q2 laissée VIDE → la clé "Notes?" est ABSENTE (pas une chaîne vide).
        let partiel = vec![
            BTreeSet::from([1]),
            BTreeSet::from([2, 0]),
            BTreeSet::new(),
        ];
        let parsed: serde_json::Value =
            serde_json::from_str(&make_ask_response_from_selections(
                &questions_json,
                &questions,
                &partiel,
            ))
            .unwrap();
        let answers = &parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"];
        assert_eq!(
            answers,
            &serde_json::json!({ "Deploy?": "staging", "Which checks?": "lint, types" })
        );
        assert!(
            answers.get("Notes?").is_none(),
            "une question sans réponse est ABSENTE de la map, jamais une chaîne vide"
        );
    }

    /// Story 8.9 — deux questions au **texte identique** : la dernière écrase, miroir exact de
    /// l'affectation TS `answers[q.question] = value`. Collision assumée, héritée du contrat de
    /// l'outil — documentée pour qu'elle ne se découvre pas en revue.
    #[test]
    fn deux_questions_de_meme_texte_la_derniere_ecrase() {
        let questions = vec![qd("Same?", &["a"], false), qd("Same?", &["b"], false)];
        let json = make_ask_response_from_selections(
            &serde_json::json!([]),
            &questions,
            &[BTreeSet::from([0]), BTreeSet::from([0])],
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"],
            serde_json::json!({ "Same?": "b" })
        );
    }

    #[test]
    fn resume_question_est_honnete() {
        // Exercé aussi dans pipe/manager — ici le corps + résumé mobile.
        // ⚠️ Story 8.9 — N == 1 : le rendu doit rester STRICTEMENT celui de la 8.4 (aucun numéro).
        let body = format_choice_body(&[QuestionData {
            multi: false,
            options: vec!["prod".into(), "staging".into()],
            header: Some("Release".into()),
            question: "Deploy now?".into(),
        }]);
        // Égalité STRICTE : les `contains` de la 8.4 qui suivaient ne pouvaient plus échouer sans
        // que cette ligne n'ait déjà échoué (revue 8.9) — retirés.
        assert_eq!(body, "Release\nDeploy now?\n• prod\n• staging");
        let n = MobileNotif {
            kind: NotifKind::Waiting,
            agent_label: "vigil".into(),
            tool: SourceTool::ClaudeCode,
            summary: Some("Asked you a question".into()),
            request_id: Some("1-0".into()),
            keyboard: Some(ActionKeyboard::Choice {
                request_id: "1-0".into(),
                questions: vec![qd("Deploy now?", &["prod"], false)],
                selected: vec![BTreeSet::new()],
            }),
            body_extra: Some(body),
        };
        let text = format_notif_text(&n);
        assert!(text.contains("Asked you a question"));
        assert!(!text.contains("Wants to run: AskUserQuestion"));
        assert!(!text.contains("1-0"), "request_id hors du texte");
        assert!(text.contains("• prod"));
    }

    // ══ Story 8.9 : lot de N questions ═══════════════════════════════════════════

    /// Fixture du lot canonique (Q0 mono, Q1 multi, Q2 mono) déjà armé dans une table.
    fn table_lot(request_id: &str) -> PendingRequests {
        pending_lot(
            request_id,
            Duration::from_secs(600),
            &vecteur_croise_questions(),
            &vecteur_croise_json(),
        )
    }

    fn selection_du_clavier(v: ActionVerdict) -> Vec<BTreeSet<usize>> {
        let ActionVerdict::Toggle { keyboard, .. } = v else {
            panic!("Toggle attendu, obtenu {v:?}");
        };
        let ActionKeyboard::Choice { selected, .. } = keyboard else {
            panic!("clavier Choice attendu");
        };
        selected
    }

    /// AC3 — question mono : la sélection se DÉPLACE, jamais deux sélections, aucun `take()`.
    #[test]
    fn mono_deplace_la_selection_sans_consommer() {
        let table = table_lot("run-lot");
        let sel = selection_du_clavier(classify_action(
            &table,
            Some(PAIRED),
            &action("run-lot", IncomingKind::Choose { question: 0, index: 0 }, PAIRED),
            Instant::now(),
        ));
        assert_eq!(sel[0], BTreeSet::from([0]));
        assert!(!table.is_empty(), "aucun take() sur une sélection");

        // Une AUTRE option de la MÊME question → la sélection se déplace.
        let sel = selection_du_clavier(classify_action(
            &table,
            Some(PAIRED),
            &action("run-lot", IncomingKind::Choose { question: 0, index: 1 }, PAIRED),
            Instant::now(),
        ));
        assert_eq!(sel[0], BTreeSet::from([1]), "la sélection se DÉPLACE");
        assert_eq!(sel[0].len(), 1, "jamais deux sélections sur une question mono");
        assert!(!table.is_empty());
    }

    /// AC3 — re-tap sur l'option déjà sélectionnée d'une question mono : verdict `AlreadySelected`
    /// (ack seul, **aucun** `update_actions` — piège n°3), état inchangé, rien de consommé.
    ///
    /// ⚠️ Ce test échoue si `AlreadySelected` est remplacé par `Toggle` : c'est précisément sa
    /// raison d'être (vérifié en phase RED).
    #[test]
    fn mono_retap_ne_change_rien_mais_accuse() {
        let table = table_lot("run-lot");
        let tap = action("run-lot", IncomingKind::Choose { question: 0, index: 1 }, PAIRED);
        let sel = selection_du_clavier(classify_action(&table, Some(PAIRED), &tap, Instant::now()));
        assert_eq!(sel[0], BTreeSet::from([1]));

        let v = classify_action(&table, Some(PAIRED), &tap, Instant::now());
        assert!(
            matches!(v, ActionVerdict::AlreadySelected),
            "re-tap mono → AlreadySelected (ack seul), obtenu {v:?}"
        );
        assert!(!table.is_empty(), "aucun take()");

        // L'état n'a pas bougé : un tap sur une AUTRE option repart bien de {1}.
        let sel = selection_du_clavier(classify_action(
            &table,
            Some(PAIRED),
            &action("run-lot", IncomingKind::Choose { question: 0, index: 0 }, PAIRED),
            Instant::now(),
        ));
        assert_eq!(sel[0], BTreeSet::from([0]));
    }

    /// AC4 — multi et mono cohabitent dans le même lot sans se contaminer.
    #[test]
    fn multi_et_mono_coexistent_sans_contamination() {
        let table = table_lot("run-lot");
        let tap = |q: u8, i: u8| action("run-lot", IncomingKind::Choose { question: q, index: i }, PAIRED);

        // Q1 (multi) : cumul.
        let _ = classify_action(&table, Some(PAIRED), &tap(1, 2), Instant::now());
        let sel = selection_du_clavier(classify_action(&table, Some(PAIRED), &tap(1, 0), Instant::now()));
        assert_eq!(sel[1], BTreeSet::from([0, 2]), "multi cumule");
        assert!(sel[0].is_empty(), "Q0 intacte");
        assert!(sel[2].is_empty(), "Q2 intacte");

        // Re-tap sur une option multi déjà cochée → DÉcoche (comportement 8.4), pas AlreadySelected.
        let sel = selection_du_clavier(classify_action(&table, Some(PAIRED), &tap(1, 2), Instant::now()));
        assert_eq!(sel[1], BTreeSet::from([0]), "multi décoche");

        // Q0 (mono) : remplace, sans toucher Q1.
        let _ = classify_action(&table, Some(PAIRED), &tap(0, 0), Instant::now());
        let sel = selection_du_clavier(classify_action(&table, Some(PAIRED), &tap(0, 1), Instant::now()));
        assert_eq!(sel[0], BTreeSet::from([1]));
        assert_eq!(sel[1], BTreeSet::from([0]), "Q1 multi non contaminée par le mono");
    }

    /// AC5 — Send incomplet : refusé, ack nommant ce qui manque, **aucun** `take()`.
    #[test]
    fn send_incomplet_refuse_sans_take() {
        let table = table_lot("run-lot");
        let send = action("run-lot", IncomingKind::Confirm, PAIRED);

        // Rien de répondu : les 3 questions manquent.
        let v = classify_action(&table, Some(PAIRED), &send, Instant::now());
        let ActionVerdict::IncompleteSelection { missing } = v else {
            panic!("IncompleteSelection attendu, obtenu {v:?}");
        };
        assert_eq!(missing, vec![0, 1, 2]);
        assert!(!table.is_empty(), "la demande N'EST PAS consommée");

        // Deux questions sur trois répondues : il reste Q2.
        let _ = classify_action(
            &table,
            Some(PAIRED),
            &action("run-lot", IncomingKind::Choose { question: 0, index: 0 }, PAIRED),
            Instant::now(),
        );
        let _ = classify_action(
            &table,
            Some(PAIRED),
            &action("run-lot", IncomingKind::Choose { question: 1, index: 1 }, PAIRED),
            Instant::now(),
        );
        let v = classify_action(&table, Some(PAIRED), &send, Instant::now());
        let ActionVerdict::IncompleteSelection { missing } = v else {
            panic!("IncompleteSelection attendu, obtenu {v:?}");
        };
        assert_eq!(missing, vec![2], "seule Q2 manque");
        assert!(!table.is_empty(), "toujours aucune réponse partielle");
    }

    /// AC6 — Send complet : consomme UNE SEULE fois, avec le JSON de l'AC1.
    #[test]
    fn send_complet_consomme_une_seule_fois() {
        let table = table_lot("run-lot");
        let send = action("run-lot", IncomingKind::Confirm, PAIRED);
        for (q, i) in [(0u8, 1u8), (1, 2), (1, 0), (2, 0)] {
            let _ = classify_action(
                &table,
                Some(PAIRED),
                &action("run-lot", IncomingKind::Choose { question: q, index: i }, PAIRED),
                Instant::now(),
            );
        }
        let ActionVerdict::ResolveChoice(request, response) =
            classify_action(&table, Some(PAIRED), &send, Instant::now())
        else {
            panic!("Send complet doit résoudre");
        };
        assert_eq!(request.request_id, "run-lot");
        assert!(table.is_empty(), "take() a consommé la demande");
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"],
            serde_json::json!({
                "Deploy?": "staging",
                "Which checks?": "lint, types",
                "Notes?": "none"
            }),
            "le JSON envoyé est celui de l'AC1"
        );
        // Anti-rejeu : un second Send ne trouve plus rien.
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &send, Instant::now()),
            ActionVerdict::Unknown
        ));
    }

    /// AC9 / décision 9 — non-régression : une SEULE question mono se résout toujours en UN tap,
    /// sans passer par un Send. C'est le comportement 8.4, explicitement conservé.
    #[test]
    fn question_unique_mono_repond_toujours_en_un_tap() {
        let table = pending_choice("run-1", Duration::from_secs(600), false, &["Alpha", "Beta"]);
        let ActionVerdict::ResolveChoice(_, response) = classify_action(
            &table,
            Some(PAIRED),
            &action("run-1", IncomingKind::Choose { question: 0, index: 1 }, PAIRED),
            Instant::now(),
        ) else {
            panic!("un tap doit répondre en question unique mono");
        };
        assert!(table.is_empty(), "consommée dès le tap");
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Pick one?"],
            "Beta"
        );
        // Le pivot dit bien « pas de Send » (l'ABSENCE du bouton est vérifiée côté canal :
        // `send_absent_en_question_unique_mono` dans telegram.rs).
        assert!(!needs_confirm(&[qd("Pick one?", &["Alpha", "Beta"], false)]));
    }

    /// AC5 (frontière) — l'ack 8.4 reste `EmptySelection` pour N == 1 multi : les deux verdicts
    /// sont délibérément distincts, pas fusionnés. `send_sans_selection_refuse` (8.4) verrouille
    /// l'autre moitié.
    #[test]
    fn empty_et_incomplete_ne_sont_pas_fusionnes() {
        // N == 1 multi, rien de coché.
        let table = pending_choice("run-1", Duration::from_secs(600), true, &["A", "B"]);
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &action("run-1", IncomingKind::Confirm, PAIRED), Instant::now()),
            ActionVerdict::EmptySelection
        ));
        // N > 1, rien de coché.
        let table = table_lot("run-lot");
        assert!(matches!(
            classify_action(&table, Some(PAIRED), &action("run-lot", IncomingKind::Confirm, PAIRED), Instant::now()),
            ActionVerdict::IncompleteSelection { .. }
        ));
    }

    /// Piège n°6 — un `callback_data` forgé porte des indices arbitraires (0..=255). Aucun panic,
    /// aucune mutation : verdict `Unknown`.
    #[test]
    fn indices_hors_bornes_ne_paniquent_pas() {
        let table = table_lot("run-lot");
        for (q, i) in [(9u8, 0u8), (0, 9), (255, 255), (3, 0)] {
            let v = classify_action(
                &table,
                Some(PAIRED),
                &action("run-lot", IncomingKind::Choose { question: q, index: i }, PAIRED),
                Instant::now(),
            );
            assert!(matches!(v, ActionVerdict::Unknown), "q={q} i={i} → {v:?}");
        }
        assert!(!table.is_empty(), "aucune mutation, aucune consommation");

        // Le chemin question-unique mono (`!needs_confirm`) est l'AUTRE branche d'`apply_choose`,
        // celle qui dimensionne un `Vec` localement — la boucle ci-dessus ne l'exerçait pas, si
        // bien que la couverture affichée était plus large que la couverture réelle (revue 8.9).
        let mono = pending_choice("run-1", Duration::from_secs(600), false, &["Alpha", "Beta"]);
        for (q, i) in [(1u8, 0u8), (0, 2), (255, 255)] {
            let v = classify_action(
                &mono,
                Some(PAIRED),
                &action("run-1", IncomingKind::Choose { question: q, index: i }, PAIRED),
                Instant::now(),
            );
            assert!(matches!(v, ActionVerdict::Unknown), "mono q={q} i={i} → {v:?}");
        }
        assert!(!mono.is_empty(), "un indice forgé ne consomme pas la demande");
    }

    /// AC2 — le corps numérote les questions **comme les boutons**, blocs séparés par une ligne
    /// vide, dans l'ordre reçu.
    #[test]
    fn corps_numerote_les_questions_comme_les_boutons() {
        let body = format_choice_body(&vecteur_croise_questions());
        assert_eq!(
            body,
            "1. Release\nDeploy?\n• prod\n• staging\n\n\
             2. Which checks?\n• lint\n• tests\n• types\n\n\
             3. Notes?\n• none"
        );
    }

    #[test]
    fn libelle_option_jamais_dans_le_callback_data() {
        // Contrat domaine : IncomingKind::Choose ne porte que des indices.
        let a = action("run-x", IncomingKind::Choose { question: 0, index: 2 }, PAIRED);
        assert_eq!(a.kind, IncomingKind::Choose { question: 0, index: 2 });
        // Les libellés vivent dans PendingKind, jamais dans l'action entrante.
        match a.kind {
            IncomingKind::Choose { question, index } => {
                assert_eq!(question, 0);
                assert_eq!(index, 2);
            },
            _ => panic!(),
        }
    }
}
