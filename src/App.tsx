import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type {
  CreateWorkspaceArgs,
  RegistryStatus,
  Repo,
  Workspace,
  WorkspaceId,
} from "./types";
import "./App.css";

function App() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [registry, setRegistry] = useState<RegistryStatus | null>(null);
  const [selectedId, setSelectedId] = useState<WorkspaceId | null>(null);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [list, reg] = await Promise.all([
        invoke<Workspace[]>("list_workspaces"),
        invoke<RegistryStatus>("registry_status"),
      ]);
      setWorkspaces(list);
      setRegistry(reg);
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
        {registry && !registryOk && <RegistryNotice registry={registry} onChanged={refresh} />}
        {selected ? (
          <WorkspaceDetail
            workspace={selected}
            onDeleted={() => setSelectedId(null)}
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
          onCreated={(ws) => {
            setCreating(false);
            setSelectedId(ws.id);
          }}
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
      // repos.toml edits require restart (per plan) — remind the user.
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
          Tethys expects a repo registry at <code>{registry.path}</code>.
          It doesn't exist yet.
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

function WorkspaceDetail({
  workspace,
  onDeleted,
}: {
  workspace: Workspace;
  onDeleted: () => void;
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

  const performDelete = async () => {
    setConfirmingDelete(false);
    setBusy(true);
    try {
      await invoke("delete_workspace", { id: workspace.id });
      onDeleted();
    } finally {
      setBusy(false);
    }
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
              Delete workspace <code>{workspace.branch}</code>? This removes it
              from Tethys state but does not touch any worktrees on disk (M3
              will wire that up).
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
                  {r.setup_script_ran_at === null && (
                    <span className="pending-badge">not created yet</span>
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
  onCreated,
}: {
  repos: Repo[];
  onClose: () => void;
  onCreated: (ws: Workspace) => void;
}) {
  const [branch, setBranch] = useState("");
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(repos.map((r) => r.key)),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const toggle = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const canSubmit = branch.trim().length > 0 && selected.size > 0;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const args: CreateWorkspaceArgs = {
        branch: branch.trim(),
        repo_selections: Array.from(selected),
      };
      const ws = await invoke<Workspace>("create_workspace", { args });
      onCreated(ws);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
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
        {error && <div className="error-banner">{error}</div>}
        <div className="modal-actions">
          <button type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="primary"
            disabled={busy || !canSubmit}
          >
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
