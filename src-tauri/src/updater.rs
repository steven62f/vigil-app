/// Dernière release de ce dépôt. L'installeur NSIS est un asset de la release.
/// Vigil ouvre la page de release : il ne consomme pas de `latest.json` signé.
const GITHUB_API_URL: &str =
    "https://api.github.com/repos/steven62f/vigil-app/releases/latest";
const USER_AGENT: &str = concat!("vigil/", env!("CARGO_PKG_VERSION"));

#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePayload {
    pub version: String,
    pub url: String,
}

pub async fn check_for_updates() -> Option<UpdatePayload> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .get(GITHUB_API_URL)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .ok()?;

    let release = resp.error_for_status().ok()?.json::<GithubRelease>().await.ok()?;

    let remote = release.tag_name.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");

    if compare_versions(current, remote) {
        Some(UpdatePayload {
            version: remote.to_string(),
            url: release.html_url,
        })
    } else {
        None
    }
}

fn compare_versions(current: &str, remote: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let mut parts = s.splitn(3, '.');
        let ma = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let mi = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let pa = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (ma, mi, pa)
    };
    parse(remote) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::compare_versions;

    #[test]
    fn nouvelle_version_detectee() {
        assert!(compare_versions("1.0.2", "1.0.3"));
        assert!(compare_versions("1.0.2", "1.1.0"));
        assert!(compare_versions("1.0.2", "2.0.0"));
    }

    #[test]
    fn meme_version_pas_update() {
        assert!(!compare_versions("1.0.2", "1.0.2"));
    }

    #[test]
    fn version_inferieure_pas_update() {
        assert!(!compare_versions("1.0.3", "1.0.2"));
    }

    #[test]
    fn tag_v_prefixe_parse_correctement() {
        let remote = "v1.0.3".trim_start_matches('v');
        assert!(compare_versions("1.0.2", remote));
    }

    #[test]
    fn endpoint_est_le_depot_vigil_app() {
        assert_eq!(
            GITHUB_API_URL,
            "https://api.github.com/repos/steven62f/vigil-app/releases/latest"
        );
        assert!(!GITHUB_API_URL.contains("vigil-releases"));
    }
}
