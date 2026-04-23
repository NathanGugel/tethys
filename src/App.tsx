import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type {
  CreateWorkspaceArgs,
  Discrepancies,
  RegistryStatus,
  Repo,
  Workspace,
  WorkspaceId,
} from "./types";
import { JobLogModal } from "./JobLogModal";
import "./App.css";

type RunningJob = {
  title: string;
  command: string;
  args: Record<string, unknown>;
  onSuccess?: (result: unknown) => void;
};

function App() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [registry, setRegistry] = useState<RegistryStatus | null>(null);
  const [discrepancies, setDiscrepancies] = useState<Discrepancies | null>(null);
  const [selectedId, setSelectedId] = useState<WorkspaceId | null>(null);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [runningJob, setRunningJob] = useState<RunningJob | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [list, reg, disc] = await Promise.all([
        invoke<Workspace[]>("list_workspaces"),
        invoke<RegistryStatus>("registry_status"),
        invoke<Discrepancies>("list_discrepancies"),
      ]);
      setWorkspaces(list);
      setRegistry(reg);
      setDiscrepancies(disc);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    refresh();
    const unlistenPromise = listen("workspace:changed", () => refresh());
    return () => {
      unlistenPromise.then((un) => un());
    };
  }, [refresh]);

  const selected = useMemo(
    () => workspaces.find((w) => w.id === selectedId) ?? null,
    [workspaces, selectedId],
  );

  const registryOk = registry?.kind === "ok";

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-header">
          <h1>Tethys</h1>
          <button
            className="primary"
            onClick={() => setCreating(true)}
            type="button"
            disabled={!registryOk}
            title={registryOk ? undefined : "Configure repos.toml first"}
          >
            New workspace
          </button>
        </div>
        <ul className="workspace-list">
          {workspaces.length === 0 && (
            <li className="empty">No workspaces yet.</li>
          )}
          {workspaces.map((w) => (
            <li
              key={w.id}
              className={w.id === selectedId ? "selected" : ""}
              onClick={() => setSelectedId(w.id)}
            >
              <div className="workspace-name">
                {w.branch}
                {w.paused && <span className="paused-badge">paused</span>}
              </div>
              {w.repo_links.length > 0 && (
                <div className="workspace-meta">
                  {w.repo_links.length}{" "}
                  {w.repo_links.length === 1 ? "repo" : "repos"}
                </div>
              )}
            </li>
          ))}
        </ul>
      </aside>

      <main className="detail">
        {error && <div className="error-banner">{error}</div>}
        {registry && !registryOk && (
          <RegistryNotice registry={registry} onChanged={refresh} />
        )}
        {discrepancies &&
          (discrepancies.orphaned_dirs.length > 0 ||
            discrepancies.missing_worktrees.length > 0) && (
            <DiscrepancyNotice
              discrepancies={discrepancies}
              onChanged={refresh}
            />
          )}
        {selected ? (
          <WorkspaceDetail
            workspace={selected}
            onDeleted={() => setSelectedId(null)}
            onRun={setRunningJob}
          />
        ) : (
          registryOk && (
            <div className="placeholder">
              Select a workspace, or create one to get started.
            </div>
          )
        )}
      </main>

      {creating && registry?.kind === "ok" && (
        <CreateWorkspaceDialog
          repos={registry.registry.repos}
          onClose={() => setCreating(false)}
          onSubmit={(args) => {
            setCreating(false);
            setRunningJob({
              title: `Creating ${args.branch}`,
              command: "create_workspace",
              args: { args },
              onSuccess: (result) => {
                const ws = result as Workspace;
                setSelectedId(ws.id);
              },
            });
          }}
        />
      )}

      {runningJob && (
        <JobLogModal
          title={runningJob.title}
          command={runningJob.command}
          args={runningJob.args}
          onClose={() => setRunningJob(null)}
          onSuccess={runningJob.onSuccess}
        />
      )}
    </div>
  );
}

function RegistryNotice({
  registry,
  onChanged,
}: {
  registry: RegistryStatus;
  onChanged: () => void;
}) {
  const openConfig = async () => {
    try {
      await invoke("open_repos_config");
    } catch (e) {
      alert(String(e));
    }
  };

  if (registry.kind === "ok") return null;

  return (
    <div className="registry-notice">
      <h2>Repos not configured</h2>
      {registry.kind === "missing" ? (
        <p>
          Tethys expects a repo registry at <code>{registry.path}</code>. It
          doesn't exist yet.
        </p>
      ) : (
        <>
          <p>
            Tethys couldn't load <code>{registry.path}</code>:
          </p>
          <pre>{registry.error}</pre>
        </>
      )}
      <p>
        Click the button below to open it in your default editor. Fill in{" "}
        <code>worktree_root</code> and at least one <code>[[repo]]</code>, then{" "}
        <strong>restart Tethys</strong> — registry changes take effect at
        launch.
      </p>
      <div className="actions">
        <button className="primary" type="button" onClick={openConfig}>
          Open repos.toml
        </button>
        <button type="button" onClick={onChanged}>
          Re-check
        </button>
      </div>
    </div>
  );
}

function DiscrepancyNotice({
  discrepancies,
  onChanged,
}: {
  discrepancies: Discrepancies;
  onChanged: () => void;
}) {
  const removeOrphan = async (path: string) => {
    try {
      await invoke("remove_orphan_dir", { path });
      onChanged();
    } catch (e) {
      alert(String(e));
    }
  };

  const forgetWorkspace = async (id: string) => {
    try {
      await invoke("forget_workspace", { id });
      onChanged();
    } catch (e) {
      alert(String(e));
    }
  };

  // Collapse missing-worktree rows by workspace_id — multiple repos per
  // workspace would otherwise show redundant Forget buttons.
  const missingByWorkspace = new Map<
    string,
    { branch: string; repos: string[] }
  >();
  for (const m of discrepancies.missing_worktrees) {
    const entry = missingByWorkspace.get(m.workspace_id) ?? {
      branch: m.branch,
      repos: [],
    };
    entry.repos.push(m.repo_key);
    missingByWorkspace.set(m.workspace_id, entry);
  }

  return (
    <div className="discrepancy-notice">
      <h2>State / disk mismatch</h2>
      <p>
        Tethys found things that don't line up between <code>state.json</code>{" "}
        and your <code>worktree_root</code>. Usually the result of a crash or
        manual filesystem surgery.
      </p>

      {discrepancies.orphaned_dirs.length > 0 && (
        <>
          <h3>Orphaned worktrees</h3>
          <p className="muted">
            Directories with no matching workspace in state. Safe to remove.
          </p>
          <ul className="discrepancy-list">
            {discrepancies.orphaned_dirs.map((o) => (
              <li key={o.path}>
                <code className="discrepancy-path">{o.path}</code>
                <button
                  type="button"
                  className="danger"
                  onClick={() => removeOrphan(o.path)}
                >
                  Remove
                </button>
              </li>
            ))}
          </ul>
        </>
      )}

      {missingByWorkspace.size > 0 && (
        <>
          <h3>Missing worktrees</h3>
          <p className="muted">
            Workspaces in state whose worktree directories have vanished. Forget
            drops the workspace from state; it does not touch disk.
          </p>
          <ul className="discrepancy-list">
            {Array.from(missingByWorkspace.entries()).map(
              ([id, { branch, repos }]) => (
                <li key={id}>
                  <div className="discrepancy-meta">
                    <code>{branch}</code>{" "}
                    <span className="muted">
                      · missing: {repos.join(", ")}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => forgetWorkspace(id)}
                  >
                    Forget
                  </button>
                </li>
              ),
            )}
          </ul>
        </>
      )}
    </div>
  );
}

function WorkspaceDetail({
  workspace,
  onDeleted,
  onRun,
}: {
  workspace: Workspace;
  onDeleted: () => void;
  onRun: (job: RunningJob) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);

  const togglePause = async () => {
    setBusy(true);
    try {
      await invoke(workspace.paused ? "resume_workspace" : "pause_workspace", {
        id: workspace.id,
      });
    } finally {
      setBusy(false);
    }
  };

  const performDelete = () => {
    setConfirmingDelete(false);
    onRun({
      title: `Deleting ${workspace.branch}`,
      command: "delete_workspace",
      args: { id: workspace.id },
      onSuccess: () => onDeleted(),
    });
  };

  return (
    <div className="workspace-detail">
      <header>
        <h2>
          <code>{workspace.branch}</code>
        </h2>
        <div className="actions">
          <button type="button" onClick={togglePause} disabled={busy}>
            {workspace.paused ? "Resume" : "Pause"}
          </button>
          <button
            type="button"
            className="danger"
            onClick={() => setConfirmingDelete(true)}
            disabled={busy}
          >
            Delete
          </button>
        </div>
      </header>
      {confirmingDelete && (
        <ConfirmDialog
          title="Delete workspace?"
          message={
            <>
              Delete workspace <code>{workspace.branch}</code>? This removes
              every worktree and clears the workspace from Tethys state.
            </>
          }
          confirmLabel="Delete"
          destructive
          onConfirm={performDelete}
          onCancel={() => setConfirmingDelete(false)}
        />
      )}
      <dl className="workspace-fields">
        <dt>Created</dt>
        <dd>{new Date(workspace.created_at).toLocaleString()}</dd>
        <dt>Repos</dt>
        <dd>
          {workspace.repo_links.length === 0 ? (
            "(none)"
          ) : (
            <ul className="repo-link-list">
              {workspace.repo_links.map((r) => (
                <li key={r.repo_key}>
                  <code>{r.repo_key}</code>
                  <span className="repo-link-path">{r.worktree_path}</span>
                  {r.setup_script_ran_at !== null && (
                    <span className="ok-badge">setup ok</span>
                  )}
                </li>
              ))}
            </ul>
          )}
        </dd>
        <dt>Sessions</dt>
        <dd>
          {workspace.sessions.length === 0
            ? "(none — wiring coming in M4/M5)"
            : workspace.sessions.map((s) => s.id).join(", ")}
        </dd>
        <dt>ID</dt>
        <dd>
          <code>{workspace.id}</code>
        </dd>
      </dl>
    </div>
  );
}

function CreateWorkspaceDialog({
  repos,
  onClose,
  onSubmit,
}: {
  repos: Repo[];
  onClose: () => void;
  onSubmit: (args: CreateWorkspaceArgs) => void;
}) {
  const [branch, setBranch] = useState("");
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(repos.map((r) => r.key)),
  );

  const toggle = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const canSubmit = branch.trim().length > 0 && selected.size > 0;

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    onSubmit({
      branch: branch.trim(),
      repo_selections: Array.from(selected),
    });
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <form
        className="modal"
        onSubmit={submit}
        onClick={(e) => e.stopPropagation()}
      >
        <h3>New workspace</h3>
        <label>
          Branch
          <input
            autoFocus
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
            placeholder="e.g. ryan/session-resume"
          />
        </label>
        <div className="repo-select">
          <div className="repo-select-label">Repos</div>
          {repos.length === 0 ? (
            <p className="muted">
              No repos in registry. Add some to <code>repos.toml</code>.
            </p>
          ) : (
            <ul>
              {repos.map((r) => (
                <li key={r.key}>
                  <label className="repo-row">
                    <input
                      type="checkbox"
                      checked={selected.has(r.key)}
                      onChange={() => toggle(r.key)}
                    />
                    <span className="repo-display">{r.key}</span>
                  </label>
                </li>
              ))}
            </ul>
          )}
        </div>
        <div className="modal-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!canSubmit}>
            Create
          </button>
        </div>
      </form>
    </div>
  );
}

function ConfirmDialog({
  title,
  message,
  confirmLabel = "Confirm",
  destructive = false,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: React.ReactNode;
  confirmLabel?: string;
  destructive?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="modal-backdrop" onClick={onCancel}>
      <div
        className="modal confirm-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <h3>{title}</h3>
        <div className="confirm-message">{message}</div>
        <div className="modal-actions">
          <button type="button" onClick={onCancel} autoFocus>
            Cancel
          </button>
          <button
            type="button"
            className={destructive ? "danger primary" : "primary"}
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

export default App;
