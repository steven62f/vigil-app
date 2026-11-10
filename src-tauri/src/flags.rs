use crate::agent::state::SourceTool;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Outils dont l'interruption fonctionne réellement, c.-à-d. dont le bridge CONSOMME le flag écrit
/// par `interrupt_agent` (Story 6.33). Seul `vigil-claude` le fait
/// (`handle_interrupt_flag_claude_code`) et tue le tour via `{"continue":false}`.
///
/// Grok ne peut pas : son contrat hook est deny-only (veto par-outil, l'agent reroute et finit son
/// tour — confirmé e2e, spike 6.33) ; la vraie parité passe par ACP. Codex / Antigravity n'ont
/// aucun canal câblé. Sans cette garde, un `interrupt_agent` sur ces outils écrivait un flag que
/// personne ne consomme → `interrupt_pending` bloqué à vie + flag orphelin.
///
/// ⚠️ **Miroir TS obligatoire** : `INTERRUPT_SUPPORTED` (`src/lib/statsTools.ts`) gate l'affordance
/// UI avec la même vérité — les deux DOIVENT rester synchro. N'ajouter un outil ici qu'APRÈS avoir
/// câblé la consommation du flag dans son bridge.
pub fn interrupt_supported(tool: &SourceTool) -> bool {
    // `match` exhaustif volontaire (et non `matches!`) : l'ajout d'un variant à `SourceTool` DOIT
    // casser la compilation ici pour forcer la question « ce bridge consomme-t-il le flag ? ».
    // Un `_ => false` (ou un `matches!`) répondrait « non » en silence → affordance morte à retrouver.
    match tool {
        SourceTool::ClaudeCode => true,
        // Kimi (Epic 9) : aucun canal d'interruption câblé — son contrat hook est deny-only comme
        // celui de Grok (`permissionDecision:"allow"` est ignoré par design), et `vigil-kimi` ne
        // consomme aucun flag. 6.33 le classe donc hors `INTERRUPT_SUPPORTED`.
        SourceTool::Codex
        | SourceTool::Grok
        | SourceTool::Antigravity
        | SourceTool::Kimi
        | SourceTool::Unknown => false,
    }
}

/// Hash SHA256-tronqué (**16 octets → 32 hex**) du chemin de transcript.
///
/// ⚠️ **CONTRAT INTER-CRATES — doit rester byte-identique à `vigil_hook_core::transcript_hash`.**
/// Cette fonction NOMME le flag d'interrupt que les binaires de hook vont CHERCHER : toute
/// divergence = flag jamais trouvé = **interruption morte en silence**. C'est arrivé (4.14 a élargi
/// celle-ci 8 → 16 octets en ne mettant à jour que le test local → interruption cassée pour tous les
/// outils du 2026-05-18 au 2026-07-17). Verrouillé par `transcript_hash_contrat_inter_crates`
/// (vecteurs dupliqués des deux côtés).
///
/// ⚠️ Sert AUSSI de clé PERSISTÉE à `session_meta` (`session_meta:{hash}`) : la rétrécir
/// orphelinerait les noms/tags/épinglages de sessions existants. En cas de nouvelle divergence,
/// c'est `vigil-hook-core` qui s'aligne sur celle-ci, jamais l'inverse.
pub(crate) fn transcript_hash(transcript_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(transcript_path.as_bytes());
    let result = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &result[..16] {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

pub fn interrupt_flag_path(transcript_path: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vigil-interrupt-{}.flag", transcript_hash(transcript_path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_hash_stable() {
        let h1 = transcript_hash("/path/to/transcript.jsonl");
        let h2 = transcript_hash("/path/to/transcript.jsonl");
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
        assert_eq!(h1.len(), 32);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Story 6.36 — VERROU du contrat inter-crates. Vecteurs DUPLIQUÉS à l'identique dans
    /// `vigil_hook_core::tests::transcript_hash_contrat_inter_crates` : les deux crates ne pouvant
    /// pas se dépendre, c'est le seul moyen mécanique de faire ROUGIR un test si l'une des copies
    /// dérive. Ne PAS « corriger » une valeur ici sans corriger l'autre côté.
    #[test]
    fn transcript_hash_contrat_inter_crates() {
        assert_eq!(transcript_hash("/foo/bar.jsonl"), "92cd7836e480832bdb9be281b4752a79");
        assert_eq!(transcript_hash(r"C:\x\y.jsonl"), "fe29a574fcf05fcf7409c4fdf4d0f33a");
    }

    /// T3 (6.33) : miroir Rust de `INTERRUPT_SUPPORTED` (TS). Claude seul consomme le flag.
    #[test]
    fn interrupt_supported_claude_only() {
        assert!(interrupt_supported(&SourceTool::ClaudeCode));
        for tool in [
            SourceTool::Codex,
            SourceTool::Grok,
            SourceTool::Antigravity,
            SourceTool::Kimi,
            SourceTool::Unknown,
        ] {
            assert!(!interrupt_supported(&tool), "{tool:?} ne consomme pas le flag d'interrupt");
        }
    }

    #[test]
    fn interrupt_path_utilise_temp_dir() {
        let path = interrupt_flag_path("/test.jsonl");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("vigil-interrupt-"));
        assert!(path.starts_with(std::env::temp_dir()));
    }
}
