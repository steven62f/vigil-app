export interface DailyStats {
  date: string;
  inputTokens: number;
  outputTokens: number;
  cacheTokens: number;
  costUsd: number;
  sessionCount: number;
}

export interface LocalStats {
  days: DailyStats[];
  totalCostUsd: number;
  totalTokens: number;
}

export interface AgentStats {
  inputTokens: number;
  outputTokens: number;
  cacheTokens: number;
  costUsd: number;
  durationDays: number | null;
}
