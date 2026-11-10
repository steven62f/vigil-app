export interface ToolsStatus {
  claudeCodeDetected: boolean;
  codexDetected: boolean;
  grokDetected: boolean;
  antigravityDetected: boolean;
  kimiDetected: boolean;
  claudeCodeEnabled: boolean;
  codexEnabled: boolean;
  grokEnabled: boolean;
  antigravityEnabled: boolean;
  kimiEnabled: boolean;
  claudeCodeHooksOk: boolean;
  codexHooksOk: boolean;
  grokHooksOk: boolean;
  antigravityHooksOk: boolean;
  kimiHooksOk: boolean;
}

export interface ToolHookDiff {
  tool: string;
  configFilePath: string;
  before: string;
  after: string;
  hookExePath: string;
}
