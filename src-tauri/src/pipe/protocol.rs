use serde::Deserialize;
use crate::agent::state::SourceTool;

pub const ALLOW_RESPONSE: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
pub const DENY_RESPONSE: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}"#;

#[derive(Debug, Deserialize)]
#[serde(tag = "hook_event_name")]
pub enum HookEvent {
    SessionStart(SessionStartPayload),
    SessionEnd(SessionEndPayload),
    PreToolUse(PreToolUsePayload),
    PermissionRequest(PermissionRequestPayload),
    PostToolUse(PostToolUsePayload),
    PostToolUseFailure(PostToolUseFailurePayload),
    Notification(NotificationPayload),
    Stop(StopPayload),
    StopFailure(StopFailurePayload),
    UserPromptSubmit(UserPromptSubmitPayload),
    SubagentStart(SubagentStartPayload),
    SubagentStop(SubagentStopPayload),
}

#[derive(Debug, Deserialize)]
pub struct SessionStartPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub source: Option<String>,
    pub model: Option<String>,
    pub parent_pid: Option<u32>,
    /// Libellé de l'hôte d'exécution injecté par le hook (« VS Code », « Windows Terminal »,
    /// « PowerShell »…). Absent → l'app garde le défaut « Terminal ».
    #[serde(default)]
    pub terminal_source: Option<String>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct SessionEndPayload {
    #[allow(dead_code)] // non lu pour l'instant.
    pub session_id: String,
    pub transcript_path: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub cwd: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct PreToolUsePayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct PermissionRequestPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
    /// Story 6.30 (Grok) — `true` = permission en mode `default`/ask : Vigil AFFICHE en lecture-seule
    /// (aucun bouton, invite terminal) car le hook Grok ne peut PAS bypass en ask (`allow` sans effet,
    /// validé e2e run 6). `false` (défaut, ex. Claude/Codex ou Grok `bypassPermissions`) = overlay
    /// actionnable (approve/deny). Ne bloque PAS le chemin des autres outils (serde default).
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct PostToolUsePayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    #[allow(dead_code)] // non lu pour l'instant.
    pub tool_response: serde_json::Value,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct PostToolUseFailurePayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    #[allow(dead_code)] // non lu pour l'instant.
    pub error: Option<String>,
    #[serde(default)]
    pub is_interrupt: bool,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct NotificationPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub message: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub notification_type: Option<String>,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct StopPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
    /// Story 8.1 (revue) — `true` uniquement sur le `Stop` **synthétique** émis par
    /// `vigil_hook_core::send_synthetic_stop` quand l'utilisateur a cliqué « Interrompre » dans
    /// Vigil. Sert à ne PAS notifier « ✅ terminé » un utilisateur qui vient de couper son agent.
    ///
    /// `#[serde(default)]` : un binaire de hook pas encore réinstallé omet le champ → `false` →
    /// on notifie, c'est-à-dire le comportement d'avant. Dégradation propre, jamais de casse.
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Debug, Deserialize)]
pub struct StopFailurePayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub error_type: Option<String>,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct UserPromptSubmitPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub prompt: Option<String>,
    pub parent_pid: Option<u32>,
    #[serde(default)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct SubagentStartPayload {
    #[allow(dead_code)] // non lu pour l'instant.
    pub session_id: String,
    pub transcript_path: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub cwd: String,
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub source_tool: SourceTool,
}

#[derive(Debug, Deserialize)]
pub struct SubagentStopPayload {
    #[allow(dead_code)] // non lu pour l'instant.
    pub session_id: String,
    pub transcript_path: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub cwd: String,
    pub agent_id: String,
    #[allow(dead_code)] // non lu pour l'instant.
    pub agent_type: String,
    #[serde(default)]
    #[allow(dead_code)] // non lu pour l'instant.
    pub agent_transcript_path: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // non lu pour l'instant.
    pub stop_hook_active: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub source_tool: SourceTool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::SourceTool;

    #[test]
    fn parse_session_start() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "abc123",
            "transcript_path": "/home/user/.claude/projects/vigil/conversation.jsonl",
            "cwd": "/home/user/vigil"
        }"#;
        let event: HookEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, HookEvent::SessionStart(_)));
    }

    #[test]
    fn parse_permission_request_waiting_approve() {
        let json = r#"{
            "hook_event_name": "PermissionRequest",
            "session_id": "abc123",
            "transcript_path": "/path/conv.jsonl",
            "cwd": "/project",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf dist/"}
        }"#;
        let event: HookEvent = serde_json::from_str(json).unwrap();
        match event {
            HookEvent::PermissionRequest(p) => assert_eq!(p.tool_name, "Bash"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_ask_user_question() {
        let json = r#"{
            "hook_event_name": "PermissionRequest",
            "session_id": "abc123",
            "transcript_path": "/path/conv.jsonl",
            "cwd": "/project",
            "tool_name": "AskUserQuestion",
            "tool_input": {
                "questions": [{"question": "Quelle branche ?", "header": "Git", "options": [], "multiSelect": false}]
            }
        }"#;
        let event: HookEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, HookEvent::PermissionRequest(p) if p.tool_name == "AskUserQuestion"));
    }

    #[test]
    fn parse_post_tool_use() {
        let json = r#"{
            "hook_event_name": "PostToolUse",
            "session_id": "abc",
            "transcript_path": "/p",
            "cwd": "/c",
            "tool_name": "Read",
            "tool_input": {"file_path": "src/main.rs"},
            "tool_response": {}
        }"#;
        assert!(matches!(serde_json::from_str::<HookEvent>(json).unwrap(), HookEvent::PostToolUse(_)));
    }

    #[test]
    fn parse_post_tool_use_failure_interrupt() {
        let json = r#"{
            "hook_event_name": "PostToolUseFailure",
            "session_id": "abc123",
            "transcript_path": "/t.jsonl",
            "cwd": "/project",
            "tool_name": "Bash",
            "tool_input": {"command": "npm test"},
            "error": "Command exited with non-zero status code 1",
            "is_interrupt": true
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUseFailure(p) => {
                assert!(p.is_interrupt);
                assert_eq!(p.tool_name, "Bash");
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_post_tool_use_failure_no_interrupt() {
        let json = r#"{
            "hook_event_name": "PostToolUseFailure",
            "session_id": "abc123",
            "transcript_path": "/t.jsonl",
            "cwd": "/project",
            "tool_name": "Bash",
            "tool_input": {"command": "npm test"},
            "error": "Command exited with non-zero status code 1",
            "is_interrupt": false
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUseFailure(p) => assert!(!p.is_interrupt),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_notification() {
        let json = r#"{
            "hook_event_name": "Notification",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "message": "Test message"
        }"#;
        assert!(matches!(serde_json::from_str::<HookEvent>(json).unwrap(), HookEvent::Notification(_)));
    }

    #[test]
    fn parse_stop() {
        let json = r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/t","cwd":"/c"}"#;
        assert!(matches!(serde_json::from_str::<HookEvent>(json).unwrap(), HookEvent::Stop(_)));
    }

    #[test]
    fn parse_stop_failure() {
        let json = r#"{"hook_event_name":"StopFailure","session_id":"s","transcript_path":"/t","cwd":"/c"}"#;
        assert!(matches!(serde_json::from_str::<HookEvent>(json).unwrap(), HookEvent::StopFailure(_)));
    }

    #[test]
    fn parse_session_end() {
        let json = r#"{"hook_event_name":"SessionEnd","session_id":"s","transcript_path":"/t","cwd":"/c","reason":"clear"}"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionEnd(p) => assert_eq!(p.reason.as_deref(), Some("clear")),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_pre_tool_use() {
        let json = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo build"}
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PreToolUse(p) => assert_eq!(p.tool_name, "Bash"),
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_user_prompt_submit() {
        let json = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s1",
            "transcript_path": "/t.jsonl",
            "cwd": "/project",
            "prompt": "Bonjour Claude",
            "parent_pid": 1234
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::UserPromptSubmit(p) => {
                assert_eq!(p.session_id, "s1");
                assert_eq!(p.transcript_path, "/t.jsonl");
                assert_eq!(p.prompt.as_deref(), Some("Bonjour Claude"));
                assert_eq!(p.parent_pid, Some(1234));
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn parse_user_prompt_submit_sans_prompt() {
        let json = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s1",
            "transcript_path": "/t.jsonl",
            "cwd": "/project"
        }"#;
        let event = serde_json::from_str::<HookEvent>(json).unwrap();
        assert!(matches!(event, HookEvent::UserPromptSubmit(p) if p.prompt.is_none()));
    }

    #[test]
    fn parse_subagent_start() {
        let json = r#"{
            "hook_event_name": "SubagentStart",
            "session_id": "s1",
            "transcript_path": "/parent.jsonl",
            "cwd": "/project",
            "agent_id": "agent-uuid-1",
            "agent_type": "Explore"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SubagentStart(p) => {
                assert_eq!(p.agent_id, "agent-uuid-1");
                assert_eq!(p.agent_type, "Explore");
                assert_eq!(p.transcript_path, "/parent.jsonl");
            }
            _ => panic!("wrong event type"),
        }
    }

    // ── source_tool désérialisation ───────────────────────────────────────────

    #[test]
    fn source_tool_codex_deserialise_correctement() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "codex"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Codex),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_claude_code_deserialise_correctement() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "claude_code"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::ClaudeCode),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_grok_deserialise_correctement() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Grok),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_antigravity_deserialise_correctement() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "antigravity"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Antigravity),
            _ => panic!("wrong variant"),
        }
    }

    // ── Antigravity : preuve que la sortie du traducteur (vigil-antigravity) désérialise
    //    dans la bonne variante HookEvent (story 6.17, T6 option b). Payloads = forme NORMALISÉE
    //    (snake_case, hook_event_name injecté) telle qu'écrite sur le pipe par le binaire.
    #[test]
    fn antigravity_pre_tool_use_normalise_deserialise() {
        let json = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "269a0c26-5309-44b7-b3bf-1e7b295dd45a",
            "transcript_path": "C:/Users/Dev/.gemini/antigravity-cli/brain/269a0c26/.system_generated/logs/transcript_full.jsonl",
            "cwd": "C:/Users/Dev",
            "tool_name": "run_command",
            "tool_input": {"CommandLine": "dir", "Cwd": "C:\\Users\\Dev"},
            "source_tool": "antigravity"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PreToolUse(p) => {
                assert_eq!(p.tool_name, "run_command");
                assert_eq!(p.source_tool, SourceTool::Antigravity);
                assert_eq!(p.tool_input["CommandLine"], "dir");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn antigravity_post_tool_use_failure_normalise_deserialise() {
        // error != "" côté Antigravity → PostToolUseFailure (tool_response non requis).
        let json = r#"{
            "hook_event_name": "PostToolUseFailure",
            "session_id": "269a0c26",
            "transcript_path": "/t.jsonl",
            "cwd": "C:/Users/Dev",
            "tool_name": "run_command",
            "tool_input": {"CommandLine": "dir"},
            "error": "boom",
            "source_tool": "antigravity"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUseFailure(p) => {
                assert_eq!(p.error.as_deref(), Some("boom"));
                assert_eq!(p.source_tool, SourceTool::Antigravity);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_post_tool_use_failure_normalise_deserialise() {
        // Story 6.37 : Grok PostToolUseFailure (error dérivé de toolResult) → désérialisation sans
        // is_interrupt (default false).
        let json = r#"{
            "hook_event_name": "PostToolUseFailure",
            "session_id": "019f5738-76b1-74c3",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-76b1-74c3\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "tool_name": "run_terminal_command",
            "tool_input": {"command": "exit 1"},
            "error": "Command failed with exit code 1",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUseFailure(p) => {
                assert_eq!(p.session_id, "019f5738-76b1-74c3");
                assert_eq!(p.tool_name, "run_terminal_command");
                assert_eq!(p.error.as_deref(), Some("Command failed with exit code 1"));
                assert!(!p.is_interrupt, "is_interrupt défaulté false (jamais positionné par Grok)");
                assert_eq!(p.source_tool, SourceTool::Grok);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_post_tool_use_failure_sans_error_deserialise() {
        // Story 6.37, AC2 : toolResult absent → error omis à la traduction → désérialisation avec
        // error=None.
        let json = r#"{
            "hook_event_name": "PostToolUseFailure",
            "session_id": "019f5738",
            "transcript_path": "/t",
            "cwd": "C:\\x",
            "tool_name": "write",
            "tool_input": {"path": "/tmp"},
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUseFailure(p) => {
                assert_eq!(p.tool_name, "write");
                assert!(p.error.is_none(), "error absent → None (Option)");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn antigravity_session_start_synthetique_porte_model() {
        // SessionStart synthétisé par le binaire (seul porteur de modelName).
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "269a0c26",
            "transcript_path": "/t.jsonl",
            "cwd": "C:/Users/Dev",
            "model": "gemini-3.5-flash-low",
            "source_tool": "antigravity"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => {
                assert_eq!(p.model.as_deref(), Some("gemini-3.5-flash-low"));
                assert_eq!(p.source_tool, SourceTool::Antigravity);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_absent_default_unknown() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Unknown),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_inconnu_default_unknown() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "outil_du_futur"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Unknown),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn champs_optionnels_absents_no_panic() {
        let json = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "tool_name": "Bash",
            "tool_input": {"command": "codex build"},
            "source_tool": "codex"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PreToolUse(p) => {
                assert_eq!(p.source_tool, SourceTool::Codex);
                assert_eq!(p.parent_pid, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_subagent_stop() {
        let json = r#"{
            "hook_event_name": "SubagentStop",
            "session_id": "s1",
            "transcript_path": "/parent.jsonl",
            "cwd": "/project",
            "agent_id": "agent-uuid-1",
            "agent_type": "Explore",
            "agent_transcript_path": "/sub.jsonl",
            "stop_hook_active": false
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SubagentStop(p) => {
                assert_eq!(p.agent_id, "agent-uuid-1");
                assert_eq!(p.agent_transcript_path.as_deref(), Some("/sub.jsonl"));
            }
            _ => panic!("wrong event type"),
        }
    }

    // ── Grok (story 6.27) : preuve que la sortie du traducteur (vigil-grok) désérialise dans la
    //    bonne variante HookEvent. Payloads = forme NORMALISÉE (snake_case, hook_event_name
    //    PascalCase injecté, source_tool="grok" ajouté par emit) telle qu'écrite sur le pipe.
    //    `transcript_path` = forme réelle Grok (backslash absolu + urlencode(cwd)).

    #[test]
    fn grok_pre_tool_use_normalise_deserialise() {
        let json = r#"{
            "hook_event_name": "PreToolUse",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "tool_name": "run_terminal_command",
            "tool_input": {"command": "dir"},
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PreToolUse(p) => {
                assert_eq!(p.tool_name, "run_terminal_command");
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.tool_input["command"], "dir");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_session_start_synthetique_deserialise() {
        // SessionStart synthétisé par le binaire (transcript_path reconstruit, pas de model en 6.27).
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => {
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert!(p.model.is_none(), "badge modèle Grok déféré 6.28 (AC5)");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_stop_normalise_deserialise() {
        let json = r#"{
            "hook_event_name": "Stop",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::Stop(p) => {
                assert_eq!(p.session_id, "019f5157-76b1-74c3-a8a4-7df953935f21");
                assert_eq!(p.source_tool, SourceTool::Grok);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Complément (review 6.27, defer #3) : couvrir les 3 events Grok restants (PostToolUse,
    // UserPromptSubmit, SessionEnd) via le VRAI désérialiseur, notamment le comportement des
    // champs optionnels `prompt`/`reason` ABSENTS (que `translate()` omet) → `None`.

    #[test]
    fn grok_post_tool_use_normalise_deserialise() {
        // `tool_response` = toolResult mappé (requis par PostToolUsePayload).
        let json = r#"{
            "hook_event_name": "PostToolUse",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "tool_name": "run_terminal_command",
            "tool_input": {"command": "dir"},
            "tool_response": {"output": "file1"},
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUse(p) => {
                assert_eq!(p.tool_name, "run_terminal_command");
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.tool_response["output"], "file1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_user_prompt_submit_normalise_deserialise() {
        // Avec `prompt` (Grok le porte) → Some.
        let avec = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "prompt": "liste le dossier",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(avec).unwrap() {
            HookEvent::UserPromptSubmit(p) => {
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.prompt.as_deref(), Some("liste le dossier"));
            }
            _ => panic!("wrong variant"),
        }
        // Sans `prompt` (translate l'omet quand le champ Grok est absent) → None, pas d'erreur.
        let sans = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(sans).unwrap() {
            HookEvent::UserPromptSubmit(p) => assert!(p.prompt.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_session_end_normalise_deserialise() {
        // Avec `reason` → Some.
        let avec = r#"{
            "hook_event_name": "SessionEnd",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-76b1-74c3-a8a4-7df953935f21\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "reason": "quit",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(avec).unwrap() {
            HookEvent::SessionEnd(p) => {
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.reason.as_deref(), Some("quit"));
            }
            _ => panic!("wrong variant"),
        }
        // Sans `reason` (translate l'omet quand le champ Grok est absent) → None, pas d'erreur.
        let sans = r#"{
            "hook_event_name": "SessionEnd",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(sans).unwrap() {
            HookEvent::SessionEnd(p) => assert!(p.reason.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    // ── Grok (story 6.30) : le `PermissionRequest` SYNTHÉTISÉ par vigil-grok depuis un
    //    `pre_tool_use` doit désérialiser dans HookEvent::PermissionRequest. Deux formes :
    //    (A) permission réelle → `tool_name` = nom NATIF Grok → WaitingApprove.
    //    (B) ask_user_question → `tool_name` = "AskUserQuestion" + questions normalisées → WaitingSpecific.

    #[test]
    fn grok_permission_request_reel_normalise_deserialise() {
        // Synthèse Approve : tool_name natif (run_terminal_command), tool_input opaque conservé.
        let json = r#"{
            "hook_event_name": "PermissionRequest",
            "session_id": "019f5157-76b1-74c3-a8a4-7df953935f21",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "tool_name": "run_terminal_command",
            "tool_input": {"command": "rm -rf dist/"},
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PermissionRequest(p) => {
                assert_eq!(p.tool_name, "run_terminal_command");
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.tool_input["command"], "rm -rf dist/");
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── Grok (story 6.31) : sous-agents. Le JSON produit par vigil-grok::translate (parent-side :
    //    session_id/transcript_path = PARENT, agent_id = subagentId = session enfant) désérialise
    //    dans HookEvent::SubagentStart / SubagentStop internes.

    #[test]
    fn grok_subagent_start_normalise_deserialise() {
        let json = r#"{
            "hook_event_name": "SubagentStart",
            "session_id": "019f5157-parent",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-parent\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "agent_id": "019f9999-child",
            "agent_type": "Explore",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SubagentStart(p) => {
                assert_eq!(p.agent_id, "019f9999-child"); // = subagentId = session enfant
                assert_eq!(p.agent_type, "Explore");
                assert_eq!(p.source_tool, SourceTool::Grok);
                // transcript_path = parent → find_parent_key par clé directe (comme Claude).
                assert!(p.transcript_path.ends_with("019f5157-parent\\updates.jsonl"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_subagent_end_normalise_deserialise() {
        // `subagent_end` Grok → variante interne SubagentStop ; agent_type peut être "" (absent côté
        // Grok) ; agent_transcript_path/stop_hook_active absents → défauts (serde default).
        let json = r#"{
            "hook_event_name": "SubagentStop",
            "session_id": "019f5157-parent",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5157-parent\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "agent_id": "019f9999-child",
            "agent_type": "",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SubagentStop(p) => {
                assert_eq!(p.agent_id, "019f9999-child");
                assert_eq!(p.agent_type, "");
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert!(p.agent_transcript_path.is_none());
                assert!(!p.stop_hook_active);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_ask_user_question_normalise_deserialise() {
        // Synthèse Question : tool_name="AskUserQuestion" (→ WaitingSpecific), questions déjà
        // normalisées par vigil-grok (multiSelect camelCase, header omis).
        let json = r#"{
            "hook_event_name": "PermissionRequest",
            "session_id": "019f5157",
            "transcript_path": "/t",
            "cwd": "C:\\Users\\Dev",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {"question": "Quelle branche ?", "options": [{"label": "main"}], "multiSelect": false}
            ]},
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PermissionRequest(p) => {
                assert_eq!(p.tool_name, "AskUserQuestion");
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.tool_input["questions"][0]["multiSelect"], false);
                assert_eq!(p.tool_input["questions"][0]["options"][0]["label"], "main");
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── Story 6.34 : État & erreurs Grok (Notification / StopFailure) ─────────
    // Le JSON reflète EXACTEMENT ce que `vigil-grok::translate()` produit (message REQUIS défaut "",
    // notification_type/error_type Options mappés depuis notificationType/error), après injection du
    // `source_tool: grok` par `emit`. Structs `NotificationPayload`/`StopFailurePayload` NON modifiées.

    #[test]
    fn grok_notification_normalise_deserialise() {
        // Fixture = payload réel `events-run2.jsonl` (l.6 : agent_error 429, message complet).
        let json = r#"{
            "hook_event_name": "Notification",
            "session_id": "019f5738-child",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-child\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "message": "Rate limited: API error (status 429 Too Many Requests)",
            "notification_type": "agent_error",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::Notification(p) => {
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert!(!p.message.is_empty());
                assert_eq!(p.notification_type.as_deref(), Some("agent_error"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn grok_stop_failure_normalise_deserialise() {
        // Fixture = payload réel `events-run2.jsonl` (l.7 : error = texte complet 429).
        let json = r#"{
            "hook_event_name": "StopFailure",
            "session_id": "019f5738-child",
            "transcript_path": "C:\\Users\\Dev\\.grok\\sessions\\C%3A%5CUsers%5CDev\\019f5738-child\\updates.jsonl",
            "cwd": "C:\\Users\\Dev",
            "error_type": "API error status 429 rate-limit",
            "source_tool": "grok"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::StopFailure(p) => {
                assert_eq!(p.source_tool, SourceTool::Grok);
                assert_eq!(p.error_type.as_deref(), Some("API error status 429 rate-limit"));
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── Kimi (story 9.1) : preuve que la sortie du traducteur (`vigil-kimi::translate`) désérialise
    //    dans la bonne variante `HookEvent`, **sans aucun nouveau variant ni champ**. Les JSON
    //    ci-dessous reproduisent la forme produite par `translate()`, plus le `source_tool:"kimi"`
    //    qu'`emit()` injecte. C'est la seconde moitié du contrat : la crate binaire ne peut pas
    //    importer `HookEvent`, elle asserte sur les champs JSON produits.
    //
    //    ⚠️ **Limite assumée, relevée en revue (2026-07-23)** : ces fixtures sont écrites À LA MAIN,
    //    pas capturées à l'exécution — rien ne les couple mécaniquement à `translate()`. Une
    //    modification du traducteur peut donc laisser les deux moitiés vertes pendant que les events
    //    cessent de se désérialiser en production. Même limite structurelle que pour Grok et
    //    Antigravity ; le fix réel (vecteurs de traduction partagés entre les deux suites) est porté
    //    en `deferred-work.md`. Famille de piège de `transcript_hash` — à traiter, pas à oublier.
    //
    //    `transcript_path` = forme réelle Kimi : **slashs avant**, résolue via `session_index.jsonl`,
    //    `agents/main/wire.jsonl` — jamais normalisée en backslash (clé de dédup + vrai chemin que la
    //    Story 9.4 devra retrouver à l'identique).

    #[test]
    fn kimi_session_start_normalise_deserialise() {
        let json = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "SessionStart",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "source": "startup",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => {
                // ⚠️ Le vrai piège de la story : sans le variant `SourceTool::Kimi`, cette assertion
                // rendrait `Unknown` via `#[serde(other)]` — panne SILENCIEUSE (ni badge, ni label).
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert!(p.transcript_path.ends_with("/agents/main/wire.jsonl"));
                assert!(!p.transcript_path.contains('\\'), "slashs avant préservés");
                assert!(p.model.is_none(), "le payload Kimi ne porte aucun modèle");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_pre_tool_use_normalise_deserialise() {
        // `path` → alias ADDITIF `file_path` (les deux clés coexistent).
        let json = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "PreToolUse",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "tool_call_id": "vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo",
            "tool_input": {"content": "", "file_path": "C:/Users/Dev/test-kimi2.txt", "path": "C:/Users/Dev/test-kimi2.txt"},
            "tool_name": "Write",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PreToolUse(p) => {
                assert_eq!(p.tool_name, "Write");
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.tool_input["file_path"], "C:/Users/Dev/test-kimi2.txt");
                // `tool_call_id` est un EXTRA Kimi, non déclaré par `PreToolUsePayload` : la réussite
                // du `from_str` ci-dessus EST la preuve qu'il est ignoré par serde sans faire échouer
                // la désérialisation. (Il n'est accessible nulle part sur `p` — il n'y a donc rien
                // d'autre à asserter à son sujet.)
                //
                // `parent_pid` absent de cette fixture → `None`. En production `emit()` l'injecte
                // via `inject_parent_pid` ; ce que ce test verrouille, c'est que son ABSENCE ne
                // casse pas la désérialisation (champ `Option`, hook pas encore réinstallé).
                assert_eq!(p.parent_pid, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_permission_request_read_only_normalise_deserialise() {
        // Mode `manual`/`auto` → `read_only: true` : `is_actionable` rend faux (notif mobile non
        // actionnable) et l'auto-permission est skippée. Chemins GÉNÉRIQUES, aucun concept neuf.
        let json = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "PermissionRequest",
            "read_only": true,
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "tool_call_id": "vKpfA2zqxwCvci1sPMkgdo7IurWVTaYo",
            "tool_input": {"content": "", "file_path": "C:/Users/Dev/test-kimi2.txt", "path": "C:/Users/Dev/test-kimi2.txt"},
            "tool_name": "Write",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PermissionRequest(p) => {
                assert!(p.read_only, "manual/auto → awareness seule");
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.tool_name, "Write");
            }
            _ => panic!("wrong variant"),
        }
        // Mode `yolo` → actionnable (le hook est le seul portier).
        let yolo = json.replace("\"read_only\": true", "\"read_only\": false");
        match serde_json::from_str::<HookEvent>(&yolo).unwrap() {
            HookEvent::PermissionRequest(p) => assert!(!p.read_only),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_post_tool_use_normalise_deserialise() {
        // ⚠️ `tool_response` (dérivé de `tool_output`) est REQUIS : l'omettre ferait échouer la
        // désérialisation EN SILENCE → aucun event ne remonterait.
        let json = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "PostToolUse",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "tool_input": {"content": "", "file_path": "C:/Users/Dev/test-kimi2.txt", "path": "C:/Users/Dev/test-kimi2.txt"},
            "tool_name": "Write",
            "tool_response": "Wrote 0 bytes to C:/Users/Dev/test-kimi2.txt",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::PostToolUse(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.tool_response, "Wrote 0 bytes to C:/Users/Dev/test-kimi2.txt");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_user_prompt_submit_normalise_deserialise() {
        // Le tableau de blocs Kimi a été APLATI en chaîne par `translate` (le payload attend
        // `Option<String>`, un tableau ferait échouer la désérialisation).
        let json = r#"{
            "cwd": "C:/Users/Dev/kimi-spike-sandbox",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Lance exactement sleep 30 en Bash.",
            "session_id": "session_050b4673-95db-43cb-ae9b-f71d3284b316",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_kimi-spike-sandbox_5bd81c3786d4/session_050b4673-95db-43cb-ae9b-f71d3284b316/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::UserPromptSubmit(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.prompt.as_deref(), Some("Lance exactement sleep 30 en Bash."));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_subagent_start_stop_normalise_deserialise() {
        // `agent_name` (un TYPE de sous-agent, pas un id unique — limite assumée de la 9.1) alimente
        // à la fois `agent_id` et `agent_type`. `transcript_path` = celui du PARENT : chez Kimi tous
        // les events portent le `session_id` du parent → AUCUNE garde anti-fantôme façon Codex.
        let start = r#"{
            "agent_id": "coder",
            "agent_type": "coder",
            "cwd": "C:/Users/Dev/kimi-spike-sandbox",
            "hook_event_name": "SubagentStart",
            "session_id": "session_050b4673-95db-43cb-ae9b-f71d3284b316",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_kimi-spike-sandbox_5bd81c3786d4/session_050b4673-95db-43cb-ae9b-f71d3284b316/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(start).unwrap() {
            HookEvent::SubagentStart(p) => {
                assert_eq!(p.agent_id, "coder");
                assert_eq!(p.agent_type, "coder");
            }
            _ => panic!("wrong variant"),
        }
        let stop = start.replace("SubagentStart", "SubagentStop");
        match serde_json::from_str::<HookEvent>(&stop).unwrap() {
            HookEvent::SubagentStop(p) => {
                assert_eq!(p.agent_id, "coder");
                assert!(p.agent_transcript_path.is_none(), "Kimi n'expose aucun transcript d'enfant");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_stop_et_session_end_normalise_deserialise() {
        let stop = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "Stop",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(stop).unwrap() {
            HookEvent::Stop(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert!(!p.interrupted, "aucun Stop synthétique côté Kimi (hors INTERRUPT_SUPPORTED)");
            }
            _ => panic!("wrong variant"),
        }
        // `SessionEnd` NATIF (`reason:"exit"`, confirmé au spike) → le `pid_watcher` n'est qu'un filet.
        let end = r#"{
            "cwd": "C:/Users/Dev/kimi-spike-sandbox",
            "hook_event_name": "SessionEnd",
            "reason": "exit",
            "session_id": "session_050b4673-95db-43cb-ae9b-f71d3284b316",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_kimi-spike-sandbox_5bd81c3786d4/session_050b4673-95db-43cb-ae9b-f71d3284b316/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(end).unwrap() {
            HookEvent::SessionEnd(p) => assert_eq!(p.reason.as_deref(), Some("exit")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kimi_events_jamais_observes_normalisent_et_deserialisent() {
        // ⚠️ Ajouté en revue (2026-07-23). `PostToolUseFailure`, `StopFailure` et `Notification` sont
        // les TROIS events que le spike n'a JAMAIS déclenchés (le modèle anticipait les erreurs) :
        // leur traduction est écrite sur la foi de la doc Moonshot seule. Ce sont donc, de tous les
        // events, ceux dont la désérialisation avait le plus besoin d'être prouvée — et ils étaient
        // les seuls à ne pas l'être. Le mapping asymétrique (`error` → `error_type` sur
        // `StopFailure`) ne tient à rien d'autre qu'à ce test.
        let failure = r#"{
            "cwd": "C:/Users/Dev",
            "error": "Access denied",
            "hook_event_name": "PostToolUseFailure",
            "tool_input": {"command": "boom"},
            "tool_name": "Bash",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(failure).unwrap() {
            HookEvent::PostToolUseFailure(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.error.as_deref(), Some("Access denied"));
                assert!(!p.is_interrupt, "Kimi est hors INTERRUPT_SUPPORTED : jamais d'interruption");
            }
            _ => panic!("wrong variant"),
        }

        // `error` (Kimi) → `error_type` (Vigil) : mapping ASYMÉTRIQUE, seul endroit qui le prouve.
        let stop_failure = r#"{
            "cwd": "C:/Users/Dev",
            "error_type": "API error 429",
            "hook_event_name": "StopFailure",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(stop_failure).unwrap() {
            HookEvent::StopFailure(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.error_type.as_deref(), Some("API error 429"));
            }
            _ => panic!("wrong variant"),
        }

        // ⚠️ `message` est REQUIS (`String`, pas d'`Option`) : `translate` le défaute à "" plutôt que
        // de l'omettre. L'omettre ferait échouer la désérialisation EN SILENCE → aucun event.
        // Sémantique Moonshot (« background task status changes ») ≠ Claude : le handler socle reste
        // purement annotatif, on ne câble rien de plus.
        let notification = r#"{
            "cwd": "C:/Users/Dev",
            "hook_event_name": "Notification",
            "message": "",
            "session_id": "session_259a872b-eeb2-4a17-a27e-d89dd17fb102",
            "transcript_path": "C:/Users/Dev/.kimi-code/sessions/wd_dev_cfa108596ad6/session_259a872b-eeb2-4a17-a27e-d89dd17fb102/agents/main/wire.jsonl",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(notification).unwrap() {
            HookEvent::Notification(p) => {
                assert_eq!(p.source_tool, SourceTool::Kimi);
                assert_eq!(p.message, "");
                assert!(p.notification_type.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn source_tool_kimi_deserialise_correctement() {
        let json = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "transcript_path": "/t",
            "cwd": "/c",
            "source_tool": "kimi"
        }"#;
        match serde_json::from_str::<HookEvent>(json).unwrap() {
            HookEvent::SessionStart(p) => assert_eq!(p.source_tool, SourceTool::Kimi),
            _ => panic!("wrong variant"),
        }
    }
}
