use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_store::StoreExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::{credentials, fetcher, UsageStats};
use crate::SETTINGS_STORE;

const POLL_INTERVAL_SECS: u64 = 5 * 60;
const COOLDOWN_SECS: i64 = 60;
// Garde anti-boucle du spawn AUTOMATIQUE d'agy : un spawn qui échoue en timeout (agy présent
// mais dont le port ne bind jamais) laisse le cache périmé → sans ce garde, le poll retenterait
// un spawn à chaque tick (5 min) indéfiniment. On ne retente donc pas avant 2h. Le Refresh
// MANUEL ignore ce garde (l'utilisateur a explicitement demandé).
const AUTO_SPAWN_GUARD_SECS: i64 = 2 * 60 * 60;

// Clés d'outils (alignées sur `SourceTool` / `EnabledTools`).
const CLAUDE: &str = "claude_code";
const CODEX: &str = "codex";
const ANTIGRAVITY: &str = "antigravity";
const GROK: &str = "grok";

#[derive(Debug, Clone, Copy)]
pub enum FetchSource {
    /// Fetch immédiat au (ré)armement du tracking (`Signal::Start`) : au lancement de l'app ou
    /// à l'activation. Autorise un spawn agy INCONDITIONNEL (rattrape la dérive, pas seulement les
    /// resets), borné par le garde en mémoire → un spawn par lancement au plus.
    Startup,
    Timer,
    AgentEnded,
    Manual,
}

enum Signal {
    Start,
    Stop,
    /// `Some(tool)` : rafraîchir un outil ; `None` : rafraîchir tous les outils suivis.
    Fetch(Option<String>, FetchSource),
}

/// DTO exposé au frontend (contrat inchangé, byte-identique côté Claude).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageState {
    pub tracking_enabled: bool,
    pub data: Option<UsageStats>,
    pub last_fetch_unix: Option<i64>,
    pub last_error: Option<String>,
}

/// Événement usage ÉTIQUETÉ par outil (Story 6.13) : payload de l'événement générique
/// `usage:state`. L'étiquette `tool` permet à chaque consommateur (Notch, fenêtre Usage)
/// de ne garder que ce qui le concerne → un seul chemin push pour TOUS les outils.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageEvent<'a> {
    tool: &'a str,
    state: UsageState,
}

/// État interne par outil (le flag de tracking est global, cf. `ManagerState`).
#[derive(Debug, Clone, Default)]
struct ToolUsage {
    data: Option<UsageStats>,
    last_fetch_unix: Option<i64>,
    last_error: Option<String>,
    /// Dernière TENTATIVE de spawn auto d'agy (unix s) — cf. `AUTO_SPAWN_GUARD_SECS`. Écrit
    /// succès comme échec ; seul le chemin auto (Timer/AgentEnded) le lit et l'écrit.
    last_spawn_attempt: Option<i64>,
}

/// État global du manager : un seul flag de tracking, une map d'états par outil.
struct ManagerState {
    tracking_enabled: bool,
    tools: HashMap<String, ToolUsage>,
}

pub struct UsageManager {
    app: AppHandle,
    state: Arc<Mutex<ManagerState>>,
    tx: UnboundedSender<Signal>,
}

impl UsageManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let initial_enabled = read_tracking_enabled(&app);

        // Restauration du cache par outil (Claude conserve ses clés d'origine).
        let mut tools = HashMap::new();
        for tool in [CLAUDE, CODEX, ANTIGRAVITY, GROK] {
            let (data, last_fetch) = read_cached_data(&app, tool);
            tools.insert(
                tool.to_string(),
                ToolUsage {
                    data,
                    last_fetch_unix: last_fetch,
                    last_error: None,
                    last_spawn_attempt: None,
                },
            );
        }

        let state = Arc::new(Mutex::new(ManagerState {
            tracking_enabled: initial_enabled,
            tools,
        }));

        let manager = Arc::new(Self {
            app: app.clone(),
            state: Arc::clone(&state),
            tx: tx.clone(),
        });

        let app_clone = app.clone();
        let state_clone = Arc::clone(&state);
        tauri::async_runtime::spawn(async move {
            run_loop(state_clone, app_clone, rx).await;
        });

        if initial_enabled {
            let _ = tx.send(Signal::Start);
        }

        manager
    }

    pub fn snapshot(&self, tool: &str) -> UsageState {
        let s = self.state.lock().unwrap();
        let t = s.tools.get(tool);
        UsageState {
            tracking_enabled: s.tracking_enabled,
            data: t.and_then(|t| t.data.clone()),
            last_fetch_unix: t.and_then(|t| t.last_fetch_unix),
            last_error: t.and_then(|t| t.last_error.clone()),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> UsageState {
        let changed = {
            let mut s = self.state.lock().unwrap();
            if s.tracking_enabled == enabled {
                false
            } else {
                s.tracking_enabled = enabled;
                true
            }
        };

        if changed {
            // Emit synchrone ÉTIQUETÉ pour chaque outil suivi → Notch/Settings/fenêtre Usage
            // voient le nouveau flag `tracking_enabled` sans attendre le prochain fetch
            // (Story 6.13 — événement générique multi-outils).
            for tool in tools_to_poll() {
                emit_state(&self.state, &self.app, &tool);
            }
            let signal = if enabled { Signal::Start } else { Signal::Stop };
            let _ = self.tx.send(signal);
        }

        self.snapshot(CLAUDE)
    }

    /// Rafraîchit un outil précis (`None` → Claude, contrat notch inchangé).
    pub fn refresh(&self, tool: &str, source: FetchSource) {
        let _ = self.tx.send(Signal::Fetch(Some(tool.to_string()), source));
    }

    /// Rafraîchit tous les outils suivis (utilisé sur fin d'agent).
    pub fn refresh_all(&self, source: FetchSource) {
        let _ = self.tx.send(Signal::Fetch(None, source));
    }
}

/// Outils à interroger : UNIQUEMENT ceux activés — aucun outil « par défaut ». Chaque outil,
/// Claude compris, est gated par son flag `EnabledTools` (principe de dé-généralisation de Claude).
/// Le `CLAUDE` inconditionnel d'avant était un reliquat de l'époque Claude-only : un poste sans
/// Claude activé (Codex/Antigravity-seul) pollait quand même l'usage Claude (lecture creds ratée,
/// et un `usage:state` d'erreur parasite pour un onglet non affiché). Le frontend gate déjà sur
/// le flag `claudeCodeEnabled` — le backend s'aligne enfin.
fn tools_to_poll() -> Vec<String> {
    let enabled = crate::config::read_enabled_tools();
    let mut tools = Vec::new();
    if enabled.claude_code {
        tools.push(CLAUDE.to_string());
    }
    if enabled.codex {
        tools.push(CODEX.to_string());
    }
    if enabled.antigravity {
        tools.push(ANTIGRAVITY.to_string());
    }
    if enabled.grok {
        tools.push(GROK.to_string());
    }
    tools
}

async fn run_loop(
    state: Arc<Mutex<ManagerState>>,
    app: AppHandle,
    mut rx: UnboundedReceiver<Signal>,
) {
    let mut active = false;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(POLL_INTERVAL_SECS);

    loop {
        let signal = if active {
            tokio::select! {
                s = rx.recv() => match s { Some(s) => s, None => return },
                _ = tokio::time::sleep_until(deadline) => Signal::Fetch(None, FetchSource::Timer),
            }
        } else {
            match rx.recv().await {
                Some(s) => s,
                None => return,
            }
        };

        match signal {
            Signal::Start => {
                if !active {
                    active = true;
                    fetch_tools(&state, &app, &tools_to_poll(), FetchSource::Startup).await;
                    deadline = tokio::time::Instant::now() + Duration::from_secs(POLL_INTERVAL_SECS);
                }
            }
            Signal::Stop => {
                active = false;
            }
            Signal::Fetch(tool, source) => {
                // Le refresh manuel fonctionne même tracking désactivé (one-shot).
                if active || matches!(source, FetchSource::Manual) {
                    match tool {
                        Some(t) => fetch_tools(&state, &app, &[t], source).await,
                        None => fetch_tools(&state, &app, &tools_to_poll(), source).await,
                    }
                    deadline = tokio::time::Instant::now() + Duration::from_secs(POLL_INTERVAL_SECS);
                }
            }
        }
    }
}

async fn fetch_tools(
    state: &Arc<Mutex<ManagerState>>,
    app: &AppHandle,
    tools: &[String],
    source: FetchSource,
) {
    for tool in tools {
        do_fetch_tool(state, app, tool, source).await;
    }
}

async fn do_fetch_tool(
    state: &Arc<Mutex<ManagerState>>,
    app: &AppHandle,
    tool: &str,
    source: FetchSource,
) {
    let now = now_unix();

    // Cooldown par outil — le Refresh MANUEL le contourne : le bouton existe pour forcer un fetch,
    // et le sauter silencieusement (no-op) dans les 60s d'un fetch réussi est trompeur. Les fetches
    // étant sérialisés par `run_loop` (un signal à la fois), un Manual ne peut pas se chevaucher.
    if !matches!(source, FetchSource::Manual) {
        let s = state.lock().unwrap();
        if let Some(t) = s.tools.get(tool) {
            let last = t.last_fetch_unix.unwrap_or(0);
            // Garde `now >= last` : si l'horloge murale recule (correction NTP, DST, réglage manuel),
            // `now - last` deviendrait négatif → toujours `< COOLDOWN_SECS` → fetch skippé jusqu'à ce
            // que le wall-clock rattrape `last`. Un `now` antérieur au dernier fetch relance le fetch.
            if last > 0 && now >= last && now - last < COOLDOWN_SECS {
                return;
            }
        }
    }

    let allow_spawn = compute_allow_spawn(state, tool, source, now);

    let result = fetch_for_tool(tool, allow_spawn).await;

    match result {
        Ok(stats) => {
            {
                let mut s = state.lock().unwrap();
                let entry = s.tools.entry(tool.to_string()).or_default();
                entry.data = Some(stats.clone());
                entry.last_fetch_unix = Some(now);
                entry.last_error = None;
            }
            persist_cache(app, tool, &stats, now);

            // Amorçage de fenêtre d'usage (Story 6.25, Claude SEUL) : sur un fetch Claude réussi,
            // si le réglage opt-in est ON et que la fenêtre 5h `primary` est FERMÉE, envoyer un
            // POST minimal pour la démarrer. Aucun horodatage : l'état ouvert/fermé s'auto-dissipe
            // (amorcé → `resets_at` présent → condition fausse au fetch suivant). Best-effort en
            // tâche DÉTACHÉE → n'ajoute aucune latence à l'émission `usage:state` ci-dessous, et
            // toute erreur (réseau/4xx) reste absorbée sans toucher le cache ni l'événement.
            if tool == CLAUDE
                && read_priming_enabled(app)
                && fetcher::primary_window_closed(&stats)
            {
                // Le fetch vient de réussir → token valide (couvre structurellement le token
                // expiré : un token mort aurait fait échouer le fetch avant cette branche).
                // Re-lecture disque triviale ; course improbable → no-op silencieux.
                if let Ok(creds) = credentials::read_credentials() {
                    let token = creds.access_token;
                    tauri::async_runtime::spawn(async move {
                        fetcher::prime_claude_window(&token).await;
                    });
                }
            }
        }
        Err(e) => {
            let mut s = state.lock().unwrap();
            s.tools.entry(tool.to_string()).or_default().last_error = Some(e);
        }
    }

    // Événement générique ÉTIQUETÉ par outil (Story 6.13) : émis pour TOUS les outils
    // (Claude compris), succès comme erreur. Chaque consommateur filtre sur son outil →
    // plus d'écrasement d'onglet possible, plus de poll nécessaire côté frontend.
    emit_state(state, app, tool);
}

async fn fetch_for_tool(tool: &str, allow_spawn: bool) -> Result<UsageStats, String> {
    match tool {
        CODEX => {
            let creds = credentials::read_codex_credentials()?;
            fetcher::fetch_codex_usage(&creds).await
        }
        // Antigravity : source LOCALE (serveur du process agy). Le droit de spawner agy s'il est
        // fermé est décidé en amont (`compute_allow_spawn`) : Refresh manuel toujours, poll auto
        // seulement si le cache est périmé (fenêtre resetée) + garde 2h. Pas de credentials
        // fichier (zéro OAuth).
        ANTIGRAVITY => fetcher::fetch_antigravity_usage(allow_spawn).await,
        // Grok : quota hebdo en crédits (billing?format=credits). Credentials `~/.grok/auth.json`
        // (token OIDC), pas de spawn (`compute_allow_spawn` retourne déjà `false` hors Antigravity).
        GROK => {
            let creds = credentials::read_grok_credentials()?;
            fetcher::fetch_grok_usage(&creds).await
        }
        // Claude (chemin inchangé : credentials, endpoint, entêtes, cooldown identiques).
        _ => {
            let creds = credentials::read_credentials()?;
            fetcher::fetch_usage(&creds.access_token).await
        }
    }
}

/// Décide si un fetch a le droit de lancer agy headless (spawn opt-in ; Antigravity uniquement).
/// - Autre outil qu'Antigravity → `false` (aucun autre outil ne spawn).
/// - `Manual` → `true` : l'utilisateur a explicitement demandé ; ignore le garde anti-boucle et
///   n'horodate rien (le manual peut ne rien spawner si agy tourne déjà — on ne veut pas étouffer
///   un futur spawn auto).
/// - `Startup` → spawn INCONDITIONNEL (rattrape aussi la dérive, pas que les resets), borné par le
///   seul garde 2h ; on horodate la tentative.
/// - `Timer`/`AgentEnded` → `true` seulement si le cache est PÉRIMÉ (une fenêtre a passé son reset)
///   ET que le garde 2h l'autorise ; on horodate alors la tentative.
///
/// L'horodatage (`last_spawn_attempt`) est écrit AVANT le spawn → anti-boucle même si le spawn
/// échoue (agy introuvable / port qui ne bind jamais → pas de nouvel essai avant 2h).
fn compute_allow_spawn(
    state: &Arc<Mutex<ManagerState>>,
    tool: &str,
    source: FetchSource,
    now: i64,
) -> bool {
    if tool != ANTIGRAVITY {
        return false;
    }
    if matches!(source, FetchSource::Manual) {
        return true;
    }
    // Startup ignore la péremption (spawn tant que le garde le permet) ; les autres exigent un cache
    // périmé pour ne pas spawner agy en tâche de fond au ralenti.
    let require_stale = !matches!(source, FetchSource::Startup);
    let mut s = state.lock().unwrap();
    let (stale, guard_ok) = match s.tools.get(tool) {
        Some(t) => (
            !require_stale || cache_stale(t.data.as_ref(), now),
            t.last_spawn_attempt
                .is_none_or(|last| now - last >= AUTO_SPAWN_GUARD_SECS),
        ),
        None => return false,
    };
    if stale && guard_ok {
        s.tools
            .entry(tool.to_string())
            .or_default()
            .last_spawn_attempt = Some(now);
        true
    } else {
        false
    }
}

/// Le cache est PÉRIMÉ dès qu'au moins une fenêtre a passé son heure de reset : la donnée
/// affichée ment (la fenêtre est repartie à plein). `resets_at` inconnu/non-parsable → PAS
/// périmé (on ne spawn pas dans le doute).
fn cache_stale(data: Option<&UsageStats>, now: i64) -> bool {
    data.map(|d| d.windows.iter().any(|w| reset_in_past(&w.resets_at, now)))
        .unwrap_or(false)
}

fn reset_in_past(resets_at: &Option<String>, now: i64) -> bool {
    matches!(resets_at.as_deref().and_then(rfc3339_to_unix), Some(r) if r <= now)
}

/// Parse un RFC3339 UTC (`"2026-07-12T00:00:00Z"`) en secondes Unix. Faute de lib de date dans le
/// projet : on lit uniquement les 19 premiers caractères `YYYY-MM-DDTHH:MM:SS` (toujours présents
/// quels que soient les fractions/le fuseau ; agy émet du `Z`, traité comme UTC). `None` si le
/// format ne colle pas → « reset inconnu » côté appelant.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + se)
}

/// Jours (signés) depuis 1970-01-01 pour une date civile — algorithme de Howard Hinnant, exact
/// sur toutes les années/bissextiles. `m` ∈ [1,12], `d` ∈ [1,31].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Émet l'état d'un outil, ÉTIQUETÉ par `tool`, sur l'événement générique `usage:state`.
/// Remplace l'ancien `usage:updated` Claude-only : un seul canal push pour TOUS les outils,
/// chaque consommateur filtrant sur l'outil qui le concerne (Story 6.13).
fn emit_state(state: &Arc<Mutex<ManagerState>>, app: &AppHandle, tool: &str) {
    let snap = {
        let s = state.lock().unwrap();
        let t = s.tools.get(tool);
        UsageState {
            tracking_enabled: s.tracking_enabled,
            data: t.and_then(|t| t.data.clone()),
            last_fetch_unix: t.and_then(|t| t.last_fetch_unix),
            last_error: t.and_then(|t| t.last_error.clone()),
        }
    };
    let _ = app.emit("usage:state", &UsageEvent { tool, state: snap });
}

/// Clés de cache persistant par outil. Claude conserve ses clés d'origine
/// (`usageLastData`/`usageLastFetch`) → non-régression ; Codex a des clés distinctes.
fn cache_keys(tool: &str) -> (&'static str, &'static str) {
    match tool {
        CODEX => ("usageLastData_codex", "usageLastFetch_codex"),
        ANTIGRAVITY => ("usageLastData_antigravity", "usageLastFetch_antigravity"),
        GROK => ("usageLastData_grok", "usageLastFetch_grok"),
        _ => ("usageLastData", "usageLastFetch"),
    }
}

fn persist_cache(app: &AppHandle, tool: &str, stats: &UsageStats, fetched_at: i64) {
    let (data_key, fetch_key) = cache_keys(tool);
    if let Ok(store) = app.store(SETTINGS_STORE) {
        if let Ok(value) = serde_json::to_value(stats) {
            store.set(data_key, value);
        }
        store.set(fetch_key, fetched_at);
        let _ = store.save();
    }
}

fn read_tracking_enabled(app: &AppHandle) -> bool {
    app.store(SETTINGS_STORE)
        .ok()
        .and_then(|s| s.get("usageTrackingEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Réglage opt-in d'amorçage de fenêtre (Story 6.25). Lu passivement depuis le store partagé à
/// chaque fetch Claude réussi, défaut `false`. Pas de commande Tauri dédiée (contrairement à
/// `set_usage_tracking_enabled`) : le priming n'a pas de cycle de vie à démarrer/arrêter, c'est un
/// simple booléen consulté à chaque tick.
///
/// Exige AUSSI que le tracking d'usage soit actif (`read_tracking_enabled`) : le priming est un
/// sous-ensemble STRICT du tracking. L'UI masque le toggle quand le tracking est OFF mais ne
/// réinitialise PAS la valeur stockée ; sans cette garde, un refresh MANUEL (qui contourne
/// `active` dans `run_loop`) amorcerait avec un flag stale → violation de l'opt-in (revue 6.25).
fn read_priming_enabled(app: &AppHandle) -> bool {
    read_tracking_enabled(app)
        && app
            .store(SETTINGS_STORE)
            .ok()
            .and_then(|s| s.get("usageWindowPrimingEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

fn read_cached_data(app: &AppHandle, tool: &str) -> (Option<UsageStats>, Option<i64>) {
    let (data_key, fetch_key) = cache_keys(tool);
    let Ok(store) = app.store(SETTINGS_STORE) else { return (None, None) };
    let data = store
        .get(data_key)
        .and_then(|v| serde_json::from_value(v).ok());
    let last_fetch = store.get(fetch_key).and_then(|v| v.as_i64());
    (data, last_fetch)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::WindowUsage;

    // 2026-07-12T00:00:00Z. Une fenêtre resetant AVANT est périmée, APRÈS est fraîche.
    const NOW: i64 = 1_783_814_400;

    fn stats_with_reset(reset: Option<&str>) -> UsageStats {
        UsageStats {
            windows: vec![WindowUsage {
                id: "w".into(),
                label: "W".into(),
                utilization: 50.0,
                resets_at: reset.map(str::to_string),
            }],
        }
    }

    fn state_with(
        tool: &str,
        data: Option<UsageStats>,
        last_spawn: Option<i64>,
    ) -> Arc<Mutex<ManagerState>> {
        let mut tools = HashMap::new();
        tools.insert(
            tool.to_string(),
            ToolUsage {
                data,
                last_fetch_unix: None,
                last_error: None,
                last_spawn_attempt: last_spawn,
            },
        );
        Arc::new(Mutex::new(ManagerState {
            tracking_enabled: true,
            tools,
        }))
    }

    fn spawn_stamp(state: &Arc<Mutex<ManagerState>>, tool: &str) -> Option<i64> {
        state.lock().unwrap().tools.get(tool).unwrap().last_spawn_attempt
    }

    #[test]
    fn rfc3339_parse_valeur_connue() {
        assert_eq!(rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_unix("2026-07-12T00:00:00Z"), Some(NOW));
        // Fractions de seconde + fuseau ignorés (on ne lit que les 19 premiers caractères).
        assert_eq!(rfc3339_to_unix("2026-07-12T00:00:00.123456Z"), Some(NOW));
        assert_eq!(rfc3339_to_unix("2026-07-12T00:00:00+02:00"), Some(NOW));
    }

    #[test]
    fn rfc3339_parse_format_invalide_none() {
        assert_eq!(rfc3339_to_unix(""), None);
        assert_eq!(rfc3339_to_unix("pas une date"), None);
        assert_eq!(rfc3339_to_unix("2026/07/12T00:00:00Z"), None); // séparateurs
        assert_eq!(rfc3339_to_unix("2026-13-12T00:00:00Z"), None); // mois invalide
        assert_eq!(rfc3339_to_unix("2026-07-12T25:00:00Z"), None); // heure invalide
    }

    #[test]
    fn reset_in_past_passe_futur_inconnu() {
        assert!(reset_in_past(&Some("2026-07-11T00:00:00Z".into()), NOW)); // hier
        assert!(reset_in_past(&Some("2026-07-12T00:00:00Z".into()), NOW)); // pile (<=)
        assert!(!reset_in_past(&Some("2026-07-13T00:00:00Z".into()), NOW)); // demain
        assert!(!reset_in_past(&None, NOW)); // inconnu
        assert!(!reset_in_past(&Some("bidon".into()), NOW)); // non-parsable
    }

    #[test]
    fn cache_stale_selon_fenetres() {
        assert!(cache_stale(Some(&stats_with_reset(Some("2026-07-11T00:00:00Z"))), NOW));
        assert!(!cache_stale(Some(&stats_with_reset(Some("2026-07-20T00:00:00Z"))), NOW));
        assert!(!cache_stale(Some(&stats_with_reset(None)), NOW));
        assert!(!cache_stale(None, NOW));
    }

    #[test]
    fn allow_spawn_autre_outil_jamais() {
        // Claude/Codex ne spawnent jamais, même cache périmé et même en Manual.
        let state = state_with(CLAUDE, Some(stats_with_reset(Some("2026-07-11T00:00:00Z"))), None);
        assert!(!compute_allow_spawn(&state, CLAUDE, FetchSource::Manual, NOW));
        assert!(!compute_allow_spawn(&state, CLAUDE, FetchSource::Timer, NOW));
    }

    #[test]
    fn allow_spawn_manual_toujours_sans_horodatage() {
        // Manual autorise même sans donnée périmée, et n'écrit PAS le garde (pour ne pas étouffer
        // un futur spawn auto si agy tournait au moment du Manual).
        let state = state_with(ANTIGRAVITY, Some(stats_with_reset(Some("2026-07-20T00:00:00Z"))), None);
        assert!(compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Manual, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), None);
    }

    #[test]
    fn allow_spawn_auto_stale_horodate() {
        let state = state_with(ANTIGRAVITY, Some(stats_with_reset(Some("2026-07-11T00:00:00Z"))), None);
        assert!(compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Timer, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), Some(NOW), "tentative horodatée");
    }

    #[test]
    fn allow_spawn_auto_pas_stale() {
        let state = state_with(ANTIGRAVITY, Some(stats_with_reset(Some("2026-07-20T00:00:00Z"))), None);
        assert!(!compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Timer, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), None);
    }

    #[test]
    fn allow_spawn_startup_inconditionnel_meme_pas_stale() {
        // Au lancement : spawn même si aucune fenêtre n'a resetée (rattrape la dérive).
        let state = state_with(ANTIGRAVITY, Some(stats_with_reset(Some("2026-07-20T00:00:00Z"))), None);
        assert!(compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Startup, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), Some(NOW));
    }

    #[test]
    fn allow_spawn_startup_borne_par_le_garde() {
        // Startup reste soumis au garde 2h (relances rapprochées dans une même session).
        let state = state_with(ANTIGRAVITY, Some(stats_with_reset(None)), Some(NOW - 3_600));
        assert!(!compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Startup, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), Some(NOW - 3_600));
    }

    #[test]
    fn allow_spawn_auto_garde_2h_bloque_puis_expire() {
        // Périmé mais tentative il y a 1h → refusé, garde inchangé.
        let state = state_with(
            ANTIGRAVITY,
            Some(stats_with_reset(Some("2026-07-11T00:00:00Z"))),
            Some(NOW - 3_600),
        );
        assert!(!compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Timer, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), Some(NOW - 3_600));

        // Tentative il y a pile 2h → à nouveau autorisé, réhorodaté.
        let state = state_with(
            ANTIGRAVITY,
            Some(stats_with_reset(Some("2026-07-11T00:00:00Z"))),
            Some(NOW - AUTO_SPAWN_GUARD_SECS),
        );
        assert!(compute_allow_spawn(&state, ANTIGRAVITY, FetchSource::Timer, NOW));
        assert_eq!(spawn_stamp(&state, ANTIGRAVITY), Some(NOW));
    }
}
