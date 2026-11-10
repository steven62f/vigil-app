// Bridge entre Kimi CLI (Moonshot) et Vigil (Epic 9 / story 9.1).
//
// ⚠️ **La prémisse d'origine de l'Epic est MORTE.** L'hypothèse « Kimi est byte-compatible Claude,
// donc un simple forward suffit » (héritée du spike Open Island, qui réutilise `ClaudeHookPayload`)
// est **fausse** — vérifiée sur les 7 events réellement observés, dans les deux modes de permission
// (spike e2e 2026-07-22). Ne PAS repartir d'un pass-through façon `vigil-claude` : le patron le plus
// proche est **`vigil-grok`** (couche de traduction + capacités conditionnées au mode).
//
// **Ce qui reste vrai et précieux** : le payload Kimi est déjà **snake_case** et porte déjà
// `hook_event_name` en **PascalCase**, c.-à-d. exactement le tag de dispatch de `HookEvent`
// (`#[serde(tag = "hook_event_name")]`). La traduction est donc BEAUCOUP plus légère que celle de
// `vigil-grok` (camelCase intégral) ou `vigil-antigravity`. ⚠️ Corollaire piège : **ne pas re-mapper
// l'event** — le réflexe « traduire comme Grok » (snake → Pascal) casserait tout.
//
// **Les 4 divergences réelles qui obligent quand même à traduire :**
//   1. `transcript_path` — **absent de TOUS les payloads**, alors que c'est la clé primaire d'un
//      agent dans Vigil (clé du `HashMap` du state engine, `PendingApprovals`,
//      `agentNotifs:{transcript_path}`, `request_resolved`, `transcript_hash`/`session_meta`). Il est
//      **résolu** via l'index de Kimi (`~/.kimi-code/session_index.jsonl`), cf. `resolve_session_in`.
//   2. `prompt` (`UserPromptSubmit`) — tableau de blocs `[{"type":"text","text":"…"}]` → **chaîne**.
//   3. `tool_input` (`Write`) — clé `path` → alias **additif** `file_path` (lu par
//      `derive_activity_label` côté state engine ET par `PermissionBlock.tsx`).
//   4. `tool_output` (`PostToolUse`) — chaîne → `tool_response` (**requis** par `PostToolUsePayload` :
//      l'omettre ferait échouer la désérialisation EN SILENCE côté pipe → aucun event).
//
// **Kimi est un jumeau de Grok** : le mode de permission décide de tout. En `manual`/`auto`,
// l'utilisateur est le portier et les hooks ne peuvent qu'opposer un veto (`permissionDecision:
// "allow"` est **ignoré** — choix de sécurité de Moonshot, mesuré deux fois) → `read_only = true`,
// awareness seule. En `yolo`, le hook devient le seul portier → `read_only = false`. Le mode ne
// figure PAS dans le payload : il est lu dans le `wire.jsonl` de la session (`permission.set_mode`).
//
// **Story 9.3 — le contrôle est branché.** En `yolo` (`read_only = false`), un `PreToolUse`
// SYNTHÉTISE un `PermissionRequest(read_only:false)` (le manager ne parque que sur cet event), l'écrit
// sur le pipe ET **BLOQUE** en lisant la décision de Vigil (`emit_and_await`, patron
// `vigil-grok::handle_approve`) ; `convert_response_to_kimi()` mappe cette décision sur stdout + code
// de sortie. En `manual`/`auto` (`read_only = true`), le chemin fire-and-forget de la 9.1 est
// INCHANGÉ (awareness read-only). La règle unifiée est : **tout payload qui doit produire un
// `PermissionRequest(read_only:false)` passe par le chemin bloquant** (`PreToolUse` en `yolo`, AC2 —
// ET `PermissionRequest` en `yolo`, bascule de mode en cours de session, D1/AC3) ; tout le reste reste
// `emit()`. L'installation des hooks TOML est en **9.2**.
//
// ⚠️ **stdout est un canal de contexte agent chez Kimi** : en sortie 0, la doc Moonshot indique que
// stdout peut être ajouté au contexte de la conversation. Tout `println!` de debug polluerait la
// conversation de l'utilisateur → utiliser `debug_log` (stderr, conditionné à `VIGIL_DEBUG`).

use std::io::Write;
use std::process;

use serde_json::{json, Value};
use vigil_hook_core::{
    check_tool_enabled, debug_log, inject_parent_pid, inject_source_tool, invoked_by_grok,
    open_vigil_pipe, read_pipe_response, read_stdin, write_payload,
};

const SOURCE_TOOL: &str = "kimi";
// Process cible dont on remonte l'arbre pour trouver le terminal parent (best-effort, pour le
// nettoyage par `pid_watcher`). Le nom est **ignoré** par `vigil-hook-core` depuis la 6.38 (repérage
// générique du CLI) ; l'argument est conservé pour la stabilité de l'API.
const PROCESS_NAME: &str = "kimi.exe";

/// Réponse de refus attendue par Kimi sur **stdout**, avec code de sortie **0** — chemin prouvé au
/// spike dans les deux modes (`manual` et `yolo`). Le chemin documenté « sortie 2 + stderr » est
/// mesuré équivalent : il n'apporte rien et divergerait des autres binaires.
///
/// Libellé de raison **neutre** : le deny peut venir de l'utilisateur, d'une règle d'auto-permission
/// ou du timeout côté `PipeManager` — éviter « par l'utilisateur », inexact dans ces cas
/// (cf. `GROK_DENY`, `vigil-grok/src/main.rs`).
const KIMI_DENY: &str =
    r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"Refusé via Vigil"}}"#;

/// Nombre de tentatives de résolution dans l'index. Voir [`RESOLVE_RETRY_DELAY_MS`].
const RESOLVE_RETRY_ATTEMPTS: u32 = 3;
/// Attente entre deux tentatives, en millisecondes (borne dure du total : ~200 ms).
///
/// **T0 mesuré (2026-07-23, 3 lancements réels de `kimi -p`)** : l'entrée de `session_index.jsonl`
/// est DÉJÀ écrite quand le hook `SessionStart` lit le fichier (trouvée à la 1ʳᵉ tentative, 3 fois
/// sur 3). Le retry n'est donc PAS sur le chemin nominal — c'est une assurance à coût nul : la sonde
/// de mesure était un script PowerShell (démarrage lent), un binaire natif part plus tôt et pourrait
/// devancer l'écriture de l'index. Sans retry, on perdrait le tout premier event d'une session.
const RESOLVE_RETRY_DELAY_MS: u64 = 50;

fn main() {
    // Garde anti-double-fire (story 6.26) : Grok scanne les emplacements de hooks d'autres outils et
    // exécute les binaires qui s'y trouvent, en injectant `GROK_SESSION_ID`. Kimi ne définit JAMAIS
    // cette variable → régression zéro, défense en profondeur (patron des binaires non-Grok).
    if invoked_by_grok() {
        debug_log("vigil-kimi: invoqué par Grok (GROK_SESSION_ID) → sortie sans traitement");
        process::exit(0);
    }

    let Some(input) = read_stdin() else {
        process::exit(0);
    };

    if !check_tool_enabled(SOURCE_TOOL) {
        process::exit(0);
    }

    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let event = str_field(&v, "hook_event_name");
    let session_id = str_field(&v, "session_id");

    // Résolution d'identité : sans `transcript_path`, aucun event Kimi n'est exploitable. Échec →
    // on n'émet RIEN (plutôt qu'un event à clé bancale) et on sort en 0 : ne jamais bloquer l'agent.
    let Some(transcript_path) = resolve_transcript_path(&session_id) else {
        debug_log(&format!(
            "vigil-kimi: session {session_id} introuvable dans session_index.jsonl → aucun event émis"
        ));
        process::exit(0);
    };

    // Le mode n'est lu QUE quand il sert au routage ou au rendu : `PreToolUse` (9.3 — décide du
    // blocage en `yolo`) ET `PermissionRequest` (9.1 — porte `read_only`). Pour tous les autres
    // events, la valeur est inerte → défaut sûr `true`, et on n'engage PAS la lecture intégrale du
    // `wire.jsonl` (append-only, croissant) inutilement. ⚠️ Conséquence 9.3 assumée : chaque
    // `PreToolUse` lit désormais le `wire.jsonl` (coût mesuré en T0, bénin) — le `set_mode` est en
    // tête de fichier, cf. `parse_mode_from_wire`.
    let read_only = if event == "PreToolUse" || event == "PermissionRequest" {
        read_only_for_session(&transcript_path)
    } else {
        true
    };

    match route(&event, read_only) {
        // Chemin BLOQUANT (AC2/AC3, patron `vigil-grok::handle_approve`) : synthétise un
        // `PermissionRequest(read_only:false)`, l'écrit sur le pipe ET lit la décision de Vigil.
        // Ferme explicitement le défer D1 : plus aucun `PermissionRequest(read_only:false)` n'est
        // émis sans que sa réponse soit lue. `emit_and_await` ne retourne jamais.
        Route::Blocking => match permission_gate_payload(&input, &transcript_path) {
            Some(payload) => emit_and_await(&payload),
            // Synthèse impossible (`tool_name` vide) → fail-open : ne jamais bloquer Kimi.
            None => process::exit(0),
        },
        // Chemin fire-and-forget INCHANGÉ depuis 9.1 (monitoring + awareness read-only). Event
        // inconnu / non mappé / JSON invalide → `translate` retourne `None` : sortie propre sans
        // écriture et sans panic (fail-open, ne bloque jamais Kimi).
        Route::FireAndForget => {
            if let Some(translated) = translate(&input, &transcript_path, read_only) {
                emit(&translated);
            }
        }
    }

    process::exit(0);
}

/// Décision de routage d'un event Kimi (9.3). **Pure** (aucun I/O, aucun `process::exit`) → c'est la
/// décision qu'on teste, pas l'effet de bord.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// L'event doit produire un `PermissionRequest(read_only:false)` sur le pipe : il ÉCRIT **et**
    /// LIT la réponse (patron Grok). C'est ce qui referme le défer D1.
    Blocking,
    /// `translate` + `emit`, sans lire de réponse (monitoring + awareness read-only, 9.1 inchangée).
    FireAndForget,
}

/// Règle unifiée de la story : **tout payload qui doit produire un `PermissionRequest(read_only:false)`
/// passe par le chemin bloquant ; tout le reste reste fire-and-forget.** L'event déclencheur du
/// blocage est `PreToolUse` (cas nominal `yolo`, AC2) OU `PermissionRequest` (bascule de mode en cours
/// de session, D1/AC3) quand `read_only` est faux. En `manual`/`auto` (`read_only` vrai) rien ne
/// bloque : awareness read-only inchangée de la 9.1.
fn route(event: &str, read_only: bool) -> Route {
    if !read_only && matches!(event, "PreToolUse" | "PermissionRequest") {
        Route::Blocking
    } else {
        Route::FireAndForget
    }
}

// ─────────────────────────────────────────────────────────────────────
// Résolution d'identité (AC1, AC2) — index Kimi → transcript_path
// ─────────────────────────────────────────────────────────────────────

/// Résout le `transcript_path` d'une session Kimi, avec **retry borné** sur le seul cas « session
/// absente de l'index » (cf. [`RESOLVE_RETRY_DELAY_MS`]). `None` = abandon → aucun event émis.
fn resolve_transcript_path(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let home = kimi_home()?;
    for attempt in 0..RESOLVE_RETRY_ATTEMPTS {
        // ⚠️ Le `?` est VOULU : un index absent ou illisible est un échec **permanent** (Kimi n'est
        // pas installé, `USERPROFILE` pointe ailleurs, droits refusés) — retenter n'y changerait
        // rien et ferait payer 100 ms de `sleep` à CHAQUE event de la session. L'AC2 borne le retry
        // au seul cas « la session n'est pas ENCORE écrite dans l'index », c.-à-d. index lisible
        // mais entrée manquante : c'est exactement ce que couvre la boucle ci-dessous.
        let index = read_index_in(&home)?;
        if let Some(path) = find_in_index(&index, session_id) {
            return Some(path);
        }
        // Dernière tentative : ne pas dormir pour rien.
        if attempt + 1 < RESOLVE_RETRY_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(RESOLVE_RETRY_DELAY_MS));
        }
    }
    None
}

/// Cœur de la résolution, **paramétré par le home** (→ testable sur un index temporaire, sans
/// dépendre de `~/.kimi-code/`). Patron `grok_transcript_path_in`.
///
/// Kimi tient son propre index append-only, une ligne JSON par session :
/// ```jsonc
/// {"sessionId":"session_…","sessionDir":"C:/Users/…/sessions/wd_<hash>/session_…","workDir":"C:/…"}
/// ```
/// → `transcript_path` = `<sessionDir>/agents/main/wire.jsonl`.
///
/// **Le chemin n'est PAS constructible par dérivation** (≠ Grok) : le hash de workspace
/// (`wd_dev_cfa108596ad6`) est opaque. L'index est la seule source.
///
/// **Parcours INVERSE, première correspondance retenue** : la session courante est en fin de
/// fichier, et une même session peut y figurer plusieurs fois (reprise) — la dernière ligne fait foi.
///
/// **`agents/main` est en dur, et c'est CORRECT** (vérifié au spike, manche 7) : les sous-agents ont
/// bien leur propre dossier sur disque (`agents/agent-0/wire.jsonl`, déclaré dans `state.json` avec
/// `type:"sub"`), mais **aucun hook n'y renvoie** — *tous* les events, y compris ceux déclenchés PAR
/// un sous-agent, portent le `session_id` du parent et aucun identifiant d'agent.
///
/// ⚠️ **Aucune normalisation de séparateur** : le chemin est produit tel que l'index l'écrit, en
/// **slashs avant**. C'est à la fois la clé de dédup et un vrai chemin de fichier que la Story 9.4
/// devra retrouver à l'identique — normaliser en backslash ferait diverger les deux.
/// Composition des deux moitiés, **paramétrée par le home** → utilisée par les tests pour valider la
/// résolution de bout en bout sur un index temporaire, sans dépendre de `~/.kimi-code/`. La
/// production passe par `resolve_transcript_path`, qui a besoin de distinguer les deux échecs
/// (« index illisible » = permanent, « session absente » = à retenter) que cette composition écrase.
#[cfg(test)]
fn resolve_session_in(kimi_home: &str, session_id: &str) -> Option<String> {
    find_in_index(&read_index_in(kimi_home)?, session_id)
}

/// Lit `~/.kimi-code/session_index.jsonl` sous le home donné. `None` = index absent ou illisible,
/// c.-à-d. un échec **permanent** que l'appelant ne doit PAS retenter (cf. `resolve_transcript_path`).
///
/// Fichier minuscule (~2 Ko pour 11 sessions) : la lecture intégrale est sans enjeu.
///
/// Le BOM est retiré : `serde_json` rejette une première ligne préfixée d'un `\u{FEFF}`, ce qui
/// suffirait à tuer la résolution d'un index ne contenant qu'une seule session. Même précaution que
/// `vigil_hook_core::check_tool_enabled_at`.
fn read_index_in(kimi_home: &str) -> Option<String> {
    let home = kimi_home.trim_end_matches(['\\', '/']);
    let content = std::fs::read_to_string(format!("{home}/.kimi-code/session_index.jsonl")).ok()?;
    Some(content.strip_prefix('\u{FEFF}').unwrap_or(&content).to_string())
}

/// Cherche `session_id` dans le contenu de l'index et rend le `transcript_path` correspondant.
/// **Pure** → testable sans disque.
fn find_in_index(content: &str, session_id: &str) -> Option<String> {
    for line in content.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue; // ligne tronquée / non-JSON (écriture concurrente) → on l'ignore
        };
        if entry.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        // ⚠️ `continue` et NON `return None` : une entrée qui matche mais dont le `sessionDir` est
        // absent, nul, d'un autre type ou vide ne doit pas MASQUER une entrée valide plus ancienne
        // pour la même session (le doublon de reprise est un cas documenté). Abandonner ici ferait
        // disparaître l'agent de Vigil pour toute la session — même politique de tolérance que la
        // ligne non-JSON ci-dessus.
        let Some(dir) = entry.get("sessionDir").and_then(Value::as_str) else {
            continue;
        };
        if dir.is_empty() {
            continue;
        }
        let dir = dir.trim_end_matches(['\\', '/']);
        return Some(format!("{dir}/agents/main/wire.jsonl"));
    }
    None
}

/// Résout le répertoire home de l'utilisateur (aligné sur `grok_home` / `config/mod.rs::home_dir`).
fn kimi_home() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(format!("{drive}{path}"))
        })
        .filter(|s| !s.is_empty())
}

// ─────────────────────────────────────────────────────────────────────
// Mode de permission (AC3) — dernier `permission.set_mode` du wire.jsonl
// ─────────────────────────────────────────────────────────────────────

/// `read_only` de la session : lit le `wire.jsonl` résolu puis applique [`read_only_from_mode`].
/// Toute erreur de lecture (fichier absent, contenu illisible) retombe sur `true` sans panic —
/// jamais de faux bouton.
fn read_only_for_session(transcript_path: &str) -> bool {
    // ⚠️ Un échec de lecture (verrou Windows pendant que Kimi écrit, UTF-8 invalide dans une sortie
    // d'outil recopiée dans le wire, droits) donne le MÊME résultat qu'un fichier sans `set_mode` :
    // `read_only = true`. C'est le bon repli, mais il est INDISCERNABLE du cas nominal — une session
    // `yolo` perdrait sa capacité en silence (mord en 9.3). D'où la trace : le repli reste sûr, mais
    // il devient observable sous `VIGIL_DEBUG`.
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(content) => content,
        Err(err) => {
            debug_log(&format!(
                "vigil-kimi: {transcript_path} illisible ({err}) → repli read_only=true (mode indéterminé)"
            ));
            return true;
        }
    };
    read_only_from_mode(parse_mode_from_wire(&content).as_deref())
}

/// `read_only = (mode ≠ "yolo")` — donc `manual` **et** `auto` sont `read_only`, et l'absence de
/// mode aussi (décision Steven #3, défaut de sûreté).
///
/// En `auto`, savoir si *cet* appel sera auto-approuvé se décide **par appel**, information que le
/// payload ne porte pas ; bloquer à tort insérerait une demande qui n'aurait pas existé — **on ne
/// crée jamais de fausse barrière**.
fn read_only_from_mode(mode: Option<&str>) -> bool {
    // `!= Some("yolo")` couvre d'un coup les deux cas de sûreté : mode différent de `yolo`, ET mode
    // absent (`None` — cas courant, cf. `parse_mode_from_wire`).
    mode != Some("yolo")
}

/// Extrait le **dernier** `{"type":"permission.set_mode","mode":…}` du contenu d'un `wire.jsonl`.
/// Fonction **pure** (prend le contenu) → testable sans disque. `None` si aucun enregistrement.
///
/// ⚠️ **Cas COURANT, pas dégénéré : 2 sessions sur 11 du poste n'ont AUCUN `permission.set_mode`**
/// (l'enregistrement n'est écrit qu'au positionnement explicite du mode). `None` doit donc retomber
/// sur `read_only = true`, jamais être traité comme une erreur.
///
/// ⚠️ **Pourquoi lire tout le fichier et non une fenêtre de queue** : mesuré sur les 9 sessions du
/// poste qui en portent un, le `set_mode` est à la **ligne 5 sur 5 à 73** — c'est-à-dire en TÊTE,
/// écrit à l'ouverture de la session. Une lecture limitée aux derniers Ko le manquerait et
/// retomberait silencieusement sur `read_only = true`, tuant la capacité `yolo` (Story 9.3). Le
/// parcours inverse sert au « dernier gagne » (changement de mode en cours de session), pas à
/// économiser la lecture — l'économie vient de l'appel, restreint aux events qui en ont besoin.
///
/// Le `type` est lu **au niveau racine de la ligne** : les lignes `llm.tools_snapshot` contiennent
/// des `"type":"string"`/`"object"` imbriqués (schémas JSON d'outils) qu'une recherche textuelle
/// confondrait.
fn parse_mode_from_wire(content: &str) -> Option<String> {
    for line in content.lines().rev() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue; // ligne corrompue / partielle → ignorée
        };
        if record.get("type").and_then(Value::as_str) != Some("permission.set_mode") {
            continue;
        }
        // ⚠️ On rend la valeur du set_mode le PLUS RÉCENT, même inexploitable (`mode` absent ou d'un
        // autre type) → `None` → `read_only = true`. Continuer le parcours laisserait gagner un mode
        // PÉRIMÉ, et dans le sens NON SÛR : une session repassée en `manual` par un enregistrement
        // qu'on ne sait pas lire retomberait sur un `yolo` plus ancien, donc sur `read_only = false`
        // et des boutons d'approbation que rien ne justifie. Le dernier mot reste au dernier
        // enregistrement, lisible ou non.
        return record.get("mode").and_then(Value::as_str).map(str::to_owned);
    }
    None
}

// ─────────────────────────────────────────────────────────────────────
// Traduction (AC4, AC5)
// ─────────────────────────────────────────────────────────────────────

/// Couche de traduction **pure** : payload brut Kimi → JSON normalisé au format `HookEvent`, prêt à
/// écrire sur le pipe. `transcript_path` et `read_only` sont **injectés** (aucun I/O ici) → testable.
///
/// `None` si l'event est inconnu, le JSON invalide, ou le `transcript_path` vide (pas de clé fiable).
///
/// NB : `source_tool` n'est PAS injecté ici — il l'est par [`emit`] (via `inject_source_tool`),
/// uniformément sur tous les events, comme dans `vigil-grok`.
///
/// Un seul `match` fait à la fois la garde event connu/inconnu ET la construction du JSON cible —
/// pas deux énumérations à maintenir synchronisées (leçon `vigil-grok`).
fn translate(raw: &str, transcript_path: &str, read_only: bool) -> Option<String> {
    if transcript_path.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(raw).ok()?;
    let session_id = str_field(&v, "session_id");
    let cwd = str_field(&v, "cwd");

    // Base commune à TOUS les events (les 4 champs que `parse_event_common` attend côté app).
    // `hook_event_name` est repris **tel quel** : Kimi l'émet déjà en PascalCase = le tag de dispatch.
    let base = |name: &str| {
        json!({
            "hook_event_name": name,
            "session_id": session_id,
            "transcript_path": transcript_path,
            "cwd": cwd,
        })
    };

    match str_field(&v, "hook_event_name").as_str() {
        "SessionStart" => {
            let mut obj = base("SessionStart");
            // `source` ("startup" / "resume") : porté par Kimi, `Option` côté payload → mappé s'il
            // existe. Pas de `model` : le payload Kimi n'en porte aucun (badge modèle hors 9.1).
            if let Some(source) = v.get("source").and_then(Value::as_str) {
                obj["source"] = json!(source);
            }
            Some(obj.to_string())
        }
        "SessionEnd" => {
            let mut obj = base("SessionEnd");
            // `reason:"exit"` confirmé au spike : Kimi a un SessionEnd NATIF (comme Claude) → le
            // `pid_watcher` n'est qu'un filet de sécurité derrière lui.
            if let Some(reason) = v.get("reason").and_then(Value::as_str) {
                obj["reason"] = json!(reason);
            }
            Some(obj.to_string())
        }
        "PreToolUse" => {
            let mut obj = base("PreToolUse");
            if !attach_tool(&mut obj, &v) {
                return None;
            }
            // Ici = chemin fire-and-forget (`manual`/`auto`, `read_only` vrai) : `PreToolUse` est
            // traduit en event de MONITORING, tel quel. En `yolo` (`read_only` faux), le `PreToolUse`
            // n'atteint JAMAIS ce bras : le routage l'a dérouté vers `permission_gate_payload` +
            // `emit_and_await` (chemin bloquant) — pas de double transition (AC2).
            Some(obj.to_string())
        }
        "PermissionRequest" => {
            let mut obj = base("PermissionRequest");
            if !attach_tool(&mut obj, &v) {
                return None;
            }
            // Ici = chemin fire-and-forget, donc `read_only` est TOUJOURS `true` (manual/auto) : un
            // `PermissionRequest` natif en `yolo` (`read_only` faux) est dérouté en amont vers le
            // chemin bloquant (D1/AC3), il n'atteint jamais ce bras. `true` → `is_actionable` rend
            // faux (notif mobile non actionnable) et l'auto-permission est skippée (overlay d'awareness
            // visible) : chemins GÉNÉRIQUES déjà en place, aucun concept neuf.
            // `action` (« Writing C:/… ») et `display` (`kind`/`operation`/`path`) restent NON
            // consommés (AC1, piège 6.33) : le libellé riche est reconstruit par le chemin générique
            // (`tool_name`+`file_path`/`command`) — les forwarder serait une donnée morte.
            obj["read_only"] = json!(read_only);
            Some(obj.to_string())
        }
        "PostToolUse" => {
            let mut obj = base("PostToolUse");
            if !attach_tool(&mut obj, &v) {
                return None;
            }
            // ⚠️ `tool_response` est REQUIS par `PostToolUsePayload` (pas d'`Option`) : l'omettre
            // ferait échouer la désérialisation EN SILENCE côté pipe → aucun event. Kimi porte
            // `tool_output` (une chaîne) ; absent → `{}`.
            obj["tool_response"] = v.get("tool_output").cloned().unwrap_or_else(|| json!({}));
            Some(obj.to_string())
        }
        // ⚠️ Event JAMAIS observé au spike (le modèle anticipait systématiquement les erreurs) : sa
        // forme est PRÉSUMÉE d'après la doc Moonshot seule → parsing TOLÉRANT, chaque champ optionnel
        // omis plutôt que défaulté à une valeur bidon. `error` est `Option` côté Vigil (et non lu) :
        // on le dérive de `error` si présent, sinon de `tool_output` (jumeau du bras `PostToolUse`).
        // Pas d'`is_interrupt` : Kimi est hors `INTERRUPT_SUPPORTED` (cf. `flags::interrupt_supported`).
        "PostToolUseFailure" => {
            let mut obj = base("PostToolUseFailure");
            if !attach_tool(&mut obj, &v) {
                return None;
            }
            // ⚠️ `is_null()` filtré des DEUX côtés : `v.get("error")` rend `Some(Value::Null)` pour
            // un `"error": null` explicite — sans ce filtre il court-circuiterait le repli sur
            // `tool_output` PUIS produirait la chaîne littérale `"null"`, c.-à-d. exactement la
            // « valeur bidon » que le parsing tolérant s'interdit. Champ omis dans ce cas.
            if let Some(err) = v
                .get("error")
                .filter(|e| !e.is_null())
                .or_else(|| v.get("tool_output"))
                .filter(|e| !e.is_null())
            {
                // La valeur peut être une chaîne OU un JSON structuré : on préserve l'info dans les
                // deux cas plutôt que de la laisser tomber.
                let text = err.as_str().map(str::to_owned).unwrap_or_else(|| err.to_string());
                obj["error"] = json!(text);
            }
            Some(obj.to_string())
        }
        // ⚠️ Event JAMAIS observé, ET sémantique Moonshot DIFFÉRENTE de Claude (« background task
        // status changes »). Le handler socle est purement **annotatif** (met à jour
        // `last_activity_label`, ne change PAS l'état) et doit le rester : ne RIEN câbler de plus.
        "Notification" => {
            let mut obj = base("Notification");
            // `message` est REQUIS par `NotificationPayload` → toujours présent, défaut "".
            obj["message"] = json!(v.get("message").and_then(Value::as_str).unwrap_or_default());
            if let Some(nt) = v.get("notification_type").and_then(Value::as_str) {
                obj["notification_type"] = json!(nt);
            }
            Some(obj.to_string())
        }
        // `stop_hook_active` porté par Kimi = extra non requis par `StopPayload` → ignoré.
        "Stop" => Some(base("Stop").to_string()),
        // ⚠️ Event JAMAIS observé → forme présumée, parsing tolérant (cf. `PostToolUseFailure`).
        "StopFailure" => {
            let mut obj = base("StopFailure");
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                obj["error_type"] = json!(err);
            }
            Some(obj.to_string())
        }
        "UserPromptSubmit" => {
            let mut obj = base("UserPromptSubmit");
            // Divergence #2 : Kimi envoie un TABLEAU de blocs, Vigil attend une chaîne
            // (`Option<String>`). Absent / vide / malformé → champ OMIS (pas de valeur bidon).
            if let Some(prompt) = flatten_prompt(v.get("prompt")) {
                obj["prompt"] = json!(prompt);
            }
            Some(obj.to_string())
        }
        // ⚠️ Limite connue et ACCEPTÉE : `agent_name` est un **type** de sous-agent (« coder »), pas
        // un identifiant unique — deux sous-agents du même type en parallèle seraient indiscernables
        // (`SubagentStop` retomberait sur le premier). Le vrai id (`agent-0`) n'existe que dans le
        // `tool_output` TEXTUEL du `PostToolUse` de l'outil `Agent` : ne PAS bâtir d'identité dessus
        // (pas de parsing de texte libre). `agent_id`/`agent_type` sont requis (String) côté payload.
        //
        // ⚠️ `agent_name` vide ou absent → `None` plutôt qu'un sous-agent d'identité VIDE : le state
        // engine indexe les sous-agents par `agent_id`, et un `SubagentStop` d'id `""` se rabattrait
        // sur le premier `""` enregistré (`.find(|s| s.agent_id == …)`). On accumulerait des
        // sous-agents fantômes bloqués en `Working`. Ne rien émettre est le seul repli honnête.
        "SubagentStart" => subagent(base("SubagentStart"), &v),
        "SubagentStop" => subagent(base("SubagentStop"), &v),
        // Les 4 events supplémentaires de Kimi (`PermissionResult`, `Interrupt`, `PreCompact`,
        // `PostCompact`) et tout event inconnu → sortie propre, sans écriture, sans panic.
        _ => None,
    }
}

/// Renseigne `agent_id`/`agent_type` (tous deux dérivés d'`agent_name`, cf. limite assumée) sur un
/// event de sous-agent. `None` si `agent_name` est vide, absent ou non-chaîne.
fn subagent(mut obj: Value, v: &Value) -> Option<String> {
    let name = str_field(v, "agent_name");
    if name.is_empty() {
        return None;
    }
    obj["agent_id"] = json!(name);
    obj["agent_type"] = json!(name);
    Some(obj.to_string())
}

/// Ajoute `tool_name` / `tool_input` / `tool_call_id` à un event d'outil. Rend `false` si l'event
/// doit être abandonné (voir la garde sur `tool_name` ci-dessous).
///
/// Divergence #3 — **alias ADDITIF** `path` → `file_path` dans `tool_input` : `derive_activity_label`
/// (state engine) et `PermissionBlock.tsx` lisent `file_path`. L'alias est **générique**, sans liste
/// blanche de `tool_name` (le spike n'a capturé que `Write`, mais `Read`/`Edit` utilisent très
/// probablement la même clé et sont traités par les mêmes consommateurs). `path` est **conservé** :
/// rien en aval ne le lit, mais l'écraser ne rapporte rien et casserait un consommateur futur.
fn attach_tool(obj: &mut Value, v: &Value) -> bool {
    // ⚠️ `tool_name` vide, absent ou non-chaîne → l'event n'est PAS émis : un outil sans nom produit
    // un overlay de permission et un libellé d'activité anonymes, moins utiles que rien — et le nom
    // d'outil est la clé des règles d'auto-permission (`resolve_permission`), qu'une chaîne vide
    // ferait matcher de travers.
    let tool_name = str_field(v, "tool_name");
    if tool_name.is_empty() {
        return false;
    }
    obj["tool_name"] = json!(tool_name);
    let mut tool_input = v.get("tool_input").cloned().unwrap_or_else(|| json!({}));
    if let Some(map) = tool_input.as_object_mut() {
        if !map.contains_key("file_path") {
            if let Some(path) = map.get("path").and_then(Value::as_str).map(str::to_owned) {
                map.insert("file_path".to_string(), json!(path));
            }
        }
    }
    obj["tool_input"] = tool_input;
    // Extra Kimi non requis par les payloads cibles (ignoré par serde côté app), conservé pour la
    // lisibilité des captures et un usage futur.
    if let Some(id) = v.get("tool_call_id").and_then(Value::as_str) {
        obj["tool_call_id"] = json!(id);
    }
    true
}

/// Aplatit le `prompt` de Kimi (tableau de blocs) en chaîne : les `text` des blocs `{"type":"text"}`
/// joints par un **saut de ligne**. Tolère une chaîne nue (forme dégradée plausible). `None` si
/// absent, vide, ou si aucun bloc texte n'est exploitable → le champ est alors **omis** (`prompt` est
/// `Option<String>` côté Vigil, et n'est lu par personne aujourd'hui).
///
/// ⚠️ **Séparateur `"\n"`, décision de revue (2026-07-23)** : une concaténation nue colle deux mots
/// (`"Analyse le fichier"` + `"config.toml"` → `fichierconfig.toml`), c.-à-d. produit un texte FAUX
/// et non seulement mal présenté. Les blocs découpent des segments (paragraphes, texte entourant une
/// pièce jointe), jamais un mot en deux : le saut de ligne préserve cette structure pour l'affichage
/// d'historique de la 9.4, là où l'espace l'aplatirait. Sans effet sur les payloads observés au
/// spike, qui ne portent qu'un seul bloc.
fn flatten_prompt(prompt: Option<&Value>) -> Option<String> {
    let prompt = prompt?;
    if let Some(s) = prompt.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    let blocks = prompt.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Lit un champ chaîne de premier niveau. Vide si absent / d'un autre type.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Émission & décision (AC6, AC7)
// ─────────────────────────────────────────────────────────────────────

/// Injecte `parent_pid` + `source_tool` puis écrit le payload sur une connexion pipe dédiée.
/// Best-effort : toute erreur (pipe indisponible, écriture) est silencieuse — ne jamais bloquer la
/// boucle de l'agent, et **jamais rien sur stdout**.
fn emit(payload: &str) {
    let payload = inject_parent_pid(payload, PROCESS_NAME).unwrap_or_else(|| payload.to_string());
    let payload = inject_source_tool(&payload, SOURCE_TOOL);
    if let Some(mut pipe) = open_vigil_pipe() {
        let _ = write_payload(&mut pipe, &payload);
    }
}

/// Chemin BLOQUANT (AC2/AC3), miroir EXACT de `vigil-grok::handle_approve` : écrit le payload sur le
/// pipe ET **BLOQUE** en lisant la réponse de Vigil sur la MÊME connexion (schéma `read_pipe_response`,
/// PAS `emit()` qui ouvre/ferme une connexion dédiée), puis mappe la décision via
/// `convert_response_to_kimi` sur stdout + code de sortie. Ne retourne jamais (termine le process).
///
/// ⚠️ **stdout est un canal de contexte agent** : sur allow, `convert_response_to_kimi` rend `("", 0)`
/// → on n'imprime RIEN (ne rien dire EST l'approbation, `permissionDecision:"allow"` étant ignoré par
/// Kimi) ; sur deny, `(KIMI_DENY, 0)`. Aucun `println!` de debug (stderr via `debug_log` uniquement).
///
/// Fail-open (pipe indisponible / écriture / lecture échouées) → `exit 0` SANS stdout : Kimi reprend
/// la main (en `yolo`, l'appel passe). On ne bloque JAMAIS Kimi si Vigil est absent.
fn emit_and_await(payload: &str) -> ! {
    let payload = inject_parent_pid(payload, PROCESS_NAME).unwrap_or_else(|| payload.to_string());
    let payload = inject_source_tool(&payload, SOURCE_TOOL);

    let Some(mut pipe) = open_vigil_pipe() else {
        process::exit(0);
    };
    if !write_payload(&mut pipe, &payload) {
        process::exit(0);
    }
    if let Some(response) = read_pipe_response(&mut pipe) {
        let (out, code) = convert_response_to_kimi(&response);
        print!("{out}");
        let _ = std::io::stdout().flush();
        process::exit(code);
    }
    process::exit(0);
}

/// Synthétise le `PermissionRequest(read_only:false)` interne que le chemin bloquant écrit sur le pipe
/// (AC2/AC3). Le manager ne parque QUE sur un `PermissionRequest` (`should_await_approval`) : un
/// `PreToolUse` forwardé tel quel n'armerait AUCUN bouton — il faut donc **synthétiser**, jamais
/// forwarder (piège central, patron `vigil-grok::handle_approve`).
///
/// Réutilise `attach_tool` (donc l'alias `path`→`file_path`) : le libellé riche est reconstruit par le
/// chemin GÉNÉRIQUE côté app, sans forwarder les champs Kimi `action`/`display` — aucun consommateur ne
/// les lit (affordance morte, leçon 6.33). `None` si `tool_name` est vide (fail-open côté appelant).
fn permission_gate_payload(raw: &str, transcript_path: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let session_id = str_field(&v, "session_id");
    let cwd = str_field(&v, "cwd");
    let mut obj = json!({
        "hook_event_name": "PermissionRequest",
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "read_only": false,
    });
    if !attach_tool(&mut obj, &v) {
        return None;
    }
    Some(obj.to_string())
}

/// Renormalise la réponse Vigil (format interne `hookSpecificOutput.decision.behavior`, cf.
/// `ALLOW_RESPONSE`/`DENY_RESPONSE` de `pipe/protocol.rs`) vers la décision Kimi : `(stdout, code)`.
///
///   - **allow** → `("", 0)` : on n'émet **RIEN**. `permissionDecision:"allow"` est **ignoré par
///     Kimi** (mesuré deux fois au spike, avec et sans saut de ligne final : hooks restrictifs par
///     design, un hook compromis ne peut pas s'auto-autoriser). En `yolo`, ne rien dire EST
///     l'approbation ; en `manual`/`auto`, on n'atteint jamais ce chemin (`read_only`).
///   - **deny** (et tout parsing en échec, défaut sûr comme `convert_response_to_grok`) →
///     `(KIMI_DENY, 0)`.
///
/// ⚠️ **Livrée pure et testée en 9.1, APPELÉE depuis la 9.3** par `emit_and_await` (le blocage de
/// `PreToolUse` + la lecture de la réponse du pipe). Le `#[allow(dead_code)]` a été retiré ici.
fn convert_response_to_kimi(vigil_response: &str) -> (String, i32) {
    let parsed: Value = serde_json::from_str(vigil_response.trim()).unwrap_or(Value::Null);
    let behavior = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("decision"))
        .and_then(|d| d.get("behavior"))
        .and_then(Value::as_str)
        .unwrap_or("deny");
    match behavior {
        "allow" => (String::new(), 0),
        _ => (KIMI_DENY.to_string(), 0),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests — fixtures = payloads du spike 2026-07-22, recopiés tels quels.
// La crate binaire ne peut pas importer `HookEvent` (défini dans src-tauri) : on
// asserte donc sur les champs JSON produits, et la preuve de désérialisation vit côté app
// (`pipe/protocol.rs` : `kimi_*_normalise_deserialise`). Contrat en deux moitiés, comme Grok.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "session_259a872b-eeb2-4a17-a27e-d89dd17fb102";
    const SESSION_SANDBOX: &str = "session_050b4673-95db-43cb-ae9b-f71d3284b316";
    // Forme réelle produite par la résolution : slashs AVANT, `agents/main` en dur.
    const TP: &str = "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl";

    // ── Payloads Kimi verbatim (spike 2026-07-22) ─────────────────────
    const SESSION_START: &str = r#"{"hook_event_name":"SessionStart","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev","source":"startup"}"#;
    const SESSION_END: &str = r#"{"hook_event_name":"SessionEnd","session_id":"session_050b4673-95db-43cb-ae9b-f71d3284b316","cwd":"C:/Users/Dev/kimi-spike-sandbox","reason":"exit"}"#;
    const PRE_TOOL_USE: &str = r#"{"hook_event_name":"PreToolUse","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev","tool_name":"Write","tool_input":{"path":"C:/Users/Dev/test-kimi2.txt","content":""},"tool_call_id":"vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo"}"#;
    const PERMISSION_REQUEST: &str = r#"{"hook_event_name":"PermissionRequest","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev","turn_id":0,"tool_call_id":"vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo","tool_name":"Write","action":"Writing C:/Users/Dev/test-kimi2.txt","tool_input":{"path":"C:/Users/Dev/test-kimi2.txt","content":""},"display":{"kind":"file_io","operation":"write","path":"C:/Users/Dev/test-kimi2.txt","content":""}}"#;
    const POST_TOOL_USE: &str = r#"{"hook_event_name":"PostToolUse","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev","tool_name":"Write","tool_input":{"path":"C:/Users/Dev/test-kimi2.txt","content":""},"tool_call_id":"vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo","tool_output":"Wrote 0 bytes to C:/Users/Dev/test-kimi2.txt"}"#;
    const STOP: &str = r#"{"hook_event_name":"Stop","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev","stop_hook_active":false}"#;
    const USER_PROMPT_SUBMIT: &str = r#"{"hook_event_name":"UserPromptSubmit","session_id":"session_050b4673-95db-43cb-ae9b-f71d3284b316","cwd":"C:/Users/Dev/kimi-spike-sandbox","prompt":[{"type":"text","text":"Lance exactement sleep 30 en Bash. N'adapte pas la durée."}]}"#;
    const SUBAGENT_START: &str = r#"{"hook_event_name":"SubagentStart","session_id":"session_050b4673-95db-43cb-ae9b-f71d3284b316","cwd":"C:/Users/Dev/kimi-spike-sandbox","agent_name":"coder","prompt":"Ton seul travail est d'attendre 5 secondes."}"#;
    // `r###` obligatoire : le `response` réel commence par `## ` (Markdown) et la séquence `"##`
    // fermerait un raw string à un ou deux dièses.
    const SUBAGENT_STOP: &str = r###"{"hook_event_name":"SubagentStop","session_id":"session_050b4673-95db-43cb-ae9b-f71d3284b316","cwd":"C:/Users/Dev/kimi-spike-sandbox","agent_name":"coder","response":"## Final Handoff"}"###;
    // Bash en mode yolo (aucun PermissionRequest n'est émis dans ce mode).
    const PRE_TOOL_USE_BASH: &str = r#"{"hook_event_name":"PreToolUse","session_id":"session_050b4673-95db-43cb-ae9b-f71d3284b316","cwd":"C:/Users/Dev/kimi-spike-sandbox","tool_name":"Bash","tool_input":{"command":"sleep 30","timeout":35},"tool_call_id":"9woNpEgeNfc4RDSxwPammZzVtvNvek9M"}"#;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("translate doit produire du JSON valide")
    }

    fn tr(raw: &str) -> Value {
        parse(&translate(raw, TP, true).expect("event attendu traduit"))
    }

    // ── Fixtures disque (index / wire) ────────────────────────────────

    /// Racine temporaire unique par test, contenant un `.kimi-code/session_index.jsonl`.
    struct TempHome(std::path::PathBuf);

    impl TempHome {
        fn new(name: &str, index_content: Option<&str>) -> Self {
            let root = std::env::temp_dir().join(format!(
                "vigil_kimi_test_{}_{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(".kimi-code")).unwrap();
            if let Some(c) = index_content {
                std::fs::write(root.join(".kimi-code/session_index.jsonl"), c).unwrap();
            }
            TempHome(root)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().replace('\\', "/")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Deux lignes VERBATIM de `~/.kimi-code/session_index.jsonl` (poste Steven).
    const INDEX: &str = concat!(
        r#"{"sessionId":"session_c719ea9d-2d64-42c9-8f73-9079645abc9a","sessionDir":"C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_c719ea9d-2d64-42c9-8f73-9079645abc9a","workDir":"C:/Users/Dev"}"#,
        "\n",
        r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","sessionDir":"C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102","workDir":"C:/Users/Dev"}"#,
        "\n"
    );

    // ── AC1 : résolution d'identité ───────────────────────────────────

    #[test]
    fn resolution_nominale_produit_le_wire_de_main() {
        let home = TempHome::new("nominal", Some(INDEX));
        assert_eq!(resolve_session_in(&home.path(), SESSION).as_deref(), Some(TP));
    }

    #[test]
    fn resolution_preserve_les_slashs_avant() {
        // ⚠️ Aucune normalisation en backslash : la Story 9.4 devra retrouver ce chemin à l'identique,
        // et c'est aussi la clé de dédup du state engine.
        let home = TempHome::new("slashs", Some(INDEX));
        let path = resolve_session_in(&home.path(), SESSION).unwrap();
        assert!(!path.contains('\\'), "aucun backslash ne doit apparaître : {path}");
        assert!(path.ends_with("/agents/main/wire.jsonl"));
    }

    #[test]
    fn resolution_parcourt_a_lenvers_et_retient_la_derniere_entree() {
        // Une session peut figurer DEUX fois dans l'index (reprise) : la dernière ligne fait foi.
        let doublon = format!(
            "{}{}\n",
            INDEX,
            r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","sessionDir":"C:/Users/Dev/.kimi-code/sessions/wd_autre_ffffffffffff/session_259a872b-eeb2-4a17-a27e-d89dd17fb102","workDir":"C:/Users/Dev"}"#
        );
        let home = TempHome::new("inverse", Some(&doublon));
        let path = resolve_session_in(&home.path(), SESSION).unwrap();
        assert!(path.contains("wd_autre_ffffffffffff"), "la DERNIÈRE ligne doit gagner : {path}");
    }

    #[test]
    fn resolution_session_absente_retourne_none() {
        let home = TempHome::new("absente", Some(INDEX));
        assert!(resolve_session_in(&home.path(), "session_jamais_vue").is_none());
    }

    #[test]
    fn resolution_index_absent_retourne_none() {
        let home = TempHome::new("index_absent", None);
        assert!(resolve_session_in(&home.path(), SESSION).is_none());
    }

    #[test]
    fn resolution_ignore_les_lignes_non_json() {
        // Ligne tronquée (écriture concurrente) en fin de fichier : elle ne doit pas masquer
        // l'entrée valide située au-dessus.
        let corrompu = format!("{INDEX}{{\"sessionId\":\"session_259a8\n");
        let home = TempHome::new("corrompu", Some(&corrompu));
        assert_eq!(resolve_session_in(&home.path(), SESSION).as_deref(), Some(TP));
    }

    #[test]
    fn resolution_sous_agent_donne_le_meme_chemin_que_le_parent() {
        // AC1 — cas SOUS-AGENT. Un `PreToolUse` déclenché PAR un sous-agent porte le `session_id` du
        // PARENT et AUCUN identifiant d'agent (vérifié au spike, manche 7) : il n'y a donc rien à
        // distinguer, et il doit résoudre vers le MÊME transcript_path.
        // ⚠️ C'est pourquoi AUCUNE garde anti-fantôme façon Codex/Grok n'est ajoutée : le modèle de
        // Kimi ne produit pas ce bug — une garde serait du code mort au mieux, une suppression
        // d'events légitimes au pire.
        let home = TempHome::new("sous_agent", Some(INDEX));
        let parent = resolve_session_in(&home.path(), SESSION).unwrap();

        let payload_sous_agent = r#"{"hook_event_name":"PreToolUse","session_id":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","cwd":"C:/Users/Dev/kimi-spike-sandbox","tool_name":"Bash","tool_input":{"command":"sleep 5"},"tool_call_id":"souschild"}"#;
        let session_du_payload = str_field(&parse(payload_sous_agent), "session_id");
        let resolu = resolve_session_in(&home.path(), &session_du_payload).unwrap();

        assert_eq!(resolu, parent);
        let v = parse(&translate(payload_sous_agent, &resolu, true).unwrap());
        assert_eq!(v["transcript_path"], parent);
    }

    // ── AC2 : fail-open + retry borné ─────────────────────────────────

    #[test]
    fn retry_borne_reste_sous_la_borne_dure() {
        // Le retry ne doit JAMAIS pouvoir dépasser ~200 ms au total (AC2).
        let total = (RESOLVE_RETRY_ATTEMPTS as u64 - 1) * RESOLVE_RETRY_DELAY_MS;
        assert!(total <= 200, "budget de retry hors borne : {total} ms");
    }

    #[test]
    fn resolution_sans_session_id_echoue_immediatement_et_sans_retry() {
        // Un payload sans `session_id` n'a aucune clé exploitable → abandon immédiat.
        //
        // ⚠️ Le nom du test PROMET l'absence de retry : on la mesure, sinon la promesse serait
        // décorative. Si la garde `is_empty()` de `resolve_transcript_path` disparaissait, la
        // fonction irait lire le vrai `~/.kimi-code/session_index.jsonl` et dormirait 2 × 50 ms
        // avant de rendre `None` — le test resterait vert sans cette borne de durée.
        let debut = std::time::Instant::now();
        assert!(resolve_transcript_path("").is_none());
        let ecoule = debut.elapsed();
        assert!(
            ecoule < std::time::Duration::from_millis(RESOLVE_RETRY_DELAY_MS),
            "abandon immédiat attendu, aucun retry : {ecoule:?}"
        );
    }

    #[test]
    fn entree_dindex_sans_session_dir_ne_masque_pas_une_entree_valide() {
        // ⚠️ Verrou de revue (2026-07-23) : une entrée qui MATCHE mais dont le `sessionDir` est
        // inexploitable doit être ignorée comme une ligne non-JSON, PAS faire abandonner tout le
        // parcours inverse — sinon un doublon de reprise malformé ferait disparaître l'agent de
        // Vigil pour toute la session, en silence.
        for cassee in [
            r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102"}"#,
            r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","sessionDir":null}"#,
            r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","sessionDir":42}"#,
            r#"{"sessionId":"session_259a872b-eeb2-4a17-a27e-d89dd17fb102","sessionDir":""}"#,
        ] {
            let index = format!("{INDEX}{cassee}\n");
            assert_eq!(
                find_in_index(&index, SESSION).as_deref(),
                Some(TP),
                "l'entrée valide au-dessus doit rester trouvable : {cassee}"
            );
        }
    }

    #[test]
    fn index_avec_bom_reste_lisible() {
        // ⚠️ `serde_json` rejette une première ligne préfixée d'un BOM : sur un index ne contenant
        // qu'UNE session, l'oubli du strip tuerait la résolution entière. Précaution déjà prise par
        // `vigil_hook_core::check_tool_enabled_at`.
        let home = TempHome::new("bom", Some(&format!("\u{FEFF}{INDEX}")));
        assert_eq!(resolve_session_in(&home.path(), SESSION).as_deref(), Some(TP));
        // Et la PREMIÈRE ligne de l'index reste atteignable (c'est elle que le BOM préfixe).
        let premiere = "session_c719ea9d-2d64-42c9-8f73-9079645abc9a";
        assert!(resolve_session_in(&home.path(), premiere).is_some());
    }

    #[test]
    fn transcript_path_vide_nemet_rien() {
        // Fail-open : plutôt aucun event qu'un event à clé bancale.
        assert!(translate(PRE_TOOL_USE, "", true).is_none());
    }

    // ── AC3 : lecture du mode ─────────────────────────────────────────

    // Ligne VERBATIM du `wire.jsonl` de la session yolo du spike.
    const SET_MODE_YOLO: &str = r#"{"type":"permission.set_mode","mode":"yolo","time":1784742341553}"#;
    const SET_MODE_MANUAL: &str = r#"{"type":"permission.set_mode","mode":"manual","time":1784742000000}"#;

    #[test]
    fn mode_yolo_donne_read_only_faux() {
        let wire = format!("{{\"type\":\"metadata\"}}\n{SET_MODE_YOLO}\n{{\"type\":\"step.begin\"}}\n");
        assert_eq!(parse_mode_from_wire(&wire).as_deref(), Some("yolo"));
        assert!(!read_only_from_mode(Some("yolo")));
    }

    #[test]
    fn mode_manual_donne_read_only_vrai() {
        assert_eq!(parse_mode_from_wire(SET_MODE_MANUAL).as_deref(), Some("manual"));
        assert!(read_only_from_mode(Some("manual")));
    }

    #[test]
    fn mode_auto_est_traite_comme_manual() {
        // Décision Steven #3 : en `auto`, l'auto-approbation se décide PAR APPEL, information que le
        // payload ne porte pas → on ne crée jamais de fausse barrière.
        assert!(read_only_from_mode(Some("auto")));
    }

    #[test]
    fn aucun_set_mode_donne_read_only_vrai() {
        // ⚠️ Cas COURANT (2 sessions sur 11 du poste), pas dégénéré : jamais une erreur.
        let wire = "{\"type\":\"metadata\"}\n{\"type\":\"llm.request\"}\n";
        assert!(parse_mode_from_wire(wire).is_none());
        assert!(read_only_from_mode(None));
    }

    #[test]
    fn deux_set_mode_le_dernier_gagne() {
        // Un changement de mode EN COURS de session doit être vu.
        let wire = format!("{SET_MODE_MANUAL}\n{{\"type\":\"step.begin\"}}\n{SET_MODE_YOLO}\n");
        assert_eq!(parse_mode_from_wire(&wire).as_deref(), Some("yolo"));
    }

    #[test]
    fn dernier_set_mode_illisible_ne_laisse_pas_gagner_un_mode_perime() {
        // ⚠️ Verrou de revue (2026-07-23), sens NON SÛR. Si le `set_mode` le plus récent porte un
        // `mode` absent ou d'un autre type, continuer le parcours ferait ressortir le `yolo`
        // précédent → `read_only = false` → boutons d'approbation sur une session repassée en
        // `manual`, que rien ne peut résoudre. Le dernier enregistrement garde le dernier mot,
        // lisible ou non, et l'illisible retombe sur le défaut sûr.
        for illisible in [
            r#"{"type":"permission.set_mode","time":1784742999999}"#,
            r#"{"type":"permission.set_mode","mode":{"name":"manual"},"time":1784742999999}"#,
            r#"{"type":"permission.set_mode","mode":null}"#,
        ] {
            let wire = format!("{SET_MODE_YOLO}\n{illisible}\n");
            assert!(
                parse_mode_from_wire(&wire).is_none(),
                "un yolo PÉRIMÉ ne doit pas ressortir : {illisible}"
            );
            assert!(read_only_from_mode(parse_mode_from_wire(&wire).as_deref()));
        }
    }

    #[test]
    fn ligne_corrompue_ignoree_sans_panic() {
        let wire = format!("{SET_MODE_YOLO}\nceci n'est pas du json\n");
        assert_eq!(parse_mode_from_wire(&wire).as_deref(), Some("yolo"));
    }

    #[test]
    fn type_imbrique_ne_trompe_pas_la_lecture() {
        // ⚠️ Les lignes `llm.tools_snapshot` contiennent des `"type":"…"` IMBRIQUÉS (schémas JSON
        // d'outils, 48 `"type":"string"` sur une seule ligne du poste) : seul le `type` **racine**
        // compte. Une recherche textuelle (`line.contains("permission.set_mode")`) se ferait piéger.
        //
        // La ligne piège porte à la fois le motif imbriqué ET une clé `mode` racine de valeur
        // DIFFÉRENTE : c'est ce qui rend le test non-décoratif. Vérifié par sabotage (2026-07-23) —
        // avec un `contains` à la place du test de `type` racine, ce test PASSE au rouge en
        // renvoyant « manual » ; sans la clé `mode` racine, le sabotage restait vert.
        let piege = r#"{"type":"config.update","mode":"manual","tools":[{"parameters":{"type":"object","properties":{"m":{"type":"permission.set_mode"}}}}]}"#;
        let wire = format!("{SET_MODE_YOLO}\n{piege}\n");
        assert_eq!(parse_mode_from_wire(&wire).as_deref(), Some("yolo"));
    }

    #[test]
    fn wire_absent_retombe_sur_read_only() {
        // Toute erreur de lecture (fichier absent) → `true`, sans panic. Jamais de faux bouton.
        let absent = std::env::temp_dir().join("vigil_kimi_wire_qui_nexiste_pas.jsonl");
        let _ = std::fs::remove_file(&absent);
        assert!(read_only_for_session(&absent.to_string_lossy()));
    }

    #[test]
    fn set_mode_en_tete_de_fichier_est_bien_trouve() {
        // ⚠️ Verrou de non-régression : sur les 9 sessions du poste qui en portent un, le `set_mode`
        // est à la LIGNE 5 (fichiers de 5 à 73 lignes) — c.-à-d. en TÊTE. Une lecture limitée à une
        // fenêtre de queue le manquerait et tuerait silencieusement la capacité `yolo` (9.3).
        let mut wire = String::from("{\"type\":\"metadata\"}\n");
        wire.push_str(SET_MODE_YOLO);
        wire.push('\n');
        for _ in 0..500 {
            wire.push_str("{\"type\":\"context.append_loop_event\",\"payload\":\"");
            wire.push_str(&"x".repeat(500));
            wire.push_str("\"}\n");
        }
        assert_eq!(parse_mode_from_wire(&wire).as_deref(), Some("yolo"));
    }

    // ── AC4 : translate(), event par event ────────────────────────────

    #[test]
    fn session_start_traduit_avec_source() {
        let v = tr(SESSION_START);
        assert_eq!(v["hook_event_name"], "SessionStart"); // conservé tel quel, PAS re-mappé
        assert_eq!(v["session_id"], SESSION);
        assert_eq!(v["transcript_path"], TP);
        assert_eq!(v["cwd"], "C:/Users/Dev");
        assert_eq!(v["source"], "startup");
    }

    #[test]
    fn session_end_traduit_avec_reason() {
        let v = tr(SESSION_END);
        assert_eq!(v["hook_event_name"], "SessionEnd");
        assert_eq!(v["reason"], "exit"); // SessionEnd NATIF (spike manche 7)
    }

    #[test]
    fn pre_tool_use_traduit_avec_alias_file_path() {
        let v = tr(PRE_TOOL_USE);
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "Write");
        assert_eq!(v["tool_input"]["file_path"], "C:/Users/Dev/test-kimi2.txt");
        assert_eq!(v["tool_input"]["path"], "C:/Users/Dev/test-kimi2.txt", "alias ADDITIF : `path` conservé");
        assert_eq!(v["tool_call_id"], "vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo");
        assert!(v.get("read_only").is_none(), "read_only n'est porté que par PermissionRequest");
    }

    #[test]
    fn alias_file_path_est_generique_et_nonecrasant() {
        // Aucun filtre sur `tool_name` : le spike n'a capturé que `Write`, mais Read/Edit passent par
        // les mêmes consommateurs (`derive_activity_label`, `PermissionBlock.tsx`).
        let raw = r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/c","tool_name":"Read","tool_input":{"path":"/a.txt"}}"#;
        assert_eq!(tr(raw)["tool_input"]["file_path"], "/a.txt");
        // `file_path` déjà présent → on ne l'écrase pas.
        let deja = r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/c","tool_name":"Edit","tool_input":{"path":"/a.txt","file_path":"/deja.txt"}}"#;
        assert_eq!(tr(deja)["tool_input"]["file_path"], "/deja.txt");
        // Pas de clé `path` (ex. Bash) → rien n'est ajouté.
        let bash = tr(PRE_TOOL_USE_BASH);
        assert!(bash["tool_input"].get("file_path").is_none());
        assert_eq!(bash["tool_input"]["command"], "sleep 30");
    }

    #[test]
    fn permission_request_porte_read_only() {
        // manual/auto → true (awareness seule) ; yolo → false (actionnable).
        let ro = parse(&translate(PERMISSION_REQUEST, TP, true).unwrap());
        assert_eq!(ro["hook_event_name"], "PermissionRequest");
        assert_eq!(ro["read_only"], true);
        assert_eq!(ro["tool_name"], "Write");
        assert_eq!(ro["tool_input"]["file_path"], "C:/Users/Dev/test-kimi2.txt");

        let actionnable = parse(&translate(PERMISSION_REQUEST, TP, false).unwrap());
        assert_eq!(actionnable["read_only"], false);
    }

    #[test]
    fn permission_request_ne_consomme_ni_action_ni_display() {
        // AC5 : présents dans le payload, exploités seulement en 9.3.
        let v = tr(PERMISSION_REQUEST);
        assert!(v.get("action").is_none());
        assert!(v.get("display").is_none());
    }

    #[test]
    fn post_tool_use_mappe_tool_output_vers_tool_response() {
        // ⚠️ `tool_response` est REQUIS : l'omettre casserait la désérialisation EN SILENCE.
        let v = tr(POST_TOOL_USE);
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["tool_response"], "Wrote 0 bytes to C:/Users/Dev/test-kimi2.txt");
    }

    #[test]
    fn post_tool_use_sans_tool_output_defaut_objet_vide() {
        let raw = r#"{"hook_event_name":"PostToolUse","session_id":"s","cwd":"/c","tool_name":"Write","tool_input":{}}"#;
        assert_eq!(tr(raw)["tool_response"], json!({}));
    }

    #[test]
    fn stop_traduit() {
        let v = tr(STOP);
        assert_eq!(v["hook_event_name"], "Stop");
        assert_eq!(v["session_id"], SESSION);
        assert!(v.get("stop_hook_active").is_none(), "extra Kimi non requis → ignoré");
    }

    #[test]
    fn user_prompt_submit_aplatit_le_tableau_de_blocs() {
        let v = tr(USER_PROMPT_SUBMIT);
        assert_eq!(v["hook_event_name"], "UserPromptSubmit");
        assert_eq!(v["prompt"], "Lance exactement sleep 30 en Bash. N'adapte pas la durée.");
    }

    #[test]
    fn prompt_absent_vide_ou_malforme_omet_le_champ() {
        for raw in [
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/c"}"#,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/c","prompt":[]}"#,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/c","prompt":[{"type":"image","url":"x"}]}"#,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/c","prompt":42}"#,
        ] {
            assert!(tr(raw).get("prompt").is_none(), "prompt omis (jamais de valeur bidon) : {raw}");
        }
    }

    #[test]
    fn prompt_multi_blocs_est_joint_par_saut_de_ligne() {
        // ⚠️ Verrou de revue (2026-07-23) : AUCUNE espace de complaisance dans les fixtures. La
        // version précédente de ce test utilisait `{"text":"un "}` — l'espace finale de la donnée
        // faisait passer une concaténation nue pour un joint correct, et le collage de deux mots
        // (`fichierconfig.toml`) restait invisible. Ici les deux blocs sont des mots pleins : le
        // séparateur est la SEULE chose que le test observe.
        let raw = r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","cwd":"/c","prompt":[{"type":"text","text":"Analyse le fichier"},{"type":"image","url":"x"},{"type":"text","text":"config.toml"}]}"#;
        assert_eq!(tr(raw)["prompt"], "Analyse le fichier\nconfig.toml");
    }

    #[test]
    fn subagent_start_et_stop_mappent_agent_name() {
        // Limite assumée : `agent_name` est un TYPE, pas un id unique (cf. commentaire de translate).
        let start = tr(SUBAGENT_START);
        assert_eq!(start["hook_event_name"], "SubagentStart");
        assert_eq!(start["agent_id"], "coder");
        assert_eq!(start["agent_type"], "coder");
        assert_eq!(start["session_id"], SESSION_SANDBOX);
        assert_eq!(start["transcript_path"], TP, "sous-agent = même transcript que le parent");

        let stop = tr(SUBAGENT_STOP);
        assert_eq!(stop["hook_event_name"], "SubagentStop");
        assert_eq!(stop["agent_id"], "coder");
        assert_eq!(stop["agent_type"], "coder");
    }

    #[test]
    fn post_tool_use_failure_tolerant() {
        // ⚠️ JAMAIS observé : forme présumée → champs optionnels omis s'ils manquent.
        let avec = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","cwd":"/c","tool_name":"Bash","tool_input":{"command":"boom"},"error":"Access denied"}"#;
        let v = tr(avec);
        assert_eq!(v["hook_event_name"], "PostToolUseFailure");
        assert_eq!(v["error"], "Access denied");
        assert!(v.get("is_interrupt").is_none(), "Kimi hors INTERRUPT_SUPPORTED");

        let sans = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","cwd":"/c","tool_name":"Bash","tool_input":{}}"#;
        assert!(tr(sans).get("error").is_none(), "error omis plutôt que défaulté");
    }

    #[test]
    fn post_tool_use_failure_error_null_nemet_pas_la_chaine_null() {
        // ⚠️ Verrou de revue (2026-07-23) : `"error": null` rend `Some(Value::Null)`, pas `None`.
        // Sans filtre, il court-circuitait le repli `tool_output` PUIS produisait la chaîne
        // littérale « null » — exactement la « valeur bidon » que le parsing tolérant s'interdit.
        let nul = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","cwd":"/c","tool_name":"Bash","tool_input":{},"error":null}"#;
        assert!(tr(nul).get("error").is_none(), "champ omis, jamais la chaîne « null »");

        // …et le repli sur `tool_output` doit bien reprendre la main.
        let repli = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","cwd":"/c","tool_name":"Bash","tool_input":{},"error":null,"tool_output":"Access denied"}"#;
        assert_eq!(tr(repli)["error"], "Access denied");
    }

    #[test]
    fn stop_failure_tolerant() {
        let avec = r#"{"hook_event_name":"StopFailure","session_id":"s","cwd":"/c","error":"API error 429"}"#;
        assert_eq!(tr(avec)["error_type"], "API error 429");
        let sans = r#"{"hook_event_name":"StopFailure","session_id":"s","cwd":"/c"}"#;
        assert!(tr(sans).get("error_type").is_none());
    }

    #[test]
    fn notification_tolerante_et_purement_annotative() {
        // ⚠️ JAMAIS observée, et sémantique Moonshot ≠ Claude → rien de plus que le handler annotatif.
        let avec = r#"{"hook_event_name":"Notification","session_id":"s","cwd":"/c","message":"Background task done","notification_type":"task"}"#;
        let v = tr(avec);
        assert_eq!(v["message"], "Background task done");
        assert_eq!(v["notification_type"], "task");
        // `message` est REQUIS côté payload → toujours présent, défaut "".
        let sans = r#"{"hook_event_name":"Notification","session_id":"s","cwd":"/c"}"#;
        assert_eq!(tr(sans)["message"], "");
    }

    #[test]
    fn les_douze_events_sont_pris_en_charge() {
        // Verrou : les 12 events de CLAUDE_EVENTS doivent tous produire un payload.
        // Le payload porte `tool_name` ET `agent_name` : depuis la revue (2026-07-23), un event
        // d'outil sans nom d'outil et un event de sous-agent sans nom d'agent sont ABANDONNÉS
        // (identité vide), cf. les deux tests suivants.
        for event in [
            "SessionStart", "SessionEnd", "PreToolUse", "PostToolUse", "PostToolUseFailure",
            "PermissionRequest", "Notification", "Stop", "StopFailure", "UserPromptSubmit",
            "SubagentStart", "SubagentStop",
        ] {
            let raw = format!(
                r#"{{"hook_event_name":"{event}","session_id":"s","cwd":"/c","tool_name":"Write","tool_input":{{}},"agent_name":"coder"}}"#
            );
            let out = translate(&raw, TP, true).unwrap_or_else(|| panic!("{event} doit être traduit"));
            assert_eq!(parse(&out)["hook_event_name"], event);
        }
    }

    #[test]
    fn event_doutil_sans_tool_name_est_abandonne() {
        // ⚠️ Verrou de revue (2026-07-23) : `tool_name` vide/absent/non-chaîne → aucun event. Un
        // outil anonyme produirait un overlay de permission et un libellé d'activité sans nom, et
        // `resolve_permission` matcherait les règles d'auto-permission sur la chaîne vide.
        for event in ["PreToolUse", "PermissionRequest", "PostToolUse", "PostToolUseFailure"] {
            for extra in ["", r#","tool_name":"""#, r#","tool_name":42"#] {
                let raw = format!(
                    r#"{{"hook_event_name":"{event}","session_id":"s","cwd":"/c","tool_input":{{}}{extra}}}"#
                );
                assert!(
                    translate(&raw, TP, true).is_none(),
                    "{event} sans tool_name exploitable doit être abandonné ({extra})"
                );
            }
        }
    }

    #[test]
    fn sous_agent_sans_agent_name_est_abandonne() {
        // ⚠️ Verrou de revue (2026-07-23) : un `agent_id` VIDE ferait retomber tout `SubagentStop`
        // d'id `""` sur le premier sous-agent vide enregistré (`.find(|s| s.agent_id == …)` côté
        // state engine) → accumulation de sous-agents fantômes bloqués en `Working`.
        for event in ["SubagentStart", "SubagentStop"] {
            for extra in ["", r#","agent_name":"""#, r#","agent_name":null"#] {
                let raw = format!(
                    r#"{{"hook_event_name":"{event}","session_id":"s","cwd":"/c"{extra}}}"#
                );
                assert!(
                    translate(&raw, TP, true).is_none(),
                    "{event} sans agent_name exploitable doit être abandonné ({extra})"
                );
            }
        }
    }

    #[test]
    fn events_kimi_supplementaires_et_inconnus_retournent_none() {
        // Les 4 events propres à Kimi n'ont aucun consommateur en aval.
        for event in ["PermissionResult", "Interrupt", "PreCompact", "PostCompact", "EventDuFutur", ""] {
            let raw = format!(r#"{{"hook_event_name":"{event}","session_id":"s","cwd":"/c"}}"#);
            assert!(translate(&raw, TP, true).is_none(), "{event} doit retourner None");
        }
    }

    #[test]
    fn json_invalide_retourne_none() {
        assert!(translate("pas du json {{{", TP, true).is_none());
    }

    // ── AC6 : convert_response_to_kimi ────────────────────────────────

    #[test]
    fn allow_nemet_rien_sur_stdout() {
        // `permissionDecision:"allow"` est IGNORÉ par Kimi (mesuré 2×) : ne rien dire EST l'Approve.
        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        assert_eq!(convert_response_to_kimi(allow), (String::new(), 0));
    }

    #[test]
    fn deny_emet_le_json_et_sort_en_zero() {
        let deny = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#;
        let (out, code) = convert_response_to_kimi(deny);
        assert_eq!(code, 0, "chemin prouvé au spike : JSON + sortie 0 (jamais sortie 2)");
        let v = parse(&out);
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            v["hookSpecificOutput"]["permissionDecisionReason"].is_string(),
            "raison NEUTRE requise (le deny peut venir du timeout ou d'une règle)"
        );
    }

    #[test]
    fn reponse_illisible_retombe_sur_deny() {
        // Défaut sûr, comme `convert_response_to_grok`.
        for r in ["pas du json {{{", "", "{}", r#"{"hookSpecificOutput":{}}"#] {
            let (out, code) = convert_response_to_kimi(r);
            assert_eq!(out, KIMI_DENY, "parsing en échec → deny : {r}");
            assert_eq!(code, 0);
        }
    }

    // ── AC2/AC3 : routage bloquant vs fire-and-forget ─────────────────

    #[test]
    fn routage_des_quatre_combinaisons_event_x_mode() {
        // ⚠️ Verrou du cœur de la story : {PreToolUse, PermissionRequest} × {yolo(false), manual(true)}.
        // yolo (read_only=false) → bloquant ; manual (read_only=true) → fire-and-forget.
        //   - (PreToolUse, false)        = cas nominal yolo (AC2)
        //   - (PermissionRequest, false) = bascule de mode en session (D1/AC3)
        //   - (PreToolUse, true)         = monitoring manual/auto (AC1)
        //   - (PermissionRequest, true)  = awareness read-only manual/auto (AC1)
        assert_eq!(route("PreToolUse", false), Route::Blocking);
        assert_eq!(route("PermissionRequest", false), Route::Blocking);
        assert_eq!(route("PreToolUse", true), Route::FireAndForget);
        assert_eq!(route("PermissionRequest", true), Route::FireAndForget);
    }

    #[test]
    fn seuls_pretooluse_et_permissionrequest_peuvent_bloquer() {
        // Aucun autre event ne bloque, même avec read_only=false (inatteignable en pratique — le mode
        // n'est lu que pour ces deux events — mais la règle doit rester close sur l'event, pas juste
        // sur read_only).
        for event in [
            "PostToolUse", "PostToolUseFailure", "SessionStart", "SessionEnd", "Stop", "StopFailure",
            "Notification", "UserPromptSubmit", "SubagentStart", "SubagentStop", "EventInconnu", "",
        ] {
            assert_eq!(route(event, false), Route::FireAndForget, "{event} ne doit jamais bloquer");
            assert_eq!(route(event, true), Route::FireAndForget);
        }
    }

    // ── AC2/AC3 : synthèse du PermissionRequest interne ───────────────

    #[test]
    fn gate_synthetise_un_permission_request_actionnable() {
        // Depuis un `PreToolUse` yolo : PermissionRequest read_only:false + tool_name/tool_input +
        // alias file_path (patron Grok). Base session_id/transcript_path/cwd.
        let v = parse(&permission_gate_payload(PRE_TOOL_USE, TP).expect("gate doit synthétiser"));
        assert_eq!(v["hook_event_name"], "PermissionRequest", "PreToolUse → PermissionRequest interne");
        assert_eq!(v["read_only"], false, "actionnable : boutons + notif + auto-permission");
        assert_eq!(v["session_id"], SESSION);
        assert_eq!(v["transcript_path"], TP);
        assert_eq!(v["cwd"], "C:/Users/Dev");
        assert_eq!(v["tool_name"], "Write");
        assert_eq!(v["tool_input"]["file_path"], "C:/Users/Dev/test-kimi2.txt", "alias path→file_path");
        assert_eq!(v["tool_input"]["path"], "C:/Users/Dev/test-kimi2.txt", "path conservé (additif)");
    }

    #[test]
    fn gate_ne_forwarde_ni_action_ni_display() {
        // D1/AC3 : depuis un `PermissionRequest` natif (bascule de mode), la synthèse repart des mêmes
        // champs génériques — les `action`/`display` riches de Kimi ne sont PAS forwardés (6.33).
        let v = parse(&permission_gate_payload(PERMISSION_REQUEST, TP).expect("gate depuis PermissionRequest natif"));
        assert_eq!(v["hook_event_name"], "PermissionRequest");
        assert_eq!(v["read_only"], false);
        assert_eq!(v["tool_name"], "Write");
        assert_eq!(v["tool_input"]["file_path"], "C:/Users/Dev/test-kimi2.txt");
        assert!(v.get("action").is_none(), "affordance morte : `action` non forwardé");
        assert!(v.get("display").is_none(), "affordance morte : `display` non forwardé");
    }

    #[test]
    fn gate_sans_tool_name_echoue() {
        // Fail-open côté appelant : `None` → `exit 0` sans stdout, on ne bloque pas Kimi sur un outil
        // anonyme (pas de barrière fantôme).
        for raw in [
            r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/c","tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/c","tool_name":"","tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","session_id":"s","cwd":"/c","tool_name":42}"#,
            "pas du json {{{",
        ] {
            assert!(permission_gate_payload(raw, TP).is_none(), "tool_name inexploitable → None : {raw}");
        }
    }

    #[test]
    fn gate_bash_sans_path_porte_la_commande() {
        // Un outil sans clé `path` (Bash) reste synthétisable : aucun `file_path`, `command` porté
        // pour le libellé générique.
        let v = parse(&permission_gate_payload(PRE_TOOL_USE_BASH, TP).unwrap());
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["tool_input"]["command"], "sleep 30");
        assert!(v["tool_input"].get("file_path").is_none());
    }
}
