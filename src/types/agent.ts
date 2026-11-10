// Mirror exact des structs Rust (Story 1.2)

export interface AskUserOption {
  label: string;
  description?: string;
}

export interface AskUserQuestion {
  question: string;
  header?: string;
  options: AskUserOption[];
  multiSelect: boolean;
}

export interface PermissionRequestInfo {
  toolName: string;
  toolInput: {
    command?: string;
    file_path?: string;
    questions?: AskUserQuestion[];
    [key: string]: unknown;
  };
  // Story 6.30 (Grok) — true = affichage lecture-seule (permission Grok en mode ask : pas de
  // boutons, invite à répondre dans le terminal). Absent/false = overlay actionnable (approve/deny).
  readOnly?: boolean;
}

export type SubagentState = "working" | "ended";

export interface SubagentSnapshot {
  agentId: string;
  agentType: string;
  state: SubagentState;
}

export type AgentState =
  | "idle"
  | "working"
  | "waitingApprove"
  | "waitingSpecific"
  | "ended"
  | "error"
  | "disconnected"
  | "unknown";

/** Libellé convivial de l'hôte d'exécution, déduit par le hook depuis l'arbre de process
 *  (« VS Code », « Windows Terminal », « PowerShell », « Cursor »…, ou nom nettoyé si inconnu).
 *  Chaîne libre (générique) — plus d'enum fermé. */
export type TerminalSource = string;

/** Miroir TS de `SourceTool` (Rust, `agent/state.rs`). Union FERMÉE dont `StatsTool` dérive
 *  (`Exclude<SourceTool, "unknown">`) : ajouter un outil ici fait CASCADER sur `toolLabel`
 *  (switch exhaustif) et sur les 4 listes de capacités de `statsTools.ts`, qui doivent alors être
 *  tranchées EXPLICITEMENT — y compris celles qui n'accueillent pas le nouvel outil. */
export type SourceTool = "claude_code" | "codex" | "grok" | "antigravity" | "kimi" | "unknown";

export interface AgentMessage {
  role: "user" | "assistant" | "tool";
  content: string;
  timestamp: number;
}

export interface AgentSnapshot {
  transcriptPath: string;
  sessionId: string | null;
  name: string;
  state: AgentState;
  waitingSince: number | null;
  lastActivity: number;
  lastActivityLabel: string | null;
  terminalPid: number | null;
  source: TerminalSource;
  permissionRequest?: PermissionRequestInfo;
  interruptPending?: boolean;
  model?: string;
  cwd?: string;
  subagents?: SubagentSnapshot[];
  customName?: string | null;
  description?: string | null;
  tags?: string[];
  pinned?: boolean;
  sourceTool: SourceTool;
}

export interface SessionMetadata {
  customName?: string | null;
  description?: string | null;
  tags: string[];
  pinned: boolean;
}

export interface SessionSummary {
  sessionId: string | null;
  transcriptPath: string;
  cwd: string | null;
  startedAt: number;
  lastActiveAt: number;
  model: string | null;
  firstUserMessage: string | null;
  customName?: string | null;
  description?: string | null;
  tags?: string[];
  pinned?: boolean;
}

export interface ProjectGroup {
  projectName: string;
  projectPath: string;
  lastActivity: number;
  sessionCount: number;
  sessions: SessionSummary[];
}
