//! Boucle d'envoi des notifications mobiles.
//!
//! Clone structurel de `usage::manager` : un canal `mpsc::unbounded`, une tâche `run_loop`
//! spawnée sur le runtime Tauri, un état partagé `Arc<Mutex<…>>`, un événement poussé au front.
//!
//! ## Fail-open absolu (AC6)
//!
//! Le chemin critique est `pipe::handle_client`, qui doit répondre au hook en un temps borné :
//! un agent bloqué, c'est un développeur bloqué. `trigger()` ne fait donc **que** pousser sur le
//! canal — aucun `await`, aucune I/O, aucune erreur possible. Tout le reste (lecture du store,
//! coffre de l'OS, réseau) se passe **ici**, en tâche de fond, et **aucune** erreur ne remonte au
//! pipe : elle est stockée dans `last_error` et poussée au front, point.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Notify;

/// Profondeur de la file d'envoi. Bornée : sur un incident réseau prolongé (chaque envoi coûtant
/// jusqu'à 10 s de timeout), une file non bornée grossirait sans limite. Au-delà, on **jette** le
/// déclencheur le plus récent — perdre une notification est toujours préférable à faire enfler la
/// mémoire ou à ralentir un agent.
const QUEUE_CAPACITY: usize = 128;

/// Backoff exponentiel du long-poll de réception (Story 8.5, AC1). Remplace le `POLL_RETRY_SECS = 5`
/// fixe de la 8.3. Base courte pour reprendre vite après un incident bref (glitch réseau), plafond
/// pour arrêter de marteler l'API sur une panne durable (réseau coupé, veille PC, `409` ou
/// `invalid_token` persistant) — un échec de réception n'échoue **jamais** un agent (le pire cas
/// reste l'auto-deny au bout du TTL).
///
/// ⚠️ Le plafond n'est PAS qu'une politesse envers l'API : c'est la **latence maximale d'un tap**
/// après une reprise réseau silencieuse. Personne ne prévient Vigil que le réseau est revenu — on
/// ne l'apprend qu'en interrogeant. Un utilisateur qui revient à son poste et tape « Approve »
/// attend donc jusqu'à ce délai. D'où **15 s** et non 30 (revue 8.5) : la séquence 1, 2, 4, 8, 15…
/// n'atteint le plafond qu'après ~15 s de panne *continue*, donc une micro-coupure (VPN, roaming)
/// ne le voit jamais, et une panne durable coûte 4 appels/min au lieu des 12/min du retry fixe.
/// L'autre fenêtre — une demande armée PENDANT le sommeil — est traitée à part par `poll_wake`,
/// qui l'écourte à zéro.
const POLL_BACKOFF_BASE_SECS: u64 = 1;
const POLL_BACKOFF_MAX_SECS: u64 = 15;

/// Nombre maximal de numéros de question énumérés dans un accusé (Story 8.9). L'accusé Telegram
/// est plafonné à 200 caractères ; le domaine réel est 4 questions, mais `MAX_QUESTIONS` autorise
/// bien plus — au-delà, la liste est abrégée plutôt que tronquée par le canal.
const ACK_MAX_LISTED: usize = 5;

/// État du backoff exponentiel du chemin d'erreur de `poll_loop`, **isolé** pour être testable sans
/// sleep ni réseau (avertissement de méthode 8.3 : la logique doit être exercée pour de vrai, pas
/// affirmée). Vit en variable **locale à `poll_loop`** : il persiste à travers les itérations
/// d'erreur successives (la fonction ne retourne pas sur erreur) et un re-spawn de la tâche le
/// réinitialise à la base — c'est voulu.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: u64,
    base: u64,
    max: u64,
}

impl Backoff {
    /// ⚠️ Invariants gardés ICI, pas supposés par les constantes : `base = 0` rendrait `advance`
    /// inerte (`0 * 2 = 0`) et transformerait le chemin d'erreur en boucle chaude — exactement ce
    /// que le backoff existe pour empêcher ; `base > max` ferait dormir plus que le plafond
    /// annoncé, et `reset()` y ramènerait après chaque succès. (Revue 8.5.)
    fn new(base: u64, max: u64) -> Self {
        let max = max.max(1);
        let base = base.clamp(1, max);
        Self { current: base, base, max }
    }
    /// Délai à observer avant le prochain essai.
    fn delay(&self) -> Duration {
        Duration::from_secs(self.current)
    }
    /// Après un échec : double le délai, borné au plafond (`saturating_mul` = pas d'overflow).
    fn advance(&mut self) {
        self.current = self.current.saturating_mul(2).min(self.max);
    }
    /// Après un cycle de réception réussi (`poll_actions` renvoie `Ok`) : retour à la base.
    fn reset(&mut self) {
        self.current = self.base;
    }
}

/// Décide ce qui suit UN tour de réception, **sans I/O** : `None` = enchaîner immédiatement,
/// `Some(d)` = attendre `d` avant de retenter.
///
/// C'est le **seul** endroit qui pilote le backoff. Le placement de `reset` (succès) et
/// d'`advance` (échec) était sinon noyé dans `poll_loop`, que rien ne peut tester (elle exige un
/// `AppHandle` et le coffre de l'OS) : inverser les deux serait passé sur toute la suite verte.
/// L'isoler ici est ce qui rend AC1 réellement verrouillé (revue 8.5, option B).
///
/// ⚠️ Le délai demandé par le canal (`rate_limited:{secs}`, 429) **prime** sur le backoff : le
/// chemin d'envoi l'honore déjà (`retry_after_secs`), le poller le jetait — on re-tapait l'API à
/// 1 s quand le serveur demandait 60 s, ce qui prolonge la limitation au lieu de la purger.
///
/// ⚠️ Aucun délai sur le chemin de succès : un `Ok` vide (long-poll sans tap) bloque déjà ~25 s
/// côté API, y ajouter un sleep ferait exploser la latence d'un tap.
fn plan_next_attempt(
    result: &Result<Vec<ChannelAction>, String>,
    backoff: &mut Backoff,
) -> Option<Duration> {
    match result {
        Ok(_) => {
            backoff.reset();
            None
        }
        Err(e) => {
            let attente = retry_after_secs(e)
                .map(Duration::from_secs)
                .unwrap_or_else(|| backoff.delay());
            backoff.advance();
            Some(attente)
        }
    }
}

/// Attend `attente`, **ou** moins si une demande est armée entre-temps (`poll_wake`).
///
/// C'est la seule chose qui rende le sommeil de backoff non aveugle : sans elle, un agent qui
/// demande une permission pendant le sommeil voit sa notification partir (l'ENVOI est un autre
/// endpoint, il peut très bien marcher pendant que `getUpdates` échoue), l'utilisateur tape, et
/// rien n'est reçu avant la fin du délai.
///
/// ⚠️ `Notify::notify_one` **mémorise un permis** s'il n'y a aucun dormeur : un armement survenu
/// hors sommeil écourte le sommeil SUIVANT. C'est voulu — « quelque chose a été armé depuis » est
/// exactement la condition pour retenter plus tôt. Au pire un appel HTTP de trop.
///
/// Ce réveil n'écourte PAS l'attente quand le réseau revient tout seul (aucun événement local ne
/// le signale) : cette fenêtre-là est bornée par le plafond du backoff, pas par ici.
async fn sleep_or_wake(attente: Duration, wake: &Notify) {
    tokio::select! {
        _ = tokio::time::sleep(attente) => {}
        _ = wake.notified() => {}
    }
}

use super::{
    classify_action, format_choice_body, next_request_id, outcome_for_phone,
    read_agent_notif_overrides, read_notif_settings, response_for_decision, retry_after_secs,
    should_send, ActionCursor, ActionKeyboard, ActionVerdict, ActiveChannel, ChannelAction,
    ChoiceData, MessageRef, MobileNotif, NotifKind, PendingKind, PendingRequest, PendingRequests,
    RequestOutcome,
};
use std::collections::BTreeSet;
use crate::agent::state::SourceTool;
use crate::pipe::manager::{approval_window_secs, PendingApprovals};

/// Transition notifiable détectée par le pipe. Construite **dans** le verrou du moteur d'état
/// (le temps de lire l'instantané) mais consommée **hors** de celui-ci.
#[derive(Debug, Clone)]
pub struct NotifTrigger {
    /// Identifie la session : c'est à la fois la clé de son nom personnalisé et celle de ses
    /// surcharges de notification (`agentNotifs:{transcript_path}`, Story 8.2). Le filtrage par
    /// agent vit donc entièrement dans `run_loop`, sans une ligne de câblage dans le pipe.
    pub transcript_path: String,
    pub kind: NotifKind,
    pub agent_label: String,
    pub tool: SourceTool,
    pub summary: Option<String>,
    /// Story 8.3/8.4 — la demande porte-t-elle un clavier mobile ? Calculé **dans le verrou
    /// d'état** du pipe (`is_mobile_actionable`), seul endroit qui tient l'instantané complet.
    pub actionable: bool,
    /// Story 8.4 — données d'un clavier de choix (si `is_choice_actionable`), sinon `None`
    /// (Approve/Deny ou notif-only).
    pub choice: Option<ChoiceData>,
}

/// État poussé au front sur `notify:state` — sert à afficher l'échec d'un envoi asynchrone dans
/// les Settings (le pipe, lui, n'en saura jamais rien).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyState {
    pub last_error: Option<String>,
}

/// État partagé entre la boucle d'ENVOI, la boucle de RÉCEPTION et les voies de résolution du
/// poste. Regroupé pour que les trois garanties d'unicité vivent au même endroit.
#[derive(Default)]
pub struct Shared {
    /// Demandes armées, en mémoire uniquement.
    pub pending: PendingRequests,
    /// Une SEULE boucle de réception à la fois : deux long-polls concurrents sur le même bot se
    /// cassent mutuellement (conflit d'écoute exclusive).
    poller_running: AtomicBool,
    /// Un appairage est en cours. Il fait lui aussi un long-poll exclusif → la boucle de réception
    /// doit s'effacer le temps qu'il dure (garde EXPLICITE, cf. AC4).
    pairing_active: AtomicBool,
    /// Point de reprise du flux entrant, **hissé hors de la tâche** : un offset confirmé en local
    /// ne prend effet côté API qu'au tour suivant. Si le curseur mourait avec la tâche, le poller
    /// suivant repartait de zéro et rejouait le dernier tap (« Already handled » non sollicité).
    action_cursor: Mutex<ActionCursor>,
    /// Réveille la boucle de réception endormie en backoff. Sans lui, une demande armée pendant
    /// le sommeil n'est pas interrogée avant la fin du délai : `ensure_poller` échoue en silence
    /// sur `try_claim_poller` (la tâche endormie détient le drapeau) et le re-`ensure_poller`
    /// n'arrive qu'à la FIN de `poll_loop`. L'utilisateur reçoit alors sa notification, tape, et
    /// sa roue tourne jusqu'au bout du backoff. (Revue 8.5.)
    poll_wake: Notify,
}

/// Garde RAII du drapeau `poller_running` : un panic ou un retour anticipé ne le laisse jamais
/// bloqué à `true` pour la durée du processus (sinon plus aucun tap n'est jamais reçu).
struct PollerClaim {
    shared: Arc<Shared>,
}

impl Drop for PollerClaim {
    fn drop(&mut self) {
        self.shared.poller_running.store(false, Ordering::SeqCst);
    }
}

/// Garde RAII du drapeau `pairing_active` : si le futur d'appairage est annulé (webview rechargée,
/// fenêtre de réglages fermée) ou panique, le drapeau redescend. Sans ça, `poll_loop` boucle sur
/// `sleep(1s)` en silence jusqu'à la fin du processus.
pub struct PairingGuard {
    shared: Arc<Shared>,
}

impl Drop for PairingGuard {
    fn drop(&mut self) {
        self.shared.pairing_active.store(false, Ordering::SeqCst);
    }
}

impl Shared {
    /// Revendique le droit d'être LA boucle de réception. `None` = une autre tourne déjà.
    /// La garde rend le drapeau à la destruction (y compris sur panic).
    fn try_claim_poller(self: &Arc<Self>) -> Option<PollerClaim> {
        if self
            .poller_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            Some(PollerClaim {
                shared: Arc::clone(self),
            })
        } else {
            None
        }
    }

    /// `true` tant qu'une boucle de réception détient le drapeau (y compris pendant le long-poll
    /// en vol après que la table se soit vidée — c'est exactement la fenêtre d'un conflit d'écoute).
    pub fn poller_in_progress(&self) -> bool {
        self.poller_running.load(Ordering::SeqCst)
    }

    /// Ouvre une fenêtre d'appairage. `None` = refus explicite.
    ///
    /// Refuse si la table n'est pas vide **OU** si le poller tourne encore (un long-poll peut
    /// survivre jusqu'à ~35 s après la résolution de la dernière demande). Tester seulement la
    /// table autorisait l'appairage pendant ce vol → conflit d'écoute exclusive, exactement ce
    /// que la décision #5 interdit.
    pub fn try_begin_pairing(self: &Arc<Self>) -> Option<PairingGuard> {
        if !self.pending.is_empty() || self.poller_in_progress() {
            return None;
        }
        if self
            .pairing_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            Some(PairingGuard {
                shared: Arc::clone(self),
            })
        } else {
            None
        }
    }

    fn pairing_in_progress(&self) -> bool {
        self.pairing_active.load(Ordering::SeqCst)
    }

    /// Oublie le point de reprise du flux entrant. À appeler quand le **bot** change : les
    /// `update_id` sont une séquence propre à un bot, et l'offset hérité du précédent (typiquement
    /// bien plus haut) filtrerait TOUTES les updates du nouveau. Symptôme : plus aucun tap ne
    /// remonte, **sans la moindre erreur affichée** (`Ok(vec![])` en continu), jusqu'au redémarrage
    /// du processus. Même raisonnement que le `chat_id = None` de `notify_set_token` — un changement
    /// de bot invalide tout ce qui décrivait le précédent. (Revue 8.5.)
    ///
    /// ⚠️ Seul endroit qui écrase le curseur au lieu de passer par `set` : c'est délibéré, `set`
    /// est monotone par construction (AC2) et ne peut donc PAS servir à revenir en arrière.
    pub fn reset_action_cursor(&self) {
        if let Ok(mut cursor) = self.action_cursor.lock() {
            *cursor = ActionCursor::default();
        }
    }
}

pub struct NotifyManager {
    state: Arc<Mutex<NotifyState>>,
    tx: Sender<NotifTrigger>,
    shared: Arc<Shared>,
    app: AppHandle,
}

impl NotifyManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(NotifyState::default()));
        let shared = Arc::new(Shared::default());

        let state_clone = Arc::clone(&state);
        let shared_clone = Arc::clone(&shared);
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            run_loop(app_clone, state_clone, shared_clone, rx).await;
        });

        Arc::new(Self { state, tx, shared, app })
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Prévient que la demande d'un agent vient d'être résolue **par une autre voie** que le tap
    /// mobile (Vue Détail, `Always approve`, auto-deny, éviction).
    ///
    /// **Fire-and-forget, et ça n'est pas négociable** : approuver depuis la Vue Détail ne doit
    /// jamais rendre une erreur parce que le réseau est tombé. Le retrait de l'entrée est immédiat
    /// (donc l'anti-rejeu vaut tout de suite) ; seul le désarmement du message part en tâche de
    /// fond, et son échec est inoffensif.
    pub fn request_resolved(&self, transcript_path: &str, outcome: RequestOutcome) {
        let Some(request) = self.shared.pending.take_by_path(transcript_path) else {
            return;
        };
        let Some(message) = request.message else {
            // Résolue AVANT que l'envoi n'ait rendu sa référence de message (« piège n°1 ») : il
            // n'y a rien à éditer, mais l'entrée est bien retirée — c'est ce qui compte.
            return;
        };
        let app = self.app.clone();
        let state = Arc::clone(&self.state);
        let text = request.text;
        tauri::async_runtime::spawn(async move {
            disarm_message(&app, &state, &message, &text, outcome).await;
        });
    }

    /// Point d'entrée du pipe : **fire-and-forget**. `try_send` ne bloque **jamais** — ni sur une
    /// file pleine, ni sur un canal fermé (arrêt de l'app) : les deux sont ignorés
    /// silencieusement. Perdre une notification est toujours préférable à ralentir un agent.
    pub fn trigger(&self, trigger: NotifTrigger) {
        let _ = self.tx.try_send(trigger);
    }

    pub fn last_error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.last_error.clone())
    }

    /// Efface l'erreur mémorisée (appelé quand l'utilisateur agit : re-appairage, test réussi,
    /// déconnexion) pour ne pas laisser un message périmé dans les Settings.
    pub fn clear_error(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.last_error = None;
        }
    }

    /// Jumelle de `clear_error` pour le curseur : appelée aux bascules d'appairage qui changent (ou
    /// retirent) le BOT, jamais sur un simple changement de destinataire — un nouveau `chat_id` sur
    /// le même bot partage la même séquence d'`update_id`, le curseur y reste valide.
    pub fn reset_action_cursor(&self) {
        self.shared.reset_action_cursor();
    }
}

/// Nom personnalisé donné par l'utilisateur à cette session, s'il existe (mêmes clé et forme que
/// l'enrichissement des instantanés). Un nom vide est traité comme absent.
fn read_custom_name(app: &AppHandle, transcript_path: &str) -> Option<String> {
    use tauri_plugin_store::StoreExt;
    let store = app.store(crate::SETTINGS_STORE).ok()?;
    let meta: crate::session_meta::SessionMetadata =
        serde_json::from_value(store.get(crate::session_meta::session_meta_key(transcript_path))?)
            .ok()?;
    meta.custom_name.filter(|n| !n.is_empty())
}

async fn run_loop(
    app: AppHandle,
    state: Arc<Mutex<NotifyState>>,
    shared: Arc<Shared>,
    mut rx: Receiver<NotifTrigger>,
) {
    while let Some(trigger) = rx.recv().await {
        // Court-circuit AVANT toute I/O réseau ou accès au coffre (AC6/AC8) : notifications
        // désactivées = deux lectures de store et rien d'autre, donc zéro latence ajoutée et
        // aucun appel sortant sur le chemin par défaut.
        //
        // Story 8.2 — la résolution à trois couches (surcharge de l'agent > global > défaut) se
        // fait ICI et nulle part ailleurs : côté Rust, donc valable **fenêtre React fermée**, et
        // avant `ActiveChannel::active` / `read_custom_name`, donc un agent totalement coupé ne
        // déclenche ni accès au coffre de l'OS, ni appel réseau, ni lecture de son nom.
        let settings = read_notif_settings(&app);
        let overrides = read_agent_notif_overrides(&app, &trigger.transcript_path);
        if !should_send(trigger.kind, &settings, &overrides) {
            continue;
        }
        let Some(channel) = ActiveChannel::active(&settings) else {
            // Appairage incomplet (pas de secret dans le coffre, ou pas de destinataire) :
            // aucun appel réseau n'est tenté.
            continue;
        };

        // ── Armement (Story 8.3, AC2) ────────────────────────────────────────────
        //
        // ⚠️ « Piège n°1 » — le `request_id` est inscrit dans la table AVANT l'appel réseau. Le
        // push mobile part avant que la réponse HTTP ne revienne : armer après, ce serait un
        // `request_id` inconnu sur un tap légitime, non déterministe et pénible à reproduire.
        let body_extra = trigger
            .choice
            .as_ref()
            .map(|c| format_choice_body(&c.questions));
        let mut notif = MobileNotif {
            kind: trigger.kind,
            // Le nom personnalisé vit dans le store (métadonnées de session), pas dans
            // l'instantané brut du moteur d'état. Résolu ICI, après le filtrage : le pipe n'a
            // ainsi aucune lecture de store à faire pour une notification qui ne partira pas.
            agent_label: read_custom_name(&app, &trigger.transcript_path)
                .unwrap_or(trigger.agent_label),
            tool: trigger.tool,
            summary: trigger.summary,
            request_id: None,
            keyboard: None,
            body_extra,
        };
        // Le texte est mémorisé avec la demande pour pouvoir ré-éditer le message à la résolution
        // (le `request_id` n'entre pas dans le rendu : le calculer avant l'armement est exact).
        if let Some((id, keyboard)) = arm_request(
            &shared,
            &trigger.transcript_path,
            trigger.kind,
            trigger.actionable,
            super::format_notif_text(&notif),
            trigger.choice,
        ) {
            notif.request_id = Some(id);
            notif.keyboard = Some(keyboard);
        }

        let result = channel.send(&notif).await;

        // Un SEUL ré-essai, et uniquement sur limitation de débit, après le délai demandé par le
        // canal (borné à 30 s). Tout autre échec n'est pas retenté : il n'y a pas de file
        // d'attente en 8.1 — une notification ratée est perdue, jamais rejouée en rafale.
        //
        // ⚠️ Le ré-essai est délégué à une tâche : l'attendre ICI figerait la boucle jusqu'à
        // 30 s, donc gèlerait les notifications de TOUS les autres agents alors qu'elles n'ont
        // aucun rapport avec l'agent limité (blocage en tête de file).
        if let Some(wait) = result.as_ref().err().and_then(|e| retry_after_secs(e)) {
            let app = app.clone();
            let state = Arc::clone(&state);
            let shared = Arc::clone(&shared);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                let retried = channel.send(&notif).await;
                settle_armed(&shared, notif.request_id.as_deref(), &retried);
                publish(&app, &state, retried.err());
                ensure_poller(&app, &state, &shared);
            });
            continue;
        }

        settle_armed(&shared, notif.request_id.as_deref(), &result);
        publish(&app, &state, result.err());
        ensure_poller(&app, &state, &shared);
    }
}

/// Réserve un `request_id` pour une demande mobile-actionnable, et l'inscrit AVANT l'envoi
/// (« piège n°1 »). `None` pour `Ended`/`Error`/non actionnable.
fn arm_request(
    shared: &Arc<Shared>,
    transcript_path: &str,
    kind: NotifKind,
    actionable: bool,
    text: String,
    choice: Option<ChoiceData>,
) -> Option<(String, ActionKeyboard)> {
    if kind != NotifKind::Waiting || !actionable {
        return None;
    }
    // Le TTL est la fenêtre d'approbation EFFECTIVE de cet agent, calculée par la MÊME fonction
    // pure que le pipe. À ce point du code les trois conditions sont déjà acquises : la demande
    // est actionnable, `should_send` a laissé passer, et `ActiveChannel::active` a exigé un
    // destinataire. Un tap ne peut donc pas arriver dans une fenêtre où le pipe a déjà auto-refusé.
    let ttl = Duration::from_secs(approval_window_secs(true, true, true));
    let request_id = next_request_id();
    let (pending_kind, keyboard) = if let Some(c) = choice {
        // ⚠️ Invariant `selected.len() == questions.len()` — posé ICI, une seule fois, jamais
        // recalculé ailleurs (piège n°6).
        let selected = vec![BTreeSet::new(); c.questions.len()];
        let keyboard = ActionKeyboard::Choice {
            request_id: request_id.clone(),
            questions: c.questions.clone(),
            selected: selected.clone(),
        };
        (
            PendingKind::Choice {
                questions: c.questions,
                selected,
                questions_json: c.questions_json,
            },
            keyboard,
        )
    } else {
        (
            PendingKind::ApproveDeny,
            ActionKeyboard::ApproveDeny {
                request_id: request_id.clone(),
            },
        )
    };
    shared.pending.insert(PendingRequest {
        request_id: request_id.clone(),
        transcript_path: transcript_path.to_string(),
        expires_at: Instant::now() + ttl,
        message: None,
        text,
        kind: pending_kind,
    });
    Some((request_id, keyboard))
}

/// Complète (ou retire) l'entrée armée selon l'issue de l'envoi. Un envoi échoué laisse la table
/// **cohérente** : plus rien à résoudre, plus rien à désarmer (AC2).
fn settle_armed(
    shared: &Arc<Shared>,
    request_id: Option<&str>,
    result: &Result<Option<MessageRef>, String>,
) {
    let Some(request_id) = request_id else {
        return;
    };
    match result {
        Ok(Some(message)) => shared.pending.attach_message(request_id, message.clone()),
        Ok(None) => {}
        Err(_) => {
            shared.pending.take(request_id);
        }
    }
}

// ── Boucle de réception à la demande (AC3, AC4, AC5) ─────────────────────────────

/// Démarre la boucle de réception si une demande est en attente et qu'aucune ne tourne déjà.
/// Appelée après chaque envoi : c'est le passage de « table vide » à « table non vide ».
///
/// ⚠️ Course `claim` / `release` : entre le moment où `poll_loop` voit la table vide et celui où
/// la garde rend le drapeau, un nouvel armement peut voir `try_claim_poller` échouer et
/// abandonner. D'où le **re-`ensure_poller` après drop de la garde** : si quelque chose a été
/// armé pendant la sortie, une nouvelle boucle démarre.
fn ensure_poller(app: &AppHandle, state: &Arc<Mutex<NotifyState>>, shared: &Arc<Shared>) {
    if shared.pending.is_empty() {
        return;
    }
    // Quelque chose vient d'être armé : si une boucle dort en backoff, qu'elle retente MAINTENANT
    // plutôt qu'au bout du délai (revue 8.5). Émis avant le claim, car c'est précisément quand le
    // claim échoue — une tâche endormie détient le drapeau — que le réveil est nécessaire.
    shared.poll_wake.notify_one();
    let Some(claim) = shared.try_claim_poller() else {
        return;
    };
    let app = app.clone();
    let state = Arc::clone(state);
    let shared_task = Arc::clone(shared);
    tauri::async_runtime::spawn(async move {
        // `claim` vit jusqu'à la fin de `poll_loop` (y compris panic) → le drapeau redescend.
        poll_loop(app.clone(), Arc::clone(&state), Arc::clone(&shared_task)).await;
        drop(claim);
        // Fenêtre de course claim/release : relancer si une demande a été armée entre-temps.
        ensure_poller(&app, &state, &shared_task);
    });
}

/// Long-poll tant qu'une demande est en attente. **Zéro appel sortant au repos** : la boucle
/// s'arrête dès que la table se vide et ne redémarre qu'à l'armement suivant.
async fn poll_loop(app: AppHandle, state: Arc<Mutex<NotifyState>>, shared: Arc<Shared>) {
    // Backoff local à la tâche : persiste entre les itérations d'erreur, réinitialisé à la base
    // dès qu'un tour de réception réussit (AC1). Un re-spawn de `poll_loop` repart à la base.
    let mut backoff = Backoff::new(POLL_BACKOFF_BASE_SECS, POLL_BACKOFF_MAX_SECS);
    while !shared.pending.is_empty() {
        // Garde d'exclusion mutuelle avec l'appairage : les deux font un long-poll exclusif sur
        // le même bot — deux écoutes concurrentes se cassent mutuellement.
        if shared.pairing_in_progress() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        for perimee in shared.pending.purge_expired(Instant::now()) {
            if let Some(message) = &perimee.message {
                disarm_message(&app, &state, message, &perimee.text, RequestOutcome::Expired).await;
            }
        }
        if shared.pending.is_empty() {
            break;
        }

        let settings = read_notif_settings(&app);
        let Some(channel) = ActiveChannel::active(&settings) else {
            // Déconnecté pendant qu'une demande était en vol (jeton retiré du coffre, dé-appairage
            // depuis les Réglages). Les demandes restantes expireront d'elles-mêmes côté pipe.
            //
            // ⚠️ NE JAMAIS `break` ici. La closure de `ensure_poller` re-appelle `ensure_poller`
            // après CHAQUE fin de `poll_loop` (rattrapage de la course claim/release) et, sur ce
            // chemin, la table n'est PAS vide : re-claim → re-spawn → re-`break` → … boucle serrée
            // de `spawn` + `read_notif_settings` + `secret::get_token` (aucun cache : un appel au
            // gestionnaire d'identifiants de l'OS par tour) jusqu'à l'expiration de la demande, soit
            // jusqu'à 25 min à 100 % d'un cœur. Bug d'INTERACTION entre deux gestes corrects
            // séparément (revue 8.5 ; présent depuis la 8.3).
            //
            // On attend donc, comme sur le chemin d'erreur mais SANS `publish` — se déconnecter est
            // un choix de l'utilisateur, pas une panne à afficher — et sans le moindre appel
            // sortant. Si le canal revient, le tour suivant reprend le polling sans rien réarmer ;
            // sinon `purge_expired` finit par vider la table et la boucle sort par le haut.
            let attente = backoff.delay();
            backoff.advance();
            sleep_or_wake(attente, &shared.poll_wake).await;
            continue;
        };

        // Curseur hissé dans `Shared` : lu avant l'appel, réécrit après. Ne jamais tenir le
        // `Mutex` à travers un `.await` réseau.
        let mut cursor = shared
            .action_cursor
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();

        let result = channel.poll_actions(&mut cursor).await;

        // Réécriture du curseur partagé, succès comme échec : avancer si le canal a bougé le
        // curseur local évite de re-traiter un lot partiellement consommé. ⚠️ AC5 : une erreur
        // réseau pure laisse `cursor` INCHANGÉ (l'échec survient avant `cursor.set` dans
        // `poll_actions`, verrouillé par `un_tour_rate_navance_jamais_le_curseur`) → la même
        // update est re-demandée au tour suivant, aucune fenêtre d'oubli.
        // ⚠️ Passer par `set` et NON par `*stored = cursor` : c'est `set` qui porte la garantie
        // de monotonie (AC2). L'écrasement direct la court-circuitait, et laissait la garantie
        // hors du seul chemin d'écriture réel (revue 8.5).
        if let (Ok(mut stored), Some(offset)) = (shared.action_cursor.lock(), cursor.get()) {
            stored.set(offset);
        }

        // Pilotage du backoff isolé dans une fonction pure et testée (AC1).
        let attente = plan_next_attempt(&result, &mut backoff);

        match result {
            Ok(actions) => {
                for action in actions {
                    handle_action(&app, &state, &shared, &channel, &settings.chat_id, action).await;
                }
            }
            // Aucune erreur ne remonte : elle s'arrête à `publish`. Le pire cas reste l'auto-deny
            // au bout du TTL — un agent n'échoue JAMAIS à cause de la réception.
            Err(e) => publish(&app, &state, Some(e)),
        }

        // Ce sommeil retarde la RÉCEPTION d'un tap — effet fonctionnel, pas seulement le
        // désarmement cosmétique d'un message périmé par `purge_expired`. Deux fenêtres, deux
        // parades (revue 8.5) : une demande armée pendant le sommeil le réveille (`poll_wake`) ;
        // une reprise réseau silencieuse, que rien ne signale localement, est bornée par le
        // plafond du backoff (15 s). Il retarde aussi le relâchement du `PollerClaim` qui bloque
        // l'appairage, fenêtre du même ordre que les ~35 s du long-poll en vol, déjà assumées par
        // `try_begin_pairing`. Jamais un blocage d'agent : l'auto-deny côté pipe a son propre timer.
        if let Some(attente) = attente {
            sleep_or_wake(attente, &shared.poll_wake).await;
        }
    }
}

/// Traite UNE action entrante : contrôle d'expéditeur, consommation / bascule, expiration,
/// réinjection dans le pipeline existant, accusé, désarmement / ré-édition clavier.
async fn handle_action(
    app: &AppHandle,
    state: &Arc<Mutex<NotifyState>>,
    shared: &Arc<Shared>,
    channel: &ActiveChannel,
    paired: &Option<String>,
    action: ChannelAction,
) {
    // Tous les contrôles de sécurité sont dans une fonction pure (ou méthodes sync de table).
    // ⚠️ Verrou PendingRequests JAMAIS tenu à travers un `.await`.
    match classify_action(
        &shared.pending,
        paired.as_deref(),
        &action,
        Instant::now(),
    ) {
        // Expéditeur étranger : rien, pas même un accusé.
        ActionVerdict::Ignored => {}
        ActionVerdict::Unknown => {
            let _ = channel
                .ack_action(&action.action_ref, "Already handled — nothing to do")
                .await;
        }
        ActionVerdict::EmptySelection => {
            let _ = channel
                .ack_action(&action.action_ref, "Select at least one option")
                .await;
        }
        // Story 8.9 — Send incomplet sur un lot : refusé, rien n'est consommé. On NOMME les
        // questions manquantes : le verdict porte leurs indices, et « Answer all {n} questions »
        // ferait lire le nombre de MANQUANTES comme le total du lot (revue 8.9). L'ack Telegram
        // est court (200 caractères) : au-delà de `ACK_MAX_LISTED` on abrège.
        ActionVerdict::IncompleteSelection { missing } => {
            let numeros: Vec<String> = missing
                .iter()
                .take(ACK_MAX_LISTED)
                .map(|q| (q + 1).to_string())
                .collect();
            let suite = if missing.len() > ACK_MAX_LISTED { "…" } else { "" };
            let message = match numeros.as_slice() {
                [q] => format!("Still missing question {q}"),
                _ => format!("Still missing questions {}{suite}", numeros.join(", ")),
            };
            let _ = channel.ack_action(&action.action_ref, &message).await;
        }
        // Story 8.9 — re-tap mono : ack SEUL. Aucun `update_actions` (piège n°3).
        ActionVerdict::AlreadySelected => {
            let _ = channel
                .ack_action(&action.action_ref, "Already selected")
                .await;
        }
        ActionVerdict::Expired(request) => {
            let _ = channel
                .ack_action(&action.action_ref, "⏱ Expired — nothing was executed")
                .await;
            if let Some(message) = &request.message {
                disarm_message(app, state, message, &request.text, RequestOutcome::Expired).await;
            }
        }
        ActionVerdict::Toggle { message, keyboard } => {
            // Bascule multi : toujours accuser (sinon roue 30 s).
            let _ = channel.ack_action(&action.action_ref, "Selection updated").await;
            if let Some(message) = message {
                if let Err(e) = channel.update_actions(&message, &keyboard).await {
                    publish(app, state, Some(e));
                }
            }
        }
        ActionVerdict::Resolve(request, decision) => {
            let response = response_for_decision(decision);
            let resolved = {
                let pa_state = app.state::<Mutex<PendingApprovals>>();
                let mut pa = match pa_state.lock() {
                    Ok(pa) => pa,
                    Err(_) => return,
                };
                pa.resolve(&request.transcript_path, response.to_string())
            };
            let outcome = if !resolved {
                let _ = channel
                    .ack_action(&action.action_ref, "Already handled — nothing to do")
                    .await;
                RequestOutcome::AlreadyHandled
            } else {
                let outcome = outcome_for_phone(decision);
                let ack = match decision {
                    super::ApprovalDecision::Approve => "✅ Approved",
                    super::ApprovalDecision::Deny => "❌ Denied",
                };
                let _ = channel.ack_action(&action.action_ref, ack).await;
                outcome
            };
            if let Some(message) = &request.message {
                disarm_message(app, state, message, &request.text, outcome).await;
            }
        }
        ActionVerdict::ResolveChoice(request, response_json) => {
            // Même chemin que `send_message` : resolve avec le JSON `make_ask_response`.
            let resolved = {
                let pa_state = app.state::<Mutex<PendingApprovals>>();
                let mut pa = match pa_state.lock() {
                    Ok(pa) => pa,
                    Err(_) => return,
                };
                pa.resolve(&request.transcript_path, response_json)
            };
            let outcome = if !resolved {
                let _ = channel
                    .ack_action(&action.action_ref, "Already handled — nothing to do")
                    .await;
                RequestOutcome::AlreadyHandled
            } else {
                let _ = channel.ack_action(&action.action_ref, "✅ Answered").await;
                RequestOutcome::AnsweredFromPhone
            };
            if let Some(message) = &request.message {
                disarm_message(app, state, message, &request.text, outcome).await;
            }
        }
    }
}

/// Retire le clavier d'un message et y reflète l'issue. Un échec est **inoffensif** : l'anti-rejeu
/// reste la garantie de correction, le désarmement n'est que du confort.
async fn disarm_message(
    app: &AppHandle,
    state: &Arc<Mutex<NotifyState>>,
    message: &MessageRef,
    text: &str,
    outcome: RequestOutcome,
) {
    let settings = read_notif_settings(app);
    let Some(channel) = ActiveChannel::active(&settings) else {
        return;
    };
    let body = if text.is_empty() {
        outcome.suffix().to_string()
    } else {
        format!("{text}\n{}", outcome.suffix())
    };
    if let Err(e) = channel.disarm(message, &body).await {
        publish(app, state, Some(e));
    }
}

/// Mémorise l'issue d'un envoi et la pousse au front. Toute erreur s'arrête ici : elle n'est
/// **jamais** propagée au pipe (fail-open absolu, AC6).
fn publish(app: &AppHandle, state: &Arc<Mutex<NotifyState>>, last_error: Option<String>) {
    if let Ok(mut s) = state.lock() {
        s.last_error = last_error.clone();
    }
    let _ = app.emit("notify:state", NotifyState { last_error });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::manager::MOBILE_APPROVAL_TIMEOUT_SECS;

    fn demande(request_id: &str) -> PendingRequest {
        PendingRequest {
            request_id: request_id.into(),
            transcript_path: format!("C:\\{request_id}.jsonl"),
            expires_at: Instant::now() + Duration::from_secs(600),
            message: None,
            text: String::new(),
            kind: PendingKind::ApproveDeny,
        }
    }

    /// AC4 — la boucle de réception suit la table : elle démarre quand une demande apparaît,
    /// s'arrête quand il n'en reste aucune, et **une seule** peut tourner à la fois.
    ///
    /// Exercice aussi la course claim/release : un armement pendant que la garde est encore
    /// détenue échoue à revendiquer, puis un re-claim après drop réussit (c'est le contrat de
    /// `ensure_poller` post-drop).
    #[test]
    fn poller_demarre_et_s_arrete_avec_la_table() {
        let shared = Arc::new(Shared::default());

        // Au repos : la table est vide → ensure_poller court-circuite avant tout claim.
        // C'est la garantie « zéro trafic au repos ».
        assert!(shared.pending.is_empty());
        assert!(!shared.poller_in_progress());

        shared.pending.insert(demande("run-1"));
        assert!(!shared.pending.is_empty(), "la table passe de vide à non-vide → la boucle démarre");

        // Unicité : deux long-polls concurrents se cassent mutuellement. La seconde revendication
        // doit échouer.
        let claim = shared.try_claim_poller().expect("première boucle");
        assert!(shared.poller_in_progress());
        assert!(shared.try_claim_poller().is_none(), "une seule boucle à la fois");

        // Une seconde demande ne relance pas une seconde boucle tant que la première détient.
        shared.pending.insert(demande("run-2"));
        assert!(shared.try_claim_poller().is_none());

        // Course claim/release : pendant que le drapeau est encore levé, un armement voit l'échec
        // (c'est le trou historique). Le drop + re-claim le referme.
        assert!(shared.pending.take("run-1").is_some());
        assert!(shared.pending.take("run-2").is_some());
        assert!(shared.pending.is_empty(), "condition d'arrêt de la boucle");
        // Nouvel armement PENDANT la sortie (drapeau encore levé) → claim échoue.
        shared.pending.insert(demande("run-3"));
        assert!(shared.try_claim_poller().is_none(), "course : drapeau encore levé");
        drop(claim);
        // Après release : ensure_poller re-tente et obtient le drapeau.
        assert!(
            shared.try_claim_poller().is_some(),
            "l'armement pendant la sortie est repris après release"
        );
    }

    /// AC4 — la boucle et l'appairage ne peuvent JAMAIS tourner ensemble. Garde **explicite**,
    /// pas déduite de l'usage : les deux font un long-poll exclusif sur le même bot, et la boucle
    /// confirme un offset que `pair()` ne consomme exprès jamais.
    ///
    /// ⚠️ Refuse aussi quand le poller tourne encore alors que la table est vide (long-poll en
    /// vol jusqu'à ~35 s) — tester seulement `is_empty` laissait passer l'appairage dans cette
    /// fenêtre.
    #[test]
    fn appairage_et_boucle_sexcluent() {
        let shared = Arc::new(Shared::default());

        // Cas nominal : on appaire au poste, aucune demande n'attend → autorisé.
        let guard = shared.try_begin_pairing().expect("appairage libre");
        assert!(shared.pairing_in_progress());
        // RAII : drop redescend le drapeau (couvre annulation / panic du futur d'appairage).
        drop(guard);
        assert!(!shared.pairing_in_progress());

        // Une demande en attente ⇒ l'appairage est REFUSÉ (`busy_receiving`), plutôt qu'un
        // conflit d'écoute.
        shared.pending.insert(demande("run-1"));
        assert!(shared.try_begin_pairing().is_none());
        assert!(!shared.pairing_in_progress(), "un refus ne laisse pas le drapeau levé");

        // Table vide MAIS poller encore en vol → refus aussi (le trou historique).
        shared.pending.take("run-1");
        assert!(shared.pending.is_empty());
        let claim = shared.try_claim_poller().expect("poller en sortie");
        assert!(shared.try_begin_pairing().is_none(), "poller en vol bloque l'appairage");
        drop(claim);

        // Poller libre, table vide → appairage possible.
        let g = shared.try_begin_pairing().expect("libre à nouveau");
        // Et deux appairages simultanés sont impossibles aussi.
        assert!(shared.try_begin_pairing().is_none());
        drop(g);
    }

    /// Le curseur d'actions survit à la tâche de polling : un second poller reprend où le
    /// précédent s'est arrêté (sinon rejeu du dernier tap → « Already handled » non sollicité).
    #[test]
    fn curseur_survit_a_la_tache_de_polling() {
        let shared = Shared::default();
        {
            let mut c = shared.action_cursor.lock().unwrap();
            c.set(42);
        }
        assert_eq!(shared.action_cursor.lock().unwrap().get(), Some(42));
    }

    /// AC2 — l'armement suit l'actionnabilité, et l'échec d'envoi laisse la table cohérente.
    #[test]
    fn armement_reserve_un_id_seulement_pour_une_demande_actionnable() {
        let shared = Arc::new(Shared::default());

        // Notif-only : `Ended`, `Error`, ou `Waiting` non actionnable → aucun identifiant réservé,
        // donc aucun bouton et aucune entrée dans la table.
        for (kind, actionable) in [
            (NotifKind::Ended, true),
            (NotifKind::Error, true),
            (NotifKind::Waiting, false),
        ] {
            assert!(
                arm_request(&shared, "C:\\a.jsonl", kind, actionable, "t".into(), None).is_none()
            );
            assert!(shared.pending.is_empty(), "{kind:?}/{actionable} ne doit rien armer");
        }

        // Waiting actionnable → identifiant réservé AVANT l'envoi (« piège n°1 ») et TTL aligné
        // sur la fenêtre mobile.
        let (id, kb) = arm_request(
            &shared,
            "C:\\a.jsonl",
            NotifKind::Waiting,
            true,
            "texte".into(),
            None,
        )
        .expect("demande actionnable armée");
        assert!(matches!(kb, ActionKeyboard::ApproveDeny { .. }));
        assert!(!shared.pending.is_empty());

        // Envoi réussi : la référence de message est attachée pour permettre le désarmement.
        settle_armed(&shared, Some(&id), &Ok(Some(MessageRef::new("42"))));
        let request = shared.pending.take_by_path("C:\\a.jsonl").unwrap();
        assert_eq!(request.message, Some(MessageRef::new("42")));
        assert_eq!(request.text, "texte", "le texte d'origine sert à ré-éditer le message");
        // Encadré des DEUX côtés : un plancher seul laisserait passer n'importe quel TTL absurde
        // (100 000 s aurait été vert), alors que ce test prétend prouver une valeur précise.
        assert!(
            request.expires_at > Instant::now() + Duration::from_secs(APPROVAL_TIMEOUT_TEST_FLOOR),
            "le TTL doit être la fenêtre mobile ({MOBILE_APPROVAL_TIMEOUT_SECS} s), pas la fenêtre par défaut ({} s)",
            crate::pipe::manager::APPROVAL_TIMEOUT_SECS
        );
        assert!(
            request.expires_at <= Instant::now() + Duration::from_secs(MOBILE_APPROVAL_TIMEOUT_SECS),
            "le TTL ne doit pas DÉPASSER la fenêtre mobile ({MOBILE_APPROVAL_TIMEOUT_SECS} s)"
        );

        // Envoi échoué : l'entrée est RETIRÉE (rien à résoudre, rien à désarmer).
        let (id, _) = arm_request(
            &shared,
            "C:\\b.jsonl",
            NotifKind::Waiting,
            true,
            String::new(),
            None,
        )
        .unwrap();
        settle_armed(&shared, Some(&id), &Err("network_error: timeout".into()));
        assert!(shared.pending.is_empty(), "un envoi raté laisse la table cohérente");

        // Story 8.4 — choix multi : armement avec PendingKind::Choice.
        let choice = ChoiceData {
            questions: vec![crate::notify::QuestionData {
                multi: true,
                options: vec!["A".into(), "B".into()],
                header: None,
                question: "Q?".into(),
            }],
            questions_json: serde_json::json!([{
                "question": "Q?",
                "options": [{ "label": "A" }, { "label": "B" }],
                "multiSelect": true
            }]),
        };
        let (id, kb) = arm_request(
            &shared,
            "C:\\c.jsonl",
            NotifKind::Waiting,
            true,
            "q".into(),
            Some(choice),
        )
        .unwrap();
        let ActionKeyboard::Choice { questions, selected, .. } = &kb else {
            panic!("clavier Choice attendu");
        };
        assert!(questions[0].multi);
        assert_eq!(selected.len(), questions.len(), "invariant d'armement");
        let req = shared.pending.take(&id).unwrap();
        let PendingKind::Choice { questions, selected, .. } = &req.kind else {
            panic!("PendingKind::Choice attendu");
        };
        assert!(questions[0].multi);
        assert_eq!(selected.len(), questions.len(), "invariant d'armement");

        // Story 8.9 — lot de 3 questions : l'invariant tient aussi, et le clavier porte TOUT le lot.
        let lot = ChoiceData {
            questions: vec![
                crate::notify::QuestionData {
                    multi: false,
                    options: vec!["a".into()],
                    header: None,
                    question: "Q1?".into(),
                },
                crate::notify::QuestionData {
                    multi: true,
                    options: vec!["b".into(), "c".into()],
                    header: None,
                    question: "Q2?".into(),
                },
                crate::notify::QuestionData {
                    multi: false,
                    options: vec!["d".into()],
                    header: None,
                    question: "Q3?".into(),
                },
            ],
            questions_json: serde_json::json!([]),
        };
        let (id, kb) = arm_request(
            &shared,
            "C:\\d.jsonl",
            NotifKind::Waiting,
            true,
            "lot".into(),
            Some(lot),
        )
        .unwrap();
        let ActionKeyboard::Choice { questions, selected, .. } = &kb else {
            panic!("clavier Choice attendu");
        };
        assert_eq!(questions.len(), 3);
        assert_eq!(selected.len(), 3, "une sélection par question");
        assert!(selected.iter().all(|s| s.is_empty()));
        let _ = shared.pending.take(&id);
    }

    /// Story 8.4 — désarmement voie poste pour une réponse à question.
    #[test]
    fn send_message_desarme_le_clavier() {
        // request_resolved (côté commandes) = take_by_path + outcome AnsweredAtDesk.
        let table = PendingRequests::default();
        table.insert(PendingRequest {
            request_id: "run-q".into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: Instant::now() + Duration::from_secs(600),
            message: Some(MessageRef::new("99")),
            text: "Asked you a question".into(),
            kind: PendingKind::Choice {
                questions: vec![crate::notify::QuestionData {
                    multi: false,
                    options: vec!["yes".into()],
                    header: None,
                    question: "?".into(),
                }],
                selected: vec![BTreeSet::new()],
                questions_json: serde_json::json!([{ "question": "?", "options": [{ "label": "yes" }] }]),
            },
        });
        let taken = table.take_by_path("C:\\t.jsonl").unwrap();
        assert_eq!(taken.message, Some(MessageRef::new("99")));
        assert!(table.is_empty());
        assert_eq!(
            RequestOutcome::AnsweredAtDesk.suffix(),
            "Answered from Vigil"
        );
    }

    /// Repère de lisibilité pour `armement_reserve_un_id_seulement_pour_une_demande_actionnable` :
    /// STRICTEMENT au-dessus de la fenêtre par défaut et strictement sous la fenêtre mobile. C'est ce
    /// qui donne au test son pouvoir discriminant — un plancher qui ne sépare plus les deux valeurs
    /// resterait vert avec le mauvais TTL, précisément le cas qu'il doit interdire.
    ///
    /// ⚠️ **DÉRIVÉ, jamais littéral.** Le plancher a déjà été aveugle une fois : figé à 200 s pour un
    /// défaut de 120 s, il a survécu au passage du défaut à 600 s sans plus rien séparer (Story 8.8).
    /// Le couplage ci-dessous rend cette panne impossible à rejouer ; les deux asserts de compilation
    /// la rendent impossible à ignorer si les deux fenêtres se rapprochaient trop.
    const APPROVAL_TIMEOUT_TEST_FLOOR: u64 = crate::pipe::manager::APPROVAL_TIMEOUT_SECS + 100;
    const _: () = assert!(APPROVAL_TIMEOUT_TEST_FLOOR > crate::pipe::manager::APPROVAL_TIMEOUT_SECS);
    const _: () = assert!(APPROVAL_TIMEOUT_TEST_FLOOR < crate::pipe::manager::MOBILE_APPROVAL_TIMEOUT_SECS);

    // ── Story 8.5 : robustesse du polling ────────────────────────────────────────

    /// AC1 — le backoff double à chaque échec, plafonne, et RETOMBE À LA BASE après un succès.
    /// Non tautologique : exerce la vraie logique de `Backoff` (fonction pure isolée), pas un
    /// `assert!(true)` (avertissement de méthode 8.3).
    #[test]
    fn backoff_exponentiel_puis_reset_apres_succes() {
        let mut b = Backoff::new(POLL_BACKOFF_BASE_SECS, POLL_BACKOFF_MAX_SECS);
        // Premier essai : la base.
        assert_eq!(b.delay(), Duration::from_secs(1), "premier essai à la base");
        // Progression géométrique.
        b.advance();
        assert_eq!(b.delay(), Duration::from_secs(2));
        b.advance();
        assert_eq!(b.delay(), Duration::from_secs(4));
        b.advance();
        assert_eq!(b.delay(), Duration::from_secs(8));
        // Plafond : ne dépasse jamais le max, même en continuant à avancer. C'est la latence
        // maximale d'un tap après une reprise réseau silencieuse — d'où 15 s (revue 8.5).
        b.advance();
        assert_eq!(b.delay(), Duration::from_secs(15), "plafonné au max, pas 16");
        b.advance();
        assert_eq!(b.delay(), Duration::from_secs(15), "reste au plafond");
        // Un cycle de réception réussi (poll_actions Ok) réinitialise à la base.
        b.reset();
        assert_eq!(b.delay(), Duration::from_secs(1), "reset après succès");
    }

    /// AC2 — l'offset n'est jamais reculé : `ActionCursor::set` garde le max. Défensif contre un
    /// update_id inférieur (doublon rejoué, réordonnancement) qui re-demanderait des updates
    /// déjà dépassées.
    #[test]
    fn offset_monotone_ne_recule_jamais() {
        let mut cursor = ActionCursor::default();
        assert_eq!(cursor.get(), None, "vierge = aucun offset");
        cursor.set(50);
        assert_eq!(cursor.get(), Some(50));
        cursor.set(51);
        assert_eq!(cursor.get(), Some(51), "avance normale");
        // Valeur inférieure : ignorée, l'offset ne recule pas.
        cursor.set(10);
        assert_eq!(cursor.get(), Some(51), "jamais de recul");
        // Égalité : idempotent.
        cursor.set(51);
        assert_eq!(cursor.get(), Some(51));
    }

    /// AC1 — ce que `poll_loop` fait d'un tour de réception, testé là où c'est testable. Un
    /// succès n'attend PAS et ramène le backoff à la base ; un échec attend puis double.
    ///
    /// ⚠️ C'est ce test, et lui seul, qui verrouille le PLACEMENT de `reset`/`advance` :
    /// `poll_loop` exige un `AppHandle` et le coffre de l'OS, donc les inverser passerait
    /// inaperçu sur toute la suite (avertissement de méthode 8.3).
    #[test]
    fn un_tour_reussi_ne_dort_pas_et_reinitialise_le_backoff() {
        let mut b = Backoff::new(POLL_BACKOFF_BASE_SECS, POLL_BACKOFF_MAX_SECS);

        // Deux échecs : on attend, et le délai double (le sleep existe SUR CE CHEMIN).
        let echec: Result<Vec<ChannelAction>, String> = Err("network_error: timeout".into());
        assert_eq!(
            plan_next_attempt(&echec, &mut b),
            Some(Duration::from_secs(1)),
            "1er échec : la base"
        );
        assert_eq!(
            plan_next_attempt(&echec, &mut b),
            Some(Duration::from_secs(2)),
            "2e échec : doublé"
        );

        // Succès : AUCUNE attente (un long-poll vide bloque déjà ~25 s côté API) …
        let succes: Result<Vec<ChannelAction>, String> = Ok(Vec::new());
        assert_eq!(
            plan_next_attempt(&succes, &mut b),
            None,
            "un tour réussi n'ajoute jamais de sleep"
        );
        // … et le backoff est retombé à la base : l'échec suivant ne repart pas de 4 s.
        assert_eq!(
            plan_next_attempt(&echec, &mut b),
            Some(Duration::from_secs(1)),
            "reset après succès"
        );
    }

    /// Revue 8.5 — un changement de bot oublie le curseur, sinon l'offset hérité de l'ancien
    /// (bien plus haut que les `update_id` du nouveau) les filtrerait tous : plus aucun tap ne
    /// remonte, sans erreur affichée, jusqu'au redémarrage du processus. La monotonie de `set`
    /// (AC2) rend le trou définitif par construction — d'où un reset explicite.
    #[test]
    fn changement_de_bot_oublie_le_curseur() {
        let shared = Arc::new(Shared::default());

        // Un bot A a fait vivre le curseur haut dans sa séquence.
        shared.action_cursor.lock().unwrap().set(500_001);
        assert_eq!(shared.action_cursor.lock().unwrap().get(), Some(500_001));

        // Sans reset, la séquence basse du bot B serait ignorée (monotonie AC2) : c'est
        // exactement le piège. On vérifie d'abord qu'il est réel …
        shared.action_cursor.lock().unwrap().set(1);
        assert_eq!(
            shared.action_cursor.lock().unwrap().get(),
            Some(500_001),
            "la monotonie empêche seule toute reprise sur un nouveau bot"
        );

        // … puis que le reset l'ouvre.
        shared.reset_action_cursor();
        assert_eq!(
            shared.action_cursor.lock().unwrap().get(),
            None,
            "changement de bot = plus aucun point de reprise"
        );
        shared.action_cursor.lock().unwrap().set(1);
        assert_eq!(
            shared.action_cursor.lock().unwrap().get(),
            Some(1),
            "la séquence du nouveau bot est acceptée telle quelle"
        );
    }

    /// Revue 8.5 — une demande armée pendant le sommeil de backoff écourte ce sommeil : sans ça,
    /// l'utilisateur reçoit sa notification (l'ENVOI est un autre endpoint, il peut marcher pendant
    /// que `getUpdates` échoue), tape, et rien n'est reçu avant la fin du délai.
    #[tokio::test]
    async fn un_armement_reveille_le_poller_endormi() {
        let wake = Notify::new();

        // Ce que fait `ensure_poller` quand un armement trouve le poller endormi (le permis est
        // mémorisé même si personne n'attend encore — c'est la course qu'on doit couvrir).
        wake.notify_one();
        let debut = Instant::now();
        sleep_or_wake(Duration::from_secs(POLL_BACKOFF_MAX_SECS), &wake).await;
        assert!(
            debut.elapsed() < Duration::from_secs(1),
            "un armement doit écourter le sommeil, pas l'attendre ({:?})",
            debut.elapsed()
        );

        // Sans armement, le délai est bien observé : le réveil n'annule pas le backoff, il ne fait
        // que le court-circuiter quand il y a une raison.
        let debut = Instant::now();
        sleep_or_wake(Duration::from_millis(50), &wake).await;
        assert!(
            debut.elapsed() >= Duration::from_millis(50),
            "sans armement, le backoff s'applique en entier ({:?})",
            debut.elapsed()
        );
    }

    /// AC1 — un `429` prime sur le backoff : le canal a chiffré son délai, on l'honore. Sans ça
    /// on re-tapait l'API à 1 s quand le serveur demandait N s, ce qui prolonge la limitation
    /// (le chemin d'ENVOI l'honorait déjà, `retry_after_secs`). Revue 8.5.
    #[test]
    fn un_429_honore_le_retry_after_du_canal() {
        let mut b = Backoff::new(POLL_BACKOFF_BASE_SECS, POLL_BACKOFF_MAX_SECS);
        let limite: Result<Vec<ChannelAction>, String> = Err("rate_limited:12".into());
        assert_eq!(
            plan_next_attempt(&limite, &mut b),
            Some(Duration::from_secs(12)),
            "le délai du canal prime sur la base du backoff"
        );
        // Le backoff a tout de même avancé : une limitation qui persiste ne re-tape pas à 1 s.
        let autre: Result<Vec<ChannelAction>, String> = Err("api_error_500".into());
        assert_eq!(plan_next_attempt(&autre, &mut b), Some(Duration::from_secs(2)));
    }

    /// AC1 — les invariants de `Backoff` sont gardés par le constructeur, pas supposés par les
    /// constantes de production (revue 8.5).
    #[test]
    fn backoff_garde_ses_invariants_hors_constantes_de_prod() {
        // base = 0 : `advance` serait inerte (0 * 2 = 0) → boucle chaude sur l'API.
        let mut zero = Backoff::new(0, 30);
        assert_eq!(zero.delay(), Duration::from_secs(1), "base plancher à 1 s");
        zero.advance();
        assert_eq!(zero.delay(), Duration::from_secs(2), "la progression redémarre vraiment");

        // base > max : ni le premier délai ni le reset ne doivent dépasser le plafond annoncé.
        let mut inverse = Backoff::new(60, 30);
        assert_eq!(inverse.delay(), Duration::from_secs(30), "borné au plafond");
        inverse.advance();
        inverse.reset();
        assert_eq!(inverse.delay(), Duration::from_secs(30), "le reset ne repasse pas au-dessus");
    }

    /// AC3 — un doublon d'update ne re-résout PAS. La première réception consomme l'entrée (`take`,
    /// anti-rejeu 8.3) → `Resolve` ; une SECONDE action identique (même `request_id`) tombe sur une
    /// table vide → `Unknown`, donc AUCUN second `resolve` sur le pipe et l'accusé sera « Already
    /// handled » (mappé en `handle_action`), jamais un faux « ✅ Approved ». Testé au niveau pur
    /// `classify_action`, sans réseau.
    #[test]
    fn doublon_dupdate_ne_reresout_pas() {
        use super::super::{ActionRef, ApprovalDecision, IncomingKind};

        let pending = PendingRequests::default();
        pending.insert(PendingRequest {
            request_id: "run-1".into(),
            transcript_path: "C:\\t.jsonl".into(),
            expires_at: Instant::now() + Duration::from_secs(600),
            message: Some(MessageRef::new("50")),
            text: "x".into(),
            kind: PendingKind::ApproveDeny,
        });
        let action = ChannelAction {
            request_id: "run-1".into(),
            kind: IncomingKind::Approve,
            sender_id: "chat-1".into(),
            action_ref: ActionRef::new("cb-1"),
        };

        // Première réception : consomme l'entrée → Resolve(Approve).
        let first = classify_action(&pending, Some("chat-1"), &action, Instant::now());
        assert!(
            matches!(first, ActionVerdict::Resolve(_, ApprovalDecision::Approve)),
            "1re réception → Resolve"
        );
        assert!(pending.is_empty(), "l'entrée est consommée une seule fois (take)");

        // Doublon Telegram (même update rejouée) : plus rien dans la table → Unknown.
        let dup = classify_action(&pending, Some("chat-1"), &action, Instant::now());
        assert!(
            matches!(dup, ActionVerdict::Unknown),
            "un doublon ne refabrique aucune décision (pas de second resolve, accusé « Already handled »)"
        );
    }

    /// Unicité du poller + exclusion mutuelle avec l'appairage, à travers les VRAIS drapeaux et
    /// gardes RAII (`try_claim_poller`, `try_begin_pairing`, `PollerClaim`) — pas `is_empty`.
    ///
    /// ⚠️ Ce test ne couvre **que** ces deux garde-fous (#1 et #3), plus la survie du curseur au
    /// re-claim. Il ne dit rien de l'arrêt/redémarrage propre (`ensure_poller`, couvert par
    /// `poller_demarre_et_s_arrete_avec_la_table`) ni du désarmement sur résolution, et il ne
    /// touche pas au backoff : il ne peut donc pas prouver « le backoff ne casse rien », ce que
    /// son nom d'origine (`robustesse_preserve_les_quatre_garde_fous`) laissait croire. Prouver
    /// la non-régression sous backoff exigerait d'injecter un canal factice dans `poll_loop`
    /// (déféré, revue 8.5).
    #[test]
    fn claim_raii_unique_et_exclusif_avec_lappairage() {
        let shared = Arc::new(Shared::default());
        shared.pending.insert(demande("run-1"));

        // Garde-fou : un SEUL poller réservé à la fois (PollerClaim RAII, try_claim_poller).
        let claim = shared.try_claim_poller().expect("premier claim");
        assert!(shared.try_claim_poller().is_none(), "unicité du poller");

        // Garde-fou : exclusion mutuelle appairage ↔ boucle — refusé tant que le poller détient.
        assert!(
            shared.try_begin_pairing().is_none(),
            "appairage refusé pendant que le poller tourne"
        );

        // Le curseur avance comme le ferait un tour réussi de poll_actions.
        shared.action_cursor.lock().unwrap().set(10);

        // Garde-fou : arrêt propre — le drop de la garde RAII rend le drapeau (fin de poll_loop
        // ou panic), il ne reste jamais bloqué à `true`.
        drop(claim);
        assert!(!shared.poller_in_progress(), "le drapeau redescend au drop (RAII)");

        // Garde-fou : le curseur survit à la frontière de tâche — un re-claim (nouvelle tâche)
        // reprend le MÊME offset, ni perdu ni remis à zéro.
        let _claim2 = shared.try_claim_poller().expect("re-claim après release");
        assert_eq!(
            shared.action_cursor.lock().unwrap().get(),
            Some(10),
            "l'offset survit au re-spawn de la tâche"
        );

    }
}
