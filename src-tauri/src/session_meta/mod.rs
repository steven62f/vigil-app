use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
}

pub fn session_meta_key(transcript_path: &str) -> String {
    format!("session_meta:{}", crate::flags::transcript_hash(transcript_path))
}

pub fn apply_to_snapshot(snapshot: &mut crate::agent::state::AgentSnapshot, meta: SessionMetadata) {
    if let Some(name) = &meta.custom_name {
        if !name.is_empty() {
            snapshot.name = name.clone();
        }
    }
    snapshot.custom_name = meta.custom_name;
    snapshot.description = meta.description;
    snapshot.tags = meta.tags;
    snapshot.pinned = meta.pinned;
}

pub fn enrich_snapshots(
    app: &tauri::AppHandle,
    snapshots: Vec<crate::agent::state::AgentSnapshot>,
) -> Vec<crate::agent::state::AgentSnapshot> {
    use tauri_plugin_store::StoreExt;
    let store = app.store(crate::SETTINGS_STORE).ok();
    snapshots
        .into_iter()
        .map(|mut s| {
            if let Some(store) = &store {
                let key = session_meta_key(&s.transcript_path);
                if let Some(meta) = store
                    .get(&key)
                    .and_then(|v| serde_json::from_value::<SessionMetadata>(v).ok())
                {
                    apply_to_snapshot(&mut s, meta);
                }
            }
            s
        })
        .collect()
}

pub fn enrich_groups(
    app: &tauri::AppHandle,
    groups: Vec<crate::history::reader::ProjectGroup>,
) -> Vec<crate::history::reader::ProjectGroup> {
    use tauri_plugin_store::StoreExt;
    let store = app.store(crate::SETTINGS_STORE).ok();
    groups
        .into_iter()
        .map(|mut group| {
            for s in &mut group.sessions {
                if let Some(store) = &store {
                    let key = session_meta_key(&s.transcript_path);
                    if let Some(meta) = store
                        .get(&key)
                        .and_then(|v| serde_json::from_value::<SessionMetadata>(v).ok())
                    {
                        s.custom_name = meta.custom_name;
                        s.description = meta.description;
                        s.tags = meta.tags;
                        s.pinned = meta.pinned;
                    }
                }
            }
            // Tri final (pinned d'abord, puis date desc) + truncate à 20 :
            // doit avoir lieu APRÈS l'enrichissement pour ne pas perdre une session
            // épinglée ancienne au rang > 20 par date.
            // `last_activity` reste celui calculé en amont (max date sur TOUTES les
            // sessions du groupe, avant truncate) — pas écrasé ici.
            crate::history::reader::finalize_group_session_ordering(&mut group, 20);
            group
        })
        .collect()
}

pub fn validate(meta: &SessionMetadata) -> Result<(), String> {
    if let Some(name) = &meta.custom_name {
        if name.trim().is_empty() {
            return Err("name_blank".to_string());
        }
        if name.chars().count() > 40 {
            return Err("name_too_long".to_string());
        }
    }
    if let Some(desc) = &meta.description {
        if desc.trim().is_empty() {
            return Err("description_blank".to_string());
        }
        if desc.chars().count() > 200 {
            return Err("description_too_long".to_string());
        }
    }
    if meta.tags.len() > 2 {
        return Err("too_many_tags".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for tag in &meta.tags {
        if tag.trim().is_empty() {
            return Err("tag_blank".to_string());
        }
        if tag.chars().count() > 20 {
            return Err("tag_too_long".to_string());
        }
        if !seen.insert(tag.to_lowercase()) {
            return Err("tag_duplicate".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_absente_retourne_default() {
        let meta = SessionMetadata::default();
        assert!(meta.custom_name.is_none());
        assert!(meta.description.is_none());
        assert!(meta.tags.is_empty());
        assert!(!meta.pinned);
    }

    #[test]
    fn validation_rejecte_nom_trop_long() {
        let meta = SessionMetadata {
            custom_name: Some("a".repeat(41)),
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("name_too_long".to_string()));
    }

    #[test]
    fn validation_accepte_nom_de_40_caracteres() {
        let meta = SessionMetadata {
            custom_name: Some("a".repeat(40)),
            ..Default::default()
        };
        assert!(validate(&meta).is_ok());
    }

    #[test]
    fn validation_rejecte_description_trop_longue() {
        let meta = SessionMetadata {
            description: Some("a".repeat(201)),
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("description_too_long".to_string()));
    }

    #[test]
    fn validation_rejecte_plus_de_2_tags() {
        let meta = SessionMetadata {
            tags: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("too_many_tags".to_string()));
    }

    #[test]
    fn validation_rejecte_tag_trop_long() {
        let meta = SessionMetadata {
            tags: vec!["a".repeat(21)],
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("tag_too_long".to_string()));
    }

    #[test]
    fn validation_rejecte_nom_whitespace_uniquement() {
        let meta = SessionMetadata {
            custom_name: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("name_blank".to_string()));
    }

    #[test]
    fn validation_rejecte_description_whitespace_uniquement() {
        let meta = SessionMetadata {
            description: Some("\t\n  ".to_string()),
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("description_blank".to_string()));
    }

    #[test]
    fn validation_rejecte_tag_vide_ou_whitespace() {
        let meta = SessionMetadata {
            tags: vec!["rust".to_string(), "   ".to_string()],
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("tag_blank".to_string()));
    }

    #[test]
    fn validation_rejecte_tag_dupliques_case_insensitive() {
        let meta = SessionMetadata {
            tags: vec!["rust".to_string(), "Rust".to_string()],
            ..Default::default()
        };
        assert_eq!(validate(&meta), Err("tag_duplicate".to_string()));
    }

    #[test]
    fn validation_accepte_metadata_complete() {
        let meta = SessionMetadata {
            custom_name: Some("Mon projet".to_string()),
            description: Some("Description courte".to_string()),
            tags: vec!["rust".to_string(), "tauri".to_string()],
            pinned: true,
        };
        assert!(validate(&meta).is_ok());
    }

    #[test]
    fn session_meta_key_stable_et_prefixe() {
        let k1 = session_meta_key("/path/to/transcript.jsonl");
        let k2 = session_meta_key("/path/to/transcript.jsonl");
        assert_eq!(k1, k2);
        assert!(k1.starts_with("session_meta:"));
        assert_eq!(k1.len(), "session_meta:".len() + 32);
    }

    #[test]
    fn session_meta_key_different_par_path() {
        let k1 = session_meta_key("/a.jsonl");
        let k2 = session_meta_key("/b.jsonl");
        assert_ne!(k1, k2);
    }

    use crate::agent::state::{AgentSnapshot, AgentState};

    fn make_test_snapshot() -> AgentSnapshot {
        AgentSnapshot {
            transcript_path: "/t.jsonl".to_string(),
            session_id: Some("s1".to_string()),
            name: "vigil".to_string(),
            state: AgentState::Idle,
            waiting_since: None,
            last_activity: 0,
            last_activity_label: None,
            terminal_pid: None,
            source: "Terminal".to_string(),
            permission_request: None,
            interrupt_pending: false,
            model: None,
            cwd: None,
            subagents: vec![],
            custom_name: None,
            description: None,
            tags: vec![],
            pinned: false,
            source_tool: crate::agent::state::SourceTool::Unknown,
        }
    }

    #[test]
    fn nom_personnalise_remplace_nom_projet_dans_snapshot() {
        let mut snap = make_test_snapshot();
        let meta = SessionMetadata {
            custom_name: Some("Mon vigil custom".to_string()),
            ..Default::default()
        };
        apply_to_snapshot(&mut snap, meta);
        assert_eq!(snap.name, "Mon vigil custom");
    }

    #[test]
    fn nom_personnalise_vide_ne_remplace_pas() {
        let mut snap = make_test_snapshot();
        let meta = SessionMetadata {
            custom_name: Some("".to_string()),
            ..Default::default()
        };
        apply_to_snapshot(&mut snap, meta);
        assert_eq!(snap.name, "vigil");
    }

    #[test]
    fn nom_personnalise_absent_garde_nom_origine() {
        let mut snap = make_test_snapshot();
        let meta = SessionMetadata::default();
        apply_to_snapshot(&mut snap, meta);
        assert_eq!(snap.name, "vigil");
    }

    #[test]
    fn apply_to_snapshot_copie_description_tags_pinned() {
        let mut snap = make_test_snapshot();
        let meta = SessionMetadata {
            description: Some("Une description".to_string()),
            tags: vec!["rust".to_string()],
            pinned: true,
            ..Default::default()
        };
        apply_to_snapshot(&mut snap, meta);
        assert_eq!(snap.description.as_deref(), Some("Une description"));
        assert_eq!(snap.tags, vec!["rust"]);
        assert!(snap.pinned);
    }

    #[test]
    fn apply_to_snapshot_expose_custom_name_brut() {
        let mut snap = make_test_snapshot();
        let meta = SessionMetadata {
            custom_name: Some("Mon custom".to_string()),
            ..Default::default()
        };
        apply_to_snapshot(&mut snap, meta);
        assert_eq!(snap.name, "Mon custom");
        assert_eq!(snap.custom_name.as_deref(), Some("Mon custom"));
    }

    #[test]
    fn apply_to_snapshot_custom_name_absent_reste_none() {
        let mut snap = make_test_snapshot();
        apply_to_snapshot(&mut snap, SessionMetadata::default());
        assert!(snap.custom_name.is_none());
    }

    #[test]
    fn serialisation_camel_case() {
        let meta = SessionMetadata {
            custom_name: Some("test".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("customName"));
        assert!(!json.contains("custom_name"));
    }
}
