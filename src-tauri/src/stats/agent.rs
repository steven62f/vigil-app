use std::io::BufRead;
use super::AgentStats;

/// Stats d'un agent Claude, parsées depuis son transcript (`type:"assistant".message.usage`).
/// Nommage explicite par outil (symétrique de `get_agent_stats_grok`, cf. `parse_claude_session` /
/// `pricing_for_claude_model`) — dé-généralisation de Claude. Codex passe encore par ce chemin
/// (zéros pré-existants, hors scope 6.32).
pub fn get_agent_stats_claude(transcript_path: &str) -> Result<AgentStats, String> {
    let file = std::fs::File::open(transcript_path).map_err(|e| e.to_string())?;
    let reader = std::io::BufReader::new(file);

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_creation: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut model = "claude-sonnet".to_string();
    let mut first_ts_secs: Option<u64> = None;

    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        if first_ts_secs.is_none() {
            if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                first_ts_secs = parse_iso8601_secs(ts);
            }
        }

        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") { continue; }

        let msg = match v.get("message") { Some(m) => m, None => continue };
        let usage = match msg.get("usage") { Some(u) => u, None => continue };

        if let Some(m) = msg.get("model").and_then(|s| s.as_str()) {
            if model == "claude-sonnet" { model = m.to_string(); }
        }

        input_tokens += usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        output_tokens += usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        cache_creation += usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        cache_read += usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    }

    let cost_usd = crate::stats::pricing::calculate_cost(
        input_tokens, output_tokens, cache_creation, cache_read, &model,
    );

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let duration_days = first_ts_secs.and_then(|ts| {
        let elapsed = now_secs.saturating_sub(ts);
        if elapsed >= 86400 { Some((elapsed / 86400) as u32) } else { None }
    });

    Ok(AgentStats {
        input_tokens,
        output_tokens,
        cache_tokens: cache_creation + cache_read,
        cost_usd,
        duration_days,
    })
}

/// Stats d'un agent Grok, parsées depuis son `updates.jsonl` (le `transcript_path` porté par
/// `AgentSnapshot`). Mêmes règles per-turn que le reader Stats (6.32) : lignes
/// `params.update.sessionUpdate == "turn_completed"` uniquement, `inputTokens` inclut le cache
/// (soustrait), `outputTokens` inclut déjà le reasoning (tel quel), `timestamp` unix SECONDES au
/// top-level. Le coût facture chaque modèle de `modelUsage` avec sa grille Grok. Scope Grok
/// uniquement — le chemin Claude (`get_agent_stats_claude`) reste byte-identique.
pub fn get_agent_stats_grok(transcript_path: &str) -> Result<AgentStats, String> {
    let file = std::fs::File::open(transcript_path).map_err(|e| e.to_string())?;
    let reader = std::io::BufReader::new(file);

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_tokens: u64 = 0;
    let mut cost_usd: f64 = 0.0;
    let mut first_ts_secs: Option<u64> = None;

    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        if first_ts_secs.is_none() {
            first_ts_secs = v.get("timestamp").and_then(|t| t.as_u64());
        }

        let update = match v.get("params").and_then(|p| p.get("update")) {
            Some(u) => u,
            None => continue,
        };
        if update.get("sessionUpdate").and_then(|s| s.as_str()) != Some("turn_completed") { continue; }
        let usage = match update.get("usage") {
            Some(u) => u,
            None => continue,
        };

        let per_model = usage.get("modelUsage").and_then(|m| m.as_object());
        match per_model.filter(|m| !m.is_empty()) {
            Some(models) => {
                for (model, mu) in models {
                    add_grok_agent_usage(mu, model, &mut input_tokens, &mut output_tokens, &mut cache_tokens, &mut cost_usd);
                }
            }
            None => add_grok_agent_usage(usage, "grok", &mut input_tokens, &mut output_tokens, &mut cache_tokens, &mut cost_usd),
        }
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let duration_days = first_ts_secs.and_then(|ts| {
        let elapsed = now_secs.saturating_sub(ts);
        if elapsed >= 86400 { Some((elapsed / 86400) as u32) } else { None }
    });

    Ok(AgentStats {
        input_tokens,
        output_tokens,
        cache_tokens,
        cost_usd,
        duration_days,
    })
}

/// Stats d'un agent Kimi, parsées depuis son `wire.jsonl` (le `transcript_path` porté par
/// `AgentSnapshot`). Mêmes règles per-turn que le reader Stats (9.4) : lignes `type == "usage.record"`
/// uniquement, `usage.inputOther` = input non caché (déjà séparé, pas de soustraction), cache =
/// `inputCacheRead + inputCacheCreation`, `time` = ms Unix au top-level. Coût via
/// `pricing_for_kimi_model` (Moonshot ~$, modèle local = 0) — jamais la grille Claude.
/// Scope Kimi uniquement — le chemin Claude (`get_agent_stats_claude`) reste byte-identique.
pub fn get_agent_stats_kimi(transcript_path: &str) -> Result<AgentStats, String> {
    let file = std::fs::File::open(transcript_path).map_err(|e| e.to_string())?;
    let reader = std::io::BufReader::new(file);

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_tokens: u64 = 0;
    let mut cost_usd: f64 = 0.0;
    let mut first_ts_secs: Option<u64> = None;

    // `map_while(Result::ok)` (pas `flatten`) : évite le lint clippy `lines_filter_map_ok` → la
    // référence de 6 warnings sur la lib reste inchangée (AC9).
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        if first_ts_secs.is_none() {
            first_ts_secs = v.get("time")
                .and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64)))
                .map(|ms| ms / 1000);
        }

        if v.get("type").and_then(|t| t.as_str()) != Some("usage.record") { continue; }
        // Garde double-comptage (miroir reader Stats) : un `usageScope` cumulatif est ignoré.
        if let Some(scope) = v.get("usageScope").and_then(|s| s.as_str()) {
            if scope != "turn" { continue; }
        }
        let usage = match v.get("usage") { Some(u) => u, None => continue };

        let model = v.get("model").and_then(|m| m.as_str()).filter(|s| !s.is_empty()).unwrap_or("kimi");
        let input = usage.get("inputOther").and_then(|x| x.as_u64()).unwrap_or(0);
        let output = usage.get("output").and_then(|x| x.as_u64()).unwrap_or(0);
        let cache_read = usage.get("inputCacheRead").and_then(|x| x.as_u64()).unwrap_or(0);
        let cache_creation = usage.get("inputCacheCreation").and_then(|x| x.as_u64()).unwrap_or(0);

        input_tokens += input;
        output_tokens += output;
        cache_tokens += cache_read + cache_creation;
        cost_usd += crate::stats::pricing::calculate_cost_for_tool("kimi", input, output, cache_creation, cache_read, model);
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let duration_days = first_ts_secs.and_then(|ts| {
        let elapsed = now_secs.saturating_sub(ts);
        if elapsed >= 86400 { Some((elapsed / 86400) as u32) } else { None }
    });

    Ok(AgentStats {
        input_tokens,
        output_tokens,
        cache_tokens,
        cost_usd,
        duration_days,
    })
}

fn add_grok_agent_usage(
    usage: &serde_json::Value,
    model: &str,
    input_tokens: &mut u64,
    output_tokens: &mut u64,
    cache_tokens: &mut u64,
    cost_usd: &mut f64,
) {
    let input_total = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cached = usage.get("cachedReadTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let input_noncache = input_total.saturating_sub(cached);

    *input_tokens += input_noncache;
    *output_tokens += output;
    *cache_tokens += cached;
    *cost_usd += crate::stats::pricing::calculate_cost_for_tool("grok", input_noncache, output, 0, cached, model);
}

fn parse_iso8601_secs(ts: &str) -> Option<u64> {
    let ts = ts.trim_end_matches('Z');
    let (date_part, time_part) = ts.split_once('T')?;
    let parts: Vec<&str> = date_part.split('-').collect();
    if parts.len() != 3 { return None; }
    let y: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let d: u64 = parts[2].parse().ok()?;

    let time_parts: Vec<&str> = time_part.split(':').collect();
    if time_parts.len() < 3 { return None; }
    let h: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    let s: u64 = time_parts[2].split('.').next()?.parse().ok()?;

    let days = days_from_epoch(y, m, d)?;
    Some(days * 86400 + h * 3600 + min * 60 + s)
}

fn days_from_epoch(y: u64, m: u64, d: u64) -> Option<u64> {
    if !(1..=12).contains(&m) || d < 1 { return None; }
    let mut total: u64 = 0;
    for year in 1970..y {
        total += if is_leap(year) { 366 } else { 365 };
    }
    let leap = is_leap(y);
    let month_days: [u64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    total += month_days.iter().take(m as usize - 1).sum::<u64>();
    total += d - 1;
    Some(total)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(name: &str, lines: &[&str]) -> std::path::PathBuf {
        let unique = format!("{}_{}", name, std::process::id());
        let path = std::env::temp_dir().join(unique);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines { writeln!(f, "{}", line).unwrap(); }
        path
    }

    #[test]
    fn get_agent_stats_agrege_tous_les_messages() {
        let path = write_jsonl("agent_stats_t1.jsonl", &[
            r#"{"type":"user","message":{},"timestamp":"2026-05-18T10:00:00Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":10,"cache_read_input_tokens":5}},"timestamp":"2026-05-18T10:00:01Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":200,"output_tokens":80,"cache_creation_input_tokens":20,"cache_read_input_tokens":10}},"timestamp":"2026-05-18T10:00:02Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":300,"output_tokens":120,"cache_creation_input_tokens":30,"cache_read_input_tokens":15}},"timestamp":"2026-05-18T10:00:03Z"}"#,
        ]);
        let result = get_agent_stats_claude(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.input_tokens, 600);
        assert_eq!(result.output_tokens, 250);
        assert_eq!(result.cache_tokens, 90); // (10+5) + (20+10) + (30+15)
    }

    #[test]
    fn cout_calcule_via_pricing_rs() {
        let path = write_jsonl("agent_stats_t2.jsonl", &[
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":1000000,"output_tokens":1000000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2026-05-18T10:00:00Z"}"#,
        ]);
        let result = get_agent_stats_claude(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!((result.cost_usd - 18.0).abs() < 0.001, "coût attendu ~$18.00, obtenu {}", result.cost_usd);
    }

    #[test]
    fn duration_days_none_si_moins_dun_jour() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts_12h_ago = now - 43200;
        let days = ts_12h_ago / 86400;
        let rem = ts_12h_ago % 86400;
        let h = rem / 3600;
        let min = (rem % 3600) / 60;
        let s = rem % 60;
        let d = days + 1; // days since epoch to date
        // Build a date string from the timestamp
        let ts_str = format_ts_from_secs(ts_12h_ago);
        let path = write_jsonl("agent_stats_t3.jsonl", &[
            &format!(r#"{{"type":"user","message":{{}},"timestamp":"{}"}}"#, ts_str),
        ]);
        let _ = (d, h, min, s); // suppress warnings
        let result = get_agent_stats_claude(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.duration_days, None);
    }

    #[test]
    fn duration_days_some_si_plus_dun_jour() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts_3d_ago = now - 3 * 86400 - 3600; // 3 days + 1h ago
        let ts_str = format_ts_from_secs(ts_3d_ago);
        let path = write_jsonl("agent_stats_t4.jsonl", &[
            &format!(r#"{{"type":"user","message":{{}},"timestamp":"{}"}}"#, ts_str),
        ]);
        let result = get_agent_stats_claude(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.duration_days, Some(3));
    }

    #[test]
    fn jsonl_absent_retourne_err() {
        let result = get_agent_stats_claude("/nonexistent/path/to/file.jsonl");
        assert!(result.is_err());
    }

    #[test]
    fn ignore_messages_non_assistant() {
        let path = write_jsonl("agent_stats_t6.jsonl", &[
            r#"{"type":"user","message":{"usage":{"input_tokens":9999,"output_tokens":9999}},"timestamp":"2026-05-18T10:00:00Z"}"#,
            r#"{"type":"system","message":{"usage":{"input_tokens":9999,"output_tokens":9999}},"timestamp":"2026-05-18T10:00:00Z"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"timestamp":"2026-05-18T10:00:01Z"}"#,
        ]);
        let result = get_agent_stats_claude(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.output_tokens, 50);
    }

    // ── Grok (6.32) : get_agent_stats_grok ──────────────────────────────────────

    #[test]
    fn grok_agent_stats_somme_les_tours_et_exclut_le_cache() {
        // 2 tours turn_completed : sommés per-turn ; inputTokens inclut cachedReadTokens (soustrait).
        let path = write_jsonl("agent_grok_t1.jsonl", &[
            r#"{"timestamp":1783943155,"params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":1000,"outputTokens":100,"cachedReadTokens":600,"modelUsage":{"grok-4.5":{"inputTokens":1000,"outputTokens":100,"cachedReadTokens":600}}}}}}"#,
            r#"{"timestamp":1783943200,"params":{"sessionId":"s1","update":{"sessionUpdate":"content","text":"x"}}}"#,
            r#"{"timestamp":1783943300,"params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":2000,"outputTokens":200,"cachedReadTokens":0,"modelUsage":{"grok-4.5":{"inputTokens":2000,"outputTokens":200,"cachedReadTokens":0}}}}}}"#,
        ]);
        let result = get_agent_stats_grok(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.input_tokens, 2400, "1000-600 + 2000");
        assert_eq!(result.output_tokens, 300);
        assert_eq!(result.cache_tokens, 600);
        assert!(result.cost_usd > 0.0, "coût Grok doit être > 0");
    }

    #[test]
    fn grok_agent_stats_jsonl_absent_retourne_err() {
        assert!(get_agent_stats_grok("/nonexistent/grok/updates.jsonl").is_err());
    }

    // ── Kimi (9.4) : get_agent_stats_kimi ────────────────────────────────────────

    #[test]
    fn kimi_agent_stats_somme_usage_record_modele_local_cout_zero() {
        // 2 usage.record sommés. inputOther = input non caché (pas de soustraction) ;
        // cache = inputCacheRead + inputCacheCreation ; modèle LOCAL → coût 0 (garde-fou §5).
        let path = write_jsonl("agent_kimi_t1.jsonl", &[
            r#"{"type":"context.append_message","message":{"role":"user"},"time":1784738461022}"#,
            r#"{"type":"usage.record","model":"lmstudio/qwen","usage":{"inputOther":1000,"output":100,"inputCacheRead":600,"inputCacheCreation":200},"usageScope":"turn","time":1784738573817}"#,
            r#"{"type":"usage.record","model":"lmstudio/qwen","usage":{"inputOther":2000,"output":200,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1784738573900}"#,
        ]);
        let result = get_agent_stats_kimi(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.input_tokens, 3000);
        assert_eq!(result.output_tokens, 300);
        assert_eq!(result.cache_tokens, 800);
        assert_eq!(result.cost_usd, 0.0, "modèle auto-hébergé → aucun coût d'API");
    }

    #[test]
    fn kimi_agent_stats_modele_moonshot_facture() {
        // Cas général : un modèle Moonshot est facturé à la grille Moonshot (> 0).
        let path = write_jsonl("agent_kimi_moonshot.jsonl", &[
            r#"{"type":"usage.record","model":"kimi-k2.6","usage":{"inputOther":1000000,"output":1000000,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1784738573817}"#,
        ]);
        let result = get_agent_stats_kimi(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(result.cost_usd > 0.0, "modèle Moonshot facturé (cas général)");
    }

    #[test]
    fn kimi_agent_stats_jsonl_absent_retourne_err() {
        assert!(get_agent_stats_kimi("/nonexistent/kimi/wire.jsonl").is_err());
    }

    fn format_ts_from_secs(secs: u64) -> String {
        let days = secs / 86400;
        let rem = secs % 86400;
        let h = rem / 3600;
        let min = (rem % 3600) / 60;
        let s = rem % 60;
        let (y, m, d) = days_to_ymd(days);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, min, s)
    }

    fn days_to_ymd(mut days: u64) -> (u64, u8, u8) {
        let mut year = 1970u64;
        loop {
            let leap = super::is_leap(year);
            let days_in_year = if leap { 366 } else { 365 };
            if days < days_in_year { break; }
            days -= days_in_year;
            year += 1;
        }
        let leap = super::is_leap(year);
        let month_days = [31u8, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 0usize;
        for &md in &month_days {
            if days < md as u64 { break; }
            days -= md as u64;
            month += 1;
        }
        (year, (month + 1) as u8, (days + 1) as u8)
    }
}
