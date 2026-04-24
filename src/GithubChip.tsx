import { openUrl } from "@tauri-apps/plugin-opener";
import type { GithubPrStatus } from "./types";
import { isStale } from "./workspaceDerived";

function CheckIcon() {
  return (
    <svg
      className="gh-icon gh-icon-check"
      viewBox="0 0 16 16"
      aria-label="checks passing"
    >
      <path
        d="M3 8.5 L6.5 12 L13 4"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function XIcon() {
  return (
    <svg
      className="gh-icon gh-icon-x"
      viewBox="0 0 16 16"
      aria-label="checks failing"
    >
      <path
        d="M4 4 L12 12 M12 4 L4 12"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
      />
    </svg>
  );
}

function PersonIcon() {
  return (
    <svg
      className="gh-icon gh-icon-person"
      viewBox="0 0 16 16"
      aria-label="approved"
    >
      <circle cx="8" cy="5.5" r="2.8" fill="currentColor" />
      <path d="M2.5 14.5 C3 10.5 13 10.5 13.5 14.5 Z" fill="currentColor" />
    </svg>
  );
}

export function GithubChip({
  status,
  linkable = true,
}: {
  status: GithubPrStatus;
  /** When false, the chip is informational only — no click-to-open, no hover. */
  linkable?: boolean;
}) {
  const stale = isStale(status.fetched_at);
  const showCheck = status.state === "open" && status.checks === "success";
  const showX = status.state === "open" && status.checks === "failure";
  const showApproved =
    status.state === "open" && status.review_decision === "approved";

  const title = [
    `PR #${status.pr_number}`,
    `state: ${status.state}${status.is_draft ? " (draft)" : ""}`,
    status.state === "open" ? `checks: ${status.checks}` : null,
    status.state === "open" ? `review: ${status.review_decision}` : null,
    status.unresolved_threads > 0
      ? `${status.unresolved_threads} unresolved`
      : null,
    status.last_error ? `error: ${status.last_error}` : null,
    stale ? `stale since ${new Date(status.fetched_at).toLocaleTimeString()}` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  const onClick = linkable
    ? (e: React.MouseEvent) => {
        e.stopPropagation();
        openUrl(status.url).catch(() => {
          /* non-fatal */
        });
      }
    : undefined;

  const classes = [
    "gh-chip",
    `gh-state-${status.state}`,
    stale ? "gh-stale" : "",
    status.is_draft ? "gh-draft" : "",
    linkable ? "" : "gh-chip-static",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <span className={classes} title={title} onClick={onClick}>
      <span className="gh-pr">#{status.pr_number}</span>
      {showCheck && <CheckIcon />}
      {showX && <XIcon />}
      {showApproved && <PersonIcon />}
      {status.state === "merged" && <span className="gh-merged-badge">merged</span>}
      {status.state === "closed" && <span className="gh-merged-badge gh-closed-badge">closed</span>}
      {status.is_draft && <span className="gh-draft-badge">draft</span>}
      {status.unresolved_threads > 0 && status.state === "open" && (
        <span className="gh-unresolved" aria-label={`${status.unresolved_threads} unresolved`}>
          {status.unresolved_threads}
        </span>
      )}
    </span>
  );
}
