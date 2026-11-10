use serde::{Deserialize, Serialize};

// Liste ORDONNÉE de fenêtres de quota, agnostique aux noms de champs (Story 6.15). L'ordre
// porte le sens : `windows[0]` = fenêtre primaire (courante), les suivantes = secondaires.
// C'est le fetcher de chaque outil qui attribue `id` + `label` en construisant la liste ;
// l'UI ne fait plus que boucler → une fenêtre d'un type inédit s'affiche automatiquement.
//
// Serde camelCase SYMÉTRIQUE : la (dé)sérialisation ne sert plus qu'au cache (l'API n'est plus
// désérialisée directement dans ce struct — cf. `parse_claude_usage`/`parse_codex_usage`). PAS
// de `#[serde(default)]` : un cache legacy (4 champs nommés, sans clé `windows`) doit ÉCHOUER à
// la désérialisation → `None` via le `.ok()` de `read_cached_data` → refetch propre (AC7).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageStats {
    pub windows: Vec<WindowUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowUsage {
    pub id: String,
    pub label: String,
    pub utilization: f32,
    pub resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsFile {
    pub claude_ai_oauth: OAuthCredentials,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredentials {
    pub access_token: String,
    pub expires_at: u64, // millisecondes depuis epoch Unix
}

pub mod credentials;
pub mod fetcher;
pub mod manager;
