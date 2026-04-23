import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type {
  CreateWorkspaceArgs,
  Discrepancies,
  RegistryStatus,
  Repo,
  SessionInfo,
  Workspace,
  WorkspaceId,
} from "./types";
import { JobLogModal } from "./JobLogModal";
import { SessionTerminal } from "./SessionTerminal";
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
  const [busy, setBusy] = useState<Set<string>>(() => new Set());

  const withBusy = async (key: string, fn: () => Promise<void>) => {
    setBusy((prev) => new Set(prev).add(key));
    try {
      await fn();
    } catch (e) {
      alert(String(e));
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  };

  const removeOrphan = (path: string) =>
    withBusy(`orphan:${path}`, async () => {
      await invoke("remove_orphan_dir", { path });
      onChanged();
    });

  const forgetWorkspace = (id: string) =>
    withBusy(`forget:${id}`, async () => {
      await invoke("forget_workspace", { id });
      onChanged();
    });

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
            {discrepancies.orphaned_dirs.map((o) => {
              const working = busy.has(`orphan:${o.path}`);
              return (
                <li key={o.path}>
                  <code className="discrepancy-path">{o.path}</code>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => removeOrphan(o.path)}
                    disabled={working}
                  >
                    {working ? (
                      <>
                        <Spinner /> Removing…
                      </>
                    ) : (
                      "Remove"
                    )}
                  </button>
                </li>
              );
            })}
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
              ([id, { branch, repos }]) => {
                const working = busy.has(`forget:${id}`);
                return (
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
                      disabled={working}
                    >
                      {working ? (
                        <>
                          <Spinner /> Forgetting…
                        </>
                      ) : (
                        "Forget"
                      )}
                    </button>
                  </li>
                );
              },
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
  const [tab, setTab] = useState<string>("__overview");
  const [sessions, setSessions] = useState<SessionInfo[]>([]);

  // Reset tab when the selected workspace changes.
  useEffect(() => {
    setTab("__overview");
  }, [workspace.id]);

  const refreshSessions = useCallback(async () => {
    try {
      const list = await invoke<SessionInfo[]>("list_sessions", {
        workspaceId: workspace.id,
      });
      setSessions(list);
    } catch (e) {
      console.error("list_sessions:", e);
    }
  }, [workspace.id]);

  useEffect(() => {
    refreshSessions();
    const sc = listen("session:changed", () => refreshSessions());
    const sx = listen("session:exit", () => refreshSessions());
    return () => {
      sc.then((un) => un());
      sx.then((un) => un());
    };
  }, [refreshSessions]);

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

  // All live sessions grouped by repo.
  const sessionsByRepo = new Map<string, SessionInfo[]>();
  for (const s of sessions) {
    const arr = sessionsByRepo.get(s.repo_key) ?? [];
    arr.push(s);
    sessionsByRepo.set(s.repo_key, arr);
  }

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

      <nav className="repo-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "__overview"}
          className={tab === "__overview" ? "active" : ""}
          onClick={() => setTab("__overview")}
        >
          Overview
        </button>
        {workspace.repo_links.map((r) => {
          const hasLive = (sessionsByRepo.get(r.repo_key) ?? []).some(
            (s) => s.running,
          );
          return (
            <button
              key={r.repo_key}
              type="button"
              role="tab"
              aria-selected={tab === r.repo_key}
              className={tab === r.repo_key ? "active" : ""}
              onClick={() => setTab(r.repo_key)}
            >
              <span>{r.repo_key}</span>
              {hasLive && <span className="tab-dot" />}
            </button>
          );
        })}
      </nav>

      {tab === "__overview" && (
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
          <dt>ID</dt>
          <dd>
            <code>{workspace.id}</code>
          </dd>
        </dl>
      )}

      {workspace.repo_links.map((r) =>
        tab === r.repo_key ? (
          <RepoSessionPane
            key={r.repo_key}
            workspace={workspace}
            repoKey={r.repo_key}
            liveSessions={sessionsByRepo.get(r.repo_key) ?? []}
            onStarted={refreshSessions}
          />
        ) : null,
      )}
    </div>
  );
}

function RepoSessionPane({
  workspace,
  repoKey,
  liveSessions,
  onStarted,
}: {
  workspace: Workspace;
  repoKey: string;
  liveSessions: SessionInfo[];
  onStarted: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const metas = workspace.sessions.filter((s) => s.repo_key === repoKey);
  // Newest first — users care about recent conversations.
  const ordered = [...metas].reverse();
  const liveById = new Map(liveSessions.map((s) => [s.id, s]));

  // Effective selection: user's explicit pick if still valid; otherwise
  // default to the first live session, or the first entry.
  const effectiveSelected = (() => {
    if (selectedId && ordered.some((m) => m.id === selectedId))
      return selectedId;
    const firstLive = ordered.find((m) => liveById.has(m.id));
    return firstLive?.id ?? ordered[0]?.id ?? null;
  })();

  const startFresh = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await invoke<SessionInfo>("start_claude_session", {
        args: { workspace_id: workspace.id, repo_key: repoKey },
      });
      setSelectedId(res.id);
      onStarted();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const resumeMeta = async (metaId: string) => {
    setBusy(true);
    setError(null);
    try {
      const res = await invoke<SessionInfo>("resume_claude_session", {
        args: {
          workspace_id: workspace.id,
          repo_key: repoKey,
          session_meta_id: metaId,
        },
      });
      setSelectedId(res.id);
      onStarted();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (ordered.length === 0) {
    return (
      <div className="session-pane empty">
        <p className="muted">No Claude session in this worktree yet.</p>
        <button
          type="button"
          className="primary"
          onClick={startFresh}
          disabled={busy}
        >
          {busy ? (
            <>
              <Spinner /> Starting…
            </>
          ) : (
            <>
              Start Claude in <code>{repoKey}</code>
            </>
          )}
        </button>
        {error && <div className="error-banner">{error}</div>}
      </div>
    );
  }

  const selected = ordered.find((m) => m.id === effectiveSelected) ?? null;
  const selectedLive = selected ? liveById.get(selected.id) ?? null : null;

  return (
    <div className="session-pane">
      <div className="session-chip-bar">
        {ordered.map((m) => {
          const live = liveById.get(m.id);
          const label = m.claude_session_id
            ? m.claude_session_id.slice(0, 8)
            : "pending";
          return (
            <button
              key={m.id}
              type="button"
              className={`session-chip${
                effectiveSelected === m.id ? " active" : ""
              }`}
              onClick={() => setSelectedId(m.id)}
            >
              <code>{label}</code>
              {live?.running && <span className="tab-dot" />}
              {live && !live.running && (
                <span className="chip-state">exited</span>
              )}
              {!live && <span className="chip-state">dormant</span>}
            </button>
          );
        })}
        <button
          type="button"
          className="session-chip new"
          onClick={startFresh}
          disabled={busy}
          title="Start a new Claude session in this worktree"
        >
          {busy ? <Spinner /> : "+"} New
        </button>
      </div>

      {error && <div className="error-banner">{error}</div>}

      {selected &&
        (selectedLive ? (
          <>
            {!selectedLive.running && (
              <div className="session-exit-banner">
                Claude exited. Scrollback preserved below.
              </div>
            )}
            <SessionTerminal sessionId={selectedLive.id} />
          </>
        ) : (
          <div className="session-dormant">
            <p>
              This Claude session is dormant. Resume re-opens the conversation
              with <code>claude --resume</code>.
            </p>
            {selected.claude_session_id ? (
              <button
                type="button"
                className="primary"
                onClick={() => resumeMeta(selected.id)}
                disabled={busy}
              >
                {busy ? (
                  <>
                    <Spinner /> Resuming…
                  </>
                ) : (
                  "Resume"
                )}
              </button>
            ) : (
              <p className="muted">
                No <code>claude_session_id</code> was captured for this
                session — can't resume. (If you just started it, wait a
                second for the SessionStart hook.)
              </p>
            )}
          </div>
        ))}
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

function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

export default App;
