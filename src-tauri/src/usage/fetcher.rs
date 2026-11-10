use super::{UsageStats, WindowUsage};
use super::credentials::{CodexCredentials, GrokCredentials};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn fetch_usage(access_token: &str) -> Result<UsageStats, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2.1.71")
        // Borne dure : `reqwest::Client` n'a AUCUN timeout par défaut → un endpoint qui accepte la
        // TCP mais ne répond jamais (proxy pendu, captive portal) figerait le poll usage awaité
        // indéfiniment. Miroir du priming (`prime_claude_window`) et d'Antigravity.
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("network_error: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("api_error_{}", status.as_u16()));
    }

    let body = resp.text().await.map_err(|e| format!("read_error: {e}"))?;
    parse_claude_usage(&body)
}

// ── Amorçage de fenêtre d'usage (Story 6.25, Claude seul) ────────────────────────
// La fenêtre 5h Claude démarre au 1er message, pas à la fin de la précédente. Un POST
// minimal `/v1/messages` (~9 tokens) sur une fenêtre FERMÉE fait démarrer le décompte plus
// tôt → resets plus tôt, anti-dérive. Spike Path A validé live : le token OAuth d'usage,
// déjà lu, est accepté direct sur `/v1/messages` (200 sans system prompt) → AUCUN spawn
// `claude`, donc aucun hook / agent fantôme / transcript à nettoyer.

/// Modèle utilisé pour l'amorçage : le Haiku le moins cher accepté par le scope OAuth Claude
/// Code (chaîne EXACTE du spike Path A, 200 / 8-1 tokens le 2026-07-10). FIGÉ, non configurable :
/// un id refusé donnerait un 4xx absorbé silencieusement → la validation e2e (AC5 : `resets_at`
/// apparaît) est le test de cette constante.
const PRIME_MODEL: &str = "claude-haiku-4-5-20251001";

/// `true` SSI la fenêtre 5h Claude (`id == "primary"`) est présente mais FERMÉE (`resets_at`
/// absent). Pur, sans I/O ni horloge. Discriminant tranché sur données réelles (spike) : une
/// fenêtre `primary` ouverte porte un `resets_at` ; fermée, elle est à 0% SANS `resets_at`.
/// N'examine QUE `primary` : une fenêtre hebdo peut avoir `resets_at: None` sans être à amorcer.
pub fn primary_window_closed(stats: &UsageStats) -> bool {
    stats
        .windows
        .iter()
        .any(|w| w.id == "primary" && w.resets_at.is_none())
}

/// Corps minimal de la requête d'amorçage (pur, testable). `max_tokens: 1` + un unique message
/// utilisateur `"."` = plancher de consommation observé au spike (input 8 / output 1).
fn prime_request_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "." }],
    })
}

/// Amorce la fenêtre 5h Claude via un `POST /v1/messages` minimal, avec les MÊMES entêtes OAuth
/// que `fetch_usage` (Bearer + `anthropic-beta: oauth-2025-04-20` + `User-Agent`) plus
/// `anthropic-version` et `Content-Type`. **Best-effort** : toute erreur (réseau, statut non-2xx)
/// est absorbée (log uniquement) et ne remonte JAMAIS à l'appelant — l'amorçage ne doit jamais
/// bloquer ni polluer le fetch d'usage. Le token est supposé valide (appelé depuis la branche
/// succès du fetch → `read_credentials` vient de réussir).
pub async fn prime_claude_window(access_token: &str) {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header("User-Agent", "claude-code/2.1.71")
        .header("Content-Type", "application/json")
        // Borne dure : la requête est spawnée en tâche DÉTACHÉE (jamais awaitée). Sans timeout,
        // une connexion half-open garderait la tâche + le socket vivants indéfiniment et
        // s'accumulerait au fil des ticks. `reqwest::Client` n'a pas de timeout par défaut (revue 6.25).
        .timeout(std::time::Duration::from_secs(10))
        .json(&prime_request_body(PRIME_MODEL))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("usage priming: non-success status {}", r.status().as_u16()),
        Err(e) => eprintln!("usage priming: network error: {e}"),
    }
}

/// Ordre CANONIQUE d'affichage des fenêtres Claude connues : `(clé JSON, id, label)`.
/// L'ordre de ce tableau EST l'ordre d'affichage — surtout PAS l'ordre d'itération de la map
/// JSON : `serde_json::Map` est un `BTreeMap` (alphabétique) sans `preserve_order`, ce qui
/// classerait `seven_day_opus` AVANT `seven_day_sonnet` et inverserait l'affichage.
const CLAUDE_WINDOWS: &[(&str, &str, &str)] = &[
    ("five_hour", "primary", "Current window"),
    ("seven_day", "weekly", "Weekly (all models)"),
    ("seven_day_sonnet", "weekly_sonnet", "Weekly Sonnet"),
    ("seven_day_opus", "weekly_opus", "Weekly Opus"),
];

/// Parse la réponse d'usage Claude (snake_case) en liste ORDONNÉE de fenêtres. Le fetcher
/// attribue `id` + `label` (l'UI ne connaît plus aucun nom de fenêtre). Les clés connues sont
/// émises dans l'ordre canonique ci-dessus ; toute clé objet SUPPLÉMENTAIRE portant un
/// `utilization` numérique est incluse en fin de liste avec un label dérivé → une fenêtre d'un
/// type inédit s'affiche automatiquement, sans toucher l'UI (AC4, objectif central de la story).
pub fn parse_claude_usage(body: &str) -> Result<UsageStats, String> {
    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parse_error: {e}"))?;

    let mut windows = Vec::new();

    // 1) Fenêtres connues, dans l'ordre canonique d'affichage. Absente → non poussée (pas de trou).
    for (key, id, label) in CLAUDE_WINDOWS {
        if let Some(w) = root.get(key).and_then(|v| window_from_claude(v, id, label)) {
            windows.push(w);
        }
    }

    // 2) Fenêtres surnuméraires (clé objet inconnue avec `utilization` numérique) → AC4. Tri
    //    alphabétique des clés restantes pour un ordre déterministe (stabilité des tests).
    let known: std::collections::HashSet<&str> =
        CLAUDE_WINDOWS.iter().map(|(k, _, _)| *k).collect();
    if let Some(obj) = root.as_object() {
        let mut extras: Vec<&String> =
            obj.keys().filter(|k| !known.contains(k.as_str())).collect();
        extras.sort();
        for key in extras {
            // Ignore une clé vide (id/label vides) ou dont l'id dérivé (= la clé brute) entrerait
            // en collision avec une fenêtre déjà poussée (clé inédite nommée comme un id connu,
            // ex. « primary »/« weekly ») → évite une collision de `key` React côté UI (AC4).
            if key.is_empty() || windows.iter().any(|w| w.id == *key) {
                continue;
            }
            let label = title_case(key);
            if let Some(w) = root.get(key).and_then(|v| window_from_claude(v, key, &label)) {
                windows.push(w);
            }
        }
    }

    // Aucune fenêtre exploitable → réponse 2xx valide mais vide/dérivée : remonter une Err claire
    // plutôt qu'un succès vide, qui blanchirait l'onglet (aucune ligne, aucun message) tout en
    // armant le cooldown 60s. Symétrique avec la garde Codex (`parse_codex_usage`).
    if windows.is_empty() {
        return Err("parse_error: no usage windows recognized".to_string());
    }

    Ok(UsageStats { windows })
}

/// Construit une `WindowUsage` depuis une valeur de fenêtre Claude. `utilization` numérique
/// REQUIS (sinon la fenêtre est ignorée : ni objet `null`, ni objet sans quota ne produit de
/// fenêtre fantôme) ; `resets_at` string | null.
fn window_from_claude(v: &serde_json::Value, id: &str, label: &str) -> Option<WindowUsage> {
    // Borne 0–100 (symétrique avec `window_from_codex`) : une valeur hors plage ne doit produire
    // ni « 150% »/« -5% », ni un float non-fini non sérialisable en JSON (cache/event perdus).
    let utilization = v.get("utilization").and_then(|x| x.as_f64())?.clamp(0.0, 100.0) as f32;
    let resets_at = v.get("resets_at").and_then(|x| x.as_str()).map(str::to_string);
    Some(WindowUsage {
        id: id.to_string(),
        label: label.to_string(),
        utilization,
        resets_at,
    })
}

/// Titre-case d'une clé snake_case (`"seven_day"` → `"Seven Day"`, `"monthly"` → `"Monthly"`)
/// pour dériver un label lisible sur une fenêtre d'un type inédit (fallback générique, AC4).
fn title_case(key: &str) -> String {
    key.split('_')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Codex (OpenAI) ────────────────────────────────────────────────────────────
// Endpoint non documenté et versant vite (spike T0, source `openai/codex`). Le
// nommage courant est `…/backend-api/wham/usage` ; l'ancien `…/codex/usage` est
// tenté en repli pour survivre au skew de version. Erreur d'auth (401/403) →
// arrêt immédiat (inutile d'essayer un autre chemin).
const CODEX_USAGE_URLS: &[&str] = &[
    "https://chatgpt.com/backend-api/wham/usage",
    "https://chatgpt.com/backend-api/codex/usage",
];

pub async fn fetch_codex_usage(creds: &CodexCredentials) -> Result<UsageStats, String> {
    let client = reqwest::Client::new();
    let mut last_err = "network_error: no endpoint tried".to_string();

    for url in CODEX_USAGE_URLS {
        let mut req = client
            .get(*url)
            // Borne dure (voir `fetch_usage`) : sans timeout, un endpoint half-open figerait le poll.
            .timeout(std::time::Duration::from_secs(10))
            .header("Authorization", format!("Bearer {}", creds.access_token))
            .header("User-Agent", "codex-cli");
        if let Some(account_id) = &creds.account_id {
            req = req.header("ChatGPT-Account-Id", account_id);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("network_error: {e}");
                continue;
            }
        };

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            // Problème d'authentification : le token/compte est en cause, pas le chemin.
            return Err(format!("api_error_{}", status.as_u16()));
        }
        if !status.is_success() {
            last_err = format!("api_error_{}", status.as_u16());
            continue;
        }

        let body = resp.text().await.map_err(|e| format!("read_error: {e}"))?;
        match parse_codex_usage(&body) {
            Ok(stats) => return Ok(stats),
            Err(e) => {
                last_err = e;
                continue;
            }
        }
    }

    Err(last_err)
}

/// Convertit la réponse Codex vers `UsageStats`. Parsing DÉFENSIF (l'enveloppe de
/// l'API n'est pas stable) : on cherche `primary_window`/`secondary_window` à
/// plusieurs emplacements candidats ; fenêtre absente → `None`, JSON non-JSON →
/// `Err`, jamais de panique. `primary` → 5h, `secondary` → 7 jours.
pub fn parse_codex_usage(body: &str) -> Result<UsageStats, String> {
    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parse_error: {e}"))?;
    let now = now_unix();

    // Construction ORDONNÉE : primaire (5h) d'abord, puis secondaire (7j). Le fetcher porte
    // id + label (Codex : « Weekly », distinct du « Weekly (all models) » de Claude).
    let mut windows = Vec::new();
    if let Some(w) = find_window(&root, "primary_window")
        .and_then(|w| window_from_codex(w, "primary", "Current window", now))
    {
        windows.push(w);
    }
    if let Some(w) = find_window(&root, "secondary_window")
        .and_then(|w| window_from_codex(w, "weekly", "Weekly", now))
    {
        windows.push(w);
    }

    // Aucune fenêtre exploitable → l'enveloppe a probablement dérivé (endpoint volatile) :
    // remonter une erreur claire plutôt qu'un succès vide, qui blanchirait l'onglet
    // (aucune ligne, aucun message) tout en armant le cooldown 60s (Refresh bloqué).
    if windows.is_empty() {
        return Err("parse_error: no usage windows recognized".to_string());
    }

    Ok(UsageStats { windows })
}

/// Localise une fenêtre par clé (`primary_window`/`secondary_window`) en essayant
/// les emplacements connus, du plus courant au plus large.
fn find_window<'a>(root: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    root.pointer(&format!("/rate_limits/rate_limit/{key}"))
        .or_else(|| root.pointer(&format!("/rate_limit/{key}")))
        .or_else(|| root.get(key))
        .filter(|w| w.is_object())
}

/// Mappe une fenêtre Codex (`used_percent` 0–100, `reset_at` unix / `reset_after_seconds` delta)
/// vers `WindowUsage` (`utilization` 0–100, `resets_at` ISO8601).
///
/// Retourne `None` si `used_percent` est absent ou non numérique : mieux vaut une
/// fenêtre indisponible (symétrique avec `resets_at`) qu'un `0%` fabriqué, qui
/// afficherait « quota plein » — la pire valeur fausse possible pour un quota.
fn window_from_codex(w: &serde_json::Value, id: &str, label: &str, now: i64) -> Option<WindowUsage> {
    // Borne 0–100 : l'endpoint étant volatile, une valeur hors plage ne doit pas
    // produire un label « 150% » / « -5% ».
    let utilization = w.get("used_percent").and_then(|x| x.as_f64())?.clamp(0.0, 100.0) as f32;

    let resets_at = w
        .get("reset_at")
        .and_then(|x| x.as_i64())
        .filter(|&t| t > 0)
        .or_else(|| {
            // Delta : même exigence de positivité que `reset_at`, et `checked_add`
            // pour éviter un overflow i64 (panic en debug) sur une valeur aberrante.
            w.get("reset_after_seconds")
                .and_then(|x| x.as_i64())
                .filter(|&delta| delta > 0)
                .and_then(|delta| now.checked_add(delta))
        })
        .map(unix_to_iso8601);

    Some(WindowUsage {
        id: id.to_string(),
        label: label.to_string(),
        utilization,
        resets_at,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix secondes → ISO8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`), sans dépendance externe
/// (algorithme days-from-civil de Howard Hinnant).
fn unix_to_iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// ── Grok (xAI) ──────────────────────────────────────────────────────────────────
// Quota HEBDOMADAIRE en crédits (recette CONFIRMÉE + réponse JSON capturée live, 2026-07-16) :
//   GET https://cli-chat-proxy.grok.com/v1/billing?format=credits
//   Headers: Authorization: Bearer <token>, x-xai-token-auth: xai-grok-cli, Accept: application/json
// Tout vit sous `config` : `creditUsagePercent` (0–100), `productUsage[]{product,usagePercent}`,
// `currentPeriod{type:WEEKLY,start,end}` (reset = `end`). 401/403 → auth en cause.

const GROK_USAGE_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

pub async fn fetch_grok_usage(creds: &GrokCredentials) -> Result<UsageStats, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(GROK_USAGE_URL)
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("Accept", "application/json")
        .header("User-Agent", "vigil-grok/1.0")
        // Borne dure (voir `fetch_usage`) : sans timeout, un endpoint half-open figerait le poll.
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("network_error: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        // 401/403 = token/compte en cause ; tout autre non-2xx = api_error_<code>.
        return Err(format!("api_error_{}", status.as_u16()));
    }

    let body = resp.text().await.map_err(|e| format!("read_error: {e}"))?;
    parse_grok_usage(&body)
}

/// Convertit la réponse `billing?format=credits` en `UsageStats`. Tout vit sous `config`. On itère
/// `productUsage[]` (une fenêtre par produit, robuste au multi-produit) ; repli sur
/// `creditUsagePercent` si `productUsage` absent/vide. `usagePercent`/`creditUsagePercent` sont en
/// **0–100** (le nom porte « Percent » ; parité `utilization` Claude/Codex) → clamp direct, PAS de
/// ×100. Reset = `currentPeriod.end` (hebdo). Aucune fenêtre exploitable → `Err` (garde 6.15).
pub fn parse_grok_usage(body: &str) -> Result<UsageStats, String> {
    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parse_error: {e}"))?;

    let config = root
        .get("config")
        .ok_or_else(|| "parse_error: no config".to_string())?;

    let resets_at = config
        .pointer("/currentPeriod/end")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut windows = Vec::new();

    // 1) Une fenêtre par produit (`productUsage[]`). `product` → id/label ; `usagePercent` 0–100.
    if let Some(products) = config.get("productUsage").and_then(|v| v.as_array()) {
        for p in products {
            let Some(product) = p.get("product").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else {
                continue;
            };
            let Some(pct) = p.get("usagePercent").and_then(|v| v.as_f64()) else {
                continue;
            };
            windows.push(WindowUsage {
                id: product.to_lowercase(),
                label: format!("{product} credits"),
                utilization: pct.clamp(0.0, 100.0) as f32,
                resets_at: resets_at.clone(),
            });
        }
    }

    // 2) Repli : `productUsage` absent/vide → une fenêtre unique depuis `creditUsagePercent`.
    if windows.is_empty() {
        if let Some(pct) = config.get("creditUsagePercent").and_then(|v| v.as_f64()) {
            windows.push(WindowUsage {
                id: "weekly".to_string(),
                label: "Weekly credits".to_string(),
                utilization: pct.clamp(0.0, 100.0) as f32,
                resets_at: resets_at.clone(),
            });
        }
    }

    if windows.is_empty() {
        return Err("parse_error: no usage windows recognized".to_string());
    }

    Ok(UsageStats { windows })
}

// ── Antigravity (Gemini Code Assist / Cascade) ──────────────────────────────────
// PIVOT (correct-course, cf. Story 6.24) : la donnée d'usage affichée par agy `/usage`
// (fenêtres HEBDO groupées « Gemini » vs « Claude & GPT ») NE vient PAS du cloud Google.
// Le cloud (`retrieveUserQuota`/`fetchAvailableModels` sur cloudcode-pa) expose le quota
// Gemini Code Assist JOURNALIER par modèle (données sans rapport), et `fetchAvailableModels`
// nécessite un login OAuth Antigravity distinct (403 avec le token gemini-cli). La vraie
// source est le SERVEUR LOCAL du process `agy` (protocole Connect-RPC de Codeium/Windsurf) :
//   POST http://127.0.0.1:<port>/exa.language_server_pb.LanguageServerService/GetUserStatus
//   Headers: Content-Type: application/json, Connect-Protocol-Version: 1   Body: {}
// Le port est un port en écoute du process `agy.exe`. Aucun token requis pour agy CLI
// (le port HTTP local répond 200 sans CSRF). Zéro OAuth, zéro secret.
//
// Conception :
//   - agy tourne  → poll `GetUserStatus` en direct (gratuit, exact).
//   - agy fermé   → `Err("agy_not_running")` ; le manager conserve le dernier cache
//                   (l'UI affiche « Updated X ago »).
//   - Refresh MANUEL + agy fermé → spawn bref d'agy (bind du port → capture → kill),
//     JAMAIS sur le poll timer (trop intrusif ; les quotas sont hebdo).

const GET_USER_STATUS_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/GetUserStatus";
const AGY_PROCESS_NAME: &str = "agy.exe";
/// Délai max d'attente du bind du port après un spawn (ms) et pas de sondage.
const SPAWN_PORT_WAIT_MS: u64 = 15_000;
const SPAWN_POLL_STEP_MS: u64 = 500;

/// Dernier port agy ayant répondu 200 à `GetUserStatus` (0 = aucun). Sondé EN PREMIER aux ticks
/// suivants pour ne pas re-brute-forcer les autres ports d'agy — dont certains sont en **TLS** :
/// un `http://` sur un port TLS fait logger « client sent an HTTP request to an HTTPS server »
/// dans la TUI agy à CHAQUE scan (bruit visible par l'utilisateur, cf. fix effet de bord 6.24).
/// Best-effort, process-global (un seul agy suivi à la fois). Auto-guéri si agy redémarre : le port
/// caché disparaît de la liste netstat → `order_ports` l'ignore → rescan complet → recache.
static LAST_GOOD_AGY_PORT: AtomicU16 = AtomicU16::new(0);

/// Ordre de sonde : le port connu-bon (`cached`) d'abord s'il est **encore** dans la liste netstat
/// courante, puis les autres dans l'ordre d'origine. `cached == 0` (aucun) ou absent → ordre
/// inchangé. Pur (testable sans I/O). Ne réordonne JAMAIS un port hors de `ports` → on ne sonde
/// jamais un port qui n'appartient pas à agy.
fn order_ports(cached: u16, ports: &[u16]) -> Vec<u16> {
    let mut ordered = Vec::with_capacity(ports.len());
    if cached != 0 && ports.contains(&cached) {
        ordered.push(cached);
    }
    ordered.extend(ports.iter().copied().filter(|&p| p != cached));
    ordered
}

/// Fetcher Antigravity LOCAL. `allow_spawn` = ce fetch vient d'un Refresh manuel : si aucun
/// agy ne tourne, on en lance un brièvement pour capturer, puis on le tue. Sur le poll timer
/// (`allow_spawn = false`), agy fermé → `Err("agy_not_running")` (le cache reste affiché).
pub async fn fetch_antigravity_usage(allow_spawn: bool) -> Result<UsageStats, String> {
    // `agy_listening_ports`/`spawn_agy_headless`/`kill_process_tree` sont SYNCHRONES et bloquants
    // (netstat, spawn, taskkill via `Command::output()`) → `spawn_blocking` pour ne pas figer un
    // worker du runtime async pendant leur exécution.
    let mut ports = tokio::task::spawn_blocking(agy_listening_ports)
        .await
        .unwrap_or_default();

    let mut spawned: Option<u32> = None;
    if ports.is_empty() {
        if !allow_spawn {
            return Err("agy_not_running".to_string());
        }
        // Repli manuel : lancer agy en arrière-plan (headless, sans fenêtre) et attendre que
        // son serveur local bind un port. Aucun prompt envoyé → aucune consommation de quota.
        spawned = Some(
            tokio::task::spawn_blocking(spawn_agy_headless)
                .await
                .map_err(|e| format!("spawn_error: {e}"))??,
        );
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(SPAWN_PORT_WAIT_MS);
        while ports.is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(SPAWN_POLL_STEP_MS)).await;
            ports = tokio::task::spawn_blocking(agy_listening_ports)
                .await
                .unwrap_or_default();
        }
    }

    let result = fetch_from_ports(&ports).await;

    // Toujours tuer l'instance qu'on a nous-mêmes lancée (jamais une instance de l'utilisateur :
    // on ne spawn que si aucun agy ne tournait).
    if let Some(pid) = spawned {
        let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
    }

    result
}

/// Sonde chaque port candidat avec `GetUserStatus` (HTTP simple) et parse la 1re réponse
/// exploitable. `network_error:` seulement si AUCUN port ne répond ; sinon l'erreur de parse
/// du dernier port tenté (enveloppe dérivée).
async fn fetch_from_ports(ports: &[u16]) -> Result<UsageStats, String> {
    if ports.is_empty() {
        return Err("agy_not_running".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .map_err(|e| format!("network_error: {e}"))?;

    // Les ports sont non vides ici → agy TOURNE. Si aucun port n'aboutit, l'erreur par défaut est
    // « injoignable » (démarrage, port gRPC/HTTPS-only), PAS « fermé » : ne pas laisser le message
    // UI dire « Antigravity is closed » alors qu'agy est ouvert. `agy_not_running` est réservé au
    // cas « aucun port » (garde ci-dessus).
    let mut last_err = "agy_unreachable".to_string();
    // Sonder le port connu-bon en premier (évite de re-taper les ports TLS d'agy à chaque tick,
    // qui logguent une erreur de handshake dans sa TUI). Cf. `LAST_GOOD_AGY_PORT`.
    let ordered = order_ports(LAST_GOOD_AGY_PORT.load(Ordering::Relaxed), ports);
    for &port in &ordered {
        let url = format!("http://127.0.0.1:{port}{GET_USER_STATUS_PATH}");
        let resp = match client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .body("{}")
            .send()
            .await
        {
            Ok(r) => r,
            // Port non-HTTP (ex. le port HTTPS/gRPC) ou fermé → port suivant.
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        // Port RPC HTTP d'agy confirmé (200 sur GetUserStatus) → mémoriser pour le sonder d'abord
        // au prochain tick (plus de brute-force des ports TLS).
        LAST_GOOD_AGY_PORT.store(port, Ordering::Relaxed);
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                last_err = format!("read_error: {e}");
                continue;
            }
        };
        match parse_antigravity_usage(&body) {
            Ok(stats) => return Ok(stats),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Pur : convertit la réponse `GetUserStatus` en `UsageStats`. Les modèles vivent dans
/// `userStatus.cascadeModelConfigData.clientModelConfigs[]`, chacun avec
/// `{ label, quotaInfo{ remainingFraction, resetTime } }`. agy GROUPE les modèles partageant
/// le même quota (même `remainingFraction` + `resetTime`) en une fenêtre (« Gemini Models »,
/// « Claude & GPT Models ») → on reproduit ce groupement. `remainingFraction` ABSENT = 0
/// (Connect/protobuf omet les valeurs à zéro) → 100% utilisé. Aucun groupe exploitable → `Err`
/// (garde 6.15, jamais un succès vide).
pub fn parse_antigravity_usage(body: &str) -> Result<UsageStats, String> {
    let root: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parse_error: {e}"))?;

    let configs = root
        .pointer("/userStatus/cascadeModelConfigData/clientModelConfigs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "parse_error: no clientModelConfigs".to_string())?;

    // Groupement PRÉSERVANT L'ORDRE : clé = (resetTime, remainingFraction arrondie). Le 1er
    // modèle d'un groupe fixe sa position ; les familles (1er mot du label) sont accumulées
    // pour dériver un label lisible générique.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Group> = std::collections::HashMap::new();

    for cfg in configs {
        let label = cfg.get("label").and_then(|v| v.as_str()).unwrap_or("");
        if label.trim().is_empty() {
            continue;
        }
        let quota = cfg.get("quotaInfo");
        let reset_time = quota
            .and_then(|q| q.get("resetTime"))
            .and_then(|v| v.as_str());
        // `remainingFraction` absent = 0 (protobuf omet le zéro) → 100% utilisé.
        let remaining = quota
            .and_then(|q| q.get("remainingFraction"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let key = format!("{}|{:.6}", reset_time.unwrap_or(""), remaining);
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Group {
                families: Vec::new(),
                remaining,
                reset_time: reset_time.map(str::to_string),
            }
        });
        let family = model_family(label);
        if !entry.families.iter().any(|f| f == &family) {
            entry.families.push(family);
        }
    }

    let mut windows = Vec::new();
    for key in &order {
        let g = &groups[key];
        // Familles triées alphabétiquement → label/id DÉTERMINISTES quel que soit l'ordre
        // (instable) des modèles dans la réponse : « Claude & GPT », jamais « GPT & Claude ».
        let mut families = g.families.clone();
        families.sort();
        let utilization = ((1.0 - g.remaining).clamp(0.0, 1.0) * 100.0) as f32;
        windows.push(WindowUsage {
            id: families.join("_").to_lowercase(),
            label: group_label(&families),
            utilization,
            resets_at: g.reset_time.clone(),
        });
    }

    if windows.is_empty() {
        return Err("parse_error: no usage windows recognized".to_string());
    }

    // Ordre d'affichage = reset le plus PROCHE d'abord (comme agy `/usage` : Gemini 70h avant
    // Claude & GPT 167h), `None` en dernier. `resetTime` est ISO8601 UTC à format fixe → la
    // comparaison lexicographique équivaut à la comparaison chronologique. Tri STABLE (préserve
    // l'ordre de 1re apparition à égalité de reset).
    windows.sort_by(|a, b| match (&a.resets_at, &b.resets_at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    // Dé-duplication des `id` (clé React côté UI, `UsageApp.tsx`) : deux groupes de quota partageant
    // le même jeu de familles produiraient le même id → suffixe `_2`, `_3`… après le tri (ordre
    // d'affichage stable). Miroir de la garde anti-collision du parser Claude (`window_from_claude`).
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for w in &mut windows {
        let count = seen.entry(w.id.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            w.id = format!("{}_{}", w.id, count);
        }
    }

    Ok(UsageStats { windows })
}

struct Group {
    families: Vec<String>,
    remaining: f64,
    reset_time: Option<String>,
}

/// Famille d'un modèle = tête alphabétique de son label (`"Gemini 3.5 Flash (High)"` →
/// `"Gemini"`, `"GPT-OSS 120B"` → `"GPT"`, `"Claude Sonnet 4.6"` → `"Claude"`). Générique :
/// un modèle futur inconnu se regroupe proprement sans table en dur.
fn model_family(label: &str) -> String {
    let trimmed = label.trim();
    let family: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    // Label sans tête alphabétique (chiffre/symbole/non-latin, ex. « 4o-mini », « 日本モデル ») :
    // replier sur le label nettoyé pour garder un id/label NON VIDES et déterministes plutôt
    // qu'un id « » (collision de clé React) et un label « Models » à espace de tête.
    if family.is_empty() {
        trimmed.to_string()
    } else {
        family
    }
}

/// Label d'un groupe : familles distinctes (ordre de 1re apparition) jointes par « & » +
/// « Models » (`["Gemini"]` → `"Gemini Models"`, `["Claude","GPT"]` → `"Claude & GPT Models"`).
fn group_label(families: &[String]) -> String {
    if families.is_empty() {
        return "Models".to_string();
    }
    format!("{} Models", families.join(" & "))
}

// ── Découverte du process agy + de ses ports en écoute (Windows) ─────────────────

/// Ports TCP en écoute appartenant à un process `agy.exe`. Vide si agy ne tourne pas.
#[cfg(target_os = "windows")]
fn agy_listening_ports() -> Vec<u16> {
    let pids = agy_pids();
    if pids.is_empty() {
        return Vec::new();
    }
    listening_ports_for_pids(&pids)
}

#[cfg(not(target_os = "windows"))]
fn agy_listening_ports() -> Vec<u16> {
    Vec::new()
}

/// PIDs des process nommés `agy.exe` (snapshot Toolhelp, comme `commands::focus_terminal`).
#[cfg(target_os = "windows")]
fn agy_pids() -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut pids = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case(AGY_PROCESS_NAME) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    pids
}

/// Ports locaux en écoute pour un ensemble de PIDs, via `netstat -ano`. On repère les sockets
/// en écoute par leur adresse distante nulle (`0.0.0.0:0` / `[::]:0`) — INDÉPENDANT de la
/// langue de Windows (l'état « LISTENING » est localisé, pas l'adresse). Fenêtre masquée.
#[cfg(target_os = "windows")]
fn listening_ports_for_pids(pids: &[u32]) -> Vec<u16> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = match std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut ports = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // TCP  <local>  <remote>  <state>  <pid>
        if cols.len() < 5 || !cols[0].eq_ignore_ascii_case("TCP") {
            continue;
        }
        if cols[2] != "0.0.0.0:0" && cols[2] != "[::]:0" {
            continue; // pas un socket en écoute
        }
        let Ok(pid) = cols[4].parse::<u32>() else { continue };
        if !pids.contains(&pid) {
            continue;
        }
        if let Some(port) = cols[1].rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    ports
}

// ── Spawn headless d'agy pour le repli manuel (Windows) ──────────────────────────

/// Lance `agy.exe` en arrière-plan sans fenêtre ni entrée/sortie ; retourne son PID. Le serveur
/// local bind son port pendant l'init (avant toute action). On n'envoie AUCUN prompt.
#[cfg(target_os = "windows")]
fn spawn_agy_headless() -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = agy_exe_path().ok_or_else(|| "agy_not_found".to_string())?;
    let child = std::process::Command::new(exe)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn_error: {e}"))?;
    Ok(child.id())
}

#[cfg(not(target_os = "windows"))]
fn spawn_agy_headless() -> Result<u32, String> {
    Err("agy_not_running".to_string())
}

/// Localise `agy.exe` : d'abord l'emplacement d'install standard `%LOCALAPPDATA%\agy\bin`,
/// puis le PATH (`where`).
#[cfg(target_os = "windows")]
fn agy_exe_path() -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = std::path::PathBuf::from(local).join("agy").join("bin").join(AGY_PROCESS_NAME);
        if p.exists() {
            return Some(p);
        }
    }
    let out = std::process::Command::new("where")
        .arg(AGY_PROCESS_NAME)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(std::path::PathBuf::from)
}

/// Tue un process et ses enfants (`taskkill /T /F`). Fenêtre masquée.
#[cfg(target_os = "windows")]
fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(target_os = "windows"))]
fn kill_process_tree(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::{parse_claude_usage, parse_codex_usage, unix_to_iso8601};
    use super::{group_label, model_family, order_ports, parse_antigravity_usage};
    use super::{prime_request_body, primary_window_closed, PRIME_MODEL};
    use super::parse_grok_usage;
    use crate::usage::{UsageStats, WindowUsage};

    fn win(id: &str, resets_at: Option<&str>) -> WindowUsage {
        WindowUsage {
            id: id.to_string(),
            label: id.to_string(),
            utilization: 0.0,
            resets_at: resets_at.map(str::to_string),
        }
    }

    // ── order_ports (fix effet de bord 6.24 : ne pas re-taper les ports TLS agy à chaque tick) ──
    #[test]
    fn order_ports_met_le_port_cache_en_tete() {
        // Port bon connu présent → sondé en premier, les autres suivent dans l'ordre d'origine.
        assert_eq!(order_ports(42137, &[8000, 42137, 9001]), vec![42137, 8000, 9001]);
    }

    #[test]
    fn order_ports_cache_absent_ou_zero_garde_ordre() {
        // cache == 0 (aucun) → ordre inchangé.
        assert_eq!(order_ports(0, &[8000, 42137, 9001]), vec![8000, 42137, 9001]);
        // cache périmé (agy redémarré, port disparu) → ignoré, ordre inchangé, aucun port hors liste.
        assert_eq!(order_ports(55555, &[8000, 42137, 9001]), vec![8000, 42137, 9001]);
    }

    #[test]
    fn order_ports_sans_doublon_et_borne_a_la_liste() {
        // Le port caché n'est jamais dupliqué ni ajouté s'il n'appartient pas à agy.
        assert_eq!(order_ports(8000, &[8000]), vec![8000]);
        assert!(order_ports(1234, &[]).is_empty());
    }

    // ── Claude ─────────────────────────────────────────────────────────────────
    #[test]
    fn claude_usage_liste_ordonnée_et_labellisée() {
        // L'API retourne snake_case, utilization en 0-100. Le fetcher construit une liste
        // ORDONNÉE (id/label portés par le fetcher), pas une désérialisation directe.
        let json = r#"{
            "five_hour":{"utilization":35.0,"resets_at":"2026-05-15T18:00:00Z"},
            "seven_day":{"utilization":47.0,"resets_at":"2026-05-21T00:00:00Z"},
            "seven_day_sonnet":{"utilization":30.0,"resets_at":"2026-05-21T00:00:00Z"},
            "seven_day_opus":{"utilization":10.0,"resets_at":"2026-05-21T00:00:00Z"}
        }"#;
        let stats = parse_claude_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 4);
        // Ordre CANONIQUE (pas l'ordre alphabétique du JSON, qui placerait opus avant sonnet).
        assert_eq!(stats.windows[0].id, "primary");
        assert_eq!(stats.windows[0].label, "Current window");
        assert!((stats.windows[0].utilization - 35.0).abs() < 0.001);
        assert_eq!(stats.windows[1].id, "weekly");
        assert_eq!(stats.windows[1].label, "Weekly (all models)");
        assert!((stats.windows[1].utilization - 47.0).abs() < 0.001);
        assert_eq!(stats.windows[2].id, "weekly_sonnet");
        assert_eq!(stats.windows[2].label, "Weekly Sonnet");
        assert_eq!(stats.windows[3].id, "weekly_opus");
        assert_eq!(stats.windows[3].label, "Weekly Opus");
        assert!((stats.windows[3].utilization - 10.0).abs() < 0.001);
    }

    #[test]
    fn claude_usage_fenetre_absente_non_poussée() {
        // seven_day_opus absent → non poussé (pas de trou) ; resets_at null autorisé ; les clés
        // sans quota (`seven_day_oauth_apps: null`, `extra_usage` sans utilization) sont ignorées.
        let json = r#"{
            "five_hour":{"utilization":20.0,"resets_at":"2026-05-15T18:00:00Z"},
            "seven_day":{"utilization":50.0,"resets_at":"2026-05-21T00:00:00Z"},
            "seven_day_sonnet":{"utilization":15.0,"resets_at":null},
            "seven_day_oauth_apps":null,
            "extra_usage":{"is_enabled":false}
        }"#;
        let stats = parse_claude_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 3, "opus absent + clés sans quota ignorées");
        assert_eq!(stats.windows[0].id, "primary");
        assert!(stats.windows.iter().all(|w| w.id != "weekly_opus"));
        let sonnet = stats.windows.iter().find(|w| w.id == "weekly_sonnet").unwrap();
        assert!(sonnet.resets_at.is_none(), "resets_at null accepté");
    }

    #[test]
    fn claude_usage_fenetre_inedite_incluse_avec_label_derive() {
        // AC4 — clé fenêtre inconnue portant `utilization` → incluse EN FIN de liste (après les
        // connues), id = clé brute, label = titre-case dérivé. C'est l'objectif central.
        let json = r#"{
            "five_hour":{"utilization":10.0,"resets_at":null},
            "monthly":{"utilization":42.0,"resets_at":"2026-06-01T00:00:00Z"}
        }"#;
        let stats = parse_claude_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 2);
        assert_eq!(stats.windows[0].id, "primary", "connue d'abord");
        assert_eq!(stats.windows[1].id, "monthly", "inédite ensuite");
        assert_eq!(stats.windows[1].label, "Monthly");
        assert!((stats.windows[1].utilization - 42.0).abs() < 0.001);
        assert_eq!(stats.windows[1].resets_at.as_deref(), Some("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn claude_usage_json_invalide_erreur() {
        let err = parse_claude_usage("<html>nope</html>").unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn claude_usage_objet_vide_donne_erreur() {
        // 200 avec JSON valide mais aucune fenêtre exploitable → Err claire (symétrique Codex),
        // pas un succès vide qui blanchirait l'onglet + armerait le cooldown.
        assert!(parse_claude_usage("{}").unwrap_err().starts_with("parse_error"));
        let err = parse_claude_usage(r#"{"extra_usage":{"is_enabled":false}}"#).unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn claude_usage_cle_inedite_collision_id_ignoree() {
        // Clé inédite littéralement nommée comme un id connu ("weekly") → NON poussée en doublon
        // (sinon collision de `key` React côté UI). La fenêtre connue `seven_day` reste seule.
        let json = r#"{
            "seven_day":{"utilization":50.0,"resets_at":null},
            "weekly":{"utilization":20.0,"resets_at":null}
        }"#;
        let stats = parse_claude_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 1, "pas de doublon d'id");
        assert_eq!(stats.windows[0].id, "weekly");
        assert!((stats.windows[0].utilization - 50.0).abs() < 0.001, "la fenêtre connue prime");
    }

    // ── Amorçage de fenêtre (Story 6.25) ─────────────────────────────────────────

    #[test]
    fn primary_window_closed_vraie_si_primary_sans_resets_at() {
        // Fermée : `primary` présente SANS `resets_at`.
        let stats = UsageStats { windows: vec![win("primary", None)] };
        assert!(primary_window_closed(&stats));
    }

    #[test]
    fn primary_window_closed_fausse_si_primary_ouverte() {
        // Ouverte : `primary` porte un `resets_at` → rien à amorcer.
        let stats = UsageStats {
            windows: vec![win("primary", Some("2026-07-12T18:00:00Z"))],
        };
        assert!(!primary_window_closed(&stats));
    }

    #[test]
    fn primary_window_closed_fausse_si_primary_absente() {
        // `primary` absente de la liste → aucune action (cas dégénéré).
        let stats = UsageStats {
            windows: vec![win("weekly", Some("2026-07-19T00:00:00Z"))],
        };
        assert!(!primary_window_closed(&stats));
    }

    #[test]
    fn primary_window_closed_ignore_fenetre_non_primary_sans_resets_at() {
        // PIÈGE : une fenêtre NON-`primary` sans `resets_at` (ex. `weekly` hebdo active) ne doit
        // PAS déclencher l'amorçage — seule la fenêtre 5h `primary` compte.
        let stats = UsageStats {
            windows: vec![
                win("primary", Some("2026-07-12T18:00:00Z")),
                win("weekly", None),
            ],
        };
        assert!(!primary_window_closed(&stats));
    }

    #[test]
    fn prime_request_body_plancher_conforme() {
        let body = prime_request_body(PRIME_MODEL);
        assert_eq!(body["model"], PRIME_MODEL);
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], ".");
    }

    // ── Codex ──────────────────────────────────────────────────────────────────

    #[test]
    fn codex_usage_enveloppe_reelle_5h_et_7j() {
        // Forme réelle (spike T0) : rate_limits.rate_limit.{primary,secondary}_window.
        let json = r#"{
            "rate_limits": {
                "plan_type": "plus",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window":   { "used_percent": 12, "limit_window_seconds": 18000,  "reset_after_seconds": 5400,   "reset_at": 1717500000 },
                    "secondary_window": { "used_percent": 34, "limit_window_seconds": 604800, "reset_after_seconds": 200000, "reset_at": 1718000000 }
                }
            },
            "rate_limit_reset_credits": {}
        }"#;
        let stats = parse_codex_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 2, "primaire + secondaire, pas de découpe par modèle");
        // used_percent est en 0-100 (comme Claude) → utilization direct ; id/label portés par le fetcher.
        assert_eq!(stats.windows[0].id, "primary");
        assert_eq!(stats.windows[0].label, "Current window");
        assert!((stats.windows[0].utilization - 12.0).abs() < 0.001);
        assert_eq!(stats.windows[1].id, "weekly");
        assert_eq!(stats.windows[1].label, "Weekly", "label Codex distinct du « Weekly (all models) » Claude");
        assert!((stats.windows[1].utilization - 34.0).abs() < 0.001);
        // reset_at (unix) converti en ISO8601.
        assert_eq!(stats.windows[0].resets_at.as_deref(), Some("2024-06-04T11:20:00Z"));
    }

    #[test]
    fn codex_usage_fenetre_absente_donne_none() {
        // Seule la fenêtre primaire est présente → une seule fenêtre dans la liste (pas de trou).
        let json = r#"{"rate_limits":{"rate_limit":{"primary_window":{"used_percent":5,"reset_at":1717500000}}}}"#;
        let stats = parse_codex_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 1);
        assert_eq!(stats.windows[0].id, "primary");
    }

    #[test]
    fn codex_usage_reset_after_seconds_en_repli() {
        // reset_at absent → repli sur reset_after_seconds (delta) : on obtient bien un ISO.
        let json = r#"{"primary_window":{"used_percent":8,"reset_after_seconds":3600}}"#;
        let stats = parse_codex_usage(json).unwrap();
        assert!(stats.windows[0].resets_at.is_some());
    }

    #[test]
    fn codex_usage_json_invalide_erreur() {
        let err = parse_codex_usage("<html>nope</html>").unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn codex_usage_objet_vide_donne_erreur() {
        // 200 avec JSON valide mais sans fenêtre reconnue → Err claire (pas de succès
        // vide qui blanchirait l'onglet + armerait le cooldown), et surtout pas de panique.
        let err = parse_codex_usage("{}").unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn codex_usage_percent_absent_donne_fenetre_none() {
        // Fenêtre présente mais `used_percent` absent → fenêtre None (pas de 0% fabriqué).
        // Seule la secondaire est exploitable ici.
        let json = r#"{"rate_limits":{"rate_limit":{
            "primary_window":{"reset_at":1717500000},
            "secondary_window":{"used_percent":40,"reset_at":1718000000}
        }}}"#;
        let stats = parse_codex_usage(json).unwrap();
        // primary sans used_percent → ignorée ; seule la secondaire subsiste, seule dans la liste.
        assert_eq!(stats.windows.len(), 1);
        assert_eq!(stats.windows[0].id, "weekly");
    }

    #[test]
    fn codex_usage_percent_hors_plage_borne() {
        // used_percent 150 → borné à 100 (pas de label « 150% »).
        let json = r#"{"primary_window":{"used_percent":150,"reset_at":1717500000}}"#;
        let stats = parse_codex_usage(json).unwrap();
        assert!((stats.windows[0].utilization - 100.0).abs() < 0.001);
    }

    #[test]
    fn unix_to_iso8601_epoch_et_date_connue() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_iso8601(1717500000), "2024-06-04T11:20:00Z");
    }

    // ── Grok (billing?format=credits, 6.32) ─────────────────────────────────────

    // Réponse RÉELLE capturée (HTTP 200, 2026-07-16). Fenêtre HEBDO, usage faible (1 %).
    const REAL_GROK_BILLING: &str = r#"{
      "config": {
        "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY", "start": "2026-07-13T13:12:56.419854+00:00", "end": "2026-07-20T13:12:56.419854+00:00" },
        "creditUsagePercent": 1.0,
        "onDemandCap": { "val": 0 },
        "onDemandUsed": { "val": 0 },
        "productUsage": [ { "product": "GrokBuild", "usagePercent": 1.0 } ],
        "billingPeriodEnd": "2026-07-20T13:12:56.419854+00:00"
      }
    }"#;

    #[test]
    fn grok_usage_reponse_reelle_produit_une_fenetre_hebdo() {
        let stats = parse_grok_usage(REAL_GROK_BILLING).unwrap();
        assert_eq!(stats.windows.len(), 1, "une fenêtre par produit (GrokBuild)");
        let w = &stats.windows[0];
        assert_eq!(w.id, "grokbuild");
        assert_eq!(w.label, "GrokBuild credits");
        // usagePercent 1.0 = 1 % utilisé (0–100 direct, PAS une fraction ×100 → surtout pas 100 %).
        assert!((w.utilization - 1.0).abs() < 0.001, "got {}", w.utilization);
        assert_eq!(w.resets_at.as_deref(), Some("2026-07-20T13:12:56.419854+00:00"));
    }

    #[test]
    fn grok_usage_multi_produits() {
        let json = r#"{"config":{"currentPeriod":{"end":"2026-07-20T00:00:00Z"},"creditUsagePercent":50.0,
            "productUsage":[{"product":"GrokBuild","usagePercent":30.0},{"product":"GrokCode","usagePercent":70.0}]}}"#;
        let stats = parse_grok_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 2);
        assert_eq!(stats.windows[0].id, "grokbuild");
        assert!((stats.windows[0].utilization - 30.0).abs() < 0.001);
        assert_eq!(stats.windows[1].id, "grokcode");
        assert!((stats.windows[1].utilization - 70.0).abs() < 0.001);
    }

    #[test]
    fn grok_usage_repli_credit_usage_percent() {
        // productUsage absent → une fenêtre unique dérivée de creditUsagePercent.
        let json = r#"{"config":{"currentPeriod":{"end":"2026-07-20T00:00:00Z"},"creditUsagePercent":12.5}}"#;
        let stats = parse_grok_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 1);
        assert_eq!(stats.windows[0].id, "weekly");
        assert_eq!(stats.windows[0].label, "Weekly credits");
        assert!((stats.windows[0].utilization - 12.5).abs() < 0.001);
        assert_eq!(stats.windows[0].resets_at.as_deref(), Some("2026-07-20T00:00:00Z"));
    }

    #[test]
    fn grok_usage_borne_utilization() {
        let json = r#"{"config":{"productUsage":[{"product":"GrokBuild","usagePercent":150.0}]}}"#;
        let stats = parse_grok_usage(json).unwrap();
        assert!((stats.windows[0].utilization - 100.0).abs() < 0.001);
    }

    #[test]
    fn grok_usage_config_absent_erreur() {
        let err = parse_grok_usage(r#"{"foo":1}"#).unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn grok_usage_config_vide_erreur() {
        // config sans productUsage NI creditUsagePercent → Err (jamais un succès vide, garde 6.15).
        let err = parse_grok_usage(r#"{"config":{"currentPeriod":{"end":"2026-07-20T00:00:00Z"}}}"#).unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn grok_usage_json_invalide_erreur() {
        let err = parse_grok_usage("<html>nope</html>").unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    // ── Antigravity (source locale GetUserStatus) ────────────────────────────────

    // Réponse RÉELLE capturée sur le serveur local d'agy (compte Free/Pro d'exemple, 2026-07-09).
    // Gemini : `remainingFraction` ABSENT (= 0 → 100% utilisé), reset 12/07 ; Claude+GPT :
    // remainingFraction 0.931479 (≈7% utilisé), reset 16/07. C'est exactement ce qu'affiche
    // agy `/usage` (« GEMINI MODELS 0% restant » / « CLAUDE AND GPT MODELS 93% restant »).
    const REAL_GET_USER_STATUS: &str = r#"{
      "userStatus": {
        "name": "Alex",
        "email": "alex@example.com",
        "cascadeModelConfigData": {
          "clientModelConfigs": [
            { "label": "Gemini 3.5 Flash (High)", "quotaInfo": { "resetTime": "2026-07-12T11:29:04Z" } },
            { "label": "Gemini 3.5 Flash (Low)",  "quotaInfo": { "resetTime": "2026-07-12T11:29:04Z" } },
            { "label": "Gemini 3.1 Pro (Low)",    "quotaInfo": { "resetTime": "2026-07-12T11:29:04Z" } },
            { "label": "Claude Sonnet 4.6 (Thinking)", "quotaInfo": { "remainingFraction": 0.931479, "resetTime": "2026-07-16T12:28:17Z" } },
            { "label": "Claude Opus 4.6 (Thinking)",   "quotaInfo": { "remainingFraction": 0.931479, "resetTime": "2026-07-16T12:28:17Z" } },
            { "label": "GPT-OSS 120B (Medium)",        "quotaInfo": { "remainingFraction": 0.931479, "resetTime": "2026-07-16T12:28:17Z" } },
            { "label": "Gemini 3.5 Flash (Medium)", "quotaInfo": { "resetTime": "2026-07-12T11:29:04Z" } }
          ]
        }
      }
    }"#;

    #[test]
    fn antigravity_usage_groupe_gemini_et_claude_gpt_comme_agy() {
        let stats = parse_antigravity_usage(REAL_GET_USER_STATUS).unwrap();
        assert_eq!(stats.windows.len(), 2, "2 groupes : Gemini / Claude & GPT");

        // Ordre préservé : Gemini d'abord (1er dans la liste), puis Claude & GPT.
        let gem = &stats.windows[0];
        assert_eq!(gem.label, "Gemini Models");
        assert_eq!(gem.id, "gemini");
        // remainingFraction absent = 0 → 100% utilisé.
        assert!((gem.utilization - 100.0).abs() < 0.01, "got {}", gem.utilization);
        assert_eq!(gem.resets_at.as_deref(), Some("2026-07-12T11:29:04Z"));

        let cg = &stats.windows[1];
        assert_eq!(cg.label, "Claude & GPT Models");
        assert_eq!(cg.id, "claude_gpt");
        // 1 - 0.931479 = 0.068521 → ~6.85% utilisé.
        assert!((cg.utilization - 6.8521).abs() < 0.05, "got {}", cg.utilization);
        assert_eq!(cg.resets_at.as_deref(), Some("2026-07-16T12:28:17Z"));
    }

    #[test]
    fn antigravity_usage_ordre_deterministe_malgre_ordre_brut_instable() {
        // Ordre brut « à l'envers » (Claude & GPT d'abord, GPT AVANT Claude, reset lointain) :
        // le parseur doit QUAND MÊME rendre Gemini (reset proche) en 1er, et « Claude & GPT »
        // (familles triées), pour matcher l'affichage stable d'agy `/usage`.
        let json = r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[
            {"label":"GPT-OSS 120B (Medium)",       "quotaInfo":{"remainingFraction":0.9,"resetTime":"2026-07-16T12:28:17Z"}},
            {"label":"Claude Sonnet 4.6 (Thinking)","quotaInfo":{"remainingFraction":0.9,"resetTime":"2026-07-16T12:28:17Z"}},
            {"label":"Gemini 3.5 Flash (High)",     "quotaInfo":{"resetTime":"2026-07-12T11:29:04Z"}}
        ]}}}"#;
        let stats = parse_antigravity_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 2);
        assert_eq!(stats.windows[0].label, "Gemini Models", "reset le plus proche d'abord");
        assert_eq!(stats.windows[1].label, "Claude & GPT Models", "familles triées, pas « GPT & Claude »");
        assert_eq!(stats.windows[1].id, "claude_gpt");
    }

    #[test]
    fn antigravity_usage_remaining_absent_vaut_zero() {
        // quotaInfo sans remainingFraction → 0 → 100% utilisé (protobuf omet le zéro).
        let json = r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[
            {"label":"Gemini 3 Flash","quotaInfo":{"resetTime":"2026-07-12T00:00:00Z"}}
        ]}}}"#;
        let stats = parse_antigravity_usage(json).unwrap();
        assert!((stats.windows[0].utilization - 100.0).abs() < 0.01);
    }

    #[test]
    fn antigravity_usage_label_vide_ignore() {
        // Modèle sans label → ignoré ; le modèle valide subsiste, groupe non vide.
        let json = r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[
            {"label":"","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-07-12T00:00:00Z"}},
            {"label":"Gemini 3 Pro","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-07-12T00:00:00Z"}}
        ]}}}"#;
        let stats = parse_antigravity_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 1);
        assert_eq!(stats.windows[0].label, "Gemini Models");
        assert!((stats.windows[0].utilization - 50.0).abs() < 0.01);
    }

    #[test]
    fn antigravity_usage_pas_de_configs_erreur() {
        // userStatus sans clientModelConfigs → Err (jamais un succès vide).
        let err = parse_antigravity_usage(r#"{"userStatus":{}}"#).unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn antigravity_usage_configs_vide_erreur() {
        let err = parse_antigravity_usage(
            r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[]}}}"#,
        )
        .unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn antigravity_usage_json_invalide_erreur() {
        let err = parse_antigravity_usage("<html>nope</html>").unwrap_err();
        assert!(err.starts_with("parse_error"), "got: {err}");
    }

    #[test]
    fn antigravity_model_family_generique() {
        assert_eq!(model_family("Gemini 3.5 Flash (High)"), "Gemini");
        assert_eq!(model_family("Claude Sonnet 4.6 (Thinking)"), "Claude");
        assert_eq!(model_family("GPT-OSS 120B (Medium)"), "GPT");
        // Modèle futur inconnu → tête alphabétique, sans table en dur.
        assert_eq!(model_family("Grok 5 Fast"), "Grok");
    }

    #[test]
    fn antigravity_group_label() {
        assert_eq!(group_label(&["Gemini".to_string()]), "Gemini Models");
        assert_eq!(
            group_label(&["Claude".to_string(), "GPT".to_string()]),
            "Claude & GPT Models"
        );
    }

    #[test]
    fn antigravity_model_family_tete_non_alphabetique_replie() {
        // Label sans tête alphabétique (chiffre) → repli sur le label nettoyé (id/label non vides),
        // pas une famille « » qui donnerait un id vide (collision de clé React) + label « Models ».
        assert_eq!(model_family("4o-mini"), "4o-mini");
        // Espace de tête → trim avant extraction.
        assert_eq!(model_family("  Gemini 3 Flash"), "Gemini");
    }

    #[test]
    fn antigravity_usage_id_unique_si_deux_groupes_meme_famille() {
        // Deux groupes de quota DISTINCTS (reset/remaining différents) mais même famille (Gemini)
        // → ids dé-dupliqués (« gemini », « gemini_2 ») pour ne pas casser la clé React `key={w.id}`.
        let json = r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[
            {"label":"Gemini 3 Flash","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-07-12T00:00:00Z"}},
            {"label":"Gemini 3 Pro","quotaInfo":{"remainingFraction":0.2,"resetTime":"2026-07-20T00:00:00Z"}}
        ]}}}"#;
        let stats = parse_antigravity_usage(json).unwrap();
        assert_eq!(stats.windows.len(), 2);
        // Tri par reset croissant : 07-12 d'abord.
        assert_eq!(stats.windows[0].id, "gemini");
        assert_eq!(stats.windows[1].id, "gemini_2", "id dé-dupliqué");
        assert_eq!(stats.windows[0].label, "Gemini Models");
        assert_eq!(stats.windows[1].label, "Gemini Models");
    }
}
