use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{DailyStats, LocalStats};
use super::pricing::{calculate_cost, calculate_cost_for_tool};

const EIGHT_DAYS_SECS: u64 = 8 * 24 * 3600;

/// Stats locales d'un outil (`"claude_code"` | `"codex"` | `"grok"`).
/// Convention `tool: &str` partagée avec `config::tool_label` / `install_hooks_for_tool`.
/// L'agrégation 7 jours (dates UTC, totaux, skip mtime>8j) est commune à tous les outils ;
/// seuls le répertoire de sessions, le scan et le parser diffèrent par format.
pub fn get_local_stats(tool: &str) -> LocalStats {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let dates = last_7_dates_utc(now_secs);
    let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();

    let mut agg: HashMap<String, DailyStats> = dates
        .iter()
        .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
        .collect();

    match tool {
        // Chemin Claude — STRICTEMENT INCHANGÉ (répertoire, scan 1 niveau, parser type:"assistant").
        "claude_code" => {
            let Some(dir) = claude_projects_dir() else { return LocalStats::default() };
            let mut files = Vec::new();
            scan_jsonl_files_local(&dir, &mut files);
            for path in files {
                if let Some(mtime) = mtime_secs(&path) {
                    if now_secs.saturating_sub(mtime) > EIGHT_DAYS_SECS { continue; }
                }
                parse_claude_session(&path, &date_set, &mut agg);
            }
        }
        // Codex CLI — arborescence YYYY/MM/DD → scan récursif, parser token_count/last_token_usage.
        "codex" => {
            let Some(dir) = codex_sessions_dir() else { return LocalStats::default() };
            let mut files = Vec::new();
            scan_jsonl_files_recursive(&dir, &mut files, 0, 4);
            for path in files {
                if let Some(mtime) = mtime_secs(&path) {
                    if now_secs.saturating_sub(mtime) > EIGHT_DAYS_SECS { continue; }
                }
                parse_codex_session(&path, &date_set, &mut agg);
            }
        }
        // Grok Build — arborescence `sessions/<urlencode(cwd)>/<sessionId>/updates.jsonl` →
        // scan récursif FILTRANT `updates.jsonl` (le dossier contient aussi `chat_history.jsonl`
        // + `summary.json` à ne PAS parser), parser `turn_completed.usage` per-turn.
        "grok" => {
            let Some(dir) = grok_sessions_dir() else { return LocalStats::default() };
            let mut files = Vec::new();
            scan_grok_updates_files(&dir, &mut files, 0, 5);
            for path in files {
                if let Some(mtime) = mtime_secs(&path) {
                    if now_secs.saturating_sub(mtime) > EIGHT_DAYS_SECS { continue; }
                }
                parse_grok_session(&path, &date_set, &mut agg);
            }
        }
        // Kimi CLI — arbre `sessions/wd_*/session_*/agents/*/wire.jsonl` → scan récursif FILTRANT
        // `wire.jsonl` (les sous-agents `agents/<id>/wire.jsonl` comptent aussi), parser
        // `usage.record` per-turn (Story 9.4). Coût via `pricing_for_kimi_model` (Moonshot ~$, local = 0).
        "kimi" => {
            let Some(dir) = kimi_sessions_dir() else { return LocalStats::default() };
            let mut files = Vec::new();
            scan_kimi_wire_files(&dir, &mut files, 0, 6);
            for path in files {
                if let Some(mtime) = mtime_secs(&path) {
                    if now_secs.saturating_sub(mtime) > EIGHT_DAYS_SECS { continue; }
                }
                parse_kimi_session(&path, &date_set, &mut agg);
            }
        }
        // Inconnu — pas de parser fiable : stats vides (placeholder géré côté frontend).
        _ => return LocalStats::default(),
    }

    finalize_stats(&dates, &mut agg)
}

/// Agrégation commune réutilisée par tous les parsers : produit le `LocalStats` final
/// (ordre des 7 dates préservé + totaux) à partir de l'agg rempli.
fn finalize_stats(dates: &[String], agg: &mut HashMap<String, DailyStats>) -> LocalStats {
    let days: Vec<DailyStats> = dates.iter()
        .filter_map(|d| agg.remove(d))
        .collect();

    let total_cost_usd = days.iter().map(|d| d.cost_usd).sum();
    let total_tokens = days.iter().map(|d| d.input_tokens + d.output_tokens + d.cache_tokens).sum();

    LocalStats { days, total_cost_usd, total_tokens }
}

fn parse_claude_session(
    path: &Path,
    date_set: &std::collections::HashSet<&str>,
    agg: &mut HashMap<String, DailyStats>,
) {
    let Ok(file) = File::open(path) else { return };
    let reader = BufReader::new(file);

    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") { continue; }

        let usage = match v.get("message").and_then(|m| m.get("usage")) {
            Some(u) => u,
            None => continue,
        };

        let timestamp = match v.get("timestamp").and_then(|t| t.as_str()) {
            Some(ts) => ts,
            None => continue,
        };
        if timestamp.len() < 10 { continue; }
        let date = &timestamp[..10];
        if !date_set.contains(date) { continue; }

        let model = v.get("message")
            .and_then(|m| m.get("model"))
            .and_then(|s| s.as_str())
            .unwrap_or("claude-sonnet");

        let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let cache_creation = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

        if let Some(entry) = agg.get_mut(date) {
            entry.input_tokens += input;
            entry.output_tokens += output;
            entry.cache_tokens += cache_creation + cache_read;
            entry.cost_usd += calculate_cost(input, output, cache_creation, cache_read, model);
        }
    }
}

/// Parser Codex CLI (schéma confirmé par spike 2026-06-01).
/// Ne retient que `type:"event_msg"` + `payload.type:"token_count"`, lit le DELTA par tour
/// (`payload.info.last_token_usage`, JAMAIS `total_token_usage` qui est cumulatif).
/// Le modèle courant est suivi via les lignes `type:"turn_context"` (`payload.model`).
fn parse_codex_session(
    path: &Path,
    date_set: &std::collections::HashSet<&str>,
    agg: &mut HashMap<String, DailyStats>,
) {
    let Ok(file) = File::open(path) else { return };
    let reader = BufReader::new(file);

    // Modèle courant : mis à jour à chaque `turn_context`, appliqué aux `token_count` suivants.
    let mut model = String::from("gpt-5");

    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        let line_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // Suivi du modèle pour le calcul de coût.
        if line_type == "turn_context" {
            if let Some(m) = v.get("payload").and_then(|p| p.get("model")).and_then(|s| s.as_str()) {
                if !m.is_empty() { model = m.to_string(); }
            }
            continue;
        }

        if line_type != "event_msg" { continue; }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") { continue; }

        // Delta du tour uniquement (total_token_usage est cumulatif → ignoré).
        let last = match payload.get("info").and_then(|i| i.get("last_token_usage")) {
            Some(l) => l,
            None => continue,
        };

        let timestamp = match v.get("timestamp").and_then(|t| t.as_str()) {
            Some(ts) => ts,
            None => continue,
        };
        if timestamp.len() < 10 { continue; }
        let date = &timestamp[..10];
        if !date_set.contains(date) { continue; }

        let input_total = last.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let cached = last.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = last.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

        // Codex : input_tokens INCLUT cached_input_tokens → on isole la portion non cachée
        // pour ne pas compter le cache deux fois. cache_creation inexistant chez Codex (= 0).
        let input_noncache = input_total.saturating_sub(cached);

        if let Some(entry) = agg.get_mut(date) {
            entry.input_tokens += input_noncache;
            entry.output_tokens += output;
            entry.cache_tokens += cached;
            entry.cost_usd += calculate_cost_for_tool("codex", input_noncache, output, 0, cached, &model);
        }
    }
}

/// Parser Grok Build (schéma `updates.jsonl` capturé disque réel, 2026-07-16).
/// Ne retient QUE les lignes `params.update.sessionUpdate == "turn_completed"` portant un objet
/// `usage`. Contrairement à Codex, `usage` est PER-TURN (on somme les tours, jamais un total
/// cumulatif). Pièges du format (cf. Dev Notes 6.32) :
/// - `timestamp` est un entier UNIX SECONDES au TOP-LEVEL (≠ ISO8601 Claude/Codex).
/// - `inputTokens` INCLUT `cachedReadTokens` → on isole la portion non cachée (comme Codex).
/// - `outputTokens` INCLUT DÉJÀ `reasoningTokens` → on l'utilise tel quel, sans ré-additionner.
/// - `cache_creation` inexistant chez Grok (= 0).
///
/// Le coût facture chaque modèle de `usage.modelUsage` avec sa grille Grok (multi-modèles/tour) ;
/// à défaut de `modelUsage`, on facture le top-level `usage` avec un modèle Grok par défaut.
fn parse_grok_session(
    path: &Path,
    date_set: &std::collections::HashSet<&str>,
    agg: &mut HashMap<String, DailyStats>,
) {
    let Ok(file) = File::open(path) else { return };
    let reader = BufReader::new(file);

    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        // Enveloppe JSON-RPC : le tour vit sous `params.update`.
        let update = match v.get("params").and_then(|p| p.get("update")) {
            Some(u) => u,
            None => continue,
        };
        if update.get("sessionUpdate").and_then(|s| s.as_str()) != Some("turn_completed") { continue; }
        let usage = match update.get("usage") {
            Some(u) => u,
            None => continue,
        };

        // `timestamp` = unix SECONDES au top-level (entier), converti en date UTC.
        let ts = match v.get("timestamp").and_then(|t| t.as_u64()) {
            Some(t) => t,
            None => continue,
        };
        let date = unix_secs_to_date_utc(ts);
        if !date_set.contains(date.as_str()) { continue; }

        let Some(entry) = agg.get_mut(&date) else { continue };

        // Coût per-modèle si `modelUsage` présent (map model_id → {tokens}), sinon top-level `usage`
        // avec un modèle par défaut. Les tokens agrégés (input non caché / output / cache) sont les
        // mêmes dans les deux cas (top-level = somme des modelUsage) → pas de double comptage.
        let per_model = usage.get("modelUsage").and_then(|m| m.as_object());
        match per_model.filter(|m| !m.is_empty()) {
            Some(models) => {
                for (model, mu) in models {
                    add_grok_usage(entry, mu, model);
                }
            }
            None => add_grok_usage(entry, usage, "grok"),
        }
    }
}

/// Ajoute un bloc d'usage Grok (`{inputTokens, outputTokens, cachedReadTokens}`) à l'agrégat du
/// jour, en facturant avec la grille `model`. `inputTokens` inclut le cache → on stocke la portion
/// non cachée à part ; `outputTokens` inclut déjà le reasoning → tel quel ; `cache_creation` = 0.
fn add_grok_usage(entry: &mut DailyStats, usage: &serde_json::Value, model: &str) {
    let input_total = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cached = usage.get("cachedReadTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let input_noncache = input_total.saturating_sub(cached);

    entry.input_tokens += input_noncache;
    entry.output_tokens += output;
    entry.cache_tokens += cached;
    entry.cost_usd += calculate_cost_for_tool("grok", input_noncache, output, 0, cached, model);
}

/// Parser Kimi CLI (schéma `wire.jsonl` capturé disque réel, spike T0 9.4).
/// Ne retient QUE les lignes `type == "usage.record"` ET `usageScope == "turn"` (ou absent) : un
/// scope cumulatif (`session`/`total`) serait un total → double comptage (piège Codex
/// `total_token_usage`). Pièges du format (vs Grok/Codex) :
/// - `time` est un UNIX **millisecondes** au TOP-LEVEL (≠ secondes Grok, ≠ ISO8601 Claude) ; lu en
///   entier avec repli float (un writer émettant `…817.0` ne doit pas faire perdre le record).
/// - `usage.inputOther` est DÉJÀ la portion non cachée (champ SÉPARÉ du cache) → pas de soustraction.
/// - le cache est scindé en `inputCacheRead` + `inputCacheCreation`.
/// - le coût suit la grille `pricing_for_kimi_model` (Moonshot facturé, modèle local = 0) — jamais
///   la grille Claude.
fn parse_kimi_session(
    path: &Path,
    date_set: &std::collections::HashSet<&str>,
    agg: &mut HashMap<String, DailyStats>,
) {
    let Ok(file) = File::open(path) else { return };
    let reader = BufReader::new(file);

    // `map_while(Result::ok)` (pas `flatten`) : évite le lint clippy `lines_filter_map_ok` et garde
    // la référence de 6 warnings sur la lib inchangée (AC9).
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        if v.get("type").and_then(|t| t.as_str()) != Some("usage.record") { continue; }
        // Garde double-comptage : ne sommer QUE les records per-turn. Un `usageScope` présent et
        // != "turn" (cumulatif) serait un total → double comptage. Absent = turn (schéma T0).
        if let Some(scope) = v.get("usageScope").and_then(|s| s.as_str()) {
            if scope != "turn" { continue; }
        }
        let usage = match v.get("usage") {
            Some(u) => u,
            None => continue,
        };

        // `time` = unix MILLISECONDES au top-level → secondes → date UTC. Entier, repli float.
        let ts_ms = match v.get("time").and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64))) {
            Some(t) => t,
            None => continue,
        };
        let date = unix_secs_to_date_utc(ts_ms / 1000);
        if !date_set.contains(date.as_str()) { continue; }

        let Some(entry) = agg.get_mut(&date) else { continue };

        let model = usage_model(&v);
        add_kimi_usage(entry, usage, &model);
    }
}

/// Modèle porté par une ligne `usage.record` (`model`, forme alias `lmstudio/qwen/…` ou `kimi-…`).
/// Sert uniquement à sélectionner la grille de coût (Moonshot facturé, local = 0). Défaut `"kimi"`
/// si absent → grille Moonshot (jamais Claude).
fn usage_model(v: &serde_json::Value) -> String {
    v.get("model")
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("kimi")
        .to_string()
}

/// Ajoute un bloc `usage` Kimi (`{inputOther, output, inputCacheRead, inputCacheCreation}`) à
/// l'agrégat du jour. `inputOther` = input non caché (déjà séparé, pas de soustraction) ; le cache =
/// lecture + création ; coût facturé via la grille Kimi (Moonshot ~$, modèle local = 0).
fn add_kimi_usage(entry: &mut DailyStats, usage: &serde_json::Value, model: &str) {
    let input = usage.get("inputOther").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = usage.get("inputCacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_creation = usage.get("inputCacheCreation").and_then(|v| v.as_u64()).unwrap_or(0);

    entry.input_tokens += input;
    entry.output_tokens += output;
    entry.cache_tokens += cache_read + cache_creation;
    entry.cost_usd += calculate_cost_for_tool("kimi", input, output, cache_creation, cache_read, model);
}

fn last_7_dates_utc(now_secs: u64) -> Vec<String> {
    (0..7).rev().map(|i| {
        let ts = now_secs.saturating_sub(i * 86_400);
        unix_secs_to_date_utc(ts)
    }).collect()
}

fn unix_secs_to_date_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let (y, m, d) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn days_to_ymd(mut days: u64) -> (u64, u8, u8) {
    let mut year = 1970u64;
    loop {
        let leap = is_leap(year);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year { break; }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days = [31u8, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0usize;
    for &md in &month_days {
        if days < md as u64 { break; }
        days -= md as u64;
        month += 1;
    }
    (year, (month + 1) as u8, (days + 1) as u8)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn claude_projects_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".claude").join("projects"))
}

fn codex_sessions_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".codex").join("sessions"))
}

fn grok_sessions_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".grok").join("sessions"))
}

fn kimi_sessions_dir() -> Option<std::path::PathBuf> {
    // Résolution home IDENTIQUE à `history::reader::kimi_index_path` / `vigil-kimi::kimi_home`
    // (USERPROFILE puis repli HOMEDRIVE+HOMEPATH, PAS `HOME`) : History et Stats DOIVENT viser la
    // même racine, sinon divergence si `USERPROFILE` est absent.
    let home = std::env::var("USERPROFILE")
        .ok()
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(format!("{drive}{path}"))
        })
        .filter(|s| !s.is_empty())?;
    Some(std::path::PathBuf::from(home).join(".kimi-code").join("sessions"))
}

/// Scan récursif borné COLLECTANT uniquement les fichiers nommés `wire.jsonl` (arborescence Kimi
/// `sessions/wd_*/session_*/agents/<id>/wire.jsonl` ; les sous-agents `agents/agent-0/…` comptent
/// dans le total, comme les sessions siblings de Grok). Le filtre par basename évite de parser
/// `state.json` / logs. Profondeur 6 = wd/session/agents/id/file suffit.
fn scan_kimi_wire_files(
    dir: &Path,
    results: &mut Vec<std::path::PathBuf>,
    depth: u32,
    max_depth: u32,
) {
    if depth > max_depth { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_kimi_wire_files(&path, results, depth + 1, max_depth);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("wire.jsonl") {
            results.push(path);
        }
    }
}

/// Scan récursif borné COLLECTANT uniquement les fichiers nommés `updates.jsonl` (arborescence
/// Grok `sessions/<encoded_cwd>/<sessionId>/updates.jsonl`, sous-agents = sessions siblings). Le
/// filtre par basename évite de parser `chat_history.jsonl` (conversation) pour rien ; `summary.json`
/// / `subagents/<sid>/meta.json` ne sont pas des `.jsonl` → ignorés naturellement.
fn scan_grok_updates_files(
    dir: &Path,
    results: &mut Vec<std::path::PathBuf>,
    depth: u32,
    max_depth: u32,
) {
    if depth > max_depth { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_grok_updates_files(&path, results, depth + 1, max_depth);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("updates.jsonl") {
            results.push(path);
        }
    }
}

/// Scan 1 niveau (chemin Claude) — INCHANGÉ.
fn scan_jsonl_files_local(dir: &Path, results: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&path) {
                for sub_entry in sub.flatten() {
                    let p = sub_entry.path();
                    if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        results.push(p);
                    }
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            results.push(path);
        }
    }
}

/// Scan récursif borné (arborescence Codex `YYYY/MM/DD/*.jsonl`).
/// `depth`/`max_depth` bornent la descente (profondeur 4 = year/month/day/file suffit).
/// Distinct de `scan_jsonl_files_local` (1 niveau) pour ne pas toucher au chemin Claude.
fn scan_jsonl_files_recursive(
    dir: &Path,
    results: &mut Vec<std::path::PathBuf>,
    depth: u32,
    max_depth: u32,
) {
    if depth > max_depth { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_jsonl_files_recursive(&path, results, depth + 1, max_depth);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            results.push(path);
        }
    }
}

fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines { writeln!(f, "{}", line).unwrap(); }
        path
    }

    #[test]
    fn extrait_usage_assistant_seulement() {
        let path = write_jsonl("stats_t1.jsonl", &[
            r#"{"type":"user","message":{},"timestamp":"2026-05-18T10:00:00Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2026-05-18T10:00:01Z"}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_claude_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 100);
        assert_eq!(agg["2026-05-18"].output_tokens, 50);
    }

    #[test]
    fn agrege_par_jour_correctement() {
        let path = write_jsonl("stats_t2.jsonl", &[
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2026-05-18T08:00:00Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":200,"output_tokens":80,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2026-05-18T14:00:00Z"}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_claude_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 300);
        assert_eq!(agg["2026-05-18"].output_tokens, 130);
    }

    #[test]
    fn ignore_entrees_hors_7j() {
        let path = write_jsonl("stats_t3.jsonl", &[
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":999,"output_tokens":999,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2020-01-01T00:00:00Z"}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_claude_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 0);
    }

    #[test]
    fn calcule_cout_sonnet() {
        let cost = super::super::pricing::calculate_cost(1_000_000, 1_000_000, 0, 0, "claude-sonnet");
        assert!((cost - 18.0).abs() < 0.001);
    }

    #[test]
    fn calcule_cout_opus_superieur() {
        let sonnet = super::super::pricing::calculate_cost(100_000, 50_000, 0, 0, "claude-sonnet");
        let opus = super::super::pricing::calculate_cost(100_000, 50_000, 0, 0, "claude-opus");
        assert!(opus > sonnet);
    }

    #[test]
    fn last_7_dates_inclut_aujourd_hui() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let dates = last_7_dates_utc(now);
        assert_eq!(dates.len(), 7);
        let today = unix_secs_to_date_utc(now);
        assert_eq!(dates[6], today);
    }

    #[test]
    fn fichier_vide_skipped() {
        let path = write_jsonl("stats_vide.jsonl", &[]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_claude_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 0);
        assert_eq!(agg["2026-05-18"].cost_usd, 0.0);
    }

    #[test]
    fn calcule_cout_cache_creation_et_read() {
        // Sonnet: cache_creation = $3.75/M, cache_read = $0.30/M
        let creation = super::super::pricing::calculate_cost(0, 0, 1_000_000, 0, "claude-sonnet");
        assert!((creation - 3.75).abs() < 0.001, "cache_creation Sonnet attendu $3.75, obtenu {}", creation);

        let read = super::super::pricing::calculate_cost(0, 0, 0, 1_000_000, "claude-sonnet");
        assert!((read - 0.30).abs() < 0.001, "cache_read Sonnet attendu $0.30, obtenu {}", read);

        // cache_creation coûte plus cher que cache_read
        assert!(creation > read);
    }

    #[test]
    fn calcule_cout_haiku() {
        // Haiku: 1M input + 1M output = $0.80 + $4.00 = $4.80
        let cost = super::super::pricing::calculate_cost(1_000_000, 1_000_000, 0, 0, "claude-haiku");
        assert!((cost - 4.80).abs() < 0.001, "Haiku attendu $4.80, obtenu {}", cost);

        // Haiku moins cher que Sonnet pour le même volume
        let sonnet = super::super::pricing::calculate_cost(1_000_000, 1_000_000, 0, 0, "claude-sonnet");
        assert!(cost < sonnet);
    }

    // ── T2 : scan récursif Codex ──────────────────────────────────────────────

    #[test]
    fn scan_recursif_trouve_fichier_profond() {
        // Reproduit l'arborescence Codex a/b/c/x.jsonl (≈ YYYY/MM/DD/file)
        let base = std::env::temp_dir().join(format!("vigil_codex_scan_{}", std::process::id()));
        let deep = base.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        let target = deep.join("x.jsonl");
        std::fs::File::create(&target).unwrap();
        // Un fichier non-jsonl ne doit pas être collecté
        std::fs::File::create(deep.join("ignore.txt")).unwrap();

        let mut results = Vec::new();
        scan_jsonl_files_recursive(&base, &mut results, 0, 4);

        let _ = std::fs::remove_dir_all(&base);
        assert!(results.iter().any(|p| p == &target), "fichier profond non trouvé : {:?}", results);
        assert_eq!(results.len(), 1, "seul le .jsonl doit être collecté");
    }

    // ── T3 : parser Codex ─────────────────────────────────────────────────────

    #[test]
    fn codex_agrege_les_deltas_pas_le_total() {
        // 2 tours : last_token_usage (deltas) doivent être sommés, total_token_usage IGNORÉ.
        let path = write_jsonl("stats_codex_delta.jsonl", &[
            r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
            r#"{"timestamp":"2026-05-18T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":50},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":50}}}}"#,
            r#"{"timestamp":"2026-05-18T11:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":0,"output_tokens":120},"last_token_usage":{"input_tokens":2000,"cached_input_tokens":0,"output_tokens":70}}}}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_codex_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        // Somme des deltas : 1000 + 2000 = 3000 (et NON 1000 + 3000 du cumulatif)
        assert_eq!(agg["2026-05-18"].input_tokens, 3000);
        assert_eq!(agg["2026-05-18"].output_tokens, 120);
    }

    #[test]
    fn codex_input_exclut_le_cache() {
        // input_tokens INCLUT cached_input_tokens → on stocke la portion non cachée + le cache à part.
        let path = write_jsonl("stats_codex_cache.jsonl", &[
            r#"{"timestamp":"2026-05-18T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10000,"cached_input_tokens":7000,"output_tokens":200}}}}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_codex_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 3000, "input non caché = 10000 - 7000");
        assert_eq!(agg["2026-05-18"].cache_tokens, 7000);
        assert_eq!(agg["2026-05-18"].output_tokens, 200);
        assert!(agg["2026-05-18"].cost_usd > 0.0, "coût Codex doit être > 0");
    }

    #[test]
    fn codex_ignore_entrees_hors_7j() {
        let path = write_jsonl("stats_codex_hors7j.jsonl", &[
            r#"{"timestamp":"2020-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":999,"cached_input_tokens":0,"output_tokens":999}}}}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_codex_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 0);
    }

    #[test]
    fn codex_ignore_lignes_non_token_count() {
        let path = write_jsonl("stats_codex_autres.jsonl", &[
            r#"{"type":"session_meta","payload":{}}"#,
            r#"{"timestamp":"2026-05-18T10:00:00Z","type":"response_item","payload":{"type":"message"}}"#,
            r#"{"timestamp":"2026-05-18T10:00:00Z","type":"event_msg","payload":{"type":"agent_message"}}"#,
        ]);
        let dates = ["2026-05-18".to_string()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_codex_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        assert_eq!(agg["2026-05-18"].input_tokens, 0);
        assert_eq!(agg["2026-05-18"].cost_usd, 0.0);
    }

    // ── T4 : pricing Codex ────────────────────────────────────────────────────

    #[test]
    fn codex_cache_moins_cher_que_input() {
        // À volume égal, le cache_read doit coûter moins cher que l'input non caché.
        let input_cost = super::super::pricing::calculate_cost_for_tool("codex", 1_000_000, 0, 0, 0, "gpt-5");
        let cache_cost = super::super::pricing::calculate_cost_for_tool("codex", 0, 0, 0, 1_000_000, "gpt-5");
        assert!(input_cost > 0.0);
        assert!(cache_cost > 0.0);
        assert!(cache_cost < input_cost, "cache ({}) doit être < input ({})", cache_cost, input_cost);
    }

    #[test]
    fn codex_mini_moins_cher_que_gpt5() {
        let mini = super::super::pricing::calculate_cost_for_tool("codex", 1_000_000, 1_000_000, 0, 0, "gpt-5.4-mini");
        let full = super::super::pricing::calculate_cost_for_tool("codex", 1_000_000, 1_000_000, 0, 0, "gpt-5");
        assert!(mini < full, "mini ({}) doit être < gpt-5 ({})", mini, full);
    }

    // ── T1/T8 : parser Grok ───────────────────────────────────────────────────

    fn grok_agg_for(ts: u64, lines: &[&str], name: &str) -> (std::collections::HashMap<String, DailyStats>, String) {
        let path = write_jsonl(name, lines);
        let date = unix_secs_to_date_utc(ts);
        let dates = [date.clone()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_grok_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        (agg, date)
    }

    #[test]
    fn grok_somme_par_tour_et_exclut_le_cache() {
        // 2 tours `turn_completed` : sommés (per-turn, PAS cumulatif). `inputTokens` inclut
        // `cachedReadTokens` → portion non cachée isolée. `outputTokens` inclut déjà le reasoning.
        let ts: u64 = 1_783_943_155;
        let lines = [
            r#"{"timestamp":1783943155,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":1000,"outputTokens":100,"cachedReadTokens":600,"reasoningTokens":40,"modelUsage":{"grok-4.5":{"inputTokens":1000,"outputTokens":100,"cachedReadTokens":600}}}}}}"#,
            r#"{"timestamp":1783943200,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":2000,"outputTokens":200,"cachedReadTokens":0,"modelUsage":{"grok-4.5":{"inputTokens":2000,"outputTokens":200,"cachedReadTokens":0}}}}}}"#,
        ];
        let (agg, date) = grok_agg_for(ts, &lines, "stats_grok_sum.jsonl");
        assert_eq!(agg[&date].input_tokens, 2400, "1000-600 + 2000");
        assert_eq!(agg[&date].output_tokens, 300);
        assert_eq!(agg[&date].cache_tokens, 600);
        assert!(agg[&date].cost_usd > 0.0);
    }

    #[test]
    fn grok_ignore_non_turn_completed_et_subagent_finished() {
        // Seules les lignes `turn_completed` avec `usage` comptent. `subagent_finished.tokens_used`
        // et les lignes de flux d'affichage (`content`) sont ignorées → zéro double comptage.
        let ts: u64 = 1_783_943_155;
        let lines = [
            r#"{"timestamp":1783943155,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"content","text":"hello"}}}"#,
            r#"{"timestamp":1783943160,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"subagent_finished","tokens_used":99999}}}"#,
            r#"{"timestamp":1783943200,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":500,"outputTokens":50,"cachedReadTokens":0}}}}"#,
        ];
        let (agg, date) = grok_agg_for(ts, &lines, "stats_grok_ignore.jsonl");
        assert_eq!(agg[&date].input_tokens, 500);
        assert_eq!(agg[&date].output_tokens, 50);
    }

    #[test]
    fn grok_ignore_tour_hors_7j() {
        // Un `turn_completed` daté hors de la fenêtre (date ∉ date_set) est ignoré.
        let ts: u64 = 1_783_943_155;
        let old_line = r#"{"timestamp":1000000000,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":9999,"outputTokens":9999,"cachedReadTokens":0}}}}"#;
        let (agg, date) = grok_agg_for(ts, &[old_line], "stats_grok_hors7j.jsonl");
        assert_eq!(agg[&date].input_tokens, 0);
        assert_eq!(agg[&date].output_tokens, 0);
    }

    #[test]
    fn grok_repli_sans_model_usage() {
        // `modelUsage` absent → on facture le top-level `usage` avec le modèle par défaut.
        let ts: u64 = 1_783_943_155;
        let line = r#"{"timestamp":1783943155,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":1000,"outputTokens":100,"cachedReadTokens":200}}}}"#;
        let (agg, date) = grok_agg_for(ts, &[line], "stats_grok_nomu.jsonl");
        assert_eq!(agg[&date].input_tokens, 800);
        assert_eq!(agg[&date].cache_tokens, 200);
        assert_eq!(agg[&date].output_tokens, 100);
        assert!(agg[&date].cost_usd > 0.0);
    }

    #[test]
    fn scan_grok_ne_ramasse_que_updates_jsonl() {
        // Le dossier de session contient updates.jsonl + chat_history.jsonl + summary.json →
        // seul updates.jsonl est collecté (pas chat_history.jsonl, pas summary.json).
        let base = std::env::temp_dir().join(format!("vigil_grok_scan_{}", std::process::id()));
        let session = base.join("C%3A%5Cproj").join("019f-session");
        std::fs::create_dir_all(&session).unwrap();
        let target = session.join("updates.jsonl");
        std::fs::File::create(&target).unwrap();
        std::fs::File::create(session.join("chat_history.jsonl")).unwrap();
        std::fs::File::create(session.join("summary.json")).unwrap();

        let mut results = Vec::new();
        scan_grok_updates_files(&base, &mut results, 0, 5);

        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(results.len(), 1, "seul updates.jsonl collecté : {:?}", results);
        assert_eq!(results[0], target);
    }

    #[test]
    fn calculate_cost_for_tool_delegue_a_anthropic_pour_claude() {
        // Pour un outil non-codex, le coût doit être identique au calcul Anthropic existant.
        let via_tool = super::super::pricing::calculate_cost_for_tool("claude_code", 1_000_000, 1_000_000, 0, 0, "claude-sonnet");
        let direct = super::super::pricing::calculate_cost(1_000_000, 1_000_000, 0, 0, "claude-sonnet");
        assert!((via_tool - direct).abs() < 1e-9);
    }

    // ── Kimi (9.4) : parser usage.record + pricing tokens-only ────────────────────

    fn kimi_agg_for(ts_ms: u64, lines: &[&str], name: &str) -> (std::collections::HashMap<String, DailyStats>, String) {
        let path = write_jsonl(name, lines);
        let date = unix_secs_to_date_utc(ts_ms / 1000);
        let dates = [date.clone()];
        let date_set: std::collections::HashSet<&str> = dates.iter().map(|s| s.as_str()).collect();
        let mut agg: std::collections::HashMap<String, DailyStats> = dates.iter()
            .map(|d| (d.clone(), DailyStats { date: d.clone(), ..Default::default() }))
            .collect();
        parse_kimi_session(&path, &date_set, &mut agg);
        let _ = std::fs::remove_file(&path);
        (agg, date)
    }

    #[test]
    fn kimi_somme_les_usage_record_par_turn() {
        // 2 usage.record : sommés per-turn. `inputOther` = input non caché (PAS de soustraction) ;
        // cache = inputCacheRead + inputCacheCreation ; `time` = ms Unix top-level.
        let ts: u64 = 1_784_738_573_817;
        let lines = [
            r#"{"type":"usage.record","model":"lmstudio/qwen","usage":{"inputOther":1000,"output":100,"inputCacheRead":600,"inputCacheCreation":200},"usageScope":"turn","time":1784738573817}"#,
            r#"{"type":"config.update","time":1784738573818}"#,
            r#"{"type":"usage.record","model":"lmstudio/qwen","usage":{"inputOther":2000,"output":200,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1784738573900}"#,
        ];
        let (agg, date) = kimi_agg_for(ts, &lines, "stats_kimi_sum.jsonl");
        assert_eq!(agg[&date].input_tokens, 3000, "1000 + 2000 (inputOther, pas de soustraction)");
        assert_eq!(agg[&date].output_tokens, 300);
        assert_eq!(agg[&date].cache_tokens, 800, "600+200 + 0");
        // Modèle LOCAL (`lmstudio/qwen`) → aucun coût d'API (garde-fou d'honnêteté §5).
        assert_eq!(agg[&date].cost_usd, 0.0, "modèle auto-hébergé → coût 0");
    }

    #[test]
    fn kimi_modele_moonshot_facture_au_tarif_moonshot() {
        // Cas GÉNÉRAL : un modèle Moonshot (kimi-k2…) est facturé à la grille Moonshot (> 0),
        // jamais 0 ni le tarif Claude.
        let ts: u64 = 1_784_738_573_817;
        let line = r#"{"type":"usage.record","model":"kimi-k2.6","usage":{"inputOther":1000000,"output":1000000,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1784738573817}"#;
        let (agg, date) = kimi_agg_for(ts, &[line], "stats_kimi_moonshot.jsonl");
        assert_eq!(agg[&date].input_tokens, 1_000_000);
        assert!(agg[&date].cost_usd > 0.0, "modèle Moonshot facturé (cas général)");
    }

    #[test]
    fn kimi_ignore_lignes_non_usage_record() {
        let ts: u64 = 1_784_738_573_817;
        let lines = [
            r#"{"type":"context.append_message","message":{"role":"user"},"time":1784738573817}"#,
            r#"{"type":"llm.request","model":"kimi-k2","time":1784738573817}"#,
        ];
        let (agg, date) = kimi_agg_for(ts, &lines, "stats_kimi_noise.jsonl");
        assert_eq!(agg[&date].input_tokens, 0);
        assert_eq!(agg[&date].cost_usd, 0.0);
    }

    #[test]
    fn kimi_ignore_record_hors_7j() {
        // `time` daté hors fenêtre (date ∉ date_set) → ignoré.
        let ts: u64 = 1_784_738_573_817;
        let old = r#"{"type":"usage.record","model":"x","usage":{"inputOther":9999,"output":9999},"usageScope":"turn","time":1000000000000}"#;
        let (agg, date) = kimi_agg_for(ts, &[old], "stats_kimi_hors7j.jsonl");
        assert_eq!(agg[&date].input_tokens, 0);
    }

    #[test]
    fn kimi_ignore_usage_record_scope_cumulatif() {
        // Garde double-comptage : un `usageScope` != "turn" (cumulatif) est ignoré ; seul le
        // per-turn est sommé (sinon on doublerait le total, piège Codex `total_token_usage`).
        let ts: u64 = 1_784_738_573_817;
        let lines = [
            r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":1000,"output":100},"usageScope":"turn","time":1784738573817}"#,
            r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":9999,"output":9999},"usageScope":"session","time":1784738573900}"#,
        ];
        let (agg, date) = kimi_agg_for(ts, &lines, "stats_kimi_scope.jsonl");
        assert_eq!(agg[&date].input_tokens, 1000, "seul le record per-turn est compté");
        assert_eq!(agg[&date].output_tokens, 100);
    }

    #[test]
    fn kimi_time_flottant_ne_perd_pas_le_record() {
        // Un `time` sérialisé en float (…817.0) ne doit PAS faire `continue` (tokens perdus) : repli
        // `as_f64`.
        let ts: u64 = 1_784_738_573_817;
        let line = r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":1234,"output":56},"usageScope":"turn","time":1784738573817.0}"#;
        let (agg, date) = kimi_agg_for(ts, &[line], "stats_kimi_floattime.jsonl");
        assert_eq!(agg[&date].input_tokens, 1234, "time flottant toléré, record compté");
    }

    #[test]
    fn kimi_cost_for_tool_grille_moonshot_jamais_la_grille_claude() {
        // §5 : un modèle Moonshot est facturé à la grille Moonshot (> 0), et à un montant DIFFÉRENT
        // de la grille Anthropic → preuve que Kimi ne tombe PAS sur le `_ => pricing_for_claude_model`.
        let kimi = super::super::pricing::calculate_cost_for_tool("kimi", 1_000_000, 1_000_000, 0, 0, "kimi-k2.6");
        let claude = super::super::pricing::calculate_cost_for_tool("claude_code", 1_000_000, 1_000_000, 0, 0, "kimi-k2.6");
        assert!(kimi > 0.0, "modèle Moonshot facturé");
        assert!((kimi - claude).abs() > 1e-6, "grille Moonshot ≠ grille Claude ({kimi} vs {claude})");
        // Modèle auto-hébergé → 0 (garde-fou d'honnêteté).
        let local = super::super::pricing::calculate_cost_for_tool("kimi", 1_000_000, 1_000_000, 0, 0, "lmstudio/qwen/qwen3-30b");
        assert_eq!(local, 0.0, "modèle local → aucun coût d'API");
    }

    #[test]
    fn kimi_providers_locaux_elargis_et_insensibles_a_la_casse() {
        // Garde-fou §5 élargi : plusieurs providers auto-hébergés reconnus, détection insensible à la
        // casse ; un modèle Moonshot reste facturé.
        let cost = |m: &str| super::super::pricing::calculate_cost_for_tool("kimi", 1_000_000, 1_000_000, 0, 0, m);
        for m in ["LMStudio/qwen", "ollama/llama3", "vllm/mixtral", "llama.cpp/phi", "localhost:1234/x"] {
            assert_eq!(cost(m), 0.0, "provider local `{m}` → coût 0");
        }
        assert!(cost("kimi-k2.6") > 0.0, "modèle Moonshot toujours facturé");
    }

    #[test]
    fn scan_kimi_ne_ramasse_que_wire_jsonl() {
        // sessions/wd_x/session_y/agents/main/wire.jsonl + agents/agent-0/wire.jsonl collectés ;
        // state.json ignoré.
        let base = std::env::temp_dir().join(format!("vigil_kimi_scan_{}", std::process::id()));
        let main = base.join("wd_x").join("session_y").join("agents").join("main");
        let sub = base.join("wd_x").join("session_y").join("agents").join("agent-0");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::File::create(main.join("wire.jsonl")).unwrap();
        std::fs::File::create(sub.join("wire.jsonl")).unwrap();
        std::fs::write(base.join("wd_x").join("session_y").join("state.json"), "{}").unwrap();

        let mut results = Vec::new();
        scan_kimi_wire_files(&base, &mut results, 0, 6);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(results.len(), 2, "les 2 wire.jsonl (main + sous-agent), pas state.json : {:?}", results);
        assert!(results.iter().all(|p| p.file_name().and_then(|n| n.to_str()) == Some("wire.jsonl")));
    }
}
