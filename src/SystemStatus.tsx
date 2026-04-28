import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import type { SystemErrorEntry, Workspace, WorkspaceId } from "./types";
import { useTauriEvent } from "./useTauriEvent";

type Props = {
  /** All workspaces, including soft-deleted. The modal lists pending deletions. */
  allWorkspaces: Workspace[];
};

const HOUR_MS = 60 * 60 * 1000;

export function SystemStatus({ allWorkspaces }: Props) {
  const [errors, setErrors] = useState<SystemErrorEntry[]>([]);
  const [open, setOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<SystemErrorEntry[]>("list_system_errors");
      setErrors(list);
    } catch (e) {
      console.error("list_system_errors:", e);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useTauriEvent("system_status:changed", () => refresh());

  const pendingDeletes = allWorkspaces.filter((w) => w.deleted_at !== null);
  const hasErrors = errors.length > 0;
  const hasNotices = hasErrors || pendingDeletes.length > 0;

  return (
    <>
      <button
        type="button"
        className={`system-status-button${hasErrors ? " has-errors" : ""}`}
        onClick={() => setOpen(true)}
        title="System status"
      >
        <span
          className={`status-dot ${
            hasErrors ? "red" : hasNotices ? "yellow" : "green"
          }`}
        />
        Status
      </button>
      {open && (
        <SystemStatusModal
          errors={errors}
          pendingDeletes={pendingDeletes}
          onClose={() => setOpen(false)}
          onRefresh={refresh}
        />
      )}
    </>
  );
}

function SystemStatusModal({
  errors,
  pendingDeletes,
  onClose,
  onRefresh,
}: {
  errors: SystemErrorEntry[];
  pendingDeletes: Workspace[];
  onClose: () => void;
  onRefresh: () => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);

  const cancelDelete = async (id: WorkspaceId) => {
    setBusy(`cancel:${id}`);
    try {
      await invoke("cancel_delete_workspace", { id });
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(null);
    }
  };

  const dismissError = async (id: string) => {
    setBusy(`err:${id}`);
    try {
      await invoke("dismiss_system_error", { id });
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(null);
    }
  };

  const runCleanupNow = async () => {
    setBusy("cleanup");
    try {
      await invoke("run_purge_now");
      // The cron will emit `system_status:changed` once it's done; in the
      // meantime there's no progress to report, so just close busy.
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy(null);
      onRefresh();
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal system-status-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <h3>System status</h3>

        <section>
          <div className="section-header">
            <h4>Pending deletions</h4>
            <button type="button" onClick={runCleanupNow} disabled={busy !== null}>
              Run cleanup now
            </button>
          </div>
          {pendingDeletes.length === 0 ? (
            <p className="muted">Nothing waiting to be cleaned up.</p>
          ) : (
            <ul className="status-list">
              {pendingDeletes.map((w) => {
                const deletedAt = w.deleted_at
                  ? new Date(w.deleted_at).getTime()
                  : 0;
                const ageMs = Date.now() - deletedAt;
                const eligible = ageMs >= HOUR_MS;
                const label = eligible
                  ? "Ready to purge on next tick"
                  : `Will purge after ${formatRemaining(HOUR_MS - ageMs)}`;
                return (
                  <li key={w.id}>
                    <div className="status-row">
                      <code>{w.branch}</code>
                      <span className="muted">{label}</span>
                    </div>
                    <button
                      type="button"
                      onClick={() => cancelDelete(w.id)}
                      disabled={busy === `cancel:${w.id}`}
                    >
                      Cancel deletion
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <section>
          <h4>Errors</h4>
          {errors.length === 0 ? (
            <p className="muted">No errors recorded.</p>
          ) : (
            <ul className="status-list">
              {errors
                .slice()
                .reverse()
                .map((err) => (
                  <li key={err.id} className="error-entry">
                    <div className="status-row">
                      <span className="error-when">
                        {new Date(err.at).toLocaleString()}
                      </span>
                      {err.workspace_branch && (
                        <code>{err.workspace_branch}</code>
                      )}
                      <span className="error-kind">{err.kind}</span>
                    </div>
                    <pre className="error-message">{err.message}</pre>
                    <button
                      type="button"
                      onClick={() => dismissError(err.id)}
                      disabled={busy === `err:${err.id}`}
                    >
                      Dismiss
                    </button>
                  </li>
                ))}
            </ul>
          )}
        </section>

        <div className="modal-actions">
          <button type="button" onClick={onClose} autoFocus>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

function formatRemaining(ms: number): string {
  if (ms <= 0) return "now";
  const minutes = Math.ceil(ms / 60000);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const rem = minutes % 60;
  return rem === 0 ? `${hours}h` : `${hours}h ${rem}m`;
}
