export const errorMessages: Record<string, string> = {
  focus_terminal_not_found: "Terminal not found — session closed",
  pipe_unavailable: "Connection to agent lost",
  approve_failed: "Failed to approve — try again",
  deny_failed: "Failed to deny — try again",
  send_message_failed: "Message not sent — try again",
  interrupt_failed: "Failed to interrupt — try again",
  // Filet anti-désynchro TS↔Rust (6.33) : normalement inatteignable, le bouton étant gaté sur
  // INTERRUPT_SUPPORTED. S'il s'affiche, c'est que l'allowlist TS et `flags::interrupt_supported`
  // (Rust) ont divergé → l'outil est à câbler côté bridge, ou à retirer de l'allowlist TS.
  interrupt_unsupported: "Interrupt isn't supported for this tool",
  hook_install_failed: "Failed to install hooks — check your permissions",
  hook_settings_locked: "Config file is locked — close the tool and try again",
  hook_settings_unreadable: "Cannot read config file — check that the file exists",
  hook_tool_unknown: "Unknown tool — operation not supported",
};
