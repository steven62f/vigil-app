export function projectNameFromCwd(cwd: string | null | undefined, fallback: string): string {
  if (!cwd) return fallback;
  const normalized = cwd.replace(/[\\/]+$/, "");
  if (!normalized) return fallback;
  const segments = normalized.split(/[\\/]/);
  const last = segments[segments.length - 1]?.trim();
  return last || fallback;
}
