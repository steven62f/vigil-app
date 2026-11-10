use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::Deserialize;
use super::{CredentialsFile, OAuthCredentials};

fn credentials_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude").join(".credentials.json")
}

// ── Codex (OpenAI) ────────────────────────────────────────────────────────────
// Chemin `~/.codex/auth.json` re-déclaré localement (convention du projet :
// duplication assumée plutôt que couplage au chemin Claude). Spike T0 : la
// structure vient de `openai/codex` (crate `login`, `AuthDotJson`/`TokenData`).

fn codex_auth_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".codex").join("auth.json")
}

/// Enveloppe de `~/.codex/auth.json` (seuls les champs utiles sont désérialisés).
#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// Credentials Codex prêts à l'emploi pour le fetcher.
/// Pas de champ d'expiration en clair (le token est un JWT) → stratégie
/// « tenter puis mapper 401 » côté fetcher (cf. spike T0).
#[derive(Debug, Clone)]
pub struct CodexCredentials {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub fn read_codex_credentials() -> Result<CodexCredentials, String> {
    let content = std::fs::read_to_string(codex_auth_path())
        .map_err(|e| format!("credentials_not_found: {e}"))?;
    parse_codex_auth(&content)
}

/// Logique pure (testable sans disque) : extrait le token porteur et l'account_id.
fn parse_codex_auth(content: &str) -> Result<CodexCredentials, String> {
    let file: CodexAuthFile = serde_json::from_str(content)
        .map_err(|e| format!("credentials_parse_error: {e}"))?;
    let tokens = file
        .tokens
        .ok_or_else(|| "credentials_not_found: no tokens".to_string())?;
    let access_token = tokens
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "credentials_not_found: no access_token".to_string())?;
    Ok(CodexCredentials {
        access_token,
        // Un account_id vide serait envoyé comme entête `ChatGPT-Account-Id:` vide et
        // provoquerait un 401/403 parasite : le traiter comme absent (entête omis).
        account_id: tokens.account_id.filter(|s| !s.is_empty()),
    })
}

// ── Grok (xAI) ──────────────────────────────────────────────────────────────────
// Chemin `~/.grok/auth.json` : objet clé = `"https://auth.x.ai::<clientId>"` → valeur avec
// `key` (access token OIDC/JWT), `expires_at` (ISO8601 avec `Z`, rotation fréquente), `email`,
// `refresh_token`, … On préfère l'entrée du client CLI Grok connu, sinon la 1re entrée-objet
// portant un `key` non vide. `expires_at` < maintenant → `token_expired` (parité Claude/Codex).

fn grok_auth_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".grok").join("auth.json")
}

/// Client id du CLI Grok (entrée préférée de `auth.json`).
const GROK_CLI_CLIENT_KEY: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

/// Credentials Grok prêts pour le fetcher. Le token est un JWT OIDC ; l'expiration est vérifiée
/// à la lecture (`expires_at`), puis stratégie « tenter puis mapper 401 » côté fetcher.
#[derive(Debug, Clone)]
pub struct GrokCredentials {
    pub access_token: String,
}

pub fn read_grok_credentials() -> Result<GrokCredentials, String> {
    let content = std::fs::read_to_string(grok_auth_path())
        .map_err(|e| format!("credentials_not_found: {e}"))?;
    parse_grok_auth(&content)
}

/// Logique pure (testable sans disque) : extrait le token porteur de l'entrée préférée (ou de la
/// 1re entrée-objet avec un `key` non vide) et vérifie `expires_at`.
fn parse_grok_auth(content: &str) -> Result<GrokCredentials, String> {
    let root: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| format!("credentials_parse_error: {e}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| "credentials_parse_error: not an object".to_string())?;

    // Entrée préférée (client CLI Grok) si elle porte un `key` non vide ; sinon la 1re entrée-objet
    // avec un `key` non vide.
    let entry = obj
        .get(GROK_CLI_CLIENT_KEY)
        .filter(|v| entry_key(v).is_some())
        .or_else(|| obj.values().find(|v| entry_key(v).is_some()))
        .ok_or_else(|| "credentials_not_found: no key".to_string())?;

    let access_token = entry_key(entry)
        .ok_or_else(|| "credentials_not_found: no key".to_string())?;

    // Expiration : `expires_at` ISO8601 → si dépassée, token mort (message clair côté UI).
    if let Some(exp) = entry
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(iso8601_to_unix_secs)
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if exp < now {
            return Err("token_expired".to_string());
        }
    }

    Ok(GrokCredentials { access_token })
}

/// Retourne le `key` (access token) non vide d'une entrée `auth.json`, ou `None`.
fn entry_key(v: &serde_json::Value) -> Option<String> {
    v.get("key")
        .and_then(|k| k.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse un ISO8601 UTC (`"2026-07-20T13:12:56.419854+00:00"` / `…Z`) en secondes Unix, en ne
/// lisant que les 19 premiers caractères `YYYY-MM-DDTHH:MM:SS` (fractions/fuseau tolérés). `None`
/// si le format ne colle pas → « expiration inconnue » (on tente le fetch). Miroir de
/// `manager::rfc3339_to_unix` (dupliqué : pas de couplage inter-modules pour un helper de 20 lignes).
fn iso8601_to_unix_secs(s: &str) -> Option<i64> {
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

/// Jours (signés) depuis 1970-01-01 (algorithme de Howard Hinnant), pour `iso8601_to_unix_secs`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn read_credentials() -> Result<OAuthCredentials, String> {
    let content = std::fs::read_to_string(credentials_path())
        .map_err(|e| format!("credentials_not_found: {e}"))?;
    let file: CredentialsFile = serde_json::from_str(&content)
        .map_err(|e| format!("credentials_parse_error: {e}"))?;
    let creds = file.claude_ai_oauth;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    if creds.expires_at < now_ms {
        return Err("token_expired".to_string());
    }
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    fn future_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000 // +1h
    }

    fn past_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 3_600_000 // -1h
    }

    #[test]
    fn credentials_lit_token_valide() {
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-test","expiresAt":{}}}}}"#,
            future_ms()
        );
        let file: crate::usage::CredentialsFile = serde_json::from_str(&json).unwrap();
        let creds = file.claude_ai_oauth;
        assert_eq!(creds.access_token, "sk-ant-oat01-test");
        assert!(creds.expires_at > 0);
    }

    #[test]
    fn credentials_detecte_token_expire() {
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-old","expiresAt":{}}}}}"#,
            past_ms()
        );
        let file: crate::usage::CredentialsFile = serde_json::from_str(&json).unwrap();
        let creds = file.claude_ai_oauth;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(creds.expires_at < now_ms, "token should be expired");
    }

    // ── Codex ──────────────────────────────────────────────────────────────────

    #[test]
    fn codex_credentials_token_et_account_present() {
        let json = r#"{
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "eyJhbGciOi.header.sig",
                "access_token": "eyJcodex.access.token",
                "refresh_token": "rft-xyz",
                "account_id": "acc-1234"
            },
            "last_refresh": "2026-06-01T10:00:00Z"
        }"#;
        let creds = super::parse_codex_auth(json).unwrap();
        assert_eq!(creds.access_token, "eyJcodex.access.token");
        assert_eq!(creds.account_id.as_deref(), Some("acc-1234"));
    }

    #[test]
    fn codex_credentials_account_id_optionnel() {
        // account_id absent → None, mais le token suffit (header ChatGPT-Account-Id omis).
        let json = r#"{"tokens":{"access_token":"tok-only"}}"#;
        let creds = super::parse_codex_auth(json).unwrap();
        assert_eq!(creds.access_token, "tok-only");
        assert!(creds.account_id.is_none());
    }

    #[test]
    fn codex_credentials_account_id_vide_traite_comme_absent() {
        // account_id chaîne vide → None (sinon entête ChatGPT-Account-Id vide → 401 parasite).
        let json = r#"{"tokens":{"access_token":"tok","account_id":""}}"#;
        let creds = super::parse_codex_auth(json).unwrap();
        assert!(creds.account_id.is_none());
    }

    #[test]
    fn codex_credentials_token_absent_erreur() {
        // Bloc tokens présent mais sans access_token → credentials_not_found (pas de panique).
        let json = r#"{"tokens":{"refresh_token":"rft"}}"#;
        let err = super::parse_codex_auth(json).unwrap_err();
        assert!(err.starts_with("credentials_not_found"), "got: {err}");
    }

    #[test]
    fn codex_credentials_json_invalide_erreur() {
        let err = super::parse_codex_auth("not json").unwrap_err();
        assert!(err.starts_with("credentials_parse_error"), "got: {err}");
    }

    // ── Grok (6.32) ────────────────────────────────────────────────────────────

    fn future_iso() -> String {
        // Année lointaine : toujours dans le futur (test insensible à l'horloge).
        "2099-01-01T00:00:00Z".to_string()
    }

    #[test]
    fn grok_credentials_token_valide() {
        let json = format!(
            r#"{{"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{{"key":"grok-token-abc","expires_at":"{}","email":"a@b.c"}}}}"#,
            future_iso()
        );
        let creds = super::parse_grok_auth(&json).unwrap();
        assert_eq!(creds.access_token, "grok-token-abc");
    }

    #[test]
    fn grok_credentials_prefere_entree_client_cli() {
        // Deux entrées : l'entrée client CLI Grok est préférée à une autre entrée-objet.
        let json = format!(
            r#"{{
                "https://auth.x.ai::autre-client":{{"key":"mauvais","expires_at":"{f}"}},
                "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{{"key":"bon","expires_at":"{f}"}}
            }}"#,
            f = future_iso()
        );
        let creds = super::parse_grok_auth(&json).unwrap();
        assert_eq!(creds.access_token, "bon");
    }

    #[test]
    fn grok_credentials_repli_premiere_entree_si_pas_de_client_cli() {
        // Entrée client CLI absente → 1re entrée-objet avec un `key` non vide.
        let json = format!(
            r#"{{"https://auth.x.ai::autre":{{"key":"seul-token","expires_at":"{}"}}}}"#,
            future_iso()
        );
        let creds = super::parse_grok_auth(&json).unwrap();
        assert_eq!(creds.access_token, "seul-token");
    }

    #[test]
    fn grok_credentials_token_expire() {
        let json = r#"{"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{"key":"vieux","expires_at":"2000-01-01T00:00:00Z"}}"#;
        let err = super::parse_grok_auth(json).unwrap_err();
        assert_eq!(err, "token_expired");
    }

    #[test]
    fn grok_credentials_expires_at_absent_toleré() {
        // Pas d'`expires_at` → on ne peut pas juger l'expiration : on tente (token retourné).
        let json = r#"{"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{"key":"sans-exp"}}"#;
        let creds = super::parse_grok_auth(json).unwrap();
        assert_eq!(creds.access_token, "sans-exp");
    }

    #[test]
    fn grok_credentials_key_absente_erreur() {
        let json = r#"{"https://auth.x.ai::x":{"email":"a@b.c"}}"#;
        let err = super::parse_grok_auth(json).unwrap_err();
        assert!(err.starts_with("credentials_not_found"), "got: {err}");
    }

    #[test]
    fn grok_credentials_json_invalide_erreur() {
        let err = super::parse_grok_auth("not json").unwrap_err();
        assert!(err.starts_with("credentials_parse_error"), "got: {err}");
    }
}
