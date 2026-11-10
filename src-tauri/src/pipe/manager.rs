use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::oneshot;
use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;
use crate::agent::state::{AgentState, SourceTool};
use crate::agent::state_engine::{AgentStateEngine, HookSettings};
use crate::notify::manager::{NotifTrigger, NotifyManager};
use crate::notify::{
    choice_from_agent, is_mobile_actionable, notif_kind_for_transition, read_agent_notif_overrides,
    read_notif_settings, should_send, NotifKind, RequestOutcome,
};
use crate::permissions::{resolve_permission, AgentPermissionOverrides, ToolPermissionAction};
use crate::pipe::protocol::{HookEvent, ALLOW_RESPONSE, DENY_RESPONSE};
use crate::usage::manager::{FetchSource, UsageManager};
use crate::SETTINGS_STORE;

const PIPE_NAME: &str = r"\\.\pipe\vigil";
const MAX_INSTANCES: usize = 20;
// Timeout effectif de la décision de permission : passé ce délai, `PipeManager` renvoie un deny
// automatique. C'est le SEUL bouton effectif — les hooks porteurs (Claude/Codex `PermissionRequest`,
// Grok `PreToolUse`) ont un plafond bien plus haut (1800s) pour ne jamais mourir avant cet auto-deny.
//
// Story 8.8 — 120 s → 600 s. Ce timeout ne protège PAS l'utilisateur, il protège l'AGENT : sans lui,
// un agent attendrait indéfiniment une décision que personne ne prendra jamais. Cela justifie son
// EXISTENCE, jamais sa VALEUR. À 120 s, le paradoxe était que la seule catégorie de demande qui EXIGE
// la présence locale (multi-questions, texte libre, éviction : jamais armable pour le mobile, donc
// résoluble uniquement dans l'UI Vigil ou le terminal) était aussi la seule privée de la fenêtre
// élargie de la Story 8.3 — elle retombait ici. « Personne ne répondra » finit en auto-deny dans les
// deux cas ; « quelqu'un allait répondre » est perdu à 120 s et sauvé à 600 s.
//
// Occupation du named pipe : une demande en attente TIENT SA CONNEXION OUVERTE pendant toute la
// fenêtre (`handle_client` : le `timeout(window_secs, rx)` précède l'écriture de la réponse) → ×5 sur
// la durée d'occupation d'un slot face à `MAX_INSTANCES = 20`. Acceptable : `PendingApprovals::register`
// évince la demande précédente DU MÊME AGENT (deny explicite envoyé sur l'ancien `tx`, que le
// `handle_client` parqué reçoit immédiatement → il écrit sa réponse et rend son slot), donc le nombre
// de slots tenus vaut le nombre d'agents DISTINCTS réellement en attente — saturer exigerait 20 agents
// bloqués simultanément. La fenêtre mobile (1500 s) occupe déjà 12,5× ce défaut d'origine depuis la
// 8.3 sans que cela ait posé problème.
//
// ⚠️ Le deny d'éviction est explicite ET NON DROPPABLE : un simple `drop` du sender ferait tomber le
// `handle_client` évincé dans la branche `_ =>` du `timeout`, qui appelle `pa.cancel(&transcript_path)`
// — donc effacerait l'entrée de la NOUVELLE demande, celle qui vient d'être enregistrée sous la même
// clé. Ne jamais « simplifier » `register` en droppant l'ancien `tx`.
pub(crate) const APPROVAL_TIMEOUT_SECS: u64 = 600;

/// Story 8.3 — fenêtre d'attente ÉLARGIE, appliquée **uniquement** quand une notification mobile
/// actionnable est réellement attendue pour cet agent (cf. `approval_window_secs`).
///
/// Pourquoi pas un allongement global : `APPROVAL_TIMEOUT_SECS` ne protège pas l'utilisateur, il
/// protège l'AGENT — sans lui, un agent attend indéfiniment une décision que personne ne prendra
/// jamais. Imposer 25 min à quelqu'un qui n'a aucun canal appairé, c'est tout le coût sans aucun
/// bénéfice.
pub const MOBILE_APPROVAL_TIMEOUT_SECS: u64 = 1500;

/// Plafond du hook porteur d'un outil, en secondes — la valeur que `config/mod.rs` **installe**
/// réellement dans le fichier de configuration de cet outil. Toute fenêtre d'approbation doit rester
/// dessous : au-delà, le hook meurt avant l'auto-deny et Vigil répond dans un tuyau fermé.
///
/// Story 9.2 — ce n'était qu'une constante unique (1800) tant que les 4 outils partageaient le même
/// plafond. **Kimi le casse** : son schéma de hook borne `timeout` à `min(1).max(600)` (mesuré dans
/// le bundle JS de `kimi.exe`, `HookDefSchema`), et une valeur hors plage ne dégrade pas le hook —
/// elle fait échouer `validateConfig` sur la config **entière** (`CONFIG_INVALID`). 600 est donc un
/// plafond DUR d'outil, pas un réglage.
///
/// ⚠️ Le `match` est **exhaustif** : ajouter un variant à `SourceTool` casse la compilation ici,
/// jamais en silence. C'est le 1er des deux points d'arrêt (le 2nd est le test d'exhaustivité de
/// `ALL_SOURCE_TOOLS`).
///
/// ⚠️ **Antigravity est nominal** : son binaire est observe-only (capacité Approve/Deny tranchée
/// MORTE en 6.22), il n'émet JAMAIS de `PermissionRequest` et `config/mod.rs` ne lui installe
/// AUCUN `timeout` (`antigravity_canonical_element`). Aucune connexion n'est
/// donc jamais parquée pour lui et son plafond n'est jamais exercé — la valeur ci-dessous n'existe
/// que pour rendre le `match` exhaustif, et le test de cohérence « timeout installé ↔ plafond »
/// l'EXCLUT au lieu de lui inventer un `timeout`.
pub const fn hook_timeout_ceiling_secs(tool: &SourceTool) -> u64 {
    match tool {
        SourceTool::ClaudeCode | SourceTool::Codex | SourceTool::Grok => 1800,
        // Nominal — jamais exercé, cf. doc ci-dessus.
        SourceTool::Antigravity => 1800,
        // Plafond DUR d'outil (`HookDefSchema`: `timeout` ∈ [1, 600]).
        SourceTool::Kimi => 600,
        // Défaut de SÛRETÉ : un payload sans `source_tool` ne dit pas quel hook est au bout du
        // tuyau → on prend le plafond le plus BAS. Sans conséquence pratique (les 5 binaires
        // injectent tous `source_tool`), mais un jour où l'un l'oublierait, l'erreur serait une
        // fenêtre trop COURTE (visible : auto-deny prématuré) et non un dépassement de plafond
        // (invisible : chez Kimi, un hook tué par son timeout est fail-open = ALLOW silencieux).
        SourceTool::Unknown => 600,
    }
}

/// Marge réservée sous le plafond hook : démarrage du binaire hook, écriture de l'événement, lecture
/// de la réponse. Nommée pour n'exister qu'à un seul endroit — elle apparaissait en dur dans l'assert
/// de compilation ET dans ses tests miroirs.
///
/// Story 9.2 — 300 → 60 s (décision Steven, 2026-07-23), **après mesure** : un processus
/// `vigil-kimi` release complet (démarrage + résolution d'identité + retry) coûte 182 ms au relevé
/// de la 9.1, 17–65 ms au relevé de cette story. 60 s représente ~330× le pire cas mesuré. La marge
/// de 300 s était un chiffre de confort tant qu'elle se prélevait sur 1800 ; prélevée sur les 600 s
/// de Kimi, elle amputerait la moitié de la fenêtre utile.
pub const HOOK_STARTUP_MARGIN_SECS: u64 = 60;

/// Plancher d'utilisabilité d'une fenêtre d'approbation. **Pas décoratif** : sans lui, un futur
/// outil au plafond minuscule produirait une fenêtre de quelques secondes sans casser aucun assert —
/// l'agent serait auto-refusé avant même que l'utilisateur ait eu le temps de lire la demande.
pub const MIN_USABLE_WINDOW_SECS: u64 = 120;

/// Tous les variants de `SourceTool`. Sert au verrou de compilation ci-dessous (produit
/// outil × fenêtre). Son exhaustivité est prouvée par `all_source_tools_derive_du_match_exhaustif`.
pub const ALL_SOURCE_TOOLS: &[SourceTool] = &[
    SourceTool::ClaudeCode,
    SourceTool::Codex,
    SourceTool::Grok,
    SourceTool::Antigravity,
    SourceTool::Kimi,
    SourceTool::Unknown,
];

/// Fenêtre d'approbation **effective** : la fenêtre voulue, écrêtée au plafond du hook de l'outil
/// moins la marge de démarrage. Fonction **pure** et `const`.
///
/// Séparée de `approval_window_secs` à dessein : celle-ci exprime la POLITIQUE produit (mobile ou
/// non), celle-là la CONTRAINTE d'outil. Les mélanger rendrait la politique dépendante de l'outil.
pub const fn effective_approval_window_secs(tool: &SourceTool, window: u64) -> u64 {
    let ceiling = hook_timeout_ceiling_secs(tool) - HOOK_STARTUP_MARGIN_SECS;
    // `u64::min` n'est pas `const` — if/else explicite.
    if window < ceiling {
        window
    } else {
        ceiling
    }
}

/// ⚠️ Verrou de **COMPILATION**, généralisé à toutes les combinaisons (outil × fenêtre). Élargir une
/// fenêtre d'approbation au-delà du plafond installé, ou abaisser le plafond d'un outil sous sa
/// fenêtre, casse ici — avant même les tests.
///
/// ⚠️ **C'est le cœur du risque de l'Epic 9.** L'assert d'origine ne connaissait que `1800` : il
/// serait resté AU VERT en ajoutant Kimi, dont le plafond réel est 600. Et le dépassement chez Kimi
/// ne se voit pas — un hook tué par son `timeout` y est **fail-open** (`runHook` répond
/// `allowResult({timedOut:true})`) : l'appel passe **sans approbation**, aucune alarme.
///
/// Deux propriétés par paire :
/// 1. la fenêtre effective + la marge tiennent sous le plafond de l'outil (jamais de hook mort) ;
/// 2. la fenêtre effective reste au-dessus du plancher d'utilisabilité (jamais d'auto-deny éclair).
const _: () = {
    let mut i = 0;
    while i < ALL_SOURCE_TOOLS.len() {
        let tool = &ALL_SOURCE_TOOLS[i];
        let mut j = 0;
        while j < 2 {
            // Les DEUX branches de `approval_window_secs` — y compris le défaut, qui à 600 s
            // consomme 100 % du plafond de Kimi (ce n'est PAS que la fenêtre mobile qui déborde).
            let window = if j == 0 {
                APPROVAL_TIMEOUT_SECS
            } else {
                MOBILE_APPROVAL_TIMEOUT_SECS
            };
            let effective = effective_approval_window_secs(tool, window);
            assert!(effective + HOOK_STARTUP_MARGIN_SECS <= hook_timeout_ceiling_secs(tool));
            assert!(effective >= MIN_USABLE_WINDOW_SECS);
            j += 1;
        }
        i += 1;
    }
};

/// Faut-il PARQUER la connexion en attendant une décision ? Fonction **pure** (code review 8.8).
///
/// Non pour une demande **read-only** : le hook porteur a déjà fait `exit(0)` (Grok `default`/ask,
/// `AskUserQuestion` Grok — et demain Kimi en manual/auto), le frontend masque Approve/Deny et
/// `actionable` est faux. Personne, nulle part, ne peut résoudre l'entrée → l'attente irait TOUJOURS
/// au bout de la fenêtre, tenant un slot de pipe (`MAX_INSTANCES = 20`) pour un zéro-bénéfice. Le coût
/// a été multiplié par 5 en passant le défaut à 600 s, ce qui a rendu le cas visible.
pub fn should_await_approval(is_permission: bool, read_only: bool) -> bool {
    is_permission && !read_only
}

/// Fenêtre d'approbation effective, en secondes. Fonction **pure** — les trois conditions de l'AC6
/// réunies, et rien d'autre :
/// 1. la demande est **mobile-actionnable** (`is_mobile_actionable` : permission 8.3 **ou** choix 8.4) ;
/// 2. `should_send(Waiting, …)` rend `true` pour **cet** agent (résolution à 3 couches réutilisée
///    telle quelle, jamais réimplémentée) ;
/// 3. un destinataire est appairé.
///
/// Hors de ce cas, on rend exactement le comportement d'avant la Story 8.3.
pub fn approval_window_secs(actionable: bool, waiting_notified: bool, has_recipient: bool) -> u64 {
    if actionable && waiting_notified && has_recipient {
        MOBILE_APPROVAL_TIMEOUT_SECS
    } else {
        APPROVAL_TIMEOUT_SECS
    }
}

pub type ApprovalSender = oneshot::Sender<String>;

pub struct PendingApprovals {
    pending: HashMap<String, ApprovalSender>,
}

impl PendingApprovals {
    pub fn new() -> Self {
        Self { pending: HashMap::new() }
    }

    /// Rend `true` si une demande PRÉCÉDENTE du même agent a été évincée (et donc déniée). Story
    /// 8.3 : l'appelant s'en sert pour désarmer le message déjà parti au téléphone — le faire ici
    /// exigerait un `AppHandle` dans une structure qui n'a aucune raison d'en avoir un.
    pub fn register(&mut self, transcript_path: String, tx: ApprovalSender) -> bool {
        let evicted = if let Some(old_tx) = self.pending.remove(&transcript_path) {
            let _ = old_tx.send(r#"{"behavior":"deny"}"#.to_string());
            true
        } else {
            false
        };
        self.pending.insert(transcript_path, tx);
        evicted
    }

    pub fn cancel(&mut self, transcript_path: &str) {
        self.pending.remove(transcript_path);
    }

    pub fn resolve(&mut self, transcript_path: &str, response: String) -> bool {
        if let Some(tx) = self.pending.remove(transcript_path) {
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }
}

pub struct PipeManager;

impl PipeManager {
    pub fn start(
        app: AppHandle,
        engine: Arc<Mutex<AgentStateEngine>>,
        usage: Arc<UsageManager>,
        notify: Arc<NotifyManager>,
    ) {
        tauri::async_runtime::spawn(async move {
            if let Err(e) = Self::run(app, engine, usage, notify).await {
                eprintln!("PipeManager error: {e}");
            }
        });
    }

    async fn run(
        app: AppHandle,
        engine: Arc<Mutex<AgentStateEngine>>,
        usage: Arc<UsageManager>,
        notify: Arc<NotifyManager>,
    ) -> anyhow::Result<()> {
        loop {
            let server = match ServerOptions::new()
                .max_instances(MAX_INSTANCES)
                .create(PIPE_NAME)
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("PipeManager: create error: {e}, retrying...");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };

            if let Err(e) = server.connect().await {
                eprintln!("PipeManager: connect error: {e}, retrying...");
                continue;
            }

            let app_clone = app.clone();
            let engine_clone = Arc::clone(&engine);
            let usage_clone = Arc::clone(&usage);
            let notify_clone = Arc::clone(&notify);

            tauri::async_runtime::spawn(async move {
                if let Err(e) =
                    Self::handle_client(server, app_clone, engine_clone, usage_clone, notify_clone)
                        .await
                {
                    eprintln!("pipe client error: {e}");
                }
            });
        }
    }

    async fn handle_client(
        mut pipe: NamedPipeServer,
        app: AppHandle,
        engine: Arc<Mutex<AgentStateEngine>>,
        usage: Arc<UsageManager>,
        notify: Arc<NotifyManager>,
    ) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        const MAX_BUF_SIZE: usize = 1024 * 1024;

        // Read until we can parse a complete JSON event or reach EOF
        loop {
            match pipe.read(&mut tmp).await {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > MAX_BUF_SIZE {
                        eprintln!("pipe client: buffer overflow, disconnecting");
                        return Ok(());
                    }
                }
                Err(_) => return Ok(()),
            }

            if let Ok(event) = serde_json::from_slice::<HookEvent>(&buf) {
                // Story 6.14 — Codex : un SOUS-AGENT émet aussi UserPromptSubmit/PreToolUse/
                // PostToolUse, mais sous SON PROPRE transcript_path et avec un champ `agent_id`
                // (absent chez le parent). Via l'upsert des handlers, ces events créeraient un
                // agent fantôme de 1er niveau. On les ignore ici : seuls SubagentStart/SubagentStop
                // (routés vers le parent par session_id dans le state engine) matérialisent le
                // sous-agent. SubagentStart/Stop portent aussi `agent_id` mais NE sont PAS filtrés.
                if is_subagent_scoped_phantom(&event, &buf) {
                    return Ok(());
                }
                let transcript_path = get_transcript_path(&event);
                let is_permission = matches!(event, HookEvent::PermissionRequest(_));

                // Résolution auto-permission (Approve/Deny) AVANT apply_event pour garantir
                // que l'agent ne transite jamais vers waitingApprove dans le snapshot (AC 4.11)
                if is_permission {
                    if let HookEvent::PermissionRequest(ref p) = event {
                        // Story 6.30 (Grok) : un `PermissionRequest` **read-only** (mode `default`/ask,
                        // `PermMode::Inform`) est NON bloquant côté hook (vigil-grok a déjà `exit 0`) →
                        // l'auto-permission serait SANS effet (réponse sur pipe fermé) ET masquerait
                        // l'overlay d'awareness. On la SKIP → l'agent passe en `waitingApprove` read-only
                        // et l'utilisateur voit qu'une permission est en attente (réponse au terminal).
                        // `read_only` est false pour Claude/Codex/Antigravity + Grok `bypassPermissions`
                        // (actionnables) → aucune régression.
                        if p.tool_name != "AskUserQuestion" && !p.read_only {
                            let global = read_global_permissions(&app);
                            let overrides = read_agent_rules(&app, &p.transcript_path);
                            let action = resolve_permission(p.source_tool.clone(), &p.tool_name, &overrides, &global);

                            let auto_response = match action {
                                ToolPermissionAction::Approve => {
                                    Some((ALLOW_RESPONSE, format!("Auto-approved: {}", p.tool_name)))
                                }
                                ToolPermissionAction::Deny => {
                                    Some((DENY_RESPONSE, format!("Auto-denied: {}", p.tool_name)))
                                }
                                ToolPermissionAction::Manual => None,
                            };

                            if let Some((response, label)) = auto_response {
                                {
                                    let hook_settings = read_hook_settings(&app);
                                    let mut eng = engine.lock().unwrap();
                                    eng.set_settings(hook_settings);
                                    eng.set_auto_permission_label(
                                        &p.transcript_path, &p.session_id, &p.cwd, p.parent_pid, &label,
                                    );
                                    let _ = eng.emit_snapshot(&app);
                                }
                                let response_line = format!("{response}\n");
                                if pipe.write_all(response_line.as_bytes()).await.is_err() {
                                    return Ok(());
                                }
                                let mut drain = [0u8; 1];
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    pipe.read(&mut drain),
                                ).await;
                                return Ok(());
                            }
                            // action == Manual : continuer avec le flux PendingApproval ci-dessous
                        }
                    }
                }

                // Capture de la TRANSITION d'état (Story 8.1) : `apply_event` mute en place et ne
                // retourne rien, donc l'état PRÉCÉDENT doit être lu juste avant l'appel. C'est le
                // seul endroit correct — changer la signature d'`apply_event` ferait rippler tout
                // `state_engine.rs` et ses ~90 tests.
                //
                // ⚠️ Aucune I/O (store, coffre, réseau) ne doit avoir lieu en tenant ce verrou :
                // il est sur le chemin critique de TOUS les hooks de TOUS les agents. On ne fait
                // que construire un déclencheur, consommé une fois le verrou relâché.
                // Story 8.3/8.4 — `actionable` (= mobile) calculé ICI, dans le MÊME verrou : seul
                // endroit qui tient l'instantané complet. `is_mobile_actionable` / `choice_from_agent`
                // sont pures : aucune I/O sous le verrou.
                let (trigger, actionable, left_waiting) = {
                    let hook_settings = read_hook_settings(&app);
                    let mut eng = engine.lock().unwrap();
                    eng.set_settings(hook_settings);
                    let prev = eng.agents.get(&transcript_path).map(|a| a.state.clone());
                    eng.apply_event(&event);
                    let _ = eng.emit_snapshot(&app);
                    let agent = eng.agents.get(&transcript_path);
                    let actionable = agent.is_some_and(is_mobile_actionable);
                    let left_waiting = agent_left_waiting(prev.as_ref(), agent.map(|a| &a.state));
                    let choice = agent.and_then(choice_from_agent);
                    let trigger = agent.and_then(|a| {
                        notif_kind_for_transition(prev, a.state.clone(), was_interrupt(&event)).map(
                            |kind| NotifTrigger {
                                transcript_path: transcript_path.clone(),
                                kind,
                                agent_label: a.name.clone(),
                                tool: a.source_tool.clone(),
                                summary: notif_summary(kind, a),
                                actionable,
                                choice,
                            },
                        )
                    });
                    (trigger, actionable, left_waiting)
                };

                // Résolution dans le TERMINAL : l'agent a quitté l'attente POUR REPRENDRE son
                // travail sans qu'aucune voie explicite (poste/mobile) n'ait désarmé son message
                // (les sorties anormales — interrupt/fin/erreur — sont exclues, cf.
                // `agent_left_waiting`, et restent en « Expired »). On retire ses boutons et on
                // l'étiquette « Handled in the terminal ». Fallback once-only (cf. `agent_left_waiting`) :
                // no-op si le poste/mobile a déjà pris l'entrée. Hors verrou (I/O interdite dessus) ;
                // fire-and-forget (`take_by_path` synchrone + édition détachée).
                if left_waiting {
                    notify.request_resolved(&transcript_path, RequestOutcome::HandledInTerminal);
                }

                // Story 8.3 — l'enregistrement précède la notification : sinon l'éviction d'une
                // demande précédente (et le désarmement de SON message) pourrait retirer l'entrée
                // que le manager vient tout juste d'armer pour la NOUVELLE demande.
                // Code review 8.8 — une demande **read-only** n'est PAS attendable. Le hook porteur a
                // déjà fait `exit(0)` (Grok mode `default`/ask, `AskUserQuestion` Grok — et demain
                // Kimi en manual/auto), `actionable` est faux et le frontend masque Approve/Deny :
                // personne, nulle part, ne peut résoudre cette entrée. Parquer la connexion revient
                // donc à tenir un slot de pipe pendant TOUTE la fenêtre pour un zéro-bénéfice —
                // 600 s depuis cette story, contre 120 s avant. On n'enregistre rien et on n'attend
                // pas ; l'agent reste en `waitingApprove` read-only (awareness), résolution au terminal.
                let read_only =
                    matches!(&event, HookEvent::PermissionRequest(p) if p.read_only);
                let approval_rx = if should_await_approval(is_permission, read_only) {
                    let (tx, rx) = oneshot::channel::<String>();
                    let evicted = {
                        let pa_state = app.state::<Mutex<PendingApprovals>>();
                        let mut pa = pa_state.lock().unwrap();
                        pa.register(transcript_path.clone(), tx)
                    };
                    if evicted {
                        // Une nouvelle demande du même agent remplace l'ancienne : le message déjà
                        // reçu au téléphone ne doit plus porter de boutons (AC7). Fire-and-forget.
                        notify.request_resolved(&transcript_path, RequestOutcome::Superseded);
                    }
                    Some(rx)
                } else {
                    // Une demande read-only évince quand même la précédente du MÊME agent : un
                    // `AskUserQuestion` (toujours read-only) peut suivre une permission actionnable
                    // chez un Grok en `bypassPermissions`. Sans ce deny explicite, l'ancienne resterait
                    // parquée jusqu'à sa propre expiration et son message téléphone garderait ses
                    // boutons. Sémantique identique à `register`, sans le nouvel enregistrement.
                    if read_only {
                        let evicted = {
                            let pa_state = app.state::<Mutex<PendingApprovals>>();
                            let mut pa = pa_state.lock().unwrap();
                            pa.resolve(&transcript_path, DENY_RESPONSE.to_string())
                        };
                        if evicted {
                            notify.request_resolved(&transcript_path, RequestOutcome::Superseded);
                        }
                    }
                    None
                };

                // Une transition Waiting *réellement notifiable* (pas une éviction
                // WaitingApprove→WaitingApprove où `notif_kind_for_transition` rend `None`).
                // Sans ce drapeau, le 2ᵉ PermissionRequest parallèle obtenait 1500 s sans jamais
                // produire de notification ni d'armement — l'utilisateur voyait « Replaced… »
                // et jamais la demande de remplacement (violation du « ssi » de l'AC6).
                let waiting_will_notify = trigger
                    .as_ref()
                    .is_some_and(|t| t.kind == NotifKind::Waiting);

                if let Some(trigger) = trigger {
                    // Fire-and-forget : un envoi sur un canal mpsc, rien d'autre. Aucun `await`
                    // réseau, aucune lecture de store, aucune erreur possible → le pipe rend la
                    // main immédiatement, notifications activées OU NON. L'enrichissement du
                    // libellé (nom personnalisé) et tout le reste se font dans `run_loop`, APRÈS
                    // le filtrage : désactivées, elles ne coûtent pas une I/O de plus ici.
                    notify.trigger(trigger);
                }

                if triggers_usage_refresh(&event) {
                    // Rafraîchit tous les outils suivis (au moins Claude ; Codex si activé).
                    usage.refresh_all(FetchSource::AgentEnded);
                }

                if let Some(rx) = approval_rx {
                    // Fenêtre adaptative (AC6). Les lectures de store n'ont lieu QUE sur une
                    // demande actionnable **et notifiée** — donc jamais sur le chemin d'un agent
                    // sans permission manuelle, ni sur une éviction sans nouveau trigger — et
                    // JAMAIS le coffre de l'OS : `chat_id` suffit à prouver l'appairage (seul
                    // `notify_confirm_pairing` l'écrit, `notify_disconnect` l'efface).
                    let requested_window_secs = if actionable && waiting_will_notify {
                        let settings = read_notif_settings(&app);
                        let overrides = read_agent_notif_overrides(&app, &transcript_path);
                        approval_window_secs(
                            true,
                            should_send(NotifKind::Waiting, &settings, &overrides),
                            settings.chat_id.as_deref().is_some_and(|c| !c.is_empty()),
                        )
                    } else {
                        APPROVAL_TIMEOUT_SECS
                    };

                    // Story 9.2 — écrêtage par outil, **POINT UNIQUE**, en AVAL de la politique
                    // ci-dessus (qui reste pure et inchangée). Il couvre donc les DEUX branches, y
                    // compris le défaut `APPROVAL_TIMEOUT_SECS` : c'est lui qui, à 600 s, consomme
                    // 100 % du plafond de Kimi — pas seulement la fenêtre mobile de 1500 s.
                    // Sur Kimi la fenêtre effective vaut 540 s dans les deux cas ; sur les autres
                    // outils (plafond 1800) elle est INCHANGÉE (600 / 1500).
                    //
                    // `approval_rx` n'est `Some` que si `should_await_approval` → `is_permission` →
                    // l'event EST un `PermissionRequest`. Le repli `Unknown` est donc inatteignable,
                    // et sûr par construction s'il le devenait (plafond le plus bas).
                    let hook_tool = match &event {
                        HookEvent::PermissionRequest(p) => p.source_tool.clone(),
                        _ => SourceTool::Unknown,
                    };
                    let window_secs =
                        effective_approval_window_secs(&hook_tool, requested_window_secs);

                    let response = match tokio::time::timeout(
                        std::time::Duration::from_secs(window_secs),
                        rx,
                    )
                    .await
                    {
                        Ok(Ok(resp)) => resp,
                        _ => {
                            // Timeout ou sender dropped : nettoyer PendingApprovals, deny automatique.
                            // L'état de l'agent reste inchangé — Claude Code reprend après le deny
                            // et émettra de nouveaux hooks qui mettront l'état à jour naturellement.
                            {
                                let pa_state = app.state::<Mutex<PendingApprovals>>();
                                let mut pa = pa_state.lock().unwrap();
                                pa.cancel(&transcript_path);
                            }
                            // Le message parti au téléphone porte encore ses boutons : les retirer
                            // (AC7). L'anti-rejeu couvrirait un tap tardif de toute façon.
                            notify.request_resolved(&transcript_path, RequestOutcome::Expired);
                            DENY_RESPONSE.to_string()
                        }
                    };

                    // les binaires hook (vigil-claude/codex/grok) utilisent read_line() → \n requis pour terminer la lecture
                    let response_line = format!("{response}\n");
                    if pipe.write_all(response_line.as_bytes()).await.is_err() {
                        let pa_state = app.state::<Mutex<PendingApprovals>>();
                        pa_state.lock().unwrap().cancel(&transcript_path);
                        return Ok(());
                    }

                    // Sur Windows, CloseHandle() sur un NamedPipeServer détruit les données
                    // encore non lues dans le buffer. On maintient la connexion ouverte
                    // jusqu'à ce que le binaire hook se déconnecte (après avoir lu la réponse).
                    let mut drain = [0u8; 1];
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        pipe.read(&mut drain),
                    ).await;
                }

                return Ok(());
            }
        }
    }
}

fn read_global_permissions(app: &AppHandle) -> HashMap<crate::permissions::ToolPermissionKey, ToolPermissionAction> {
    let store = app.store(SETTINGS_STORE).ok();
    store
        .as_ref()
        .and_then(|s| s.get("toolPermissions"))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn read_agent_rules(app: &AppHandle, transcript_path: &str) -> AgentPermissionOverrides {
    let store = app.store(SETTINGS_STORE).ok();
    let key = format!("agentRules:{}", transcript_path);
    store
        .as_ref()
        .and_then(|s| s.get(&key))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn read_hook_settings(app: &AppHandle) -> HookSettings {
    let store = app.store(SETTINGS_STORE).ok();
    HookSettings {
        show_subagents: store
            .as_ref()
            .and_then(|s| s.get("showSubagents"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        auto_cleanup_disconnected: store
            .as_ref()
            .and_then(|s| s.get("autoCleanupDisconnected"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        hide_notch_if_empty: store
            .as_ref()
            .and_then(|s| s.get("hideNotchIfEmpty"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// `true` si l'événement est une INTERRUPTION déclenchée par l'utilisateur. Un agent qui finit
/// parce qu'on l'a coupé ne mérite pas de notification : l'utilisateur était au clavier.
///
/// ⚠️ Les DEUX branches sont nécessaires, et c'est la seconde qui porte le cas réel : le bouton
/// « Interrompre » de Vigil écrit un flag que `vigil_hook_core::handle_interrupt_flag_claude_code`
/// consomme en émettant un `Stop` **synthétique** — pas un `PostToolUseFailure`. La première
/// branche n'est levée par **aucun** outil aujourd'hui (Grok l'omet volontairement, Antigravity
/// n'a pas le champ) ; elle reste pour les outils qui l'émettraient un jour.
fn was_interrupt(event: &HookEvent) -> bool {
    match event {
        HookEvent::PostToolUseFailure(p) => p.is_interrupt,
        HookEvent::Stop(p) => p.interrupted,
        _ => false,
    }
}

/// L'agent vient-il de RÉSOUDRE sa demande dans le terminal — c.-à-d. quitter un état d'attente
/// (`WaitingApprove`/`WaitingSpecific`) pour REPRENDRE son travail (`Working`/`Idle`) ?
///
/// Sert de FALLBACK de désarmement du message mobile quand la demande est résolue **dans le
/// terminal** : aucune voie explicite (approve/deny/send_message au poste, tap au téléphone)
/// n'a alors désarmé le clavier, et sans ça il survivrait jusqu'à l'auto-deny (~25 min).
///
/// ⚠️ On exige que l'état courant soit une REPRISE (`Working`/`Idle`), PAS n'importe quelle sortie
/// d'attente. Les sorties ANORMALES — interruption ou fin de session (`Ended`), erreur (`Error`),
/// déconnexion (`Disconnected`) ou disparition de l'agent (`None`) — ne sont PAS des résolutions :
/// rien n'a été traité. Les inclure produirait un « Handled in the terminal » MENSONGER (l'action
/// d'interruption est d'ailleurs faite dans Vigil, pas au terminal). On les laisse plutôt au
/// handler parqué, qui les étiquette correctement « Expired — nothing was executed » à son
/// timeout — cohérent avec la voie abrupte (pid_watcher), hors `handle_client` (revue 8.7).
///
/// ⚠️ Ce prédicat est vrai AUSSI sur les résolutions poste/téléphone (leur hook de suite —
/// PostToolUse — fait aussi reprendre le travail). C'est sans danger : `request_resolved` est
/// once-only (`take_by_path` synchrone), et la voie explicite a DÉJÀ pris l'entrée au moment de
/// la décision, bien avant que le CLI n'exécute l'outil et n'émette ce hook de suite → l'appel
/// fallback ne trouve rien et l'étiquette d'origine (« Approved from Vigil », …) est préservée.
fn agent_left_waiting(prev: Option<&AgentState>, current: Option<&AgentState>) -> bool {
    let is_waiting =
        |s: &AgentState| matches!(s, AgentState::WaitingApprove | AgentState::WaitingSpecific);
    let is_resumed = |s: &AgentState| matches!(s, AgentState::Working | AgentState::Idle);
    prev.is_some_and(is_waiting) && current.is_some_and(is_resumed)
}

/// Résumé d'action embarqué dans la notification.
///
/// Pour un `Waiting`, `last_activity_label` serait **faux** : `PermissionRequest` ne l'écrit pas
/// (cf. `state_engine.rs`), il porte donc encore le libellé du `PreToolUse` PRÉCÉDENT. La
/// notification qui vous demande d'agir décrirait ainsi l'action d'avant. On lit le nom de
/// l'outil réellement demandé — plus exact, et accessoirement sans chemin ni argument.
fn notif_summary(kind: NotifKind, agent: &crate::agent::state::AgentSnapshot) -> Option<String> {
    match kind {
        NotifKind::Waiting => agent
            .permission_request
            .as_ref()
            .map(|r| {
                // Story 8.4 — résumé honnête pour une question (pas « Wants to run: AskUserQuestion »).
                // Story 8.9 — et honnête sur le NOMBRE. Tableau absent / vide / malformé → « a
                // question » (comportement 8.4 préservé) : un résumé n'a pas à deviner.
                if r.tool_name == "AskUserQuestion" {
                    let n = r
                        .tool_input
                        .get("questions")
                        .and_then(|q| q.as_array())
                        .map_or(0, Vec::len);
                    if n > 1 {
                        format!("Asked you {n} questions")
                    } else {
                        "Asked you a question".to_string()
                    }
                } else {
                    format!("Wants to run: {}", r.tool_name)
                }
            })
            .or_else(|| agent.last_activity_label.clone()),
        _ => agent.last_activity_label.clone(),
    }
}

fn triggers_usage_refresh(event: &HookEvent) -> bool {
    match event {
        HookEvent::Stop(_) => true,
        // Story 6.34 (review) — un `StopFailure` porte typiquement une erreur de quota (rate-limit
        // 429), soit précisément le signal d'épuisement d'usage : rafraîchir la fenêtre, comme sur
        // `Stop`. Cross-outil (bénéficie aussi à Claude, seul émetteur jusqu'à 6.34).
        HookEvent::StopFailure(_) => true,
        HookEvent::PostToolUseFailure(p) => p.is_interrupt,
        _ => false,
    }
}

fn get_transcript_path(event: &HookEvent) -> String {
    match event {
        HookEvent::SessionStart(p) => p.transcript_path.clone(),
        HookEvent::SessionEnd(p) => p.transcript_path.clone(),
        HookEvent::PreToolUse(p) => p.transcript_path.clone(),
        HookEvent::PermissionRequest(p) => p.transcript_path.clone(),
        HookEvent::PostToolUse(p) => p.transcript_path.clone(),
        HookEvent::PostToolUseFailure(p) => p.transcript_path.clone(),
        HookEvent::Notification(p) => p.transcript_path.clone(),
        HookEvent::Stop(p) => p.transcript_path.clone(),
        HookEvent::StopFailure(p) => p.transcript_path.clone(),
        HookEvent::UserPromptSubmit(p) => p.transcript_path.clone(),
        HookEvent::SubagentStart(p) => p.transcript_path.clone(),
        HookEvent::SubagentStop(p) => p.transcript_path.clone(),
    }
}

/// Story 6.14 — Détecte un event de cycle de tour (`UserPromptSubmit`/`PreToolUse`/`PostToolUse`)
/// émis par un SOUS-AGENT Codex, reconnaissable à un champ `agent_id` non vide dans le payload
/// brut (le parent n'en porte pas). Ces events ne doivent PAS créer d'agent de 1er niveau
/// (bug fantôme : chaque sous-agent Codex a son propre transcript_path → upsert d'un faux agent).
///
/// `SubagentStart`/`SubagentStop` portent aussi `agent_id` mais renvoient volontairement `false` :
/// ils sont routés vers l'agent parent (par `session_id`) par le state engine.
/// `agent_id` est lu sur le JSON brut : les structs de payload ne le déclarent pas (les 3 outils
/// partagent `protocol.rs`, on évite un champ transverse + la maintenance de 16 constructions).
///
/// Borné à `source_tool == Codex` : le bug fantôme est propre au modèle sous-agent de Codex.
/// Claude/Grok n'émettent pas d'`agent_id` sur ces events aujourd'hui — mais si l'un le faisait un
/// jour, le filtrer ici droperait silencieusement ses events (agent figé). La garde préserve leur
/// chemin byte-identique et court-circuite aussi le re-parse JSON pour eux.
fn is_subagent_scoped_phantom(event: &HookEvent, raw: &[u8]) -> bool {
    let source_tool = match event {
        HookEvent::UserPromptSubmit(p) => &p.source_tool,
        HookEvent::PreToolUse(p) => &p.source_tool,
        HookEvent::PostToolUse(p) => &p.source_tool,
        _ => return false,
    };
    if *source_tool != SourceTool::Codex {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("agent_id")
                .and_then(|a| a.as_str())
                .map(|s| !s.is_empty())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Payload sous-agent Codex réel (capture story 6.14) : porte `agent_id`.
    const SUB_PRETOOL: &str = r#"{"hook_event_name":"PreToolUse","session_id":"019f3165","turn_id":"t","agent_id":"019f3168-027b","agent_type":"worker","transcript_path":"C:\\sub.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{"command":"Start-Sleep -Seconds 15"},"source_tool":"codex"}"#;
    // Payload parent Codex : PAS de `agent_id`.
    const PARENT_PROMPT: &str = r#"{"hook_event_name":"UserPromptSubmit","session_id":"019f3165","turn_id":"t","transcript_path":"C:\\parent.jsonl","cwd":"C:\\","prompt":"Bonjour","source_tool":"codex"}"#;
    // SubagentStart : porte `agent_id` mais NE doit PAS être filtré (routé vers le parent).
    const SUBAGENT_START: &str = r#"{"hook_event_name":"SubagentStart","session_id":"019f3165","transcript_path":"C:\\sub.jsonl","cwd":"C:\\","agent_id":"019f3168-027b","agent_type":"worker"}"#;

    fn parse(raw: &str) -> HookEvent {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn phantom_vrai_pour_pretooluse_de_sous_agent() {
        assert!(is_subagent_scoped_phantom(&parse(SUB_PRETOOL), SUB_PRETOOL.as_bytes()));
    }

    #[test]
    fn phantom_faux_pour_userpromptsubmit_parent() {
        assert!(!is_subagent_scoped_phantom(&parse(PARENT_PROMPT), PARENT_PROMPT.as_bytes()));
    }

    #[test]
    fn phantom_faux_pour_subagent_start_meme_avec_agent_id() {
        // SubagentStart porte agent_id mais doit passer (routé vers le parent, pas un fantôme).
        assert!(!is_subagent_scoped_phantom(&parse(SUBAGENT_START), SUBAGENT_START.as_bytes()));
    }

    #[test]
    fn phantom_faux_pour_agent_id_vide() {
        let raw = r#"{"hook_event_name":"PreToolUse","session_id":"s","agent_id":"","transcript_path":"C:\\t.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{},"source_tool":"codex"}"#;
        assert!(!is_subagent_scoped_phantom(&parse(raw), raw.as_bytes()));
    }

    #[test]
    fn phantom_faux_pour_pretooluse_claude_meme_avec_agent_id() {
        // Non-régression : un event Claude porteur d'`agent_id` NE doit PAS être filtré.
        // La garde `source_tool == Codex` protège Claude/Grok d'un drop silencieux.
        let raw = r#"{"hook_event_name":"PreToolUse","session_id":"s","turn_id":"t","agent_id":"sub-1","transcript_path":"C:\\sub.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{},"source_tool":"claude_code"}"#;
        assert!(!is_subagent_scoped_phantom(&parse(raw), raw.as_bytes()));
    }

    /// Story 8.1 — le drapeau qui étouffe la notification de fin sur interruption utilisateur.
    /// Un `Stop` normal ou un échec d'outil ordinaire doivent rester notifiables.
    #[test]
    fn was_interrupt_seulement_sur_interruption_utilisateur() {
        let interrupt = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","transcript_path":"C:\\t.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{},"is_interrupt":true,"source_tool":"claude_code"}"#;
        let echec = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","transcript_path":"C:\\t.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{},"is_interrupt":false,"source_tool":"claude_code"}"#;
        let stop = r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"C:\\t.jsonl","cwd":"C:\\","source_tool":"claude_code"}"#;

        assert!(was_interrupt(&parse(interrupt)));
        assert!(!was_interrupt(&parse(echec)));
        assert!(!was_interrupt(&parse(stop)));
    }

    /// ⚠️ Revue 8.1 — LE cas réel d'interruption, celui qui manquait. Le bouton « Interrompre »
    /// de Vigil ne produit PAS de `PostToolUseFailure` : hook-core émet un `Stop` **synthétique**
    /// marqué `interrupted` (`send_synthetic_stop`). Sans cette branche, l'utilisateur qui coupe
    /// son agent reçoit « ✅ terminé » sur son téléphone.
    #[test]
    fn was_interrupt_sur_stop_synthetique_du_bouton_interrompre() {
        let synthetique = r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"C:\\t.jsonl","cwd":"C:\\","source_tool":"claude_code","interrupted":true}"#;
        assert!(was_interrupt(&parse(synthetique)));

        // Contrat de compatibilité : un hook pas encore réinstallé omet le champ → `false` →
        // on notifie (comportement d'avant). Dégradation, jamais de casse.
        let ancien = r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"C:\\t.jsonl","cwd":"C:\\","source_tool":"claude_code"}"#;
        assert!(!was_interrupt(&parse(ancien)));
    }

    /// Revue 8.1 — la notification `Waiting` doit décrire la demande EN COURS, pas l'action
    /// précédente : `PermissionRequest` n'écrit pas `last_activity_label`, qui porte donc encore
    /// le libellé du `PreToolUse` d'avant.
    #[test]
    fn resume_du_waiting_decrit_la_demande_pas_l_action_precedente() {
        use crate::agent::state::{AgentSnapshot, PermissionRequestInfo};

        let mut agent: AgentSnapshot = serde_json::from_value(serde_json::json!({
            "transcriptPath": "C:\\t.jsonl", "name": "vigil", "state": "waitingApprove",
            "lastActivity": 0, "source": "Terminal", "sourceTool": "claude_code",
        }))
        .unwrap();
        agent.last_activity_label = Some("Editing C:\\p\\config.ts".into());
        agent.permission_request = Some(PermissionRequestInfo {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
            read_only: false,
        });

        assert_eq!(
            notif_summary(NotifKind::Waiting, &agent),
            Some("Wants to run: Bash".to_string())
        );
        // Les autres types gardent le libellé d'activité.
        assert_eq!(
            notif_summary(NotifKind::Ended, &agent),
            Some("Editing C:\\p\\config.ts".to_string())
        );

        // Story 8.4 — question → résumé honnête.
        agent.permission_request = Some(PermissionRequestInfo {
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({ "questions": [] }),
            read_only: false,
        });
        assert_eq!(
            notif_summary(NotifKind::Waiting, &agent),
            Some("Asked you a question".to_string())
        );

        // Story 8.9 — honnête sur le NOMBRE aussi.
        agent.permission_request = Some(PermissionRequestInfo {
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({ "questions": [
                { "question": "Q1?" }, { "question": "Q2?" }, { "question": "Q3?" }
            ] }),
            read_only: false,
        });
        assert_eq!(
            notif_summary(NotifKind::Waiting, &agent),
            Some("Asked you 3 questions".to_string())
        );
    }

    #[test]
    fn resume_question_est_honnete() {
        use crate::agent::state::{AgentSnapshot, PermissionRequestInfo};
        let mut agent: AgentSnapshot = serde_json::from_value(serde_json::json!({
            "transcriptPath": "C:\\t.jsonl", "name": "vigil", "state": "waitingSpecific",
            "lastActivity": 0, "source": "Terminal", "sourceTool": "claude_code",
        }))
        .unwrap();
        agent.permission_request = Some(PermissionRequestInfo {
            tool_name: "AskUserQuestion".into(),
            tool_input: serde_json::json!({}),
            read_only: false,
        });
        assert_eq!(
            notif_summary(NotifKind::Waiting, &agent).as_deref(),
            Some("Asked you a question")
        );
        assert!(!notif_summary(NotifKind::Waiting, &agent)
            .unwrap()
            .contains("Wants to run"));

        // Story 8.9, AC10 — le résumé dit la vérité sur le nombre. Les formes dégradées
        // (tableau absent ci-dessus, vide, malformé) gardent le libellé 8.4 : un résumé ne devine
        // pas.
        let cas: [(serde_json::Value, &str); 5] = [
            (serde_json::json!({ "questions": [] }), "Asked you a question"),
            (serde_json::json!({ "questions": "pas un tableau" }), "Asked you a question"),
            (serde_json::json!({ "questions": [{ "question": "Q1?" }] }), "Asked you a question"),
            (
                serde_json::json!({ "questions": [{ "question": "Q1?" }, { "question": "Q2?" }] }),
                "Asked you 2 questions",
            ),
            (
                serde_json::json!({ "questions": [{}, {}, {}, {}] }),
                "Asked you 4 questions",
            ),
        ];
        for (tool_input, attendu) in cas {
            agent.permission_request = Some(PermissionRequestInfo {
                tool_name: "AskUserQuestion".into(),
                tool_input: tool_input.clone(),
                read_only: false,
            });
            assert_eq!(
                notif_summary(NotifKind::Waiting, &agent).as_deref(),
                Some(attendu),
                "{tool_input}"
            );
        }
    }

    #[test]
    fn timeout_long_aussi_pour_question_armee() {
        // La fenêtre longue dépend de `actionable` (maintenant mobile-actionnable), pas du type
        // de clavier. Une question armée + notif + destinataire → 1500 s comme une permission.
        assert_eq!(approval_window_secs(true, true, true), MOBILE_APPROVAL_TIMEOUT_SECS);
        // Question non armable → actionable false → 600 s. ⚠️ Story 8.9 : « multi-questions » n'est
        // PLUS un exemple de non-armable (c'est même le cas qui gagne la fenêtre longue) ; les
        // exemples qui restent sont read_only et le texte libre pur (8.6 différée).
        assert_eq!(approval_window_secs(false, true, true), APPROVAL_TIMEOUT_SECS);
    }

    // ══ Story 8.3 : fenêtre d'approbation adaptative (AC6) ═══════════════════════

    /// Les TROIS conditions, une par une. Chacune manquante ramène à la fenêtre par DÉFAUT (600 s
    /// depuis la Story 8.8, 120 s auparavant) :
    /// le comportement d'avant cette story est strictement inchangé pour qui n'a pas appairé de
    /// canal ou a coupé `Waiting` sur cet agent.
    #[test]
    fn timeout_long_seulement_si_notif_mobile_attendue() {
        // Les trois réunies → fenêtre longue.
        assert_eq!(approval_window_secs(true, true, true), MOBILE_APPROVAL_TIMEOUT_SECS);

        // Demande NON actionnable (`WaitingSpecific`, Grok read-only, aucune demande) : personne
        // ne recevra de boutons, allonger l'attente n'apporterait rien.
        assert_eq!(approval_window_secs(false, true, true), APPROVAL_TIMEOUT_SECS);
        // `Waiting` coupé sur CET agent (surcharge 8.2) ou globalement : aucune notification ne
        // partira, le téléphone ne sonnera pas.
        assert_eq!(approval_window_secs(true, false, true), APPROVAL_TIMEOUT_SECS);
        // Aucun destinataire appairé : la longue fenêtre serait tout le risque, zéro bénéfice.
        assert_eq!(approval_window_secs(true, true, false), APPROVAL_TIMEOUT_SECS);
        // Et toutes les combinaisons dégradées.
        assert_eq!(approval_window_secs(false, false, false), APPROVAL_TIMEOUT_SECS);
        assert_eq!(approval_window_secs(true, false, false), APPROVAL_TIMEOUT_SECS);
        assert_eq!(approval_window_secs(false, true, false), APPROVAL_TIMEOUT_SECS);
        assert_eq!(approval_window_secs(false, false, true), APPROVAL_TIMEOUT_SECS);
    }

    /// Éviction (2ᵉ PermissionRequest du même agent) : `prev == next == WaitingApprove` ⇒
    /// `notif_kind_for_transition` rend `None` ⇒ aucun trigger. La fenêtre longue ne doit PAS
    /// s'appliquer — c'est le cas que le câblage `waiting_will_notify` referme dans `handle_client`.
    #[test]
    fn eviction_waiting_ne_produit_pas_de_trigger_notif() {
        use crate::agent::state::AgentState;
        use crate::notify::notif_kind_for_transition;

        assert_eq!(
            notif_kind_for_transition(
                Some(AgentState::WaitingApprove),
                AgentState::WaitingApprove,
                false,
            ),
            None,
            "éviction : pas de nouvelle notif → le pipe doit rester à la fenêtre par défaut (600 s)"
        );
        // Et approval_window_secs(true, false, true) = 600 s (waiting_notified = false).
        assert_eq!(approval_window_secs(true, false, true), APPROVAL_TIMEOUT_SECS);
    }

    /// Code review 8.8 — une demande read-only ne doit JAMAIS parquer la connexion : le hook a déjà
    /// rendu la main, rien ni personne ne peut résoudre l'entrée, donc l'attente irait toujours au
    /// bout de la fenêtre (600 s depuis cette story) pour tenir un slot de pipe sans aucun bénéfice.
    /// Les demandes actionnables, elles, doivent continuer d'attendre — c'est tout l'objet du pipe.
    #[test]
    fn read_only_ne_parque_jamais_la_connexion() {
        // Le seul cas qui attend : une permission actionnable.
        assert!(should_await_approval(true, false));
        // Grok `default`/ask et `AskUserQuestion` : `read_only` → jamais d'attente.
        assert!(!should_await_approval(true, true));
        // Tout ce qui n'est pas un `PermissionRequest` n'a jamais attendu (fire-and-forget).
        assert!(!should_await_approval(false, false));
        assert!(!should_await_approval(false, true));
    }

    /// La fenêtre effective doit rester SOUS le plafond du hook porteur de CHAQUE outil, déjà
    /// installé dans sa configuration (`config/mod.rs`). Au-delà, le hook mourrait avant l'auto-deny
    /// et Vigil répondrait dans un tuyau fermé — et chez Kimi ce dépassement est **fail-open**
    /// (l'appel passe sans approbation, aucune alarme).
    ///
    /// Le verrou DUR est le `const _: () = { … }` en tête de fichier : il casse la compilation. Ce
    /// test vérifie la même propriété sur la valeur réellement RENDUE, celle qui finira dans le
    /// `tokio::time::timeout` — les constantes pourraient rester conformes pendant que la
    /// composition `approval_window_secs` → `effective_approval_window_secs`, elle, dérive.
    #[test]
    fn fenetre_effective_reste_sous_le_plafond_de_chaque_outil() {
        // Story 8.8 — le défaut vaut 600 s, la fenêtre mobile 1500 s. Valeurs LITTÉRALES : ce sont
        // les seules assertions du fichier qui casseraient si quelqu'un modifiait une constante de
        // POLITIQUE sans passer par une story. `approval_window_secs` reste pure et agnostique.
        assert_eq!(approval_window_secs(true, true, true), 1500);
        assert_eq!(approval_window_secs(false, false, false), 600);

        for tool in ALL_SOURCE_TOOLS {
            let plafond = hook_timeout_ceiling_secs(tool);
            for demandee in [
                approval_window_secs(false, false, false),
                approval_window_secs(true, true, true),
            ] {
                let effective = effective_approval_window_secs(tool, demandee);
                assert!(
                    effective + HOOK_STARTUP_MARGIN_SECS <= plafond,
                    "{tool:?} : fenêtre {effective}s + marge dépasse le plafond {plafond}s",
                );
                assert!(
                    effective >= MIN_USABLE_WINDOW_SECS,
                    "{tool:?} : fenêtre {effective}s sous le plancher d'utilisabilité",
                );
                assert!(effective <= demandee, "l'écrêtage ne doit jamais ALLONGER");
            }
        }
    }

    /// AC5 — les valeurs de plafond, outil par outil. Verrou de VALEUR : le `const` de compilation
    /// prouve la cohérence des inégalités, pas que Kimi vaut bien 600.
    #[test]
    fn plafond_par_outil_valeurs_exactes() {
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::ClaudeCode), 1800);
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::Codex), 1800);
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::Grok), 1800);
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::Antigravity), 1800);
        // Plafond DUR d'outil : `HookDefSchema` borne `timeout` à [1, 600].
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::Kimi), 600);
        // Défaut de sûreté : le plus BAS des plafonds, jamais 1800.
        assert_eq!(hook_timeout_ceiling_secs(&SourceTool::Unknown), 600);
        let mini = ALL_SOURCE_TOOLS.iter().map(hook_timeout_ceiling_secs).min().unwrap();
        assert_eq!(
            hook_timeout_ceiling_secs(&SourceTool::Unknown),
            mini,
            "`Unknown` doit TOUJOURS porter le plafond le plus bas (payload sans source_tool)",
        );
    }

    /// AC5 — l'écrêtage mord chez Kimi (les DEUX branches) et laisse les autres INTACTS.
    #[test]
    fn ecretage_mord_sur_kimi_et_laisse_les_autres_intacts() {
        // Kimi : 600 − 60 = 540, atteint par le défaut (600) comme par la fenêtre mobile (1500).
        assert_eq!(effective_approval_window_secs(&SourceTool::Kimi, 600), 540);
        assert_eq!(effective_approval_window_secs(&SourceTool::Kimi, 1500), 540);
        // ⚠️ La branche `else { APPROVAL_TIMEOUT_SECS }` DOIT être couverte : c'est elle qui, à
        // 600 s, consomme 100 % du plafond de Kimi — pas seulement la fenêtre mobile de la 8.3.
        assert!(effective_approval_window_secs(&SourceTool::Kimi, APPROVAL_TIMEOUT_SECS) < APPROVAL_TIMEOUT_SECS);

        for tool in [SourceTool::ClaudeCode, SourceTool::Codex, SourceTool::Grok, SourceTool::Antigravity] {
            assert_eq!(effective_approval_window_secs(&tool, 600), 600, "{tool:?} inchangé");
            assert_eq!(effective_approval_window_secs(&tool, 1500), 1500, "{tool:?} inchangé");
        }
    }

    /// AC5 — exhaustivité de `ALL_SOURCE_TOOLS`, **dérivée d'un `match` exhaustif**.
    ///
    /// `successeur` est le 2ᵉ point d'arrêt de compilation (le 1er est `hook_timeout_ceiling_secs`) :
    /// ajouter un variant à `SourceTool` casse le build ICI, et la seule réparation possible est de
    /// NOMMER le nouveau variant. Le chaînage force ensuite à le GREFFER dans la suite — un
    /// `NouveauVariant => None` laissé en cul-de-sac casserait `chaine` (deux terminateurs) et
    /// serait rattrapé par l'égalité finale avec `ALL_SOURCE_TOOLS`. Jamais en silence.
    #[test]
    fn all_source_tools_derive_du_match_exhaustif() {
        fn successeur(tool: &SourceTool) -> Option<SourceTool> {
            match tool {
                SourceTool::ClaudeCode => Some(SourceTool::Codex),
                SourceTool::Codex => Some(SourceTool::Grok),
                SourceTool::Grok => Some(SourceTool::Antigravity),
                SourceTool::Antigravity => Some(SourceTool::Kimi),
                SourceTool::Kimi => Some(SourceTool::Unknown),
                SourceTool::Unknown => None,
            }
        }

        let mut chaine = vec![SourceTool::ClaudeCode];
        while let Some(suivant) = successeur(chaine.last().unwrap()) {
            assert!(!chaine.contains(&suivant), "cycle dans `successeur` : {suivant:?}");
            chaine.push(suivant);
            assert!(chaine.len() <= 64, "chaîne non terminée");
        }

        assert_eq!(
            chaine.as_slice(),
            ALL_SOURCE_TOOLS,
            "ALL_SOURCE_TOOLS doit être exactement la suite dérivée du `match` exhaustif",
        );
    }

    /// AC7 — l'éviction d'une demande par une plus récente doit être SIGNALÉE, sinon le message
    /// déjà reçu au téléphone garde des boutons qui ne résolvent plus rien.
    #[test]
    fn register_signale_l_eviction() {
        let mut pa = PendingApprovals::new();
        let (tx1, rx1) = oneshot::channel::<String>();
        assert!(!pa.register("C:\\t.jsonl".into(), tx1), "première demande : rien à évincer");

        let (tx2, _rx2) = oneshot::channel::<String>();
        assert!(pa.register("C:\\t.jsonl".into(), tx2), "la précédente est évincée");
        // Comportement historique préservé : l'évincée est DÉNIÉE, pas laissée en suspens.
        assert_eq!(rx1.blocking_recv().unwrap(), r#"{"behavior":"deny"}"#);

        // Un AUTRE agent n'évince personne.
        let (tx3, _rx3) = oneshot::channel::<String>();
        assert!(!pa.register("C:\\autre.jsonl".into(), tx3));
    }

    /// Désarmement « terminal » : le prédicat est vrai UNIQUEMENT quand l'agent quitte l'attente
    /// pour REPRENDRE son travail (`Working`/`Idle`). Vrai aussi sur les résolutions poste/mobile
    /// (hook de suite) — sans danger car le désarmement est once-only (documenté sur
    /// `agent_left_waiting`).
    #[test]
    fn agent_left_waiting_detecte_la_sortie_d_attente() {
        use AgentState::*;
        // Résolution terminal → reprise du travail → vrai (les deux natures d'attente).
        assert!(agent_left_waiting(Some(&WaitingApprove), Some(&Working)));
        assert!(agent_left_waiting(Some(&WaitingSpecific), Some(&Idle)));
        // Sorties ANORMALES (rien n'a été traité) → faux : on laisse le handler parqué étiqueter
        // « Expired », jamais un « Handled in the terminal » mensonger (revue 8.7).
        assert!(!agent_left_waiting(Some(&WaitingApprove), Some(&Ended))); // interrupt / fin de session
        assert!(!agent_left_waiting(Some(&WaitingSpecific), Some(&Error))); // StopFailure
        assert!(!agent_left_waiting(Some(&WaitingApprove), Some(&Disconnected))); // déconnexion
        assert!(!agent_left_waiting(Some(&WaitingApprove), None)); // agent disparu
        // Toujours en attente (éviction WaitingApprove→WaitingSpecific incluse) → faux.
        assert!(!agent_left_waiting(Some(&WaitingApprove), Some(&WaitingApprove)));
        assert!(!agent_left_waiting(Some(&WaitingApprove), Some(&WaitingSpecific)));
        // N'attendait pas → faux (jamais de désarmement parasite).
        assert!(!agent_left_waiting(Some(&Working), Some(&Idle)));
        assert!(!agent_left_waiting(None, Some(&Working)));
    }

    #[test]
    fn phantom_faux_pour_pretooluse_source_tool_absent() {
        // source_tool absent → défaut Unknown ≠ Codex → pas filtré (fail-open, jamais de drop).
        let raw = r#"{"hook_event_name":"PreToolUse","session_id":"s","turn_id":"t","agent_id":"sub-1","transcript_path":"C:\\sub.jsonl","cwd":"C:\\","tool_name":"Bash","tool_input":{}}"#;
        assert!(!is_subagent_scoped_phantom(&parse(raw), raw.as_bytes()));
    }
}
