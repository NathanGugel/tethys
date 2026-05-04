import type { ChecksRollup, RepoLink, Workspace } from "./types";

/** Five minutes — matches the poller's stale threshold. */
const STALE_MS = 5 * 60 * 1000;

export function isStale(fetchedAt: string, nowMs: number = Date.now()): boolean {
  const t = new Date(fetchedAt).getTime();
  if (Number.isNaN(t)) return false;
  return nowMs - t > STALE_MS;
}

/**
 * True when every GitHub-linked repo in the workspace has a merged PR.
 * Non-GitHub repos (no `github` field) are ignored — they don't block
 * deletion. A workspace with no GitHub-linked repos at all returns false
 * so we don't suggest deleting an unsynced workspace.
 */
export function isReadyToDelete(ws: Workspace): boolean {
  const linked = ws.repo_links.filter((r) => r.github !== null);
  if (linked.length === 0) return false;
  return linked.every((r) => r.github!.state === "merged");
}

/**
 * Worst-case rollup across all open PRs in the workspace.
 * Failure > Pending > Success. Neutral/None are ignored.
 */
export function checksSummary(ws: Workspace): ChecksRollup | null {
  let worst: ChecksRollup | null = null;
  for (const r of ws.repo_links) {
    if (!r.github || r.github.state !== "open") continue;
    if (r.github.has_merge_conflicts) return "failure";
    const c = r.github.checks;
    if (c === "failure") return "failure";
    if (c === "pending") worst = "pending";
    else if (c === "success" && worst === null) worst = "success";
  }
  return worst;
}

/** Sum of unresolved review threads across all open PRs. */
export function unresolvedTotal(ws: Workspace): number {
  let sum = 0;
  for (const r of ws.repo_links) {
    if (r.github && r.github.state === "open") sum += r.github.unresolved_threads;
  }
  return sum;
}

/** Find the primary PR chip to show on a workspace row — the first open PR, else first. */
export function primaryRepoLink(ws: Workspace): RepoLink | null {
  const open = ws.repo_links.find((r) => r.github?.state === "open");
  if (open) return open;
  return ws.repo_links.find((r) => r.github !== null) ?? null;
}
